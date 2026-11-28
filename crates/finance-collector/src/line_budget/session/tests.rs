use std::{
    collections::{HashSet, VecDeque},
    sync::{Arc, Mutex},
};

use chrono::{NaiveDate, TimeDelta, Utc};
use finance_ib::depth::{
    DepthPermissions, DepthRequest, DepthState, DepthSubscription, DepthSymbol, DepthUpdate,
};
use finance_ib::market::{
    MarketDataSource, MarketDataType, MarketEvent, MarketNotice, MarketSubscription,
    OptionContract, OptionRight, QuoteField,
};
use ibapi::{
    Client,
    contracts::{Contract, SecurityType},
    market_data::realtime::ExactMarketDepth,
    replay::{MarketReplay, MarketWrite},
    subscriptions::Subscription,
};
use tokio::{sync::oneshot, task::JoinHandle, time::advance};

use super::*;
use crate::line_budget::{
    BudgetConfig, CoverageInterval, DepthFrames, LineHandle, LineMessage, SubscriptionDemandHandle,
    SubscriptionInstrumentRegistry, watchlist,
};

const FIRST_OPTION: i32 = 10_000;

#[derive(Clone, Copy, Eq, PartialEq, Debug)]
enum Operation {
    Open,
    Confirm,
    Definitions,
    Resolve,
    Subscribe(RequestKey),
    Cancel(RequestKey),
    FailedCancel(RequestKey),
    DepthOpen(DepthSymbol),
    DepthCancel(DepthSymbol),
    Close,
}

#[derive(Clone, Copy, Debug)]
struct Call {
    time: Instant,
    session: usize,
    operation: Operation,
}

#[derive(Default)]
struct GatewayState {
    calls: Vec<Call>,
    active: HashSet<RequestKey>,
    seen_options: HashSet<i32>,
    peak: usize,
    current_session: usize,
    disconnect: Option<watch::Sender<bool>>,
    global_pacing: Option<watch::Sender<Option<Instant>>>,
    depth_pacing: Option<oneshot::Sender<(DepthSymbol, Instant, usize)>>,
    depth_client: Option<Arc<Client>>,
    depth_confirmations: Vec<DepthSymbol>,
    failed_opens: usize,
    pace_on_failed_open: bool,
    fail_next_cancel: bool,
    throttle_next_option: bool,
    first_equity_timeouts: usize,
    first_equity_timeout_delay: Option<Duration>,
    option_underlying: Option<i32>,
    incomplete_next_series: bool,
}

#[derive(Clone)]
struct Gateway {
    state: Arc<Mutex<GatewayState>>,
    options: usize,
    limit: usize,
}

impl Gateway {
    fn new(options: usize, config: &BudgetConfig) -> Self {
        Self {
            state: Arc::default(),
            options,
            limit: config.account_lines - config.reserve_lines,
        }
    }

    async fn open(
        self,
        pacing: watch::Sender<Option<Instant>>,
    ) -> Result<FakeSession, MarketError> {
        let (session, disconnected) = {
            let mut state = self.state.lock().unwrap();
            let session = state.current_session + 1;
            state.record(session, Operation::Open);
            if state.failed_opens > 0 {
                state.failed_opens -= 1;
                if std::mem::take(&mut state.pace_on_failed_open) {
                    pacing.send_replace(Some(Instant::now()));
                }
                return Err(MarketError::Disconnected);
            }
            assert!(state.active.is_empty());
            let (disconnect, disconnected) = watch::channel(false);
            state.current_session = session;
            state.disconnect = Some(disconnect);
            state.global_pacing = Some(pacing);
            (session, disconnected)
        };
        Ok(FakeSession {
            gateway: self,
            session,
            disconnected,
            confirmed: Mutex::default(),
        })
    }

    fn disconnect(&self, failed_opens: usize) {
        let mut state = self.state.lock().unwrap();
        state.failed_opens = failed_opens;
        state.disconnect.as_ref().unwrap().send(true).unwrap();
    }

    fn global_pacing(&self) -> Instant {
        let now = Instant::now();
        let state = self.state.lock().unwrap();
        state
            .global_pacing
            .as_ref()
            .unwrap()
            .send_replace(Some(now));
        now
    }

    fn calls(&self) -> Vec<Call> {
        self.state.lock().unwrap().calls.clone()
    }

    fn count(&self, session: usize, operation: Operation) -> usize {
        self.state
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter(|call| call.session == session && call.operation == operation)
            .count()
    }
}

impl GatewayState {
    fn record(&mut self, session: usize, operation: Operation) {
        self.calls.push(Call {
            time: Instant::now(),
            session,
            operation,
        });
    }
}

struct FakeSession {
    gateway: Gateway,
    session: usize,
    disconnected: watch::Receiver<bool>,
    confirmed: Mutex<HashSet<i32>>,
}

#[async_trait]
impl Session for FakeSession {
    async fn disconnected(&self) -> MarketError {
        let _ = self.disconnected.clone().wait_for(|lost| *lost).await;
        MarketError::Disconnected
    }

    async fn close(&self) {
        let client = self.gateway.state.lock().unwrap().depth_client.take();
        if let Some(client) = client {
            // With paused time, ready SDK Drop tasks run before this timer. This
            // reproduces a second executor thread sending before close takes effect.
            tokio::time::sleep(Duration::from_millis(1)).await;
            client.disconnect().await;
        }
        let mut state = self.gateway.state.lock().unwrap();
        state.record(self.session, Operation::Close);
        state.active.clear();
        state.disconnect = None;
        state.global_pacing = None;
    }
}

struct FakeDepth {
    gateway: Gateway,
    session: usize,
    symbol: DepthSymbol,
    subscription: Option<Subscription<ExactMarketDepth>>,
}

#[async_trait]
impl DepthSource for FakeSession {
    type Subscription = FakeDepth;

    async fn confirm_depth(&self, symbol: DepthSymbol) -> Result<ConfirmedContract, DepthError> {
        self.gateway
            .state
            .lock()
            .unwrap()
            .depth_confirmations
            .push(symbol);
        Ok(self.confirm(&symbol.contract_query()).await?)
    }

    async fn subscribe_depth(&self, request: &DepthRequest) -> Result<FakeDepth, DepthError> {
        let symbol = request.contract().symbol.parse().unwrap();
        let client = self.gateway.state.lock().unwrap().depth_client.clone();
        let subscription = if let Some(client) = client {
            let contract = Contract {
                contract_id: request.contract().con_id,
                security_type: SecurityType::Stock,
                exchange: "ISLAND".into(),
                currency: "USD".into(),
                ..Contract::default()
            };
            Some(
                client
                    .market_depth(&contract, request.rows().try_into().unwrap())
                    .subscribe_exact()
                    .await
                    .unwrap(),
            )
        } else {
            None
        };
        self.gateway
            .state
            .lock()
            .unwrap()
            .record(self.session, Operation::DepthOpen(symbol));
        Ok(FakeDepth {
            gateway: self.gateway.clone(),
            session: self.session,
            symbol,
            subscription,
        })
    }
}

#[async_trait]
impl DepthSubscription for FakeDepth {
    fn request_id(&self) -> i32 {
        1
    }

    async fn next(&mut self) -> Result<Option<DepthUpdate>, DepthError> {
        {
            let mut state = self.gateway.state.lock().unwrap();
            if let Some(notice) = state.depth_pacing.take() {
                notice
                    .send((self.symbol, Instant::now(), state.calls.len()))
                    .unwrap();
                return Err(DepthError::Pacing);
            }
        }
        pending().await
    }

    async fn cancel(&mut self) -> Result<(), DepthError> {
        if let Some(subscription) = self.subscription.take() {
            subscription.try_cancel().await.unwrap();
        }
        self.gateway
            .state
            .lock()
            .unwrap()
            .record(self.session, Operation::DepthCancel(self.symbol));
        Ok(())
    }
}

#[async_trait]
impl MarketDataSource for FakeSession {
    type Subscription = FakeSubscription;

    async fn confirm(&self, query: &ContractQuery) -> Result<ConfirmedContract, MarketError> {
        let ContractQuery::Equity {
            symbol,
            primary_exchange,
        } = query
        else {
            return Err(MarketError::InvalidRequest);
        };
        let index = watchlist()
            .iter()
            .position(|candidate| candidate == query)
            .unwrap();
        let con_id = i32::try_from(index).unwrap() + 1;
        let timeout_delay = {
            let mut state = self.gateway.state.lock().unwrap();
            state.record(self.session, Operation::Confirm);
            if con_id == 1 && state.first_equity_timeouts > 0 {
                state.first_equity_timeouts -= 1;
                return Err(MarketError::Timeout);
            }
            if con_id == 1 {
                state.first_equity_timeout_delay
            } else {
                None
            }
        };
        if let Some(delay) = timeout_delay {
            tokio::time::sleep(delay).await;
            return Err(MarketError::Timeout);
        }
        self.confirmed.lock().unwrap().insert(con_id);
        Ok(ConfirmedContract {
            con_id,
            symbol: symbol.clone(),
            primary_exchange: Some(primary_exchange.clone()),
            kind: ContractKind::Equity,
            liquid_hours: Vec::new(),
        })
    }

    async fn subscribe(&self, request: &MarketRequest) -> Result<Self::Subscription, MarketError> {
        let key = request.key();
        let mut state = self.gateway.state.lock().unwrap();
        state.record(self.session, Operation::Subscribe(key));
        // Confirmations are per session, as in the real PaperMarket adapter.
        assert!(
            self.confirmed.lock().unwrap().contains(&key.con_id),
            "unconfirmed key in session {}: {key:?}",
            self.session
        );
        assert!(
            state.active.insert(key),
            "duplicate upstream subscription: {key:?}"
        );
        state.peak = state.peak.max(state.active.len());
        assert!(state.peak <= self.gateway.limit, "reserve was used");
        if key.product == Product::Opra {
            assert!(key.parameters.option_open_interest);
            state.seen_options.insert(key.con_id);
        }
        let mut events = VecDeque::from([
            MarketEvent::DataType(Some(MarketDataType::Realtime)),
            MarketEvent::Quote {
                field: QuoteField::LastPrice,
                value: Some("100".parse().unwrap()),
                delayed: false,
                received_at: Utc::now(),
            },
        ]);
        if key.product == Product::Opra && std::mem::take(&mut state.throttle_next_option) {
            events.push_back(MarketEvent::Notice(MarketNotice::Pacing));
        }
        Ok(FakeSubscription {
            gateway: self.gateway.clone(),
            session: self.session,
            key,
            events,
        })
    }
}

#[async_trait]
impl OptionChainSource for FakeSession {
    async fn option_series(
        &self,
        equity: &ConfirmedContract,
    ) -> Result<Vec<OptionSeries>, MarketError> {
        let option_underlying = {
            let mut state = self.gateway.state.lock().unwrap();
            state.record(self.session, Operation::Definitions);
            state.option_underlying.unwrap_or(1)
        };
        if equity.con_id != option_underlying {
            return Ok(Vec::new());
        }
        Ok(vec![OptionSeries {
            expiry: NaiveDate::from_ymd_opt(2099, 1, 2).unwrap(),
            trading_class: "SYNTH".into(),
            multiplier: "100".parse().unwrap(),
            strikes: (0..self.gateway.options.div_ceil(2))
                .map(|index| (100 + index).to_string().parse().unwrap())
                .collect(),
        }])
    }

    async fn confirm_series(
        &self,
        equity: &ConfirmedContract,
        series: &OptionSeries,
    ) -> Result<Vec<ConfirmedContract>, MarketError> {
        {
            let mut state = self.gateway.state.lock().unwrap();
            state.record(self.session, Operation::Resolve);
            if std::mem::take(&mut state.incomplete_next_series) {
                return Err(MarketError::IncompleteResponse);
            }
        }
        let mut confirmed = self.confirmed.lock().unwrap();
        // Fixed synthetic input: one expiry, integer strikes 100 upward, call
        // then put at each strike, and sequential conIds starting at 10000.
        Ok((0..self.gateway.options)
            .map(|index| {
                let con_id = FIRST_OPTION + i32::try_from(index).unwrap();
                confirmed.insert(con_id);
                ConfirmedContract {
                    con_id,
                    symbol: "SYNTH".into(),
                    primary_exchange: None,
                    liquid_hours: Vec::new(),
                    kind: ContractKind::Option(OptionContract {
                        expiry: series.expiry,
                        strike: series.strikes[index / 2].clone(),
                        right: if index % 2 == 0 {
                            OptionRight::Call
                        } else {
                            OptionRight::Put
                        },
                        multiplier: series.multiplier.clone(),
                        underlying_con_id: equity.con_id,
                    }),
                }
            })
            .collect())
    }
}

struct FakeSubscription {
    gateway: Gateway,
    session: usize,
    key: RequestKey,
    events: VecDeque<MarketEvent>,
}

#[async_trait]
impl MarketSubscription for FakeSubscription {
    fn request_id(&self) -> i32 {
        self.key.con_id
    }

    async fn next(&mut self) -> Result<Option<MarketEvent>, MarketError> {
        if let Some(event) = self.events.pop_front() {
            Ok(Some(event))
        } else {
            std::future::pending().await
        }
    }

    async fn cancel(&mut self) -> Result<(), MarketError> {
        let mut state = self.gateway.state.lock().unwrap();
        if std::mem::take(&mut state.fail_next_cancel) {
            state.record(self.session, Operation::FailedCancel(self.key));
            return Err(MarketError::Timeout);
        }
        state.record(self.session, Operation::Cancel(self.key));
        assert!(state.active.remove(&self.key));
        Ok(())
    }
}

fn start(
    config: BudgetConfig,
    options: usize,
) -> (
    Gateway,
    LineHandle,
    watch::Sender<bool>,
    JoinHandle<Result<(), MarketError>>,
) {
    let gateway = Gateway::new(options, &config);
    let mut equities = watchlist();
    equities.truncate(config.stock_lines);
    let scheduler = LineScheduler::new(config, equities).unwrap();
    let handle = scheduler.handle();
    let (stop, stopped) = watch::channel(false);
    let fake = gateway.clone();
    let task = tokio::spawn(scheduler.run(move |pacing| fake.clone().open(pacing), stopped));
    (gateway, handle, stop, task)
}

async fn tick(period: Duration) {
    advance(period).await;
    // Run the driver's scheduled action and the spawned subscription reader.
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
}

async fn until(steps: usize, tick_size: Duration, mut condition: impl FnMut() -> bool) {
    for _ in 0..steps {
        if condition() {
            return;
        }
        tick(tick_size).await;
    }
    assert!(
        condition(),
        "condition not reached in {steps} virtual clock steps"
    );
}

fn check_request_spacing(gateway: &Gateway, interval: Duration) {
    let requests: Vec<_> = gateway
        .calls()
        .into_iter()
        .filter(|call| !matches!(call.operation, Operation::Open | Operation::Close))
        .collect();
    for window in requests.windows(2) {
        assert!(
            window[1].time.duration_since(window[0].time) >= interval,
            "unpaced requests: {window:?}"
        );
    }
}

fn compact_config() -> BudgetConfig {
    BudgetConfig {
        account_lines: 5,
        stock_lines: 1,
        hot_lines: 1,
        rotation_lines: 1,
        reserve_lines: 2,
        ..BudgetConfig::default()
    }
}

#[tokio::test(start_paused = true)]
async fn runtime_demand_reaches_line_and_depth_scheduling_across_reconnect() {
    // Fixed in-memory paper-session replay. No broker client is opened.
    let config = BudgetConfig {
        account_lines: 6,
        stock_lines: 2,
        ..compact_config()
    };
    let step = config.request_interval;
    let gateway = Gateway::new(0, &config);
    let frames = DepthFrames::default();
    let (scheduler, handle) = LineScheduler::new(config, watchlist().into_iter().take(2).collect())
        .unwrap()
        .with_subscription_demand(Some((
            frames.clone(),
            DepthPermissions {
                equity_l1_confirmed: true,
                totalview_confirmed: true,
                eds_confirmed: true,
            },
        )));
    let registry = SubscriptionInstrumentRegistry::default();
    let demand = |focus: Option<&str>| {
        registry
            .validate(
                vec!["SPY.ARCA".parse().unwrap()],
                focus.map(|id| id.parse().unwrap()),
            )
            .unwrap()
    };
    handle.replace(demand(Some("NVDA.NASDAQ"))).unwrap();
    let (stop, stopped) = watch::channel(false);
    let fake = gateway.clone();
    let task = tokio::spawn(scheduler.run(move |pacing| fake.clone().open(pacing), stopped));

    until(20, step, || {
        gateway.count(1, Operation::Subscribe(stock_key(1))) == 1
    })
    .await;
    let first_stock = gateway
        .calls()
        .iter()
        .find_map(|call| match call.operation {
            Operation::Subscribe(key) if key.product == Product::EquityL1 => Some(key),
            _ => None,
        });
    assert_eq!(
        first_stock,
        Some(stock_key(2)),
        "requested SPY must be scheduled before QQQ"
    );
    assert_eq!(gateway.count(1, Operation::DepthOpen(DepthSymbol::Nvda)), 1);

    handle.replace(demand(Some("TSLA.NASDAQ"))).unwrap();
    until(20, step, || {
        gateway.count(1, Operation::DepthOpen(DepthSymbol::Tsla)) == 1
    })
    .await;
    let calls = gateway.calls();
    let cancel = calls
        .iter()
        .position(|call| call.operation == Operation::DepthCancel(DepthSymbol::Nvda))
        .unwrap();
    let open = calls
        .iter()
        .position(|call| call.operation == Operation::DepthOpen(DepthSymbol::Tsla))
        .unwrap();
    assert!(cancel < open);
    assert_eq!(gateway.count(1, Operation::DepthOpen(DepthSymbol::Qqq)), 1);
    assert_eq!(gateway.count(1, Operation::DepthOpen(DepthSymbol::Aapl)), 1);
    assert!(frames.latest(DepthSymbol::Tsla).is_some());

    gateway.disconnect(0);
    until(20, step, || {
        gateway.count(2, Operation::DepthOpen(DepthSymbol::Tsla)) == 1
    })
    .await;
    assert!(frames.latest(DepthSymbol::Nvda).is_none());
    handle.replace(SubscriptionDemand::default()).unwrap();
    until(20, step, || {
        gateway.count(2, Operation::DepthCancel(DepthSymbol::Tsla)) == 1
    })
    .await;
    assert_eq!(
        gateway.count(2, Operation::DepthCancel(DepthSymbol::Aapl)),
        0
    );
    assert_eq!(
        gateway.count(2, Operation::DepthCancel(DepthSymbol::Qqq)),
        0
    );
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
    assert!(frames.latest(DepthSymbol::Tsla).is_none());
    assert!(handle.current().is_none());
}

fn start_demand_owner(
    scheduler: LineScheduler,
    gateway: &Gateway,
    depth: bool,
) -> (
    SubscriptionDemandHandle,
    watch::Sender<bool>,
    JoinHandle<Result<(), MarketError>>,
) {
    let (scheduler, demand) = scheduler.with_subscription_demand(depth.then(|| {
        (
            DepthFrames::default(),
            DepthPermissions {
                equity_l1_confirmed: true,
                totalview_confirmed: true,
                eds_confirmed: true,
            },
        )
    }));
    let (stop, stopped) = watch::channel(false);
    let fake = gateway.clone();
    let task = tokio::spawn(scheduler.run(move |pacing| fake.clone().open(pacing), stopped));
    (demand, stop, task)
}

async fn verify_depth_teardown_during_cooldown(disconnect: bool) {
    let config = compact_config();
    let step = config.request_interval;
    let cooldown = config.pacing_cooldown;
    let gateway = Gateway::new(0, &config);
    // The synthetic contracts use the real SDK subscription, Drop and message
    // bus over in-memory IO. No broker address or account is involved.
    let (replay, client) = MarketReplay::open().unwrap();
    gateway.state.lock().unwrap().depth_client = Some(Arc::new(client));
    let scheduler = LineScheduler::new(config, vec![watchlist().remove(0)]).unwrap();
    let (demand, stop, task) = start_demand_owner(scheduler, &gateway, true);
    let focus = |symbol: &str| {
        SubscriptionInstrumentRegistry::default()
            .validate(Vec::new(), Some(symbol.parse().unwrap()))
            .unwrap()
    };
    demand.replace(focus("NVDA.NASDAQ")).unwrap();
    until(20, step, || {
        gateway.count(1, Operation::DepthOpen(DepthSymbol::Nvda)) == 1
    })
    .await;
    let opened = replay.writes();
    assert_eq!(opened.len(), 3);
    for (packet, id) in opened.iter().zip([1, 3, 5]) {
        let MarketWrite::SubscribeDepth {
            con_id,
            exchange,
            smart_depth,
            ..
        } = packet
        else {
            panic!("unexpected depth request: {packet:?}");
        };
        assert_eq!(
            (*con_id, exchange.as_deref(), *smart_depth),
            (Some(id), Some("ISLAND"), false)
        );
    }
    let (held_write, deadline) = if disconnect {
        // The old page cancel is already on the wire but its write has not returned.
        let hold = replay.hold_next_write();
        demand.replace(focus("TSLA.NASDAQ")).unwrap();
        until(8, step, || {
            matches!(
                replay.writes().last(),
                Some(MarketWrite::CancelDepth { .. })
            )
        })
        .await;
        assert_eq!(replay.writes().len(), 4);
        (Some(hold), gateway.global_pacing() + cooldown)
    } else {
        let deadline = gateway.global_pacing() + cooldown;
        demand.replace(focus("TSLA.NASDAQ")).unwrap();
        (None, deadline)
    };
    let before = replay.writes();
    tick(step).await;
    assert_eq!(replay.writes(), before);

    let stop_after_reconnect = if disconnect {
        // A terminal farm notice can invalidate an otherwise writable connection.
        gateway.disconnect(0);
        until(4, step, || gateway.count(1, Operation::Close) == 1).await;
        Some(stop)
    } else {
        // finance-api signals shutdown by dropping this sender.
        drop(stop);
        until(4, step, || task.is_finished()).await;
        None
    };
    assert!(
        Instant::now() < deadline,
        "session teardown waited for cooldown"
    );
    assert!(replay.is_closed());
    if let Some(hold) = held_write {
        assert!(hold.send(()).is_err(), "old write survived session close");
    }
    assert_eq!(gateway.count(1, Operation::Close), 1);
    assert_eq!(gateway.count(2, Operation::Open), 0);
    tick(deadline - Instant::now() - step).await;
    assert_eq!(gateway.count(2, Operation::Open), 0);
    if let Some(stop) = stop_after_reconnect {
        until(20, step, || {
            gateway.count(2, Operation::DepthOpen(DepthSymbol::Tsla)) == 1
        })
        .await;
        stop.send(true).unwrap();
    }
    task.await.unwrap().unwrap();
    tick(cooldown).await; // Flush any SDK cancellation still queued after teardown.
    assert_eq!(
        replay.writes(),
        before,
        "SDK Drop wrote an unpaced depth cancel"
    );
}

#[tokio::test(start_paused = true)]
async fn stop_during_focus_cooldown_closes_the_session_before_sdk_depth_drop() {
    verify_depth_teardown_during_cooldown(false).await;
}

#[tokio::test(start_paused = true)]
async fn disconnect_during_focus_cooldown_retires_parked_sdk_write_before_reconnect() {
    verify_depth_teardown_during_cooldown(true).await;
}

#[derive(Clone, Copy)]
enum PendingDepthWrite {
    Cancel,
    Confirm,
    Subscribe,
}

async fn supersede_depth_focus(pending: PendingDepthWrite, replacement: Option<DepthSymbol>) {
    // Fixed watchlist IDs and virtual time drive the same owner channel as GET/PUT.
    let config = compact_config();
    let step = config.request_interval;
    let cooldown = config.pacing_cooldown;
    let gateway = Gateway::new(0, &config);
    let scheduler = LineScheduler::new(config, vec![watchlist().remove(0)]).unwrap();
    let frames = DepthFrames::default();
    let (scheduler, demand) = scheduler.with_subscription_demand(Some((
        frames.clone(),
        DepthPermissions {
            equity_l1_confirmed: true,
            totalview_confirmed: true,
            eds_confirmed: true,
        },
    )));
    let registry = SubscriptionInstrumentRegistry::default();
    let select = |symbol: Option<DepthSymbol>| {
        let id = symbol.map(|symbol| format!("{}.NASDAQ", symbol.as_str()).parse().unwrap());
        let snapshot = registry.validate(Vec::new(), id).unwrap();
        assert_eq!(demand.replace(snapshot).unwrap().depth_focus(), symbol);
        assert_eq!(demand.current().unwrap().depth_focus(), symbol);
    };
    let confirmations = |symbol| {
        gateway
            .state
            .lock()
            .unwrap()
            .depth_confirmations
            .iter()
            .filter(|&&s| s == symbol)
            .count()
    };
    select(Some(DepthSymbol::Nvda));
    let (stop, stopped) = watch::channel(false);
    let fake = gateway.clone();
    let task = tokio::spawn(scheduler.run(move |pacing| fake.clone().open(pacing), stopped));
    until(24, step, || {
        gateway.count(1, Operation::DepthOpen(DepthSymbol::Nvda)) == 1
    })
    .await;

    if matches!(pending, PendingDepthWrite::Cancel) {
        gateway.global_pacing();
    }
    select(Some(DepthSymbol::Tsla));
    let old_cancel = Operation::DepthCancel(DepthSymbol::Nvda);
    until(20, step, || match pending {
        PendingDepthWrite::Cancel => frames
            .latest(DepthSymbol::Nvda)
            .is_some_and(|frame| frame.state == DepthState::Inactive),
        PendingDepthWrite::Confirm => gateway.count(1, old_cancel) == 1,
        PendingDepthWrite::Subscribe => confirmations(DepthSymbol::Tsla) == 1,
    })
    .await;
    assert_eq!(
        gateway.count(1, old_cancel),
        usize::from(!matches!(pending, PendingDepthWrite::Cancel))
    );
    let confirmed_before = usize::from(matches!(pending, PendingDepthWrite::Subscribe));
    assert_eq!(confirmations(DepthSymbol::Tsla), confirmed_before);
    assert_eq!(gateway.count(1, Operation::DepthOpen(DepthSymbol::Tsla)), 0);
    let before = gateway.calls().len();
    gateway.global_pacing();
    select(replacement);
    tick(cooldown - step).await;
    assert_eq!(
        gateway.calls().len(),
        before,
        "request sent during cooldown"
    );
    // Let the scheduler run past the cooldown and several shared write slots.
    for _ in 0..24 {
        tick(step).await;
    }
    assert_eq!(gateway.count(1, old_cancel), 1);
    assert_eq!(
        confirmations(DepthSymbol::Tsla),
        confirmed_before,
        "confirmed superseded focus"
    );
    assert_eq!(
        gateway.count(1, Operation::DepthOpen(DepthSymbol::Tsla)),
        0,
        "opened superseded focus"
    );
    assert_eq!(
        gateway.count(1, Operation::DepthCancel(DepthSymbol::Tsla)),
        0
    );
    assert!(
        frames
            .latest(DepthSymbol::Tsla)
            .is_none_or(|frame| frame.state == DepthState::Inactive)
    );
    if let Some(latest) = replacement {
        assert_eq!(confirmations(latest), 1);
        assert_eq!(gateway.count(1, Operation::DepthOpen(latest)), 1);
    }
    for pin in [DepthSymbol::Qqq, DepthSymbol::Aapl] {
        assert_eq!(gateway.count(1, Operation::DepthOpen(pin)), 1);
        assert_eq!(gateway.count(1, Operation::DepthCancel(pin)), 0);
    }
    assert_eq!(gateway.count(1, Operation::Close), 0);
    check_request_spacing(&gateway, step);
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn superseded_focus_cleared_while_old_depth_cancel_waits_for_cooldown() {
    supersede_depth_focus(PendingDepthWrite::Cancel, None).await;
}

#[tokio::test(start_paused = true)]
async fn superseded_focus_replaced_while_depth_confirmation_waits_for_a_slot() {
    supersede_depth_focus(PendingDepthWrite::Confirm, Some(DepthSymbol::Meta)).await;
}

#[tokio::test(start_paused = true)]
async fn superseded_focus_replaced_after_confirmation_before_depth_subscribe() {
    supersede_depth_focus(PendingDepthWrite::Subscribe, Some(DepthSymbol::Msft)).await;
}

#[tokio::test(start_paused = true)]
async fn demand_focus_and_option_rotation_share_spacing_and_extended_global_cooldown() {
    let config = BudgetConfig {
        rotation_dwell: Duration::from_secs(1),
        pacing_cooldown: Duration::from_secs(2),
        ..compact_config()
    };
    let step = config.request_interval;
    let cooldown = config.pacing_cooldown;
    let gateway = Gateway::new(2, &config);
    let scheduler = LineScheduler::new(config, vec![watchlist().remove(0)]).unwrap();
    let (demand, stop, task) = start_demand_owner(scheduler, &gateway, true);
    let focus = |symbol: &str| {
        SubscriptionInstrumentRegistry::default()
            .validate(Vec::new(), Some(symbol.parse().unwrap()))
            .unwrap()
    };
    demand.replace(focus("NVDA.NASDAQ")).unwrap();
    let rotation = option_key(FIRST_OPTION);
    until(30, step, || {
        gateway.count(1, Operation::DepthOpen(DepthSymbol::Nvda)) == 1
            && gateway.count(1, Operation::Subscribe(rotation)) == 1
    })
    .await;

    let prior = gateway.calls().len();
    gateway.global_pacing();
    demand.replace(focus("TSLA.NASDAQ")).unwrap();
    tick(cooldown / 2).await;
    assert_eq!(gateway.calls().len(), prior, "focus wrote during cooldown");
    let extended = gateway.global_pacing() + cooldown;
    tick(cooldown - step).await;
    assert_eq!(
        gateway.calls().len(),
        prior,
        "a second notice extended cooldown"
    );

    until(20, step, || {
        gateway.count(1, Operation::DepthOpen(DepthSymbol::Tsla)) == 1
            && gateway.count(1, Operation::Cancel(rotation)) >= 1
    })
    .await;
    let calls = gateway.calls();
    assert!(calls[prior..].iter().all(|call| call.time >= extended));
    assert_eq!(
        gateway.count(1, Operation::DepthCancel(DepthSymbol::Nvda)),
        1
    );
    assert_eq!(gateway.count(1, Operation::Close), 0);
    // The trace includes L1 and depth confirmations as well as subscribe and cancel.
    check_request_spacing(&gateway, step);
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn depth_stream_pacing_delays_its_cancel_and_pending_line_requests() {
    let config = BudgetConfig {
        pacing_cooldown: Duration::from_secs(2),
        ..compact_config()
    };
    let step = config.request_interval;
    let cooldown = config.pacing_cooldown;
    let gateway = Gateway::new(2, &config);
    let (notice, received) = oneshot::channel();
    gateway.state.lock().unwrap().depth_pacing = Some(notice);
    let scheduler = LineScheduler::new(config, vec![watchlist().remove(0)]).unwrap();
    let (_demand, stop, task) = start_demand_owner(scheduler, &gateway, true);
    let (symbol, at, before) = tokio::time::timeout(20 * step, received)
        .await
        .unwrap()
        .unwrap();
    tick(cooldown - step).await;
    assert_eq!(gateway.calls().len(), before);
    until(20, step, || {
        gateway.count(1, Operation::DepthCancel(symbol)) == 1
            && gateway.count(1, Operation::Subscribe(option_key(FIRST_OPTION))) == 1
    })
    .await;
    let calls = gateway.calls();
    assert!(
        calls[before..]
            .iter()
            .all(|call| call.time >= at + cooldown)
    );
    assert_eq!(gateway.count(1, Operation::Close), 0);
    check_request_spacing(&gateway, step);
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn demanded_stock_with_short_retry_yields_to_first_confirmation_and_option_chain() {
    let config = BudgetConfig {
        account_lines: 6,
        stock_lines: 2,
        request_interval: Duration::from_millis(250),
        reconnect_initial: Duration::from_millis(100),
        reconnect_max: Duration::from_millis(100),
        ..compact_config()
    };
    let step = config.request_interval;
    let gateway = Gateway::new(1, &config);
    {
        let mut state = gateway.state.lock().unwrap();
        state.first_equity_timeouts = usize::MAX;
        state.option_underlying = Some(2);
    }
    // Put SPY first in the base queue so the first QQQ timeout proves demand priority.
    let stocks = watchlist().into_iter().take(2).rev().collect();
    let scheduler = LineScheduler::new(config, stocks).unwrap();
    let lines = scheduler.handle();
    let (demand, stop, task) = start_demand_owner(scheduler, &gateway, false);
    demand
        .replace(
            SubscriptionInstrumentRegistry::default()
                .validate(vec!["QQQ.NASDAQ".parse().unwrap()], None)
                .unwrap(),
        )
        .unwrap();
    until(5, step, || {
        gateway.count(1, Operation::Subscribe(stock_key(2))) == 1
    })
    .await;
    assert_eq!(gateway.count(1, Operation::Confirm), 2);
    until(20, step, || lines.status().observed_options == 1).await;
    assert!(gateway.count(1, Operation::Confirm) > 2);
    assert_eq!(gateway.count(1, Operation::Subscribe(stock_key(1))), 0);
    assert_eq!(gateway.count(1, Operation::Close), 0);
    assert!(!lines.status().discovery_complete);
    check_request_spacing(&gateway, step);
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn stock_confirmation_timeout_retries_in_session_and_recovers_chain() {
    let config = BudgetConfig {
        account_lines: 6,
        stock_lines: 2,
        ..compact_config()
    };
    let step = config.request_interval;
    let initial_retry = config.reconnect_initial;
    let (gateway, handle, stop, task) = start(config, 1);
    gateway.state.lock().unwrap().first_equity_timeouts = 2;
    let timed_out_stock = stock_key(1);
    let other_stock = stock_key(2);
    until(8, step, || {
        gateway.count(1, Operation::Subscribe(other_stock)) == 1
    })
    .await;
    assert_eq!(gateway.count(1, Operation::Confirm), 2);
    assert_eq!(gateway.count(1, Operation::Subscribe(timed_out_stock)), 0);
    assert!(!handle.status().discovery_complete);

    until(40, step, || handle.status().discovery_complete).await;
    let stock_attempts: Vec<_> = gateway
        .calls()
        .into_iter()
        .filter(|call| call.operation == Operation::Confirm)
        .collect();
    assert_eq!(stock_attempts.len(), 4);
    assert!(stock_attempts.iter().all(|call| call.session == 1));
    assert!(stock_attempts[2].time >= stock_attempts[0].time + initial_retry);
    assert!(stock_attempts[3].time >= stock_attempts[2].time + 2 * initial_retry);
    assert_eq!(gateway.count(1, Operation::Subscribe(timed_out_stock)), 1);
    assert_eq!(gateway.count(1, Operation::Subscribe(other_stock)), 1);
    assert_eq!(gateway.count(1, Operation::Close), 0);
    assert_eq!(handle.status().stock_lines, 2);
    assert_eq!(handle.status().confirmed_options, 1);
    assert_eq!(handle.option_contracts()[0].con_id, FIRST_OPTION);
    check_request_spacing(&gateway, step);
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn persistent_stock_timeout_with_short_backoff_yields_to_other_stocks_and_chains() {
    let config = BudgetConfig {
        account_lines: 6,
        stock_lines: 2,
        request_interval: Duration::from_millis(250),
        reconnect_initial: Duration::from_millis(100),
        reconnect_max: Duration::from_millis(200),
        ..compact_config()
    };
    let step = config.request_interval;
    let (gateway, handle, stop, task) = start(config, 1);
    {
        let mut state = gateway.state.lock().unwrap();
        state.first_equity_timeout_delay = Some(Duration::from_secs(1));
        state.option_underlying = Some(2);
    }
    let healthy_stock = stock_key(2);
    until(12, step, || {
        gateway.count(1, Operation::Subscribe(healthy_stock)) == 1
    })
    .await;
    assert_eq!(gateway.count(1, Operation::Confirm), 2);

    until(36, step, || handle.status().observed_options == 1).await;
    assert!(gateway.count(1, Operation::Confirm) > 2);
    assert_eq!(gateway.count(1, Operation::Subscribe(stock_key(1))), 0);
    assert_eq!(gateway.count(1, Operation::Subscribe(healthy_stock)), 1);
    assert_eq!(gateway.count(1, Operation::Close), 0);
    let status = handle.status();
    assert_eq!(status.phase, SchedulerPhase::Online);
    assert_eq!(status.stock_lines, 1);
    assert_eq!(status.confirmed_options, 1);
    assert!(!status.discovery_complete);
    check_request_spacing(&gateway, step);
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn incomplete_series_cannot_complete_discovery_before_clean_refresh() {
    let config = BudgetConfig {
        chain_refresh: Duration::from_secs(3),
        ..compact_config()
    };
    let step = config.request_interval;
    let (gateway, handle, stop, task) = start(config, 1);
    gateway.state.lock().unwrap().incomplete_next_series = true;
    let key = option_key(FIRST_OPTION);
    until(15, step, || gateway.count(1, Operation::Resolve) == 1).await;
    for _ in 0..3 {
        tick(step).await;
        assert!(!handle.status().discovery_complete);
        assert!(handle.option_contracts().is_empty());
        assert_eq!(gateway.count(1, Operation::Subscribe(key)), 0);
    }
    until(20, step, || handle.status().discovery_complete).await;
    assert_eq!(gateway.count(1, Operation::Resolve), 2);
    assert_eq!(handle.status().confirmed_options, 1);
    until(5, step, || gateway.count(1, Operation::Subscribe(key)) == 1).await;
    assert_eq!(gateway.count(1, Operation::Close), 0);
    check_request_spacing(&gateway, step);
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn thousand_option_chain_stays_in_budget_and_every_contract_gets_a_turn() {
    let config = BudgetConfig::default();
    let step = config.request_interval;
    let (gateway, handle, stop, task) = start(config, 1_000);
    let hot = (FIRST_OPTION..FIRST_OPTION + 16).collect::<Vec<_>>();
    handle.set_selection(Some(1), &hot).unwrap();
    until(150, step, || {
        let status = handle.status();
        status.stock_lines == 9 && status.hot_lines == 16 && status.rotation_lines == 45
    })
    .await;
    let status = handle.status();
    assert_eq!((status.account_lines, status.reserve_lines), (100, 30));
    assert!(status.discovery_complete);
    assert_eq!(status.confirmed_options, 1_000);
    assert_eq!(handle.option_contracts().len(), 1_000);

    until(3_000, step, || {
        gateway.state.lock().unwrap().seen_options.len() == 1_000
    })
    .await;
    until(5, step, || handle.status().observed_options == 1_000).await;
    assert_eq!(gateway.state.lock().unwrap().peak, 70);
    for con_id in hot {
        assert_eq!(
            gateway.count(1, Operation::Subscribe(option_key(con_id))),
            1
        );
        assert_eq!(gateway.count(1, Operation::Cancel(option_key(con_id))), 0);
    }
    assert!(
        gateway
            .calls()
            .iter()
            .any(|call| matches!(call.operation, Operation::Cancel(_)))
    );
    check_request_spacing(&gateway, step);
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn independent_readers_share_one_rotation_generation_when_promoted_to_hot() {
    let config = compact_config();
    let step = config.request_interval;
    let (gateway, handle, stop, task) = start(config, 2);
    let key = option_key(FIRST_OPTION);
    let mut page = handle.reader(key);
    let mut background = handle.reader(key);
    until(15, step, || {
        gateway.count(1, Operation::Subscribe(key)) == 1
    })
    .await;
    let page_open = page.recv().await.unwrap();
    let background_open = background.recv().await.unwrap();
    assert_eq!(page_open, background_open);
    assert_eq!(page_open.message, LineMessage::Opened);

    handle
        .set_selection(Some(1), &[FIRST_OPTION, FIRST_OPTION])
        .unwrap();
    until(6, step, || handle.status().hot_lines == 1).await;
    let page_event = page.recv().await.unwrap();
    let background_event = background.recv().await.unwrap();
    assert_eq!(page_event, background_event);
    assert_eq!(page_event.generation, page_open.generation);
    assert!(matches!(page_event.message, LineMessage::Event(_)));
    assert_eq!(gateway.count(1, Operation::Subscribe(key)), 1);
    assert_eq!(gateway.count(1, Operation::Cancel(key)), 0);
    assert!(gateway.state.lock().unwrap().active.contains(&key));
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn disconnect_retries_back_off_and_reconfirm_before_resuming_with_queryable_gap() {
    let config = compact_config();
    let step = config.request_interval;
    let initial = config.reconnect_initial;
    let (gateway, handle, stop, task) = start(config, 1);
    let key = option_key(FIRST_OPTION);
    let before_tracking = handle.status().as_of - TimeDelta::milliseconds(1);
    handle.set_selection(Some(1), &[FIRST_OPTION]).unwrap();
    until(15, step, || handle.status().observed_options == 1).await;
    tick(step).await;
    gateway.disconnect(2);
    until(4, step, || gateway.count(1, Operation::Close) == 1).await;
    let offline_since = handle.status().as_of;
    assert_eq!(handle.status().phase, SchedulerPhase::Reconnecting);
    tick(step).await;
    assert_eq!(gateway.count(2, Operation::Open), 0);
    let offline_until = handle.status().as_of;

    until(100, step, || {
        gateway.count(2, Operation::Subscribe(key)) == 1
    })
    .await;
    tick(step).await;
    let calls = gateway.calls();
    let retry_times: Vec<_> = calls
        .iter()
        .filter(|call| call.session == 2 && call.operation == Operation::Open)
        .map(|call| call.time)
        .collect();
    assert_eq!(retry_times.len(), 3);
    let closed = calls
        .iter()
        .find(|call| call.session == 1 && call.operation == Operation::Close)
        .unwrap();
    assert!(retry_times[0] >= closed.time + initial);
    assert!(retry_times[1] >= retry_times[0] + initial * 2);
    assert!(retry_times[2] >= retry_times[1] + initial * 4);
    let recent: Vec<_> = calls
        .iter()
        .filter(|call| call.session == 2)
        .map(|call| call.operation)
        .collect();
    let resolve = recent
        .iter()
        .position(|call| *call == Operation::Resolve)
        .unwrap();
    let resubscribe = recent
        .iter()
        .position(|call| *call == Operation::Subscribe(key))
        .unwrap();
    assert!(resolve < resubscribe);
    check_request_spacing(&gateway, step);

    let end = handle.status().as_of;
    let report = handle.coverage(key, before_tracking, end).unwrap();
    assert_eq!(report.covered.len(), 2);
    assert!(!report.unknown.is_empty());
    assert!(
        report
            .uncovered
            .iter()
            .any(|gap| gap.start <= offline_since && gap.end >= offline_until)
    );
    assert!(
        report
            .last_observed_at
            .is_some_and(|observed| observed > offline_until)
    );
    let absent = handle
        .coverage(option_key(FIRST_OPTION + 1), before_tracking, end)
        .unwrap();
    assert_eq!(
        absent.unknown,
        [CoverageInterval {
            start: before_tracking,
            end
        }]
    );
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn ambiguous_cancel_closes_the_session_before_any_replacement() {
    let config = BudgetConfig {
        rotation_dwell: Duration::from_secs(1),
        ..compact_config()
    };
    let step = config.request_interval;
    let (gateway, handle, stop, task) = start(config, 2);
    let first = option_key(FIRST_OPTION);
    until(15, step, || handle.status().observed_options == 1).await;
    let sample = handle.status().as_of;
    gateway.state.lock().unwrap().fail_next_cancel = true;
    until(12, step, || gateway.count(1, Operation::Close) == 1).await;
    assert_eq!(gateway.count(1, Operation::FailedCancel(first)), 1);
    assert_eq!(
        gateway.count(1, Operation::Subscribe(option_key(FIRST_OPTION + 1))),
        0
    );
    assert_eq!(handle.status().phase, SchedulerPhase::Reconnecting);
    tick(step).await;
    let end = handle.status().as_of;
    let report = handle.coverage(first, sample, end).unwrap();
    assert!(!report.covered.is_empty());
    assert_eq!(report.uncovered.last().map(|gap| gap.end), Some(end));
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn pacing_notice_delays_writes_then_resumes_the_same_hot_contract() {
    let config = BudgetConfig {
        pacing_cooldown: Duration::from_secs(2),
        ..compact_config()
    };
    let step = config.request_interval;
    let cooldown = config.pacing_cooldown;
    let (gateway, handle, stop, task) = start(config, 1);
    let key = option_key(FIRST_OPTION);
    gateway.state.lock().unwrap().throttle_next_option = true;
    handle.set_selection(Some(1), &[FIRST_OPTION]).unwrap();
    until(15, step, || {
        gateway.count(1, Operation::Subscribe(key)) == 1
    })
    .await;
    let initial_requests = gateway.calls().len();
    for _ in 0..6 {
        tick(step).await;
    }
    assert_eq!(gateway.calls().len(), initial_requests);
    until(8, step, || gateway.count(1, Operation::Subscribe(key)) == 2).await;
    let writes: Vec<_> = gateway.calls().into_iter().filter(|call| {
        matches!(call.operation, Operation::Subscribe(candidate) | Operation::Cancel(candidate) if candidate == key)
    }).collect();
    assert_eq!(
        writes.iter().map(|call| call.operation).collect::<Vec<_>>(),
        [
            Operation::Subscribe(key),
            Operation::Cancel(key),
            Operation::Subscribe(key)
        ]
    );
    assert!(writes[1].time >= writes[0].time + cooldown);
    assert!(writes[2].time >= writes[1].time + step);
    assert_eq!(gateway.count(2, Operation::Open), 0);
    assert_eq!(handle.status().hot_lines, 1);
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn global_pacing_delays_rotation_without_closing_active_lines() {
    let config = BudgetConfig {
        rotation_dwell: Duration::from_secs(1),
        pacing_cooldown: Duration::from_secs(2),
        ..compact_config()
    };
    let cooldown = config.pacing_cooldown;
    let step = config.request_interval;
    let (gateway, handle, stop, task) = start(config, 2);
    let key = option_key(FIRST_OPTION);
    until(15, step, || {
        gateway.count(1, Operation::Subscribe(key)) == 1
    })
    .await;

    let first_notice = gateway.global_pacing();
    let prior_writes = gateway.calls().len();
    for _ in 0..4 {
        tick(step).await;
    }
    let latest_notice = gateway.global_pacing();
    for _ in 0..6 {
        tick(step).await;
    }
    assert!(Instant::now() >= first_notice + cooldown);
    assert!(Instant::now() < latest_notice + cooldown);
    assert_eq!(gateway.calls().len(), prior_writes);
    assert_eq!(handle.status().rotation_lines, 1);
    assert_eq!(handle.status().phase, SchedulerPhase::Online);

    until(8, step, || gateway.count(1, Operation::Cancel(key)) == 1).await;
    let after_notice = gateway.calls()[prior_writes];
    assert_eq!(after_notice.operation, Operation::Cancel(key));
    assert!(after_notice.time >= latest_notice + cooldown);
    assert_eq!(gateway.count(1, Operation::Close), 0);
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn global_pacing_deadline_survives_disconnect_before_driver_sees_notice() {
    let config = BudgetConfig {
        reconnect_initial: Duration::from_millis(250),
        pacing_cooldown: Duration::from_secs(2),
        ..compact_config()
    };
    let cooldown = config.pacing_cooldown;
    let step = config.request_interval;
    let (gateway, _, stop, task) = start(config, 0);
    until(15, step, || {
        gateway.count(1, Operation::Subscribe(stock_key(1))) == 1
    })
    .await;

    let paced_at = gateway.global_pacing();
    gateway.disconnect(0);
    for _ in 0..7 {
        tick(step).await;
    }
    assert_eq!(gateway.count(1, Operation::Close), 1);
    assert_eq!(gateway.count(2, Operation::Open), 0);

    until(4, step, || gateway.count(2, Operation::Open) == 1).await;
    let reopened = gateway
        .calls()
        .into_iter()
        .find(|call| call.session == 2 && call.operation == Operation::Open)
        .unwrap();
    assert!(reopened.time >= paced_at + cooldown);
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn startup_disconnect_retries_only_after_global_pacing_deadline() {
    let config = compact_config();
    let step = config.request_interval;
    let backoff = config.reconnect_initial;
    let cooldown = config.pacing_cooldown;
    let (gateway, handle, stop, task) = start(config, 1);
    {
        let mut state = gateway.state.lock().unwrap();
        state.failed_opens = 1;
        state.pace_on_failed_open = true;
    }
    until(3, step, || {
        handle.status().phase == SchedulerPhase::Reconnecting
    })
    .await;
    let first_open = gateway.calls()[0].time;
    assert_eq!(gateway.count(1, Operation::Open), 1);

    tick(backoff).await;
    assert_eq!(gateway.count(1, Operation::Open), 1);
    let just_before_deadline = first_open + cooldown - step;
    tick(just_before_deadline.duration_since(Instant::now())).await;
    assert_eq!(gateway.count(1, Operation::Open), 1);

    until(8, step, || handle.status().stock_lines == 1).await;
    let attempts: Vec<_> = gateway
        .calls()
        .into_iter()
        .filter(|call| call.operation == Operation::Open)
        .collect();
    assert_eq!(attempts.len(), 2);
    assert!(attempts[1].time >= first_open + cooldown);
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn handshake_failure_waits_for_conservative_cooldown_without_notice() {
    let handshake_time = Duration::from_secs(1);
    for (cooldown, backoff, minimum) in [(2, 1, 15), (20, 1, 20), (2, 22, 22)] {
        let config = BudgetConfig {
            pacing_cooldown: Duration::from_secs(cooldown),
            reconnect_initial: Duration::from_secs(backoff),
            reconnect_max: Duration::from_secs(backoff),
            ..compact_config()
        };
        let step = config.request_interval;
        let gateway = Gateway::new(0, &config);
        let scheduler = LineScheduler::new(config, vec![watchlist().remove(0)]).unwrap();
        let handle = scheduler.handle();
        let (stop, stopped) = watch::channel(false);
        let opener = gateway.clone();
        let mut first_attempt = true;
        let task = tokio::spawn(scheduler.run(
            move |pacing| {
                let opener = opener.clone();
                let fail = std::mem::take(&mut first_attempt);
                async move {
                    if fail {
                        opener.state.lock().unwrap().record(1, Operation::Open);
                        tokio::time::sleep(handshake_time).await;
                        return Err(MarketError::HandshakeFailed);
                    }
                    opener.open(pacing).await
                }
            },
            stopped,
        ));
        until(10, step, || {
            handle.status().phase == SchedulerPhase::Reconnecting
        })
        .await;
        let first_open = gateway.calls()[0].time;
        let deadline = first_open + handshake_time + Duration::from_secs(minimum);
        tick((deadline - step).duration_since(Instant::now())).await;
        assert_eq!(gateway.count(1, Operation::Open), 1, "{cooldown}/{backoff}");

        until(8, step, || handle.status().stock_lines == 1).await;
        let attempts: Vec<_> = gateway
            .calls()
            .into_iter()
            .filter(|call| call.operation == Operation::Open)
            .collect();
        assert_eq!(attempts.len(), 2);
        assert!(attempts[1].time >= deadline, "{cooldown}/{backoff}");
        stop.send(true).unwrap();
        task.await.unwrap().unwrap();
    }
}
