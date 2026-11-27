//! On-demand history for an explicitly supplied chain of confirmed options.

use std::{
    collections::{BTreeMap, HashSet, btree_map::Entry},
    path::Path,
};

use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
pub use finance_ib::historical::OptionMinuteBar as TradeBar;
use finance_ib::market::{
    ContractKind, ContractQuery, MarketError, OptionMinuteSource, PaperMarket,
};
use finance_market_protocol::{InstrumentId, U64Text};

pub mod databento;

/// Metadata from a qualified IBKR option contract. All times are UTC Unix nanoseconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmedOption {
    pub instrument_id: InstrumentId,
    pub con_id: i32,
    pub expires_at: U64Text,
}

/// Only bars fully contained in `[start, end)` are returned. A bar can close at `end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BarWindow {
    pub start: U64Text,
    pub end: U64Text,
}

/// Source of option history. Implementations own request pacing and pagination.
#[async_trait]
pub trait PaperOptionHistory: Send + Sync {
    /// Fetch one-minute TRADES bars for the qualified contract. A successful empty
    /// response indicates no bars. Permission and transport failures return errors.
    async fn fetch_trade_bars(
        &self,
        contract: &ConfirmedOption,
        window: BarWindow,
    ) -> Result<Vec<TradeBar>>;
}

/// Historical reads through F9a's verified paper market session. Callers pass
/// the same local data root as F11 so both readers share historical pacing.
pub struct IbkrOptionHistory<'a, M = PaperMarket> {
    market: &'a M,
    data_root: &'a Path,
}

impl<'a, M: OptionMinuteSource> IbkrOptionHistory<'a, M> {
    pub fn new(market: &'a M, data_root: &'a Path) -> Self {
        Self { market, data_root }
    }
}

#[async_trait]
impl<M: OptionMinuteSource> PaperOptionHistory for IbkrOptionHistory<'_, M> {
    async fn fetch_trade_bars(
        &self,
        contract: &ConfirmedOption,
        window: BarWindow,
    ) -> Result<Vec<TradeBar>> {
        let confirmed = self
            .market
            .confirm(&ContractQuery::Option {
                con_id: contract.con_id,
            })
            .await?;
        ensure!(
            confirmed.con_id == contract.con_id
                && matches!(confirmed.kind, ContractKind::Option(_)),
            "F9a returned a different option contract"
        );
        Ok(self
            .market
            .option_trade_minutes(
                &confirmed,
                window.start..window.end,
                contract.expires_at,
                self.data_root,
            )
            .await?)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum OptionBars {
    Available(Vec<TradeBar>),
    NoBars,
    Expired,
}

#[derive(Debug)]
pub struct ContractHistory {
    pub instrument_id: InstrumentId,
    pub bars: Result<OptionBars>,
}

/// Fetch serially in input order. `now` is the request clock, sampled again
/// before every provider call so that a contract can expire during a chain read.
pub async fn fetch_ibkr_chain(
    provider: &dyn PaperOptionHistory,
    contracts: &[ConfirmedOption],
    window: BarWindow,
    now: impl Fn() -> U64Text,
) -> Result<Vec<ContractHistory>> {
    ensure!(window.start < window.end, "empty or reversed bar window");
    ensure!(window.end <= now(), "bar window ends in the future");
    let mut ids = HashSet::new();
    let mut con_ids = HashSet::new();
    for contract in contracts {
        ensure!(contract.con_id > 0, "IBKR conId must be positive");
        ensure!(
            ids.insert(&contract.instrument_id) && con_ids.insert(contract.con_id),
            "duplicate option instrument_id or IBKR conId"
        );
    }

    let mut results = Vec::with_capacity(contracts.len());
    for contract in contracts {
        let bars = if now() >= contract.expires_at {
            Ok(OptionBars::Expired)
        } else {
            fetch_contract(provider, contract, window).await
        };
        results.push(ContractHistory {
            instrument_id: contract.instrument_id.clone(),
            bars,
        });
    }
    Ok(results)
}

async fn fetch_contract(
    provider: &dyn PaperOptionHistory,
    contract: &ConfirmedOption,
    window: BarWindow,
) -> Result<OptionBars> {
    let bars = match provider.fetch_trade_bars(contract, window).await {
        Err(error) if error.downcast_ref() == Some(&MarketError::ExpiredContract) => {
            return Ok(OptionBars::Expired);
        }
        result => result?,
    };
    let mut selected = BTreeMap::<U64Text, TradeBar>::new();
    for bar in bars {
        // Range-based provider requests may yield overlapping or still-open bars.
        if bar.data.open_ts < window.start || bar.close_ts > window.end {
            continue;
        }
        bar.data
            .validate_at_close(bar.close_ts)
            .context("invalid IBKR option bar")?;
        match selected.entry(bar.close_ts) {
            Entry::Vacant(entry) => {
                entry.insert(bar);
            }
            Entry::Occupied(entry) => ensure!(
                entry.get() == &bar,
                "conflicting IBKR option bars at {}",
                bar.close_ts.get()
            ),
        }
    }
    Ok(if selected.is_empty() {
        OptionBars::NoBars
    } else {
        OptionBars::Available(selected.into_values().collect())
    })
}
