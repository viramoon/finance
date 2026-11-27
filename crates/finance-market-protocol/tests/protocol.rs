use finance_market_protocol::{
    Capability, Event, Instrument, ProtocolJson, ValidationError, decode_json,
};
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

fn assert_roundtrip<T: DeserializeOwned + ProtocolJson + Serialize>(values: &Value) {
    for value in values.as_array().unwrap() {
        let json = value.to_string();
        assert_eq!(serde_json::to_value(decode::<T>(value)).unwrap(), *value);
        assert_eq!(
            serde_json::to_value(decode_json::<T>(&json).unwrap()).unwrap(),
            *value
        );
    }
}

fn assert_rejected<T: DeserializeOwned + Debug + ProtocolJson>(
    value: &Value,
    path: &str,
    kind: &str,
    serde_reason: &str,
) {
    assert_rejected_json::<T>(&value.to_string(), path, kind, Some(serde_reason));
}

fn assert_rejected_json<T: DeserializeOwned + Debug + ProtocolJson>(
    text: &str,
    path: &str,
    kind: &str,
    serde_reason: Option<&str>,
) {
    let error = decode_json::<T>(text).unwrap_err();
    assert_eq!(error.path(), path, "{error}");
    assert_eq!(error.kind().as_str(), kind, "{error}");
    let serde_error = serde_json::from_str::<T>(text)
        .expect_err("direct Serde decoding accepted input rejected by the diagnostic decoder");
    if let Some(reason) = serde_reason {
        assert!(
            serde_error.to_string().contains(reason),
            "expected {reason:?} in the native Serde error, got {serde_error}"
        );
    }
}

fn assert_constraint(error: ValidationError, path: &str, reason: &str) {
    assert_eq!(error.path(), path, "{error}");
    if let Some(field) = path.strip_prefix('/') {
        assert_eq!(error.field(), field.replace('/', "."), "{error}");
    }
    assert_eq!(error.kind().as_str(), "constraint_violation", "{error}");
    assert_eq!(error.reason(), reason);
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
    assert_rejected::<Event>(&event, "/payload/price", "invalid_decimal", "exponent");
    let payload = event.as_object_mut().unwrap().remove("payload").unwrap();
    let envelope = event.to_string();
    let payload_before_kind = format!(
        r#"{{"payload":{payload},{}"#,
        envelope.strip_prefix('{').unwrap()
    );
    assert_rejected_json::<Event>(
        &payload_before_kind,
        "/payload/price",
        "invalid_decimal",
        Some("exponent"),
    );

    let mut event = original.clone();
    event["payload"]["quantity"] = json!("4e0");
    assert_rejected::<Event>(&event, "/payload/quantity", "invalid_decimal", "exponent");

    let mut correction = examples["events"][2].clone();
    correction["payload"]["replacement"]["payload"]["price"] = json!("6.6311e2");
    assert_rejected::<Event>(
        &correction,
        "/payload/replacement/payload/price",
        "invalid_decimal",
        "exponent",
    );

    let mut event = original.clone();
    event.as_object_mut().unwrap().remove("first_available_ts");
    assert_rejected::<Event>(
        &event,
        "/first_available_ts",
        "missing_field",
        "missing field `first_available_ts`",
    );

    let mut event = original.clone();
    event["payload"].as_object_mut().unwrap().remove("quantity");
    assert_rejected::<Event>(
        &event,
        "/payload/quantity",
        "missing_field",
        "missing field `quantity`",
    );

    let mut event = original.clone();
    event["surprise"] = json!(true);
    assert_rejected::<Event>(&event, "/surprise", "unknown_field", "surprise");

    let mut event = original.clone();
    event["payload"]["surprise"] = json!(true);
    assert_rejected::<Event>(&event, "/payload/surprise", "unknown_field", "surprise");

    let mut event = original.clone();
    event["payload"]["bad/key~"] = json!(true);
    assert_rejected::<Event>(&event, "/payload/bad~1key~0", "unknown_field", "bad/key~");

    let mut heartbeat = examples["events"][6].clone();
    heartbeat["payload"]["surprise"] = json!(true);
    assert_rejected::<Event>(&heartbeat, "/payload/surprise", "unknown_field", "surprise");

    let mut correction = examples["events"][2].clone();
    correction["payload"]["replacement"]["surprise"] = json!(true);
    assert_rejected::<Event>(
        &correction,
        "/payload/replacement/surprise",
        "unknown_field",
        "surprise",
    );

    let mut event = original.clone();
    event["exchange_ts"] = json!(null);
    assert_rejected::<Event>(
        &event,
        "/exchange_ts",
        "constraint_violation",
        "exchange_ts",
    );

    let mut event = original.clone();
    event["seq"] = json!(9007199254740992_u64);
    assert_rejected::<Event>(&event, "/seq", "invalid_type", "expected a string");
    event["seq"] = json!("09007199254740992");
    assert_rejected::<Event>(&event, "/seq", "invalid_integer", "canonical unsigned");

    event["seq"] = json!("18446744073709551616");
    assert_rejected::<Event>(&event, "/seq", "out_of_range", "unsigned 64-bit range");

    let mut event = original.clone();
    event["native_id"] = Value::Null;
    assert_rejected::<Event>(&event, "/native_id", "invalid_type", "expected a string");

    let mut event = original.clone();
    event["revision"] = json!("1");
    assert_rejected::<Event>(&event, "/revision", "constraint_violation", "revision");

    let mut correction = examples["events"][2].clone();
    correction["payload"]["target_seq"] = correction["seq"].clone();
    assert_rejected::<Event>(
        &correction,
        "/payload/target_seq",
        "constraint_violation",
        "target",
    );

    let mut reset = examples["events"][4].clone();
    reset["seq"] = json!("1");
    assert_rejected::<Event>(&reset, "/seq", "constraint_violation", "seq");

    let mut event = original.clone();
    event["kind"] = json!("option_chain");
    assert_rejected::<Event>(&event, "/kind", "invalid_value", "option_chain");
    event["kind"] = original["kind"].clone();
    event["schema_version"] = json!("2.0");
    assert_rejected::<Event>(&event, "/schema_version", "invalid_value", "2.0");

    let duplicate_price = original.to_string().replace(
        "\"price\":\"663.1200\"",
        "\"price\":\"663.1200\",\"price\":\"663.1100\"",
    );
    assert_rejected_json::<Event>(
        &duplicate_price,
        "/payload/price",
        "duplicate_field",
        Some("duplicate field `price`"),
    );
}

#[test]
fn diagnostic_decoder_rejects_incomplete_or_trailing_json() {
    assert_rejected_json::<Event>(r#"{"payload":"#, "", "invalid_json", None);

    let valid = &examples()["events"][0];
    let trailing_value = format!("{valid} {{}}");
    assert_rejected_json::<Event>(&trailing_value, "", "invalid_json", None);

    let cursor = format!(r#""cursor":{}"#, valid["cursor"]);
    let valid = valid.to_string();
    let unpaired_surrogate = valid.replacen(&cursor, r#""cursor":"\ud800""#, 1);
    assert_rejected_json::<Event>(&unpaired_surrogate, "/cursor", "invalid_value", None);
    let malformed_escape = valid.replacen(&cursor, r#""cursor":"\ud8xx""#, 1);
    assert_rejected_json::<Event>(&malformed_escape, "", "invalid_json", None);
}

#[test]
fn diagnostic_decoder_handles_extreme_numbers_and_invalid_constants() {
    let examples = examples();
    let event = &examples["events"][0];
    let quantity = format!(r#""quantity":{}"#, event["payload"]["quantity"]);
    let event = event.to_string();
    let long_integer = "9".repeat(4301);
    for number in [
        "1e1000000000000000000",
        concat!("1e-", "1000000000000000000"),
        &long_integer,
    ] {
        let text = event.replacen(&quantity, &format!(r#""quantity":{number}"#), 1);
        assert_rejected_json::<Event>(&text, "/payload/quantity", "invalid_type", None);
    }

    for constant in ["NaN", "Infinity", "-Infinity"] {
        let text = event.replacen(&quantity, &format!(r#""quantity":{constant}"#), 1);
        assert_rejected_json::<Event>(&text, "", "invalid_json", None);
    }

    let capability = &examples["capabilities"][2];
    let depth = format!(r#""max_depth":{}"#, capability["max_depth"]);
    let capability = capability.to_string();
    for number in [&long_integer, &format!("-{long_integer}")] {
        let text = capability.replacen(&depth, &format!(r#""max_depth":{number}"#), 1);
        assert_rejected_json::<Capability>(&text, "/max_depth", "out_of_range", None);
    }
}

#[test]
fn unit_tags_require_json_strings() {
    let examples = examples();

    let mut trade = examples["events"][0].clone();
    trade["payload"]["side"] = json!({"buy": null});
    assert_rejected::<Event>(&trade, "/payload/side", "invalid_type", "expected a string");

    let mut instrument = examples["instruments"][0].clone();
    instrument["asset_kind"] = json!({"etf": null});
    assert_rejected::<Instrument>(
        &instrument,
        "/asset_kind",
        "invalid_type",
        "expected a string",
    );

    let mut capability = examples["capabilities"][0].clone();
    capability["message_kinds"] = json!([{"trade": null}]);
    assert_rejected::<Capability>(
        &capability,
        "/message_kinds/0",
        "invalid_type",
        "expected a string",
    );

    let mut capability = examples["capabilities"][0].clone();
    capability["schema_version"] = json!({"1.0": null});
    assert_rejected::<Capability>(
        &capability,
        "/schema_version",
        "invalid_type",
        "expected a string",
    );
}

#[test]
fn tagged_payload_discriminants_require_json_strings() {
    let examples = examples();

    let mut trade = examples["events"][0].clone();
    trade["kind"] = json!({"trade": null});
    assert_rejected::<Event>(&trade, "/kind", "invalid_type", "expected a string");

    let mut correction = examples["events"][2].clone();
    correction["payload"]["replacement"]["kind"] = json!({"trade": null});
    assert_rejected::<Event>(
        &correction,
        "/payload/replacement/kind",
        "invalid_type",
        "expected a string",
    );

    let duplicate_kind = examples["events"][0].to_string().replace(
        "\"kind\":\"trade\"",
        "\"kind\":\"trade\",\"kind\":\"trade\"",
    );
    assert_rejected_json::<Event>(
        &duplicate_kind,
        "/kind",
        "duplicate_field",
        Some("duplicate field `kind`"),
    );
}

#[test]
fn named_payloads_and_records_require_json_objects() {
    let examples = examples();
    let mut trade = examples["events"][0].clone();
    trade["payload"] = json!(["663.1200", "4", "buy"]);
    assert_rejected::<Event>(&trade, "/payload", "invalid_type", "JSON object");

    let mut heartbeat = examples["events"][6].clone();
    heartbeat["payload"] = json!([]);
    assert_rejected::<Event>(&heartbeat, "/payload", "invalid_type", "JSON object");

    let mut correction = examples["events"][2].clone();
    correction["payload"]["replacement"] =
        json!(["trade", correction["payload"]["replacement"]["payload"]]);
    assert_rejected::<Event>(
        &correction,
        "/payload/replacement",
        "invalid_type",
        "JSON object",
    );

    let mut snapshot = examples["events"][9].clone();
    snapshot["payload"]["asks"][0]["quantity"] = json!(false);
    assert_rejected::<Event>(
        &snapshot,
        "/payload/asks/0/quantity",
        "invalid_type",
        "expected a string",
    );
    snapshot["payload"]["asks"][0] = json!(["101012.2", "0.008"]);
    assert_rejected::<Event>(&snapshot, "/payload/asks/0", "invalid_type", "JSON object");

    let capability = json!([
        "1.0",
        "fixture",
        "us-etf-delayed",
        ["trade"],
        0,
        true,
        "delayed"
    ]);
    assert_rejected::<Capability>(&capability, "", "invalid_type", "JSON object");
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
    assert_constraint(
        error,
        "/payload/price",
        "is not a multiple of instrument tick_size",
    );

    let mut trade = records[0].clone();
    trade["payload"]["quantity"] = json!("4.01");
    let error = decode::<Event>(&trade)
        .validate_against(&arca, &delayed)
        .unwrap_err();
    assert_constraint(
        error,
        "/payload/quantity",
        "is not a multiple of instrument lot_size",
    );

    let mut correction = records[2].clone();
    correction["payload"]["replacement"]["payload"]["price"] = json!("663.111");
    let error = decode::<Event>(&correction)
        .validate_against(&arca, &delayed)
        .unwrap_err();
    assert_constraint(
        error,
        "/payload/replacement/payload/price",
        "is not a multiple of instrument tick_size",
    );

    let mut bar = records[8].clone();
    bar["payload"]["open"] = json!("663.12345678901234001");
    let error = decode::<Event>(&bar)
        .validate_against(&research, &daily)
        .unwrap_err();
    assert_constraint(
        error,
        "/payload/open",
        "is not a multiple of instrument tick_size",
    );

    let mut snapshot = records[9].clone();
    snapshot["payload"]["bids"][0]["price"] = json!("101012.101");
    let error = decode::<Event>(&snapshot)
        .validate_against(&perp, &order_book)
        .unwrap_err();
    assert_constraint(
        error,
        "/payload/bids/0/price",
        "is not a multiple of instrument tick_size",
    );

    let mut delta = records[10].clone();
    delta["payload"]["updates"][1]["quantity"] = json!("0.0121");
    let error = decode::<Event>(&delta)
        .validate_against(&perp, &order_book)
        .unwrap_err();
    assert_constraint(
        error,
        "/payload/updates/1/quantity",
        "is not a multiple of instrument lot_size",
    );

    let mut shallow = capabilities[2].clone();
    shallow["max_depth"] = json!(1);
    let error = decode::<Event>(&records[9])
        .validate_against(&perp, &decode(&shallow))
        .unwrap_err();
    assert!(error.to_string().contains("depth"), "{error}");
    assert_constraint(
        error,
        "/payload/bids",
        "snapshot depth exceeds source/product max_depth",
    );

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
    assert_constraint(
        error,
        "/payload/price",
        "is not a multiple of instrument tick_size",
    );
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

    assert_constraint(
        first.validate_successor(&second).unwrap_err(),
        "/seq",
        "sequence regressed or repeated within the epoch",
    );
    assert_constraint(
        first.validate_successor(&first).unwrap_err(),
        "/seq",
        "sequence regressed or repeated within the epoch",
    );
    assert_constraint(
        old_epoch.validate_successor(&first).unwrap_err(),
        "/seq",
        "sequence gap within the epoch",
    );
    assert_constraint(
        after_reset.validate_successor(&old_epoch).unwrap_err(),
        "/epoch",
        "an epoch change requires a reset",
    );
    assert_constraint(
        old_epoch.validate_successor(&reset).unwrap_err(),
        "/epoch",
        "a reset must advance the epoch and an epoch cannot regress",
    );
    assert_constraint(
        reset.validate_successor(&reset).unwrap_err(),
        "/epoch",
        "a reset must advance the epoch and an epoch cannot regress",
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
    let error = bats.validate_successor(&first).unwrap_err();
    assert_eq!(error.field(), "stream");
    assert_constraint(error, "", "cannot compare positions from different streams");
}

#[test]
fn instrument_and_capability_require_valid_metadata() {
    let examples = examples();
    let mut instrument = examples["instruments"][0].clone();
    instrument["venue"] = json!("BATS");
    assert_rejected::<Instrument>(&instrument, "/venue", "constraint_violation", "venue");

    let mut instrument = examples["instruments"][0].clone();
    instrument["tick_size"] = json!("0.00");
    assert_rejected::<Instrument>(
        &instrument,
        "/tick_size",
        "constraint_violation",
        "tick_size",
    );
    instrument["tick_size"] = json!("1e-2");
    assert_rejected::<Instrument>(&instrument, "/tick_size", "invalid_decimal", "exponent");

    let mut instrument = examples["instruments"][3].clone();
    instrument
        .as_object_mut()
        .unwrap()
        .remove("settlement_currency");
    assert_rejected::<Instrument>(
        &instrument,
        "/settlement_currency",
        "missing_field",
        "missing field `settlement_currency`",
    );

    let mut capability = examples["capabilities"][0].clone();
    capability["message_kinds"] = json!(["trade", "trade"]);
    assert_rejected::<Capability>(
        &capability,
        "/message_kinds",
        "constraint_violation",
        "message_kinds",
    );

    let mut capability = examples["capabilities"][2].clone();
    capability["max_depth"] = json!(0);
    assert_rejected::<Capability>(
        &capability,
        "/max_depth",
        "constraint_violation",
        "max_depth",
    );
    capability["max_depth"] = json!(true);
    assert_rejected::<Capability>(&capability, "/max_depth", "invalid_type", "expected u32");
    capability["max_depth"] = json!(u64::from(u32::MAX) + 1);
    assert_rejected::<Capability>(&capability, "/max_depth", "out_of_range", "expected u32");

    let capability = &examples["capabilities"][2];
    let depth = format!(r#""max_depth":{}"#, capability["max_depth"]);
    let capability = capability.to_string();
    for (value, kind) in [
        ("18446744073709551616", "out_of_range"),
        ("-18446744073709551616", "out_of_range"),
        ("1e999", "invalid_type"),
    ] {
        let json = capability.replacen(&depth, &format!(r#""max_depth":{value}"#), 1);
        assert_rejected_json::<Capability>(&json, "/max_depth", kind, None);
    }
}

#[test]
fn perpetual_context_schema_exports_and_keeps_v1_kind_boundaries() {
    let directory = tempfile::tempdir().unwrap();
    assert!(
        Command::new(env!("CARGO_BIN_EXE_export-market-protocol"))
            .arg(directory.path())
            .arg("--v2")
            .status()
            .unwrap()
            .success()
    );
    let committed = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../protocol/v2");
    for name in [
        "event.schema.json",
        "capability.schema.json",
        "examples.json",
    ] {
        let exported = fs::read(directory.path().join(name)).unwrap();
        assert_eq!(exported, fs::read(committed.join(name)).unwrap(), "{name}");
        let document: Value = serde_json::from_slice(&exported).unwrap();
        assert_references_resolve(&document, &document);
    }
    let examples: Value =
        serde_json::from_str(include_str!("../../../protocol/v2/examples.json")).unwrap();
    assert_roundtrip::<Event>(&examples["events"]);
    assert_roundtrip::<Capability>(&examples["capabilities"]);
    let context = &examples["events"][0];
    let mut invalid = context.clone();
    invalid["schema_version"] = json!("1.0");
    assert_rejected::<Event>(
        &invalid,
        "/schema_version",
        "invalid_value",
        "perpetual_context",
    );
    invalid = context.clone();
    invalid["payload"]["open_interest"] = json!("-1");
    assert_rejected::<Event>(
        &invalid,
        "/payload/open_interest",
        "constraint_violation",
        "nonnegative",
    );
    invalid = context.clone();
    invalid["payload"]["mark_price"] = json!(123.5);
    assert_rejected::<Event>(
        &invalid,
        "/payload/mark_price",
        "invalid_type",
        "expected a string",
    );
}

#[test]
fn option_history_exports_round_trip_and_enforce_batch_and_version() {
    let directory = tempfile::tempdir().unwrap();
    assert!(
        Command::new(env!("CARGO_BIN_EXE_export-market-protocol"))
            .arg(directory.path())
            .arg("--v3")
            .status()
            .unwrap()
            .success()
    );
    let committed = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../protocol/v3");
    for name in [
        "event.schema.json",
        "capability.schema.json",
        "examples.json",
    ] {
        let exported = fs::read(directory.path().join(name)).unwrap();
        assert_eq!(exported, fs::read(committed.join(name)).unwrap(), "{name}");
        let document: Value = serde_json::from_slice(&exported).unwrap();
        assert_references_resolve(&document, &document);
    }
    let examples: Value =
        serde_json::from_str(include_str!("../../../protocol/v3/examples.json")).unwrap();
    assert_roundtrip::<Event>(&examples["events"]);
    assert_roundtrip::<Capability>(&examples["capabilities"]);
    let mut trade = examples["events"][1].clone();
    trade["schema_version"] = json!("1.0");
    assert_rejected::<Event>(&trade, "/schema_version", "invalid_value", "option history");
    trade["schema_version"] = json!("3.0");
    trade["payload"]["batch"]["occurrence"] = json!("0");
    assert_rejected::<Event>(
        &trade,
        "/payload/batch/occurrence",
        "constraint_violation",
        "greater than zero",
    );
}
