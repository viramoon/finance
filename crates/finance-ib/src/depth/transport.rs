//! The single-venue depth adapter on a verified paper connection.

use super::{LineWrite, PaperMarket, bounded_request, positive_decimal, quantity};
use crate::{
    PaperPeer,
    depth::{
        DepthError, DepthLevel, DepthOperation, DepthRequest, DepthSide, DepthSource,
        DepthSubscription, DepthSymbol, DepthUpdate,
    },
    market::{ConfirmedContract, MarketDataSource, MarketError},
};
use async_trait::async_trait;
use ibapi::{
    contracts::{Contract, SecurityType},
    market_data::{SmartDepth, realtime::ExactMarketDepth},
    prelude::StreamExt,
    subscriptions::{Subscription, SubscriptionItem},
};
use std::{sync::Arc, time::Duration};

const NASDAQ_ROUTE: &str = "ISLAND";

#[async_trait]
impl DepthSource for PaperMarket {
    type Subscription = PaperDepthSubscription;

    async fn confirm_depth(&self, symbol: DepthSymbol) -> Result<ConfirmedContract, DepthError> {
        Ok(self.confirm(&symbol.contract_query()).await?)
    }

    async fn subscribe_depth(
        &self,
        request: &DepthRequest,
    ) -> Result<Self::Subscription, DepthError> {
        if !self.peer.is_active() {
            return Err(DepthError::Disconnected);
        }
        let confirmed = request.contract();
        if self.confirmed.lock().await.get(&confirmed.con_id) != Some(confirmed) {
            return Err(DepthError::UnconfirmedContract);
        }
        let rows = i32::try_from(request.rows()).map_err(|_| DepthError::InvalidRequest)?;
        let contract = Contract {
            contract_id: confirmed.con_id,
            security_type: SecurityType::Stock,
            exchange: NASDAQ_ROUTE.into(),
            currency: "USD".into(),
            ..Contract::default()
        };
        let builder = self
            .peer
            .client
            .market_depth(&contract, rows)
            .smart_depth(SmartDepth::No);
        let mut write = LineWrite::new(&self.peer);
        let subscription = self
            .peer
            .while_connected(bounded_request(self.timeout, builder.subscribe_exact()))
            .await?;
        let request_id = subscription.request_id().ok_or(DepthError::Transport)?;
        write.completed = true;
        Ok(PaperDepthSubscription {
            subscription: Some(subscription),
            peer: self.peer.clone(),
            timeout: self.timeout,
            request_id,
            read_finished: false,
        })
    }
}

/// A direct Nasdaq depth line. Terminated reads retain the handle for checked cancellation.
pub struct PaperDepthSubscription {
    subscription: Option<Subscription<ExactMarketDepth>>,
    peer: Arc<PaperPeer>,
    timeout: Duration,
    request_id: i32,
    read_finished: bool,
}

impl Drop for PaperDepthSubscription {
    fn drop(&mut self) {
        if self.subscription.is_some() {
            // The SDK's best-effort cancel on drop has no checked write result.
            self.peer.invalidate();
        }
    }
}

#[async_trait]
impl DepthSubscription for PaperDepthSubscription {
    fn request_id(&self) -> i32 {
        self.request_id
    }

    async fn next(&mut self) -> Result<Option<DepthUpdate>, DepthError> {
        let result = self
            .peer
            .while_connected(async {
                let Some(subscription) = &mut self.subscription else {
                    return Ok(Ok(None));
                };
                if self.read_finished {
                    return Ok(Ok(None));
                }
                match read_update(subscription).await {
                    Err(DepthError::Disconnected) => Err(MarketError::Disconnected),
                    result => Ok(result.map(Some)),
                }
            })
            .await?;
        if !matches!(result, Ok(Some(DepthUpdate::Row { .. }))) {
            self.read_finished = true;
        }
        result
    }

    async fn cancel(&mut self) -> Result<(), DepthError> {
        let Some(subscription) = self.subscription.take() else {
            return Ok(self.peer.while_connected(async { Ok(()) }).await?);
        };
        let mut write = LineWrite::new(&self.peer);
        self.peer
            .while_connected(bounded_request(self.timeout, subscription.try_cancel()))
            .await?;
        write.completed = true;
        Ok(())
    }
}

async fn read_update(
    subscription: &mut Subscription<ExactMarketDepth>,
) -> Result<DepthUpdate, DepthError> {
    while let Some(item) = subscription.next().await {
        match item {
            Ok(SubscriptionItem::Data(row)) => return depth_row(row),
            Ok(SubscriptionItem::Notice(notice)) | Err(ibapi::Error::Notice(notice)) => {
                match notice.code {
                    code if crate::invalidates_market_session(code) => {
                        return Err(DepthError::Disconnected);
                    }
                    316 | 317 => return Ok(DepthUpdate::Reset),
                    200 => return Err(DepthError::UnconfirmedContract),
                    309 => return Err(DepthError::Capacity),
                    354 | 10089 | 10090 | 10167 | 10168 | 10186 => {
                        return Err(DepthError::NotEntitled);
                    }
                    10197 => return Err(DepthError::CompetingSession),
                    100 | 101 | 420 => return Err(DepthError::Pacing),
                    2100..=2169 => {}
                    _ => return Err(DepthError::Transport),
                }
            }
            Err(ibapi::Error::SubscriptionLagged(_)) => return Ok(DepthUpdate::Reset),
            Err(ibapi::Error::ProtobufDecode(_) | ibapi::Error::Parse(..)) => {
                return Err(DepthError::InvalidCallback);
            }
            Err(error) => return Err(super::transport_error(error).into()),
        }
    }
    Err(DepthError::Disconnected)
}

fn depth_row(row: ExactMarketDepth) -> Result<DepthUpdate, DepthError> {
    if row.smart_depth {
        return Err(DepthError::InvalidCallback);
    }
    let side = match row.side {
        0 => DepthSide::Ask,
        1 => DepthSide::Bid,
        _ => return Err(DepthError::InvalidCallback),
    };
    let operation = match row.operation {
        0 => DepthOperation::Insert,
        1 => DepthOperation::Update,
        2 => DepthOperation::Delete,
        _ => return Err(DepthError::InvalidCallback),
    };
    let position = u32::try_from(row.position).map_err(|_| DepthError::InvalidCallback)?;
    let level = if operation == DepthOperation::Delete {
        // Delete is positional. The price and size fields can be absent.
        None
    } else {
        // The wire price is a double. All finance state is decimal and the
        // wire size is already decimal text.
        let price = row
            .price
            .and_then(positive_decimal)
            .ok_or(DepthError::InvalidCallback)?;
        let size = quantity(row.size).ok_or(DepthError::InvalidCallback)?;
        Some(DepthLevel { price, size })
    };
    Ok(DepthUpdate::Row {
        side,
        operation,
        position,
        level,
    })
}

#[cfg(test)]
#[path = "transport_tests.rs"]
mod tests;
