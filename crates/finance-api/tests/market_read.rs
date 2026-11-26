use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use finance_api::{AppState, router};
use finance_collector::market_ingress::{MarketIngress, QualityReport, RecoveryOutcome, wal_root};
use serde_json::{Value, json};
use std::{fs, path::Path, process::Command};
use tower::ServiceExt;

#[path = "support/market_read_fixture.rs"]
mod fixture;
use fixture::{
    SOURCE, TRADES, index_fixture, index_trade, policy, projection_dir, publish,
    publish_perpetual_context, quality,
};

const WAL: &str = "/api/v1/market/wal?source=fixture&product=us-etf-delayed";
const MARKS: &str = "/api/v1/market/watermarks?source=fixture&product=us-etf-delayed";
const GAPS: &str = "/api/v1/quality/gaps?source=fixture&product=us-etf-delayed";
const LATEST: &str = "/api/v1/market/projection/latest?source=fixture&product=us-etf-delayed&channel=trades&instrument_id=SPY.ARCA&epoch=41&seq=";

fn app(root: &Path) -> Router {
    router(AppState {
        data_root: root.to_owned(),
        research_root: root.join("unused-research"),
    })
}

async fn get(app: &Router, uri: &str, status: StatusCode) -> Value {
    let response = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), status, "{uri}");
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "private, no-store"
    );
    let bytes = to_bytes(response.into_body(), 256 * 1024).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    if status.is_success() {
        let as_of = chrono::DateTime::parse_from_rfc3339(body["as_of"].as_str().unwrap()).unwrap();
        assert_eq!(as_of.offset().local_minus_utc(), 0);
    }
    body
}

#[tokio::test]
async fn wal_pages_pin_the_durable_prefix_and_preserve_wire_values() {
    let root = tempfile::tempdir().unwrap();
    let mut ingress = MarketIngress::open(root.path(), policy(TRADES, true)).unwrap();
    let quotes = publish(&mut ingress, &[0, 1], true);
    let app = app(root.path());

    let staged = get(&app, WAL, StatusCode::OK).await;
    assert_eq!(staged["durable_through"], "0");
    assert_eq!(staged["records"], json!([]));
    assert_eq!(staged["next_after"], Value::Null);
    assert_eq!(ingress.flush().unwrap(), Some(2));

    let first = get(&app, &format!("{WAL}&limit=1"), StatusCode::OK).await;
    assert_eq!(
        first["access"],
        serde_json::to_value(policy(TRADES, true)).unwrap()
    );
    assert_eq!(first["through"], "2");
    assert_eq!(first["next_after"], "1");
    assert_eq!(first["records"][0]["position"], "1");
    assert_eq!(first["records"][0]["generation"], "1");
    assert_eq!(first["records"][0]["event"], quotes[0]);
    assert_eq!(first["records"][0]["event"]["payload"]["price"], "663.1200");

    publish(&mut ingress, &[2], true);
    assert_eq!(ingress.flush().unwrap(), Some(3));
    let second = get(&app, &format!("{WAL}&after=1&through=2"), StatusCode::OK).await;
    assert_eq!(second["durable_through"], "3");
    assert_eq!(second["through"], "2");
    assert_eq!(second["next_after"], Value::Null);
    assert_eq!(second["records"].as_array().unwrap().len(), 1);
    assert_eq!(second["records"][0]["event"], quotes[1]);
    assert_eq!(second["records"][0]["event"]["seq"], "9007199254740993");
    assert_eq!(
        second["records"][0]["event"]["exchange_ts"],
        quotes[0]["exchange_ts"]
    );

    for param in [
        "after=01",
        "after=18446744073709551616",
        "after=3&through=2",
        "through=4",
        "limit=0",
        "limit=4097",
        "other=1",
    ] {
        let invalid = get(&app, &format!("{WAL}&{param}"), StatusCode::BAD_REQUEST).await;
        assert_eq!(invalid["error"]["code"], "market_query_invalid");
    }
    let invalid = get(
        &app,
        "/api/v1/market/wal?source=..%2Ffixture&product=us-etf-delayed",
        StatusCode::BAD_REQUEST,
    )
    .await;
    assert_eq!(invalid["error"]["code"], "market_query_invalid");

    let mut replay = MarketIngress::open(root.path(), policy("research-replay", true)).unwrap();
    publish(&mut replay, &[8], true);
    replay.flush().unwrap();
    let exact = get(
        &app,
        "/api/v1/market/wal?source=fixture&product=research-replay",
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        exact["records"][0]["event"]["payload"]["open"],
        "663.1234567890123400"
    );
    let mut book = MarketIngress::open(root.path(), policy("perp-replay", true)).unwrap();
    publish(&mut book, &[9, 10], true);
    book.flush().unwrap();
    let book = get(
        &app,
        "/api/v1/market/wal?source=fixture&product=perp-replay",
        StatusCode::OK,
    )
    .await;
    assert_eq!(book["records"][1]["event"]["seq"], u64::MAX.to_string());

    let products = get(
        &app,
        "/api/v1/market/products?source=fixture&offset=1&limit=1",
        StatusCode::OK,
    )
    .await;
    assert_eq!(products["items"][0]["product"], "research-replay");
    assert_eq!(products["next_offset"], 2);
    let products = get(
        &app,
        "/api/v1/market/products?offset=2&limit=1",
        StatusCode::OK,
    )
    .await;
    assert_eq!(products["items"][0]["product"], TRADES);
    assert_eq!(products["next_offset"], Value::Null);
}

#[tokio::test]
async fn display_only_data_neither_persists_nor_appears_in_read_contract() {
    let root = tempfile::tempdir().unwrap();
    let mut ingress = MarketIngress::open(root.path(), policy(TRADES, false)).unwrap();
    publish(&mut ingress, &[0], false);
    ingress.report_quality(quality("trades", 1, 2)).unwrap();
    assert_eq!(ingress.display_events().count(), 1);
    assert_eq!(ingress.quality().unwrap().len(), 1);
    assert_eq!(ingress.flush().unwrap(), None);
    assert!(!wal_root(root.path()).exists());

    let app = app(root.path());
    let products = get(&app, "/api/v1/market/products", StatusCode::OK).await;
    assert_eq!(products["items"], json!([]));
    for uri in [WAL, MARKS, GAPS, &format!("{LATEST}9007199254740992")] {
        let body = get(&app, uri, StatusCode::NOT_FOUND).await;
        assert_eq!(body["error"]["code"], "market_product_not_found");
        assert!(!body.to_string().contains("fixture.synthetic"));
    }
    assert!(fs::read_dir(root.path()).unwrap().next().is_none());
}

#[tokio::test]
async fn malformed_or_mismatched_permissions_fail_closed() {
    let root = tempfile::tempdir().unwrap();
    let mut ingress = MarketIngress::open(root.path(), policy(TRADES, true)).unwrap();
    publish(&mut ingress, &[0], true);
    ingress.flush().unwrap();
    let app = app(root.path());
    let binding = wal_root(root.path())
        .join(SOURCE)
        .join(TRADES)
        .join("access.json");
    let mut revoked = policy(TRADES, true);
    revoked.license = "fixture.changed".parse().unwrap();
    fs::write(&binding, serde_json::to_vec(&revoked).unwrap()).unwrap();
    for uri in [WAL, MARKS] {
        let body = get(&app, uri, StatusCode::INTERNAL_SERVER_ERROR).await;
        assert_eq!(body["error"]["code"], "market_storage_error");
        assert!(!body.to_string().contains(root.path().to_str().unwrap()));
        assert!(!body.to_string().contains("fixture.changed"));
    }
    revoked.record_local = false;
    fs::write(&binding, serde_json::to_vec(&revoked).unwrap()).unwrap();
    for uri in [
        "/api/v1/market/products",
        WAL,
        MARKS,
        GAPS,
        &format!("{LATEST}9007199254740992"),
    ] {
        let body = get(&app, uri, StatusCode::INTERNAL_SERVER_ERROR).await;
        assert_eq!(body["error"]["code"], "market_storage_error");
    }
}

#[tokio::test]
async fn quality_filters_half_open_utc_windows_and_keeps_unknowns() {
    let root = tempfile::tempdir().unwrap();
    let mut ingress = MarketIngress::open(root.path(), policy(TRADES, true)).unwrap();
    let past = quality("trades", 1, 9);
    let boundary = quality("trades", 1, 10);
    let recovered = QualityReport {
        outcome: Some(RecoveryOutcome::HistoryUnrecoverable),
        count: Some(u64::MAX.into()),
        ..quality("trades", 19, 20)
    };
    for item in [
        past,
        boundary.clone(),
        quality("book", 12, 17),
        recovered.clone(),
        quality("trades", 20, 21),
    ] {
        ingress.report_quality(item).unwrap();
    }
    let app = app(root.path());
    let query = format!("{GAPS}&channel=trades&instrument_id=SPY.ARCA&from_ts=10&to_ts=20&limit=1");
    let first = get(&app, &query, StatusCode::OK).await;
    assert_eq!(first["access"]["license"], "fixture.synthetic");
    assert_eq!(first["items"], json!([boundary]));
    assert_eq!(first["items"][0]["outcome"], Value::Null);
    assert_eq!(first["items"][0]["count"], Value::Null);
    assert_eq!(first["next_offset"], 1);
    let next = get(&app, &format!("{query}&offset=1"), StatusCode::OK).await;
    assert_eq!(next["items"], json!([recovered]));
    assert_eq!(next["items"][0]["count"], u64::MAX.to_string());
    assert_eq!(next["next_offset"], Value::Null);
    let empty = get(
        &app,
        &format!("{GAPS}&instrument_id=SPY.BATS"),
        StatusCode::OK,
    )
    .await;
    assert_eq!(empty["items"], json!([]));
    for invalid in [
        "from_ts=20&to_ts=20",
        "from_ts=30&to_ts=20",
        "offset=-1",
        "limit=501",
    ] {
        assert_eq!(
            get(&app, &format!("{GAPS}&{invalid}"), StatusCode::BAD_REQUEST).await["error"]["code"],
            "market_query_invalid"
        );
    }
}

#[tokio::test]
async fn projected_latest_checks_the_index_revision_position_and_version() {
    let root = tempfile::tempdir().unwrap();
    let mut ingress = MarketIngress::open(root.path(), policy(TRADES, true)).unwrap();
    let events = publish(&mut ingress, &[0, 1, 1, 2], true);
    ingress.flush().unwrap();
    let app = app(root.path());
    let corrected = format!("{LATEST}9007199254740992");
    let unchanged = format!("{LATEST}9007199254740993");
    let missing = get(&app, &corrected, StatusCode::NOT_FOUND).await;
    assert_eq!(missing["error"]["code"], "market_projection_not_found");
    assert!(!projection_dir(root.path()).exists());

    let index = index_fixture(root.path(), "4");
    index_trade(&index, "9007199254740992", "1", 4);
    index_trade(&index, "9007199254740993", "0", 2);
    let value = get(&app, &corrected, StatusCode::OK).await;
    assert_eq!(value["access"]["license"], "fixture.synthetic");
    assert_eq!(value["projected_through"], "4");
    assert_eq!(value["record"]["position"], "4");
    assert_eq!(value["record"]["event"], events[3]);
    assert_eq!(
        value["record"]["event"]["payload"]["replacement"]["payload"]["price"],
        "663.1100"
    );
    let value = get(&app, &unchanged, StatusCode::OK).await;
    assert_eq!(value["record"]["position"], "2");
    assert_eq!(value["record"]["event"], events[1]);
    let missing = get(
        &app,
        &format!("{LATEST}18446744073709551615"),
        StatusCode::NOT_FOUND,
    )
    .await;
    assert_eq!(missing["error"]["code"], "market_projection_not_found");

    let files: Vec<_> = fs::read_dir(projection_dir(root.path()))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(files, ["projection.sqlite3"]);

    index_trade(&index, "9007199254740992", "1", 2);
    assert_eq!(
        get(&app, &corrected, StatusCode::INTERNAL_SERVER_ERROR).await["error"]["code"],
        "market_storage_error"
    );
    index_trade(&index, "9007199254740992", "1", 4);
    index
        .execute("UPDATE checkpoint SET projected_through='5'", [])
        .unwrap();
    get(&app, &corrected, StatusCode::INTERNAL_SERVER_ERROR).await;
    index
        .execute("UPDATE checkpoint SET projected_through='4'", [])
        .unwrap();
    index.execute_batch("PRAGMA user_version = 2").unwrap();
    get(&app, &corrected, StatusCode::INTERNAL_SERVER_ERROR).await;
}

#[tokio::test]
#[ignore = "requires FINANCE_F3_TEST_PYTHON and FINANCE_F3_TEST_WAL_BIN"]
async fn python_projector_index_is_readable_over_http() {
    let root = tempfile::tempdir().unwrap();
    let mut ingress = MarketIngress::open(root.path(), policy(TRADES, true)).unwrap();
    let events = publish(&mut ingress, &[0, 1, 1, 2], true);
    ingress.flush().unwrap();
    publish(&mut ingress, &[3], true);
    let context = publish_perpetual_context(root.path());
    let python = std::env::var_os("FINANCE_F3_TEST_PYTHON").unwrap();
    let wal_bin = std::env::var_os("FINANCE_F3_TEST_WAL_BIN").unwrap();
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output = Command::new(python)
        .env("PYTHONPATH", repo.join("python"))
        .args([
            "-B",
            "-m",
            "fertile_finance.market_projector",
            "--data-root",
        ])
        .arg(root.path())
        .arg("--wal-bin")
        .arg(wal_bin)
        .arg("--once")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let app = app(root.path());
    let projected = get(&app, &format!("{LATEST}9007199254740992"), StatusCode::OK).await;
    assert_eq!(projected["record"]["event"], events[3]);
    let mark = get(&app, MARKS, StatusCode::OK).await;
    assert_eq!(mark["access"]["license"], "fixture.synthetic");
    assert_eq!(
        mark["watermarks"],
        json!({
            "source_latest": {
                "channel": events[3]["channel"],
                "instrument_id": events[3]["instrument_id"],
                "epoch": events[3]["epoch"],
                "seq": events[3]["seq"],
                "exchange_ts": events[3]["exchange_ts"],
                "receive_ts": events[3]["receive_ts"],
            },
            "local_durable": "4", "projected": "4", "remote_verified": null,
        })
    );
    let mut files: Vec<_> = fs::read_dir(projection_dir(root.path()))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    files.sort();
    assert_eq!(
        files,
        ["projection.sqlite3", "watermark.json", "writer.lock"]
    );

    let product = "source=hyperliquid&product=perp-public";
    let wal = get(
        &app,
        &format!("/api/v1/market/wal?{product}"),
        StatusCode::OK,
    )
    .await;
    assert_eq!(wal["records"][0]["event"], context);
    let projection = format!(
        "/api/v1/market/projection/latest?{product}&channel=perpetual_context&instrument_id=BTC-USD-PERP.HYPERLIQUID&epoch=1&seq=1"
    );
    let projected = get(&app, &projection, StatusCode::OK).await;
    assert_eq!(projected["access"]["license"], context["license"]);
    assert_eq!(projected["projected_through"], "1");
    assert_eq!(projected["record"]["event"], context);
}
