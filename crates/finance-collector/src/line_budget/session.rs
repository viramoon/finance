use std::{
    collections::{HashMap, HashSet, VecDeque},
    future::{Future, pending},
    time::Duration,
};

use async_trait::async_trait;
use finance_ib::depth::{DepthError, DepthSource};
use finance_ib::market::{
    ConfirmedContract, ContractKind, ContractQuery, MarketDataSource, MarketError, MarketRequest,
    OptionChainSource, OptionSeries, PaperMarket, Product, RequestKey, RequestParameters,
    option_market_date,
};
use finance_ib::{ConnectionManager, IbTransport};
use tokio::{
    sync::watch,
    time::{Instant, sleep_until},
};

use super::{
    LinePool, LineScheduler, SchedulerPhase, SubscriptionDemand,
    depth::drive_depth,
    policy::{DEFAULT_PACING_COOLDOWN, rank_options},
    unexpired,
    worker::{Feed, Worker},
};

#[cfg(test)]
mod tests;

#[async_trait]
trait Session: OptionChainSource + DepthSource {
    async fn disconnected(&self) -> MarketError;
    async fn close(&self);
}

#[async_trait]
impl Session for PaperMarket {
    async fn disconnected(&self) -> MarketError {
        PaperMarket::disconnected(self).await
    }

    async fn close(&self) {
        PaperMarket::close(self).await;
    }
}

async fn stop_requested(stop: &mut watch::Receiver<bool>) {
    let _ = stop.wait_for(|requested| *requested).await;
}

fn fatal(error: MarketError) -> bool {
    matches!(
        error,
        MarketError::UnsafeAccount | MarketError::InvalidConfiguration
    )
}

fn unavailable(error: MarketError) -> bool {
    matches!(
        error,
        MarketError::UnconfirmedContract
            | MarketError::ExpiredContract
            | MarketError::NotEntitled
            | MarketError::InvalidRequest
    )
}

fn incomplete_metadata(error: MarketError) -> bool {
    matches!(
        error,
        MarketError::Timeout | MarketError::IncompleteResponse
    )
}

impl LineScheduler {
    /// The caller owns the designated paper market client ID and runs one scheduler.
    /// Dropping the stop sender or setting it to true ends the session.
    pub async fn run_paper(
        self,
        manager: &ConnectionManager<IbTransport>,
        stop: watch::Receiver<bool>,
    ) -> Result<(), MarketError> {
        self.run(
            |pacing| async move { manager.open_market_with_pacing(&pacing).await },
            stop,
        )
        .await
    }

    async fn run<S, F, Open>(
        self,
        mut open: Open,
        mut stop: watch::Receiver<bool>,
    ) -> Result<(), MarketError>
    where
        S: Session,
        <S as MarketDataSource>::Subscription: 'static,
        F: Future<Output = Result<S, MarketError>>,
        Open: FnMut(watch::Sender<Option<Instant>>) -> F,
    {
        let config = &self.shared.config;
        let mut backoff = config.reconnect_initial;
        let pacing = &self.shared.pacing;
        let outcome = loop {
            let error = match tokio::select! {
                biased;
                _ = stop_requested(&mut stop) => break Ok(()),
                result = open(pacing.notices.clone()) => result,
            } {
                Ok(session) => {
                    let online_since = Instant::now();
                    let mut online = Online::new(&self);
                    self.shared.state.lock().expect("line state").phase = SchedulerPhase::Online;
                    let result = {
                        // A live SDK subscription sends a best-effort cancel on Drop.
                        // Keep both drivers (including pending IO) until close gates all writes.
                        let mut depth = std::pin::pin!(self.depth_session(&session));
                        let mut lines = std::pin::pin!(online.drive(&session));
                        let result = tokio::select! {
                            biased;
                            _ = stop_requested(&mut stop) => Ok(()),
                            error = session.disconnected() => Err(error),
                            result = &mut depth => result,
                            result = &mut lines => result,
                        };
                        if let Some((frames, _)) = &self.depth {
                            frames.clear();
                        }
                        let live = {
                            let mut state = self.shared.state.lock().expect("line state");
                            state.phase = SchedulerPhase::Reconnecting;
                            state.discovery_complete = false;
                            state.live.keys().copied().collect::<Vec<_>>()
                        };
                        for key in live {
                            self.shared.closed(key);
                        }
                        session.close().await;
                        result
                    };
                    for worker in online.active.values_mut() {
                        worker.abort().await;
                    }
                    self.shared.state.lock().expect("line state").live.clear();
                    if online_since.elapsed() >= config.reconnect_max {
                        backoff = config.reconnect_initial;
                    }
                    match result {
                        Ok(()) => break Ok(()),
                        Err(error) => error,
                    }
                }
                Err(error) => error,
            };
            if fatal(error) {
                break Err(error);
            }
            let now = Instant::now();
            if error == MarketError::Pacing {
                pacing.note_pacing();
            }
            self.shared.state.lock().expect("line state").phase = SchedulerPhase::Reconnecting;
            // The SDK can discard global notices when it fails before returning a peer.
            let retry_delay = if error == MarketError::HandshakeFailed {
                backoff
                    .max(config.pacing_cooldown)
                    .max(DEFAULT_PACING_COOLDOWN)
            } else {
                backoff
            };
            let retry_at = pacing.reconnect_at(now + retry_delay).await;
            tracing::warn!(%error, wait_ms = retry_at.duration_since(now).as_millis() as u64, "IB L1 session recovering");
            tokio::select! {
                biased;
                _ = stop_requested(&mut stop) => break Ok(()),
                _ = pacing.wait_until(retry_at) => {}
            }
            backoff = backoff.saturating_mul(2).min(config.reconnect_max);
        };
        self.shared.state.lock().expect("line state").phase = SchedulerPhase::Stopped;
        outcome
    }

    async fn depth_session(&self, session: &impl DepthSource) -> Result<(), MarketError> {
        let (Some((frames, permissions)), Some(demand)) = (&self.depth, &self.demand) else {
            return pending().await;
        };
        drive_depth(
            session,
            &self.shared.pacing,
            demand.clone(),
            *permissions,
            frames,
        )
        .await
        .map_err(|error| match error {
            DepthError::Pacing => MarketError::Pacing,
            // End the shared session before reopening an ambiguous allocation.
            _ => MarketError::Disconnected,
        })
    }
}

pub(super) fn option_key(con_id: i32) -> RequestKey {
    RequestKey {
        con_id,
        product: Product::Opra,
        parameters: RequestParameters {
            option_open_interest: true,
        },
    }
}

#[derive(Clone)]
enum Discover {
    Definitions(ConfirmedContract),
    Series(ConfirmedContract, OptionSeries),
}

enum Action {
    ConfirmStock(usize),
    Discover(Discover),
    Start(ConfirmedContract, LinePool, bool),
    Cancel(RequestKey),
}

struct PendingStock {
    query: ContractQuery,
    retry_at: Option<Instant>,
    retry_delay: Duration,
}

struct Online<'a> {
    owner: &'a LineScheduler,
    watchlist: Vec<PendingStock>,
    defer_stock_retry: bool,
    stocks: Vec<ConfirmedContract>,
    stock_confirmation_failed: bool,
    active: HashMap<RequestKey, Worker>,
    suppressed: HashSet<RequestKey>,
    swept: HashSet<i32>,
    discovery: VecDeque<Discover>,
    seen: HashSet<i32>,
    eligible: HashSet<i32>,
    discovery_failed: bool,
    refresh: Instant,
    prefer_discovery: bool,
    expiry_check: Instant,
    demand: SubscriptionDemand,
}

impl<'a> Online<'a> {
    fn new(owner: &'a LineScheduler) -> Self {
        owner
            .shared
            .state
            .lock()
            .expect("line state")
            .discovery_complete = false;
        Self {
            owner,
            watchlist: owner
                .stocks
                .iter()
                .cloned()
                .map(|query| PendingStock {
                    query,
                    retry_at: None,
                    retry_delay: owner.shared.config.reconnect_initial,
                })
                .collect(),
            defer_stock_retry: false,
            stocks: Vec::new(),
            stock_confirmation_failed: false,
            active: Default::default(),
            suppressed: HashSet::new(),
            swept: HashSet::new(),
            discovery: VecDeque::new(),
            seen: HashSet::new(),
            eligible: HashSet::new(),
            discovery_failed: false,
            refresh: Instant::now(),
            prefer_discovery: true,
            expiry_check: Instant::now(),
            demand: SubscriptionDemand::default(),
        }
    }

    fn next_stock_retry(&self) -> Option<Instant> {
        self.watchlist
            .iter()
            .filter_map(|stock| stock.retry_at)
            .min()
    }

    async fn drive<S: Session>(&mut self, session: &S) -> Result<(), MarketError>
    where
        <S as MarketDataSource>::Subscription: 'static,
    {
        let shared = &self.owner.shared;
        let config = &shared.config;
        let mut demand = self.owner.demand.clone();
        if let Some(updates) = &mut demand {
            self.demand = updates.borrow_and_update().clone();
        }
        loop {
            if self
                .active
                .values()
                .any(|worker| worker.feed() == Feed::Lost)
            {
                return Err(MarketError::Disconnected);
            }
            let slot = tokio::select! {
                biased;
                _ = shared.changed.notified() => continue,
                slot = shared.pacing.slot() => slot,
            };
            if let Some(updates) = &mut demand
                && updates.has_changed().unwrap_or(false)
            {
                self.demand = updates.borrow_and_update().clone();
            }
            if let Some(action) = self.action(Instant::now())? {
                let (result, next) = slot.run(self.execute(session, action)).await;
                match result {
                    Err(MarketError::Pacing) => shared.pacing.note_pacing(),
                    other => other?,
                }
                // Re-evaluate line priorities at the next request interval. Depth can
                // queue its next operation while this driver has no write to issue.
                tokio::select! {
                    _ = sleep_until(next) => {},
                    _ = shared.changed.notified() => {},
                }
                continue;
            }
            drop(slot);
            let next_dwell = {
                let state = shared.state.lock().expect("line state");
                self.active
                    .iter()
                    .filter_map(|(key, worker)| {
                        let line = state.live.get(key)?;
                        (line.pool == LinePool::Rotation)
                            .then_some(worker.started + config.rotation_dwell)
                    })
                    .min()
            };
            let deadline = next_dwell
                .unwrap_or(self.refresh)
                .min(self.refresh)
                .min(self.expiry_check);
            let deadline = deadline.min(self.next_stock_retry().unwrap_or(deadline));
            tokio::select! {
                _ = shared.changed.notified() => {},
                _ = demand_changed(&mut demand) => {},
                _ = sleep_until(deadline) => {},
            }
        }
    }

    fn action(&mut self, now: Instant) -> Result<Option<Action>, MarketError> {
        let shared = &self.owner.shared;
        let config = &shared.config;
        let wall = shared.now();
        let today = option_market_date(wall)?;
        let mut state = shared.state.lock().expect("line state");
        if now >= self.expiry_check {
            state
                .options
                .retain(|_, contract| unexpired(contract, today));
            self.eligible.retain(|id| state.options.contains_key(id));
            self.swept.retain(|id| state.options.contains_key(id));
            self.expiry_check = now + Duration::from_secs(60);
        }

        let hot: Vec<_> = state
            .selection
            .hot
            .iter()
            .copied()
            .map(option_key)
            .filter(|key| self.eligible.contains(&key.con_id) && !self.suppressed.contains(key))
            .collect();
        // Retire stale subscriptions before promoting or allocating new ones.
        if let Some(key) = state.live.iter().find_map(|(&key, line)| {
            let worker = &self.active[&key];
            let retire = matches!(worker.feed(), Feed::Denied | Feed::Throttled)
                || (key.product == Product::Opra && !state.options.contains_key(&key.con_id))
                || (line.pool == LinePool::Hot && !hot.contains(&key));
            retire.then_some(key)
        }) {
            return Ok(Some(Action::Cancel(key)));
        }
        for key in &hot {
            if let Some(line) = state.live.get_mut(key) {
                line.pool = LinePool::Hot;
            }
        }
        if let Some(contract) = self.stocks.iter().find(|contract| {
            let key = stock_key(contract.con_id);
            !self.active.contains_key(&key) && !self.suppressed.contains(&key)
        }) {
            return Ok(Some(Action::Start(
                contract.clone(),
                LinePool::Stock,
                false,
            )));
        }
        let ready_stock = self
            .watchlist
            .iter()
            .enumerate()
            .filter(|(_, stock)| stock.retry_at.is_none_or(|retry_at| retry_at <= now))
            .min_by_key(|(_, stock)| match (&stock.query, stock.retry_at) {
                // All first confirmations precede retries. Equal retry priority
                // keeps the queue order established on timeout even with demand.
                (_, Some(_)) => 2,
                (
                    ContractQuery::Equity {
                        symbol,
                        primary_exchange,
                    },
                    None,
                ) if self.demand.wants_equity(symbol, primary_exchange) => 0,
                _ => 1,
            })
            .map(|(index, _)| index);
        // A short retry can be due at the next paced request. After a timeout,
        // give other work a turn while keeping first confirmations a priority.
        if let Some(index) = ready_stock
            && (!self.defer_stock_retry || self.watchlist[index].retry_at.is_none())
        {
            return Ok(Some(Action::ConfirmStock(index)));
        }
        if let Some(contract) = hot.iter().find_map(|key| {
            (!self.active.contains_key(key)).then(|| state.options[&key.con_id].clone())
        }) {
            return Ok(Some(Action::Start(contract, LinePool::Hot, false)));
        }

        if self.discovery.is_empty() && now >= self.refresh {
            let mut equities = self.stocks.clone();
            equities.sort_by_key(|stock| {
                (
                    Some(stock.con_id) != state.selection.focus,
                    !self.wanted_stock(stock),
                )
            });
            self.discovery
                .extend(equities.into_iter().map(Discover::Definitions));
            self.seen.clear();
            self.discovery_failed = false;
            state.discovery_complete = self.discovery.is_empty() && self.discovery_succeeded();
            self.refresh = now + config.chain_refresh;
        }
        let discovery = self.discovery.front().cloned().map(Action::Discover);
        let rotation = state
            .live
            .iter()
            .filter(|(_, line)| line.pool == LinePool::Rotation);
        let mut count = 0;
        for (&key, _) in rotation {
            count += 1;
            let worker = &self.active[&key];
            if worker.feed() == Feed::Sampled || now >= worker.started + config.rotation_dwell {
                return Ok(Some(Action::Cancel(key)));
            }
        }
        if self.prefer_discovery && discovery.is_some() {
            return Ok(discovery);
        }
        if count < config.rotation_lines {
            let mut ranked = rank_options(
                state
                    .options
                    .values()
                    .filter(|contract| {
                        let key = option_key(contract.con_id);
                        self.eligible.contains(&key.con_id)
                            && !hot.contains(&key)
                            && !self.suppressed.contains(&key)
                    })
                    .cloned(),
                state.selection.focus,
                &state.spots,
                today,
            );
            if !self.demand.watchlist().is_empty() {
                let wanted: HashSet<_> = self
                    .stocks
                    .iter()
                    .filter(|stock| self.wanted_stock(stock))
                    .map(|stock| stock.con_id)
                    .collect();
                ranked.sort_by_key(|contract| {
                    let ContractKind::Option(option) = &contract.kind else {
                        unreachable!("ranked options")
                    };
                    (
                        Some(option.underlying_con_id) != state.selection.focus,
                        !wanted.contains(&option.underlying_con_id),
                    )
                });
            }
            if let Some((contract, new_sweep)) = self.next_option(ranked) {
                return Ok(Some(Action::Start(contract, LinePool::Rotation, new_sweep)));
            }
        }
        Ok(discovery.or(ready_stock.map(Action::ConfirmStock)))
    }

    fn next_option(&self, ranked: Vec<ConfirmedContract>) -> Option<(ConfirmedContract, bool)> {
        let available =
            |contract: &ConfirmedContract| !self.active.contains_key(&option_key(contract.con_id));
        let unseen = |contract: &ConfirmedContract| !self.swept.contains(&contract.con_id);
        if let Some(contract) = ranked
            .iter()
            .filter(|contract| unseen(contract))
            .find(|contract| available(contract))
        {
            return Some((contract.clone(), false));
        }
        // Active lines can straddle two sweeps. Give them their turn before repeating.
        if ranked.iter().any(unseen) {
            return None;
        }
        ranked
            .into_iter()
            .find(available)
            .map(|contract| (contract, true))
    }

    fn discovery_succeeded(&self) -> bool {
        !self.discovery_failed && !self.stock_confirmation_failed && self.watchlist.is_empty()
    }

    fn wanted_stock(&self, contract: &ConfirmedContract) -> bool {
        contract
            .primary_exchange
            .as_ref()
            .is_some_and(|venue| self.demand.wants_equity(&contract.symbol, venue))
    }

    async fn execute<S: Session>(&mut self, session: &S, action: Action) -> Result<(), MarketError>
    where
        <S as MarketDataSource>::Subscription: 'static,
    {
        let shared = &self.owner.shared;
        self.defer_stock_retry = false;
        match action {
            Action::ConfirmStock(index) => {
                let result = session.confirm(&self.watchlist[index].query).await;
                match result {
                    Ok(equity) if equity.product() == Product::EquityL1 && equity.con_id > 0 => {
                        if let Some(stock) = self
                            .stocks
                            .iter_mut()
                            .find(|stock| stock.con_id == equity.con_id)
                        {
                            *stock = equity;
                        } else {
                            shared
                                .state
                                .lock()
                                .expect("line state")
                                .coverage
                                .track(stock_key(equity.con_id), shared.now());
                            if self.discovery.is_empty() {
                                self.refresh = Instant::now();
                            } else {
                                self.discovery
                                    .push_back(Discover::Definitions(equity.clone()));
                            }
                            self.stocks.push(equity);
                        }
                    }
                    Ok(_) => return Err(MarketError::InvalidConfiguration),
                    Err(error) if incomplete_metadata(error) => {
                        let mut pending = self.watchlist.remove(index);
                        pending.retry_at = Some(Instant::now() + pending.retry_delay);
                        pending.retry_delay = pending
                            .retry_delay
                            .saturating_mul(2)
                            .min(shared.config.reconnect_max);
                        self.watchlist.push(pending);
                        self.defer_stock_retry = true;
                        return Ok(());
                    }
                    Err(error) if unavailable(error) => self.stock_confirmation_failed = true,
                    Err(error) => return Err(error),
                }
                self.watchlist.remove(index);
            }
            Action::Discover(discover) => {
                self.discover(session, discover).await?;
                self.prefer_discovery = false;
            }
            Action::Start(contract, pool, new_sweep) => {
                let key = if pool == LinePool::Stock {
                    stock_key(contract.con_id)
                } else {
                    option_key(contract.con_id)
                };
                let request = MarketRequest::new(contract, key.parameters)?;
                if pool == LinePool::Rotation {
                    if new_sweep {
                        self.swept.clear();
                    }
                    self.swept.insert(key.con_id);
                    self.prefer_discovery = true;
                }
                match session.subscribe(&request).await {
                    Ok(subscription) => {
                        self.active
                            .insert(key, Worker::start(shared.clone(), key, pool, subscription));
                    }
                    Err(error) if unavailable(error) => {
                        self.suppressed.insert(key);
                    }
                    Err(error) => return Err(error),
                }
            }
            Action::Cancel(key) => {
                let worker = self.active.get_mut(&key).expect("allocated line");
                if worker.feed() == Feed::Denied {
                    self.suppressed.insert(key);
                }
                // Any failed or ambiguous cancel invalidates the entire allocation.
                if let Err(error) = worker.cancel().await {
                    if error == MarketError::Pacing {
                        shared.pacing.note_pacing();
                    }
                    return Err(MarketError::Disconnected);
                }
                self.active.remove(&key);
                shared.state.lock().expect("line state").live.remove(&key);
                self.prefer_discovery = true;
            }
        }
        Ok(())
    }

    async fn discover<S: Session>(
        &mut self,
        session: &S,
        task: Discover,
    ) -> Result<(), MarketError> {
        let shared = &self.owner.shared;
        match task {
            Discover::Definitions(equity) => match session.option_series(&equity).await {
                Ok(series) => {
                    self.discovery.extend(
                        series
                            .into_iter()
                            .map(|series| Discover::Series(equity.clone(), series)),
                    );
                }
                Err(error) if unavailable(error) || incomplete_metadata(error) => {
                    self.discovery_failed = true
                }
                Err(error) => return Err(error),
            },
            Discover::Series(equity, series) => {
                match session.confirm_series(&equity, &series).await {
                    Ok(contracts) => {
                        let now = shared.now();
                        let today = option_market_date(now)?;
                        let mut state = shared.state.lock().expect("line state");
                        for contract in contracts.into_iter().filter(|contract| {
                        unexpired(contract, today) && matches!(&contract.kind, ContractKind::Option(option)
                            if option.underlying_con_id == equity.con_id && option.expiry == series.expiry)
                    }) {
                        let con_id = contract.con_id;
                        self.seen.insert(con_id);
                        self.eligible.insert(con_id);
                        state.coverage.track(option_key(con_id), now);
                        state.options.insert(con_id, contract);
                    }
                    }
                    Err(error) if unavailable(error) || incomplete_metadata(error) => {
                        self.discovery_failed = true
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        self.discovery.pop_front();
        if self.discovery.is_empty() {
            let mut state = shared.state.lock().expect("line state");
            state.discovery_complete = self.discovery_succeeded();
            if state.discovery_complete {
                state.options.retain(|id, _| self.seen.contains(id));
                self.eligible.retain(|id| self.seen.contains(id));
                self.swept.retain(|id| self.seen.contains(id));
            }
            self.refresh = Instant::now() + shared.config.chain_refresh;
        }
        Ok(())
    }
}

async fn demand_changed(receiver: &mut Option<watch::Receiver<SubscriptionDemand>>) {
    if let Some(updates) = receiver {
        if updates.changed().await.is_ok() {
            // `changed` marks the update seen. Leave it pending for the drive loop.
            updates.mark_changed();
            return;
        }
        *receiver = None;
    }
    pending::<()>().await;
}

fn stock_key(con_id: i32) -> RequestKey {
    RequestKey {
        con_id,
        product: Product::EquityL1,
        parameters: RequestParameters::default(),
    }
}
