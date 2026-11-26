use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use finance_collector::{
    hyperliquid::{Config, Hyperliquid, PRODUCT, RejectedFrame, SOURCE, TransportEvent},
    market_ingress::{GapCause, RecoveryOutcome, read_quality, wal_root},
    market_wal::WalReader,
};
use finance_market_protocol::{Event, Message, SchemaVersion, TradeSide};
use rust_decimal::Decimal;
use serde_json::{Value, json};

const META: &str = include_str!("../../../tests/fixtures/hyperliquid/meta.json");
const INITIAL: &str = include_str!("../../../tests/fixtures/hyperliquid/ws-initial.jsonl");
const RECONNECTED: &str = include_str!("../../../tests/fixtures/hyperliquid/ws-reconnected.jsonl");
const BASE: u64 = 1_700_000_001_000_000_000;

fn config(record_local: bool) -> Value {
    json!({
        "coins": ["BTC", "ETH", "SOL"],
        "policy": {
            "source": SOURCE, "product": PRODUCT, "license": "synthetic",
            "display_live": !record_local, "record_local": record_local,
            "derive_non_display": false, "backup_personal_drive": false
        }
    })
}

fn control(event: &str, ns: u64) -> Value {
    json!({"event": event, "receive_ts": (BASE + ns).to_string()})
}

fn payload(event: &str, ns: u64, raw: &str) -> Value {
    let mut entry = control(event, ns);
    entry["data"] = json!(raw);
    entry
}

fn frames(input: &'static str, offset: u64) -> impl Iterator<Item = Value> {
    input
        .lines()
        .zip(offset..)
        .map(|(frame, ns)| payload("frame", ns, frame))
}

fn replay(root: &Path, commands: impl IntoIterator<Item = Value>) {
    let settings = root.join("hyperliquid.json");
    let input = root.join("transport.jsonl");
    fs::write(&settings, config(true).to_string()).unwrap();
    write_transport(&input, commands);
    let result = Command::new(env!("CARGO_BIN_EXE_finance-hyperliquid"))
        .arg(settings)
        .arg("--replay")
        .arg(input)
        .env_clear()
        .env("FERTILE_DATA_ROOT", root)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(result.stdout.is_empty());
}

fn write_transport(path: &Path, commands: impl IntoIterator<Item = Value>) {
    fs::write(
        path,
        commands
            .into_iter()
            .map(|value| format!("{value}\n"))
            .collect::<String>(),
    )
    .unwrap();
}

fn old_btc_book(ns: u64) -> Value {
    payload("frame", ns, INITIAL.lines().next().unwrap())
}

fn events(root: &Path) -> Vec<Event> {
    WalReader::open(&wal_root(root), SOURCE, PRODUCT)
        .unwrap()
        .read_after(0, 200)
        .unwrap()
        .into_iter()
        .map(|row| row.event)
        .collect()
}

fn assert_successors(events: &[Event]) {
    let mut latest = BTreeMap::new();
    for event in events {
        let key = (event.instrument_id.as_str(), event.channel.as_str());
        if let Some(previous) = latest.insert(key, event) {
            event.validate_successor(previous).unwrap();
        }
    }
    assert_eq!(latest.len(), 9);
}

#[test]
fn replayed_three_coins_restore_snapshot_and_report_unknown_history() {
    let temp = tempfile::tempdir().unwrap();
    replay(
        temp.path(),
        [payload("metadata", 0, META), control("connected", 1)]
            .into_iter()
            .chain(frames(INITIAL, 10))
            .chain([control("disconnected", 100), control("connected", 300)])
            .chain([old_btc_book(309)])
            .chain(frames(RECONNECTED, 310)),
    );

    let published = events(temp.path());
    assert_successors(&published);
    let reports = read_quality(
        temp.path(),
        &SOURCE.parse().unwrap(),
        &PRODUCT.parse().unwrap(),
    )
    .unwrap();
    let expected_books = [
        ("BTC", &[("123.4", "0.00003")][..], &[][..]),
        ("ETH", &[("54.12", "0.1001")], &[("54.13", "0.0001")]),
        ("SOL", &[], &[("9.1237", "0.07")]),
    ];
    for (index, (coin, bids, asks)) in expected_books.into_iter().enumerate() {
        let id = format!("{coin}-USD-PERP.HYPERLIQUID");
        let book: Vec<_> = published
            .iter()
            .filter(|event| event.instrument_id.as_str() == id && event.channel.as_str() == "book")
            .collect();
        assert!(matches!(&book[0].message, Message::Reset(_)));
        assert!(matches!(&book[1].message, Message::Snapshot(_)));
        assert!(
            matches!(&book[2].message, Message::Reset(reset) if reset.reason.as_str() == "disconnected")
        );
        let Message::Gap(gap) = &book[3].message else {
            panic!("missing book gap")
        };
        assert_eq!(gap.reason.as_str(), "continuity_unknown");
        assert_eq!(gap.from_ts.get(), BASE + 11 + index as u64);
        assert_eq!(gap.to_ts.get(), BASE + 310 + index as u64);
        let Message::Snapshot(snapshot) = &book[4].message else {
            panic!("missing replacement snapshot")
        };
        assert_eq!(
            book[4].exchange_ts.unwrap().get(),
            1_700_000_000_301_000_000
        );
        for (levels, expected) in [(&snapshot.bids, bids), (&snapshot.asks, asks)] {
            let actual: Vec<_> = levels
                .iter()
                .map(|level| (level.price.as_str(), level.quantity.as_str()))
                .collect();
            assert_eq!(actual, expected, "{coin}");
        }
        assert_eq!(book.len(), 5);
        assert!(book.iter().all(|event| event.native_id.is_none()));
        let id_matches = |report: &&finance_collector::market_ingress::QualityReport| {
            report.instrument_id.as_str() == id && report.channel.as_str() == "book"
        };
        let quality = reports.iter().filter(id_matches).collect::<Vec<_>>();
        assert_eq!(quality.len(), 3);
        assert!(
            quality
                .iter()
                .all(|report| report.cause == GapCause::Disconnected)
        );
        assert!(
            quality
                .iter()
                .any(|report| report.outcome.is_none() && report.to_ts.get() == BASE + 100)
        );
        assert!(quality.iter().any(|report| report.outcome
            == Some(RecoveryOutcome::HistoryUnrecoverable)
            && report.from_ts == gap.from_ts
            && report.to_ts == gap.to_ts
            && report.count.is_none()));
        assert!(quality.iter().any(|report| report.outcome
            == Some(RecoveryOutcome::BookRestoredFromSnapshot)
            && report.to_ts == book[4].receive_ts
            && report.count.unwrap().get() == 1));

        let context = published
            .iter()
            .filter(|event| {
                event.instrument_id.as_str() == id
                    && matches!(event.message, Message::PerpetualContext(_))
            })
            .collect::<Vec<_>>();
        assert_eq!(context.len(), 2);
        assert!(
            context
                .iter()
                .all(|event| event.schema_version == SchemaVersion::V2
                    && event.exchange_ts.is_none()
                    && event.first_available_ts == event.receive_ts)
        );
    }
    let btc_context = published
        .iter()
        .find_map(|event| match &event.message {
            Message::PerpetualContext(ctx) if event.instrument_id.as_str().starts_with("BTC-") => {
                Some(ctx)
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(btc_context.mark_price.as_str(), "123.456789");
    assert_eq!(btc_context.open_interest.as_str(), "7.12345");
    assert_eq!(btc_context.funding_rate.as_str(), "-0.0000123400");
}

#[test]
fn unknown_trades_survive_overlap_and_contribute_to_total_volume() {
    let temp = tempfile::tempdir().unwrap();
    replay(
        temp.path(),
        [payload("metadata", 0, META), control("connected", 1)]
            .into_iter()
            .chain(frames(INITIAL, 10))
            .chain([control("disconnected", 100), control("connected", 300)])
            .chain(frames(RECONNECTED, 310)),
    );
    let published = events(temp.path());
    let mut quantities = [Decimal::ZERO; 3];
    let mut ids = Vec::new();
    for event in published
        .iter()
        .filter(|event| event.instrument_id.as_str().starts_with("BTC-"))
    {
        if let Message::Trade(trade) = &event.message {
            let side = match trade.side {
                TradeSide::Buy => 0,
                TradeSide::Sell => 1,
                TradeSide::Unknown => 2,
            };
            quantities[side] += trade.quantity.value();
            ids.push(event.native_id.as_ref().unwrap().as_str());
            assert!(
                !serde_json::to_string(event)
                    .unwrap()
                    .contains("synthetic-buyer")
            );
        }
    }
    assert_eq!(
        ids,
        [
            "BTC:1700000000002:9007199254740993",
            "BTC:1700000000002:9007199254740994",
            "BTC:1700000000002:9007199254740995",
            "BTC:1700000000302:9007199254740993",
        ]
    );
    assert_eq!((quantities[0] - quantities[1]).to_string(), "0.10000");
    assert_eq!(quantities[2].to_string(), "0.00015");
    assert_eq!(
        quantities.into_iter().sum::<Decimal>().to_string(),
        "0.14705"
    );
    assert_eq!(
        published
            .iter()
            .filter(|event| matches!(event.message, Message::Trade(_)))
            .count(),
        8
    );
}

#[test]
fn reopened_writer_resumes_with_a_new_epoch_and_skips_committed_overlap() {
    let temp = tempfile::tempdir().unwrap();
    replay(
        temp.path(),
        [payload("metadata", 0, META), control("connected", 1)]
            .into_iter()
            .chain(frames(INITIAL, 10)),
    );
    let before = events(temp.path());
    replay(
        temp.path(),
        [payload("metadata", 200, META), control("connected", 300)]
            .into_iter()
            .chain([old_btc_book(309)])
            .chain(frames(RECONNECTED, 310)),
    );
    let after = events(temp.path());
    assert_eq!(&after[..before.len()], &before);
    assert_successors(&after);
    assert_eq!(
        after
            .iter()
            .filter(|event| matches!(event.message, Message::Snapshot(_)))
            .count(),
        6
    );
    assert!(after[before.len()].epoch > before[0].epoch);
    assert_eq!(
        after
            .iter()
            .filter(|event| matches!(event.message, Message::Trade(_)))
            .count(),
        8
    );
    assert_eq!(
        after
            .iter()
            .filter(|event| matches!(event.message, Message::Gap(_)))
            .count(),
        9
    );
}

#[test]
fn display_only_routes_data_and_quality_without_creating_files() {
    let temp = tempfile::tempdir().unwrap();
    let absent = temp.path().join("absent");
    let mut adapter = Hyperliquid::open(
        &absent,
        serde_json::from_value(config(false)).unwrap(),
        META,
        BASE,
    )
    .unwrap();
    let instruments: Vec<_> = adapter
        .instruments()
        .map(|i| {
            (
                i.instrument_id.as_str(),
                i.tick_size.as_str(),
                i.lot_size.as_str(),
                i.quote_currency.as_str(),
                i.settlement_currency.as_ref().unwrap().as_str(),
            )
        })
        .collect();
    assert_eq!(
        instruments,
        [
            ("BTC-USD-PERP.HYPERLIQUID", "0.1", "0.00001", "USD", "USDC"),
            ("ETH-USD-PERP.HYPERLIQUID", "0.01", "0.0001", "USD", "USDC"),
            ("SOL-USD-PERP.HYPERLIQUID", "0.0001", "0.01", "USD", "USDC"),
        ]
    );
    assert!(!adapter.capability().can_backfill);
    for command in [control("connected", 1)]
        .into_iter()
        .chain(frames(INITIAL, 10))
        .chain([control("disconnected", 100), control("connected", 300)])
        .chain(frames(RECONNECTED, 310))
    {
        adapter
            .apply(serde_json::from_value::<TransportEvent>(command).unwrap())
            .unwrap();
    }
    let ingress = adapter.ingress();
    assert_eq!(ingress.flush().unwrap(), None);
    assert_eq!(ingress.watermarks().unwrap().local_durable.get(), 0);
    assert!(
        ingress
            .display_events()
            .any(|event| matches!(event.message, Message::PerpetualContext(_)))
    );
    assert!(!ingress.quality().unwrap().is_empty());
    assert!(!absent.exists());
}

#[test]
fn rejects_bad_metadata_policy_and_non_exact_prices_before_publishing() {
    let temp = tempfile::tempdir().unwrap();
    let settings = || serde_json::from_value::<Config>(config(false)).unwrap();
    let bogus = META.replace("\"szDecimals\":5", "\"szDecimals\":7");
    assert!(Hyperliquid::open(temp.path(), settings(), &bogus, BASE).is_err());
    assert!(Hyperliquid::open(temp.path(), settings(), r#"{"universe":[]}"#, BASE).is_err());
    let mut no_display = config(false);
    no_display["policy"]["display_live"] = false.into();
    assert!(
        Hyperliquid::open(
            temp.path(),
            serde_json::from_value(no_display).unwrap(),
            META,
            BASE
        )
        .is_err()
    );
    let mut adapter = Hyperliquid::open(temp.path(), settings(), META, BASE).unwrap();
    adapter
        .apply(TransportEvent::Connected {
            receive_ts: BASE.into(),
        })
        .unwrap();
    let book = INITIAL.lines().next().unwrap();
    for invalid in [
        book.replace("\"123.1\"", "123.1"),
        book.replace("\"123.1\"", "\"123.11\""),
    ] {
        assert!(
            adapter
                .apply(TransportEvent::Frame {
                    data: invalid,
                    receive_ts: (BASE + 1).into()
                })
                .is_err()
        );
    }
    assert!(
        adapter
            .ingress()
            .display_events()
            .all(|event| matches!(event.message, Message::Reset(_)))
    );
}

#[test]
fn invalid_trade_batch_cannot_publish_its_valid_prefix() {
    let root = tempfile::tempdir().unwrap();
    let mut adapter = Hyperliquid::open(
        root.path(),
        serde_json::from_value(config(false)).unwrap(),
        META,
        BASE,
    )
    .unwrap();
    adapter
        .apply(TransportEvent::Connected {
            receive_ts: BASE.into(),
        })
        .unwrap();
    let mut frame: Value = serde_json::from_str(INITIAL.lines().nth(3).unwrap()).unwrap();
    frame["data"][1]["sz"] = json!("0");
    let error = adapter
        .apply(TransportEvent::Frame {
            data: frame.to_string(),
            receive_ts: (BASE + 10).into(),
        })
        .unwrap_err();
    assert!(error.is::<RejectedFrame>());
    assert!(
        adapter
            .ingress()
            .display_events()
            .all(|event| matches!(event.message, Message::Reset(_)))
    );
}

#[test]
fn live_collector_replaces_bad_transport_and_recovers_fresh_book() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let config_path = root.join("hyperliquid.json");
    fs::write(&config_path, config(true).to_string()).unwrap();
    let fresh = RECONNECTED.lines().nth(1).unwrap();
    let mut invalid: Value = serde_json::from_str(fresh).unwrap();
    invalid["data"]["levels"][0][0]["px"] = json!("123.41");
    write_transport(
        &root.join("first.jsonl"),
        [
            payload("metadata", 0, META),
            control("connected", 1),
            old_btc_book(10),
            payload("frame", 20, &invalid.to_string()),
            // The line already buffered behind the rejection belongs to the closed transport.
            payload("frame", 21, fresh),
        ],
    );
    write_transport(
        &root.join("second.jsonl"),
        [
            payload("metadata", 200, META),
            control("connected", 201),
            old_btc_book(210),
            payload("frame", 220, fresh),
        ],
    );
    let driver = root.join("stub-python");
    fs::write(
        &driver,
        r#"#!/bin/sh
set -eu
test "$*" = "-m fertile_finance.hyperliquid_transport BTC ETH SOL"
cd "$FERTILE_DATA_ROOT"
if ! test -e first.started; then
    : > first.started
    /bin/cat first.jsonl
    exec /bin/sleep 8
fi
: > second.started
/bin/cat second.jsonl
attempts=0
while ! test -e finish && test "$attempts" -lt 80; do
    /bin/sleep 0.1
    attempts=$((attempts + 1))
done
"#,
    )
    .unwrap();
    fs::set_permissions(&driver, fs::Permissions::from_mode(0o700)).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_finance-hyperliquid"))
        .arg(config_path)
        .env_clear()
        .env("FERTILE_DATA_ROOT", root)
        .env("FINANCE_PYTHON", driver)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(6);
    let mut recovered = false;
    while child.try_wait().unwrap().is_none() && Instant::now() < deadline {
        if events(root).iter().any(|event| {
            event.receive_ts.get() == BASE + 220 && matches!(event.message, Message::Snapshot(_))
        }) {
            recovered = true;
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    // The fixture holds its second connection open until recovery is observed.
    let still_running = child.try_wait().unwrap().is_none();
    fs::write(root.join("finish"), "").unwrap();
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(recovered && still_running, "{stderr}");
    assert!(
        !output.status.success() && stderr.contains("Hyperliquid transport ended"),
        "{stderr}"
    );
    assert!(output.stdout.is_empty());
    assert_eq!(
        stderr.matches("invalid Hyperliquid market frame").count(),
        1
    );
    assert!(root.join("second.started").is_file());
    let published = events(root);
    assert_successors(&published);
    let snapshots: Vec<_> = published
        .iter()
        .filter(|event| matches!(event.message, Message::Snapshot(_)))
        .map(|event| event.receive_ts.get())
        .collect();
    assert_eq!(snapshots, [BASE + 10, BASE + 220]);
    let gap = published
        .iter()
        .find_map(|event| match &event.message {
            Message::Gap(gap) if event.channel.as_str() == "book" => Some(gap),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        (gap.from_ts.get(), gap.to_ts.get()),
        (BASE + 11, BASE + 219)
    );
    assert_eq!(gap.reason.as_str(), "continuity_unknown");
    let reports = read_quality(root, &SOURCE.parse().unwrap(), &PRODUCT.parse().unwrap()).unwrap();
    let recovery: Vec<_> = reports
        .iter()
        .filter(|report| report.outcome == Some(RecoveryOutcome::BookRestoredFromSnapshot))
        .collect();
    assert_eq!(recovery.len(), 1);
    assert_eq!(recovery[0].to_ts.get(), BASE + 220);
}
