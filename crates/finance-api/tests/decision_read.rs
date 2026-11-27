use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header::CACHE_CONTROL},
};
use finance_api::{AppState, router};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::{fs, path::Path};
use tower::ServiceExt;

// Fixed synthetic IDs and boundary values. The temporary SQLite schemas mirror
// the producer tables in decision.py and decision_bundle.py.
const PLAN: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
const LICENSE: &str = "fixture.synthetic";
const PATH: &str = "/api/v1/decisions/signals";
const STORE: &str = "finance-state/decisions";
const LARGE: &str = "18446744073709551615";
const DECIMAL: &str = "12345678901234567890.12345678";
const FRAC: &str = "0.123456789012345678901234567";

fn app(root: &Path) -> Router {
    router(AppState {
        data_root: root.into(),
        research_root: root.join("research"),
    })
}

async fn get(app: &Router, path: &str) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    assert_eq!(response.headers()[CACHE_CONTROL], "private, no-store");
    let body = to_bytes(response.into_body(), 65536).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

fn publication(root: &Path, kind: &str, allowed: bool) {
    let path = root.join(STORE).join("read-v1.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, json!({
        "schema_version":1, "kind":kind, "plan_sha256":PLAN,
        "access":{"license":LICENSE, "record_local":allowed, "derive_non_display":allowed, "redistribute":false}
    }).to_string()).unwrap();
}

#[tokio::test]
async fn sma_decision_signals_preserve_u64_and_decimal_and_require_matching_license() {
    let root = tempfile::tempdir().unwrap();
    let app = app(root.path());
    let (status, error) = get(&app, PATH).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(error["error"]["code"], "decision_not_published");
    assert!(!root.path().join(STORE).exists());

    publication(root.path(), "sma_crossover", true);
    let db_path = root.path().join(STORE).join("decisions.sqlite3");
    assert_eq!(get(&app, PATH).await.0, StatusCode::NOT_FOUND);
    assert!(!db_path.exists());

    let conn = Connection::open(db_path).unwrap();
    conn.execute_batch(
        "CREATE TABLE decisions (
           replay_offset INTEGER PRIMARY KEY, plan_sha256 TEXT NOT NULL,
           source TEXT NOT NULL, product TEXT NOT NULL, channel TEXT NOT NULL,
           instrument_id TEXT NOT NULL, epoch TEXT NOT NULL, seq TEXT NOT NULL,
           cursor TEXT NOT NULL, license TEXT NOT NULL, exchange_ts TEXT NOT NULL,
           first_available_ts TEXT NOT NULL, decision_ts TEXT NOT NULL, target_shares TEXT NOT NULL,
           UNIQUE(epoch,seq));",
    )
    .unwrap();
    let offset = 9_007_199_254_740_993_i64;
    for (position, seq, license) in [
        (offset, "18446744073709551613", LICENSE),
        (offset + 1, "18446744073709551614", "fixture.display-only"),
        (offset + 2, LARGE, LICENSE),
    ] {
        conn.execute(
            "INSERT INTO decisions VALUES(?1,?2,'fixture','sma','bars','SYNTH.XNYS',?3,?4,
             'fixture-cursor',?5,?3,?3,?3,?6)",
            params![position, PLAN, LARGE, seq, license, DECIMAL],
        )
        .unwrap();
    }

    let (status, page) = get(&app, &format!("{PATH}?limit=1")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["plan_sha256"], PLAN);
    assert_eq!(
        page["access"],
        json!({
            "license":LICENSE,"record_local":true,"derive_non_display":true,"redistribute":false
        })
    );
    assert_eq!(page["items"][0]["kind"], "sma_crossover");
    assert_eq!(
        page["items"][0]["signal"]["replay_offset"],
        offset.to_string()
    );
    let after = page["next_cursor"].as_str().unwrap();
    let (status, page) = get(&app, &format!("{PATH}?limit=1&after={after}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["next_cursor"], Value::Null);
    let row = &page["items"][0]["signal"];
    for field in [
        "epoch",
        "seq",
        "exchange_ts",
        "first_available_ts",
        "decision_ts",
    ] {
        assert_eq!(row[field], LARGE, "{field}");
    }
    assert_eq!(row["target_shares"], DECIMAL);
    assert_eq!(row["license"], LICENSE);

    publication(root.path(), "sma_crossover", false);
    let (status, body) = get(&app, PATH).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.get("items").is_none());
    assert!(!body.to_string().contains(LICENSE));
}

#[tokio::test]
async fn bundle_decision_paginates_within_a_session_and_rejects_other_plan_cursors() {
    let root = tempfile::tempdir().unwrap();
    let app = app(root.path());
    publication(root.path(), "strategy_bundle", true);
    let conn = Connection::open(root.path().join(STORE).join("decisions.sqlite3")).unwrap();
    conn.execute_batch(
        "CREATE TABLE bundle_decisions (
           replay_offset INTEGER NOT NULL, plan_sha256 TEXT NOT NULL, session TEXT NOT NULL,
           signal_ts TEXT NOT NULL, recipe TEXT NOT NULL, instrument_id TEXT NOT NULL,
           target_shares TEXT NOT NULL, reason TEXT,
           PRIMARY KEY(replay_offset,recipe,instrument_id));",
    )
    .unwrap();
    for (plan, recipe, symbol, reason) in [
        (PLAN, "B", "SYNTH.XNYS", Some("signal")),
        (PLAN, "A", "TWO.XNYS", None),
        (PLAN, "A", "ONE.XNYS", None),
        ("other-plan", "C", "SYNTH.XNYS", None),
    ] {
        conn.execute(
            "INSERT INTO bundle_decisions VALUES(42,?1,'2026-01-05',?2,?3,?4,?5,?6)",
            params![plan, LARGE, recipe, symbol, FRAC, reason],
        )
        .unwrap();
    }

    let (status, page) = get(&app, &format!("{PATH}?limit=1")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        page["items"][0],
        json!({
            "kind": "strategy_bundle",
            "signal": {"replay_offset":"42","session":"2026-01-05","signal_ts":LARGE,
                       "recipe":"A","instrument_id":"ONE.XNYS","target_shares":FRAC,"reason":null}
        })
    );
    let after = page["next_cursor"].as_str().unwrap();
    let (status, page) = get(&app, &format!("{PATH}?after={after}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["items"].as_array().unwrap().len(), 2);
    assert_eq!(page["items"][0]["signal"]["instrument_id"], "TWO.XNYS");
    assert_eq!(page["items"][1]["signal"]["reason"], "signal");
    assert_eq!(page["next_cursor"], Value::Null);

    for after in [
        "other-plan@42@A@ONE.XNYS".into(),
        format!("{PLAN}@42"),
        format!("{PLAN}@-1@A@ONE.XNYS"),
    ] {
        assert_eq!(
            get(&app, &format!("{PATH}?after={after}")).await.0,
            StatusCode::BAD_REQUEST
        );
    }
}
