use anyhow::{Result, ensure};
use async_trait::async_trait;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path, time::Duration};
use tokio::time::sleep;

pub type Watermarks = BTreeMap<String, i64>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Bar {
    pub instrument: String,
    pub ts_ns: i64,
    pub open: String,
    pub high: String,
    pub low: String,
    pub close: String,
    pub volume: String,
}

#[derive(Clone, Debug)]
pub struct SourceStatus {
    pub connected: bool,
    pub mode: String,
    pub detail: String,
}

#[async_trait]
pub trait BarSource: Send {
    async fn subscribe(&mut self, instruments: &[String]) -> Result<()>;
    // next_bar must be cancellation safe. None means finite replay is exhausted.
    async fn next_bar(&mut self) -> Result<Option<Bar>>;
    async fn backfill(
        &mut self,
        instrument: &str,
        from_exclusive: i64,
        to_inclusive: i64,
    ) -> Result<Vec<Bar>>;
    fn status(&self) -> SourceStatus;
}

pub fn now_ns() -> i64 {
    Utc::now()
        .timestamp_nanos_opt()
        .expect("current UTC time fits signed nanoseconds")
}

#[derive(Deserialize, Debug, Clone)]
struct Candle {
    open: String,
    high: String,
    low: String,
    close: String,
    volume: String,
}

// The persisted origin makes (instrument, ts_ns) stable across process restarts.
// Only this source scales the wall clock interval for accelerated replay.
pub struct ReplaySource {
    candles: Vec<Candle>,
    instruments: Vec<String>,
    origin: i64,
    interval: i64,
    limit: Option<i64>,
    next_slot: i64,
    next_instrument: usize,
}
impl ReplaySource {
    pub fn new(path: &Path, origin: i64, interval_ms: u32, limit: Option<u32>) -> Result<Self> {
        ensure!(interval_ms > 0, "invalid replay interval");
        let candles = csv::Reader::from_path(path)?
            .deserialize()
            .collect::<Result<Vec<Candle>, _>>()?;
        ensure!(!candles.is_empty(), "empty replay CSV");
        Ok(Self {
            candles,
            origin,
            interval: i64::from(interval_ms) * 1_000_000,
            limit: limit.map(i64::from),
            instruments: vec![],
            next_slot: 0,
            next_instrument: 0,
        })
    }
    fn bar(&self, instrument: &str, slot: i64) -> Bar {
        let c = &self.candles[slot as usize % self.candles.len()];
        Bar {
            instrument: instrument.into(),
            ts_ns: self.origin + slot * self.interval,
            open: c.open.clone(),
            high: c.high.clone(),
            low: c.low.clone(),
            close: c.close.clone(),
            volume: c.volume.clone(),
        }
    }
}
#[async_trait]
impl BarSource for ReplaySource {
    async fn subscribe(&mut self, instruments: &[String]) -> Result<()> {
        ensure!(!instruments.is_empty(), "no replay instruments");
        self.instruments = instruments.to_vec();
        self.next_slot = ((now_ns() - self.origin) / self.interval).max(0);
        self.next_instrument = 0;
        Ok(())
    }
    async fn next_bar(&mut self) -> Result<Option<Bar>> {
        if self.limit.is_some_and(|n| self.next_slot >= n) {
            return Ok(None);
        }
        let ts = self.origin + self.next_slot * self.interval;
        if let Ok(ns) = u64::try_from(ts - now_ns()) {
            sleep(Duration::from_nanos(ns)).await;
        }
        let bar = self.bar(&self.instruments[self.next_instrument], self.next_slot);
        self.next_instrument += 1;
        if self.next_instrument == self.instruments.len() {
            self.next_slot += 1;
            self.next_instrument = 0;
        }
        Ok(Some(bar))
    }
    async fn backfill(
        &mut self,
        instrument: &str,
        from_exclusive: i64,
        to_inclusive: i64,
    ) -> Result<Vec<Bar>> {
        let first = ((from_exclusive - self.origin).div_euclid(self.interval) + 1).max(0);
        let last = (to_inclusive - self.origin).div_euclid(self.interval);
        let last = self.limit.map_or(last, |n| last.min(n - 1));
        Ok((first..=last)
            .map(|slot| self.bar(instrument, slot))
            .collect())
    }
    fn status(&self) -> SourceStatus {
        SourceStatus {
            connected: !self.instruments.is_empty(),
            mode: "replay".into(),
            detail: "CSV".into(),
        }
    }
}
