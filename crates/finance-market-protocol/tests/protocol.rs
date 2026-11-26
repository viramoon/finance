use finance_market_protocol::{Capability, Event, Instrument};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{collections::BTreeSet, fmt::Debug, fs, path::Path, process::Command};

const EXAMPLES: &str = include_str!("../../../protocol/v1/examples.json");

fn examples() -> Value {
    serde_json::from_str(EXAMPLES).unwrap()
}

fn decode<T: DeserializeOwned>(value: &Value) -> T {
    serde_json::from_str(&value.to_string()).unwrap()
}

fn assert_roundtrip<T: DeserializeOwned + Serialize>(values: &Value) {
    for value in values.as_array().unwrap() {
        assert_eq!(serde_json::to_value(decode::<T>(value)).unwrap(), *value);
    }
}

fn assert_rejected<T: DeserializeOwned + Debug>(value: &Value, expected: &str) {
    let error = serde_json::from_str::<T>(&value.to_string()).unwrap_err();
    assert!(
        error.to_string().contains(expected),
        "expected {expected:?} in {error} for {value}"
    );
}

fn assert_references_resolve(root: &Value, value: &Value) {
    match value {
        Value::Object(object) => {
            if let Some(reference) = object.get("$ref") {
                let reference = reference.as_str().unwrap();
                let pointer = reference.strip_prefix("#/$defs/").unwrap();
                assert!(
                    root.pointer(&format!("/$defs/{pointer}")).is_some(),
                    "unresolved schema reference {reference}"
                );
            }
            for child in object.values() {
                assert_references_resolve(root, child);
            }
        }
        Value::Array(values) => {
            for child in values {
                assert_references_resolve(root, child);
            }
        }
        _ => {}
    }
}

#[test]
fn producer_exports_match_committed_draft_2020_12_contract() {
    let directory = tempfile::tempdir().unwrap();
    let status = Command::new(env!("CARGO_BIN_EXE_export-market-protocol"))
        .arg(directory.path())
        .status()
        .unwrap();
    assert!(status.success());

    let committed = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../protocol/v1");
    for name in [
        "instrument.schema.json",
        "capability.schema.json",
        "event.schema.json",
        "examples.json",
    ] {
        let exported = fs::read(directory.path().join(name)).unwrap();
        assert_eq!(
            exported,
            fs::read(committed.join(name)).unwrap(),
            "regenerate {name} with export-market-protocol"
        );
        let document: Value = serde_json::from_slice(&exported).unwrap();
        if name.ends_with(".schema.json") {
            assert_eq!(
                document["$schema"],
                "https://json-schema.org/draft/2020-12/schema"
            );
            assert_references_resolve(&document, &document);
        } else {
            let input: Value =
                serde_json::from_str(include_str!("../fixtures/examples.input.json")).unwrap();
            assert_eq!(
                document, input,
                "fixture export must preserve all wire values"
            );
        }
    }
}

#[test]
fn exported_schema_keeps_envelopes_closed_and_exact_wire_types() {
    let event: Value =
        serde_json::from_str(include_str!("../../../protocol/v1/event.schema.json")).unwrap();
    assert_eq!(event["unevaluatedProperties"], false);
    assert_eq!(
        event["$defs"]["MarketMessage"]["unevaluatedProperties"],
        false
    );
    for definition in ["Trade", "Heartbeat", "Correction", "BookLevel"] {
        assert_eq!(event["$defs"][definition]["type"], "object");
        assert_eq!(event["$defs"][definition]["additionalProperties"], false);
    }
    assert_eq!(event["$defs"]["DecimalText"]["type"], "string");
    assert_eq!(event["$defs"]["U64Text"]["type"], "string");

    let capability: Value =
        serde_json::from_str(include_str!("../../../protocol/v1/capability.schema.json")).unwrap();
    assert_eq!(
        capability["properties"]["message_kinds"]["uniqueItems"],
        true
    );
    let max_depth = &capability["properties"]["max_depth"];
    assert_eq!(max_depth["type"], "integer");
    assert_eq!(max_depth["minimum"], 0);
    assert_eq!(max_depth["maximum"], u32::MAX);
    assert!(max_depth.get("format").is_none());
}

#[test]
fn fixture_roundtrips_distinct_venues_nanoseconds_corrections_and_delays() {
    let examples = examples();
    assert_roundtrip::<Instrument>(&examples["instruments"]);
    assert_roundtrip::<Capability>(&examples["capabilities"]);
    assert_roundtrip::<Event>(&examples["events"]);

    let instruments = examples["instruments"].as_array().unwrap();
    assert_eq!(
        instruments[0]["supplier_symbols"],
        instruments[1]["supplier_symbols"]
    );
    assert_eq!(instruments[0]["instrument_id"], "SPY.ARCA");
    assert_eq!(instruments[1]["instrument_id"], "SPY.BATS");
    assert_eq!(
        instruments[2]["instrument_id"],
        "SPY-SPLIT-NORMALIZED.RESEARCH"
    );
    assert_eq!(instruments[3]["instrument_id"], "SPX.CBOE");
    assert_eq!(instruments[4]["instrument_id"], "BTC-USDT-PERP.BINANCE");

    let records = examples["events"].as_array().unwrap();
    let first = &records[0];
    let second = &records[1];
    let correction = &records[2];
    assert_eq!(first["exchange_ts"], "1760000000123456789");
    assert_eq!(first["exchange_ts"], second["exchange_ts"]);
    assert_ne!(first["native_id"], second["native_id"]);
    assert_eq!(first["seq"], "9007199254740992");
    assert_eq!(second["seq"], "9007199254740993");
    assert_eq!(first["payload"]["price"], "663.1200");
    assert_eq!(first["revision"], "0");
    assert_eq!(correction["revision"], "1");
    assert_eq!(correction["payload"]["target_epoch"], first["epoch"]);
    assert_eq!(correction["payload"]["target_seq"], first["seq"]);
    assert_eq!(
        correction["payload"]["replacement"]["payload"]["price"],
        "663.1100"
    );
    assert_eq!(correction["first_available_ts"], "1760001300123456789");
    assert_eq!(first["first_available_ts"], "1760001200123456789");
    assert_eq!(first["receive_ts"], "1760001200123457789");
    assert_eq!(examples["capabilities"][0]["delay_tier"], "delayed");
    assert_eq!(records[10]["seq"], "18446744073709551615");
    assert_eq!(
        records
            .iter()
            .map(|record| record["kind"].as_str().unwrap())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([
            "bar_closed",
            "correction",
            "delta",
            "gap",
            "heartbeat",
            "reset",
            "snapshot",
            "status",
            "trade",
        ])
    );
}

#[test]
fn malformed_events_are_rejected_with_specific_errors() {
    let examples = examples();
    let original = &examples["events"][0];
    let mut event = original.clone();
    event["payload"]["price"] = json!("6.6312e2");
    assert_rejected::<Event>(&event, "exponent");

    let mut event = original.clone();
    event.as_object_mut().unwrap().remove("first_available_ts");
    assert_rejected::<Event>(&event, "first_available_ts");

    let mut event = original.clone();
    event["payload"].as_object_mut().unwrap().remove("quantity");
    assert_rejected::<Event>(&event, "quantity");

    let mut event = original.clone();
    event["surprise"] = json!(true);
    assert_rejected::<Event>(&event, "surprise");

    let mut event = original.clone();
    event["payload"]["surprise"] = json!(true);
    assert_rejected::<Event>(&event, "surprise");

    let mut correction = examples["events"][2].clone();
    correction["payload"]["replacement"]["surprise"] = json!(true);
    assert_rejected::<Event>(&correction, "surprise");

    let mut event = original.clone();
    event["exchange_ts"] = json!(null);
    assert_rejected::<Event>(&event, "exchange_ts");

    let mut event = original.clone();
    event["seq"] = json!(9007199254740992_u64);
    assert_rejected::<Event>(&event, "string");

    let mut event = original.clone();
    event["seq"] = json!("18446744073709551616");
    assert_rejected::<Event>(&event, "64");

    let mut event = original.clone();
    event["native_id"] = Value::Null;
    assert_rejected::<Event>(&event, "string");

    let mut event = original.clone();
    event["revision"] = json!("1");
    assert_rejected::<Event>(&event, "revision");

    let mut correction = examples["events"][2].clone();
    correction["payload"]["target_seq"] = correction["seq"].clone();
    assert_rejected::<Event>(&correction, "target");

    let mut reset = examples["events"][4].clone();
    reset["seq"] = json!("1");
    assert_rejected::<Event>(&reset, "seq");

    let duplicate_price = original.to_string().replace(
        "\"price\":\"663.1200\"",
        "\"price\":\"663.1200\",\"price\":\"663.1100\"",
    );
    let error = serde_json::from_str::<Event>(&duplicate_price).unwrap_err();
    assert!(error.to_string().contains("duplicate field"), "{error}");
}

#[test]
fn unit_tags_require_json_strings() {
    let examples = examples();

    let mut trade = examples["events"][0].clone();
    trade["payload"]["side"] = json!({"buy": null});
    assert_rejected::<Event>(&trade, "expected a string");

    let mut instrument = examples["instruments"][0].clone();
    instrument["asset_kind"] = json!({"etf": null});
    assert_rejected::<Instrument>(&instrument, "expected a string");

    let mut capability = examples["capabilities"][0].clone();
    capability["message_kinds"] = json!([{"trade": null}]);
    assert_rejected::<Capability>(&capability, "expected a string");

    let mut capability = examples["capabilities"][0].clone();
    capability["schema_version"] = json!({"1.0": null});
    assert_rejected::<Capability>(&capability, "expected a string");
}

#[test]
fn tagged_payload_discriminants_require_json_strings() {
    let examples = examples();

    let mut trade = examples["events"][0].clone();
    trade["kind"] = json!({"trade": null});
    assert_rejected::<Event>(&trade, "expected a string");

    let mut correction = examples["events"][2].clone();
    correction["payload"]["replacement"]["kind"] = json!({"trade": null});
    assert_rejected::<Event>(&correction, "expected a string");

    let duplicate_kind = examples["events"][0].to_string().replace(
        "\"kind\":\"trade\"",
        "\"kind\":\"trade\",\"kind\":\"trade\"",
    );
    let error = serde_json::from_str::<Event>(&duplicate_kind).unwrap_err();
    assert!(
        error.to_string().contains("duplicate field `kind`"),
        "{error}"
    );
}

#[test]
fn named_payloads_and_records_require_json_objects() {
    let examples = examples();
    let mut trade = examples["events"][0].clone();
    trade["payload"] = json!(["663.1200", "4", "buy"]);
    assert_rejected::<Event>(&trade, "JSON object");

    let mut heartbeat = examples["events"][6].clone();
    heartbeat["payload"] = json!([]);
    assert_rejected::<Event>(&heartbeat, "JSON object");

    let mut correction = examples["events"][2].clone();
    correction["payload"]["replacement"] =
        json!(["trade", correction["payload"]["replacement"]["payload"]]);
    assert_rejected::<Event>(&correction, "JSON object");

    let mut snapshot = examples["events"][9].clone();
    snapshot["payload"]["asks"][0] = json!(["101012.2", "0.008"]);
    assert_rejected::<Event>(&snapshot, "JSON object");

    let capability = json!([
        "1.0",
        "fixture",
        "us-etf-delayed",
        ["trade"],
        0,
        true,
        "delayed"
    ]);
    assert_rejected::<Capability>(&capability, "JSON object");
}

#[test]
fn tick_lot_and_depth_cover_all_market_payloads_and_replacements() {
    let examples = examples();
    let records = &examples["events"];
    let instruments = &examples["instruments"];
    let capabilities = &examples["capabilities"];
    let arca: Instrument = decode(&instruments[0]);
    let research: Instrument = decode(&instruments[2]);
    let perp: Instrument = decode(&instruments[4]);
    let delayed: Capability = decode(&capabilities[0]);
    let daily: Capability = decode(&capabilities[1]);
    let order_book: Capability = decode(&capabilities[2]);

    let mut trade = records[0].clone();
    trade["payload"]["price"] = json!("663.12001");
    let error = decode::<Event>(&trade)
        .validate_against(&arca, &delayed)
        .unwrap_err();
    assert_eq!(error.field(), "payload.price");

    let mut trade = records[0].clone();
    trade["payload"]["quantity"] = json!("4.01");
    let error = decode::<Event>(&trade)
        .validate_against(&arca, &delayed)
        .unwrap_err();
    assert_eq!(error.field(), "payload.quantity");

    let mut correction = records[2].clone();
    correction["payload"]["replacement"]["payload"]["price"] = json!("663.111");
    let error = decode::<Event>(&correction)
        .validate_against(&arca, &delayed)
        .unwrap_err();
    assert_eq!(error.field(), "payload.replacement.payload.price");

    let mut bar = records[8].clone();
    bar["payload"]["open"] = json!("663.12345678901234001");
    let error = decode::<Event>(&bar)
        .validate_against(&research, &daily)
        .unwrap_err();
    assert_eq!(error.field(), "payload.open");

    let mut snapshot = records[9].clone();
    snapshot["payload"]["bids"][0]["price"] = json!("101012.101");
    let error = decode::<Event>(&snapshot)
        .validate_against(&perp, &order_book)
        .unwrap_err();
    assert!(error.field().contains("bids") && error.field().contains("price"));

    let mut delta = records[10].clone();
    delta["payload"]["updates"][1]["quantity"] = json!("0.0121");
    let error = decode::<Event>(&delta)
        .validate_against(&perp, &order_book)
        .unwrap_err();
    assert!(error.field().contains("updates") && error.field().contains("quantity"));

    let mut shallow = capabilities[2].clone();
    shallow["max_depth"] = json!(1);
    let error = decode::<Event>(&records[9])
        .validate_against(&perp, &decode(&shallow))
        .unwrap_err();
    assert!(error.to_string().contains("depth"), "{error}");

    let mut precise_instrument = instruments[0].clone();
    precise_instrument["tick_size"] = json!("0.000000000000000000000000003");
    let precise_instrument = decode(&precise_instrument);
    let mut wide_price = records[0].clone();
    wide_price["payload"]["price"] = json!("9999999999999999999999999999");
    decode::<Event>(&wide_price)
        .validate_against(&precise_instrument, &delayed)
        .unwrap();
    wide_price["payload"]["price"] = json!("9999999999999999999999999998");
    let error = decode::<Event>(&wide_price)
        .validate_against(&precise_instrument, &delayed)
        .unwrap_err();
    assert_eq!(error.field(), "payload.price");
}

#[test]
fn stream_validation_rejects_regressions_gaps_and_undeclared_epoch_changes() {
    let examples = examples();
    let records = &examples["events"];
    let first: Event = decode(&records[0]);
    let second: Event = decode(&records[1]);
    let old_epoch: Event = decode(&records[3]);
    let reset: Event = decode(&records[4]);
    let after_reset: Event = decode(&records[5]);

    assert_eq!(
        first.validate_successor(&second).unwrap_err().field(),
        "seq"
    );
    assert_eq!(first.validate_successor(&first).unwrap_err().field(), "seq");
    assert_eq!(
        old_epoch.validate_successor(&first).unwrap_err().field(),
        "seq"
    );
    assert_eq!(
        after_reset
            .validate_successor(&old_epoch)
            .unwrap_err()
            .field(),
        "epoch"
    );
    assert_eq!(
        old_epoch.validate_successor(&reset).unwrap_err().field(),
        "epoch"
    );
    assert_eq!(
        reset.validate_successor(&reset).unwrap_err().field(),
        "epoch"
    );

    second.validate_successor(&first).unwrap();
    reset.validate_successor(&old_epoch).unwrap();
    after_reset.validate_successor(&reset).unwrap();

    let mut late_correction = records[2].clone();
    late_correction["epoch"] = json!("42");
    late_correction["seq"] = json!("2");
    decode::<Event>(&late_correction)
        .validate_successor(&after_reset)
        .unwrap();

    let bats: Event = decode(&records[7]);
    assert!(bats.validate_successor(&first).is_err());
}

#[test]
fn instrument_and_capability_require_valid_metadata() {
    let examples = examples();
    let mut instrument = examples["instruments"][0].clone();
    instrument["venue"] = json!("BATS");
    assert_rejected::<Instrument>(&instrument, "venue");

    let mut instrument = examples["instruments"][0].clone();
    instrument["tick_size"] = json!("0.00");
    assert_rejected::<Instrument>(&instrument, "tick_size");

    let mut instrument = examples["instruments"][3].clone();
    instrument
        .as_object_mut()
        .unwrap()
        .remove("settlement_currency");
    assert_rejected::<Instrument>(&instrument, "settlement_currency");

    let mut capability = examples["capabilities"][0].clone();
    capability["message_kinds"] = json!(["trade", "trade"]);
    assert_rejected::<Capability>(&capability, "message_kinds");

    let mut capability = examples["capabilities"][2].clone();
    capability["max_depth"] = json!(0);
    assert_rejected::<Capability>(&capability, "max_depth");
}
