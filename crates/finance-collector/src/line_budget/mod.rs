//! Shared, read-only IBKR L1 subscriptions and option-chain rotation.

mod coverage;
mod depth;
mod policy;
mod session;
mod worker;

pub use coverage::{CoverageInterval, CoverageReport};
pub use depth::{DepthAllocation, DepthBudget, DepthBudgetError};
pub use policy::{BudgetConfig, BudgetConfigError, watchlist};

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{Arc, Mutex},
};

use anyhow::{Result, ensure};
use chrono::{DateTime, NaiveDate, TimeDelta, Utc};
use finance_ib::market::{
    ConfirmedContract, ContractKind, ContractQuery, MarketEvent, RequestKey, option_market_date,
};
use finance_market_protocol::DecimalText;
use tokio::{
    sync::{Notify, broadcast},
    time::Instant,
};

use self::coverage::Coverage;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SchedulerPhase {
    Starting,
    Online,
    Reconnecting,
    Stopped,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LinePool {
    Stock,
    Hot,
    Rotation,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LineMessage {
    Opened,
    Event(MarketEvent),
    Closed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LineUpdate {
    pub key: RequestKey,
    /// Unique within one scheduler run, including across reconnects and rotations.
    pub generation: u64,
    pub message: LineMessage,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BudgetStatus {
    pub phase: SchedulerPhase,
    pub as_of: DateTime<Utc>,
    pub account_lines: usize,
    pub reserve_lines: usize,
    pub stock_lines: usize,
    pub hot_lines: usize,
    pub rotation_lines: usize,
    pub confirmed_options: usize,
    pub observed_options: usize,
    pub discovery_complete: bool,
}

#[derive(Clone)]
pub struct LineHandle(Arc<Shared>);

pub struct LineReader {
    key: RequestKey,
    receiver: broadcast::Receiver<LineUpdate>,
}

impl LineReader {
    /// A lag error means the reader missed updates. It must discard local line state.
    pub async fn recv(&mut self) -> Result<LineUpdate, broadcast::error::RecvError> {
        loop {
            let update = self.receiver.recv().await?;
            if update.key == self.key {
                return Ok(update);
            }
        }
    }
}

impl LineHandle {
    /// Readers never allocate lines. Background and page readers consume the same generation.
    pub fn reader(&self, key: RequestKey) -> LineReader {
        LineReader {
            key,
            receiver: self.0.updates.subscribe(),
        }
    }

    /// Tail updates for collection-wide consumers. Each receiver has its own cursor.
    pub fn updates(&self) -> broadcast::Receiver<LineUpdate> {
        self.0.updates.subscribe()
    }

    /// The page supplies an equity conId and an ordered list of desired hot option conIds.
    /// Only discovered contracts can enter the hot pool. Duplicates share the same place.
    pub fn set_selection(&self, focus: Option<i32>, hot_options: &[i32]) -> Result<()> {
        ensure!(
            focus.is_none_or(|id| id > 0) && hot_options.iter().all(|id| *id > 0),
            "selection requires positive conIds"
        );
        let mut seen = HashSet::new();
        let hot = hot_options
            .iter()
            .copied()
            .filter(|id| seen.insert(*id))
            .take(self.0.config.hot_lines)
            .collect();
        self.0.state.lock().expect("line state").selection = Selection { focus, hot };
        self.0.changed.notify_one();
        Ok(())
    }

    pub fn status(&self) -> BudgetStatus {
        let state = self.0.state.lock().expect("line state");
        let as_of = self.0.now();
        let oldest = as_of
            .checked_sub_signed(
                TimeDelta::from_std(self.0.config.coverage_retention).expect("validated retention"),
            )
            .unwrap_or(DateTime::<Utc>::MIN_UTC);
        let (mut stock, mut hot, mut rotation) = (0, 0, 0);
        for line in state.live.values() {
            match line.pool {
                LinePool::Stock => stock += 1,
                LinePool::Hot => hot += 1,
                LinePool::Rotation => rotation += 1,
            }
        }
        let (mut confirmed_options, mut observed_options) = (0, 0);
        for contract in state.current_options(as_of) {
            confirmed_options += 1;
            if state
                .coverage
                .last_observed_at(session::option_key(contract.con_id))
                .is_some_and(|receipt| receipt >= oldest)
            {
                observed_options += 1;
            }
        }
        BudgetStatus {
            phase: state.phase,
            as_of,
            account_lines: self.0.config.account_lines,
            reserve_lines: self.0.config.reserve_lines,
            stock_lines: stock,
            hot_lines: hot,
            rotation_lines: rotation,
            confirmed_options,
            observed_options,
            discovery_complete: state.discovery_complete,
        }
    }

    /// Current confirmed (unexpired) chain, ordered by conId. Check discovery_complete
    /// before treating the size as the denominator for this watchlist.
    pub fn option_contracts(&self) -> Vec<ConfirmedContract> {
        self.option_chain().2
    }

    /// Read the confirmed roster, its observation time and discovery status under one lock.
    pub fn option_chain(&self) -> (DateTime<Utc>, bool, Vec<ConfirmedContract>) {
        let state = self.0.state.lock().expect("line state");
        let as_of = self.0.now();
        (
            as_of,
            state.discovery_complete,
            state.current_options(as_of).cloned().collect(),
        )
    }

    /// Coverage of the scheduler's shared OPRA line including the OI request.
    pub fn option_coverage(
        &self,
        con_id: i32,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Option<CoverageReport> {
        self.coverage(session::option_key(con_id), start, end)
    }

    /// Resolve metadata for a single current option without cloning the chain.
    pub fn option_contract(&self, con_id: i32) -> Option<ConfirmedContract> {
        let today = option_market_date(self.0.now()).ok()?;
        self.0
            .state
            .lock()
            .expect("line state")
            .options
            .get(&con_id)
            .filter(|contract| unexpired(contract, today))
            .cloned()
    }

    pub fn coverage(
        &self,
        key: RequestKey,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Option<CoverageReport> {
        let state = self.0.state.lock().expect("line state");
        state.coverage.query(key, start, end, self.0.now())
    }
}

pub struct LineScheduler {
    shared: Arc<Shared>,
    stocks: Vec<ContractQuery>,
}

impl LineScheduler {
    pub fn new(config: BudgetConfig, stocks: Vec<ContractQuery>) -> Result<Self> {
        config.validate()?;
        ensure!(
            stocks.len() <= config.stock_lines
                && stocks
                    .iter()
                    .all(|query| matches!(query, ContractQuery::Equity { .. }))
                && stocks
                    .iter()
                    .enumerate()
                    .all(|(index, query)| !stocks[..index].contains(query)),
            "watchlist must contain distinct equity queries within the stock allocation"
        );
        let (updates, _) = broadcast::channel(4096);
        let state = State {
            coverage: Coverage::new(config.coverage_retention),
            phase: SchedulerPhase::Starting,
            options: BTreeMap::new(),
            live: HashMap::new(),
            spots: HashMap::new(),
            selection: Selection::default(),
            discovery_complete: false,
            generation: 0,
            cooldown: None,
        };
        Ok(Self {
            shared: Arc::new(Shared {
                config,
                updates,
                changed: Notify::new(),
                state: Mutex::new(state),
                start: Utc::now(),
                clock: Instant::now(),
            }),
            stocks,
        })
    }

    pub fn handle(&self) -> LineHandle {
        LineHandle(self.shared.clone())
    }
}

struct Shared {
    config: BudgetConfig,
    updates: broadcast::Sender<LineUpdate>,
    changed: Notify,
    state: Mutex<State>,
    start: DateTime<Utc>,
    clock: Instant,
}

impl Shared {
    fn now(&self) -> DateTime<Utc> {
        self.start + TimeDelta::from_std(self.clock.elapsed()).expect("scheduler clock range")
    }
}

#[derive(Default)]
struct Selection {
    focus: Option<i32>,
    hot: Vec<i32>,
}

struct State {
    coverage: Coverage,
    phase: SchedulerPhase,
    options: BTreeMap<i32, ConfirmedContract>,
    live: HashMap<RequestKey, LiveLine>,
    spots: HashMap<i32, DecimalText>,
    selection: Selection,
    discovery_complete: bool,
    generation: u64,
    cooldown: Option<Instant>,
}

impl State {
    fn current_options(&self, as_of: DateTime<Utc>) -> impl Iterator<Item = &ConfirmedContract> {
        let today = option_market_date(as_of).ok();
        self.options
            .values()
            .filter(move |contract| today.is_some_and(|today| unexpired(contract, today)))
    }
}

fn unexpired(contract: &ConfirmedContract, today: NaiveDate) -> bool {
    contract.con_id > 0
        && matches!(&contract.kind, ContractKind::Option(option) if option.expiry >= today)
}

struct LiveLine {
    pool: LinePool,
    generation: u64,
    receiving: bool,
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::TimeZone;
    use finance_ib::market::{
        LiquidHours, MarketDataType, OptionContract, OptionRight, QuoteField,
    };

    use crate::{
        market_ingress::ProductPolicy,
        option_flow::{FlowUse, IBKR, LiveOptionFlow, OPRA, observation::ObservationEngine},
    };

    use super::*;

    fn option(con_id: i32, expiry: NaiveDate) -> ConfirmedContract {
        ConfirmedContract {
            con_id,
            symbol: "SYNTH".into(),
            primary_exchange: None,
            kind: ContractKind::Option(OptionContract {
                expiry,
                strike: "100".parse().unwrap(),
                right: OptionRight::Call,
                multiplier: "100".parse().unwrap(),
                underlying_con_id: 1,
            }),
            liquid_hours: Vec::new(),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn disconnected_chain_excludes_prior_day_options_after_eastern_midnight() {
        let mut scheduler = LineScheduler::new(BudgetConfig::default(), Vec::new()).unwrap();
        // In July, 04:00 UTC is midnight Eastern. The initial UTC date is already July 9.
        let before_midnight = Utc.with_ymd_and_hms(2025, 7, 9, 3, 59, 59).unwrap();
        Arc::get_mut(&mut scheduler.shared).unwrap().start = before_midnight;
        let handle = scheduler.handle();
        let today = NaiveDate::from_ymd_opt(2025, 7, 8).unwrap();
        let expiring = option(2, today);
        let following = option(3, today.succ_opt().unwrap());
        let key = session::option_key(expiring.con_id);
        let observed_at = before_midnight - TimeDelta::seconds(4);
        let closed_at = before_midnight - TimeDelta::seconds(1);
        {
            let mut state = scheduler.shared.state.lock().unwrap();
            state.phase = SchedulerPhase::Reconnecting;
            for contract in [&expiring, &following] {
                state.options.insert(contract.con_id, contract.clone());
            }
            state.coverage.opened(key, observed_at);
            state.coverage.observed(key, observed_at);
            state.coverage.closed(key, closed_at);
        }

        assert_eq!(handle.option_contracts(), [expiring, following.clone()]);
        assert_eq!(handle.status().confirmed_options, 2);
        assert_eq!(handle.status().observed_options, 1);

        tokio::time::advance(Duration::from_secs(1)).await;
        let status = handle.status();
        assert_eq!(status.phase, SchedulerPhase::Reconnecting);
        assert_eq!(status.confirmed_options, 1);
        assert_eq!(status.observed_options, 0);
        assert_eq!(
            handle.option_contracts().as_slice(),
            std::slice::from_ref(&following)
        );
        let report = handle.coverage(key, observed_at, status.as_of).unwrap();
        assert_eq!(report.last_observed_at, Some(observed_at));
        assert_eq!(
            report.covered,
            [CoverageInterval {
                start: observed_at,
                end: closed_at
            }]
        );
        assert_eq!(
            report.uncovered,
            [CoverageInterval {
                start: closed_at,
                end: status.as_of
            }]
        );

        scheduler.shared.state.lock().unwrap().phase = SchedulerPhase::Stopped;
        assert_eq!(handle.option_contracts(), [following]);
        assert_eq!(handle.status().confirmed_options, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn live_option_flow_keeps_equal_receipt_times_without_inventing_coverage() {
        let mut scheduler = LineScheduler::new(BudgetConfig::default(), Vec::new()).unwrap();
        let open = Utc.with_ymd_and_hms(2040, 1, 20, 14, 30, 0).unwrap();
        Arc::get_mut(&mut scheduler.shared).unwrap().start = open;
        let mut contract = option(2, NaiveDate::from_ymd_opt(2040, 1, 21).unwrap());
        contract.liquid_hours.push(LiquidHours::Open {
            start: open,
            end: open + TimeDelta::hours(1),
        });
        let key = session::option_key(contract.con_id);
        {
            let mut state = scheduler.shared.state.lock().unwrap();
            state.options.insert(contract.con_id, contract);
            state.coverage.opened(key, open);
            state.coverage.observed(key, open);
        }
        let mut flow = LiveOptionFlow::watch(
            scheduler.handle(),
            ObservationEngine::default(),
            &ProductPolicy {
                source: IBKR.parse().unwrap(),
                product: OPRA.parse().unwrap(),
                license: "synthetic".parse().unwrap(),
                display_live: true,
                record_local: false,
                derive_non_display: false,
                backup_personal_drive: false,
            },
            FlowUse::PrivateDisplay,
        )
        .unwrap();

        let send = |event| {
            scheduler
                .shared
                .updates
                .send(LineUpdate {
                    key,
                    generation: 1,
                    message: LineMessage::Event(event),
                })
                .unwrap();
        };
        let volume = |value: &str, received_at| MarketEvent::Quote {
            field: QuoteField::CumulativeVolume,
            value: Some(value.parse().unwrap()),
            delayed: false,
            received_at,
        };
        send(MarketEvent::DataType(Some(MarketDataType::Realtime)));
        send(volume("20", open));
        send(volume("24", open));
        let same_time = flow.next().await.unwrap();
        assert_eq!(same_time.clue.window(), (open, open));
        assert!(same_time.coverage.is_none());
        assert_eq!(same_time.budget.confirmed_options, 1);

        tokio::time::advance(Duration::from_secs(1)).await;
        let next_second = open + TimeDelta::seconds(1);
        send(volume("25", next_second));
        let later = flow.next().await.unwrap();
        assert_eq!(later.clue.window(), (open, next_second));
        let report = later.coverage.unwrap();
        assert_eq!(
            report.covered,
            [CoverageInterval {
                start: open,
                end: next_second,
            }]
        );
        assert!(report.uncovered.is_empty());
        assert!(report.unknown.is_empty());
    }
}
