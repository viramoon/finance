use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use finance_api::{AppState, router};
use serde_json::{Value, json};
use tower::ServiceExt;

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
