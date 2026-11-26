use finance_collector::market_ingress::{
    GapCause, MarketIngress, ProductPolicy, PublishResult, QualityReport,
};
use finance_market_protocol::{Capability, Event, Instrument, ProtocolJson, decode_json};
use rusqlite::{Connection, params};
use serde_json::Value;
use std::{fs, path::Path};

// Validated, generated F1 synthetic events. Reuse the two equal-time trades,
// their correction, the long-precision bar and the u64::MAX book delta.
const EXAMPLES: &str = include_str!("../../../../protocol/v1/examples.json");
pub const SOURCE: &str = "fixture";
pub const TRADES: &str = "us-etf-delayed";

pub fn policy(product: &str, record_local: bool) -> ProductPolicy {
    ProductPolicy {
        source: SOURCE.parse().unwrap(),
        product: product.parse().unwrap(),
        license: "fixture.synthetic".parse().unwrap(),
        display_live: true,
        record_local,
        derive_non_display: record_local,
        backup_personal_drive: false,
    }
}

fn from_fixture<T: ProtocolJson>(value: &Value) -> T {
    decode_json(&value.to_string()).unwrap()
}

pub fn publish(ingress: &mut MarketIngress, indexes: &[usize], record_local: bool) -> Vec<Value> {
    let examples: Value = serde_json::from_str(EXAMPLES).unwrap();
    indexes
        .iter()
        .map(|&index| {
            let data = &examples["events"][index];
            let instrument = examples["instruments"]
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| entry["instrument_id"] == data["instrument_id"])
                .unwrap();
            let capability = examples["capabilities"]
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| {
                    entry["source"] == data["source"] && entry["product"] == data["product"]
                })
                .unwrap();
            let outcome = ingress
                .publish(
                    from_fixture::<Event>(data),
                    &from_fixture::<Instrument>(instrument),
                    &from_fixture::<Capability>(capability),
                )
                .unwrap();
            if record_local {
                assert!(matches!(outcome, PublishResult::Staged(_)));
            } else {
                assert_eq!(outcome, PublishResult::DisplayOnly);
            }
            data.clone()
        })
        .collect()
}

pub fn quality(channel: &str, from: u64, to: u64) -> QualityReport {
    QualityReport {
        channel: channel.parse().unwrap(),
        instrument_id: "SPY.ARCA".parse().unwrap(),
        from_ts: from.into(),
        to_ts: to.into(),
        cause: GapCause::Rotation,
        outcome: None,
        count: None,
    }
}

pub fn projection_dir(root: &Path) -> std::path::PathBuf {
    root.join("finance-state/market-projection")
        .join(SOURCE)
        .join(TRADES)
}

// Represent only the v1 rows consumed by the read side. The opt-in test below
// checks the same path against SQLite and Parquet produced by the real projector.
pub fn index_fixture(root: &Path, through: &str) -> Connection {
    let dir = projection_dir(root);
    fs::create_dir_all(&dir).unwrap();
    let index = Connection::open(dir.join("projection.sqlite3")).unwrap();
    index.execute_batch(
        "PRAGMA user_version = 1;
         CREATE TABLE checkpoint (singleton INTEGER PRIMARY KEY, projected_through TEXT NOT NULL);
         CREATE TABLE latest (channel TEXT NOT NULL, instrument_id TEXT NOT NULL, epoch TEXT NOT NULL,
         seq TEXT NOT NULL, kind TEXT NOT NULL, revision TEXT NOT NULL, position TEXT NOT NULL,
         PRIMARY KEY (channel, instrument_id, epoch, seq)) WITHOUT ROWID;",
    ).unwrap();
    index
        .execute("INSERT INTO checkpoint VALUES (1, ?1)", [through])
        .unwrap();
    index
}

pub fn index_trade(index: &Connection, seq: &str, revision: &str, position: u64) {
    index.execute("INSERT OR REPLACE INTO latest VALUES ('trades', 'SPY.ARCA', '41', ?1, 'trade', ?2, ?3)", params![seq, revision, format!("{position:020}")]).unwrap();
}
