use std::{collections::VecDeque, future::pending, path::Path, time::Duration};

use finance_market_protocol::{Event, Message, SchemaVersion, TradeSide};
use serde_json::{Value, json};
use tokio::{sync::watch, time::Instant};

use super::{
    Config, Observer, Status,
    runner::{Session, run_with},
    transport::Frame,
    wire::{CONTEXT_PRODUCT, PERP_PRODUCT, SOURCE, SPOT_PRODUCT},
};
use crate::{
    market_ingress::{GapCause, RecoveryOutcome, read_quality, wal_root},
    market_wal::WalReader,
};

const ACCESS: &str = "
  license: fixture.synthetic
  display_live: true
  record_local: true
  derive_non_display: false
  backup_personal_drive: false
";

fn config(spot: bool, perp: bool) -> Config {
    let mut yaml = String::from("enabled: true\n");
    if spot {
        yaml.push_str("spot:");
        yaml.push_str(ACCESS);
    }
    if perp {
        yaml.push_str("perpetual:\n  symbols: [BTCUSDT]\n");
        for name in ["market", "context"] {
            yaml.push_str(&format!("  {name}:\n"));
            for line in ACCESS.lines().filter(|line| !line.is_empty()) {
                yaml.push_str(&format!("  {line}\n"));
            }
        }
    }
    serde_yaml::from_str(&yaml).unwrap()
}

#[derive(Default)]
struct Capture {
    statuses: Vec<Status>,
    events: Vec<Event>,
}

impl Observer for Capture {
    fn status(&mut self, status: Status) {
        self.statuses.push(status);
    }
    fn display(&mut self, event: &Event) {
        self.events.push(event.clone());
    }
}

struct Replay(VecDeque<Frame>);

impl Replay {
    fn from(data: Vec<Value>) -> Self {
        Self(
            data.into_iter()
                .map(|value| serde_json::from_value(value).unwrap())
                .collect(),
        )
    }
}

impl Session for Replay {
    async fn next(&mut self) -> anyhow::Result<Frame> {
        match self.0.pop_front() {
            Some(frame) => Ok(frame),
            None => pending().await,
        }
    }
    async fn close(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

fn metadata(market: &str) -> Value {
    let symbols = if market == "spot" {
        vec!["BTC", "ETH", "SOL"]
    } else {
        vec!["BTC"]
    };
    json!({"type": "metadata", "market": market, "data": {"symbols": symbols.into_iter().map(|base| json!({
        "symbol": format!("{base}USDT"), "baseAsset": base, "quoteAsset": "USDT", "status": "TRADING",
        "contractType": "PERPETUAL", "marginAsset": "USDT",
        "filters": [{"filterType":"PRICE_FILTER","tickSize":"0.01000000"},{"filterType":"LOT_SIZE","stepSize":"0.00100000"}]
    })).collect::<Vec<_>>()}})
}

fn frame(name: &str, market: &str, data: Value) -> Value {
    json!({"type": name, "market": market, "symbol": "BTCUSDT", "received": 1_500_000_000u64, "data": data})
}

fn failure(code: u16, retry_after: Option<u64>) -> Value {
    json!({"type": "failure", "status": code, "retry_after": retry_after})
}

fn events(root: &Path, product: &str) -> Vec<Event> {
    let reader = WalReader::open(&wal_root(root), SOURCE, product).unwrap();
    reader
        .read_after(0, 128)
        .unwrap()
        .into_iter()
        .map(|row| row.event)
        .collect()
}

#[tokio::test]
async fn omitted_or_disabled_profile_never_starts_network_or_opens_wal() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("absent.yaml");
    let cfg = Config::load(&path).unwrap();
    let root = tmp.path().join("absent-data");
    let (_shutdown, stop) = watch::channel(false);
    let mut observer = Capture::default();
    for cfg in [
        cfg,
        serde_yaml::from_str("{}").unwrap(),
        serde_yaml::from_str("enabled: false").unwrap(),
    ] {
        let result = run_with::<Replay>(&cfg, &root, stop.clone(), &mut observer, |_| {
            panic!("transport must not be started")
        })
        .await
        .unwrap();
        assert_eq!(result, Status::DisabledByProfile);
    }
    assert_eq!(observer.statuses, vec![Status::DisabledByProfile; 3]);
    assert!(!root.exists());
}

#[tokio::test(start_paused = true)]
async fn region_rejection_stops_all_retries_and_rate_limit_honors_delay() {
    let mut cfg = config(true, false);
    cfg.spot.as_mut().unwrap().record_local = false;
    for code in [403, 451] {
        let start = Instant::now();
        let mut attempts = 0;
        let (_shutdown, stop) = watch::channel(false);
        let mut observer = Capture::default();
        let result = run_with(&cfg, Path::new("/unused"), stop, &mut observer, |_| {
            attempts += 1;
            let response = if attempts == 1 {
                failure(429, Some(90))
            } else {
                assert_eq!(
                    Instant::now().duration_since(start),
                    Duration::from_secs(90)
                );
                failure(code, None)
            };
            Ok(Replay::from(vec![response]))
        })
        .await
        .unwrap();
        assert_eq!(attempts, 2);
        assert_eq!(result, Status::RegionRestricted { http_status: code });
        assert_eq!(
            observer.statuses,
            [
                Status::Connecting,
                Status::RateLimited { http_status: 429 },
                Status::Connecting,
                Status::RegionRestricted { http_status: code },
            ]
        );
    }
}

#[tokio::test(start_paused = true)]
async fn depth_gap_resets_stream_and_new_snapshot_reports_recovery_to_wal() {
    assert_depth_gap_recovery(None).await;
}

#[tokio::test(start_paused = true)]
async fn depth_gap_with_invalid_quantity_still_resets_and_recovers() {
    assert_depth_gap_recovery(Some("-1.000")).await;
}

async fn assert_depth_gap_recovery(invalid_quantity: Option<&str>) {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = config(true, false);
    let depth: Value =
        serde_json::from_str(include_str!("../../fixtures/binance/depth.json")).unwrap();
    let (_shutdown, stop) = watch::channel(false);
    let mut observer = Capture::default();
    let mut first_gap = depth["spot_gap"].clone();
    if let Some(quantity) = invalid_quantity {
        first_gap["b"][0][1] = json!(quantity);
    }
    let mut sessions = VecDeque::from([
        Replay::from(vec![
            metadata("spot"),
            frame("stream", "spot", depth["spot_bridge"].clone()),
            frame("snapshot", "spot", depth["spot_snapshot"].clone()),
            frame("stream", "spot", depth["spot_next"].clone()),
            frame("stream", "spot", first_gap),
        ]),
        Replay::from(vec![
            metadata("spot"),
            frame("stream", "spot", depth["spot_gap"].clone()),
            frame("snapshot", "spot", depth["spot_recovery"].clone()),
            failure(451, None),
        ]),
    ]);
    let result = run_with(&cfg, tmp.path(), stop, &mut observer, |_| {
        Ok(sessions.pop_front().unwrap())
    })
    .await
    .unwrap();
    assert_eq!(result, Status::RegionRestricted { http_status: 451 });
    let captured = events(tmp.path(), SPOT_PRODUCT);
    let btc: Vec<_> = captured
        .iter()
        .filter(|event| {
            event.instrument_id.as_str() == "BTC-USDT.BINANCE" && event.channel.as_str() == "depth"
        })
        .collect();
    let [
        first_reset,
        first_snapshot,
        bridge,
        next,
        gap,
        reset,
        recovered,
        update,
        ..,
    ] = btc.as_slice()
    else {
        panic!("incomplete depth stream: {btc:?}");
    };
    assert!(matches!(first_reset.message, Message::Reset(_)));
    assert!(matches!(first_snapshot.message, Message::Snapshot(_)));
    assert_eq!(
        bridge.native_id.as_ref().unwrap().as_str(),
        "9007199254741001"
    );
    assert_eq!(
        next.native_id.as_ref().unwrap().as_str(),
        "9007199254741003"
    );
    assert!(matches!(gap.message, Message::Gap(_)));
    assert!(matches!(reset.message, Message::Reset(_)));
    assert_eq!(reset.seq.get(), 0);
    assert_eq!(reset.epoch.get(), gap.epoch.get() + 1);
    assert!(matches!(recovered.message, Message::Snapshot(_)));
    assert_eq!(
        update.native_id.as_ref().unwrap().as_str(),
        "9007199254741006"
    );
    for pair in btc.windows(2) {
        pair[1].validate_successor(pair[0]).unwrap();
    }
    assert_eq!(captured, observer.events);
    let quality = read_quality(
        tmp.path(),
        &SOURCE.parse().unwrap(),
        &SPOT_PRODUCT.parse().unwrap(),
    )
    .unwrap();
    assert!(
        quality
            .iter()
            .any(|report| report.cause == GapCause::SourceGap
                && report.outcome == Some(RecoveryOutcome::BookRestoredFromSnapshot)
                && report.count.unwrap().get() == 1)
    );
    assert!(
        quality
            .iter()
            .any(|report| report.cause == GapCause::SourceGap
                && report.outcome == Some(RecoveryOutcome::HistoryUnrecoverable)
                && report.count.is_none())
    );
}

#[tokio::test]
async fn spot_and_perpetual_are_distinct_and_context_uses_its_own_policy() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cfg = config(true, true);
    let perp = cfg.perpetual.as_mut().unwrap();
    perp.market.display_live = false;
    perp.context.license = "fixture.context".parse().unwrap();
    perp.context.record_local = false;
    let data: Value =
        serde_json::from_str(include_str!("../../fixtures/binance/observations.json")).unwrap();
    let (_shutdown, stop) = watch::channel(false);
    let mut observer = Capture::default();
    run_with(&cfg, tmp.path(), stop, &mut observer, |_| {
        Ok(Replay::from(vec![
            metadata("spot"),
            metadata("perpetual"),
            frame("stream", "spot", data["spot_trade"].clone()),
            frame("stream", "perpetual", data["perp_trade"].clone()),
            frame("stream", "perpetual", data["perp_trade"].clone()),
            frame("stream", "perpetual", data["funding"].clone()),
            frame("interest", "perpetual", data["interest"].clone()),
            failure(403, None),
        ]))
    })
    .await
    .unwrap();

    for (product, id, side) in [
        (SPOT_PRODUCT, "BTC-USDT.BINANCE", TradeSide::Sell),
        (PERP_PRODUCT, "BTC-USDT-PERP.BINANCE", TradeSide::Buy),
    ] {
        let trades: Vec<_> = events(tmp.path(), product)
            .into_iter()
            .filter(|event| matches!(event.message, Message::Trade(_)))
            .collect();
        let [event] = trades.as_slice() else {
            panic!("unexpected trades: {trades:?}")
        };
        let Message::Trade(trade) = &event.message else {
            unreachable!()
        };
        assert_eq!(event.instrument_id.as_str(), id);
        assert_eq!(
            event.native_id.as_ref().unwrap().as_str(),
            "9007199254741011"
        );
        assert_eq!(event.exchange_ts.unwrap().get(), 999_000_000);
        assert_eq!(trade.price.as_str(), "10.010");
        assert_eq!(trade.quantity.as_str(), "0.001000");
        assert_eq!(trade.side, side);
    }
    assert!(
        observer
            .events
            .iter()
            .all(|event| event.product.as_str() != PERP_PRODUCT)
    );
    assert!(
        !wal_root(tmp.path())
            .join(SOURCE)
            .join(CONTEXT_PRODUCT)
            .exists()
    );
    let context: Vec<_> = observer
        .events
        .iter()
        .filter(|event| event.product.as_str() == CONTEXT_PRODUCT)
        .collect();
    assert!(context.iter().all(
        |event| event.instrument_id.as_str() == "BTC-USDT-PERP.BINANCE"
            && event.license.as_str() == "fixture.context"
            && event.schema_version == SchemaVersion::V2
    ));
    assert!(
        context
            .iter()
            .any(|event| matches!(&event.message, Message::FundingRate(value)
        if value.rate.as_str() == "-0.00000001" && value.next_funding_ts == Some(2_000_000_000.into())))
    );
    assert!(context.iter().any(
        |event| matches!(&event.message, Message::OpenInterest(value)
        if value.quantity.as_str() == "123.456789123456")
    ));
}

#[tokio::test]
async fn context_records_exact_values_in_f3_and_wal_restarts_advance_epoch() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = config(false, true);
    let data: Value =
        serde_json::from_str(include_str!("../../fixtures/binance/observations.json")).unwrap();
    for _ in 0..2 {
        let (_shutdown, stop) = watch::channel(false);
        run_with(&cfg, tmp.path(), stop, &mut Capture::default(), |_| {
            Ok(Replay::from(vec![
                metadata("perpetual"),
                frame("stream", "perpetual", data["funding"].clone()),
                frame("interest", "perpetual", data["interest"].clone()),
                failure(451, None),
            ]))
        })
        .await
        .unwrap();
    }
    let stored = events(tmp.path(), CONTEXT_PRODUCT);
    for (channel, expected) in [
        ("funding_rate", "-0.00000001"),
        ("open_interest", "123.456789123456"),
    ] {
        let stream: Vec<_> = stored
            .iter()
            .filter(|event| event.channel.as_str() == channel)
            .collect();
        for pair in stream.windows(2) {
            pair[1].validate_successor(pair[0]).unwrap();
        }
        let observations: Vec<_> = stream
            .iter()
            .filter_map(|event| match &event.message {
                Message::FundingRate(value) => Some(value.rate.as_str()),
                Message::OpenInterest(value) => Some(value.quantity.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(observations, [expected; 2]);
        assert!(stream[4].epoch > stream[3].epoch);
    }
}

#[tokio::test]
async fn missing_next_funding_time_publishes_rate_and_keeps_perpetual_feed_running() {
    let cfg = config(false, true);
    let data: Value =
        serde_json::from_str(include_str!("../../fixtures/binance/observations.json")).unwrap();
    for missing_time in [Some(0), Some(-1), None] {
        let tmp = tempfile::tempdir().unwrap();
        let mut funding = data["funding"].clone();
        match missing_time {
            Some(time) => funding["T"] = json!(time),
            None => {
                funding.as_object_mut().unwrap().remove("T");
            }
        }
        let (_shutdown, stop) = watch::channel(false);
        let mut observer = Capture::default();
        let end = run_with(&cfg, tmp.path(), stop, &mut observer, |_| {
            Ok(Replay::from(vec![
                metadata("perpetual"),
                frame("stream", "perpetual", funding.clone()),
                frame("stream", "perpetual", data["perp_trade"].clone()),
                frame("interest", "perpetual", data["interest"].clone()),
                frame("stream", "perpetual", data["funding"].clone()),
                failure(451, None),
            ]))
        })
        .await
        .unwrap();
        assert_eq!(end, Status::RegionRestricted { http_status: 451 });
        assert!(!observer.statuses.contains(&Status::Recovering));

        let context = events(tmp.path(), CONTEXT_PRODUCT);
        let funding: Vec<_> = context
            .iter()
            .filter_map(|event| match &event.message {
                Message::FundingRate(rate) => Some(serde_json::to_value(rate).unwrap()),
                _ => None,
            })
            .collect();
        assert_eq!(
            funding,
            [
                json!({"rate": "-0.00000001", "next_funding_ts": null}),
                json!({"rate": "-0.00000001", "next_funding_ts": "2000000000"}),
            ]
        );
        assert!(
            context
                .iter()
                .any(|event| matches!(event.message, Message::OpenInterest(_)))
        );
        assert!(
            events(tmp.path(), PERP_PRODUCT)
                .iter()
                .any(|event| matches!(event.message, Message::Trade(_)))
        );
    }
}

#[tokio::test(start_paused = true)]
async fn continuous_depth_with_invalid_tick_or_lot_resets_and_restores_book() {
    let cfg = config(true, false);
    let depth: Value =
        serde_json::from_str(include_str!("../../fixtures/binance/depth.json")).unwrap();
    let off_tick_price = "10.015";
    let off_lot_quantity = "1.0005";
    for (field, invalid, initialized) in [
        (0, off_tick_price, true),
        (1, off_lot_quantity, true),
        (0, off_tick_price, false),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let (_shutdown, stop) = watch::channel(false);
        let mut observer = Capture::default();
        let mut next = depth["spot_next"].clone();
        next["b"][0][field] = json!(invalid);
        let mut first = vec![
            metadata("spot"),
            frame("stream", "spot", depth["spot_bridge"].clone()),
        ];
        let snapshot = frame("snapshot", "spot", depth["spot_snapshot"].clone());
        let invalid_delta = frame("stream", "spot", next);
        first.extend(if initialized {
            [snapshot, invalid_delta]
        } else {
            [invalid_delta, snapshot]
        });
        let mut sessions = VecDeque::from([
            Replay::from(first),
            Replay::from(vec![
                metadata("spot"),
                frame("stream", "spot", depth["spot_gap"].clone()),
                frame("snapshot", "spot", depth["spot_recovery"].clone()),
                failure(451, None),
            ]),
        ]);
        let end = run_with(&cfg, tmp.path(), stop, &mut observer, |_| {
            Ok(sessions.pop_front().unwrap())
        })
        .await
        .unwrap();
        assert_eq!(end, Status::RegionRestricted { http_status: 451 });
        assert!(sessions.is_empty());

        let captured = events(tmp.path(), SPOT_PRODUCT);
        let btc: Vec<_> = captured
            .iter()
            .filter(|event| {
                event.instrument_id.as_str() == "BTC-USDT.BINANCE"
                    && event.channel.as_str() == "depth"
            })
            .collect();
        assert_eq!(captured, observer.events);
        assert!(btc.iter().all(|event| {
            event
                .native_id
                .as_ref()
                .is_none_or(|id| id.as_str() != "9007199254741003")
        }));
        let gap_index = usize::from(initialized) * 2 + 1;
        let [gap, reset, snapshot, update, ..] = &btc[gap_index..] else {
            panic!("incomplete recovery: {btc:?}");
        };
        assert!(matches!(&gap.message, Message::Gap(details)
            if details.reason.as_str() == "binance_depth_invalid_increment"));
        assert!(matches!(&reset.message, Message::Reset(details)
            if details.reason.as_str() == "binance_depth_invalid_increment"));
        assert_eq!(reset.epoch.get(), gap.epoch.get() + 1);
        assert!(matches!(snapshot.message, Message::Snapshot(_)));
        assert_eq!(
            snapshot.native_id.as_ref().unwrap().as_str(),
            "9007199254741005"
        );
        assert!(matches!(update.message, Message::Delta(_)));
        for pair in btc.windows(2) {
            pair[1].validate_successor(pair[0]).unwrap();
        }
        let quality = read_quality(
            tmp.path(),
            &SOURCE.parse().unwrap(),
            &SPOT_PRODUCT.parse().unwrap(),
        )
        .unwrap();
        assert!(quality.iter().any(|report| {
            report.cause == GapCause::SourceGap
                && report.outcome == Some(RecoveryOutcome::BookRestoredFromSnapshot)
        }));
    }
}
