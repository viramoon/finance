use super::{
    HistoricalIssueReason, HistoricalKind, HistoricalOptionSource, HistoricalPage,
    HistoricalPageRequest, HistoricalRows, HistoryQuote, HistoryTrade, PAGE_LIMIT,
    contract_available,
};
use crate::{
    PaperPeer, Session,
    historical::{REQUEST_TIMEOUT, pacing::Pacer},
    market::{ConfirmedContract, MarketNotice, market_notice_kind},
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use finance_market_protocol::DecimalText;
use ibapi::{
    contracts::{Contract, SecurityType},
    market_data::{
        TradingHours,
        historical::{ExactTickBidAsk, ExactTickLast},
    },
};
use rust_decimal::Decimal;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};
use time::OffsetDateTime;
use tokio::sync::Mutex;

/// The historical read port of a verified paper market session.
pub struct PaperOptionHistory<'a> {
    peer: &'a PaperPeer,
    confirmed: &'a Mutex<HashMap<i32, ConfirmedContract>>,
    root: PathBuf,
}

impl<'a> PaperOptionHistory<'a> {
    pub(crate) fn new(
        peer: &'a PaperPeer,
        confirmed: &'a Mutex<HashMap<i32, ConfirmedContract>>,
        root: &Path,
    ) -> Self {
        Self {
            peer,
            confirmed,
            root: root.to_owned(),
        }
    }
}

#[async_trait]
impl HistoricalOptionSource for PaperOptionHistory<'_> {
    async fn option_tick_page(
        &self,
        contract: &ConfirmedContract,
        request: HistoricalPageRequest,
    ) -> Result<HistoricalPage, HistoricalIssueReason> {
        contract_available(contract, Utc::now())?;
        if !self.peer.is_active() {
            return Err(HistoricalIssueReason::Disconnected);
        }
        if self.confirmed.lock().await.get(&contract.con_id) != Some(contract) {
            return Err(HistoricalIssueReason::UnconfirmedContract);
        }
        if !(1..=PAGE_LIMIT).contains(&request.limit)
            || request.start.timestamp() < 0
            || request.start.timestamp_subsec_nanos() != 0
            || request.start > Utc::now()
        {
            return Err(HistoricalIssueReason::ProviderError);
        }
        let pacing = tokio::time::timeout(REQUEST_TIMEOUT, Pacer::acquire(&self.root))
            .await
            .map_err(|_| HistoricalIssueReason::PacingState)?
            .map_err(|_| HistoricalIssueReason::PacingState)?;
        pacing
            .wait()
            .await
            .map_err(|_| HistoricalIssueReason::PacingState)?;
        contract_available(contract, Utc::now())?;
        if !self.peer.is_active() {
            return Err(HistoricalIssueReason::Disconnected);
        }
        pacing
            .begin()
            .map_err(|_| HistoricalIssueReason::PacingState)?;
        let result = tokio::select! {
            biased;
            _ = self.peer.disconnected() => Err(HistoricalIssueReason::Disconnected),
            reply = tokio::time::timeout(REQUEST_TIMEOUT, fetch_page(self.peer, contract, request)) => {
                reply.unwrap_or(Err(HistoricalIssueReason::Timeout))
            }
        };
        // A timeout interrupted an outstanding wire request. Stop using that
        // session rather than relying on an unacknowledged SDK drop cancel.
        if matches!(
            result,
            Err(HistoricalIssueReason::Timeout | HistoricalIssueReason::Disconnected)
        ) {
            self.peer.invalidate();
        }
        pacing
            .finish()
            .map_err(|_| HistoricalIssueReason::PacingState)?;
        if result.is_ok() && !self.peer.is_active() {
            return Err(HistoricalIssueReason::Disconnected);
        }
        result
    }
}

async fn fetch_page(
    peer: &PaperPeer,
    confirmed: &ConfirmedContract,
    request: HistoricalPageRequest,
) -> Result<HistoricalPage, HistoricalIssueReason> {
    let start = OffsetDateTime::from_unix_timestamp(request.start.timestamp())
        .map_err(|_| HistoricalIssueReason::ProviderError)?;
    let contract = Contract {
        contract_id: confirmed.con_id,
        security_type: SecurityType::Option,
        exchange: "SMART".into(),
        currency: "USD".into(),
        ..Contract::default()
    };
    let builder = peer
        .client
        .historical_ticks(&contract, i32::from(request.limit))
        .starting(start)
        .trading_hours(TradingHours::Regular);
    let (request_id, rows) = match request.kind {
        HistoricalKind::Trades => {
            let (id, rows) = builder.trade_exact().await.map_err(history_error)?;
            let rows = rows.into_iter().map(trade).collect::<Result<_, _>>()?;
            (id, HistoricalRows::Trades(rows))
        }
        HistoricalKind::BidAsk => {
            let (id, rows) = builder.bid_ask_exact().await.map_err(history_error)?;
            let rows = rows.into_iter().map(quote).collect::<Result<_, _>>()?;
            (id, HistoricalRows::BidAsk(rows))
        }
    };
    Ok(HistoricalPage {
        rows,
        obtained_at: Utc::now(),
        provider_request_id: Some(request_id),
    })
}

fn history_error(error: ibapi::Error) -> HistoricalIssueReason {
    use HistoricalIssueReason as Reason;
    match error {
        ibapi::Error::ServerVersion(..) => Reason::Unsupported,
        ibapi::Error::UnexpectedEndOfStream | ibapi::Error::ResponseLagged(_) => {
            Reason::IncompletePage
        }
        error if crate::is_disconnect(&error) => Reason::Disconnected,
        ibapi::Error::Notice(notice) if notice.code == 200 => Reason::UnconfirmedContract,
        ibapi::Error::Notice(notice) => notice_reason(&notice),
        _ => Reason::ProviderError,
    }
}

fn notice_reason(notice: &ibapi::Notice) -> HistoricalIssueReason {
    use HistoricalIssueReason as Reason;
    let message = notice.message.to_ascii_lowercase();
    // Never return or log raw notice text. Error 162 is overloaded; only
    // explicitly recognizable reasons get a more specific status.
    match (notice.code, market_notice_kind(notice)) {
        (_, MarketNotice::Pacing) => Reason::Pacing,
        (
            _,
            MarketNotice::NotEntitled
            | MarketNotice::PartialEntitlement
            | MarketNotice::CompetingSession,
        ) => Reason::PermissionDenied,
        (162 | 165 | 166, _) if message.contains("expired") => Reason::ExpiredContract,
        (162 | 165, _) if message.contains("permission") || message.contains("not subscribed") => {
            Reason::PermissionDenied
        }
        (162 | 165, _) if message.contains("query returned no data") => Reason::NoHistory,
        (166, _) => Reason::ExpiredContract,
        _ => Reason::ProviderError,
    }
}

fn seconds(value: Option<i64>) -> Result<DateTime<Utc>, HistoricalIssueReason> {
    value
        .filter(|second| *second > 0)
        .and_then(|second| DateTime::from_timestamp(second, 0))
        .ok_or(HistoricalIssueReason::IncompletePage)
}

// IB sends prices as binary64, and sends quantities as decimal text. Decimal
// receives the SDK price's shortest round-trip text exactly once. Quantities
// bypass binary64 SDK conversions entirely.
fn price(value: Option<f64>) -> Result<Option<DecimalText>, HistoricalIssueReason> {
    match value {
        Some(value) if !value.is_finite() => Err(HistoricalIssueReason::IncompletePage),
        Some(value) => Ok(Some(
            value
                .to_string()
                .try_into()
                .map_err(|_| HistoricalIssueReason::IncompletePage)?,
        )),
        None => Ok(None),
    }
}

fn quantity(value: Option<String>) -> Result<Option<DecimalText>, HistoricalIssueReason> {
    value
        .map(|value| {
            value
                .try_into()
                .map_err(|_| HistoricalIssueReason::IncompletePage)
        })
        .transpose()
}

fn trade(row: ExactTickLast) -> Result<HistoryTrade, HistoricalIssueReason> {
    let price = price(row.price)?
        .filter(|price| price.value() >= Decimal::ZERO)
        .ok_or(HistoricalIssueReason::IncompletePage)?;
    let quantity = quantity(row.size)?
        .filter(|quantity| quantity.value() >= Decimal::ZERO)
        .ok_or(HistoricalIssueReason::IncompletePage)?;
    Ok(HistoryTrade {
        occurred_at: seconds(row.timestamp)?,
        price,
        quantity,
        exchange: row.exchange.unwrap_or_default(),
        special_conditions: row.special_conditions.unwrap_or_default(),
        past_limit: row.tick_attribute_last.past_limit,
        unreported: row.tick_attribute_last.unreported,
    })
}

fn quote(row: ExactTickBidAsk) -> Result<HistoryQuote, HistoricalIssueReason> {
    Ok(HistoryQuote {
        occurred_at: seconds(row.timestamp)?,
        bid: price(row.price_bid)?.filter(|price| price.value() > Decimal::ZERO),
        ask: price(row.price_ask)?.filter(|price| price.value() > Decimal::ZERO),
        bid_quantity: quantity(row.size_bid)?.filter(|quantity| quantity.value() >= Decimal::ZERO),
        ask_quantity: quantity(row.size_ask)?.filter(|quantity| quantity.value() >= Decimal::ZERO),
        bid_past_low: row.tick_attribute_bid_ask.bid_past_low,
        ask_past_high: row.tick_attribute_bid_ask.ask_past_high,
    })
}

#[cfg(test)]
mod tests;
