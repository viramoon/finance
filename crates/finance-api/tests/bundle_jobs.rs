use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use finance_api::{
    ApiDoc, AppState, router,
    strategy_bundle::{InputFormat, MemoryBindings, compile, example_catalog},
};
use serde_json::{Value, json};
use std::{fs, path::Path};
use utoipa::OpenApi;

const ENTRY_EXIT: &str =
    include_str!("../../../strategies/fixtures/strategy-bundle/v1/entry-exit.yaml");
const ID: &str = "example.equity-momentum";
const VERSION: &str = "1.0.0";
const BARS_REF: &str = "example.reviewed-us-daily@1.0.0";
const UNIVERSE_REF: &str = "example.us-cash-equities@1.0.0";
const BACKTESTS: &str = "/api/v1/backtests";

async fn request(root: &Path, method: &str, path: &str, key: &str, body: Value) -> (u16, Value) {
    raw_request(root, method, path, key, body.to_string()).await
}

async fn raw_request(
    root: &Path,
    method: &str,
    path: &str,
    key: &str,
    body: String,
) -> (u16, Value) {
    use tower::ServiceExt;
    let app = router(AppState {
        data_root: root.to_owned(),
        research_root: root.join("research"),
    });
    let response = app
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json")
                .header("Idempotency-Key", key)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    (status.as_u16(), serde_json::from_slice(&body).unwrap())
}

fn submission(source: Value) -> Value {
    json!({
        "strategy": "StrategyBundle",
        "start": "2025-01-02T00:00:00Z",
        "end": "2025-03-04T00:00:00Z",
        "source": source,
    })
}

fn inline(content: &str) -> Value {
    submission(json!({ "kind": "inline", "format": "yaml", "content": content }))
}

fn selected() -> Value {
    submission(json!({ "kind": "registered", "id": ID, "version": VERSION }))
}

fn inputs(root: &Path) -> std::path::PathBuf {
    root.join("finance-state/backtests/strategy-inputs")
}

fn publish(root: &Path, bars_ref: &str, universe_ref: &str, generation: u8) -> std::path::PathBuf {
    // Synthetic admission-only header. Python worker integration exercises complete data.
    let directory = inputs(root).join(bars_ref);
    fs::create_dir_all(&directory).unwrap();
    let path = directory.join(format!("{universe_ref}.json"));
    fs::write(
        &path,
        json!({
            "schema": "fertile.finance/strategy-replay/v1",
            "bars_ref": bars_ref,
            "universe_ref": universe_ref,
            "currency": "USD",
            "calendar": "XNYS",
            "timeframe": "1d",
            "price_basis": "raw_split_free_reviewed",
            "fixture_generation": generation,
        })
        .to_string(),
    )
    .unwrap();
    path
}

fn queue(root: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(root.join("finance-state/backtests/jobs.sqlite3")).unwrap()
}

fn persisted_request(db: &rusqlite::Connection, id: i64) -> Value {
    let request: String = db
        .query_row("SELECT request FROM jobs WHERE id=?1", [id], |row| {
            row.get(0)
        })
        .unwrap();
    serde_json::from_str(&request).unwrap()
}

#[tokio::test]
async fn configured_retry_is_atomic_across_inline_and_registered_sources() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let req = inline(ENTRY_EXIT);

    assert_eq!(
        request(root, "POST", BACKTESTS, "idem", req.clone())
            .await
            .1["error"]["code"],
        "no_data"
    );
    assert_eq!(
        request(root, "POST", BACKTESTS, "idem", selected()).await.0,
        404,
        "a failed enqueue must roll back its version registration"
    );
    let first = request(
        root,
        "POST",
        "/api/v1/strategies",
        "",
        json!({ "format": "yaml", "content": ENTRY_EXIT }),
    )
    .await;
    assert_eq!(first.0, 200);
    assert_eq!(first.1["id"], ID);
    assert_eq!(first.1["version"], VERSION);

    let alias = publish(root, BARS_REF, UNIVERSE_REF, 1);
    let pinned = fs::read(&alias).unwrap();
    let (a, b) = tokio::join!(
        request(root, "POST", BACKTESTS, "idem", req.clone()),
        request(root, "POST", BACKTESTS, "idem", selected()),
    );
    assert_eq!(a.0, 202);
    assert_eq!(a, b);
    let job_id = a.1["id"].as_i64().unwrap();
    let db = queue(root);
    let queued = persisted_request(&db, job_id);
    assert_eq!(queued["plan"]["content_sha256"], first.1["content_sha256"]);
    let digest = queued["snapshot_sha256"].as_str().unwrap();
    let stored: Vec<u8> = db
        .query_row(
            "SELECT content FROM strategy_backtest_snapshots WHERE snapshot_sha256=?1",
            [digest],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored, pinned);
    let rows: i64 = db
        .query_row("SELECT count(*) FROM jobs", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 1);

    fs::remove_file(&alias).unwrap();
    let canonical_json = submission(json!({
        "kind": "inline",
        "format": "json",
        "content": queued["plan"]["bundle"].to_string(),
    }));
    assert_eq!(
        request(root, "POST", BACKTESTS, "idem", canonical_json).await,
        a
    );
    let detail = request(
        root,
        "GET",
        &format!("{BACKTESTS}/{job_id}"),
        "",
        Value::Null,
    )
    .await;
    assert_eq!(
        detail.1["request"],
        json!({
            "strategy": "StrategyBundle",
            "start": req["start"],
            "end": req["end"],
            "source": selected()["source"],
            "content_sha256": first.1["content_sha256"],
        })
    );
    let changed = ENTRY_EXIT.replace(r#"threshold: "0.02""#, r#"threshold: "0.03""#);
    assert_ne!(changed, ENTRY_EXIT);
    assert_eq!(
        request(root, "POST", BACKTESTS, "idem", inline(&changed))
            .await
            .1["error"]["code"],
        "key_conflict"
    );
    assert_eq!(
        request(
            root,
            "POST",
            "/api/v1/strategies",
            "",
            json!({ "format": "yaml", "content": changed }),
        )
        .await
        .1["error"]["code"],
        "VERSION_CONFLICT"
    );

    publish(root, BARS_REF, UNIVERSE_REF, 2);
    let next = request(root, "POST", BACKTESTS, "next", selected()).await;
    assert_eq!(next.0, 202);
    let next_request = persisted_request(&db, next.1["id"].as_i64().unwrap());
    assert_eq!(queued["plan"], next_request["plan"]);
    assert_ne!(queued["snapshot_sha256"], next_request["snapshot_sha256"]);
}

#[tokio::test]
async fn version_conflict_locates_yaml_and_json_source_in_registration_and_inline_jobs() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let registration = json!({"format": "yaml", "content": ENTRY_EXIT});
    let (status, original) =
        request(root, "POST", "/api/v1/strategies", "", registration.clone()).await;
    assert_eq!(status, 200);

    let catalog = example_catalog();
    let mut bindings = MemoryBindings::default();
    let first = compile(
        ENTRY_EXIT.as_bytes(),
        InputFormat::Yaml,
        &catalog,
        &mut bindings,
    )
    .unwrap();
    let marker = "synthetic-private-title";
    let yaml = format!(
        "# Synthetic conflict fixture\n\n{}",
        ENTRY_EXIT.replacen("日线动量入场与持有期退出", marker, 1)
    );
    let changed = compile(
        yaml.as_bytes(),
        InputFormat::Yaml,
        &catalog,
        &mut MemoryBindings::default(),
    )
    .unwrap();
    assert_ne!(first.content_sha256, changed.content_sha256);
    let json = serde_json::to_string_pretty(&changed.bundle).unwrap();

    for (format, input_format, content, version_prefix) in [
        ("yaml", InputFormat::Yaml, yaml.as_str(), "version: "),
        ("json", InputFormat::Json, json.as_str(), "\"version\": "),
    ] {
        let diagnostic =
            compile(content.as_bytes(), input_format, &catalog, &mut bindings).unwrap_err();
        let value_offset = content.find(version_prefix).unwrap() + version_prefix.len();
        let before_value = &content[..value_offset];
        assert!(content[value_offset..].starts_with("\"1.0.0\""));
        let line = before_value.lines().count();
        let column = before_value.rsplit('\n').next().unwrap().chars().count() + 1;

        for (path, key, body) in [
            (
                "/api/v1/strategies",
                "",
                json!({"format": format, "content": content}),
            ),
            (
                BACKTESTS,
                format,
                submission(json!({"kind": "inline", "format": format, "content": content})),
            ),
        ] {
            let (status, body) = request(root, "POST", path, key, body).await;
            assert_eq!(status, 409);
            assert_eq!(body["error"]["code"], "VERSION_CONFLICT");
            assert_eq!(body["error"]["path"], "/metadata/version");
            assert_eq!(
                body["error"]["location"],
                json!({"line": line, "column": column})
            );
            assert_eq!(body["error"], serde_json::to_value(&diagnostic).unwrap());
            let message = body.to_string();
            for private in [
                marker,
                ID,
                VERSION,
                &first.content_sha256,
                &first.source_sha256,
                &changed.content_sha256,
                &changed.source_sha256,
            ] {
                assert!(
                    !message.contains(private),
                    "version conflict leaked private input"
                );
            }
        }
    }

    assert_eq!(
        request(root, "POST", "/api/v1/strategies", "", registration).await,
        (200, original)
    );
    let staged_jobs: i64 = queue(root)
        .query_row("SELECT COUNT(*) FROM jobs", [], |row| row.get(0))
        .unwrap();
    assert_eq!(staged_jobs, 0);
}

#[tokio::test]
async fn trusted_catalog_keeps_independent_versioned_universe_and_bar_refs() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let bars = "fixture.daily@1.2.3";
    let universe = "fixture.as-of@3.2.1";
    publish(root, bars, universe, 1);
    let catalog = inputs(root).join("catalog.json");
    fs::write(
        &catalog,
        json!({
            "universes": [{
                "id": universe,
                "calendar": "XNYS",
                "currency": "USD",
                "asset": "cash_equity",
                "point_in_time": true,
                "available_at": "session_open"
            }],
            "bars": [{
                "id": bars,
                "calendar": "XNYS",
                "currency": "USD",
                "asset": "cash_equity",
                "timeframe": "1d",
                "price_basis": "raw_split_free_reviewed",
                "available_at": "session_close"
            }]
        })
        .to_string(),
    )
    .unwrap();
    let content = ENTRY_EXIT
        .replace(BARS_REF, bars)
        .replace(UNIVERSE_REF, universe);
    let first = request(root, "POST", BACKTESTS, "catalog", inline(&content)).await;
    assert_eq!(first.0, 202);
    fs::remove_file(catalog).unwrap();
    assert_eq!(
        request(root, "POST", BACKTESTS, "catalog", inline(&content)).await,
        first
    );
    assert_eq!(
        request(root, "POST", BACKTESTS, "catalog", selected()).await,
        first
    );
    assert_eq!(
        request(root, "POST", BACKTESTS, "new-key", selected())
            .await
            .0,
        202
    );
    let second = persisted_request(&queue(root), first.1["id"].as_i64().unwrap());
    assert_eq!(second["plan"]["inputs"]["bars"]["id"], bars);
    assert_eq!(second["plan"]["inputs"]["universe"]["id"], universe);
}

#[tokio::test]
async fn configured_admission_reuses_f2_diagnostics_and_keeps_legacy_limit() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let bad = include_str!("../../../strategies/fixtures/strategy-bundle/v1/duplicate-key.txt");
    let (status, body) = request(root, "POST", BACKTESTS, "bad", inline(bad)).await;
    assert_eq!(status, 422);
    assert_eq!(body["error"]["code"], "DUPLICATE_KEY");
    assert_eq!(body["error"]["path"], "/metadata/id");
    assert!(body["error"]["location"]["line"].as_u64().unwrap() >= 1);

    let mut with_extra_source_field = selected();
    with_extra_source_field["source"]["content"] = json!(ENTRY_EXIT);
    assert_eq!(
        request(root, "POST", BACKTESTS, "bad", with_extra_source_field)
            .await
            .1["error"]["code"],
        "invalid_request"
    );

    publish(root, BARS_REF, UNIVERSE_REF, 1);
    let large = format!("{ENTRY_EXIT}\n# {}\n", "a".repeat(8192));
    assert_eq!(
        request(root, "POST", BACKTESTS, "large", inline(&large))
            .await
            .0,
        202
    );
    let legacy = json!({
        "strategy": "DemoStrategy", "instrument": "6EH4.XCME", "params": {},
        "start": "2024-01-01T23:02:00Z", "end": "2024-01-01T23:06:00Z"
    });
    let legacy_whitespace = format!("{legacy}{}", " ".repeat(8192));
    assert_eq!(
        raw_request(root, "POST", BACKTESTS, "padded", legacy_whitespace)
            .await
            .0,
        422
    );
    assert_eq!(
        request(root, "POST", BACKTESTS, "padded", legacy).await.0,
        202
    );
}

#[tokio::test]
async fn configured_half_open_interval_can_include_last_supported_data_day() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    publish(root, BARS_REF, UNIVERSE_REF, 1);
    let mut req = inline(ENTRY_EXIT);
    req["start"] = json!("2100-12-29T00:00:00Z");
    req["end"] = json!("2101-01-01T00:00:00Z");
    assert_eq!(
        request(root, "POST", BACKTESTS, "last", req.clone())
            .await
            .0,
        202
    );
    req["end"] = json!("2101-01-02T00:00:00Z");
    assert_eq!(
        request(root, "POST", BACKTESTS, "after-last", req).await.0,
        422
    );
}

#[cfg(unix)]
#[tokio::test]
async fn replay_path_rejects_a_symlinked_bar_alias_directory() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("local");
    let external = temp.path().join("external");
    let path = publish(&external, BARS_REF, UNIVERSE_REF, 1);
    fs::create_dir_all(inputs(&root)).unwrap();
    std::os::unix::fs::symlink(path.parent().unwrap(), inputs(&root).join(BARS_REF)).unwrap();
    let (status, body) = request(&root, "POST", BACKTESTS, "link", inline(ENTRY_EXIT)).await;
    assert_eq!(status, 500);
    assert_eq!(body["error"]["code"], "strategy_input_error");
}

#[tokio::test]
async fn historical_sma_job_remains_readable_without_strategy_bundle_fields() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    assert_eq!(
        request(root, "GET", BACKTESTS, "", Value::Null).await.0,
        200
    );
    let historical_request = json!({
        "strategy":"SmaCrossover", "instrument":"AAA.XNYS",
        "start":"2024-01-02T00:00:00Z", "end":"2024-03-28T00:00:00Z",
        "params":{"fast_period":5,"slow_period":20,"shares":10}
    });
    let historical_summary = json!({
        "run_id":"synthetic-history", "strategy":"SmaCrossover", "instrument":"AAA.XNYS",
        "start":"2024-01-02T00:00:00Z", "end":"2024-03-28T00:00:00Z",
        "total_orders":2, "total_positions":1, "pnl":"12.34", "currency":"USD",
        "generated_at":"2024-04-01T00:00:00Z"
    });
    queue(root).execute(
        "INSERT INTO jobs(idem,request,status,attempt,summary) VALUES ('historic',?1,'succeeded',1,?2)",
        [historical_request.to_string(), historical_summary.to_string()],
    ).unwrap();
    let (status, job) = request(root, "GET", &format!("{BACKTESTS}/1"), "", Value::Null).await;
    assert_eq!(status, 200);
    assert_eq!(job["request"], historical_request);
    assert_eq!(job["summary"], historical_summary);
    assert_eq!(
        request(
            root,
            "GET",
            &format!("{BACKTESTS}/by-key"),
            "historic",
            Value::Null
        )
        .await,
        (200, json!({"id":1}))
    );
}

#[test]
fn openapi_exposes_bundle_alternatives_and_manifest_without_changing_legacy_shape() {
    let spec = serde_json::to_value(ApiDoc::openapi()).unwrap();
    let s = &spec["components"]["schemas"];
    assert_eq!(
        s["Job"]["properties"]["request"]["$ref"],
        "#/components/schemas/JobRequestView"
    );
    assert_eq!(
        s["JobRequest"]["required"],
        json!(["strategy", "instrument", "start", "end", "params"])
    );
    assert_eq!(
        s["SubmitRequest"]["oneOf"][0]["$ref"],
        "#/components/schemas/JobRequest"
    );
    assert_eq!(
        s["SubmitRequest"]["oneOf"][1]["$ref"],
        "#/components/schemas/BundleSubmission"
    );
    assert_eq!(s["StrategyFormat"]["enum"], json!(["yaml", "json"]));
    assert_eq!(s["BundleStrategy"]["enum"], json!(["StrategyBundle"]));
    assert_eq!(
        s["AcceptedSource"]["oneOf"][0]["properties"]["kind"]["enum"],
        json!(["registered"])
    );
    assert_eq!(
        s["BacktestSummary"]["properties"]["run_manifest"]["type"],
        "object"
    );
    assert_eq!(
        s["BacktestSummary"]["properties"]["strategy_report"]["type"],
        "object"
    );
    assert!(spec["paths"]["/api/v1/strategies"]["post"].is_object());
}

#[tokio::test]
async fn strategy_preview_registration_and_pinned_selection_share_a_canonical_plan() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let registry = "/api/v1/strategies";
    let validate = "/api/v1/strategies/validate";
    let select = format!("{registry}/{ID}/versions/{VERSION}");
    let threshold = "0.012345678901234567";
    let content = ENTRY_EXIT.replace("0.02", threshold);
    let yaml = json!({"format":"yaml","content":content});
    let (status, preview) = request(root, "POST", validate, "", yaml.clone()).await;
    assert_eq!(status, 200);
    let plan = &preview["plan"];
    assert_eq!(plan["plan_version"], "fertile.finance/strategy-plan/v1");
    assert_eq!(
        plan["bundle"]["spec"]["modules"]["A1"]["params"]["threshold"],
        threshold
    );
    assert_eq!(
        preview["strategy"]["content_sha256"],
        plan["content_sha256"]
    );
    assert_eq!(root.read_dir().unwrap().count(), 0);
    assert_eq!(request(root, "GET", &select, "", Value::Null).await.0, 404);
    assert_eq!(
        request(root, "GET", registry, "", Value::Null).await,
        (200, json!({"items":[],"next_cursor":null}))
    );
    assert_eq!(root.read_dir().unwrap().count(), 0);

    let json_doc = json!({"format":"json","content":plan["bundle"].to_string()});
    let (status, alternate) = request(root, "POST", validate, "", json_doc.clone()).await;
    assert_eq!(status, 200);
    assert_eq!(alternate["strategy"], preview["strategy"]);
    assert_eq!(alternate["plan"]["bundle"], plan["bundle"]);
    assert_ne!(alternate["plan"]["source_sha256"], plan["source_sha256"]);

    assert_eq!(
        request(root, "POST", registry, "", yaml.clone()).await,
        (200, preview["strategy"].clone())
    );
    assert_eq!(
        request(root, "POST", registry, "", json_doc).await,
        (200, preview["strategy"].clone())
    );
    let second = json!({"format":"yaml","content":content.replace("\"1.0.0\"", "\"2.0.0\"")});
    assert_eq!(request(root, "POST", registry, "", second).await.0, 200);

    let query = format!("{registry}?id={ID}&limit=1");
    let (status, first) = request(root, "GET", &query, "", Value::Null).await;
    assert_eq!(status, 200);
    assert_eq!(first["items"], json!([preview["strategy"]]));
    assert_eq!(first["next_cursor"], format!("{ID}@{VERSION}"));
    let next = format!("{query}&after={}", first["next_cursor"].as_str().unwrap());
    let (status, second) = request(root, "GET", &next, "", Value::Null).await;
    assert_eq!(status, 200);
    assert_eq!(second["items"][0]["version"], "2.0.0");
    assert_eq!(second["next_cursor"], Value::Null);
    assert_eq!(
        request(
            root,
            "GET",
            &format!("{query}&after=other@1.0.0"),
            "",
            Value::Null
        )
        .await
        .0,
        422
    );

    // A selection depends on stored input metadata rather than the current catalog.
    fs::create_dir_all(inputs(root)).unwrap();
    fs::write(
        inputs(root).join("catalog.json"),
        "synthetic invalid catalog",
    )
    .unwrap();
    assert_eq!(
        request(root, "GET", &select, "", Value::Null).await,
        (200, preview)
    );
}

#[tokio::test]
async fn strategy_validation_returns_compiler_location_and_enforces_upload_limit() {
    let temp = tempfile::tempdir().unwrap();
    let path = "/api/v1/strategies/validate";
    let bad = json!({"format":"yaml","content":ENTRY_EXIT.replace("threshold: \"0.02\"", "threshold: 0.02")});
    let (status, body) = request(temp.path(), "POST", path, "", bad).await;
    assert_eq!(status, 422);
    assert_eq!(
        body["error"]["path"],
        "/spec/modules/<key>/params/threshold"
    );
    assert!(body["error"]["location"]["line"].as_u64().unwrap() > 0);
    assert!(body["error"]["location"]["column"].as_u64().unwrap() > 0);

    let large =
        json!({"format":"yaml","content":"x".repeat(finance_api::strategy_bundle::MAX_BYTES + 1)});
    let (status, body) = request(temp.path(), "POST", path, "", large).await;
    assert_eq!(status, 422);
    assert_eq!(body["error"]["code"], "INPUT_TOO_LARGE");
    assert_eq!(temp.path().read_dir().unwrap().count(), 0);
}
