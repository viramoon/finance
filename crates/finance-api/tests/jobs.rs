use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use finance_api::{AppState, router};
use serde_json::{Value, json};
use tower::ServiceExt;

#[path = "support/backtest_list_fixture.rs"]
mod fixture;
use fixture::{BUNDLE, insert_list_fixture};

const BODY: &str = r#"{"strategy":"DemoStrategy","instrument":"6EH4.XCME","start":"2024-01-01T23:02:00Z","end":"2024-01-01T23:06:00Z","params":{}}"#;
const DAILY: &str = r#"{"strategy":"SmaCrossover","instrument":"IBM.XNYS","start":"2025-01-02T00:00:00Z","end":"2025-03-04T00:00:00Z","params":{"fast_period":5,"slow_period":20,"shares":10}}"#;

async fn request(
    root: &tempfile::TempDir,
    method: &str,
    path: &str,
    key: &str,
    body: &str,
) -> (u16, Value) {
    let app = router(AppState {
        data_root: root.path().to_owned(),
        research_root: root.path().join("research"),
    });
    let req = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .header("Idempotency-Key", key)
        .body(Body::from(body.to_owned()))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    let status = res.status();
    let data = to_bytes(res.into_body(), 1 << 20).await.unwrap();
    (status.as_u16(), serde_json::from_slice(&data).unwrap())
}

#[tokio::test]
async fn legacy_visibility_migrates_existing_queue_and_preserves_unfiltered_pages() {
    let root = tempfile::tempdir().unwrap();
    let path = "/api/v1/backtests";
    let filtered = "/api/v1/backtests?visibility=legacy&limit=1";
    let queue = root.path().join("finance-state/backtests");
    std::fs::create_dir_all(&queue).unwrap();
    let db = rusqlite::Connection::open(queue.join("jobs.sqlite3")).unwrap();
    // Queue schema before this migration. The old worker and API write these columns.
    let old_schema = "CREATE TABLE IF NOT EXISTS jobs (
        id INTEGER PRIMARY KEY AUTOINCREMENT, idem TEXT UNIQUE NOT NULL, request TEXT NOT NULL,
        status TEXT NOT NULL DEFAULT 'queued'
            CHECK(status IN ('queued','running','succeeded','failed','timed_out')),
        attempt INTEGER NOT NULL DEFAULT 0 CHECK(attempt BETWEEN 0 AND 2),
        summary TEXT, error TEXT,
        created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
    );
    CREATE INDEX IF NOT EXISTS jobs_status_id ON jobs(status,id);";
    db.execute_batch(old_schema).unwrap();
    insert_list_fixture(&db, 1, BODY);
    insert_list_fixture(&db, 2, BUNDLE);

    let public_job = json!({
        "id": 1, "status": "failed", "request": serde_json::from_str::<Value>(BODY).unwrap(),
        "attempt": 0, "created_at": "2025-01-02T00:00:00Z", "summary": null, "error": null
    });
    let private_job = json!({
        "id": 2, "status": "failed", "attempt": 0, "created_at": "2025-01-02T00:00:00Z",
        "summary": null, "error": null,
        "request": {"strategy": "StrategyBundle", "start": "2025-01-02T00:00:00Z",
            "end": "2025-03-04T00:00:00Z",
            "source": {"kind": "registered", "id": "synthetic.bundle", "version": "1.0.0"},
            "content_sha256": "a".repeat(64)}
    });
    assert_eq!(
        request(&root, "GET", filtered, "", "").await,
        (200, json!({"items": [public_job], "next_cursor": null}))
    );
    for (suffix, expected) in [
        (
            "",
            json!({"items": [public_job, private_job], "next_cursor": null}),
        ),
        ("?limit=1", json!({"items": [public_job], "next_cursor": 1})),
        (
            "?limit=1&cursor=1",
            json!({"items": [private_job], "next_cursor": null}),
        ),
    ] {
        assert_eq!(
            request(&root, "GET", &format!("{path}{suffix}"), "", "").await,
            (200, expected)
        );
    }

    // Simulate an older API reopening the upgraded database and submitting a job.
    drop(db);
    let db = rusqlite::Connection::open(queue.join("jobs.sqlite3")).unwrap();
    db.execute_batch(old_schema).unwrap();
    let old_rows = db
        .prepare("SELECT id,request FROM jobs WHERE id>?1 ORDER BY id LIMIT ?2")
        .unwrap()
        .query_map([0, 3], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<Vec<(i64, String)>>>()
        .unwrap();
    assert_eq!(old_rows, [(1, BODY.to_owned()), (2, BUNDLE.to_owned())]);
    db.execute(
        "INSERT INTO jobs(idem,request) VALUES ('older-api',?1)",
        [DAILY],
    )
    .unwrap();
    assert_eq!(
        request(&root, "POST", path, "upgraded-api", BODY).await,
        (202, json!({"id": 4}))
    );
    let (status, page) = request(&root, "GET", filtered, "", "").await;
    assert_eq!((status, page["next_cursor"].as_i64()), (200, Some(1)));
    let (status, page) = request(
        &root,
        "GET",
        "/api/v1/backtests?visibility=legacy&cursor=1",
        "",
        "",
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(page["items"][0]["request"]["strategy"], "SmaCrossover");
    assert_eq!(page["items"][1]["request"]["strategy"], "DemoStrategy");
    assert_eq!(page["items"].as_array().unwrap().len(), 2);
    assert_eq!(page["next_cursor"], Value::Null);
}

#[tokio::test]
async fn legacy_visibility_pages_across_ten_thousand_private_jobs_with_exact_i64_cursors() {
    let root = tempfile::tempdir().unwrap();
    let path = "/api/v1/backtests?visibility=legacy";
    let empty = (200, json!({"items": [], "next_cursor": null}));
    assert_eq!(request(&root, "GET", path, "", "").await, empty);
    let mut db =
        rusqlite::Connection::open(root.path().join("finance-state/backtests/jobs.sqlite3"))
            .unwrap();
    insert_list_fixture(&db, 2, BUNDLE);
    assert_eq!(request(&root, "GET", path, "", "").await, empty);

    let public_ids = [1, (1_i64 << 52) + 1, (1_i64 << 53) + 1, i64::MAX];
    for id in public_ids {
        insert_list_fixture(&db, id, BODY);
    }
    let route = |cursor| format!("{path}&limit=1&cursor={cursor}");
    let mut expected = Vec::new();
    for (idx, id) in public_ids.into_iter().enumerate() {
        let cursor = if idx == 0 { 0 } else { public_ids[idx - 1] };
        let (status, page) = request(&root, "GET", &route(cursor), "", "").await;
        assert_eq!(status, 200);
        assert_eq!(page["items"].as_array().unwrap().len(), 1);
        assert_eq!(page["items"][0]["id"].as_i64(), Some(id));
        assert_eq!(
            page["next_cursor"],
            if idx + 1 < public_ids.len() {
                json!(id)
            } else {
                Value::Null
            }
        );
        expected.push((cursor, page));
    }

    let tx = db.transaction().unwrap();
    // Deterministic synthetic history, using consecutive IDs and a fixed request.
    for offset in 1..=10_000 {
        insert_list_fixture(&tx, public_ids[1] + offset, BUNDLE);
    }
    insert_list_fixture(&tx, i64::MAX - 1, BUNDLE);
    tx.commit().unwrap();
    for (cursor, page) in expected {
        assert_eq!(
            request(&root, "GET", &route(cursor), "", "").await,
            (200, page)
        );
    }
    assert_eq!(request(&root, "GET", &route(i64::MAX), "", "").await, empty);
}

#[tokio::test]
async fn legacy_visibility_rejects_invalid_values_and_cursors() {
    let root = tempfile::tempdir().unwrap();
    for query in [
        "visibility=",
        "visibility=all",
        "visibility=legacy&visibility=legacy",
        "visibility=legacy&cursor=-1",
        "visibility=legacy&cursor=9223372036854775808",
        "visibility=legacy&limit=101",
    ] {
        let (status, body) =
            request(&root, "GET", &format!("/api/v1/backtests?{query}"), "", "").await;
        assert_eq!(status, 422, "{query}");
        assert_eq!(body["error"]["code"], "invalid_request", "{query}");
    }
}

#[tokio::test]
async fn concurrent_keys_and_pagination() {
    let temp = tempfile::tempdir().unwrap();
    let path = "/api/v1/backtests";
    let (a, b) = tokio::join!(
        request(&temp, "POST", path, "k", BODY),
        request(&temp, "POST", path, "k", BODY)
    );
    assert_eq!(a.0, 202);
    assert_eq!(a, b);
    let second = request(&temp, "POST", path, "other", BODY).await;
    assert_ne!(a.1["id"], second.1["id"]);
    let id = a.1["id"].as_i64().unwrap();
    let detail = request(&temp, "GET", &format!("{path}/{id}"), "", "").await;
    assert_eq!(detail.1["status"], "queued");
    assert_eq!(detail.1["summary"], Value::Null);
    let page = request(&temp, "GET", "/api/v1/backtests?limit=1", "", "").await;
    assert_eq!(page.1["next_cursor"], a.1["id"]);
    let last = request(&temp, "GET", &format!("{path}?limit=1&cursor={id}"), "", "").await;
    assert_eq!(last.1["items"][0]["id"], second.1["id"]);
    assert_eq!(last.1["next_cursor"], Value::Null);
    assert_eq!(
        request(&temp, "POST", path, "k", &BODY.replace("23:02", "23:03"))
            .await
            .0,
        409
    );
    let conn = rusqlite::Connection::open(temp.path().join("finance-state/backtests/jobs.sqlite3"))
        .unwrap();
    for i in 0..126 {
        conn.execute(
            "INSERT INTO jobs(idem,request) VALUES (?1,?2)",
            rusqlite::params![format!("fill-{i}"), BODY],
        )
        .unwrap();
    }
    assert_eq!(request(&temp, "POST", path, "full", BODY).await.0, 429);
    assert_eq!(request(&temp, "POST", path, "k", BODY).await, a);
}
#[tokio::test]
async fn rejects_inputs_with_structured_errors() {
    let temp = tempfile::tempdir().unwrap();
    let path = "/api/v1/backtests";
    for body in [
        BODY.replace("6EH4.XCME", "unknown"),
        BODY.replace("DemoStrategy", "import"),
        BODY.replace("23:02", "23:20"),
        BODY.replace("{}", "{\"x\":1}"),
        BODY.replace("23:06", "23:02"),
        "x".repeat(9000),
    ] {
        let (code, data) = request(&temp, "POST", path, "k", &body).await;
        assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY.as_u16());
        assert_eq!(data["error"]["code"], json!("invalid_request"));
    }
    assert_eq!(request(&temp, "POST", path, "", BODY).await.0, 422);
    assert_eq!(
        request(&temp, "GET", "/api/v1/backtests/123", "", "")
            .await
            .0,
        404
    );
    assert_eq!(
        request(&temp, "GET", "/api/v1/backtests?cursor=-1", "", "")
            .await
            .0,
        422
    );
}

#[tokio::test]
async fn historical_factor_momentum_cannot_enter_backtest_queue() {
    let temp = tempfile::tempdir().unwrap();
    let path = "/api/v1/backtests";
    for strategy in ["research.factor-momentum", "research.factor-momentum@1.0.0"] {
        let body = BODY.replace("DemoStrategy", strategy);
        let (status, data) = request(&temp, "POST", path, "historical", &body).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY.as_u16());
        assert_eq!(data["error"]["code"], "invalid_request");
    }
    let (status, data) = request(&temp, "GET", path, "", "").await;
    assert_eq!(status, StatusCode::OK.as_u16());
    assert_eq!(data["items"], json!([]));
}

#[tokio::test]
async fn queue_access_does_not_recreate_missing_data_mount() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("missing-mount");
    let app = router(AppState {
        data_root: root.clone(),
        research_root: root.join("research"),
    });
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/backtests")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert!(!root.exists());
}

#[tokio::test]
async fn reviewed_yahoo_split_catalog_is_listed_and_can_submit_a_job() {
    let temp = tempfile::tempdir().unwrap();
    let dataset = temp.path().join("research/IBM.XNYS");
    std::fs::create_dir_all(&dataset).unwrap();
    let manifest = dataset.join("dataset.json");
    let mut metadata = json!({
        "instrument": "IBM.XNYS",
        "asset_type": "stock",
        "source": "Constructed fixture",
        "source_sha256": "a".repeat(64),
        "data_kind": "fixture",
        "calendar": "XNYS",
        "first_session": "1975-11-25",
        "last_session": "2025-03-04",
        "observations": 12400,
        "price_basis": "yahoo_chart_split_normalized",
        "corporate_actions": "cash_dividends_excluded_yahoo_split_normalized_reviewed"
    });
    std::fs::write(&manifest, metadata.to_string()).unwrap();
    let (status, listed) = request(&temp, "GET", "/api/v1/backtests/datasets", "", "").await;
    assert_eq!(status, 200);
    assert_eq!(listed, json!({"items": [metadata.clone()]}));
    assert_eq!(
        request(&temp, "POST", "/api/v1/backtests", "yahoo", DAILY)
            .await
            .0,
        202
    );

    metadata["corporate_actions"] = json!("cash_dividends_excluded_split_free_reviewed");
    std::fs::write(&manifest, metadata.to_string()).unwrap();
    assert_eq!(
        request(&temp, "GET", "/api/v1/backtests/datasets", "", "")
            .await
            .0,
        500
    );
}

#[tokio::test]
async fn historical_catalog_and_lookup_use_the_committed_request() {
    let t = tempfile::tempdir().unwrap();
    let path = "/api/v1/backtests";
    let dataset = t.path().join("research/IBM.XNYS");
    assert_eq!(
        request(&t, "GET", &format!("{path}/datasets"), "", "").await,
        (200, json!({"items":[]}))
    );
    assert_eq!(
        request(&t, "POST", path, "daily", DAILY).await.1["error"]["code"],
        "no_data"
    );
    std::fs::create_dir_all(&dataset).unwrap();
    std::fs::write(dataset.join("dataset.json"), json!({
        "instrument":"IBM.XNYS", "asset_type":"stock", "source":"fixture", "source_sha256":"a".repeat(64),
        "data_kind":"fixture", "calendar":"XNYS", "first_session":"2025-01-02", "last_session":"2025-03-04",
        "observations":42, "price_basis":"raw_unadjusted", "corporate_actions":"cash_dividends_excluded_split_free_reviewed"
    }).to_string()).unwrap();
    assert_eq!(
        request(&t, "GET", &format!("{path}/datasets"), "", "")
            .await
            .1["items"][0]["instrument"],
        "IBM.XNYS"
    );
    for bad in [
        DAILY.replace("2025-01-02", "1969-12-31"),
        DAILY.replace("\"shares\":10", "\"shares\":0"),
        DAILY.replace("IBM.XNYS", "^GSPC.XNYS"),
    ] {
        assert_eq!(request(&t, "POST", path, "bad", &bad).await.0, 422);
    }
    let lookup = format!("{path}/by-key");
    assert_eq!(request(&t, "GET", &lookup, "daily", "").await.0, 404);
    let accepted = request(&t, "POST", path, "daily", DAILY).await;
    assert_eq!(accepted.0, 202);
    assert_eq!(
        request(&t, "GET", &lookup, "daily", "").await,
        (200, accepted.1.clone())
    );
    // A retry of previously committed work can be identified even if the catalog goes away.
    std::fs::remove_file(dataset.join("dataset.json")).unwrap();
    assert_eq!(request(&t, "POST", path, "daily", DAILY).await, accepted);
}

#[tokio::test]
async fn cancellation_is_idempotent_and_preserves_legacy_job_and_key_contract() {
    let root = tempfile::tempdir().unwrap();
    let queue = "/api/v1/backtests";
    for (id, key) in [(1, "queued"), (2, "running"), (3, "done")] {
        assert_eq!(
            request(&root, "POST", queue, key, BODY).await,
            (202, json!({"id":id}))
        );
    }
    let conn = rusqlite::Connection::open(root.path().join("finance-state/backtests/jobs.sqlite3"))
        .unwrap();
    conn.execute_batch(
        "UPDATE jobs SET status='running',attempt=1 WHERE id=2;
         INSERT INTO job_leases VALUES(2,9223372036854775807);
         UPDATE jobs SET status='succeeded',attempt=1 WHERE id=3;",
    )
    .unwrap();
    let cancel_first = format!("{queue}/1/cancel");
    let (a, b) = tokio::join!(
        request(&root, "POST", &cancel_first, "", ""),
        request(&root, "POST", &cancel_first, "", ""),
    );
    assert_eq!(a, b);
    assert_eq!(a.0, 200);
    assert_eq!(a.1["attempt"], 0);
    for (id, key) in [(1, "queued"), (2, "running")] {
        let (status, job) = request(&root, "POST", &format!("{queue}/{id}/cancel"), "", "").await;
        assert_eq!(status, 200);
        assert_eq!(job["status"], "failed");
        assert_eq!(job["error"], "job_cancelled");
        assert_eq!(job["summary"], Value::Null);
        assert_eq!(
            request(&root, "GET", &format!("{queue}/{id}"), "", "").await,
            (200, job)
        );
        assert_eq!(
            request(&root, "GET", &format!("{queue}/by-key"), key, "").await,
            (200, json!({"id":id}))
        );
        assert_eq!(
            request(&root, "POST", queue, key, BODY).await,
            (202, json!({"id":id}))
        );
    }
    assert_eq!(
        conn.query_row("SELECT job_id FROM job_leases", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        2
    );
    for (id, code, error) in [
        (3, 409, "job_already_finished"),
        (4, 404, "job_not_found"),
        (0, 422, "invalid_request"),
    ] {
        let (status, body) = request(&root, "POST", &format!("{queue}/{id}/cancel"), "", "").await;
        assert_eq!(status, code);
        assert_eq!(body["error"]["code"], error);
    }
    assert_eq!(
        request(&root, "GET", &format!("{queue}/3"), "", "").await.1["status"],
        "succeeded"
    );
}

#[tokio::test]
async fn worker_status_is_read_only_and_counts_legacy_and_current_leases() {
    let root = tempfile::tempdir().unwrap();
    let path = "/api/v1/backtests/worker-status";
    let (status, empty) = request(&root, "GET", path, "", "").await;
    assert_eq!(status, 200);
    assert_eq!(empty["queue_present"], false);
    assert_eq!(empty["running"], 0);
    assert_eq!(root.path().read_dir().unwrap().count(), 0);

    let queue = "/api/v1/backtests";
    for key in ["queued", "active", "expired", "legacy"] {
        assert_eq!(request(&root, "POST", queue, key, BODY).await.0, 202);
    }
    let conn = rusqlite::Connection::open(root.path().join("finance-state/backtests/jobs.sqlite3"))
        .unwrap();
    conn.execute_batch(
        "UPDATE jobs SET status='running',attempt=1 WHERE id>1;
         INSERT INTO job_leases VALUES(2,9223372036854775807),(3,0);",
    )
    .unwrap();
    let (status, body) = request(&root, "GET", path, "", "").await;
    assert_eq!(status, 200);
    assert_eq!(body["queue_present"], true);
    assert_eq!(body["queued"], 1);
    assert_eq!(body["running"], 3);
    assert_eq!(body["leased_running"], 1);
    assert_eq!(body["unleased_running"], 2);
    assert_eq!(
        conn.query_row("SELECT count(*) FROM job_leases", [], |row| row
            .get::<_, u32>(0))
            .unwrap(),
        2
    );
    conn.execute_batch("DROP TABLE job_leases;").unwrap();
    let (status, body) = request(&root, "GET", path, "", "").await;
    assert_eq!(status, 200);
    assert_eq!(body["running"], 3);
    assert_eq!(body["leased_running"], 0);
    assert_eq!(body["unleased_running"], 3);
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name='job_leases'",
            [],
            |row| row.get::<_, u32>(0)
        )
        .unwrap(),
        0
    );
}

#[tokio::test]
async fn cancellation_of_old_running_job_without_lease_is_recovered_after_unlock() {
    use std::{fs, path::Path, process::Command};

    let root = tempfile::tempdir().unwrap();
    assert_eq!(
        request(&root, "POST", "/api/v1/backtests", "legacy", BODY)
            .await
            .0,
        202
    );
    let directory = root.path().join("finance-state/backtests");
    let conn = rusqlite::Connection::open(directory.join("jobs.sqlite3")).unwrap();
    conn.execute_batch(
        "UPDATE jobs SET status='running',attempt=1 WHERE id=1; DROP TABLE job_leases;",
    )
    .unwrap();
    fs::create_dir(directory.join("run-1-1-synthetic")).unwrap();
    let (status, job) = request(&root, "POST", "/api/v1/backtests/1/cancel", "", "").await;
    assert_eq!(status, 200);
    assert_eq!(job["error"], "job_cancelled");

    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let recovered = Command::new("python3")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("PYTHONPATH", repo.join("python"))
        .args([
            "-B",
            "-c",
            r#"
from contextlib import closing
import fcntl
from pathlib import Path
import sys

from fertile_finance import worker

directory = Path(sys.argv[1])
guard_path = worker.guard_path(directory, 1)
scratch = directory / "run-1-1-synthetic"
with closing(worker.open_queue(directory)) as db:
    assert db.execute("SELECT count(*) FROM job_leases").fetchone() == (0,)
    with worker.open_guard(guard_path, create=True) as guard:
        fcntl.flock(guard, fcntl.LOCK_EX)
        worker.recover(db, directory)
        assert guard_path.is_file() and scratch.is_dir()
    worker.recover(db, directory)
    assert not guard_path.exists() and not scratch.exists()
    assert worker.claim(db, directory, 1) is None
"#,
        ])
        .arg(directory)
        .output()
        .unwrap();
    assert!(
        recovered.status.success(),
        "{}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert_eq!(
        request(&root, "GET", "/api/v1/backtests/1", "", "").await,
        (200, job)
    );
}

#[cfg(unix)]
#[tokio::test]
async fn cancellation_refuses_linked_queue_without_writing_outside_data_root() {
    use std::{fs, os::unix::fs::symlink};

    let outside = tempfile::tempdir().unwrap();
    assert_eq!(
        request(&outside, "POST", "/api/v1/backtests", "outside", BODY)
            .await
            .0,
        202
    );
    let database = "finance-state/backtests/jobs.sqlite3";
    let original = fs::read(outside.path().join(database)).unwrap();
    for component in ["finance-state", "finance-state/backtests", database] {
        let root = tempfile::tempdir().unwrap();
        let link = root.path().join(component);
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        symlink(outside.path().join(component), link).unwrap();
        let (status, body) = request(&root, "POST", "/api/v1/backtests/1/cancel", "", "").await;
        assert_eq!(status, 500, "{component}");
        assert_eq!(body["error"]["code"], "queue_error");
        assert_eq!(fs::read(outside.path().join(database)).unwrap(), original);
    }

    let root = tempfile::tempdir().unwrap();
    let outside_empty = tempfile::tempdir().unwrap();
    symlink(outside_empty.path(), root.path().join("finance-state")).unwrap();
    assert_eq!(
        request(&root, "POST", "/api/v1/backtests/1/cancel", "", "")
            .await
            .0,
        500
    );
    assert_eq!(outside_empty.path().read_dir().unwrap().count(), 0);
}
