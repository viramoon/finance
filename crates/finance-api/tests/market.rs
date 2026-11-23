use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use chrono::{Duration, SecondsFormat, Utc};
use finance_api::{AppState, router};
use serde_json::{Value, json};
use std::{fs, path::Path};
use tower::ServiceExt;

async fn market(root: &Path) -> Value {
    let r = router(AppState {
        data_root: root.into(),
        research_root: root.join("research"),
    })
    .oneshot(
        Request::get("/api/v1/market/status")
            .body(Body::empty())
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(r.status(), 200);
    serde_json::from_slice(&to_bytes(r.into_body(), 8192).await.unwrap()).unwrap()
}
#[tokio::test]
async fn missing_expired_stale_and_current_reports() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    assert_eq!(market(root).await["state"], "missing");
    let dir = root.join("finance-state/collector");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("status.json");
    let now = Utc::now();
    let timestamp =
        |age| (now - Duration::seconds(age)).to_rfc3339_opts(SecondsFormat::Nanos, true);
    let mut report = json!({"state":"ready","source":"replay","updated_at":timestamp(0),"max_age_seconds":10,"instruments":[{
      "instrument":"SPY.ARCA","last_bar_at":timestamp(1),"age_seconds":0,"stale":false
    }]});
    fs::write(&path, report.to_string()).unwrap();
    assert_eq!(market(root).await["state"], "ready");
    report["instruments"][0]["last_bar_at"] = json!(timestamp(60));
    fs::write(&path, report.to_string()).unwrap();
    assert_eq!(market(root).await["instruments"][0]["stale"], true);
    report["updated_at"] = json!(timestamp(120));
    fs::write(&path, report.to_string()).unwrap();
    assert_eq!(market(root).await["state"], "expired");
    fs::write(&path, b"bad json").unwrap();
    assert_eq!(market(root).await["state"], "invalid");
}
