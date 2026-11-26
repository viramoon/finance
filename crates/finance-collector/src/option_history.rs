//! On-demand history for an explicitly supplied chain of confirmed options.

use std::collections::{BTreeMap, HashSet, btree_map::Entry};

use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use finance_market_protocol::{ClosedBar, InstrumentId, U64Text};

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

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct TradeBar {
    pub close_ts: U64Text,
    pub data: ClosedBar,
}

/// Integration point for F9a. The implementation must use a verified, read-only
/// paper session and own request pacing, pagination, and provider error mapping.
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
    let bars = provider.fetch_trade_bars(contract, window).await?;
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
