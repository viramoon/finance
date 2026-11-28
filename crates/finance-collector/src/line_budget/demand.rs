use std::collections::HashSet;

use finance_ib::{
    depth::{DepthSymbol, VENUE},
    market::ContractQuery,
};
use finance_market_protocol::InstrumentId;
use serde::Serialize;
use tokio::sync::watch;
use utoipa::ToSchema;

use super::watchlist;

/// F1 IDs within the deployment's instrument registry, independent of active feeds.
pub struct SubscriptionInstrumentRegistry(HashSet<InstrumentId>);

impl FromIterator<InstrumentId> for SubscriptionInstrumentRegistry {
    fn from_iter<T: IntoIterator<Item = InstrumentId>>(ids: T) -> Self {
        Self(ids.into_iter().collect())
    }
}

impl Default for SubscriptionInstrumentRegistry {
    fn default() -> Self {
        let mut registry: Self = watchlist()
            .into_iter()
            .filter_map(|query| match query {
                ContractQuery::Equity {
                    symbol,
                    primary_exchange,
                } => Some(format!("{symbol}.{primary_exchange}")),
                _ => None,
            })
            .map(|id| id.parse().expect("default equity instrument ID"))
            .collect();
        for coin in ["BTC", "ETH", "SOL"] {
            for id in [
                format!("{coin}-USD-PERP.HYPERLIQUID"),
                format!("{coin}-USDT.BINANCE"),
                format!("{coin}-USDT-PERP.BINANCE"),
            ] {
                registry
                    .0
                    .insert(id.parse().expect("default crypto instrument ID"));
            }
        }
        registry
    }
}

impl SubscriptionInstrumentRegistry {
    /// Validate the full update and normalize the set before handing it to the owner.
    pub fn validate(
        &self,
        mut watchlist: Vec<InstrumentId>,
        depth_focus: Option<InstrumentId>,
    ) -> Option<SubscriptionDemand> {
        if watchlist.iter().any(|id| !self.0.contains(id))
            || depth_focus
                .as_ref()
                .is_some_and(|id| !self.0.contains(id) || depth_symbol(id).is_none())
        {
            return None;
        }
        watchlist.sort_unstable_by(|left, right| left.as_str().cmp(right.as_str()));
        watchlist.dedup();
        Some(SubscriptionDemand {
            watchlist,
            depth_focus,
        })
    }
}

fn depth_symbol(id: &InstrumentId) -> Option<DepthSymbol> {
    let (symbol, venue) = id.as_str().rsplit_once('.')?;
    if venue != VENUE {
        return None;
    }
    symbol.parse().ok()
}

/// Additional watchlist and page-depth demand. The owner retains its base allocation.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, ToSchema)]
pub struct SubscriptionDemand {
    watchlist: Vec<InstrumentId>,
    #[schema(required = true)]
    depth_focus: Option<InstrumentId>,
}

impl SubscriptionDemand {
    pub fn watchlist(&self) -> &[InstrumentId] {
        &self.watchlist
    }

    pub fn depth_focus(&self) -> Option<DepthSymbol> {
        self.depth_focus.as_ref().and_then(depth_symbol)
    }
}

/// The HTTP side of the local owner's latest-value channel.
#[derive(Clone)]
pub struct SubscriptionDemandHandle(watch::Sender<SubscriptionDemand>);

impl SubscriptionDemandHandle {
    /// The owner holds the receiver and reconciles snapshots under its own policy.
    pub fn channel() -> (Self, watch::Receiver<SubscriptionDemand>) {
        let (sender, receiver) = watch::channel(SubscriptionDemand::default());
        (Self(sender), receiver)
    }

    pub fn current(&self) -> Option<SubscriptionDemand> {
        (!self.0.is_closed()).then(|| self.0.borrow().clone())
    }

    /// Replace atomically. Equivalent sets do not wake the owner.
    pub fn replace(&self, demand: SubscriptionDemand) -> Option<SubscriptionDemand> {
        let mut accepted = false;
        self.0.send_if_modified(|current| {
            accepted = !self.0.is_closed();
            if !accepted || *current == demand {
                return false;
            }
            *current = demand.clone();
            true
        });
        accepted.then_some(demand)
    }
}
