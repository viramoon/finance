use finance_collector::market_ingress::{MarketIngress, ProductPolicy};
use finance_market_protocol::{Capability, Event, Instrument, ProtocolJson, decode_json};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::process::{Command, Output};
use std::{env, fs};

const EXAMPLES: &str = include_str!("../../../protocol/v1/examples.json");
const OPTION_EXAMPLES: &str = include_str!("../../../protocol/v3/examples.json");
const WAL_BIN: &str = env!("CARGO_BIN_EXE_finance-market-wal");
const SOURCE: &str = "fixture";
const PRODUCT: &str = "us-etf-delayed";

fn fixture<T: ProtocolJson>(value: &Value) -> T {
    decode_json(&value.to_string()).unwrap()
}

fn policy(backup_personal_drive: bool) -> ProductPolicy {
    ProductPolicy {
        source: SOURCE.parse().unwrap(),
        product: PRODUCT.parse().unwrap(),
        license: "fixture.synthetic".parse().unwrap(),
        display_live: true,
        record_local: true,
        derive_non_display: true,
        backup_personal_drive,
    }
}

fn stage_deliveries(ingress: &mut MarketIngress) -> Vec<Value> {
    // F1 generated synthetic examples specify two distinct trades at the same
    // nanosecond, a revision of the first trade, and a gap after the revision.
    let examples: Value = serde_json::from_str(EXAMPLES).unwrap();
    let instrument = fixture::<Instrument>(&examples["instruments"][0]);
    let capability = fixture::<Capability>(&examples["capabilities"][0]);
    [0, 1, 1, 2]
        .into_iter()
        .map(|index| {
            let event = &examples["events"][index];
            ingress
                .publish(fixture::<Event>(event), &instrument, &capability)
                .unwrap();
            event.clone()
        })
        .collect()
}

fn cli(root: &Path, action: &str) -> Command {
    let mut command = Command::new(WAL_BIN);
    command.arg("--data-root").arg(root).arg(action);
    if !matches!(action, "products" | "restore") {
        command.args(["--source", SOURCE, "--product", PRODUCT]);
    }
    command
}

fn checked_output(output: Output) -> Vec<u8> {
    assert!(
        output.status.success(),
        "process exited {}; stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn cli_json(root: &Path, action: &str) -> Value {
    serde_json::from_slice(&checked_output(cli(root, action).output().unwrap())).unwrap()
}

fn assert_watermarks(root: &Path, event: &Value, durable: &str, projected: &str) -> Value {
    let marks = cli_json(root, "watermarks");
    assert_eq!(
        marks,
        json!({
            "source_latest": {
                "channel": event["channel"],
                "instrument_id": event["instrument_id"],
                "epoch": event["epoch"],
                "seq": event["seq"],
                "exchange_ts": event["exchange_ts"],
                "receive_ts": event["receive_ts"],
            },
            "local_durable": durable,
            "projected": projected,
            "remote_verified": null,
        }),
    );
    marks
}

fn read(root: &Path, after: &str, limit: &str) -> Vec<Value> {
    let output = cli(root, "read")
        .args(["--after", after, "--limit", limit])
        .output()
        .unwrap();
    String::from_utf8(checked_output(output))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn cli_replays_only_confirmed_events_and_checksums_the_sealed_prefix() {
    let root = tempfile::tempdir().unwrap();
    let mut ingress = MarketIngress::open(root.path(), policy(true)).unwrap();
    let events = stage_deliveries(&mut ingress);

    assert!(read(root.path(), "0", "10").is_empty());
    assert_eq!(ingress.flush().unwrap(), Some(4));
    assert_eq!(
        cli_json(root.path(), "products"),
        json!([{"source": SOURCE, "product": PRODUCT}])
    );
    assert_eq!(
        [read(root.path(), "0", "2"), read(root.path(), "2", "10")].concat(),
        events
            .iter()
            .enumerate()
            .map(|(index, event)| json!({
                "position": (index + 1).to_string(),
                "generation": "1",
                "event": event,
            }))
            .collect::<Vec<_>>()
    );
    assert!(read(root.path(), "4", "10").is_empty());
    assert!(
        cli_json(root.path(), "manifest")["wal"]["segments"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(!cli(root.path(), "seal").output().unwrap().status.success());

    ingress.seal_for_backup().unwrap();
    let manifest = cli_json(root.path(), "manifest");
    drop(ingress);
    assert_eq!(cli_json(root.path(), "seal"), manifest);
    assert_eq!(manifest["access"]["backup_personal_drive"], true);
    assert_eq!(manifest["wal"]["sealed_through"], "4");
    let [segment] = manifest["wal"]["segments"].as_array().unwrap().as_slice() else {
        panic!("expected a single sealed segment");
    };
    assert_eq!(segment["first"], "1");
    assert_eq!(segment["last"], "4");
    let file = root
        .path()
        .join("finance-state/market-wal")
        .join(segment["path"].as_str().unwrap());
    let bytes = fs::read(file).unwrap();
    assert_eq!(segment["bytes"], bytes.len().to_string());
    assert_eq!(segment["sha256"], format!("{:x}", Sha256::digest(&bytes)));

    let marks = assert_watermarks(root.path(), &events[3], "4", "0");

    let isolated = tempfile::tempdir().unwrap();
    let copied_file = isolated
        .path()
        .join("finance-state/market-wal")
        .join(segment["path"].as_str().unwrap());
    fs::create_dir_all(copied_file.parent().unwrap()).unwrap();
    let backup = isolated.path().join("backup.json");
    fs::write(&backup, manifest.to_string()).unwrap();
    let restore = || {
        cli(isolated.path(), "restore")
            .arg("--manifest")
            .arg(&backup)
            .output()
            .unwrap()
    };

    let mut corrupted = bytes.clone();
    let last_byte = corrupted.last_mut().unwrap();
    *last_byte ^= 1;
    fs::write(&copied_file, corrupted).unwrap();
    assert!(!restore().status.success());
    assert_eq!(cli_json(isolated.path(), "products"), json!([]));

    fs::write(copied_file, bytes).unwrap();
    checked_output(restore());
    checked_output(restore());
    assert_eq!(
        read(isolated.path(), "0", "10"),
        read(root.path(), "0", "10")
    );
    assert_eq!(cli_json(isolated.path(), "watermarks"), marks);
}

#[test]
fn cli_reports_all_four_watermarks_with_nonzero_projection() {
    let root = tempfile::tempdir().unwrap();
    let mut ingress = MarketIngress::open(root.path(), policy(true)).unwrap();
    let events = stage_deliveries(&mut ingress);
    assert_eq!(ingress.flush().unwrap(), Some(4));
    let projection = root
        .path()
        .join("finance-state/market-projection")
        .join(SOURCE)
        .join(PRODUCT);
    fs::create_dir_all(&projection).unwrap();
    let marker = projection.join("watermark.json");
    fs::write(&marker, r#"{"projected_through":"3"}"#).unwrap();
    assert_watermarks(root.path(), &events[3], "4", "3");

    fs::write(&marker, r#"{"projected_through":"5"}"#).unwrap();
    let output = cli(root.path(), "watermarks").output().unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("projection watermark is ahead"));
}

#[test]
fn cli_refuses_remote_manifest_for_locally_recordable_product() {
    let root = tempfile::tempdir().unwrap();
    let mut ingress = MarketIngress::open(root.path(), policy(false)).unwrap();
    stage_deliveries(&mut ingress);
    ingress.seal().unwrap();

    assert_eq!(read(root.path(), "0", "10").len(), 4);
    let output = cli(root.path(), "manifest").output().unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
}

#[test]
#[ignore = "requires FINANCE_F3_TEST_PYTHON pointing to the built project Python environment"]
fn historical_option_wal_projects_through_python_cli() {
    let root = tempfile::tempdir().unwrap();
    let examples: Value = serde_json::from_str(OPTION_EXAMPLES).unwrap();
    let instrument = fixture::<Instrument>(&examples["instruments"][0]);
    let capability = fixture::<Capability>(&examples["capabilities"][0]);
    let events = examples["events"].as_array().unwrap();
    let mut ingress = MarketIngress::open(
        root.path(),
        ProductPolicy {
            source: capability.source.clone(),
            product: capability.product.clone(),
            license: events[0]["license"].as_str().unwrap().parse().unwrap(),
            ..policy(false)
        },
    )
    .unwrap();
    for event in events {
        ingress
            .publish(fixture::<Event>(event), &instrument, &capability)
            .unwrap();
    }
    assert_eq!(ingress.flush().unwrap(), Some(2));

    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let python = env::var_os("FINANCE_F3_TEST_PYTHON").unwrap();
    checked_output(
        Command::new(&python)
            .env("PYTHONPATH", repo.join("python"))
            .args([
                "-B",
                "-m",
                "fertile_finance.market_projector",
                "--data-root",
            ])
            .arg(root.path())
            .args(["--wal-bin", WAL_BIN, "--once"])
            .output()
            .unwrap(),
    );
    assert_eq!(ingress.watermarks().unwrap().projected.get(), 2);
    let output = checked_output(
        Command::new(&python)
            .env("PYTHONPATH", repo.join("python"))
            .args([
                "-B",
                "-c",
                r#"
import sys
from pathlib import Path
import pyarrow.parquet as pq
from fertile_finance.market_projector import projected_partitions

partition, = projected_partitions(Path(sys.argv[1]), "ibkr", "opra-history")
for event in pq.ParquetFile(partition.path).read()["event_json"].to_pylist():
    print(event)
"#,
            ])
            .arg(root.path())
            .output()
            .unwrap(),
    );
    let projected: Vec<Value> = std::str::from_utf8(&output)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(&projected, events);
}

#[test]
#[ignore = "requires FINANCE_F3_TEST_PYTHON pointing to the built project Python environment"]
fn committed_wal_projects_through_python_cli() {
    let root = tempfile::tempdir().unwrap();
    let mut ingress = MarketIngress::open(root.path(), policy(true)).unwrap();
    let events = stage_deliveries(&mut ingress);
    ingress.flush().unwrap();

    let examples: Value = serde_json::from_str(EXAMPLES).unwrap();
    let instrument = fixture::<Instrument>(&examples["instruments"][0]);
    let capability = fixture::<Capability>(&examples["capabilities"][0]);
    ingress
        .publish(
            fixture::<Event>(&examples["events"][3]),
            &instrument,
            &capability,
        )
        .unwrap();

    let python = env::var_os("FINANCE_F3_TEST_PYTHON").unwrap();
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let projector = || {
        let stdout = checked_output(
            Command::new(&python)
                .env("PYTHONPATH", repo.join("python"))
                .args([
                    "-B",
                    "-m",
                    "fertile_finance.market_projector",
                    "--data-root",
                ])
                .arg(root.path())
                .args(["--wal-bin", WAL_BIN, "--once"])
                .output()
                .unwrap(),
        );
        assert!(stdout.is_empty(), "the projection CLI should be silent");
    };
    assert_watermarks(root.path(), &events[3], "4", "0");
    projector();
    assert_watermarks(root.path(), &events[3], "4", "4");
    projector();
    assert_watermarks(root.path(), &events[3], "4", "4");

    ingress.seal().unwrap();
    projector();
    assert_watermarks(root.path(), &examples["events"][3], "5", "5");

    let verify = r#"
import json
from pathlib import Path
import sys
import pyarrow.parquet as pq
from fertile_finance.market_projector import projected_partitions, read_latest_event

root, source, product = Path(sys.argv[1]), sys.argv[2], sys.argv[3]
partitions = projected_partitions(root, source, product)
assert [(p.first_position, p.last_position, p.row_count) for p in partitions] == [(1, 4, 3), (5, 5, 1)]
table = pq.read_table(partitions[0].path)
assert table['wal_position'].to_pylist() == [1, 2, 4]
assert len(set(table['exchange_ts'].to_pylist())) == 1
events = [json.loads(value) for value in table['event_json'].to_pylist()]
assert [event['payload']['price'] for event in events[:2]] == ['663.1200', '663.13']
latest = read_latest_event(root, source, product, 'trades', 'SPY.ARCA', '41', '9007199254740992')
assert json.loads(latest.to_json()) == events[2]
assert events[2]['revision'] == '1'
assert events[2]['payload']['replacement']['payload']['price'] == '663.1100'
"#;
    // Each verifier must exit cleanly after its final read of the latest event.
    // Use fresh interpreters so each run exercises Arrow's shutdown path.
    for _ in 0..8 {
        checked_output(
            Command::new(&python)
                .env("PYTHONPATH", repo.join("python"))
                .args(["-B", "-c", verify])
                .arg(root.path())
                .args([SOURCE, PRODUCT])
                .output()
                .unwrap(),
        );
    }
}

#[test]
#[ignore = "requires FINANCE_F3_TEST_PYTHON pointing to the built project Python environment"]
fn perpetual_context_wal_projects_and_indexes_exact_decimal_strings() {
    let root = tempfile::tempdir().unwrap();
    let examples: Value = serde_json::from_str(EXAMPLES).unwrap();
    let instrument_json = examples["instruments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["instrument_id"] == "BTC-USDT-PERP.BINANCE")
        .unwrap();
    let instrument = fixture::<Instrument>(instrument_json);
    let access = ProductPolicy {
        source: "binance".parse().unwrap(),
        product: "usdt-perp-context".parse().unwrap(),
        ..policy(false)
    };
    let capability = fixture::<Capability>(&json!({
        "schema_version": "2.0", "source": access.source, "product": access.product,
        "message_kinds": ["funding_rate", "open_interest"], "max_depth": 0,
        "can_backfill": false, "delay_tier": "realtime"
    }));
    let mut ingress = MarketIngress::open(root.path(), access.clone()).unwrap();
    let data: Value =
        serde_json::from_str(include_str!("../fixtures/binance/observations.json")).unwrap();
    for (kind, seq, payload) in [
        (
            "funding_rate",
            "1",
            json!({"rate": data["funding"]["r"], "next_funding_ts": "2000000000"}),
        ),
        (
            "open_interest",
            "1",
            json!({"quantity": data["interest"]["openInterest"]}),
        ),
        (
            "funding_rate",
            "2",
            json!({"rate": data["funding"]["r"], "next_funding_ts": null}),
        ),
    ] {
        let event = fixture::<Event>(&json!({
            "schema_version": "2.0", "channel": kind, "kind": kind, "payload": payload,
            "source": access.source, "product": access.product, "license": access.license,
            "instrument_id": instrument.instrument_id, "asset_kind": "perpetual", "venue": "BINANCE",
            "exchange_ts": "1000000000", "receive_ts": "1000000001", "first_available_ts": "1000000001",
            "epoch": "1", "seq": seq, "revision": "0", "cursor": format!("1:{seq}")
        }));
        ingress.publish(event, &instrument, &capability).unwrap();
    }
    assert_eq!(ingress.flush().unwrap(), Some(3));

    let python = env::var_os("FINANCE_F3_TEST_PYTHON").unwrap();
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let verify = r#"
from pathlib import Path
import sys
from fertile_finance.market_projector import main, projected_partitions, read_latest_event

root = Path(sys.argv[1])
sys.argv[1:2] = ["--data-root", str(root), "--wal-bin"]
assert main() == 0
source, product = "binance", "usdt-perp-context"
[part] = projected_partitions(root, source, product)
assert part.row_count == 3
def get(channel, seq="1"):
    return read_latest_event(root, source, product, channel, "BTC-USDT-PERP.BINANCE", "1", seq)
assert get("funding_rate").to_dict()["payload"]["rate"] == "-0.00000001"
assert get("open_interest").to_dict()["payload"]["quantity"] == "123.456789123456"
assert get("funding_rate", "2").to_dict()["payload"] == {
    "rate": "-0.00000001", "next_funding_ts": None,
}
"#;
    checked_output(
        Command::new(&python)
            .env("PYTHONPATH", repo.join("python"))
            .args(["-B", "-c", verify])
            .arg(root.path())
            .args([WAL_BIN, "--once"])
            .output()
            .unwrap(),
    );
}
