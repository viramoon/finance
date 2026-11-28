use finance_api::read_stream;
use finance_market_protocol::{DecimalText, U64Text};
use serde::Deserialize;
use serde_json::{Value, from_value, json};
use std::{fs, path::PathBuf};

const WS_SCHEMA: &str = "option-flow-ws.schema.json";
const SSE_SCHEMA: &str = "option-flow-sse.schema.json";
const EXAMPLES: &str = "option-flow-stream-examples.json";

fn protocol_directory() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../protocol/v1")
}

fn protocol_json(name: &str) -> Value {
    serde_json::from_slice(&fs::read(protocol_directory().join(name)).unwrap()).unwrap()
}

#[test]
fn option_flow_stream_export_is_deterministic_and_matches_tracked_files() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    read_stream::export(first.path()).unwrap();
    read_stream::export(second.path()).unwrap();
    for name in [WS_SCHEMA, SSE_SCHEMA, EXAMPLES] {
        let bytes = fs::read(first.path().join(name)).unwrap();
        assert_eq!(bytes, fs::read(second.path().join(name)).unwrap(), "{name}");
        assert_eq!(
            bytes,
            fs::read(protocol_directory().join(name)).unwrap(),
            "{name} drifted from its exporter"
        );
    }
}

#[test]
fn option_flow_schemas_share_closed_versioned_envelopes() {
    let ws = protocol_json(WS_SCHEMA);
    let mut sse = protocol_json(SSE_SCHEMA);
    assert_eq!(
        ws["$id"],
        "urn:viramoon:finance:read-stream:1.0:option-flow-ws"
    );
    assert_eq!(
        sse["$id"],
        "urn:viramoon:finance:read-stream:1.0:option-flow-sse"
    );
    sse["$id"] = ws["$id"].clone();
    sse.as_object_mut().unwrap().remove("description");
    assert_eq!(ws, sse);
    assert_eq!(ws["unevaluatedProperties"], false);
    assert_eq!(ws["$defs"]["OptionFlowVersion"]["enum"], json!(["1.0"]));

    let variants = ws["oneOf"].as_array().unwrap();
    assert_eq!(variants.len(), 3);
    let variant = |event| {
        variants
            .iter()
            .find(|branch| branch["properties"]["event"]["enum"][0] == event)
            .unwrap()
    };
    let observation = variant("observation");
    let status = variant("status");
    let reset = variant("reset_required");
    assert_eq!(observation["properties"]["id"]["type"], "string");
    assert!(
        observation["required"]
            .as_array()
            .unwrap()
            .contains(&json!("id"))
    );
    assert_eq!(status["properties"]["id"]["type"], "string");
    assert!(
        !status["required"]
            .as_array()
            .unwrap()
            .contains(&json!("id"))
    );
    assert_eq!(
        status["if"]["properties"]["data"]["properties"]["state"]["const"],
        "live"
    );
    assert_eq!(status["then"]["required"], json!(["id"]));
    assert_eq!(status["else"]["not"]["required"], json!(["id"]));
    assert!(reset["properties"].get("id").is_none());

    let definitions = &ws["$defs"];
    let observation = &definitions["OptionFlowObservationData"]["properties"];
    assert_eq!(observation["seq"]["$ref"], "#/$defs/U64Text");
    assert_eq!(observation["access"]["$ref"], "#/$defs/ProductPolicy");
    assert_eq!(
        observation["observation"]["$ref"],
        "#/$defs/OptionFlowObservation"
    );
    assert_eq!(
        definitions["FlowObservation"]["properties"]["generation"]["$ref"],
        "#/$defs/U64Text"
    );
    assert_eq!(
        definitions["L1Sample"]["properties"]["value"]["$ref"],
        "#/$defs/DecimalText"
    );
    assert_eq!(definitions["U64Text"]["type"], "string");
    assert_eq!(definitions["DecimalText"]["type"], "string");
    assert_eq!(
        definitions["OptionFlowResetData"]["properties"]["snapshot"]["const"],
        "/api/v1/options/flow/snapshot"
    );
}

#[derive(Deserialize)]
struct ConsumerObservationFrame {
    id: String,
    data: ConsumerObservation,
}

#[derive(Deserialize)]
struct ConsumerObservation {
    schema_version: String,
    seq: U64Text,
    observation: ConsumerItem,
}

#[derive(Deserialize)]
struct ConsumerItem {
    clue: ConsumerClue,
}

#[derive(Deserialize)]
struct ConsumerClue {
    generation: U64Text,
    observation: ConsumerEvidence,
}

#[derive(Deserialize)]
struct ConsumerEvidence {
    evidence: ConsumerVolume,
}

#[derive(Deserialize)]
struct ConsumerVolume {
    delta_contracts: DecimalText,
    sampled_last_price: ConsumerSample,
    sampled_last_size: ConsumerSample,
}

#[derive(Deserialize)]
struct ConsumerSample {
    value: DecimalText,
}

#[test]
fn option_flow_examples_cover_replay_reordering_and_exact_values() {
    let samples = protocol_json(EXAMPLES);
    assert_eq!(samples["live"][0]["event"], "status");
    assert_eq!(samples["live"][0]["data"]["state"], "live");
    assert!(samples["live"][0]["id"].is_string());

    let first = &samples["live"][1];
    let second = &samples["live"][2];
    let reader = |value: &Value| from_value::<ConsumerObservationFrame>(value.clone()).unwrap();
    let first_read = reader(first);
    let second_read = reader(second);
    assert_eq!(first["event"], "observation");
    assert_eq!(first_read.data.schema_version, "1.0");
    assert_ne!(first_read.id, second_read.id);
    assert_eq!(first_read.data.seq.get(), u64::MAX - 1);
    assert_eq!(second_read.data.seq.get(), u64::MAX);
    assert_eq!(first_read.data.observation.clue.generation.get(), u64::MAX);
    let volume = first_read.data.observation.clue.observation.evidence;
    assert_eq!(volume.delta_contracts.as_str(), "9007199254740993");
    assert_eq!(
        volume.sampled_last_price.value.as_str(),
        "1.000000000000000001"
    );
    assert_eq!(volume.sampled_last_size.value.as_str(), "2.1250");
    assert!(
        second["data"]["observation"]["clue"]["observed_at"]
            .as_str()
            .unwrap()
            < first["data"]["observation"]["clue"]["observed_at"]
                .as_str()
                .unwrap()
    );
    let mut inexact = second.clone();
    inexact["data"]["seq"] = json!(u64::MAX);
    assert!(from_value::<ConsumerObservationFrame>(inexact).is_err());
    let mut inexact = second.clone();
    inexact["data"]["observation"]["clue"]["observation"]["evidence"]["sampled_last_price"]["value"] =
        json!(1);
    assert!(from_value::<ConsumerObservationFrame>(inexact).is_err());

    assert_eq!(samples["replayed"][0]["id"], first["id"]);
    assert_eq!(&samples["replayed"][1], second);
    assert_eq!(samples["out_of_order"], json!([second, first, second]));
    assert_eq!(
        first["data"]["access"],
        json!({
            "source": "ibkr",
            "product": "opra",
            "license": "fixture.synthetic",
            "display_live": true,
            "record_local": false,
            "derive_non_display": false,
            "backup_personal_drive": false,
        })
    );
    assert_eq!(
        first["data"]["observation"]["coverage"]["access"],
        first["data"]["access"]
    );
    assert_eq!(first["data"]["observation"]["coverage"]["con_id"], 42);
    assert!(first["data"]["observation"]["coverage"]["uncovered"][0].is_object());
    assert!(second["data"]["observation"]["coverage"].is_null());

    let disabled = &samples["disabled"][0];
    assert_eq!(disabled["event"], "status");
    assert_eq!(disabled["data"]["state"], "disabled");
    assert!(disabled.get("id").is_none());

    let expired = &samples["expired"][0];
    assert_eq!(expired["event"], "reset_required");
    assert_eq!(expired["data"]["reason"], "cursor_unavailable");
    assert_eq!(expired["data"]["snapshot"], "/api/v1/options/flow/snapshot");
    assert!(expired.get("id").is_none());
}
