use std::{fs, os::unix::fs::symlink};

use crate::{
    market_ingress::{QualityReport, recording_policy, wal_root},
    market_wal::WalReader,
    option_flow::IBKR,
};

use super::*;

fn policy() -> ProductPolicy {
    ProductPolicy {
        source: IBKR.parse().unwrap(),
        product: OPRA_HISTORY.parse().unwrap(),
        license: "synthetic".parse().unwrap(),
        display_live: true,
        record_local: true,
        derive_non_display: false,
        backup_personal_drive: false,
    }
}

fn worktree(path: &Path) {
    fs::create_dir(path.join(".git")).unwrap();
    fs::write(path.join(".git/HEAD"), "ref: refs/heads/synthetic\n").unwrap();
}

#[test]
fn recording_rejects_linked_wal_directory_components() {
    let data_root = tempfile::tempdir().unwrap();
    let checkout = tempfile::tempdir().unwrap();
    worktree(checkout.path());
    let mut destination = data_root.path().to_owned();
    let mut protected = checkout.path().to_owned();

    for component in ["finance-state", "market-wal", IBKR, OPRA_HISTORY] {
        destination.push(component);
        protected.push(component);
        fs::create_dir(&protected).unwrap();
        symlink(&protected, &destination).unwrap();

        assert!(
            OptionHistoryIngress::open(data_root.path(), policy(), FlowUse::PrivateDisplay)
                .is_err(),
            "must reject linked {component}"
        );
        assert!(protected.read_dir().unwrap().next().is_none());
        fs::remove_file(&destination).unwrap();
        fs::create_dir(&destination).unwrap();
    }
}

#[test]
fn recording_rejects_linked_wal_metadata_without_touching_target() {
    let checkout = tempfile::tempdir().unwrap();
    worktree(checkout.path());
    let access = policy();
    let normal = tempfile::tempdir().unwrap();
    drop(OptionHistoryIngress::open(normal.path(), policy(), FlowUse::PrivateDisplay).unwrap());
    let checkpoint = fs::read(
        wal_root(normal.path())
            .join(IBKR)
            .join(OPRA_HISTORY)
            .join("checkpoint.json"),
    )
    .unwrap();

    for (name, contents) in [
        ("access.json", serde_json::to_vec(&access).unwrap()),
        ("writer.lock", Vec::new()),
        ("checkpoint.json", checkpoint),
        ("quality.json", b"[]\n".to_vec()),
    ] {
        let data_root = tempfile::tempdir().unwrap();
        let product = wal_root(data_root.path()).join(IBKR).join(OPRA_HISTORY);
        fs::create_dir_all(&product).unwrap();
        let target = checkout.path().join(name);
        let link = product.join(name);
        fs::write(&target, &contents).unwrap();
        symlink(&target, &link).unwrap();

        assert!(
            OptionHistoryIngress::open(data_root.path(), policy(), FlowUse::PrivateDisplay)
                .is_err(),
            "must reject linked {name}"
        );
        assert_eq!(fs::read(&target).unwrap(), contents);
        assert!(link.is_symlink());
        if name == "access.json" {
            assert!(recording_policy(data_root.path(), &access.source, &access.product).is_err());
        }
        if name != "checkpoint.json" {
            assert!(!product.join("checkpoint.json").exists());
        }
    }
}

#[test]
fn recording_pins_wal_directory_when_live_path_is_replaced_by_link() {
    let data_root = tempfile::tempdir().unwrap();
    let mut history =
        OptionHistoryIngress::open(data_root.path(), policy(), FlowUse::PrivateDisplay).unwrap();
    let original = wal_root(data_root.path());
    let held = data_root.path().join("held-wal");
    fs::rename(&original, &held).unwrap();

    let checkout = tempfile::tempdir().unwrap();
    worktree(checkout.path());
    let protected = checkout.path().join(IBKR).join(OPRA_HISTORY);
    fs::create_dir_all(&protected).unwrap();
    symlink(checkout.path(), &original).unwrap();

    let examples: serde_json::Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../protocol/v3/examples.json"
    )))
    .unwrap();
    let event: Event = serde_json::from_value(examples["events"][0].clone()).unwrap();
    let instrument = serde_json::from_value(examples["instruments"][0].clone()).unwrap();
    let capability = serde_json::from_value(examples["capabilities"][0].clone()).unwrap();
    let report = QualityReport {
        channel: event.channel.clone(),
        instrument_id: event.instrument_id.clone(),
        from_ts: 0.into(),
        to_ts: 1.into(),
        cause: GapCause::SourceGap,
        outcome: None,
        count: None,
    };

    let ingress = history.ingress();
    ingress
        .publish(event.clone(), &instrument, &capability)
        .unwrap();
    assert_eq!(ingress.flush().unwrap(), Some(1));
    ingress.report_quality(report.clone()).unwrap();
    assert_eq!(ingress.quality().unwrap(), [report]);
    assert_eq!(ingress.watermarks().unwrap().local_durable.get(), 1);
    assert!(protected.read_dir().unwrap().next().is_none());
    let reader = WalReader::open(&held, IBKR, OPRA_HISTORY).unwrap();
    assert_eq!(reader.read_after(0, 1).unwrap()[0].event, event);
    assert!(WalReader::open(&original, IBKR, OPRA_HISTORY).is_err());
}
