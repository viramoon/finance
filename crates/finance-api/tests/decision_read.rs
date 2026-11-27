use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header::CACHE_CONTROL},
};
use finance_api::{AppState, router, strategy_bundle::compile_legacy_sma};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};
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
    publish_plan(root, kind, allowed, PLAN);
}

fn publish_plan(root: &Path, kind: &str, allowed: bool, plan: &str) {
    let path = root.join(STORE).join("read-v1.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, json!({
        "schema_version":1, "kind":kind, "plan_sha256":plan,
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

    // Legacy rows stay unpublishable without the producer's checkpoint proof.
    assert_eq!(get(&app, PATH).await.0, StatusCode::NOT_FOUND);
    conn.execute_batch(
        "CREATE TABLE decision_checkpoint(id INTEGER PRIMARY KEY, plan_sha256 TEXT,
                                         replay_offset INTEGER, replay_sha256 TEXT);
         CREATE TABLE sma_replay_license(id INTEGER PRIMARY KEY, license TEXT,
                                         replay_offset INTEGER, replay_sha256 TEXT);",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO decision_checkpoint VALUES(1,?1,?2,?3)",
        params![PLAN, offset + 2, PLAN],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO sma_replay_license VALUES(1,?1,?2,?3)",
        params![LICENSE, offset + 2, PLAN],
    )
    .unwrap();

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

fn replay_synthetic_sma(root: &Path) -> Output {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    Command::new("python3")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("FERTILE_DATA_ROOT", root)
        .env("PYTHONPATH", repo.join("python"))
        .args(["-B", "-m", "fertile_finance.decision", "--plan"])
        .arg(root.join("plan.json"))
        .arg("--replay")
        .arg(root.join("replay.ndjson"))
        .output()
        .unwrap()
}

fn write_synthetic_sma(root: &Path, first_price: u64, first_license: &str, plan: &Value) {
    fs::write(root.join("plan.json"), plan.to_string()).unwrap();
    let replay = [first_price, 10, 20].into_iter().enumerate().map(|(index, price)| {
        let time = 100 + 3 * index;
        let license = if index == 0 { first_license } else { LICENSE };
        let px = format!("{price}.00");
        json!({
            "schema_version":"1.0", "channel":"bars.1d", "source":"fixture",
            "product":"sma", "instrument_id":"SYNTH.XNYS", "asset_kind":"stock",
            "venue":"XNYS", "exchange_ts":time.to_string(), "receive_ts":(time+1).to_string(),
            "first_available_ts":time.to_string(), "epoch":"1",
            "seq":(u64::MAX-2+index as u64).to_string(), "cursor":format!("fixture-{index}"),
            "revision":"0", "license":license, "kind":"bar_closed",
            "payload":{"open_ts":(time-1).to_string(), "open":px, "high":px, "low":px, "close":px, "volume":"100"}
        }).to_string() + "\n"
    }).collect::<String>();
    fs::write(root.join("replay.ndjson"), replay).unwrap();
}

#[tokio::test]
async fn mixed_license_sma_warmup_cannot_influence_a_published_signal() {
    let plan = compile_legacy_sma("SYNTH.XNYS", 2, 3, 10).unwrap();
    let plan_hash = plan["content_sha256"].as_str().unwrap();
    // These two warmup prices make the real evaluator choose different targets
    // if the first bar is eligible. Mark it display-only in the second replay.
    for (first_price, expected_target) in [(4, "10"), (40, "0")] {
        let root = tempfile::tempdir().unwrap();
        let process = app(root.path());
        write_synthetic_sma(root.path(), first_price, LICENSE, &plan);
        publish_plan(root.path(), "sma_crossover", true, plan_hash);
        let result = replay_synthetic_sma(root.path());
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let (status, page) = get(&process, PATH).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(page["items"][0]["signal"]["seq"], LARGE);
        assert_eq!(page["items"][0]["signal"]["target_shares"], expected_target);

        // Old stores do not carry proof. A restart can verify the whole
        // committed replay prefix and certify a still-homogeneous store.
        let db = Connection::open(root.path().join(STORE).join("decisions.sqlite3")).unwrap();
        db.execute("DROP TABLE sma_replay_license", []).unwrap();
        assert_eq!(get(&process, PATH).await.0, StatusCode::NOT_FOUND);
        let result = replay_synthetic_sma(root.path());
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(get(&process, PATH).await, (StatusCode::OK, page));

        // An older writer may advance a checkpoint without updating proof.
        db.execute(
            "UPDATE decision_checkpoint SET replay_offset=replay_offset+1",
            [],
        )
        .unwrap();
        assert_eq!(get(&process, PATH).await.0, StatusCode::NOT_FOUND);

        let mixed = tempfile::tempdir().unwrap();
        write_synthetic_sma(mixed.path(), first_price, "fixture.display-only", &plan);
        publish_plan(mixed.path(), "sma_crossover", true, plan_hash);
        let result = replay_synthetic_sma(mixed.path());
        assert_eq!(result.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&result.stderr).contains("replay license changed"));
        let (status, body) = get(&app(mixed.path()), PATH).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"]["code"], "decision_not_published");
        let db = Connection::open(mixed.path().join(STORE).join("decisions.sqlite3")).unwrap();
        assert_eq!(
            db.query_row("SELECT count(*) FROM decisions", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn decision_publication_and_database_refuse_links_below_data_root() {
    use std::os::unix::fs::symlink;

    let outside = tempfile::tempdir().unwrap();
    publication(outside.path(), "strategy_bundle", true);
    let database = outside.path().join(STORE).join("decisions.sqlite3");
    drop(Connection::open(&database).unwrap());
    for parent in ["finance-state", STORE] {
        let root = tempfile::tempdir().unwrap();
        let link = root.path().join(parent);
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        symlink(outside.path().join(parent), &link).unwrap();
        let (status, body) = get(&app(root.path()), PATH).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{parent}");
        assert_eq!(body["error"]["code"], "decision_storage_error");
        assert!(body.get("access").is_none());
    }

    let root = tempfile::tempdir().unwrap();
    publication(root.path(), "strategy_bundle", true);
    symlink(database, root.path().join(STORE).join("decisions.sqlite3")).unwrap();
    assert_eq!(
        get(&app(root.path()), PATH).await.0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
}
