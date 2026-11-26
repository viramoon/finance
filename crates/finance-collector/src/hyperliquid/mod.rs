//! Public Hyperliquid perpetual market data published through F3.

mod wire;

use std::{
    collections::{BTreeMap, BTreeSet, HashSet, VecDeque},
    path::Path,
};

use anyhow::{Context, Result, bail, ensure};
use finance_market_protocol::{
    AssetKind, BookSnapshot, Capability, DelayTier, Event, Gap, Instrument, MarketKind, Message,
    NonEmptyText, Reset, SchemaVersion, SettlementKind, SupplierSymbol, Token,
};
use serde::Deserialize;

use crate::{
    market_ingress::{
        GapCause, MarketIngress, ProductPolicy, QualityReport, RecoveryOutcome, wal_root,
    },
    market_wal::{MAX_REPLAY_RECORDS, WalReader},
};
pub use wire::TransportEvent;
use wire::{Frame, Metadata};

pub const SOURCE: &str = "hyperliquid";
pub const PRODUCT: &str = "perp-public";
const DEPTH: u32 = 20;
const NS_PER_MS: u64 = 1_000_000;
const RECORD_DEDUP_NS: u64 = 120_000_000_000;
const DISPLAY_DEDUP_NS: u64 = 5_000_000_000;
const MAX_TRADE_IDS: usize = 32_768;
const RESTART_LOOKBACK: u64 = 65_536;
const CONTINUITY_UNKNOWN: &str = "continuity_unknown";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Coin {
    Btc,
    Eth,
    Sol,
}

impl Coin {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Btc => "BTC",
            Self::Eth => "ETH",
            Self::Sol => "SOL",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub coins: BTreeSet<Coin>,
    pub policy: ProductPolicy,
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.coins.is_empty(),
            "select at least one Hyperliquid coin"
        );
        ensure!(
            self.policy.source.as_str() == SOURCE && self.policy.product.as_str() == PRODUCT,
            "Hyperliquid policy must bind hyperliquid/perp-public"
        );
        ensure!(
            self.policy.record_local || self.policy.display_live,
            "Hyperliquid policy must permit recording or live display"
        );
        ensure!(
            !self.policy.backup_personal_drive || self.policy.record_local,
            "WAL backup requires permission to record locally"
        );
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Channel {
    Book,
    Trades,
    Context,
}

impl Channel {
    const ALL: [Self; 3] = [Self::Book, Self::Trades, Self::Context];

    fn token(self) -> Token {
        match self {
            Self::Book => "book",
            Self::Trades => "trades",
            Self::Context => "perpetual_context",
        }
        .parse()
        .expect("static channel token")
    }
}

#[derive(Default)]
struct Stream {
    seq: u64,
    last_receive_ts: Option<u64>,
    gap_from_ts: Option<u64>,
}

#[derive(Default)]
struct TradeIds {
    order: VecDeque<(u64, (u64, u64))>,
    seen: HashSet<(u64, u64)>,
}

impl TradeIds {
    fn expire(&mut self, now: u64, window: u64) {
        while self
            .order
            .front()
            .is_some_and(|(received, _)| now.saturating_sub(*received) >= window)
        {
            let (_, id) = self.order.pop_front().expect("front was present");
            self.seen.remove(&id);
        }
    }

    fn insert(&mut self, now: u64, id: (u64, u64)) {
        if !self.seen.insert(id) {
            return;
        }
        if self.order.len() == MAX_TRADE_IDS {
            let (_, oldest) = self.order.pop_front().expect("trade ID cache is full");
            self.seen.remove(&oldest);
        }
        self.order.push_back((now, id));
    }
}

struct Asset {
    instrument: Instrument,
    streams: [Stream; 3],
    last_book_time: Option<u64>,
    trade_ids: TradeIds,
}

fn step(decimals: u32) -> String {
    if decimals == 0 {
        return "1".into();
    }
    format!("0.{}1", "0".repeat((decimals - 1) as usize))
}

fn instrument(coin: Coin, size_decimals: u32, policy: &ProductPolicy) -> Result<Instrument> {
    let symbol = coin.as_str();
    ensure!(
        size_decimals <= 6,
        "unsupported size precision for {symbol}"
    );
    let instrument = Instrument {
        schema_version: SchemaVersion::V1,
        instrument_id: format!("{symbol}-USD-PERP.HYPERLIQUID").parse()?,
        asset_kind: AssetKind::Perpetual,
        venue: "HYPERLIQUID".parse()?,
        quote_currency: "USD".parse()?,
        tick_size: step(6 - size_decimals).parse()?,
        lot_size: step(size_decimals).parse()?,
        contract_multiplier: "1".parse()?,
        settlement_kind: SettlementKind::Cash,
        settlement_currency: Some("USDC".parse()?),
        calendar_id: "24X7".parse()?,
        supplier_symbols: vec![SupplierSymbol {
            source: policy.source.clone(),
            product: policy.product.clone(),
            symbol: symbol.parse()?,
        }],
    };
    instrument.validate()?;
    Ok(instrument)
}

pub struct Hyperliquid {
    ingress: MarketIngress,
    policy: ProductPolicy,
    capability: Capability,
    assets: BTreeMap<Coin, Asset>,
    epoch_base: u64,
    session: u32,
    connected_since: Option<u64>,
    dedup_window: u64,
}

impl Hyperliquid {
    pub fn open(data_root: &Path, config: Config, metadata: &str, receive_ts: u64) -> Result<Self> {
        config.validate()?;
        let metadata: Metadata = decode(metadata, "metadata")?;
        let mut assets = BTreeMap::new();
        for coin in config.coins {
            let mut definitions = metadata
                .universe
                .iter()
                .filter(|asset| asset.name == coin.as_str());
            let definition = definitions
                .next()
                .with_context(|| format!("missing metadata for {}", coin.as_str()))?;
            ensure!(
                !definition.is_delisted && definitions.next().is_none(),
                "invalid metadata for {}",
                coin.as_str()
            );
            assets.insert(
                coin,
                Asset {
                    instrument: instrument(coin, definition.sz_decimals, &config.policy)?,
                    streams: Default::default(),
                    last_book_time: None,
                    trade_ids: TradeIds::default(),
                },
            );
        }
        let policy = config.policy;
        let capability = Capability {
            schema_version: SchemaVersion::V2,
            source: policy.source.clone(),
            product: policy.product.clone(),
            message_kinds: vec![
                MarketKind::Trade,
                MarketKind::Snapshot,
                MarketKind::PerpetualContext,
            ],
            max_depth: DEPTH,
            can_backfill: false,
            delay_tier: DelayTier::Realtime,
        };
        capability.validate()?;
        let ingress = MarketIngress::open(data_root, policy.clone())?;
        // The high bits separate persisted epochs across WAL writer restarts.
        let epoch_base = if let Some(generation) = ingress.writer_generation() {
            restore_recent(data_root, &mut assets, receive_ts)?;
            generation
                .checked_mul(1 << 32)
                .context("Hyperliquid writer epoch exhausted")?
        } else {
            receive_ts
        };
        let dedup_window = if policy.record_local {
            RECORD_DEDUP_NS
        } else {
            DISPLAY_DEDUP_NS
        };
        Ok(Self {
            ingress,
            policy,
            capability,
            assets,
            epoch_base,
            session: 0,
            connected_since: None,
            dedup_window,
        })
    }

    pub fn instruments(&self) -> impl Iterator<Item = &Instrument> {
        self.assets.values().map(|asset| &asset.instrument)
    }

    pub fn capability(&self) -> &Capability {
        &self.capability
    }

    pub fn ingress(&mut self) -> &mut MarketIngress {
        &mut self.ingress
    }

    pub fn expire_display(&mut self, receive_ts: u64) {
        self.ingress.expire_display();
        for asset in self.assets.values_mut() {
            asset.trade_ids.expire(receive_ts, self.dedup_window);
        }
    }

    pub fn apply(&mut self, event: TransportEvent) -> Result<()> {
        match event {
            TransportEvent::Metadata { .. } => bail!("unexpected second metadata message"),
            TransportEvent::Connected { receive_ts } => self.connect(receive_ts.get()),
            TransportEvent::Disconnected { receive_ts } => self.disconnect(receive_ts.get()),
            TransportEvent::Frame { data, receive_ts } => self.frame(&data, receive_ts.get()),
        }
    }

    fn connect(&mut self, receive_ts: u64) -> Result<()> {
        ensure!(
            self.connected_since.is_none(),
            "duplicate Hyperliquid connected message"
        );
        if self.session == 0 {
            self.reset_streams(receive_ts, "subscription_started")?;
        }
        self.connected_since = Some(receive_ts);
        Ok(())
    }

    pub fn disconnect(&mut self, receive_ts: u64) -> Result<()> {
        let Some(started) = self.connected_since.take() else {
            return Ok(());
        };
        let mut observed = false;
        for asset in self.assets.values_mut() {
            for channel in Channel::ALL {
                let stream = &mut asset.streams[channel as usize];
                if stream.gap_from_ts.is_none() {
                    observed = true;
                    let from = stream
                        .last_receive_ts
                        .take()
                        .map_or(started, |last| last.saturating_add(1));
                    stream.gap_from_ts = Some(from);
                    self.ingress.report_quality(quality(
                        &asset.instrument,
                        channel,
                        from,
                        receive_ts.max(from),
                        None,
                        None,
                    ))?;
                }
            }
        }
        if observed {
            self.reset_streams(receive_ts, "disconnected")?;
            self.ingress.flush()?;
        }
        Ok(())
    }

    fn reset_streams(&mut self, receive_ts: u64, reason: &str) -> Result<()> {
        self.session = self
            .session
            .checked_add(1)
            .context("Hyperliquid session epoch exhausted")?;
        let epoch = self.epoch()?;
        let reason: NonEmptyText = reason.parse()?;
        for asset in self.assets.values_mut() {
            for channel in Channel::ALL {
                self.ingress.publish(
                    event(
                        &self.policy,
                        &asset.instrument,
                        channel,
                        epoch,
                        0,
                        Observation {
                            message: Message::Reset(Reset {
                                reason: reason.clone(),
                            }),
                            exchange_ts: None,
                            receive_ts,
                            native_id: None,
                        },
                    ),
                    &asset.instrument,
                    &self.capability,
                )?;
                asset.streams[channel as usize].seq = 0;
            }
        }
        Ok(())
    }

    fn epoch(&self) -> Result<u64> {
        self.epoch_base
            .checked_add(u64::from(self.session))
            .context("Hyperliquid epoch exhausted")
    }

    fn frame(&mut self, frame: &str, receive_ts: u64) -> Result<()> {
        ensure!(
            self.connected_since.is_some(),
            "Hyperliquid frame outside a connected session"
        );
        match decode(frame, "WebSocket frame")? {
            Frame::Book(book) => {
                let coin = book.coin;
                let asset = self.assets.get(&coin).context("unexpected book coin")?;
                if asset
                    .last_book_time
                    .is_some_and(|latest| book.time < latest)
                {
                    return Ok(());
                }
                let [bids, asks] = book.levels;
                let snapshot = BookSnapshot {
                    bids: bids
                        .into_iter()
                        .map(TryInto::try_into)
                        .collect::<Result<_>>()?,
                    asks: asks
                        .into_iter()
                        .map(TryInto::try_into)
                        .collect::<Result<_>>()?,
                };
                self.publish(
                    coin,
                    Channel::Book,
                    Observation {
                        message: Message::Snapshot(snapshot),
                        exchange_ts: Some(nanos(book.time)?),
                        receive_ts,
                        native_id: None,
                    },
                )?;
                self.assets
                    .get_mut(&coin)
                    .expect("mapped coin")
                    .last_book_time = Some(book.time);
            }
            Frame::Trades(trades) => {
                for raw in trades {
                    let coin = raw.coin;
                    let id = (raw.time, raw.tid);
                    let asset = self
                        .assets
                        .get_mut(&coin)
                        .context("unexpected trade coin")?;
                    asset.trade_ids.expire(receive_ts, self.dedup_window);
                    if asset.trade_ids.seen.contains(&id) {
                        continue;
                    }
                    self.publish(
                        coin,
                        Channel::Trades,
                        Observation {
                            message: Message::Trade(raw.try_into()?),
                            exchange_ts: Some(nanos(id.0)?),
                            receive_ts,
                            native_id: Some(
                                format!("{}:{}:{}", coin.as_str(), id.0, id.1).parse()?,
                            ),
                        },
                    )?;
                    self.assets
                        .get_mut(&coin)
                        .expect("mapped coin")
                        .trade_ids
                        .insert(receive_ts, id);
                }
            }
            Frame::Context(context) => self.publish(
                context.coin,
                Channel::Context,
                Observation {
                    message: Message::PerpetualContext(context.ctx.into()),
                    exchange_ts: None,
                    receive_ts,
                    native_id: None,
                },
            )?,
            Frame::Error => bail!("Hyperliquid refused a public subscription"),
            Frame::Other => {}
        }
        Ok(())
    }

    fn publish(&mut self, coin: Coin, channel: Channel, observation: Observation) -> Result<()> {
        let epoch = self.epoch()?;
        let asset = self
            .assets
            .get_mut(&coin)
            .context("unexpected context coin")?;
        let stream = &mut asset.streams[channel as usize];
        let receive_ts = observation.receive_ts;
        let from = stream.gap_from_ts;
        let has_gap = from.is_some_and(|start| start < receive_ts);
        let seq = stream
            .seq
            .checked_add(if has_gap { 2 } else { 1 })
            .context("Hyperliquid sequence exhausted")?;
        let market = event(
            &self.policy,
            &asset.instrument,
            channel,
            epoch,
            seq,
            observation,
        );
        market.validate_against(&asset.instrument, &self.capability)?;

        if let Some(from) = from.filter(|_| has_gap) {
            let to = receive_ts - 1;
            self.ingress.publish(
                event(
                    &self.policy,
                    &asset.instrument,
                    channel,
                    epoch,
                    seq - 1,
                    Observation {
                        message: Message::Gap(Gap {
                            from_ts: from.into(),
                            to_ts: to.into(),
                            reason: CONTINUITY_UNKNOWN.parse()?,
                        }),
                        exchange_ts: None,
                        receive_ts,
                        native_id: None,
                    },
                ),
                &asset.instrument,
                &self.capability,
            )?;
        }
        self.ingress
            .publish(market, &asset.instrument, &self.capability)?;
        if let Some(from) = from {
            self.ingress.flush()?;
            if has_gap {
                self.ingress.report_quality(quality(
                    &asset.instrument,
                    channel,
                    from,
                    receive_ts - 1,
                    Some(RecoveryOutcome::HistoryUnrecoverable),
                    None,
                ))?;
            }
        }
        if let Some(from) = from
            && channel == Channel::Book
        {
            self.ingress.report_quality(quality(
                &asset.instrument,
                channel,
                from.min(receive_ts),
                receive_ts,
                Some(RecoveryOutcome::BookRestoredFromSnapshot),
                Some(1),
            ))?;
        }
        stream.seq = seq;
        stream.gap_from_ts = None;
        stream.last_receive_ts = Some(receive_ts);
        Ok(())
    }
}

fn restore_recent(data_root: &Path, assets: &mut BTreeMap<Coin, Asset>, now: u64) -> Result<()> {
    let reader = WalReader::open(&wal_root(data_root), SOURCE, PRODUCT)?;
    let cutoff = reader.durable_position();
    let mut cursor = cutoff.saturating_sub(RESTART_LOOKBACK);
    while cursor < cutoff {
        let page = reader.read_after(cursor, MAX_REPLAY_RECORDS)?;
        cursor = page
            .last()
            .context("missing committed Hyperliquid WAL page")?
            .position
            .get();
        for record in page {
            let event = record.event;
            let Some(asset) = assets
                .values_mut()
                .find(|asset| asset.instrument.instrument_id == event.instrument_id)
            else {
                continue;
            };
            let received = event.receive_ts.get();
            let channel = match (event.message, event.exchange_ts) {
                (Message::Snapshot(_), Some(time)) => {
                    asset.last_book_time = Some(time.get() / NS_PER_MS);
                    Channel::Book
                }
                (Message::Trade(_), Some(time)) => {
                    if now.saturating_sub(received) < RECORD_DEDUP_NS
                        && let Some(id) = event
                            .native_id
                            .as_ref()
                            .and_then(|id| id.as_str().rsplit_once(':'))
                        && let Ok(tid) = id.1.parse()
                    {
                        asset
                            .trade_ids
                            .insert(received, (time.get() / NS_PER_MS, tid));
                    }
                    Channel::Trades
                }
                (Message::PerpetualContext(_), _) => Channel::Context,
                _ => continue,
            };
            asset.streams[channel as usize].gap_from_ts = Some(received.saturating_add(1));
        }
    }
    Ok(())
}

struct Observation {
    message: Message,
    exchange_ts: Option<u64>,
    receive_ts: u64,
    native_id: Option<NonEmptyText>,
}

fn event(
    policy: &ProductPolicy,
    instrument: &Instrument,
    channel: Channel,
    epoch: u64,
    seq: u64,
    observed: Observation,
) -> Event {
    Event {
        schema_version: if matches!(&observed.message, Message::PerpetualContext(_)) {
            SchemaVersion::V2
        } else {
            SchemaVersion::V1
        },
        channel: channel.token(),
        source: policy.source.clone(),
        product: policy.product.clone(),
        instrument_id: instrument.instrument_id.clone(),
        asset_kind: instrument.asset_kind,
        venue: instrument.venue.clone(),
        exchange_ts: observed.exchange_ts.map(Into::into),
        receive_ts: observed.receive_ts.into(),
        first_available_ts: observed.receive_ts.into(),
        epoch: epoch.into(),
        seq: seq.into(),
        cursor: format!("{epoch}:{seq}").parse().expect("nonempty cursor"),
        revision: 0.into(),
        license: policy.license.clone(),
        native_id: observed.native_id,
        message: observed.message,
    }
}

fn quality(
    instrument: &Instrument,
    channel: Channel,
    from: u64,
    to: u64,
    outcome: Option<RecoveryOutcome>,
    count: Option<u64>,
) -> QualityReport {
    QualityReport {
        channel: channel.token(),
        instrument_id: instrument.instrument_id.clone(),
        from_ts: from.into(),
        to_ts: to.into(),
        cause: GapCause::Disconnected,
        outcome,
        count: count.map(Into::into),
    }
}

fn nanos(ms: u64) -> Result<u64> {
    ms.checked_mul(NS_PER_MS)
        .context("Hyperliquid timestamp exceeds nanosecond range")
}

fn decode<'a, T: Deserialize<'a>>(body: &'a str, label: &str) -> Result<T> {
    serde_json::from_str(body).map_err(|error| {
        anyhow::anyhow!(
            "invalid Hyperliquid {label} at line {}, column {}",
            error.line(),
            error.column()
        )
    })
}
