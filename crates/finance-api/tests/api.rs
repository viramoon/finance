use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use finance_api::{ApiDoc, AppState, config::Settings, router};
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs};
use tower::ServiceExt;
use utoipa::OpenApi;

async fn call(app: Router, path: &str) -> (StatusCode, Value) {
    let response = app
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    (
        response.status(),
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap(),
    )
}
fn summary() -> Value {
    json!({"run_id":"test-1","strategy":"DemoStrategy","instrument":"6EH4.XCME",
    "start":"2024-01-01T23:01:00Z","end":"2024-01-01T23:09:00Z","total_orders":0,
    "total_positions":0,"pnl":"12345678901234567890.12345678","currency":"USD","generated_at":"2024-01-02T00:00:00Z"})
}
#[tokio::test]
async fn live_ready_and_missing_result() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(AppState {
        data_root: dir.path().into(),
    });
    assert_eq!(call(app.clone(), "/healthz/live").await.0, StatusCode::OK);
    let (code, health) = call(app.clone(), "/healthz/ready").await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(
        health["checks"][0],
        json!({"name":"data_root_readable","ok":true})
    );
    let (code, body) = call(app, "/api/v1/backtests/sample").await;
    assert_eq!(code, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "sample_not_found");
    // 不存在的目录，以及普通文件，均不能作为可读数据根目录。
    for root in [dir.path().join("missing"), dir.path().join("not-directory")] {
        if root.ends_with("not-directory") {
            fs::write(&root, "file").unwrap();
        }
        let app = router(AppState { data_root: root });
        let (code, health) = call(app.clone(), "/healthz/ready").await;
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(health["checks"][0]["ok"], false);
        assert_eq!(call(app, "/healthz/live").await.0, StatusCode::OK);
    }
}
#[tokio::test]
async fn sample_exact_amount_and_corrupt_results() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir
        .path()
        .join("finance-state/backtests/sample/latest.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let app = router(AppState {
        data_root: dir.path().into(),
    });
    let expected = summary();
    fs::write(&path, expected.to_string()).unwrap();
    let (status, actual) = call(app.clone(), "/api/v1/backtests/sample").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(actual, expected);
    for invalid in [
        "broken".to_string(),
        {
            let mut v = summary();
            v["pnl"] = json!(1.2);
            v.to_string()
        },
        {
            let mut v = summary();
            v["pnl"] = json!("NaN");
            v.to_string()
        },
    ] {
        fs::write(&path, invalid).unwrap();
        let (status, body) = call(app.clone(), "/api/v1/backtests/sample").await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body["error"]["code"], "sample_invalid");
    }
}
#[test]
fn config_layers_and_invalid_values() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("base.yaml"),
        "api: {bind_host: 127.0.0.1, port: 8081}\npaths: {data_root: /base}\n",
    )
    .unwrap();
    fs::write(dir.path().join("stable.yaml"), "api: {port: 8082}\n").unwrap();
    let mut env = BTreeMap::from([("FERTILE_PROFILE".into(), "stable".into())]);
    assert_eq!(Settings::load(dir.path(), &env).unwrap().api.port, 8082);
    env.insert("FINANCE_API_PORT".into(), "8083".into());
    env.insert("FERTILE_DATA_ROOT".into(), "/override".into());
    let settings = Settings::load(dir.path(), &env).unwrap();
    assert_eq!(settings.api.port, 8083);
    assert_eq!(settings.paths.data_root.to_str().unwrap(), "/override");
    env.insert("FINANCE_API_PORT".into(), "0".into());
    assert!(Settings::load(dir.path(), &env).is_err());
    env.remove("FINANCE_API_PORT");
    env.insert("FERTILE_DATA_ROOT".into(), "relative".into());
    assert!(Settings::load(dir.path(), &env).is_err());
    env.insert("FERTILE_PROFILE".into(), "../secret".into());
    assert!(Settings::load(dir.path(), &env).is_err());
    env.insert("FERTILE_PROFILE".into(), "stable".into());
    env.remove("FERTILE_DATA_ROOT");
    fs::write(dir.path().join("stable.yaml"), "unexpected: true\n").unwrap();
    assert!(Settings::load(dir.path(), &env).is_err());
}
#[test]
fn openapi_has_routes_errors_and_string_amount() {
    let spec = serde_json::to_value(ApiDoc::openapi()).unwrap();
    for path in [
        "/healthz/live",
        "/healthz/ready",
        "/api/v1/backtests/sample",
    ] {
        assert!(spec["paths"][path]["get"].is_object());
    }
    assert!(spec["paths"]["/healthz/ready"]["get"]["responses"]["503"].is_object());
    assert!(spec["paths"]["/api/v1/backtests/sample"]["get"]["responses"]["404"].is_object());
    assert_eq!(
        spec["components"]["schemas"]["BacktestSummary"]["properties"]["pnl"]["type"],
        "string"
    );
}
