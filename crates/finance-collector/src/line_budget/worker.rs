use std::sync::Arc;

use finance_ib::market::{
    MarketError, MarketEvent, MarketNotice, MarketObservation, MarketSubscription, Product,
    QuoteField, RequestKey,
};
use tokio::{
    sync::{oneshot, watch},
    task::JoinHandle,
    time::Instant,
};

use super::{LineMessage, LinePool, LineUpdate, LiveLine, Shared};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Feed {
    Reading,
    Sampled,
    Denied,
    Throttled,
    Lost,
}

pub(super) struct Worker {
    pub(super) started: Instant,
    feed: watch::Receiver<Feed>,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<(), MarketError>>>,
}

impl Worker {
    pub(super) fn start<S: MarketSubscription + 'static>(
        shared: Arc<Shared>,
        key: RequestKey,
        pool: LinePool,
        subscription: S,
    ) -> Self {
        let (stop, cancel) = oneshot::channel();
        let (signal, feed) = watch::channel(Feed::Reading);
        shared.opened(key, pool);
        Self {
            started: Instant::now(),
            feed,
            stop: Some(stop),
            task: Some(tokio::spawn(read_line(
                shared,
                key,
                subscription,
                signal,
                cancel,
            ))),
        }
    }

    pub(super) fn feed(&self) -> Feed {
        if self.task.as_ref().is_none_or(JoinHandle::is_finished) {
            Feed::Lost
        } else {
            *self.feed.borrow()
        }
    }

    pub(super) async fn cancel(&mut self) -> Result<(), MarketError> {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        let result = self.task.as_mut().expect("allocated reader").await;
        self.task = None;
        result.map_err(|_| MarketError::Transport)?
    }

    pub(super) async fn abort(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

async fn read_line<S: MarketSubscription>(
    shared: Arc<Shared>,
    key: RequestKey,
    mut subscription: S,
    signal: watch::Sender<Feed>,
    mut cancel: oneshot::Receiver<()>,
) -> Result<(), MarketError> {
    let mut observation = MarketObservation::default();
    loop {
        let event = tokio::select! {
            biased;
            _ = &mut cancel => {
                shared.closed(key);
                return subscription.cancel().await;
            }
            event = subscription.next() => event,
        };
        let feed = match event {
            Ok(Some(event)) => {
                observation.apply(event.clone());
                shared.event(key, &observation, event)
            }
            Err(MarketError::Pacing) => {
                shared.pacing.note_pacing();
                Feed::Throttled
            }
            Err(MarketError::NotEntitled) => Feed::Denied,
            _ => Feed::Lost,
        };
        if feed != Feed::Reading
            && signal.send_if_modified(|previous| {
                let changed = *previous != feed;
                *previous = feed;
                changed
            })
        {
            shared.changed.notify_one();
        }
        if matches!(feed, Feed::Denied | Feed::Throttled | Feed::Lost) {
            shared.closed(key);
            cancel.await.map_err(|_| MarketError::Disconnected)?;
            return subscription.cancel().await;
        }
    }
}

impl Shared {
    fn publish(&self, key: RequestKey, generation: u64, message: LineMessage) {
        let _ = self.updates.send(LineUpdate {
            key,
            generation,
            message,
        });
    }

    fn opened(&self, key: RequestKey, pool: LinePool) {
        let mut state = self.state.lock().expect("line state");
        state.generation += 1;
        let generation = state.generation;
        state.live.insert(
            key,
            LiveLine {
                pool,
                generation,
                receiving: true,
            },
        );
        state.coverage.opened(key, self.now());
        self.publish(key, generation, LineMessage::Opened);
    }

    pub(super) fn closed(&self, key: RequestKey) {
        let mut state = self.state.lock().expect("line state");
        let Some(line) = state.live.get_mut(&key).filter(|line| line.receiving) else {
            return;
        };
        line.receiving = false;
        let generation = line.generation;
        state.coverage.closed(key, self.now());
        if key.product == Product::EquityL1 {
            state.spots.remove(&key.con_id);
        }
        self.publish(key, generation, LineMessage::Closed);
    }

    fn event(&self, key: RequestKey, observation: &MarketObservation, event: MarketEvent) -> Feed {
        let mut state = self.state.lock().expect("line state");
        let Some(line) = state.live.get(&key).filter(|line| line.receiving) else {
            return Feed::Lost;
        };
        let generation = line.generation;
        let evidence = observation.evidence();
        let observed = match &event {
            MarketEvent::Quote {
                field,
                delayed,
                value,
                ..
            } => {
                let current = evidence
                    .market_data_type
                    .is_some_and(|kind| kind.is_delayed() == *delayed);
                let valid = value.as_ref().filter(|price| price.value() > 0.into());
                if key.product == Product::EquityL1
                    && *field == QuoteField::LastPrice
                    && let Some(price) = valid.filter(|_| current)
                {
                    state.spots.insert(key.con_id, price.clone());
                }
                current
                    && valid.is_some()
                    && *field != QuoteField::CumulativeVolume
                    && (evidence.bid_ask_valid || *field == QuoteField::LastPrice)
            }
            MarketEvent::OpenInterest { value, .. } => value.is_some(),
            MarketEvent::Greeks(fields) => fields.gamma.is_some(),
            _ => false,
        };
        if observed {
            state.coverage.observed(key, self.now());
        }
        let feed = match event {
            MarketEvent::Notice(MarketNotice::NotEntitled | MarketNotice::CompetingSession) => {
                Feed::Denied
            }
            MarketEvent::Notice(MarketNotice::Pacing) => {
                self.pacing.note_pacing();
                Feed::Throttled
            }
            _ if evidence.open_interest_received && evidence.greeks_received => Feed::Sampled,
            _ => Feed::Reading,
        };
        self.publish(key, generation, LineMessage::Event(event));
        feed
    }
}
