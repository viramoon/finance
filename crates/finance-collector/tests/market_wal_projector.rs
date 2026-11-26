use finance_collector::market_ingress::{MarketIngress, ProductPolicy};
use finance_market_protocol::{Capability, Event, Instrument, ProtocolJson, decode_json};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::process::{Command, Output};
use std::{env, fs};

const EXAMPLES: &str = include_str!("../../../protocol/v1/examples.json");
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

    let marks = cli_json(root.path(), "watermarks");
    assert_eq!(
        marks["source_latest"]["receive_ts"],
        events[3]["receive_ts"]
    );
    assert_eq!(marks["local_durable"], "4");
    assert_eq!(marks["projected"], "0");
    assert!(marks["remote_verified"].is_null());

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
fn committed_wal_projects_through_python_cli() {
    let root = tempfile::tempdir().unwrap();
    let mut ingress = MarketIngress::open(root.path(), policy(true)).unwrap();
    stage_deliveries(&mut ingress);
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
    };
    projector();
    assert_eq!(cli_json(root.path(), "watermarks")["projected"], "4");
    projector();
    assert_eq!(cli_json(root.path(), "watermarks")["projected"], "4");

    ingress.seal().unwrap();
    projector();
    let marks = cli_json(root.path(), "watermarks");
    assert_eq!(marks["local_durable"], "5");
    assert_eq!(marks["projected"], "5");
    assert!(marks["remote_verified"].is_null());

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
