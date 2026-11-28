use std::{future::Future, time::Duration};

use tokio::{
    sync::{Mutex, MutexGuard, watch},
    time::{Instant, sleep_until},
};

use super::BudgetConfig;

/// One writer and one pacing deadline for the owner's paper connection, across reconnects.
pub(super) struct RequestPacing {
    next: Mutex<Instant>,
    pub notices: watch::Sender<Option<Instant>>,
    interval: Duration,
    cooldown: Duration,
}

impl RequestPacing {
    pub fn new(config: &BudgetConfig) -> Self {
        Self {
            next: Mutex::new(Instant::now()),
            notices: watch::Sender::new(None),
            interval: config.request_interval,
            cooldown: config.pacing_cooldown,
        }
    }

    pub fn note_pacing(&self) {
        // Timestamp under the watch lock shared with the Gateway notice reader.
        self.notices
            .send_modify(|notice| *notice = Some(Instant::now()));
    }

    fn deadline(&self, earliest: Instant) -> Instant {
        self.notices
            .borrow()
            .map_or(earliest, |notice| earliest.max(notice + self.cooldown))
    }

    pub async fn reconnect_at(&self, earliest: Instant) -> Instant {
        self.deadline(earliest.max(*self.next.lock().await))
    }

    pub async fn wait_until(&self, earliest: Instant) {
        loop {
            let deadline = self.deadline(earliest);
            if deadline <= Instant::now() {
                return;
            }
            // A notice during the wait can only extend the deadline.
            sleep_until(deadline).await;
        }
    }

    pub async fn slot(&self) -> RequestSlot<'_> {
        let next = self.next.lock().await;
        self.wait_until(*next).await;
        RequestSlot { next, pacing: self }
    }
}

pub(super) struct RequestSlot<'a> {
    next: MutexGuard<'a, Instant>,
    pacing: &'a RequestPacing,
}

impl RequestSlot<'_> {
    /// Reserve the slot even if the session ends during IO. On completion start
    /// the interval from the response, as for the original L1 scheduler.
    pub async fn run<F: Future>(mut self, request: F) -> (F::Output, Instant) {
        // Ranking a large chain can take time after acquiring a slot.
        // Honor notices received during selection before polling the write.
        self.pacing.wait_until(*self.next).await;
        *self.next = Instant::now() + self.pacing.interval;
        let result = request.await;
        *self.next = Instant::now() + self.pacing.interval;
        (result, *self.next)
    }
}
