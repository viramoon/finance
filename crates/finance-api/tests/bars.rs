use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header::CACHE_CONTROL},
};
use chrono::{DateTime, TimeDelta, Utc};
use finance_api::{ApiDoc, AppState, router};
use serde_json::{Value, json};
use std::{fmt::Write, fs, path::Path};
use tower::ServiceExt;
use utoipa::OpenApi;

const CHART: &str = "/api/v1/market/bars";
const VALUES: &str = "/api/v1/indicators/values";
const SCOPE: &str =
    "instrument_id=MOCK.XNYS&source=synthetic&product=test-bars&timeframe=1m&adjustment=raw";
const WINDOW: &str = "start=2024-01-02T14:30:00Z&end=2024-01-02T14:34:00Z";
const PROJECTION: &str = "finance-state/bars/v1/synthetic/test-bars/MOCK.XNYS/raw/1m.json";
const EMA: &str = r#"[{"use":"indicator.ema@1.0.0","params":{"period":2}}]"#;

fn store(root: &Path, data: &Value) {
    let file = root.join(PROJECTION);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(file, serde_json::to_vec(data).unwrap()).unwrap();
}

fn projection() -> Value {
    // Deterministic synthetic progressions only. Price = 10 + 10*i, one minute
    // per row, with deliberately varied scales and an exact volume above 2^53.
    let rows: Vec<_> = ["10.0000", "20.00", "30.0000", "40.00"]
        .iter()
        .enumerate()
        .map(|(index, close)| {
            let minute = 30 + index;
            let available = if index == 3 {
                "33:12"
            } else {
                ["31:00", "32:00", "33:00"][index]
            };
            json!({
                "open_ts":format!("2024-01-02T14:{minute}:00Z"),
                "end_ts":format!("2024-01-02T14:{}:00Z", minute + 1),
                "first_available_ts":format!("2024-01-02T14:{available}Z"),
                "trading_day":"2024-01-02",
                "open":close, "high":close, "low":close, "close":close,
                "volume":"9007199254740993.000000000"
            })
        })
        .collect();
    json!({
        "schema_version":"1.0",
        "series": {
            "instrument_id":"MOCK.XNYS", "source":"synthetic", "product":"test-bars",
            "timeframe":"1m", "adjustment":"raw"
        },
        "provenance": {
            "license":"synthetic", "delay_tier":"replay", "calendar_id":"XNYS",
            "market_timezone":"America/New_York", "display":"authorized_display"
        },
        "as_of":"2024-01-02T14:33:30Z",
        "history_complete":true,
        "closed":rows[..3],
        "provisional":rows[3]
    })
}

fn app(root: &Path) -> Router {
    router(AppState {
        data_root: root.into(),
        research_root: root.join("research"),
    })
}

async fn call(app: &Router, uri: &str) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.headers()[CACHE_CONTROL], "private, no-store");
    (
        response.status(),
        serde_json::from_slice(&to_bytes(response.into_body(), 512 * 1024).await.unwrap()).unwrap(),
    )
}

fn encoded(text: &str) -> String {
    let mut output = String::new();
    for byte in text.bytes() {
        write!(output, "%{byte:02X}").unwrap();
    }
    output
}

#[tokio::test]
async fn chart_pages_replay_prefix_and_preserve_precision_and_status() {
    let dir = tempfile::tempdir().unwrap();
    store(dir.path(), &projection());
    let app = app(dir.path());
    let ema = encoded(EMA);
    let (status, result) = call(&app, &format!("{CHART}?{SCOPE}&{WINDOW}&indicators={ema}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["as_of"], "2024-01-02T14:33:30Z");
    assert_eq!(result["series"], projection()["series"]);
    assert_eq!(result["provenance"], projection()["provenance"]);
    assert_eq!(
        result["coverage"],
        json!({
            "history_complete": true,
            "first_closed_open_ts":"2024-01-02T14:30:00Z",
            "last_closed_end_ts":"2024-01-02T14:33:00Z",
            "closed_bars":3, "returned_closed_bars":3, "returned_provisional":false,
        })
    );
    assert_eq!(result["rows"].as_array().unwrap().len(), 3);
    assert_eq!(result["rows"][0]["bar"]["close"], "10.0000");
    assert_eq!(
        result["rows"][0]["bar"]["volume"],
        "9007199254740993.000000000"
    );
    assert_eq!(result["rows"][0]["provisional"], false);
    assert_eq!(
        result["rows"][0]["indicator_values"][0],
        json!({"status":"warmup","values":{}})
    );
    assert_eq!(
        result["rows"][1]["indicator_values"][0],
        json!({"status":"ready","values":{"value":"15"}})
    );
    assert_eq!(
        result["rows"][2]["indicator_values"][0]["values"]["value"],
        "25"
    );

    let visible = "start=2024-01-02T14:31:00Z&end=2024-01-02T14:33:00Z";
    let (status, page) = call(
        &app,
        &format!("{CHART}?{SCOPE}&{visible}&limit=1&include_provisional=true&indicators={ema}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["rows"], json!([result["rows"][1]]));
    assert_eq!(page["next_start"], "2024-01-02T14:32:00Z");
    let third = "start=2024-01-02T14:32:00Z&end=2024-01-02T14:34:00Z";
    let (status, page) = call(
        &app,
        &format!("{CHART}?{SCOPE}&{third}&limit=1&include_provisional=true&indicators={ema}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["rows"], json!([result["rows"][2]]));
    assert_eq!(page["next_start"], "2024-01-02T14:33:00Z");

    let fourth = "start=2024-01-02T14:33:00Z&end=2024-01-02T14:34:00Z";
    let (status, page) = call(
        &app,
        &format!("{CHART}?{SCOPE}&{fourth}&limit=1&include_provisional=true&indicators={ema}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["next_start"], Value::Null);
    assert_eq!(page["coverage"]["returned_provisional"], true);
    assert_eq!(page["rows"][0]["provisional"], true);
    assert_eq!(page["rows"][0]["bar"]["close"], "40.00");
    assert_eq!(
        page["rows"][0]["bar"]["first_available_ts"],
        "2024-01-02T14:33:12Z"
    );
    assert_eq!(page["rows"][0]["indicator_values"][0]["status"], "ready");
    assert_eq!(
        page["rows"][0]["indicator_values"][0]["values"]["value"],
        "35"
    );

    let (status, values) = call(
        &app,
        &format!("{VALUES}?{SCOPE}&{fourth}&include_provisional=true&indicators={ema}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        values["indicators"],
        serde_json::from_str::<Value>(EMA).unwrap()
    );
    assert_eq!(
        values["rows"][0]["indicator_values"],
        page["rows"][0]["indicator_values"]
    );
    assert_eq!(values["rows"][0]["provisional"], true);
    assert_eq!(
        values["rows"][0]["open_ts"],
        page["rows"][0]["bar"]["open_ts"]
    );
    assert!(values["rows"][0].get("bar").is_none());

    let (_, ended) = call(&app, &format!("{CHART}?{SCOPE}&{fourth}&indicators={ema}")).await;
    assert_eq!(ended["rows"], json!([]));
}

#[tokio::test]
async fn chart_indicators_propagate_overflow_from_visible_and_replayed_bars() {
    let dir = tempfile::tempdir().unwrap();
    let mut snapshot = projection();
    // Three synthetic prices. The second bar's one-bar return exceeds
    // Decimal's range; the third's would be 1% if the failed bar were accepted.
    let smallest_f1_price = "0.000000000000000000000000001";
    let closes = [smallest_f1_price, "100", "101"];
    for (bar, close) in snapshot["closed"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .zip(closes)
    {
        for field in ["open", "high", "low", "close"] {
            bar[field] = json!(close);
        }
    }
    store(dir.path(), &snapshot);
    let app = app(dir.path());
    let indicator = encoded(r#"[{"use":"indicator.close_return@1.0.0","params":{"period":1}}]"#);
    let (status, first) = call(
        &app,
        &format!("{CHART}?{SCOPE}&{WINDOW}&indicators={indicator}&limit=1"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first["next_start"], "2024-01-02T14:31:00Z");
    assert_eq!(first["rows"][0]["indicator_values"][0]["status"], "warmup");

    for start in ["14:31:00", "14:32:00", "14:33:00"] {
        let window = format!("start=2024-01-02T{start}Z&end=2024-01-02T14:34:00Z");
        for route in [CHART, VALUES] {
            let (status, error) = call(
                &app,
                &format!(
                    "{route}?{SCOPE}&{window}&indicators={indicator}&limit=1&include_provisional=true"
                ),
            )
            .await;
            assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{route} {start}");
            assert_eq!(
                error,
                json!({"error":{
                    "code":"indicator_calculation_failed",
                    "message":"Cannot calculate indicators for this projection"
                }})
            );
        }
    }
}

#[tokio::test]
async fn chart_preserves_finest_f1_price_and_rejects_overprecise_wire_price() {
    let dir = tempfile::tempdir().unwrap();
    let mut snapshot = projection();
    let inside = "0.000000000000000000000000001";
    for field in ["open", "high", "close"] {
        snapshot["closed"][0][field] = json!(inside);
    }
    // Both closes are within [low, high], so the negative case isolates wire precision.
    snapshot["closed"][0]["low"] = json!("0");
    store(dir.path(), &snapshot);
    let app = app(dir.path());
    let indicator = encoded(r#"[{"use":"indicator.ema@1.0.0","params":{"period":1}}]"#);
    let uri = format!("{CHART}?{SCOPE}&{WINDOW}&indicators={indicator}&limit=1");
    let (status, page) = call(&app, &uri).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["rows"][0]["bar"]["close"], inside);
    assert_eq!(
        page["rows"][0]["indicator_values"][0]["values"]["value"],
        "0"
    );

    snapshot["closed"][0]["close"] = json!("0.0000000000000000000000000001");
    store(dir.path(), &snapshot);
    // Check raw bars first, without any indicator input validation in this path.
    for query in [
        format!("{CHART}?{SCOPE}&{WINDOW}&limit=1"),
        uri,
        format!("{VALUES}?{SCOPE}&{WINDOW}&indicators={indicator}&limit=1"),
    ] {
        let (status, error) = call(&app, &query).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{query}");
        assert_eq!(
            error,
            json!({"error": {"code": "bars_invalid", "message": "Invalid bars projection"}}),
            "{query}"
        );
    }
}

#[tokio::test]
async fn chart_rejects_unapproved_mismatched_invalid_or_incomplete_indicator_input() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(dir.path());
    let uri = format!("{CHART}?{SCOPE}&{WINDOW}");
    let indicators = format!("{uri}&indicators={}", encoded(EMA));
    let missing = call(&app, &uri).await;
    assert_eq!(missing.0, StatusCode::NOT_FOUND);
    assert_eq!(missing.1["error"]["code"], "bars_not_found");

    let mut partial = projection();
    partial["history_complete"] = json!(false);
    store(dir.path(), &partial);
    let (status, result) = call(&app, &uri).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["coverage"]["history_complete"], false);
    assert_eq!(result["rows"][0]["indicator_values"], json!([]));
    let (status, result) = call(&app, &indicators).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(result["error"]["code"], "indicator_history_unavailable");
    assert_eq!(
        call(
            &app,
            &uri.replace("adjustment=raw", "adjustment=split_normalized")
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );

    for (pointer, value) in [
        ("/provenance/display", json!("synthetic-private-value")),
        ("/provenance/display", json!({"authorized_display": null})),
        ("/schema_version", json!({"1.0": null})),
        ("/provenance/market_timezone", json!("../escape")),
        ("/series/instrument_id", json!("MOCK2.XNYS")),
        ("/closed/0/volume", json!(9007199254740993u64)),
        (
            "/closed/0/first_available_ts",
            json!("2024-01-02T14:29:59Z"),
        ),
        (
            "/provisional/first_available_ts",
            json!("2024-01-02T14:33:31Z"),
        ),
        ("/provisional/open_ts", json!("2024-01-02T14:32:59Z")),
    ] {
        let mut invalid = projection();
        *invalid.pointer_mut(pointer).unwrap() = value;
        store(dir.path(), &invalid);
        let (status, body) = call(&app, &uri).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{pointer}");
        assert_eq!(
            body,
            json!({"error":{"code":"bars_invalid","message":"Invalid bars projection"}})
        );
    }
}

#[tokio::test]
async fn chart_validates_exact_series_time_range_and_indicators() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(dir.path());
    let uri = format!("{CHART}?{SCOPE}&{WINDOW}");
    for invalid in [
        uri.replace("MOCK.XNYS", "%2e%2e%2fMOCK.XNYS"),
        uri.replace("timeframe=1m", "timeframe=01m"),
        uri.replace("timeframe=1m", "timeframe=2d"),
        uri.replace("T14:34:00Z", "T14:30:00Z"),
        format!("{uri}&as_of=2024-01-02T14:33:30Z"),
        format!("{uri}&limit=1001"),
        format!("{uri}&limit=2&limit=3"),
        format!("{uri}&token={}", "a".repeat(8192)),
    ] {
        let (status, body) = call(&app, &invalid).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], "invalid_bars_query");
    }
    for request in [
        EMA.replace("1.0.0", "2.0.0"),
        EMA.replace(r#"{"period":2}"#, "7"),
        EMA.replace(r#"{"period":2}"#, r#"{"period":1,"period":2}"#),
        EMA.replace(":2", r#":{"$serde_json::private::Number":"2"}"#),
        EMA.replace(":2", ":2.0"),
        EMA.replace("period", "unknown"),
        EMA.replace(":2", ":0"),
        "not-json".into(),
    ] {
        let (status, body) = call(&app, &format!("{uri}&indicators={}", encoded(&request))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{request}");
        assert_eq!(body["error"]["code"], "invalid_indicator");
    }
    let (status, body) = call(&app, &format!("{VALUES}?{SCOPE}&{WINDOW}")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid_indicator");
}

#[cfg(unix)]
#[tokio::test]
async fn local_projection_reader_rejects_symlinks_and_nonregular_or_oversized_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(PROJECTION);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let other = dir.path().join("other-source.json");
    fs::write(&other, serde_json::to_vec(&projection()).unwrap()).unwrap();
    std::os::unix::fs::symlink(&other, &path).unwrap();
    let app = app(dir.path());
    let uri = format!("{CHART}?{SCOPE}&{WINDOW}");
    assert_eq!(call(&app, &uri).await.1["error"]["code"], "bars_invalid");
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    assert_eq!(call(&app, &uri).await.1["error"]["code"], "bars_invalid");
    fs::remove_dir(&path).unwrap();
    fs::File::create(&path)
        .unwrap()
        .set_len(32 * 1024 * 1024 + 1)
        .unwrap();
    assert_eq!(call(&app, &uri).await.1["error"]["code"], "bars_invalid");
}

#[tokio::test]
async fn continuous_bars_may_end_at_utc_midnight_but_cannot_cross_it() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(dir.path());
    let mut snapshot = projection();
    snapshot["provenance"]["calendar_id"] = json!("24X7");
    snapshot["provenance"]["market_timezone"] = json!("UTC");
    let mut bar = snapshot["closed"][0].clone();
    bar["open_ts"] = json!("2024-01-02T23:59:00Z");
    bar["end_ts"] = json!("2024-01-03T00:00:00Z");
    bar["first_available_ts"] = json!("2024-01-03T00:00:01Z");
    snapshot["closed"] = json!([bar]);
    snapshot["provisional"] = Value::Null;
    snapshot["as_of"] = json!("2024-01-03T00:01:00Z");
    store(dir.path(), &snapshot);
    let uri = format!("{CHART}?{SCOPE}&start=2024-01-02T23:59:00Z&end=2024-01-03T00:00:00Z");
    let (status, result) = call(&app, &uri).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["rows"][0]["bar"]["end_ts"], "2024-01-03T00:00:00Z");

    snapshot["closed"][0]["end_ts"] = json!("2024-01-03T00:00:00.000000001Z");
    store(dir.path(), &snapshot);
    let (status, result) = call(&app, &uri).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(result["error"]["code"], "bars_invalid");
}

#[tokio::test]
async fn smaller_visible_page_recovers_from_indicator_work_limit_with_full_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(dir.path());
    let mut snapshot = projection();
    snapshot["provenance"]["calendar_id"] = json!("24X7");
    snapshot["provenance"]["market_timezone"] = json!("UTC");
    // Constant synthetic close across 4,000 one-minute observations. The known
    // Bollinger bands all equal the close regardless of the period.
    let first: DateTime<Utc> = "2024-01-02T00:00:00Z".parse().unwrap();
    const NUM_BARS: i64 = 4_000;
    let base = snapshot["closed"][0].clone();
    let rows: Vec<_> = (0..NUM_BARS)
        .map(|minute| {
            let mut bar = base.clone();
            let start = first + TimeDelta::minutes(minute);
            let end = start + TimeDelta::minutes(1);
            bar["open_ts"] = json!(start);
            bar["end_ts"] = json!(end);
            bar["first_available_ts"] = json!(end);
            bar["trading_day"] = json!(start.date_naive());
            bar
        })
        .collect();
    let end = first + TimeDelta::minutes(NUM_BARS);
    snapshot["closed"] = json!(rows);
    snapshot["provisional"] = Value::Null;
    snapshot["as_of"] = json!(end);
    store(dir.path(), &snapshot);

    let start = first + TimeDelta::minutes(2_520);
    let start = encoded(&start.to_rfc3339());
    let end = encoded(&end.to_rfc3339());
    let indicators = [2_520, 2_519, 2_518]
        .map(|period| json!({"use":"indicator.bollinger@1.0.0","params":{"period":period}}));
    let indicators = encoded(&json!(indicators).to_string());
    let uri = format!("{CHART}?{SCOPE}&start={start}&end={end}&indicators={indicators}");
    let (status, result) = call(&app, &format!("{uri}&limit=1000")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(result["error"]["code"], "indicator_work_limit");
    let (status, result) = call(&app, &format!("{uri}&limit=1")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["rows"].as_array().unwrap().len(), 1);
    assert!(result["next_start"].is_string());
    let points = result["rows"][0]["indicator_values"].as_array().unwrap();
    assert_eq!(points.len(), 3);
    for point in points {
        assert_eq!(
            point,
            &json!({"status":"ready","values":{"lower":"10","middle":"10","upper":"10"}})
        );
    }

    let late_start = encoded(&(first + TimeDelta::minutes(3_500)).to_rfc3339());
    let (status, result) = call(
        &app,
        &format!("{CHART}?{SCOPE}&start={late_start}&end={end}&indicators={indicators}&limit=1"),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(result["error"]["code"], "indicator_work_limit");
}

#[tokio::test]
async fn registry_and_exported_openapi_describe_chart_routes() {
    let dir = tempfile::tempdir().unwrap();
    let (status, catalog) = call(&app(dir.path()), "/api/v1/indicators").await;
    assert_eq!(status, StatusCode::OK);
    let definitions = catalog["definitions"].as_array().unwrap();
    assert_eq!(definitions.len(), 10);
    let rsi = definitions
        .iter()
        .find(|indicator| indicator["id"] == "indicator.rsi_wilder@1.0.0")
        .unwrap();
    assert_eq!(rsi["warmup"], "period");
    assert!(rsi["seed"].is_string());

    let api = serde_json::to_value(ApiDoc::openapi()).unwrap();
    let schemas = &api["components"]["schemas"];
    assert_eq!(schemas["DecimalText"]["type"], "string");
    assert_eq!(
        schemas["ProjectedBar"]["properties"]["close"]["$ref"],
        "#/components/schemas/DecimalText"
    );
    assert_eq!(
        schemas["ChartValue"]["properties"]["values"]["additionalProperties"]["type"],
        "string"
    );
    assert_eq!(
        schemas["ValueStatus"]["enum"],
        json!(["warmup", "ready", "undefined"])
    );
    assert_eq!(
        api["paths"]["/api/v1/indicators"]["get"]["responses"]["200"]["content"]["application/json"]
            ["schema"]["$ref"],
        "#/components/schemas/IndicatorCatalog"
    );
    for (path, required) in [(CHART, false), (VALUES, true)] {
        let operation = &api["paths"][path]["get"];
        let params = operation["parameters"].as_array().unwrap();
        let selection = params
            .iter()
            .find(|param| param["name"] == "indicators")
            .unwrap();
        assert_eq!(selection["required"].as_bool().unwrap_or(false), required);
        assert!(operation["responses"]["404"].is_object());
        assert!(operation["responses"]["422"].is_object());
    }
}
