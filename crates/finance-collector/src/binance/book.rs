use std::collections::{BTreeMap, VecDeque};

use anyhow::{Context, Result, ensure};
use finance_market_protocol::{BookDelta, BookSide, BookSnapshot};
use rust_decimal::Decimal;

use super::wire::{Diff, Market, Snapshot, ns};

const BUFFER_LIMIT: usize = 512;
const LEVEL_LIMIT: usize = 20_000;
const BUFFER_LEVEL_LIMIT: usize = 32_000;

struct Update {
    first: u64,
    last: u64,
    previous: Option<u64>,
    exchange: u64,
    delta: BookDelta,
    receive: u64,
}

pub struct DepthFrame<T> {
    pub payload: T,
    pub native_id: u64,
    pub exchange: Option<u64>,
    pub receive: u64,
}

pub enum Depth {
    Pending,
    Initialized(DepthFrame<BookSnapshot>, Vec<DepthFrame<BookDelta>>),
    Updated(DepthFrame<BookDelta>),
    Gap,
}

#[derive(Default)]
pub struct Book {
    pending: VecDeque<Update>,
    snapshot: Option<DepthFrame<BookSnapshot>>,
    pending_levels: usize,
    current: Option<Levels>,
}

#[derive(Default)]
struct Levels {
    last: u64,
    bids: BTreeMap<Decimal, Decimal>,
    asks: BTreeMap<Decimal, Decimal>,
}

impl Levels {
    fn apply(&mut self, delta: &BookDelta, last: u64) -> bool {
        for update in &delta.updates {
            let levels = match update.side {
                BookSide::Bid => &mut self.bids,
                BookSide::Ask => &mut self.asks,
            };
            let price = update.price.value();
            let quantity = update.quantity.value();
            if quantity == Decimal::ZERO {
                levels.remove(&price);
            } else {
                levels.insert(price, quantity);
            }
        }
        self.last = last;
        self.bids.len() + self.asks.len() <= LEVEL_LIMIT
    }
}

impl Book {
    pub fn ready(&self) -> bool {
        self.current.is_some()
    }

    pub fn snapshot(&mut self, snapshot: Snapshot, receive: u64, market: Market) -> Result<Depth> {
        ensure!(
            self.snapshot.is_none() && self.current.is_none(),
            "unexpected depth snapshot"
        );
        let payload = snapshot.payload()?;
        self.snapshot = Some(DepthFrame {
            payload,
            native_id: snapshot.last_update_id,
            exchange: snapshot.event_ms.map(ns).transpose()?,
            receive,
        });
        self.bridge(market)
    }

    pub fn update(&mut self, diff: Diff, receive: u64, market: Market) -> Result<Depth> {
        if !diff.sequence_valid(market) {
            *self = Self::default();
            return Ok(Depth::Gap);
        }
        let delta = diff.payload()?;
        let update = Update {
            first: diff.first,
            last: diff.last,
            previous: diff.previous,
            exchange: ns(diff.event_ms)?,
            delta,
            receive,
        };
        if let Some(current) = &mut self.current {
            if update.last <= current.last {
                return Ok(Depth::Pending);
            }
            if !follows(market, &update, current.last) || !current.apply(&update.delta, update.last)
            {
                *self = Self::default();
                return Ok(Depth::Gap);
            }
            return Ok(Depth::Updated(update.frame()));
        }
        self.pending_levels += update.delta.updates.len();
        if self.pending.len() == BUFFER_LIMIT || self.pending_levels > BUFFER_LEVEL_LIMIT {
            *self = Self::default();
            return Ok(Depth::Gap);
        }
        self.pending.push_back(update);
        self.bridge(market)
    }

    fn bridge(&mut self, market: Market) -> Result<Depth> {
        let Some(snapshot) = &self.snapshot else {
            return Ok(Depth::Pending);
        };
        let target = match market {
            Market::Spot => snapshot
                .native_id
                .checked_add(1)
                .context("depth snapshot update ID overflow")?,
            Market::Perpetual => snapshot.native_id,
        };
        while self
            .pending
            .front()
            .is_some_and(|update| update.last < target)
        {
            self.pending_levels -= self
                .pending
                .pop_front()
                .expect("old buffered update")
                .delta
                .updates
                .len();
        }
        let Some(first) = self.pending.front() else {
            return Ok(Depth::Pending);
        };
        if first.first > target {
            *self = Self::default();
            return Ok(Depth::Gap);
        }
        // Spot REST does not timestamp the snapshot. The bridge provides the
        // exchange-time upper bound of its sequence-aligned baseline.
        let snapshot_exchange = snapshot.exchange.unwrap_or(first.exchange);
        let mut current = Levels {
            last: snapshot.native_id,
            bids: snapshot
                .payload
                .bids
                .iter()
                .map(|v| (v.price.value(), v.quantity.value()))
                .collect(),
            asks: snapshot
                .payload
                .asks
                .iter()
                .map(|v| (v.price.value(), v.quantity.value()))
                .collect(),
        };
        let mut frames = Vec::with_capacity(self.pending.len());
        self.pending_levels = 0;
        for (index, update) in self.pending.drain(..).enumerate() {
            if index > 0 {
                if update.last <= current.last {
                    continue;
                }
                if !follows(market, &update, current.last) {
                    self.snapshot = None;
                    return Ok(Depth::Gap);
                }
            }
            if !current.apply(&update.delta, update.last) {
                self.snapshot = None;
                return Ok(Depth::Gap);
            }
            frames.push(update.frame());
        }
        self.current = Some(current);
        let mut snapshot = self.snapshot.take().expect("pending snapshot");
        snapshot.exchange = Some(snapshot_exchange);
        Ok(Depth::Initialized(snapshot, frames))
    }
}

impl Update {
    fn frame(self) -> DepthFrame<BookDelta> {
        DepthFrame {
            payload: self.delta,
            native_id: self.last,
            exchange: Some(self.exchange),
            receive: self.receive,
        }
    }
}

fn follows(market: Market, diff: &Update, last: u64) -> bool {
    match market {
        Market::Spot => diff.first <= last.saturating_add(1),
        Market::Perpetual => diff.previous == Some(last),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixtures() -> serde_json::Value {
        serde_json::from_str(include_str!("../../fixtures/binance/depth.json")).unwrap()
    }

    fn update(book: &mut Book, data: &serde_json::Value, name: &str, market: Market) -> Depth {
        book.update(
            serde_json::from_value(data[name].clone()).unwrap(),
            10,
            market,
        )
        .unwrap()
    }

    fn snapshot(book: &mut Book, data: &serde_json::Value, name: &str, market: Market) -> Depth {
        book.snapshot(
            serde_json::from_value(data[name].clone()).unwrap(),
            20,
            market,
        )
        .unwrap()
    }

    #[test]
    fn spot_bridges_rebuilds_and_discards_on_sequence_gap() {
        let data = fixtures();
        let mut book = Book::default();
        let market = Market::Spot;
        assert!(matches!(
            update(&mut book, &data, "spot_bridge", market),
            Depth::Pending
        ));
        let Depth::Initialized(initial, bridge) =
            snapshot(&mut book, &data, "spot_snapshot", market)
        else {
            panic!("expected a bridged spot snapshot");
        };
        assert_eq!(initial.native_id, 9_007_199_254_741_000);
        assert_eq!(initial.exchange, Some(1_001_000_000));
        assert_eq!(bridge.len(), 1);
        assert!(matches!(
            update(&mut book, &data, "spot_bridge", market),
            Depth::Pending
        ));
        assert!(matches!(
            update(&mut book, &data, "spot_next", market),
            Depth::Updated(_)
        ));
        let current = book.current.as_ref().unwrap();
        assert_eq!(
            current.bids,
            BTreeMap::from([("10.02".parse().unwrap(), "1.000".parse().unwrap())])
        );
        assert_eq!(
            current.asks,
            BTreeMap::from([("10.04".parse().unwrap(), "5.000".parse().unwrap())])
        );
        assert!(matches!(
            update(&mut book, &data, "spot_gap", market),
            Depth::Gap
        ));
        assert!(!book.ready());
        assert!(matches!(
            update(&mut book, &data, "spot_gap", market),
            Depth::Pending
        ));
        assert!(matches!(
            snapshot(&mut book, &data, "spot_recovery", market),
            Depth::Initialized(..)
        ));
    }

    #[test]
    fn perpetual_bridges_inclusive_snapshot_and_requires_previous_update_id() {
        let data = fixtures();
        let mut book = Book::default();
        let market = Market::Perpetual;
        assert!(matches!(
            snapshot(&mut book, &data, "perp_snapshot", market),
            Depth::Pending
        ));
        let Depth::Initialized(initial, updates) = update(&mut book, &data, "perp_bridge", market)
        else {
            panic!("expected a bridged perpetual snapshot");
        };
        assert_eq!(initial.exchange, Some(1_000_000_000));
        assert_eq!(updates[0].native_id, 800);
        assert!(matches!(
            update(&mut book, &data, "perp_next", market),
            Depth::Updated(_)
        ));
        let current = book.current.as_ref().unwrap();
        assert!(current.bids.is_empty());
        assert_eq!(
            current.asks.get(&"10.04".parse().unwrap()),
            Some(&"2".parse().unwrap())
        );
        assert!(matches!(
            update(&mut book, &data, "perp_gap", market),
            Depth::Gap
        ));
        assert!(!book.ready());
        let mut missing_pu = data["perp_bridge"].clone();
        missing_pu.as_object_mut().unwrap().remove("pu");
        let missing_pu = serde_json::from_value(missing_pu).unwrap();
        assert!(matches!(
            book.update(missing_pu, 21, market).unwrap(),
            Depth::Gap
        ));
    }

    #[test]
    fn stale_snapshot_and_buffered_gap_never_publish_partial_books() {
        let data = fixtures();
        let market = Market::Spot;
        let mut book = Book::default();
        update(&mut book, &data, "spot_next", market);
        assert!(matches!(
            snapshot(&mut book, &data, "spot_snapshot", market),
            Depth::Gap
        ));
        update(&mut book, &data, "spot_bridge", market);
        update(&mut book, &data, "spot_gap", market);
        assert!(matches!(
            snapshot(&mut book, &data, "spot_snapshot", market),
            Depth::Gap
        ));
        assert!(!book.ready());
    }
}
