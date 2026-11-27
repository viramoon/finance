use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header::CACHE_CONTROL},
};
use chrono::{DateTime, Duration, Utc};
use finance_api::{AppState, router};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{collections::BTreeSet, fs, path::Path, process::Command};
use tower::ServiceExt;

#[test]
fn backtest_list_openapi_adds_optional_legacy_visibility_and_keeps_int64_cursor() {
    use finance_api::ApiDoc;
    use utoipa::OpenApi;

    let spec = serde_json::to_value(ApiDoc::openapi()).unwrap();
    let list = &spec["paths"]["/api/v1/backtests"]["get"];
    let parameters = list["parameters"].as_array().unwrap();
    let parameter = |name| parameters.iter().find(|p| p["name"] == name).unwrap();
    assert_eq!(parameter("cursor")["required"], false);
    assert_eq!(
        parameter("cursor")["schema"],
        json!({"type": "integer", "format": "int64"})
    );
    let visibility = parameter("visibility");
    assert_eq!(visibility["in"], "query");
    assert_eq!(visibility["required"], false);
    assert_eq!(
        visibility["schema"],
        json!({"$ref": "#/components/schemas/BacktestVisibility"})
    );
    assert_eq!(
        spec["components"]["schemas"]["BacktestVisibility"],
        json!({"type": "string", "enum": ["legacy"]})
    );
    assert_eq!(
        list["responses"]["200"]["content"]["application/json"]["schema"],
        json!({"$ref": "#/components/schemas/Page"})
    );
    assert_eq!(
        spec["components"]["schemas"]["Page"]["properties"]["next_cursor"],
        json!({"type": ["integer", "null"], "format": "int64"})
    );
}

// Pre-F10a consumers use their own types so changes to the producer's DTOs
// cannot silently change what this client considers a compatible response.
#[derive(Deserialize)]
struct LegacyMarketStatus {
    state: String,
    source: Option<String>,
    updated_at: Option<DateTime<Utc>>,
    max_age_seconds: Option<u32>,
    instruments: Vec<LegacyInstrumentStatus>,
}

#[derive(Deserialize)]
struct LegacyInstrumentStatus {
    instrument: String,
    last_bar_at: Option<DateTime<Utc>>,
    age_seconds: Option<i64>,
    stale: bool,
}

#[derive(Deserialize)]
struct LegacySample {
    run_id: String,
    strategy: String,
    instrument: String,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    total_orders: u64,
    total_positions: u64,
    pnl: String,
    currency: String,
    generated_at: DateTime<Utc>,
}

fn app(root: &Path) -> Router {
    router(AppState {
        data_root: root.into(),
        research_root: root.join("research"),
    })
}

async fn get(router: Router, path: &str) -> axum::response::Response {
    router
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn read<T: DeserializeOwned>(router: Router, path: &str) -> T {
    let response = get(router, path).await;
    assert_eq!(response.status(), StatusCode::OK);
    serde_json::from_slice(&to_bytes(response.into_body(), 8192).await.unwrap()).unwrap()
}

fn write_json(root: &Path, path: &str, value: Value) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
}

#[tokio::test]
async fn legacy_market_status_and_sample_clients_decode_existing_fields() {
    let data = tempfile::tempdir().unwrap();
    let now = Utc::now();
    let start = now - Duration::hours(2);
    let end = now - Duration::hours(1);
    let route = app(data.path());
    write_json(
        data.path(),
        "finance-state/collector/status.json",
        json!({
            "state": "ready", "source": "replay", "updated_at": now, "max_age_seconds": 120,
            "instruments": [{"instrument": "SYNTH.XNYS", "last_bar_at": now,
                             "age_seconds": 0, "stale": false}]
        }),
    );
    write_json(
        data.path(),
        "finance-state/backtests/sample/latest.json",
        json!({
            "run_id": "synthetic-1", "strategy": "DemoStrategy", "instrument": "SYNTH.XNYS",
            "start": start, "end": end, "total_orders": 0, "total_positions": 0,
            "pnl": "12345678901234567890.12345678", "currency": "USD", "generated_at": now
        }),
    );

    let status: LegacyMarketStatus = read(route.clone(), "/api/v1/market/status").await;
    assert_eq!(status.state, "ready");
    assert_eq!(status.source.as_deref(), Some("replay"));
    assert_eq!(status.updated_at, Some(now));
    assert_eq!(status.max_age_seconds, Some(120));
    assert_eq!(status.instruments.len(), 1);
    let instrument = &status.instruments[0];
    assert_eq!(instrument.instrument, "SYNTH.XNYS");
    assert_eq!(instrument.last_bar_at, Some(now));
    assert!(instrument.age_seconds.is_some());
    assert!(!instrument.stale);

    let sample: LegacySample = read(route, "/api/v1/backtests/sample").await;
    assert_eq!(sample.run_id, "synthetic-1");
    assert_eq!(sample.strategy, "DemoStrategy");
    assert_eq!(sample.instrument, "SYNTH.XNYS");
    assert_eq!(sample.start, start);
    assert_eq!(sample.end, end);
    assert_eq!((sample.total_orders, sample.total_positions), (0, 0));
    assert_eq!(sample.pnl, "12345678901234567890.12345678");
    assert_eq!(sample.currency, "USD");
    assert_eq!(sample.generated_at, now);
}

#[tokio::test]
async fn restricted_read_responses_are_not_cacheable() {
    let data = tempfile::tempdir().unwrap();
    let router = app(data.path());
    for (path, successful) in [
        ("/api/v1/market/products", true),
        ("/api/v1/factors", true),
        ("/api/v1/market/wal", false),
    ] {
        let response = get(router.clone(), path).await;
        assert_eq!(response.status().is_success(), successful);
        assert_eq!(response.headers()[CACHE_CONTROL], "private, no-store");
    }
}

#[test]
fn openapi_cli_is_deterministic_and_resolves_read_contract_refs() {
    fn export() -> Vec<u8> {
        let result = Command::new(env!("CARGO_BIN_EXE_finance-api"))
            .arg("export-openapi")
            .output()
            .unwrap();
        assert!(result.status.success());
        result.stdout
    }

    fn check_refs(node: &Value, spec: &Value) {
        match node {
            Value::Object(fields) => {
                if let Some(reference) = fields.get("$ref").and_then(Value::as_str) {
                    let location = reference
                        .strip_prefix('#')
                        .expect("local OpenAPI reference");
                    assert!(spec.pointer(location).is_some(), "unresolved {reference}");
                }
                for value in fields.values() {
                    check_refs(value, spec);
                }
            }
            Value::Array(items) => items.iter().for_each(|value| check_refs(value, spec)),
            _ => {}
        }
    }

    let first = export();
    assert_eq!(first, export());
    let spec: Value = serde_json::from_slice(&first).unwrap();
    assert_eq!(spec["info"]["version"], "0.0.0");
    check_refs(&spec, &spec);
    let mut ids = BTreeSet::new();
    for path in spec["paths"].as_object().unwrap().values() {
        for operation in path.as_object().unwrap().values() {
            let id = operation["operationId"].as_str().unwrap();
            assert!(ids.insert(id), "duplicate OpenAPI operation ID: {id}");
        }
    }
    for path in [
        "/healthz/live",
        "/healthz/ready",
        "/api/v1/backtests",
        "/api/v1/backtests/{id}",
        "/api/v1/backtests/by-key",
        "/api/v1/backtests/datasets",
        "/api/v1/backtests/sample",
        "/api/v1/market/status",
    ] {
        assert!(spec["paths"][path]["get"].is_object(), "{path}");
    }
    assert!(spec["paths"]["/api/v1/backtests"]["post"].is_object());
    for path in [
        "/api/v1/market/products",
        "/api/v1/market/wal",
        "/api/v1/market/watermarks",
        "/api/v1/market/projection/latest",
        "/api/v1/quality/gaps",
        "/api/v1/factors",
        "/api/v1/factors/{id}/evaluations",
    ] {
        let operations = spec["paths"][path].as_object().expect(path);
        assert_eq!(operations.len(), 1, "{path}");
        assert!(operations["get"]["responses"]["200"].is_object(), "{path}");
    }
    let schemas = &spec["components"]["schemas"];
    assert_eq!(schemas["DecimalText"]["type"], "string");
    assert_eq!(schemas["U64Text"]["type"], "string");
    assert_eq!(schemas["Event"]["unevaluatedProperties"], false);
    assert_eq!(schemas["MarketMessage"]["unevaluatedProperties"], false);
    assert_eq!(
        schemas["SchemaVersion"]["enum"],
        json!(["1.0", "2.0", "3.0"])
    );
    let event_version = schemas["Event"]["allOf"]
        .as_array()
        .unwrap()
        .last()
        .unwrap();
    assert_eq!(
        event_version["oneOf"][0]["properties"],
        json!({
            "schema_version": {"const": "1.0"},
            "kind": {"not": {"enum": [
                "perpetual_context", "funding_rate", "open_interest",
                "option_history_trade", "option_history_quote"
            ]}}
        })
    );
    assert_eq!(
        event_version["oneOf"][1]["properties"],
        json!({
            "schema_version": {"const": "2.0"},
            "kind": {"enum": [
                "perpetual_context", "funding_rate", "open_interest", "gap", "reset", "heartbeat", "status"
            ]},
            "asset_kind": {"const": "perpetual"},
            "revision": {"const": "0"}
        })
    );
    assert_eq!(
        event_version["oneOf"][2]["properties"],
        json!({
            "schema_version": {"const": "3.0"},
            "kind": {"enum": ["option_history_trade", "option_history_quote"]},
            "asset_kind": {"const": "option"},
            "revision": {"const": "0"},
            "exchange_ts": {"$ref": "#/components/schemas/U64Text"}
        })
    );
}

#[test]
fn batch_two_openapi_exposes_new_operations_and_preserves_legacy_types() {
    use finance_api::ApiDoc;
    use utoipa::OpenApi;

    let spec = serde_json::to_value(ApiDoc::openapi()).unwrap();
    let paths = &spec["paths"];
    for (path, method, schema) in [
        ("/api/v1/strategies/validate", "post", "StrategyPreview"),
        ("/api/v1/strategies", "get", "StrategyVersionPage"),
        (
            "/api/v1/strategies/{id}/versions/{version}",
            "get",
            "StrategyPreview",
        ),
        ("/api/v1/backtests/{id}/cancel", "post", "Job"),
        ("/api/v1/backtests/worker-status", "get", "WorkerStatus"),
        ("/api/v1/decisions/signals", "get", "DecisionSignalPage"),
        (
            "/api/v1/factors/{id}/evaluations/{evaluation_id}",
            "get",
            "FactorEvaluationDetail",
        ),
    ] {
        assert_eq!(
            paths[path][method]["responses"]["200"]["content"]["application/json"]["schema"]["$ref"],
            format!("#/components/schemas/{schema}"),
        );
    }
    let schemas = &spec["components"]["schemas"];
    assert_eq!(schemas["Accepted"]["properties"]["id"]["type"], "integer");
    assert_eq!(
        schemas["JobStatus"]["enum"],
        json!(["queued", "running", "succeeded", "failed", "timed_out"])
    );
    assert_eq!(
        schemas["BacktestSummary"]["properties"]["pnl"]["type"],
        "string"
    );
    assert_eq!(
        schemas["SmaSignal"]["properties"]["seq"]["$ref"],
        "#/components/schemas/U64Text"
    );
    assert_eq!(
        schemas["BundleSignal"]["properties"]["target_shares"]["$ref"],
        "#/components/schemas/DecimalText"
    );
    assert_eq!(
        schemas["StrategyPreview"]["properties"]["plan"]["$ref"],
        "#/components/schemas/CompiledPlan"
    );
}
