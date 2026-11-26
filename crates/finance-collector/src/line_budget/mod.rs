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
use chrono::{DateTime, TimeDelta, Utc};
use finance_ib::market::{ConfirmedContract, ContractQuery, MarketEvent, RequestKey};
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
        BudgetStatus {
            phase: state.phase,
            as_of,
            account_lines: self.0.config.account_lines,
            reserve_lines: self.0.config.reserve_lines,
            stock_lines: stock,
            hot_lines: hot,
            rotation_lines: rotation,
            confirmed_options: state.options.len(),
            observed_options: state
                .options
                .keys()
                .filter(|&&id| {
                    state
                        .coverage
                        .last_observed_at(session::option_key(id))
                        .is_some_and(|receipt| receipt >= oldest)
                })
                .count(),
            discovery_complete: state.discovery_complete,
        }
    }

    /// Current confirmed (unexpired) chain, ordered by conId. Check discovery_complete
    /// before treating the size as the denominator for this watchlist.
    pub fn option_contracts(&self) -> Vec<ConfirmedContract> {
        self.0
            .state
            .lock()
            .expect("line state")
            .options
            .values()
            .cloned()
            .collect()
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

struct LiveLine {
    pool: LinePool,
    generation: u64,
    receiving: bool,
}
