use std::{
    collections::{HashMap, HashSet, VecDeque},
    future::Future,
    time::Duration,
};

use async_trait::async_trait;
use chrono::NaiveDate;
use finance_ib::market::{
    ConfirmedContract, ContractKind, ContractQuery, MarketError, MarketRequest, OptionChainSource,
    OptionSeries, PaperMarket, Product, RequestKey, RequestParameters, option_market_date,
};
use finance_ib::{ConnectionManager, IbTransport};
use tokio::{
    sync::watch,
    time::{Instant, sleep_until},
};

use super::{
    LinePool, LineScheduler, SchedulerPhase,
    policy::rank_options,
    worker::{Feed, Worker},
};

#[cfg(test)]
mod tests;

#[async_trait]
trait Session: OptionChainSource {
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

impl LineScheduler {
    /// The caller owns the designated paper market client ID and runs one scheduler.
    /// Dropping the stop sender or setting it to true ends the session.
    pub async fn run_paper(
        self,
        manager: &ConnectionManager<IbTransport>,
        stop: watch::Receiver<bool>,
    ) -> Result<(), MarketError> {
        self.run(|| manager.open_market(), stop).await
    }

    async fn run<S, F, Open>(
        self,
        mut open: Open,
        mut stop: watch::Receiver<bool>,
    ) -> Result<(), MarketError>
    where
        S: Session,
        S::Subscription: 'static,
        F: Future<Output = Result<S, MarketError>>,
        Open: FnMut() -> F,
    {
        let config = &self.shared.config;
        let mut backoff = config.reconnect_initial;
        let mut next_request = Instant::now();
        let outcome = loop {
            let error = match tokio::select! {
                biased;
                _ = stop_requested(&mut stop) => break Ok(()),
                result = open() => result,
            } {
                Ok(session) => {
                    let online_since = Instant::now();
                    let mut online = Online::new(&self);
                    self.shared.state.lock().expect("line state").phase = SchedulerPhase::Online;
                    let result = tokio::select! {
                        biased;
                        _ = stop_requested(&mut stop) => Ok(()),
                        error = session.disconnected() => Err(error),
                        result = online.drive(&session, &mut next_request) => result,
                    };
                    {
                        let mut state = self.shared.state.lock().expect("line state");
                        state.phase = SchedulerPhase::Reconnecting;
                        state.discovery_complete = false;
                    }
                    for &key in online.active.keys() {
                        self.shared.closed(key);
                    }
                    session.close().await;
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
                next_request = next_request.max(now + config.pacing_cooldown);
            }
            {
                let mut state = self.shared.state.lock().expect("line state");
                state.phase = SchedulerPhase::Reconnecting;
                next_request = next_request.max(state.cooldown.unwrap_or(now));
            }
            let retry_at = (now + backoff).max(next_request);
            tracing::warn!(%error, wait_ms = retry_at.duration_since(now).as_millis() as u64, "IB L1 session recovering");
            tokio::select! {
                biased;
                _ = stop_requested(&mut stop) => break Ok(()),
                _ = sleep_until(retry_at) => {}
            }
            backoff = backoff.saturating_mul(2).min(config.reconnect_max);
        };
        self.shared.state.lock().expect("line state").phase = SchedulerPhase::Stopped;
        outcome
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
    ConfirmStock(ContractQuery),
    Discover(Discover),
    Start(ConfirmedContract, LinePool, bool),
    Cancel(RequestKey),
}

struct Online<'a> {
    owner: &'a LineScheduler,
    watchlist: VecDeque<ContractQuery>,
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
            watchlist: owner.stocks.iter().cloned().collect(),
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
        }
    }

    async fn drive<S: Session>(
        &mut self,
        session: &S,
        next_request: &mut Instant,
    ) -> Result<(), MarketError>
    where
        S::Subscription: 'static,
    {
        let shared = &self.owner.shared;
        let config = &shared.config;
        loop {
            let now = Instant::now();
            if self
                .active
                .values()
                .any(|worker| worker.feed() == Feed::Lost)
            {
                return Err(MarketError::Disconnected);
            }
            if let Some(cooldown) = shared.state.lock().expect("line state").cooldown {
                *next_request = (*next_request).max(cooldown);
            }
            let action = self.action(now)?;
            if let Some(action) = action {
                if now >= *next_request {
                    *next_request = now + config.request_interval;
                    let result = self.execute(session, action).await;
                    *next_request = Instant::now() + config.request_interval;
                    match result {
                        Err(MarketError::Pacing) => {
                            *next_request =
                                (*next_request).max(Instant::now() + config.pacing_cooldown);
                        }
                        other => other?,
                    }
                    continue;
                }
                tokio::select! {
                    _ = shared.changed.notified() => {},
                    _ = sleep_until(*next_request) => {},
                }
            } else {
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
                tokio::select! {
                    _ = shared.changed.notified() => {},
                    _ = sleep_until(deadline) => {},
                }
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
        if let Some(query) = self.watchlist.front() {
            return Ok(Some(Action::ConfirmStock(query.clone())));
        }
        if let Some(contract) = hot.iter().find_map(|key| {
            (!self.active.contains_key(key)).then(|| state.options[&key.con_id].clone())
        }) {
            return Ok(Some(Action::Start(contract, LinePool::Hot, false)));
        }

        if self.discovery.is_empty() && now >= self.refresh {
            let mut equities = self.stocks.clone();
            equities.sort_by_key(|stock| Some(stock.con_id) != state.selection.focus);
            self.discovery
                .extend(equities.into_iter().map(Discover::Definitions));
            self.seen.clear();
            self.discovery_failed = self.stock_confirmation_failed;
            state.discovery_complete = self.discovery.is_empty() && !self.discovery_failed;
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
            let ranked = rank_options(
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
            if let Some((contract, new_sweep)) = self.next_option(ranked) {
                return Ok(Some(Action::Start(contract, LinePool::Rotation, new_sweep)));
            }
        }
        Ok(discovery)
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

    async fn execute<S: Session>(&mut self, session: &S, action: Action) -> Result<(), MarketError>
    where
        S::Subscription: 'static,
    {
        let shared = &self.owner.shared;
        match action {
            Action::ConfirmStock(query) => {
                let result = session.confirm(&query).await;
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
                            self.stocks.push(equity);
                        }
                    }
                    Ok(_) => return Err(MarketError::InvalidConfiguration),
                    Err(error) if unavailable(error) || error == MarketError::Timeout => {
                        self.stock_confirmation_failed = true
                    }
                    Err(error) => return Err(error),
                }
                self.watchlist.pop_front();
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
                let result = worker.cancel().await;
                if result.is_err() {
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
                Err(error) if unavailable(error) || error == MarketError::Timeout => {
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
                    Err(error) if unavailable(error) || error == MarketError::Timeout => {
                        self.discovery_failed = true
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        self.discovery.pop_front();
        if self.discovery.is_empty() {
            let mut state = shared.state.lock().expect("line state");
            state.discovery_complete = !self.discovery_failed;
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

fn stock_key(con_id: i32) -> RequestKey {
    RequestKey {
        con_id,
        product: Product::EquityL1,
        parameters: RequestParameters::default(),
    }
}

fn unexpired(contract: &ConfirmedContract, today: NaiveDate) -> bool {
    contract.con_id > 0
        && matches!(&contract.kind, ContractKind::Option(option) if option.expiry >= today)
}
