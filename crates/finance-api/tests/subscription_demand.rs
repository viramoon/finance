use std::sync::Arc;

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Method, Request, StatusCode, header},
};
use finance_api::{
    ApiDoc, AppState,
    market_options::{DepthFrames, Licensed, MarketReadSources},
    router_with_market_reads,
};
use finance_collector::line_budget::{
    BudgetConfig, LineScheduler, SubscriptionDemand, SubscriptionDemandHandle,
};
use finance_ib::depth::DepthSymbol;
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;
use utoipa::OpenApi;

const DEMAND: &str = "/api/v1/market/subscriptions/demand";
const BUDGET: &str = "/api/v1/market/subscriptions";
const INVALID: StatusCode = StatusCode::BAD_REQUEST;

fn app(root: &TempDir, sources: MarketReadSources) -> Router {
    router_with_market_reads(
        AppState {
            data_root: root.path().into(),
            research_root: root.path().join("research"),
        },
        sources,
    )
}

async fn request(
    app: &Router,
    path: &str,
    method: Method,
    body: Option<&str>,
    status: StatusCode,
) -> Value {
    let mut request = Request::builder().method(method).uri(path);
    if body.is_some() {
        request = request.header(header::CONTENT_TYPE, "application/json");
    }
    let response = app
        .clone()
        .oneshot(
            request
                .body(body.map_or_else(Body::empty, |text| Body::from(text.to_owned())))
                .unwrap(),
        )
        .await
        .unwrap();
    let code = response.status();
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "private, no-store"
    );
    let body = to_bytes(response.into_body(), 16_384).await.unwrap();
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(code, status, "{value}");
    value
}

async fn get(app: &Router, path: &str, status: StatusCode) -> Value {
    request(app, path, Method::GET, None, status).await
}

async fn put(app: &Router, body: &str, status: StatusCode) -> Value {
    request(app, DEMAND, Method::PUT, Some(body), status).await
}

#[tokio::test]
async fn default_and_detached_owner_return_disabled_without_retaining_demand() {
    // In-process router with no session, remote endpoint, or collector process.
    let root = tempfile::tempdir().unwrap();
    let default_app = app(&root, MarketReadSources::default());
    let disabled = json!({
        "state": "disabled", "demand": {"watchlist": [], "depth_focus": null},
        "budget": null, "depth_budget": {"pinned_slots": 2, "page_focus_slots": 1, "access": null}
    });
    assert_eq!(get(&default_app, DEMAND, StatusCode::OK).await, disabled);
    for body in [
        "{}",
        r#"{"watchlist":[],"depth_focus":null}"#,
        r#"{"watchlist":["BTC-USDT.BINANCE"],"depth_focus":"AAPL.NASDAQ"}"#,
    ] {
        assert_eq!(put(&default_app, body, StatusCode::OK).await, disabled);
    }
    assert_eq!(
        put(&default_app, r#"{"watchlist":["FAKE.NASDAQ"]}"#, INVALID).await["error"]["code"],
        "subscription_demand_invalid"
    );
    assert_eq!(
        get(&default_app, BUDGET, StatusCode::SERVICE_UNAVAILABLE).await["error"]["code"],
        "market_read_source_unavailable"
    );

    let (handle, owner) = SubscriptionDemandHandle::channel();
    let detached_app = app(
        &root,
        MarketReadSources {
            subscription_demand: Some(handle),
            ..Default::default()
        },
    );
    assert_eq!(
        put(
            &detached_app,
            r#"{"watchlist":["SPY.ARCA"]}"#,
            StatusCode::OK
        )
        .await["state"],
        "enabled"
    );
    drop(owner);
    assert_eq!(get(&detached_app, DEMAND, StatusCode::OK).await, disabled);
    assert_eq!(put(&detached_app, "{}", StatusCode::OK).await, disabled);
    assert_eq!(root.path().read_dir().unwrap().count(), 0);
}

#[tokio::test]
async fn publishes_one_canonical_snapshot_and_omission_clears_each_field() {
    let root = tempfile::tempdir().unwrap();
    let (handle, mut owner) = SubscriptionDemandHandle::channel();
    let app = app(
        &root,
        MarketReadSources {
            subscription_demand: Some(handle),
            ..Default::default()
        },
    );
    assert_eq!(get(&app, DEMAND, StatusCode::OK).await["state"], "enabled");
    assert!(!owner.has_changed().unwrap());

    let result = put(
        &app,
        r#"{"watchlist":["SPY.ARCA","ETH-USD-PERP.HYPERLIQUID","SPY.ARCA"],"depth_focus":"AAPL.NASDAQ"}"#,
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        result["demand"],
        json!({"watchlist":["ETH-USD-PERP.HYPERLIQUID","SPY.ARCA"],"depth_focus":"AAPL.NASDAQ"})
    );
    assert!(owner.has_changed().unwrap());
    {
        let snapshot = owner.borrow_and_update();
        assert_eq!(serde_json::to_value(&*snapshot).unwrap(), result["demand"]);
        assert_eq!(snapshot.depth_focus(), Some(DepthSymbol::Aapl));
        assert_eq!(snapshot.watchlist()[0].as_str(), "ETH-USD-PERP.HYPERLIQUID");
    }

    let repeated = put(
        &app,
        r#"{"depth_focus":"AAPL.NASDAQ","watchlist":["ETH-USD-PERP.HYPERLIQUID","SPY.ARCA"]}"#,
        StatusCode::OK,
    )
    .await;
    assert_eq!(repeated, result);
    assert_eq!(get(&app, DEMAND, StatusCode::OK).await, result);
    assert!(!owner.has_changed().unwrap());

    let focus_only = put(&app, r#"{"depth_focus":"QQQ.NASDAQ"}"#, StatusCode::OK).await;
    assert_eq!(
        focus_only["demand"],
        json!({"watchlist":[],"depth_focus":"QQQ.NASDAQ"})
    );
    assert_eq!(
        owner.borrow_and_update().depth_focus(),
        Some(DepthSymbol::Qqq)
    );

    let watchlist_only = put(&app, r#"{"watchlist":["TSLA.NASDAQ"]}"#, StatusCode::OK).await;
    assert_eq!(
        watchlist_only["demand"],
        json!({"watchlist":["TSLA.NASDAQ"],"depth_focus":null})
    );
    assert!(owner.borrow_and_update().depth_focus().is_none());

    let cleared = put(&app, "{}", StatusCode::OK).await;
    assert_eq!(
        cleared["demand"],
        json!({"watchlist":[],"depth_focus":null})
    );
    assert_eq!(*owner.borrow_and_update(), SubscriptionDemand::default());
    assert_eq!(
        put(
            &app,
            r#"{"watchlist":[],"depth_focus":null}"#,
            StatusCode::OK
        )
        .await,
        cleared
    );
    assert!(!owner.has_changed().unwrap());
}

#[tokio::test]
async fn validates_full_registry_and_rejects_entire_invalid_or_private_payload() {
    // Fixed synthetic registry extension. There is no quote or upstream account.
    let root = tempfile::tempdir().unwrap();
    let (handle, mut owner) = SubscriptionDemandHandle::channel();
    let app = app(
        &root,
        MarketReadSources {
            subscription_demand: Some(handle),
            subscription_registry: ["SYNTHETIC.XNAS", "AAPL.NASDAQ", "SPY.ARCA"]
                .into_iter()
                .map(|id| id.parse().unwrap())
                .collect(),
            ..Default::default()
        },
    );
    let accepted = put(
        &app,
        r#"{"watchlist":["SYNTHETIC.XNAS"],"depth_focus":"AAPL.NASDAQ"}"#,
        StatusCode::OK,
    )
    .await;
    let snapshot = owner.borrow_and_update().clone();
    for body in [
        r#"{"watchlist":["SYNTHETIC"]}"#,
        r#"{"watchlist":["AAPL.NASDAQ","SYNTHETIC.BATS"]}"#,
        r#"{"watchlist":["spy.ARCA"]}"#,
        r#"{"watchlist":["TSLA.NASDAQ"]}"#,
        r#"{"watchlist":[12345678901234567890]}"#,
        r#"{"watchlist":null}"#,
        r#"{"depth_focus":"NVDA.NASDAQ"}"#,
        r#"{"depth_focus":"SPY.ARCA"}"#,
        r#"{"depth_focus":"AAPL.XNAS"}"#,
        r#"{"depth_focus":"AAPL.NASDAQ","watchlist":[],"credential":"synthetic"}"#,
        r#"{"job_id":"synthetic"}"#,
        r#"{"license":"synthetic"}"#,
        r#"{"watchlist":["SYNTHETIC.XNAS"],"watchlist":[]}"#,
        r#"{} {}"#,
        "[]",
        "null",
    ] {
        assert_eq!(
            put(&app, body, INVALID).await,
            json!({"error":{"code":"subscription_demand_invalid","message":"Invalid subscription demand"}})
        );
    }
    assert!(!owner.has_changed().unwrap());
    assert_eq!(*owner.borrow(), snapshot);
    assert_eq!(get(&app, DEMAND, StatusCode::OK).await, accepted);
}

#[tokio::test]
async fn demand_status_exposes_owner_budget_and_display_policies() {
    // The real scheduler handle is used for budget reads, with no worker started.
    let root = tempfile::tempdir().unwrap();
    let scheduler = LineScheduler::new(BudgetConfig::default(), Vec::new()).unwrap();
    let (handle, _owner) = SubscriptionDemandHandle::channel();
    let app = app(
        &root,
        MarketReadSources {
            subscription_demand: Some(handle),
            option_lines: Some(Licensed::new(
                Arc::new(scheduler.handle()),
                "fixture.opra".parse().unwrap(),
            )),
            depth: Some(Licensed::new(
                DepthFrames::default(),
                "fixture.totalview".parse().unwrap(),
            )),
            ..Default::default()
        },
    );
    let demand = put(&app, r#"{"depth_focus":"QQQ.NASDAQ"}"#, StatusCode::OK).await;
    let budget = get(&app, BUDGET, StatusCode::OK).await;
    for field in [
        "account_lines",
        "reserve_lines",
        "stock_lines",
        "hot_lines",
        "rotation_lines",
    ] {
        assert_eq!(demand["budget"][field], budget[field]);
    }
    assert_eq!(budget["account_lines"], 100);
    assert_eq!(budget["reserve_lines"], 30);
    assert_eq!(budget["stock_lines"], 0);
    assert_eq!(budget["phase"], "starting");
    for (access, product, license) in [
        (&demand["budget"]["access"], "opra", "fixture.opra"),
        (
            &demand["depth_budget"]["access"],
            "nasdaq-totalview",
            "fixture.totalview",
        ),
    ] {
        assert_eq!(
            access,
            &json!({
                "source":"ibkr","product":product,"license":license,"display_live":true,
                "record_local":false,"derive_non_display":false,"backup_personal_drive":false
            })
        );
    }
}

#[test]
fn generated_openapi_declares_demand_and_keeps_budget_a_separate_get() {
    let spec = serde_json::to_value(ApiDoc::openapi()).unwrap();
    let route = &spec["paths"][DEMAND];
    assert!(route["get"]["responses"]["200"].is_object());
    let put = &route["put"];
    assert_eq!(
        put["requestBody"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/SubscriptionDemandRequest"
    );
    assert!(put["responses"]["400"].is_object());
    let request = &spec["components"]["schemas"]["SubscriptionDemandRequest"];
    assert_eq!(request["additionalProperties"], false);
    assert!(request.get("required").is_none());
    assert_eq!(
        request["properties"]["watchlist"]["items"]["$ref"],
        "#/components/schemas/InstrumentId"
    );
    assert!(spec["paths"][BUDGET]["get"]["responses"]["503"].is_object());
    assert!(spec["paths"][BUDGET].get("put").is_none());
}
