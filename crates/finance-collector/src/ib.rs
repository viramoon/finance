//! Paper market data. Session recovery queues the live stream before the collector
//! reads its durable watermarks and publishes recovered files.
use crate::source::{Bar, BarSource, SourceStatus, now_ns};
use anyhow::{Context, Result, anyhow, bail, ensure};
use async_trait::async_trait;
use finance_ib::{
    ConnectionManager, Error as SessionError, IbConfig, IbTransport, PaperPeer, Recovery,
    RecoveryStage,
};
use ibapi::{
    contracts::Contract,
    market_data::{
        MarketDataType, TradingHours,
        historical::{self, BarTimestamp, HistoricalBarUpdate},
    },
    prelude::{StreamExt, SubscriptionItemStreamExt},
};
use rust_decimal::Decimal;
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use time::OffsetDateTime;
use tokio::{
    sync::{Mutex, mpsc, oneshot, watch},
    task::{JoinHandle, JoinSet},
    time::{Instant, sleep_until, timeout},
};

const MINUTE: i64 = 60_000_000_000;
const DELAY_NS: i64 = 20 * MINUTE;
const CHUNK: i64 = 3_600_000_000_000;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(45);

// ibapi 3.2.0 exposes decoded IB prices/volumes as binary64. Convert at the SDK
// boundary, reject non-finite/out-of-range data and verify roundtrip. No financial
// arithmetic (including accumulation) runs in binary floating point here.
fn decimal_text(value: f64) -> Result<String> {
    let text = value.to_string();
    let dec = Decimal::from_str_exact(&text)
        .with_context(|| format!("unrepresentable IB decimal {text}"))?;
    ensure!(
        dec.to_string().parse::<f64>()? == value,
        "IB decimal did not roundtrip"
    );
    Ok(dec.to_string())
}
fn closed(instrument: &str, b: historical::Bar) -> Result<Bar> {
    let BarTimestamp::DateTime(at) = b.date else {
        bail!("expected intraday bar")
    };
    let begin = i64::try_from(at.unix_timestamp_nanos())?;
    let end = begin.checked_add(MINUTE).context("timestamp overflow")?;
    Ok(Bar {
        instrument: instrument.into(),
        ts_ns: end,
        open: decimal_text(b.open)?,
        high: decimal_text(b.high)?,
        low: decimal_text(b.low)?,
        close: decimal_text(b.close)?,
        volume: decimal_text(b.volume)?,
    })
}
fn contract(id: &str) -> Result<Contract> {
    let (ticker, exchange) = id
        .rsplit_once('.')
        .context("expected SYMBOL.PRIMARY_EXCHANGE")?;
    ensure!(
        !ticker.is_empty() && !exchange.is_empty(),
        "empty stock identifier"
    );
    Ok(Contract::stock(ticker)
        .primary(exchange)
        .in_currency("USD")
        .build())
}
fn builder<'a>(
    peer: &'a PaperPeer,
    contract: &'a Contract,
) -> historical::HistoricalDataBuilder<'a, ibapi::Client> {
    peer.historical_data(contract, historical::BarSize::Min)
        .what_to_show(historical::WhatToShow::Trades)
        .trading_hours(TradingHours::Extended)
}

#[derive(Clone)]
struct Pacer(Arc<Mutex<Instant>>);
impl Pacer {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(Instant::now())))
    }
    async fn reserve(&self) {
        loop {
            let mut next = self.0.lock().await;
            if *next <= Instant::now() {
                // Shared across all clients of this source. Even identical bulk/stream
                // requests after a reconnect stay outside IB’s 15-second window.
                *next = Instant::now() + Duration::from_secs(16);
                return;
            }
            let deadline = *next;
            drop(next);
            sleep_until(deadline).await;
        }
    }
    async fn penalize(&self, attempt: u32) {
        let mut next = self.0.lock().await;
        *next = (*next).max(Instant::now() + Duration::from_secs(16u64 << attempt.min(4)));
    }
}
async fn paced<T, F, Fut>(pacer: &Pacer, mut request: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, ibapi::Error>>,
{
    for attempt in 0..=3 {
        pacer.reserve().await;
        let result = timeout(REQUEST_TIMEOUT, request())
            .await
            .context("IB market data read timeout")?;
        match result {
            Err(e) if pacing(&e) => {
                pacer.penalize(attempt).await;
                if attempt == 3 {
                    return Err(e.into());
                }
            }
            other => return Ok(other?),
        }
    }
    unreachable!()
}
fn pacing(e: &ibapi::Error) -> bool {
    matches!(e, ibapi::Error::Notice(n) if matches!(n.code,100|420) || (n.code==162 && n.message.to_lowercase().contains("pac")))
}
type Batch = BTreeMap<i64, Bar>;
fn insert_bar(batch: &mut Batch, b: Bar, from: i64, to: i64) -> Result<()> {
    if b.ts_ns > from && b.ts_ns <= to {
        if let Some(existing) = batch.get(&b.ts_ns) {
            ensure!(
                *existing == b,
                "conflicting IB historical bars at {}",
                b.ts_ns
            );
        } else {
            batch.insert(b.ts_ns, b);
        }
    }
    Ok(())
}
#[async_trait]
trait History: Send + Sync {
    async fn fetch(&self, id: &str, from: i64, to: i64) -> Result<Vec<Bar>>;
}
struct IbHistory {
    peer: Arc<PaperPeer>,
    pacer: Pacer,
}
#[async_trait]
impl History for IbHistory {
    async fn fetch(&self, id: &str, from: i64, to: i64) -> Result<Vec<Bar>> {
        let contract = contract(id)?;
        let end = OffsetDateTime::from_unix_timestamp_nanos(i128::from(to))?;
        // Allow for API timestamps being start-of-bar and trim inclusively by close time.
        let seconds = i32::try_from((to - from) / 1_000_000_000 + 120)?;
        let response = paced(&self.pacer, || {
            builder(&self.peer, &contract)
                .ending(end)
                .duration(historical::Duration::seconds(seconds))
                .fetch()
        })
        .await;
        match response {
            Ok(data) => data.bars.into_iter().map(|b| closed(id, b)).collect(),
            Err(e) if matches!(e.downcast_ref::<ibapi::Error>(),Some(ibapi::Error::Notice(n)) if n.code == 162 && n.message.to_lowercase().contains("no data")) => {
                Ok(Vec::new())
            }
            Err(e) => Err(e),
        }
    }
}
async fn backfill_range(history: &impl History, id: &str, from: i64, to: i64) -> Result<Vec<Bar>> {
    let mut result = Batch::new();
    let mut start = from;
    while start < to {
        let end = to.min(start.saturating_add(CHUNK));
        for b in history.fetch(id, start, end).await? {
            insert_bar(&mut result, b, start, end)?;
        }
        start = end;
    }
    Ok(result.into_values().collect())
}

// KeepUpToDate delivers repeated updates for the open minute. Only a later
// timestamp finalizes it. Initial history is reconciled independently by watermarks.
fn update(last: &mut Option<Bar>, bar: Bar) -> Result<Option<Bar>> {
    if let Some(prev) = last.as_ref() {
        ensure!(bar.ts_ns >= prev.ts_ns, "out of order IB update");
        if prev.ts_ns == bar.ts_ns {
            *last = Some(bar);
            return Ok(None);
        }
    }
    Ok(last.replace(bar))
}
type MarketItem = Result<Bar>;
struct Generation {
    peer: Arc<PaperPeer>,
    bars: mpsc::Receiver<MarketItem>,
    disconnected: watch::Receiver<bool>,
    recovered: Option<oneshot::Sender<()>>,
}
struct Hooks {
    ids: Vec<String>,
    pacer: Pacer,
    delayed: bool,
    plans: mpsc::Sender<Generation>,
    tasks: JoinSet<()>,
    disconnected: Option<watch::Sender<bool>>,
    waiting: Option<oneshot::Receiver<()>>,
}
// HMDS only: ask for closed bars at least twenty minutes old. IB delayed
// permissions differ by exchange, so an entitlement error has to stay an error.
async fn delayed_stream(
    id: &str,
    history: &IbHistory,
    tx: &mpsc::Sender<MarketItem>,
    mut cursor: i64,
) -> Result<()> {
    loop {
        let cutoff = now_ns() - DELAY_NS;
        for bar in backfill_range(history, id, cursor, cutoff).await? {
            tx.send(Ok(bar))
                .await
                .context("closed collector generation")?;
        }
        cursor = cutoff;
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}
fn market_update(
    id: &str,
    current: &mut Option<Bar>,
    msg: HistoricalBarUpdate,
) -> Result<Option<Bar>> {
    match msg {
        HistoricalBarUpdate::Historical(history) => {
            if let Some(b) = history.bars.last() {
                *current = Some(closed(id, *b)?);
            }
            Ok(None)
        }
        HistoricalBarUpdate::Update(b) => update(current, closed(id, b)?),
        // HistoricalDataEnd delimits the initial snapshot. keepUpToDate sends
        // HistoricalDataUpdate on this same subscription after the marker.
        HistoricalBarUpdate::End { .. } => Ok(None),
    }
}
impl Hooks {
    async fn subscriptions(&mut self, peer: Arc<PaperPeer>) -> Result<()> {
        self.tasks.shutdown().await;
        let mode = if self.delayed {
            MarketDataType::Delayed
        } else {
            MarketDataType::Realtime
        };
        timeout(REQUEST_TIMEOUT, peer.switch_market_data_type(mode)).await??;
        let (sender, bars) = mpsc::channel::<MarketItem>(256);
        for id in &self.ids {
            let id = id.clone();
            let tx = sender.clone();
            let c = contract(&id)?;
            if self.delayed {
                let hist = IbHistory {
                    peer: peer.clone(),
                    pacer: self.pacer.clone(),
                };
                let cursor = now_ns() - DELAY_NS - MINUTE;
                self.tasks.spawn(async move {
                    if let Err(e) = delayed_stream(&id, &hist, &tx, cursor).await {
                        let _ = tx.send(Err(e)).await;
                    }
                });
                continue;
            }
            let feed = paced(&self.pacer, || {
                builder(&peer, &c)
                    .duration(historical::Duration::seconds(300))
                    .stream()
            })
            .await?;
            let mut stream = feed.filter_data();
            let first = timeout(REQUEST_TIMEOUT, stream.next())
                .await
                .context("initial IB market snapshot timed out")?
                .context("subscription closed before snapshot")?;
            let first = match first {
                Err(e) if pacing(&e) => {
                    self.pacer.penalize(0).await;
                    return Err(e.into());
                }
                other => other?,
            };
            let mut current = None;
            market_update(&id, &mut current, first)?;
            let pacer = self.pacer.clone();
            self.tasks.spawn(async move {
                let result: Result<()> = async {
                    while let Some(item) = stream.next().await {
                        if let Err(e) = &item
                            && pacing(e)
                        {
                            pacer.penalize(0).await;
                        }
                        if let Some(bar) = market_update(&id, &mut current, item?)? {
                            tx.send(Ok(bar))
                                .await
                                .context("closed collector generation")?;
                        }
                    }
                    bail!("IB data stream closed")
                }
                .await;
                if let Err(e) = result {
                    let _ = tx.send(Err(e)).await;
                }
            });
        }
        let (done, waiting) = oneshot::channel();
        let (disconnected, signal) = watch::channel(false);
        self.disconnected = Some(disconnected);
        self.plans
            .send(Generation {
                peer,
                bars,
                recovered: Some(done),
                disconnected: signal,
            })
            .await
            .context("collector stopped")?;
        self.waiting = Some(waiting);
        Ok(())
    }
}
#[async_trait]
impl Recovery<PaperPeer> for Hooks {
    async fn restore(
        &mut self,
        stage: RecoveryStage,
        peer: Arc<PaperPeer>,
    ) -> finance_ib::Result<()> {
        let result: Result<()> = async {
            match stage {
                RecoveryStage::Subscriptions => self.subscriptions(peer).await?,
                RecoveryStage::Backfill => self
                    .waiting
                    .take()
                    .context("no recovery barrier")?
                    .await
                    .context("collector abandoned historical recovery")?,
                RecoveryStage::Reconciliation => {} // read-only collector has no orders/positions
            }
            Ok(())
        }
        .await;
        result.map_err(|e| SessionError::Recovery(format!("{e:#}")))
    }
    async fn check(&mut self) -> finance_ib::Result<()> {
        if self.tasks.try_join_next().is_some() {
            return Err(SessionError::Recovery("market subscription ended".into()));
        }
        Ok(())
    }
    async fn offline(&mut self) {
        if let Some(ended) = self.disconnected.take() {
            ended.send_replace(true);
        }
        self.tasks.shutdown().await;
        self.waiting = None;
    }
}
pub struct IbBarSource {
    config: IbConfig,
    delayed: bool,
    pacer: Pacer,
    manager: Option<JoinHandle<finance_ib::Result<()>>>,
    plans: Option<mpsc::Receiver<Generation>>,
    generation: Option<Generation>,
}
impl IbBarSource {
    pub fn new(delayed: bool) -> Result<Self> {
        Ok(Self {
            config: IbConfig::load()?,
            delayed,
            pacer: Pacer::new(),
            manager: None,
            plans: None,
            generation: None,
        })
    }
}
impl Drop for IbBarSource {
    fn drop(&mut self) {
        if let Some(m) = self.manager.take() {
            m.abort();
        }
    }
}
#[async_trait]
impl BarSource for IbBarSource {
    async fn subscribe(&mut self, ids: &[String]) -> Result<()> {
        self.generation = None;
        ensure!(
            !ids.is_empty() && ids.len() <= 3,
            "IB source supports 1..=3 instruments"
        );
        if self.manager.is_none() {
            let (plans, rx) = mpsc::channel(1);
            self.plans = Some(rx);
            let mut hooks = Hooks {
                ids: ids.to_vec(),
                delayed: self.delayed,
                pacer: self.pacer.clone(),
                plans,
                tasks: JoinSet::new(),
                disconnected: None,
                waiting: None,
            };
            let manager = ConnectionManager::new(self.config.clone(), IbTransport)?;
            self.manager = Some(tokio::spawn(async move { manager.run(&mut hooks).await }));
        }
        let rx = self.plans.as_mut().context("no paper session")?;
        self.generation = Some(
            timeout(Duration::from_secs(2), rx.recv())
                .await
                .context("paper connection/subscription pending")?
                .context("paper manager stopped (verify DU account and Gateway)")?,
        );
        Ok(())
    }
    async fn backfill(&mut self, id: &str, from: i64, to: i64) -> Result<Vec<Bar>> {
        let generation = self
            .generation
            .as_mut()
            .context("no verified paper session")?;
        let history = IbHistory {
            peer: generation.peer.clone(),
            pacer: self.pacer.clone(),
        };
        let cutoff = if self.delayed {
            to.min(now_ns() - DELAY_NS)
        } else {
            to
        };
        tokio::select! {
            biased;
            _ = generation.disconnected.changed() => bail!("IB session lost during historical recovery"),
            data = backfill_range(&history, id, from, cutoff) => {
                let bars = data?;
                tracing::info!(instrument=id, from_exclusive=from, to_inclusive=cutoff, bars=bars.len(), "IB historical recovery");
                Ok(bars)
            }
        }
    }
    async fn next_bar(&mut self) -> Result<Option<Bar>> {
        let gen_ = self.generation.as_mut().context("no paper generation")?;
        if let Some(done) = gen_.recovered.take() {
            done.send(())
                .map_err(|_| anyhow!("IB session lost during backfill"))?;
        }
        let item = tokio::select! {
            biased;
            _ = gen_.disconnected.changed() => bail!("IB session lost; repair from durable watermarks"),
            item = gen_.bars.recv() => item,
        };
        match item {
            Some(item) => item.map(Some),
            None => bail!("IB session invalidated; resubscribe and repair from durable watermarks"),
        }
    }
    fn status(&self) -> SourceStatus {
        SourceStatus {
            connected: self.generation.is_some(),
            mode: if self.delayed {
                "ib-delayed"
            } else {
                "ib-realtime"
            }
            .into(),
            detail: "paper; one-minute TRADES".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    fn input(start_minute: i64, close: f64) -> historical::Bar {
        historical::Bar {
            date: BarTimestamp::DateTime(
                OffsetDateTime::from_unix_timestamp(start_minute * 60).unwrap(),
            ),
            open: 123.1,
            high: 124.0,
            low: 122.88,
            close,
            volume: 10.125,
            wap: 123.7,
            count: 3,
        }
    }
    fn bar(start: i64) -> Bar {
        closed("SPY.ARCA", input(start, 123.98)).unwrap()
    }
    #[test]
    fn updates_finalize_once_after_snapshot_end_in_decimal() {
        let mut current = None;
        let id = "SPY.ARCA";
        let msg = |t, v| HistoricalBarUpdate::Update(input(t, v));
        assert_eq!(
            market_update(id, &mut current, msg(0, 123.1)).unwrap(),
            None
        );
        let marker = HistoricalBarUpdate::End {
            start: OffsetDateTime::UNIX_EPOCH,
            end: OffsetDateTime::UNIX_EPOCH,
        };
        assert_eq!(market_update(id, &mut current, marker).unwrap(), None);
        assert_eq!(
            market_update(id, &mut current, msg(0, 123.98)).unwrap(),
            None
        );
        let complete = market_update(id, &mut current, msg(1, 124.0))
            .unwrap()
            .unwrap();
        assert_eq!(complete, bar(0));
        assert_eq!(
            (
                complete.ts_ns,
                complete.close.as_str(),
                complete.volume.as_str()
            ),
            (MINUTE, "123.98", "10.125")
        );
        assert!(market_update(id, &mut current, msg(0, 123.98)).is_err());
        assert!(decimal_text(f64::NAN).is_err());
    }

    #[derive(Default)]
    struct OverlappingHistory(StdMutex<Vec<(i64, i64)>>);
    #[async_trait]
    impl History for OverlappingHistory {
        async fn fetch(&self, _: &str, from: i64, to: i64) -> Result<Vec<Bar>> {
            self.0.lock().unwrap().push((from, to));
            // HMDS may return out-of-range bars and repeat boundary bars.
            let mut rows = Vec::new();
            for i in (from / MINUTE - 1)..=(to / MINUTE) {
                rows.extend([bar(i), bar(i)]);
            }
            rows.reverse();
            Ok(rows)
        }
    }
    #[tokio::test]
    async fn resumed_watermark_chunks_deduplicate_without_a_gap() {
        let history = OverlappingHistory::default();
        let durable = 2 * MINUTE;
        let end = durable + 2 * CHUNK + MINUTE;
        let rows = backfill_range(&history, "SPY.ARCA", durable, end)
            .await
            .unwrap();
        assert_eq!(
            rows.iter().map(|b| b.ts_ns).collect::<Vec<_>>(),
            ((durable / MINUTE + 1)..=end / MINUTE)
                .map(|t| t * MINUTE)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            *history.0.lock().unwrap(),
            vec![
                (durable, durable + CHUNK),
                (durable + CHUNK, durable + 2 * CHUNK),
                (durable + 2 * CHUNK, end)
            ]
        );
        let mut batch = Batch::new();
        insert_bar(&mut batch, bar(3), durable, end).unwrap();
        let mut conflict = bar(3);
        conflict.close = "0.01".into();
        assert!(insert_bar(&mut batch, conflict, durable, end).is_err());
    }
    fn notice(code: i32, text: &str) -> ibapi::Error {
        ibapi::Error::Notice(ibapi::Notice {
            code,
            message: text.into(),
            error_time: None,
            advanced_order_reject_json: String::new(),
        })
    }
    #[tokio::test(start_paused = true)]
    async fn pacing_retries_back_off_across_instruments_and_cap_attempts() {
        let pacer = Pacer::new();
        let started = Instant::now();
        let times = StdMutex::new(Vec::new());
        let retry = || {
            let mut calls = times.lock().unwrap();
            calls.push(started.elapsed().as_secs());
            std::future::ready(if calls.len() <= 2 {
                Err(notice(162, "Historical pacing violation"))
            } else {
                Ok(7)
            })
        };
        assert_eq!(paced(&pacer, retry).await.unwrap(), 7);
        assert_eq!(*times.lock().unwrap(), vec![0, 16, 48]);
        let sibling = pacer.clone();
        assert!(
            paced(&sibling, || async {
                Err::<(), _>(notice(354, "market data not subscribed"))
            })
            .await
            .is_err()
        );
        assert_eq!(started.elapsed().as_secs(), 64);
        let mut attempts = 0;
        assert!(
            paced(&pacer, || {
                attempts += 1;
                std::future::ready(Err::<(), _>(notice(420, "pacing")))
            })
            .await
            .is_err()
        );
        assert_eq!(attempts, 4);
    }
}
