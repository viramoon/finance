use axum::{
    body::to_bytes,
    http::header::CONTENT_TYPE,
    response::{IntoResponse, Sse},
};
use finance_api::read_stream::{self, StatusSseMessage};
use finance_market_protocol::{Event, decode_json};
use futures_util::stream;
use serde::Deserialize;
use serde_json::{Value, from_value, json};
use std::{convert::Infallible, fs, path::PathBuf};

fn protocol_directory() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../protocol/v1")
}

fn protocol_json(file: &str) -> Value {
    serde_json::from_slice(&fs::read(protocol_directory().join(file)).unwrap()).unwrap()
}

#[test]
fn read_stream_export_is_deterministic_and_matches_tracked_files() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    read_stream::export(first.path()).unwrap();
    read_stream::export(second.path()).unwrap();
    for name in [
        "market-ws.schema.json",
        "market-status-sse.schema.json",
        "read-stream-examples.json",
    ] {
        let bytes = fs::read(first.path().join(name)).unwrap();
        assert_eq!(bytes, fs::read(second.path().join(name)).unwrap(), "{name}");
        assert_eq!(
            bytes,
            fs::read(protocol_directory().join(name)).unwrap(),
            "{name} drifted from its exporter"
        );
    }

    let ws = protocol_json("market-ws.schema.json");
    let mut f1 = protocol_json("event.schema.json");
    f1["$id"] = json!("urn:viramoon:finance:read-stream:1.0:market-ws");
    assert_eq!(ws, f1, "The WS payload must match the existing F1 event");
}

#[derive(Deserialize)]
struct ExistingWsReader {
    schema_version: String,
    kind: String,
    cursor: String,
    seq: String,
    license: String,
    payload: ExistingTrade,
}

#[derive(Deserialize)]
struct ExistingTrade {
    price: String,
    quantity: String,
}

#[test]
fn existing_f1_reader_keeps_market_ws_fields_and_decimal_lexemes() {
    let samples = protocol_json("read-stream-examples.json");
    let frames = samples["market_ws"].as_array().unwrap();
    let trade = frames
        .iter()
        .find(|frame| frame["kind"] == "trade")
        .unwrap();
    let reader: ExistingWsReader = from_value(trade.clone()).unwrap();
    assert_eq!(reader.schema_version, "1.0");
    assert_eq!(reader.kind, "trade");
    assert_eq!(reader.cursor, "fixture:arca:first");
    assert_eq!(reader.seq, "9007199254740992");
    assert_eq!(reader.license, "fixture.synthetic");
    assert_eq!(reader.payload.price, "663.1200");
    assert_eq!(reader.payload.quantity, "4");

    for frame in frames {
        let event = decode_json::<Event>(&serde_json::to_string(frame).unwrap()).unwrap();
        assert_eq!(*frame, serde_json::to_value(event).unwrap());
    }
}

#[test]
fn market_ws_preserves_max_u64_and_f1_snapshot_continuation() {
    let samples = protocol_json("read-stream-examples.json");
    let frames = samples["market_ws"].as_array().unwrap();
    let frame = |kind| frames.iter().find(|frame| frame["kind"] == kind).unwrap();
    let snapshot: Event = from_value(frame("snapshot").clone()).unwrap();
    let delta: Event = from_value(frame("delta").clone()).unwrap();
    assert_eq!(delta.seq.get(), u64::MAX);
    delta.validate_successor(&snapshot).unwrap();

    let mut bad = frame("delta").clone();
    bad["seq"] = json!(u64::MAX);
    assert!(from_value::<Event>(bad.clone()).is_err());
    bad["seq"] = json!("18446744073709551616");
    assert!(from_value::<Event>(bad).is_err());
}

#[derive(Deserialize)]
struct ExistingStatusReader {
    state: String,
    source: Option<String>,
    instruments: Vec<ExistingInstrument>,
}

#[derive(Deserialize)]
struct ExistingInstrument {
    instrument: String,
    stale: bool,
}

#[test]
fn sse_status_frames_keep_existing_status_fields_and_resume_id() {
    let samples = protocol_json("read-stream-examples.json");
    let frames = samples["market_status_sse"].as_array().unwrap();
    for frame in frames {
        assert_eq!(frame["event"], "market_status");
        assert_eq!(frame["data"]["schema_version"], "1.0");
        let decoded: StatusSseMessage = from_value(frame.clone()).unwrap();
        assert_eq!(*frame, serde_json::to_value(&decoded).unwrap());
        assert!(decoded.to_sse_event().is_ok());
    }

    let ready: ExistingStatusReader = from_value(frames[0]["data"]["status"].clone()).unwrap();
    assert_eq!(ready.state, "ready");
    assert_eq!(ready.source.as_deref(), Some("replay"));
    assert_eq!(ready.instruments[0].instrument, "SYNTH.XNYS");
    assert!(!ready.instruments[0].stale);

    let missing: ExistingStatusReader = from_value(frames[1]["data"]["status"].clone()).unwrap();
    assert_eq!(missing.state, "missing");
    assert_eq!(missing.source, None);
    assert!(missing.instruments.is_empty());

    assert_eq!(frames[1]["id"], "18446744073709551615");
    let mut bad = frames[1].clone();
    bad["unknown"] = json!(true);
    assert!(from_value::<StatusSseMessage>(bad).is_err());
    let mut bad = frames[1].clone();
    bad["id"] = json!(u64::MAX);
    assert!(from_value::<StatusSseMessage>(bad.clone()).is_err());
    bad["id"] = frames[1]["id"].clone();
    bad["data"]["schema_version"] = json!("2.0");
    assert!(from_value::<StatusSseMessage>(bad).is_err());
}

#[tokio::test]
async fn sse_status_http_body_matches_the_compatibility_samples() {
    let samples = protocol_json("read-stream-examples.json");
    let expected_frames = samples["market_status_sse"].as_array().unwrap();
    let events: Vec<_> = expected_frames
        .iter()
        .map(|frame| {
            let message: StatusSseMessage = from_value(frame.clone()).unwrap();
            Ok::<_, Infallible>(message.to_sse_event().unwrap())
        })
        .collect();
    let response = Sse::new(stream::iter(events)).into_response();
    assert_eq!(response.headers()[CONTENT_TYPE], "text/event-stream");
    let body = to_bytes(response.into_body(), 8192).await.unwrap();
    let wire = std::str::from_utf8(&body).unwrap();
    let encoded_frames: Vec<_> = wire
        .strip_suffix("\n\n")
        .expect("SSE stream must end with a blank line")
        .split("\n\n")
        .collect();
    assert_eq!(encoded_frames.len(), expected_frames.len());

    for (encoded, expected) in encoded_frames.into_iter().zip(expected_frames) {
        let fields: Vec<_> = encoded
            .lines()
            .map(|line| line.split_once(':').expect("SSE field without a colon"))
            .collect();
        assert_eq!(fields.len(), 3, "expected event, id and data fields");
        let field = |name| {
            let (_, value) = fields
                .iter()
                .find(|(key, _)| *key == name)
                .unwrap_or_else(|| panic!("missing SSE {name} field"));
            value.strip_prefix(' ').unwrap_or(value)
        };
        assert_eq!(field("event"), expected["event"].as_str().unwrap());
        assert_eq!(field("id"), expected["id"].as_str().unwrap());
        let data: Value = serde_json::from_str(field("data")).unwrap();
        assert_eq!(data, expected["data"]);
    }
}
