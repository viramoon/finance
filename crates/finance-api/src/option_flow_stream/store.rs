use crate::market_options::{
    OptionFlowWindow,
    flow::{is_display_access, validate_coverage_key},
};
use chrono::{DateTime, Utc};
use finance_collector::{
    market_ingress::ProductPolicy,
    option_flow::{OPRA, ObservedFlow},
};
use std::{
    collections::VecDeque,
    error::Error,
    fmt,
    sync::{Arc, Mutex},
};
use tokio::sync::watch;

/// Bounded, process-local replay for a shared owner's authorized OPRA observations.
/// The owner publishes observations it already obtained through its read-only path.
pub struct OptionFlowStream {
    access: ProductPolicy,
    shared: Arc<Shared>,
    updated: watch::Sender<()>,
}

const RETAINED_OBSERVATIONS: usize = 100;

#[derive(Debug)]
pub enum PublishError {
    InvalidPolicy,
    InvalidObservation,
    EpochUnavailable(getrandom::Error),
    SequenceExhausted,
}

impl fmt::Display for PublishError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InvalidPolicy => "Option flow requires a valid OPRA display policy",
            Self::InvalidObservation => "Option flow observation does not match the stream",
            Self::EpochUnavailable(_) => "Could not allocate an option flow stream epoch",
            Self::SequenceExhausted => "Option flow stream sequence exhausted",
        };
        formatter.write_str(message)
    }
}

impl Error for PublishError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::EpochUnavailable(error) => Some(error),
            _ => None,
        }
    }
}

struct Shared {
    epoch: u128,
    capacity: usize,
    history: Mutex<History>,
}

impl Shared {
    fn cursor(&self, con_id: i32, seq: u64) -> String {
        format!("{:032x}.{con_id}.{seq}", self.epoch)
    }

    fn decode_cursor(&self, con_id: i32, raw: &str) -> Result<u64, CursorUnavailable> {
        let (_, seq) = raw.rsplit_once('.').ok_or(CursorUnavailable)?;
        let seq = seq.parse().map_err(|_| CursorUnavailable)?;
        (raw == self.cursor(con_id, seq))
            .then_some(seq)
            .ok_or(CursorUnavailable)
    }
}

struct History {
    seq: u64,
    entries: VecDeque<(u64, Arc<ObservedFlow>)>,
}

impl History {
    fn check_position(&self, seq: u64) -> Result<(), CursorUnavailable> {
        let first_position = self.entries.front().map_or(self.seq, |(seq, _)| seq - 1);
        if (first_position..=self.seq).contains(&seq) {
            Ok(())
        } else {
            Err(CursorUnavailable)
        }
    }
}

#[derive(Debug)]
pub(crate) struct CursorUnavailable;

pub(crate) struct FlowStreamSnapshot {
    pub as_of: DateTime<Utc>,
    pub cursor: String,
    pub records: Vec<Arc<ObservedFlow>>,
}

pub(crate) struct SequencedObservation {
    pub seq: u64,
    pub cursor: String,
    pub record: Arc<ObservedFlow>,
}

pub(crate) struct Subscription {
    shared: Arc<Shared>,
    updated: watch::Receiver<()>,
    con_id: i32,
    position: u64,
}

impl OptionFlowStream {
    pub fn new(access: ProductPolicy) -> Result<Self, PublishError> {
        if !is_display_access(&access, OPRA) {
            return Err(PublishError::InvalidPolicy);
        }
        let mut epoch = [0; 16];
        getrandom::fill(&mut epoch).map_err(PublishError::EpochUnavailable)?;
        Ok(Self::with_epoch(
            access,
            u128::from_be_bytes(epoch),
            RETAINED_OBSERVATIONS,
            0,
        ))
    }

    fn with_epoch(access: ProductPolicy, epoch: u128, capacity: usize, seq: u64) -> Self {
        Self {
            access,
            shared: Arc::new(Shared {
                epoch,
                capacity,
                history: Mutex::new(History {
                    seq,
                    entries: VecDeque::with_capacity(capacity),
                }),
            }),
            updated: watch::channel(()).0,
        }
    }

    pub(crate) fn access(&self) -> &ProductPolicy {
        &self.access
    }

    /// Accept one observation in publication order, including distinct entries
    /// that share receipt times or have identical evidence.
    pub fn publish(&self, event: ObservedFlow) -> Result<(), PublishError> {
        if event.access != self.access || event.clue.con_id <= 0 {
            return Err(PublishError::InvalidObservation);
        }
        validate_coverage_key(event.coverage.as_ref(), event.clue.con_id)
            .map_err(|_| PublishError::InvalidObservation)?;

        let mut history = self
            .shared
            .history
            .lock()
            .expect("option flow history lock");
        let seq = history
            .seq
            .checked_add(1)
            .ok_or(PublishError::SequenceExhausted)?;
        if history.entries.len() == self.shared.capacity {
            history.entries.pop_front();
        }
        history.entries.push_back((seq, Arc::new(event)));
        history.seq = seq;
        self.updated.send_replace(());
        Ok(())
    }

    pub(crate) fn snapshot(&self, window: OptionFlowWindow) -> FlowStreamSnapshot {
        let history = self
            .shared
            .history
            .lock()
            .expect("option flow history lock");
        let mut as_of = Utc::now();
        let records = history
            .entries
            .iter()
            .filter_map(|(_, record)| {
                let clue = &record.clue;
                if clue.con_id == window.con_id
                    && (window.start..window.end).contains(&clue.observed_at)
                {
                    as_of = as_of.max(clue.observed_at);
                    Some(Arc::clone(record))
                } else {
                    None
                }
            })
            .collect();
        FlowStreamSnapshot {
            as_of,
            cursor: self.shared.cursor(window.con_id, history.seq),
            records,
        }
    }

    pub(crate) fn subscribe(
        &self,
        con_id: i32,
        cursor: Option<&str>,
    ) -> Result<Subscription, CursorUnavailable> {
        let updated = self.updated.subscribe();
        let history = self
            .shared
            .history
            .lock()
            .expect("option flow history lock");
        let position = match cursor {
            Some(cursor) => self.shared.decode_cursor(con_id, cursor)?,
            None => history.seq,
        };
        history.check_position(position)?;
        Ok(Subscription {
            shared: Arc::clone(&self.shared),
            updated,
            con_id,
            position,
        })
    }
}

impl Subscription {
    pub(crate) fn cursor(&self) -> String {
        self.shared.cursor(self.con_id, self.position)
    }

    pub(crate) async fn next(&mut self) -> Result<Option<SequencedObservation>, CursorUnavailable> {
        loop {
            {
                let history = self
                    .shared
                    .history
                    .lock()
                    .expect("option flow history lock");
                history.check_position(self.position)?;
                if let Some((seq, record)) = history
                    .entries
                    .iter()
                    .find(|(seq, record)| *seq > self.position && record.clue.con_id == self.con_id)
                {
                    self.position = *seq;
                    return Ok(Some(SequencedObservation {
                        seq: *seq,
                        cursor: self.cursor(),
                        record: Arc::clone(record),
                    }));
                }
                // The watch receiver remembers changes between this scan and `changed`.
                self.position = history.seq;
            }
            if self.updated.changed().await.is_err() {
                return Ok(None);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::option_flow_stream::wire::synthetic_observation;
    use chrono::TimeDelta;
    use std::time::Duration;
    use tokio::time::timeout;

    fn test_stream(capacity: usize, seq: u64) -> OptionFlowStream {
        OptionFlowStream::with_epoch(synthetic_observation().access, 3, capacity, seq)
    }

    async fn next_observation(subscription: &mut Subscription) -> SequencedObservation {
        timeout(Duration::from_secs(2), subscription.next())
            .await
            .expect("timed out waiting for an observation")
            .unwrap()
            .unwrap()
    }

    #[tokio::test]
    async fn resumes_after_last_delivered_observation_and_wakes_on_publish() {
        let record = synthetic_observation();
        let con_id = record.clue.con_id;
        let stream = test_stream(4, u64::MAX - 2);
        let mut fresh = stream.subscribe(con_id, None).unwrap();
        let (first, ()) = tokio::join!(next_observation(&mut fresh), async {
            tokio::task::yield_now().await;
            stream.publish(record.clone()).unwrap();
        });
        assert_eq!(first.seq, u64::MAX - 1);

        stream.publish(record.clone()).unwrap();
        let mut resumed = stream.subscribe(con_id, Some(&first.cursor)).unwrap();
        let second = next_observation(&mut resumed).await;
        assert_eq!(second.seq, u64::MAX);
        assert_ne!(second.cursor, first.cursor);
        assert_eq!(second.record.clue, first.record.clue);
        assert!(matches!(
            stream.publish(record),
            Err(PublishError::SequenceExhausted)
        ));
    }

    #[tokio::test]
    async fn expired_scoped_and_future_cursors_require_reset() {
        let mut record = synthetic_observation();
        let con_id = record.clue.con_id;
        let stream = test_stream(2, 0);
        let mut slow = stream.subscribe(con_id, None).unwrap();
        let start = slow.cursor();
        stream.publish(record.clone()).unwrap();
        let floor_cursor = stream.subscribe(con_id, None).unwrap().cursor();

        record.clue.con_id += 1;
        record.coverage = None;
        stream.publish(record).unwrap();
        stream.publish(synthetic_observation()).unwrap();

        assert!(matches!(
            timeout(Duration::from_secs(2), slow.next()).await.unwrap(),
            Err(CursorUnavailable)
        ));
        assert!(matches!(
            stream.subscribe(con_id, Some(&start)),
            Err(CursorUnavailable)
        ));
        assert!(matches!(
            stream.subscribe(con_id + 1, Some(&floor_cursor)),
            Err(CursorUnavailable)
        ));
        assert!(matches!(
            stream.subscribe(con_id, Some(&stream.shared.cursor(con_id, 4))),
            Err(CursorUnavailable)
        ));
        let replacement = OptionFlowStream::new(synthetic_observation().access).unwrap();
        assert!(matches!(
            replacement.subscribe(con_id, Some(&start)),
            Err(CursorUnavailable)
        ));
        let mut resumed = stream.subscribe(con_id, Some(&floor_cursor)).unwrap();
        assert_eq!(next_observation(&mut resumed).await.seq, 3);
    }

    #[tokio::test]
    async fn snapshot_and_cursor_capture_the_same_publication_boundary() {
        let record = synthetic_observation();
        let con_id = record.clue.con_id;
        let end = record.clue.observed_at + TimeDelta::seconds(1);
        let window = OptionFlowWindow {
            con_id,
            start: end - TimeDelta::seconds(3),
            end,
        };
        let stream = test_stream(4, 0);
        let mut outside_window = record.clone();
        outside_window.clue.observed_at = window.end;
        stream.publish(outside_window).unwrap();
        stream.publish(record.clone()).unwrap();
        let mut delayed = record.clone();
        delayed.clue.observed_at = window.start;
        stream.publish(delayed.clone()).unwrap();

        let snapshot = stream.snapshot(window);
        assert_eq!(snapshot.records.len(), 2);
        assert_eq!(snapshot.records[0].clue, record.clue);
        assert_eq!(snapshot.records[1].clue, delayed.clue);
        assert!(snapshot.as_of >= record.clue.observed_at);
        stream.publish(record.clone()).unwrap();
        let mut resumed = stream.subscribe(con_id, Some(&snapshot.cursor)).unwrap();
        let after_snapshot = next_observation(&mut resumed).await;
        assert_eq!(after_snapshot.seq, 4);
        assert_eq!(after_snapshot.record.clue, record.clue);
    }

    #[test]
    fn rejects_unlicensed_sources_and_mismatched_observation_access_or_coverage() {
        let mut record = synthetic_observation();
        let mut invalid_access = record.access.clone();
        invalid_access.display_live = false;
        assert!(matches!(
            OptionFlowStream::new(invalid_access),
            Err(PublishError::InvalidPolicy)
        ));
        let stream = test_stream(4, 0);
        let mut wrong_access = record.clone();
        wrong_access.access.license = "different-synthetic-license".parse().unwrap();
        assert!(matches!(
            stream.publish(wrong_access),
            Err(PublishError::InvalidObservation)
        ));
        record.coverage.as_mut().unwrap().key.con_id += 1;
        assert!(matches!(
            stream.publish(record),
            Err(PublishError::InvalidObservation)
        ));
    }
}
