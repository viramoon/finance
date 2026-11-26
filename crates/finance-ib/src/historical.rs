//! Bounded historical stock minute requests via a verified, read-only paper session.
mod pacing;

use crate::{ConnectionManager, Error, IbTransport, PaperPeer, Result, Session, Transport};
use async_trait::async_trait;
use ibapi::{
    contracts::Contract,
    market_data::{TradingHours, historical},
};
use pacing::Pacer;
use rust_decimal::Decimal;
use serde::Serialize;
use std::{collections::BTreeMap, path::Path, time::Duration};
use time::OffsetDateTime;

const MINUTE: i64 = 60;
const NANOS: i64 = 1_000_000_000;
const DAY: i64 = 24 * 60 * MINUTE;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(45);

pub struct MinuteRequest {
    symbol: String,
    venue: &'static str,
    primary: &'static str,
    start: i64,
    end: i64,
}

impl MinuteRequest {
    pub fn parse(symbol: &str, start: &str, end: &str) -> Result<Self> {
        let parse = |value: &str| {
            value
                .parse::<i64>()
                .map_err(|_| Error::Config("historical bounds must be UTC epoch seconds".into()))
        };
        let (start, end) = (parse(start)?, parse(end)?);
        if start < 0
            || end <= start
            || end - start > DAY
            || start % MINUTE != 0
            || end % MINUTE != 0
            || end > OffsetDateTime::now_utc().unix_timestamp()
            || end > i64::MAX / NANOS
        {
            return Err(Error::Config(
                "historical range must be closed, minute-aligned, and at most one day".into(),
            ));
        }
        let (venue, primary) = match symbol {
            "SPY" => ("ARCA", "ARCA"),
            "QQQ" | "AAPL" | "MSFT" | "NVDA" | "AMZN" | "GOOGL" | "META" | "TSLA" => {
                ("XNAS", "NASDAQ")
            }
            _ => return Err(Error::Config("unsupported historical stock symbol".into())),
        };
        Ok(Self {
            symbol: symbol.into(),
            venue,
            primary,
            start,
            end,
        })
    }

    fn contract(&self) -> Contract {
        Contract::stock(&self.symbol)
            .primary(self.primary)
            .in_currency("USD")
            .build()
    }
}

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct MinuteBar {
    pub instrument: String,
    pub ts_ns: i64,
    pub open: String,
    pub high: String,
    pub low: String,
    pub close: String,
    pub volume: String,
}

impl ConnectionManager<IbTransport> {
    /// The probe client role is reserved for short, read-only paper requests.
    pub async fn historical_minute(
        &self,
        request: MinuteRequest,
        root: &Path,
    ) -> Result<Vec<MinuteBar>> {
        self.fetch_paper_minute(request, root).await
    }
}

impl<T: Transport> ConnectionManager<T> {
    async fn fetch_paper_minute(
        &self,
        request: MinuteRequest,
        root: &Path,
    ) -> Result<Vec<MinuteBar>>
    where
        T::S: MinuteSession,
    {
        let pacing = Pacer::acquire(root).await?;
        pacing.wait().await?;
        let (paper, _) = self.open(self.config.probe_client_id).await?;
        let raw_bars = async {
            pacing.begin()?;
            tokio::select! {
                biased;
                error = paper.disconnected() => Err(error),
                result = tokio::time::timeout(REQUEST_TIMEOUT, paper.minute_bars(&request)) => {
                    result.map_err(|_| Error::Timeout("historical-minute"))?
                }
            }
        }
        .await;
        paper.close().await;
        pacing.finish()?;
        closed_bars(&request, raw_bars?)
    }
}

#[async_trait]
trait MinuteSession: Session {
    async fn minute_bars(&self, request: &MinuteRequest) -> Result<Vec<historical::Bar>>;
}

#[async_trait]
impl MinuteSession for PaperPeer {
    async fn minute_bars(&self, request: &MinuteRequest) -> Result<Vec<historical::Bar>> {
        let end = OffsetDateTime::from_unix_timestamp(request.end)
            .map_err(|_| Error::Config("historical end timestamp out of range".into()))?;
        let contract = request.contract();
        // IB supports one-minute bars with a 1 D duration. Responses for shorter
        // caller windows are clipped by close time in closed_bars.
        let response = self
            .client
            .historical_data(&contract, historical::BarSize::Min)
            .what_to_show(historical::WhatToShow::Trades)
            .trading_hours(TradingHours::Regular)
            .ending(end)
            .duration(historical::Duration::days(1))
            .fetch()
            .await;
        read_bars(response)
    }
}

fn read_bars(
    response: std::result::Result<historical::HistoricalData, ibapi::Error>,
) -> Result<Vec<historical::Bar>> {
    match response {
        Ok(response)
            if response.start == OffsetDateTime::UNIX_EPOCH
                || response.end == OffsetDateTime::UNIX_EPOCH
                || response.end < response.start =>
        {
            Err(Error::Gateway(
                "historical-minute response has an invalid or missing end marker".into(),
            ))
        }
        Ok(response) => Ok(response.bars),
        Err(ibapi::Error::Notice(notice))
            if notice.code == 162
                && notice
                    .message
                    .to_ascii_lowercase()
                    .contains("query returned no data") =>
        {
            Ok(Vec::new())
        }
        Err(ibapi::Error::Notice(notice)) => Err(Error::Gateway(format!(
            "historical-minute notice {}",
            notice.code
        ))),
        Err(error) => Err(Error::Gateway(format!("historical-minute: {error}"))),
    }
}

fn closed_bars(request: &MinuteRequest, raw: Vec<historical::Bar>) -> Result<Vec<MinuteBar>> {
    let instrument = format!("{}.{}", request.symbol, request.venue);
    let mut bars = BTreeMap::new();
    for raw in raw {
        let historical::BarTimestamp::DateTime(start) = raw.date else {
            return Err(Error::Gateway("expected intraday IB bar timestamp".into()));
        };
        let start = start.unix_timestamp();
        if start <= 0 || start % MINUTE != 0 {
            return Err(Error::Gateway("invalid IB minute timestamp".into()));
        }
        let end = start + MINUTE;
        if end <= request.start || end > request.end {
            continue;
        }
        let open = decoded_decimal(raw.open)?;
        let high = decoded_decimal(raw.high)?;
        let low = decoded_decimal(raw.low)?;
        let close = decoded_decimal(raw.close)?;
        let volume = decoded_decimal(raw.volume)?;
        if low <= Decimal::ZERO
            || low > open.min(close)
            || high < open.max(close)
            || volume < Decimal::ZERO
            || volume >= Decimal::from(1u64 << 53)
        {
            return Err(Error::Gateway("invalid IB minute OHLCV values".into()));
        }
        let bar = MinuteBar {
            instrument: instrument.clone(),
            ts_ns: end * NANOS,
            open: open.to_string(),
            high: high.to_string(),
            low: low.to_string(),
            close: close.to_string(),
            volume: volume.to_string(),
        };
        if bars.get(&end).is_some_and(|existing| existing != &bar) {
            return Err(Error::Gateway("conflicting IB minute bars".into()));
        }
        bars.insert(end, bar);
    }
    Ok(bars.into_values().collect())
}

// ibapi 3.2.0 exposes OHLC and volume as f64, including the volume which
// is decimal text on the wire. Use the shortest round-trip representation
// at this boundary and perform all validation and output in decimal.
fn decoded_decimal(value: f64) -> Result<Decimal> {
    let decimal = Decimal::from_str_exact(&value.to_string())
        .map_err(|_| Error::Gateway("unrepresentable IB minute decimal".into()))?;
    if decimal.to_string().parse::<f64>().ok() != Some(value) {
        return Err(Error::Gateway("IB minute decimal did not roundtrip".into()));
    }
    Ok(decimal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::IbConfig;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    // Synthetic minute timestamps and decimal-like prices, with no data source.
    const START: i64 = 1_735_917_000;
    const END: i64 = START + 2 * MINUTE;

    fn request(symbol: &str) -> MinuteRequest {
        MinuteRequest::parse(symbol, &START.to_string(), &END.to_string()).unwrap()
    }

    fn bar(start: i64) -> historical::Bar {
        historical::Bar {
            date: OffsetDateTime::from_unix_timestamp(start).unwrap().into(),
            open: 13.12,
            high: 14.24,
            low: 12.36,
            close: 13.47,
            volume: 1000.25,
            wap: 0.0,
            count: 12,
        }
    }

    #[test]
    fn nine_symbols_and_bounded_closed_range() {
        for symbol in [
            "QQQ", "SPY", "AAPL", "MSFT", "NVDA", "AMZN", "GOOGL", "META", "TSLA",
        ] {
            let request = request(symbol);
            let contract = request.contract();
            assert_eq!(contract.symbol.to_string(), symbol);
            let expected_exchange = if symbol == "SPY" { "ARCA" } else { "NASDAQ" };
            assert_eq!(contract.primary_exchange.to_string(), expected_exchange);
            assert_eq!(contract.exchange.to_string(), "SMART");
        }
        for (symbol, start, end) in [
            ("IBM", START, END),
            ("SPY", START + 1, END),
            ("SPY", START, START),
            ("SPY", START, START + DAY + MINUTE),
            ("SPY", -60, 60),
            ("SPY", 7_999_999_980, 8_000_000_040),
        ] {
            assert!(MinuteRequest::parse(symbol, &start.to_string(), &end.to_string()).is_err());
        }
    }

    #[test]
    fn closed_bar_payload_clips_orders_and_deduplicates() {
        let request = request("SPY");
        let bars = closed_bars(
            &request,
            vec![
                bar(END),
                bar(END - MINUTE),
                bar(START),
                bar(START - MINUTE),
                bar(START),
            ],
        )
        .unwrap();
        assert_eq!(bars.len(), 2);
        assert_eq!(bars[0].ts_ns, (START + MINUTE) * NANOS);
        assert_eq!(bars[1].ts_ns, END * NANOS);
        assert_eq!(
            serde_json::to_value(&bars[0]).unwrap(),
            serde_json::json!({
                "instrument": "SPY.ARCA", "ts_ns": (START + MINUTE) * NANOS,
                "open": "13.12", "high": "14.24", "low": "12.36", "close": "13.47",
                "volume": "1000.25",
            })
        );
        let mut conflicting = bar(START);
        conflicting.close = 13.48;
        assert!(closed_bars(&request, vec![bar(START), conflicting]).is_err());
        assert!(closed_bars(&request, vec![bar(START + 1)]).is_err());
        for volume in [f64::NAN, f64::INFINITY, -1.0, 9_007_199_254_740_992.0] {
            let mut invalid = bar(START);
            invalid.volume = volume;
            assert!(closed_bars(&request, vec![invalid]).is_err());
        }
    }

    #[test]
    fn distinguishes_confirmed_empty_history_from_incomplete_or_denied_response() {
        let notice = |code, message: &str| {
            Err(ibapi::Error::Notice(ibapi::Notice {
                code,
                message: message.into(),
                error_time: None,
                advanced_order_reject_json: String::new(),
            }))
        };
        assert!(
            read_bars(notice(162, "HMDS query returned no data"))
                .unwrap()
                .is_empty()
        );
        assert!(read_bars(notice(162, "No data permissions")).is_err());
        assert!(read_bars(notice(162, "Historical pacing violation")).is_err());
        assert!(
            read_bars(Ok(historical::HistoricalData {
                bars: vec![bar(START)],
                start: OffsetDateTime::UNIX_EPOCH,
                end: OffsetDateTime::UNIX_EPOCH,
            }))
            .is_err()
        );
    }

    #[derive(Clone)]
    struct MockSession {
        account: &'static str,
        sent: Arc<AtomicUsize>,
        closed: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Transport for MockSession {
        type S = Self;

        async fn connect(&self, _: &str, id: i32) -> Result<Self> {
            assert_eq!(id, 91);
            Ok(self.clone())
        }
    }

    #[async_trait]
    impl Session for MockSession {
        async fn accounts(&self) -> Result<Vec<String>> {
            Ok(vec![self.account.into()])
        }

        async fn server_time(&self) -> Result<i64> {
            unreachable!("historical requests do not need a server-time round trip")
        }

        async fn close(&self) {
            self.closed.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl MinuteSession for MockSession {
        async fn minute_bars(&self, request: &MinuteRequest) -> Result<Vec<historical::Bar>> {
            self.sent.fetch_add(1, Ordering::SeqCst);
            Ok(vec![bar(request.start)])
        }
    }

    #[tokio::test]
    async fn history_only_sends_after_paper_account_verification() {
        let root = tempfile::tempdir().unwrap();
        let config_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config");
        let config = IbConfig::from_layers(&config_dir, "cloudbox", |_| None).unwrap();
        let sent = Arc::new(AtomicUsize::new(0));
        let closed = Arc::new(AtomicUsize::new(0));
        let mock = MockSession {
            account: "U-SYNTHETIC",
            sent: sent.clone(),
            closed: closed.clone(),
        };
        let manager = ConnectionManager::new(config.clone(), mock.clone()).unwrap();
        assert!(matches!(
            manager
                .fetch_paper_minute(request("SPY"), root.path())
                .await,
            Err(Error::UnsafeAccount)
        ));
        assert_eq!(sent.load(Ordering::SeqCst), 0);
        assert_eq!(closed.load(Ordering::SeqCst), 1);

        let manager = ConnectionManager::new(
            config,
            MockSession {
                account: "DU-SYNTHETIC",
                ..mock
            },
        )
        .unwrap();
        let bars = manager
            .fetch_paper_minute(request("AAPL"), root.path())
            .await
            .unwrap();
        assert_eq!(bars.len(), 1);
        assert_eq!(bars[0].instrument, "AAPL.XNAS");
        assert_eq!(sent.load(Ordering::SeqCst), 1);
        assert_eq!(closed.load(Ordering::SeqCst), 2);
    }
}
