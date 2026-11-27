//! Private option-flow observation and historical verification.

mod decimal;
pub mod evidence;
mod ingress;
pub mod observation;

pub use ingress::{HistoricalFlow, OPRA_HISTORY, OptionHistoryIngress};

use anyhow::{Result, ensure};
use chrono::{DateTime, NaiveDate, Utc};
use finance_ib::market::{ConfirmedContract, ContractKind, Product};
use tokio::sync::broadcast;

use crate::{
    line_budget::{BudgetStatus, CoverageReport, LineHandle, LineUpdate},
    market_ingress::ProductPolicy,
};

use observation::{FlowObservation, ObservationEngine};

pub const IBKR: &str = "ibkr";
pub const OPRA: &str = "opra";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FlowUse {
    PrivateDisplay,
    NonDisplayDerivation,
}

impl FlowUse {
    fn authorize(self, policy: &ProductPolicy, product: &str) -> Result<()> {
        ensure!(
            policy.source.as_str() == IBKR && policy.product.as_str() == product,
            "option flow requires the matching IBKR product policy"
        );
        ensure!(
            match self {
                Self::PrivateDisplay => policy.display_live,
                Self::NonDisplayDerivation => policy.derive_non_display,
            },
            "option flow use is not permitted for this product"
        );
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct ObservedFlow {
    pub clue: FlowObservation,
    pub coverage: Option<CoverageReport>,
    pub budget: BudgetStatus,
}

/// A tail reader of the existing scheduler. Construct it before starting the
/// scheduler to see the first subscriptions. It never allocates lines.
pub struct LiveOptionFlow {
    scheduler: LineHandle,
    updates: broadcast::Receiver<LineUpdate>,
    observer: ObservationEngine,
}

impl LiveOptionFlow {
    pub fn watch(
        scheduler: LineHandle,
        observer: ObservationEngine,
        policy: &ProductPolicy,
        usage: FlowUse,
    ) -> Result<Self> {
        usage.authorize(policy, OPRA)?;
        let updates = scheduler.updates();
        Ok(Self {
            scheduler,
            updates,
            observer,
        })
    }

    pub fn observation(&self) -> &ObservationEngine {
        &self.observer
    }

    /// Waits for the next hint. The caller can cancel the wait when stopping the scheduler.
    pub async fn next(&mut self) -> Option<ObservedFlow> {
        loop {
            let update = match self.updates.recv().await {
                Ok(update) => update,
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    self.observer.reader_lagged();
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            };
            if update.key.product != Product::Opra {
                continue;
            }
            let contract = self.scheduler.option_contract(update.key.con_id);
            if let Some(clue) = self.observer.apply(&update, contract.as_ref()) {
                let (start, end) = clue.window();
                return Some(ObservedFlow {
                    coverage: self.scheduler.coverage(update.key, start, end),
                    budget: self.scheduler.status(),
                    clue,
                });
            }
        }
    }
}

/// Put nearest expiries first when selecting a bounded set of verification requests.
pub fn prioritize_history(contracts: &mut [ConfirmedContract], market_date: NaiveDate) {
    contracts.sort_by_key(|contract| match &contract.kind {
        ContractKind::Option(option) if option.expiry >= market_date => (0, Some(option.expiry)),
        ContractKind::Option(option) => (1, Some(option.expiry)),
        ContractKind::Equity => (2, None),
    });
}

fn nanos(time: DateTime<Utc>) -> Result<u64> {
    u64::try_from(time.timestamp())?
        .checked_mul(1_000_000_000)
        .and_then(|base| base.checked_add(u64::from(time.timestamp_subsec_nanos())))
        .ok_or_else(|| anyhow::anyhow!("option flow clock is outside the protocol range"))
}
