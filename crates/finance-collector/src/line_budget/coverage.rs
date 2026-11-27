use std::{
    collections::{HashMap, VecDeque},
    time::Duration,
};

use chrono::{DateTime, TimeDelta, Utc};
use finance_ib::market::RequestKey;

const MAX_TRACKED_KEYS: usize = 16_384;
const MAX_CLOSED_WINDOWS: usize = 128;

/// A half-open UTC interval `[start, end)`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CoverageInterval {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CoverageReport {
    pub key: RequestKey,
    pub last_observed_at: Option<DateTime<Utc>>,
    /// A subscription had already supplied qualifying data during these intervals.
    /// Coverage describes local observation and does not imply vendor tick continuity.
    pub covered: Vec<CoverageInterval>,
    /// Intervals with a tracked key but without observable coverage.
    pub uncovered: Vec<CoverageInterval>,
    /// Intervals before tracking or outside retained history.
    pub unknown: Vec<CoverageInterval>,
}

#[derive(Default)]
enum Active {
    #[default]
    Inactive,
    AwaitingObservation,
    Observed(DateTime<Utc>),
}

struct History {
    known_since: DateTime<Utc>,
    last_activity: DateTime<Utc>,
    last_observed_at: Option<DateTime<Utc>>,
    active: Active,
    closed: VecDeque<CoverageInterval>,
}

pub(super) struct Coverage {
    retention: TimeDelta,
    histories: HashMap<RequestKey, History>,
}

impl Coverage {
    pub(super) fn new(retention: Duration) -> Self {
        Self {
            retention: TimeDelta::from_std(retention).unwrap_or(TimeDelta::MAX),
            histories: HashMap::new(),
        }
    }

    pub(super) fn track(&mut self, key: RequestKey, at: DateTime<Utc>) {
        if self.histories.contains_key(&key) {
            return;
        }
        if self.histories.len() == MAX_TRACKED_KEYS {
            let evicted = self
                .histories
                .iter()
                .filter(|(_, history)| matches!(history.active, Active::Inactive))
                .min_by_key(|(_, history)| history.last_activity)
                .map(|(&key, _)| key);
            let Some(evicted) = evicted else {
                return;
            };
            self.histories.remove(&evicted);
        }
        self.histories.insert(
            key,
            History {
                known_since: at,
                last_activity: at,
                last_observed_at: None,
                active: Active::Inactive,
                closed: VecDeque::new(),
            },
        );
    }

    pub(super) fn opened(&mut self, key: RequestKey, at: DateTime<Utc>) {
        self.track(key, at);
        if let Some(history) = self.histories.get_mut(&key)
            && matches!(history.active, Active::Inactive)
        {
            history.last_activity = history.last_activity.max(at);
            history.active = Active::AwaitingObservation;
        }
    }

    pub(super) fn observed(&mut self, key: RequestKey, at: DateTime<Utc>) {
        let Some(history) = self.histories.get_mut(&key) else {
            return;
        };
        let at = history.last_activity.max(at);
        match history.active {
            Active::Inactive => return,
            Active::AwaitingObservation => history.active = Active::Observed(at),
            Active::Observed(_) => {}
        }
        history.last_activity = at;
        history.last_observed_at = Some(at);
    }

    pub(super) fn closed(&mut self, key: RequestKey, at: DateTime<Utc>) {
        let cutoff = self.cutoff(at);
        let Some(history) = self.histories.get_mut(&key) else {
            return;
        };
        let active = std::mem::take(&mut history.active);
        if matches!(active, Active::Inactive) {
            return;
        }
        let end = history.last_activity.max(at);
        history.last_activity = end;
        while let Some(old) = history.closed.front().filter(|old| old.end <= cutoff) {
            history.known_since = history.known_since.max(old.end);
            history.closed.pop_front();
        }
        if let Active::Observed(start) = active
            && start < end
        {
            if let Some(previous) = history.closed.back_mut()
                && previous.end == start
            {
                previous.end = end;
            } else {
                history.closed.push_back(CoverageInterval { start, end });
            }
            if history.closed.len() > MAX_CLOSED_WINDOWS
                && let Some(evicted) = history.closed.pop_front()
            {
                history.known_since = evicted.end;
            }
        }
    }

    pub(super) fn last_observed_at(&self, key: RequestKey) -> Option<DateTime<Utc>> {
        self.histories.get(&key)?.last_observed_at
    }

    pub(super) fn query(
        &self,
        key: RequestKey,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        as_of: DateTime<Utc>,
    ) -> Option<CoverageReport> {
        if start >= end || end > as_of {
            return None;
        }

        let mut report = CoverageReport {
            key,
            last_observed_at: self.last_observed_at(key).filter(|at| *at <= as_of),
            covered: Vec::new(),
            uncovered: Vec::new(),
            unknown: Vec::new(),
        };
        let Some(history) = self.histories.get(&key) else {
            report.unknown.push(CoverageInterval { start, end });
            return Some(report);
        };

        let mut cursor = start
            .max(history.known_since)
            .max(self.cutoff(as_of))
            .min(end);
        if start < cursor {
            report.unknown.push(CoverageInterval { start, end: cursor });
        }
        let active = match history.active {
            Active::Observed(start) => Some(CoverageInterval { start, end: as_of }),
            Active::Inactive | Active::AwaitingObservation => None,
        };
        for interval in history.closed.iter().copied().chain(active) {
            let begin = interval.start.max(cursor);
            let through = interval.end.min(end);
            if begin >= through {
                continue;
            }
            if cursor < begin {
                report.uncovered.push(CoverageInterval {
                    start: cursor,
                    end: begin,
                });
            }
            if let Some(previous) = report.covered.last_mut()
                && previous.end == begin
            {
                previous.end = through;
            } else {
                report.covered.push(CoverageInterval {
                    start: begin,
                    end: through,
                });
            }
            cursor = through;
        }
        if cursor < end {
            report
                .uncovered
                .push(CoverageInterval { start: cursor, end });
        }
        Some(report)
    }

    fn cutoff(&self, as_of: DateTime<Utc>) -> DateTime<Utc> {
        as_of
            .checked_sub_signed(self.retention)
            .unwrap_or(DateTime::<Utc>::MIN_UTC)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use finance_ib::market::{Product, RequestParameters};

    fn key(con_id: i32) -> RequestKey {
        RequestKey {
            con_id,
            product: Product::Opra,
            parameters: RequestParameters::default(),
        }
    }

    fn at(second: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(second, 0).unwrap()
    }

    fn interval(start: i64, end: i64) -> CoverageInterval {
        CoverageInterval {
            start: at(start),
            end: at(end),
        }
    }

    #[test]
    fn queries_rotation_outage_and_unobserved_subscription_intervals() {
        let mut coverage = Coverage::new(Duration::from_secs(60));
        let key = key(1);
        coverage.track(key, at(2));
        coverage.opened(key, at(4));
        coverage.closed(key, at(6));
        coverage.opened(key, at(8));
        coverage.observed(key, at(9));
        coverage.observed(key, at(11));
        coverage.closed(key, at(12));
        coverage.observed(key, at(14));
        coverage.opened(key, at(17));
        coverage.observed(key, at(19));

        let report = coverage.query(key, at(0), at(22), at(22)).unwrap();
        assert_eq!(report.unknown, [interval(0, 2)]);
        assert_eq!(report.uncovered, [interval(2, 9), interval(12, 19)]);
        assert_eq!(report.covered, [interval(9, 12), interval(19, 22)]);
        assert_eq!(report.last_observed_at, Some(at(19)));

        let report = coverage.query(key, at(10), at(18), at(22)).unwrap();
        assert!(report.unknown.is_empty());
        assert_eq!(report.covered, [interval(10, 12)]);
        assert_eq!(report.uncovered, [interval(12, 18)]);
    }

    #[test]
    fn distinguishes_never_scheduled_candidates_from_unknown_history() {
        let mut coverage = Coverage::new(Duration::from_secs(10));
        let known = key(1);
        let unknown = key(2);
        coverage.track(known, at(1));
        coverage.track(known, at(12));
        let report = coverage.query(known, at(0), at(15), at(15)).unwrap();
        assert_eq!(report.unknown, [interval(0, 5)]);
        assert_eq!(report.uncovered, [interval(5, 15)]);
        assert!(report.covered.is_empty());
        assert_eq!(report.last_observed_at, None);

        let report = coverage.query(unknown, at(0), at(15), at(15)).unwrap();
        assert_eq!(report.unknown, [interval(0, 15)]);
        assert!(report.uncovered.is_empty());
        assert!(coverage.query(known, at(15), at(15), at(15)).is_none());
        assert!(coverage.query(known, at(14), at(16), at(15)).is_none());
    }

    #[test]
    fn evicted_intervals_stay_unknown() {
        let mut coverage = Coverage::new(Duration::from_secs(3600));
        let key = key(1);
        for index in 0..=MAX_CLOSED_WINDOWS {
            let start = at((index * 2) as i64);
            coverage.opened(key, start);
            coverage.observed(key, start);
            coverage.closed(key, start + TimeDelta::seconds(1));
        }
        let end = at((MAX_CLOSED_WINDOWS * 2 + 2) as i64);
        let report = coverage.query(key, at(0), end, end).unwrap();
        assert_eq!(report.unknown, [interval(0, 1)]);
        assert_eq!(report.covered.len(), MAX_CLOSED_WINDOWS);
        assert_eq!(report.uncovered.len(), MAX_CLOSED_WINDOWS + 1);
    }

    #[test]
    fn key_capacity_evicts_oldest_inactive_history_and_keeps_open_line() {
        let mut coverage = Coverage::new(Duration::from_secs(60));
        let active = key(1);
        let oldest_inactive = key(2);
        coverage.opened(active, at(0));
        coverage.observed(active, at(0));
        coverage.track(oldest_inactive, at(1));
        for index in 3..=MAX_TRACKED_KEYS as i32 + 1 {
            coverage.track(key(index), at(2));
        }

        let report = coverage.query(active, at(0), at(3), at(3)).unwrap();
        assert_eq!(report.covered, [interval(0, 3)]);
        let report = coverage
            .query(oldest_inactive, at(0), at(3), at(3))
            .unwrap();
        assert_eq!(report.unknown, [interval(0, 3)]);
        assert!(report.uncovered.is_empty());
    }
}
