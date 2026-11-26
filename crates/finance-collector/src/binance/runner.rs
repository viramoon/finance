use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, ensure};
use finance_market_protocol::{
    Capability, Event, FundingRate, Gap, Instrument, Message, OpenInterest, Reset, Trade, TradeSide,
};
use rust_decimal::Decimal;
use serde::Serialize;
use tokio::{
    sync::watch,
    time::{Instant, MissedTickBehavior, interval, sleep, sleep_until},
};

use super::{
    Config,
    book::{Book, Depth, DepthFrame},
    transport::{Frame, Transport},
    wire::{self, CONTEXT_PRODUCT, Market, PERP_PRODUCT, SPOT_PRODUCT, StreamMessage, ns},
};
use crate::market_ingress::{GapCause, MarketIngress, QualityReport, RecoveryOutcome};

const FLUSH_INTERVAL: Duration = Duration::from_millis(250);
const SESSION_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_BACKOFF: u64 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Status {
    DisabledByProfile,
    Connecting,
    Synchronizing,
    Live,
    Recovering,
    RateLimited { http_status: u16 },
    RegionRestricted { http_status: u16 },
    Stopped,
}

pub trait Observer {
    fn status(&mut self, _status: Status) {}
    /// Called only for events admitted by F3 and authorized for live display.
    fn display(&mut self, _event: &Event) {}
}

pub async fn run(
    config: &Config,
    root: &Path,
    stop: watch::Receiver<bool>,
    observer: &mut impl Observer,
) -> Result<Status> {
    run_with(config, root, stop, observer, Transport::start).await
}

pub(super) trait Session {
    fn next(&mut self) -> impl Future<Output = Result<Frame>>;
    fn close(&mut self) -> impl Future<Output = Result<()>>;
}

impl Session for Transport {
    async fn next(&mut self) -> Result<Frame> {
        self.next().await
    }
    async fn close(&mut self) -> Result<()> {
        self.close().await
    }
}

pub(super) async fn run_with<T: Session>(
    config: &Config,
    root: &Path,
    mut stop: watch::Receiver<bool>,
    observer: &mut impl Observer,
    mut connect: impl FnMut(&Config) -> Result<T>,
) -> Result<Status> {
    if !config.enabled {
        observer.status(Status::DisabledByProfile);
        return Ok(Status::DisabledByProfile);
    }
    let mut feed = Feed::open(config, root, observer)?;
    let mut backoff = 1;
    while !*stop.borrow() {
        feed.observer.status(Status::Connecting);
        let start = Instant::now();
        let mut session = connect(config)?;
        let result = feed.session(&mut session, &mut stop).await;
        let close = session.close().await;
        if let Ok(
            end @ End::Http {
                code: code @ (403 | 451),
                ..
            },
        ) = result
        {
            // A terminal venue response must stay terminal even if local cleanup fails.
            let cleanup = feed.interrupt(end);
            if let Err(error) = close.and(cleanup) {
                tracing::warn!(%error, "Binance regional refusal cleanup failed");
            }
            let status = Status::RegionRestricted { http_status: code };
            feed.observer.status(status);
            return Ok(status);
        }
        close?;
        let end = result?;
        if matches!(end, End::Stopped) {
            break;
        }
        if start.elapsed() >= Duration::from_secs(MAX_BACKOFF) {
            backoff = 1;
        }
        feed.interrupt(end)?;
        let wait = match end {
            End::Http {
                code: code @ (418 | 429),
                retry_after,
            } => {
                feed.observer
                    .status(Status::RateLimited { http_status: code });
                retry_after.unwrap_or(MAX_BACKOFF).max(MAX_BACKOFF)
            }
            _ => {
                feed.observer.status(Status::Recovering);
                backoff
            }
        };
        tokio::select! {
            _ = stop.changed() => break,
            _ = sleep(Duration::from_secs(wait.min(86_400))) => {}
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
    feed.flush()?;
    feed.observer.status(Status::Stopped);
    Ok(Status::Stopped)
}

#[derive(Clone, Copy)]
enum End {
    Stopped,
    Disconnect,
    DepthGap(Market, usize, &'static str),
    Http { code: u16, retry_after: Option<u64> },
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Channel {
    Depth,
    Trades,
    Funding,
    Interest,
}

impl Channel {
    const MARKET: [Self; 2] = [Self::Depth, Self::Trades];
    const CONTEXT: [Self; 2] = [Self::Funding, Self::Interest];

    fn name(self) -> &'static str {
        match self {
            Self::Depth => "depth",
            Self::Trades => "trades",
            Self::Funding => "funding_rate",
            Self::Interest => "open_interest",
        }
    }
}

struct Product {
    capability: Capability,
    ingress: MarketIngress,
    license: finance_market_protocol::Token,
    display_live: bool,
    base_epoch: u64,
}

struct Feed<'a, O> {
    config: &'a Config,
    observer: &'a mut O,
    products: BTreeMap<String, Product>,
    markets: BTreeMap<Market, Vec<Symbol>>,
}

struct Symbol {
    native: String,
    instrument: Instrument,
    streams: BTreeMap<Channel, Stream>,
    book: Book,
    last_trade: Option<u64>,
    last_interest: Option<u64>,
}

struct Stream {
    epoch: u64,
    seq: u64,
    last_receive: Option<u64>,
    gap: Option<(u64, GapCause)>,
}

struct Observation {
    message: Message,
    native: Option<u64>,
    exchange: Option<u64>,
    receive: u64,
}

impl Observation {
    fn control(message: Message, receive: u64) -> Self {
        Self {
            message,
            native: None,
            exchange: None,
            receive,
        }
    }

    fn event(message: Message, native: Option<u64>, exchange: u64, receive: u64) -> Self {
        Self {
            message,
            native,
            exchange: Some(exchange),
            receive,
        }
    }
}

impl Product {
    fn publish(
        &mut self,
        instrument: &Instrument,
        channel: Channel,
        stream: &mut Stream,
        observation: Observation,
        observer: &mut impl Observer,
    ) -> Result<()> {
        if !matches!(observation.message, Message::Reset(_)) {
            stream.seq = stream
                .seq
                .checked_add(1)
                .context("Binance sequence exhausted")?;
        }
        let event = Event {
            schema_version: self.capability.schema_version,
            channel: channel.name().parse()?,
            source: self.capability.source.clone(),
            product: self.capability.product.clone(),
            instrument_id: instrument.instrument_id.clone(),
            asset_kind: instrument.asset_kind,
            venue: instrument.venue.clone(),
            exchange_ts: observation.exchange.map(Into::into),
            receive_ts: observation.receive.into(),
            first_available_ts: now()?.max(observation.receive).into(),
            epoch: stream.epoch.into(),
            seq: stream.seq.into(),
            cursor: format!("{}:{}", stream.epoch, stream.seq).parse()?,
            revision: 0.into(),
            license: self.license.clone(),
            native_id: observation
                .native
                .map(|id| id.to_string().parse())
                .transpose()?,
            message: observation.message,
        };
        if self.display_live {
            self.ingress
                .publish(event.clone(), instrument, &self.capability)?;
            observer.display(&event);
        } else {
            self.ingress.publish(event, instrument, &self.capability)?;
        }
        if observation.exchange.is_some() {
            let observed = stream.last_receive.unwrap_or(0).max(observation.receive);
            stream.last_receive = Some(observed);
            if let Some((from, cause)) = stream.gap.take() {
                self.report(
                    instrument,
                    channel,
                    from,
                    observed,
                    cause,
                    Some(RecoveryOutcome::HistoryUnrecoverable),
                )?;
                if channel == Channel::Depth {
                    self.report(
                        instrument,
                        channel,
                        from,
                        observed,
                        cause,
                        Some(RecoveryOutcome::BookRestoredFromSnapshot),
                    )?;
                }
            }
        }
        Ok(())
    }

    fn report(
        &mut self,
        instrument: &Instrument,
        channel: Channel,
        from: u64,
        to: u64,
        cause: GapCause,
        outcome: Option<RecoveryOutcome>,
    ) -> Result<()> {
        self.ingress.report_quality(QualityReport {
            channel: channel.name().parse()?,
            instrument_id: instrument.instrument_id.clone(),
            from_ts: from.into(),
            to_ts: to.max(from).into(),
            cause,
            outcome,
            count: (outcome == Some(RecoveryOutcome::BookRestoredFromSnapshot)).then_some(1.into()),
        })
    }
}

impl<'a, O: Observer> Feed<'a, O> {
    fn open(config: &'a Config, root: &Path, observer: &'a mut O) -> Result<Self> {
        let mut products = BTreeMap::new();
        let display_epoch = now()?;
        for policy in config.policies()? {
            let name = policy.product.as_str().to_owned();
            let capability = wire::capability(&policy, name == CONTEXT_PRODUCT);
            let ingress = MarketIngress::open(root, policy.clone())?;
            let base_epoch = match ingress.writer_generation() {
                // Keep restarts and in-process resets disjoint in persisted streams.
                Some(generation) => generation
                    .checked_mul(1 << 32)
                    .context("Binance writer epoch exhausted")?,
                None => display_epoch,
            };
            products.insert(
                name,
                Product {
                    capability,
                    ingress,
                    license: policy.license,
                    display_live: policy.display_live,
                    base_epoch,
                },
            );
        }
        Ok(Self {
            config,
            observer,
            products,
            markets: BTreeMap::new(),
        })
    }

    fn flush(&mut self) -> Result<()> {
        for product in self.products.values_mut() {
            product.ingress.flush()?;
            product.ingress.expire_display();
        }
        Ok(())
    }

    async fn session(
        &mut self,
        session: &mut impl Session,
        stop: &mut watch::Receiver<bool>,
    ) -> Result<End> {
        let mut timer = interval(FLUSH_INTERVAL);
        timer.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut idle = Instant::now() + SESSION_TIMEOUT;
        let mut syncing = Some(idle);
        let mut metadata = BTreeSet::new();
        loop {
            if *stop.borrow() {
                return Ok(End::Stopped);
            }
            tokio::select! {
                _ = stop.changed() => return Ok(End::Stopped),
                _ = timer.tick() => self.flush()?,
                _ = sleep_until(idle.min(syncing.unwrap_or(idle))) => return Ok(End::Disconnect),
                frame = session.next() => {
                    let Ok(frame) = frame else { return Ok(End::Disconnect) };
                    idle = Instant::now() + SESSION_TIMEOUT;
                    if let Frame::Failure { status, retry_after } = frame {
                        return Ok(status.map_or(End::Disconnect, |code| End::Http { code, retry_after }));
                    }
                    if let Frame::Metadata { market, data } = frame {
                        ensure!(metadata.insert(market), "duplicate Binance metadata frame");
                        self.metadata(market, data)?;
                        self.observer.status(Status::Synchronizing);
                    } else if let Some(end) = self.data(frame, &metadata)? {
                        return Ok(end);
                    }
                    if syncing.is_some() && self.live() {
                        syncing = None;
                        self.observer.status(Status::Live);
                    }
                }
            }
        }
    }

    fn metadata(&mut self, market: Market, info: wire::ExchangeInfo) -> Result<()> {
        let (product, symbols, channels) = match market {
            Market::Spot if self.config.spot.is_some() => (
                SPOT_PRODUCT,
                wire::SYMBOLS.to_vec(),
                Channel::MARKET.to_vec(),
            ),
            Market::Perpetual => {
                let config = self
                    .config
                    .perpetual
                    .as_ref()
                    .context("unexpected perpetual metadata")?;
                (
                    PERP_PRODUCT,
                    config.symbols.iter().map(String::as_str).collect(),
                    [Channel::MARKET, Channel::CONTEXT].concat(),
                )
            }
            Market::Spot => anyhow::bail!("unexpected spot metadata"),
        };
        let mut policies = self.config.policies()?;
        policies.retain(|p| {
            p.product.as_str() == product
                || (market == Market::Perpetual && p.product.as_str() == CONTEXT_PRODUCT)
        });
        let policies: Vec<_> = policies.iter().collect();
        let configured = self.markets.entry(market).or_default();
        for (index, native) in symbols.into_iter().enumerate() {
            let mut records = info.symbols.iter().filter(|info| info.symbol == native);
            let instrument = records
                .next()
                .context("missing selected Binance symbol")?
                .instrument(market, &policies)?;
            ensure!(
                records.next().is_none(),
                "duplicate selected Binance symbol"
            );
            if let Some(symbol) = configured.get_mut(index) {
                symbol.instrument = instrument;
                continue;
            }
            let mut streams = BTreeMap::new();
            let receive = now()?;
            for &channel in &channels {
                let product = self
                    .products
                    .get_mut(product_for(market, channel))
                    .expect("enabled Binance product");
                let mut stream = Stream {
                    epoch: product.base_epoch,
                    seq: 0,
                    last_receive: None,
                    gap: None,
                };
                product.publish(
                    &instrument,
                    channel,
                    &mut stream,
                    reset("binance_session_start", receive)?,
                    self.observer,
                )?;
                streams.insert(channel, stream);
            }
            configured.push(Symbol {
                native: native.to_owned(),
                instrument,
                streams,
                book: Book::default(),
                last_trade: None,
                last_interest: None,
            });
        }
        Ok(())
    }

    fn live(&self) -> bool {
        let expected =
            usize::from(self.config.spot.is_some()) + usize::from(self.config.perpetual.is_some());
        self.markets.len() == expected
            && self
                .markets
                .values()
                .flatten()
                .all(|symbol| symbol.book.ready())
    }

    fn data(&mut self, frame: Frame, metadata: &BTreeSet<Market>) -> Result<Option<End>> {
        match frame {
            Frame::Snapshot {
                market,
                symbol,
                received,
                data,
            } => {
                let (index, state) = locate(&mut self.markets, metadata, market, &symbol)?;
                let depth = state.book.snapshot(data, received, market)?;
                self.depth(market, index, depth)
            }
            Frame::Stream {
                market,
                received,
                data: StreamMessage::Depth { symbol, diff },
            } => {
                let (index, state) = locate(&mut self.markets, metadata, market, &symbol)?;
                let depth = state.book.update(
                    diff,
                    received,
                    market,
                    &state.instrument.tick_size,
                    &state.instrument.lot_size,
                )?;
                self.depth(market, index, depth)
            }
            Frame::Stream {
                market,
                received,
                data,
            } => {
                let (symbol, channel, observation) = match data {
                    StreamMessage::SpotTrade {
                        symbol,
                        id,
                        trade_ms,
                        price,
                        quantity,
                        buyer_maker,
                    } if market == Market::Spot => (
                        symbol,
                        Channel::Trades,
                        trade(id, trade_ms, price, quantity, buyer_maker, received)?,
                    ),
                    StreamMessage::PerpetualTrade {
                        symbol,
                        id,
                        trade_ms,
                        price,
                        quantity,
                        buyer_maker,
                    } if market == Market::Perpetual => (
                        symbol,
                        Channel::Trades,
                        trade(id, trade_ms, price, quantity, buyer_maker, received)?,
                    ),
                    StreamMessage::Funding {
                        symbol,
                        event_ms,
                        rate,
                        next_funding_ms,
                    } if market == Market::Perpetual => (
                        symbol,
                        Channel::Funding,
                        Observation::event(
                            Message::FundingRate(FundingRate {
                                rate,
                                next_funding_ts: next_funding_ms
                                    .filter(|&millis| millis > 0)
                                    .map(|millis| ns(millis as u64))
                                    .transpose()?
                                    .map(Into::into),
                            }),
                            None,
                            ns(event_ms)?,
                            received,
                        ),
                    ),
                    _ => anyhow::bail!("unexpected Binance stream message for product"),
                };
                let (index, state) = locate(&mut self.markets, metadata, market, &symbol)?;
                if channel == Channel::Trades {
                    let id = observation.native.expect("Binance trade ID");
                    if state.last_trade.is_some_and(|last| last >= id) {
                        return Ok(None);
                    }
                    state.last_trade = Some(id);
                }
                self.publish(market, index, channel, observation)?;
                Ok(None)
            }
            Frame::Interest { received, data } => {
                let market = Market::Perpetual;
                let (index, state) = locate(&mut self.markets, metadata, market, &data.symbol)?;
                if state.last_interest.is_some_and(|last| last >= data.time) {
                    return Ok(None);
                }
                state.last_interest = Some(data.time);
                self.publish(
                    market,
                    index,
                    Channel::Interest,
                    Observation::event(
                        Message::OpenInterest(OpenInterest {
                            quantity: data.open_interest,
                        }),
                        None,
                        ns(data.time)?,
                        received,
                    ),
                )?;
                Ok(None)
            }
            Frame::Metadata { .. } | Frame::Failure { .. } => {
                unreachable!("session handles control frames")
            }
        }
    }

    fn depth(&mut self, market: Market, index: usize, depth: Depth) -> Result<Option<End>> {
        match depth {
            Depth::Pending => {}
            Depth::Gap => {
                return Ok(Some(End::DepthGap(
                    market,
                    index,
                    "binance_depth_sequence_gap",
                )));
            }
            Depth::InvalidIncrement => {
                return Ok(Some(End::DepthGap(
                    market,
                    index,
                    "binance_depth_invalid_increment",
                )));
            }
            Depth::Initialized(snapshot, updates) => {
                self.publish(
                    market,
                    index,
                    Channel::Depth,
                    depth_observation(snapshot, Message::Snapshot),
                )?;
                for update in updates {
                    self.delta(market, index, update)?;
                }
            }
            Depth::Updated(update) => self.delta(market, index, update)?,
        }
        Ok(None)
    }

    fn delta(
        &mut self,
        market: Market,
        index: usize,
        update: DepthFrame<finance_market_protocol::BookDelta>,
    ) -> Result<()> {
        if !update.payload.updates.is_empty() {
            self.publish(
                market,
                index,
                Channel::Depth,
                depth_observation(update, Message::Delta),
            )?;
        }
        Ok(())
    }

    fn publish(
        &mut self,
        market: Market,
        index: usize,
        channel: Channel,
        observation: Observation,
    ) -> Result<()> {
        let symbol = &mut self.markets.get_mut(&market).expect("configured market")[index];
        let stream = symbol
            .streams
            .get_mut(&channel)
            .expect("configured channel");
        self.products
            .get_mut(product_for(market, channel))
            .expect("enabled product")
            .publish(
                &symbol.instrument,
                channel,
                stream,
                observation,
                self.observer,
            )
    }

    fn interrupt(&mut self, end: End) -> Result<()> {
        let receive = now()?;
        for (&market, symbols) in &mut self.markets {
            for (index, symbol) in symbols.iter_mut().enumerate() {
                symbol.book = Book::default();
                for (&channel, stream) in &mut symbol.streams {
                    if stream.gap.is_some() {
                        continue;
                    }
                    let (cause, reason) = match end {
                        End::DepthGap(m, i, reason)
                            if m == market && i == index && channel == Channel::Depth =>
                        {
                            (GapCause::SourceGap, reason)
                        }
                        End::Http {
                            code: 418 | 429, ..
                        } => (GapCause::SourceRateLimit, "binance_rate_limited"),
                        End::Http {
                            code: 403 | 451, ..
                        } => (GapCause::Disconnected, "binance_region_restricted"),
                        _ => (GapCause::Disconnected, "binance_disconnected"),
                    };
                    let from = stream.last_receive.unwrap_or(receive);
                    let product = self
                        .products
                        .get_mut(product_for(market, channel))
                        .expect("enabled product");
                    let gap = Message::Gap(Gap {
                        from_ts: from.into(),
                        to_ts: receive.max(from).into(),
                        reason: reason.parse()?,
                    });
                    product.publish(
                        &symbol.instrument,
                        channel,
                        stream,
                        Observation::control(gap, receive),
                        self.observer,
                    )?;
                    product.report(&symbol.instrument, channel, from, receive, cause, None)?;
                    stream.gap = Some((from, cause));
                    stream.epoch = stream
                        .epoch
                        .checked_add(1)
                        .context("Binance epoch exhausted")?;
                    ensure!(
                        stream.epoch - product.base_epoch < (1 << 32),
                        "Binance epoch allocation exhausted"
                    );
                    stream.seq = 0;
                    product.publish(
                        &symbol.instrument,
                        channel,
                        stream,
                        reset(reason, receive)?,
                        self.observer,
                    )?;
                }
            }
        }
        self.flush()
    }
}

fn locate<'a>(
    markets: &'a mut BTreeMap<Market, Vec<Symbol>>,
    metadata: &BTreeSet<Market>,
    market: Market,
    native: &str,
) -> Result<(usize, &'a mut Symbol)> {
    ensure!(
        metadata.contains(&market),
        "Binance data arrived before metadata"
    );
    markets
        .get_mut(&market)
        .and_then(|symbols| {
            symbols
                .iter_mut()
                .enumerate()
                .find(|(_, state)| state.native == native)
        })
        .context("unexpected symbol from Binance")
}

fn product_for(market: Market, channel: Channel) -> &'static str {
    match (market, channel) {
        (Market::Spot, _) => SPOT_PRODUCT,
        (Market::Perpetual, Channel::Depth | Channel::Trades) => PERP_PRODUCT,
        (Market::Perpetual, Channel::Funding | Channel::Interest) => CONTEXT_PRODUCT,
    }
}

fn depth_observation<T>(frame: DepthFrame<T>, message: impl FnOnce(T) -> Message) -> Observation {
    Observation {
        message: message(frame.payload),
        native: Some(frame.native_id),
        exchange: frame.exchange,
        receive: frame.receive,
    }
}

fn trade(
    id: u64,
    millis: u64,
    price: finance_market_protocol::DecimalText,
    quantity: finance_market_protocol::DecimalText,
    buyer_maker: bool,
    receive: u64,
) -> Result<Observation> {
    ensure!(price.value() > Decimal::ZERO, "invalid Binance trade price");
    let side = if buyer_maker {
        TradeSide::Sell
    } else {
        TradeSide::Buy
    };
    Ok(Observation::event(
        Message::Trade(Trade {
            price,
            quantity,
            side,
        }),
        Some(id),
        ns(millis)?,
        receive,
    ))
}

fn reset(reason: &str, receive: u64) -> Result<Observation> {
    Ok(Observation::control(
        Message::Reset(Reset {
            reason: reason.parse()?,
        }),
        receive,
    ))
}

fn now() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_nanos()
        .try_into()?)
}
