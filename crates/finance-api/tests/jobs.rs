use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use finance_api::{AppState, router};
use serde_json::{Value, json};
use tower::ServiceExt;

const BODY: &str = r#"{"strategy":"DemoStrategy","instrument":"6EH4.XCME","start":"2024-01-01T23:02:00Z","end":"2024-01-01T23:06:00Z","params":{}}"#;
async fn request(
    root: &tempfile::TempDir,
    method: &str,
    path: &str,
    key: &str,
    body: &str,
) -> (u16, Value) {
    let app = router(AppState {
        data_root: root.path().to_owned(),
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
