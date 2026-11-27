use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header::CACHE_CONTROL},
};
use chrono::{DateTime, Utc};
use finance_api::{
    ApiDoc, AppState,
    market_options::{
        DepthFrames, ExposureInput, ExposureSource, HistoryInput, HistorySource, Licensed,
        MarketReadSources, OptionLineSource,
    },
    router_with_market_reads,
};
use finance_collector::{
    line_budget::{
        BudgetConfig, BudgetStatus, CoverageInterval, CoverageReport, LineScheduler, SchedulerPhase,
    },
    market_ingress::{MarketIngress, ProductPolicy, PublishResult},
    option_exposure::{OptionChainSnapshot, TimingPolicy},
    option_history::{BarWindow, ContractHistory, OptionBars, TradeBar},
};
use finance_ib::{
    depth::{DepthFrame, DepthLevel, DepthPermissions, DepthState, DepthSymbol},
    market::{
        ConfirmedContract, ContractKind, OptionContract, OptionRight, Product, RequestKey,
        RequestParameters,
    },
};
use finance_market_protocol::{Capability, Event, Instrument, decode_json};
use serde_json::{Value, json};
use std::{fs, path::Path, sync::Arc};
use tower::ServiceExt;
use utoipa::OpenApi;

const LICENSE: &str = "fixture.synthetic";
const AS_OF: &str = "2025-03-07T16:02:00Z";
const START: u64 = 1_741_363_200_000_000_000;
const MINUTE: u64 = 60_000_000_000;
const PRICE: &str = "123.4500000000000000000000000";

fn app(root: &Path, sources: MarketReadSources) -> Router {
    router_with_market_reads(
        AppState {
            data_root: root.into(),
            research_root: root.join("research"),
        },
        sources,
    )
}

fn licensed<T>(value: T) -> Licensed<T> {
    Licensed::new(value, LICENSE.parse().unwrap())
}

async fn get(app: &Router, uri: &str, status: StatusCode) -> Value {
    let response = app
        .clone()
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.headers()[CACHE_CONTROL], "private, no-store");
    let actual = response.status();
    let bytes = to_bytes(response.into_body(), 256 * 1024).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(status, actual, "{uri}: {body}");
    body
}

fn assert_private_access(body: &Value, product: &str) {
    assert_eq!(
        body["access"],
        json!({
            "source": "ibkr", "product": product, "license": LICENSE, "display_live": true,
            "record_local": false, "derive_non_display": false, "backup_personal_drive": false
        })
    );
}

#[tokio::test]
async fn binance_status_reads_both_default_profiles_and_reports_enabled_product_permissions() {
    let root = tempfile::tempdir().unwrap();
    let status = "/api/v1/market/binance/status";
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/binance");
    for path in [
        dir.join("cloudbox.yaml"),
        dir.join("stable.yaml"),
        root.path().join("missing.yaml"),
    ] {
        let app = app(
            root.path(),
            MarketReadSources {
                binance_config: Some(path),
                ..Default::default()
            },
        );
        assert_eq!(
            get(&app, status, StatusCode::OK).await,
            json!({
                "state": "disabled_by_profile", "products": []
            })
        );
    }
    // Deterministic synthetic profile. This test never starts the F4 runner.
    let config = root.path().join("binance.yaml");
    fs::write(&config, "enabled: true\nspot:\n  license: fixture.synthetic\n  display_live: true\n  record_local: false\n  derive_non_display: false\n  backup_personal_drive: false\n").unwrap();
    let app = app(
        root.path(),
        MarketReadSources {
            binance_config: Some(config.clone()),
            ..Default::default()
        },
    );
    assert_eq!(
        get(&app, status, StatusCode::OK).await,
        json!({
            "state": "enabled_by_profile",
            "products": [{
                "source": "binance", "product": "spot", "license": LICENSE, "display_live": true,
                "record_local": false, "derive_non_display": false, "backup_personal_drive": false
            }]
        })
    );
    let wal = get(
        &app,
        "/api/v1/market/wal?source=binance&product=spot",
        StatusCode::NOT_FOUND,
    )
    .await;
    assert_eq!(wal["error"]["code"], "market_product_not_found");
    fs::write(&config, "enabled: true\n").unwrap();
    let invalid = get(&app, status, StatusCode::INTERNAL_SERVER_ERROR).await;
    assert_eq!(invalid["error"]["code"], "binance_profile_invalid");
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
}

#[tokio::test]
async fn binance_context_uses_f3_for_licensed_events_and_preserves_max_sequence() {
    let root = tempfile::tempdir().unwrap();
    // Start with the generated synthetic F1 v2 instrument. Assign the F4
    // context product and a fixed synthetic funding rate to the new event.
    let fixture: Value =
        serde_json::from_str(include_str!("../../../protocol/v2/examples.json")).unwrap();
    let mut instrument = fixture["instruments"][0].clone();
    instrument["instrument_id"] = json!("BTC-USDT-PERP.BINANCE");
    instrument["venue"] = json!("BINANCE");
    instrument["quote_currency"] = json!("USDT");
    instrument["settlement_currency"] = json!("USDT");
    instrument["supplier_symbols"] =
        json!([{"source":"binance", "product":"usdt-perp-context", "symbol":"BTCUSDT"}]);
    let instrument: Instrument = decode_json(&instrument.to_string()).unwrap();
    let capability: Capability = decode_json(
        &json!({
            "schema_version": "2.0", "source": "binance", "product": "usdt-perp-context",
            "message_kinds": ["funding_rate", "open_interest"], "max_depth": 0,
            "can_backfill": false, "delay_tier": "realtime"
        })
        .to_string(),
    )
    .unwrap();
    let mut data = fixture["events"][0].clone();
    data["instrument_id"] = json!("BTC-USDT-PERP.BINANCE");
    data["venue"] = json!("BINANCE");
    data["source"] = json!("binance");
    data["product"] = json!("usdt-perp-context");
    data["license"] = json!(LICENSE);
    data["seq"] = json!(u64::MAX.to_string());
    data["cursor"] = json!(format!("1:{}", u64::MAX));
    data["channel"] = json!("funding_rate");
    data["kind"] = json!("funding_rate");
    data["exchange_ts"] = data["receive_ts"].clone();
    data["payload"] = json!({"rate": "-0.000000010000", "next_funding_ts": null});
    let event: Event = decode_json(&data.to_string()).unwrap();
    let policy = ProductPolicy {
        source: event.source.clone(),
        product: event.product.clone(),
        license: event.license.clone(),
        display_live: true,
        record_local: true,
        derive_non_display: false,
        backup_personal_drive: false,
    };
    let mut ingress = MarketIngress::open(root.path(), policy.clone()).unwrap();
    assert!(matches!(
        ingress.publish(event, &instrument, &capability).unwrap(),
        PublishResult::Staged(_)
    ));
    assert_eq!(ingress.flush().unwrap(), Some(1));
    let app = app(root.path(), MarketReadSources::default());
    let body = get(
        &app,
        "/api/v1/market/wal?source=binance&product=usdt-perp-context",
        StatusCode::OK,
    )
    .await;
    assert_eq!(body["access"], serde_json::to_value(policy).unwrap());
    assert_eq!(body["records"][0]["event"], data);
    let watermarks = get(
        &app,
        "/api/v1/market/watermarks?source=binance&product=usdt-perp-context",
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        watermarks["watermarks"]["source_latest"]["seq"],
        u64::MAX.to_string()
    );
}

// Fixed synthetic IDs, expiry dates, strike and observation intervals.
struct ReplayLines;

impl OptionLineSource for ReplayLines {
    fn option_chain(&self) -> (DateTime<Utc>, bool, Vec<ConfirmedContract>) {
        let contracts = [
            (29, 8, "2025-03-14"),
            (23, 9, "2025-03-07"),
            (20, 8, "2025-03-07"),
        ]
        .map(|(con_id, underlying_con_id, expiry)| ConfirmedContract {
            con_id,
            symbol: "SYNTH".into(),
            primary_exchange: None,
            kind: ContractKind::Option(OptionContract {
                expiry: expiry.parse().unwrap(),
                strike: PRICE.parse().unwrap(),
                right: OptionRight::Put,
                multiplier: "125".parse().unwrap(),
                underlying_con_id,
            }),
            liquid_hours: Vec::new(),
        });
        (AS_OF.parse().unwrap(), false, contracts.to_vec())
    }

    fn status(&self) -> BudgetStatus {
        BudgetStatus {
            phase: SchedulerPhase::Online,
            as_of: AS_OF.parse().unwrap(),
            account_lines: 100,
            reserve_lines: 30,
            stock_lines: 9,
            hot_lines: 1,
            rotation_lines: 2,
            confirmed_options: 3,
            observed_options: 1,
            discovery_complete: false,
        }
    }

    fn option_coverage(
        &self,
        con_id: i32,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Option<CoverageReport> {
        assert_eq!(con_id, 20);
        if end > AS_OF.parse::<DateTime<Utc>>().unwrap() {
            return None;
        }
        let first = start + chrono::TimeDelta::seconds(30);
        let last = end - chrono::TimeDelta::seconds(30);
        Some(CoverageReport {
            key: RequestKey {
                con_id,
                product: Product::Opra,
                parameters: RequestParameters {
                    option_open_interest: true,
                },
            },
            last_observed_at: Some(last),
            unknown: vec![CoverageInterval { start, end: first }],
            covered: vec![CoverageInterval {
                start: first,
                end: last,
            }],
            uncovered: vec![CoverageInterval { start: last, end }],
        })
    }
}

#[tokio::test]
async fn option_chain_filters_and_pages_and_budget_preserves_coverage() {
    let root = tempfile::tempdir().unwrap();
    let app = app(
        root.path(),
        MarketReadSources {
            option_lines: Some(licensed::<Arc<dyn OptionLineSource>>(Arc::new(ReplayLines))),
            ..Default::default()
        },
    );
    let path = "/api/v1/options/chain?underlying_con_id=8";
    let first = get(&app, &format!("{path}&limit=1"), StatusCode::OK).await;
    assert_private_access(&first, "opra");
    assert_eq!(first["as_of"], AS_OF);
    assert_eq!(first["discovery_complete"], false);
    assert_eq!(
        first["items"],
        json!([{
            "con_id": 20, "symbol": "SYNTH", "primary_exchange": null, "expiry": "2025-03-07",
            "strike": PRICE, "right": "put", "multiplier": "125"
        }])
    );
    assert_eq!(first["next_after_con_id"], 20);
    let second = get(&app, &format!("{path}&after_con_id=20"), StatusCode::OK).await;
    assert_eq!(second["items"].as_array().unwrap().len(), 1);
    assert_eq!(second["items"][0]["con_id"], 29);
    assert_eq!(second["next_after_con_id"], Value::Null);
    let filtered = get(&app, &format!("{path}&expiry=2025-03-07"), StatusCode::OK).await;
    assert_eq!(filtered["items"], first["items"]);
    assert_eq!(
        get(
            &app,
            &format!("{path}&after_con_id=0"),
            StatusCode::BAD_REQUEST
        )
        .await["error"]["code"],
        "market_read_query_invalid"
    );

    let budget = get(&app, "/api/v1/market/subscriptions", StatusCode::OK).await;
    assert_private_access(&budget, "opra");
    assert_eq!(budget["phase"], "online");
    assert_eq!(budget["reserve_lines"], 30);
    assert_eq!(budget["confirmed_options"], 3);
    assert_eq!(budget["observed_options"], 1);
    assert_eq!(budget["discovery_complete"], false);
    let coverage = get(&app, "/api/v1/market/subscriptions/coverage?con_id=20&start=2025-03-07T16:00:00Z&end=2025-03-07T16:02:00Z", StatusCode::OK).await;
    assert_private_access(&coverage, "opra");
    assert_eq!(
        coverage["unknown"],
        json!([{ "start": "2025-03-07T16:00:00Z", "end": "2025-03-07T16:00:30Z" }])
    );
    assert_eq!(
        coverage["covered"],
        json!([{ "start": "2025-03-07T16:00:30Z", "end": "2025-03-07T16:01:30Z" }])
    );
    assert_eq!(
        coverage["uncovered"],
        json!([{ "start": "2025-03-07T16:01:30Z", "end": AS_OF }])
    );
    assert_eq!(coverage["last_observed_at"], "2025-03-07T16:01:30Z");
    get(&app, "/api/v1/market/subscriptions/coverage?con_id=20&start=2025-03-07T16:00:00Z&end=2025-03-07T16:03:00Z", StatusCode::BAD_REQUEST).await;
}

#[tokio::test]
async fn line_handle_can_serve_without_allocating_a_subscription() {
    let root = tempfile::tempdir().unwrap();
    let scheduler = LineScheduler::new(BudgetConfig::default(), Vec::new()).unwrap();
    let handle = scheduler.handle();
    let app = app(
        root.path(),
        MarketReadSources {
            option_lines: Some(licensed::<Arc<dyn OptionLineSource>>(Arc::new(handle))),
            ..Default::default()
        },
    );
    let chain = get(
        &app,
        "/api/v1/options/chain?underlying_con_id=8",
        StatusCode::OK,
    )
    .await;
    assert_eq!(chain["items"], json!([]));
    assert_eq!(chain["discovery_complete"], false);
    let budget = get(&app, "/api/v1/market/subscriptions", StatusCode::OK).await;
    assert_eq!(budget["phase"], "starting");
    assert_eq!(budget["account_lines"], 100);
    assert_eq!(budget["reserve_lines"], 30);
    for key in ["stock_lines", "hot_lines", "rotation_lines"] {
        assert_eq!(budget[key], 0);
    }
    let coverage = get(&app,
        "/api/v1/market/subscriptions/coverage?con_id=20&start=2025-03-07T16:00:00Z&end=2025-03-07T16:02:00Z",
        StatusCode::OK).await;
    assert_eq!(coverage["covered"], json!([]));
    assert_eq!(coverage["unknown"].as_array().unwrap().len(), 1);
    assert!(fs::read_dir(root.path()).unwrap().next().is_none());
}

#[tokio::test]
async fn nasdaq_depth_is_display_only_and_clears_before_reset_or_permission_loss() {
    let root = tempfile::tempdir().unwrap();
    let frames = DepthFrames::default();
    let app = app(
        root.path(),
        MarketReadSources {
            depth: Some(licensed(frames.clone())),
            ..Default::default()
        },
    );
    let book = "/api/v1/market/book?symbol=QQQ";
    get(&app, book, StatusCode::NOT_FOUND).await;
    get(
        &app,
        "/api/v1/market/book?symbol=SPY",
        StatusCode::BAD_REQUEST,
    )
    .await;
    let mut frame = DepthFrame {
        symbol: DepthSymbol::Qqq,
        permissions: DepthPermissions {
            equity_l1_confirmed: true,
            totalview_confirmed: true,
            eds_confirmed: true,
        },
        state: DepthState::Receiving,
        request_id: Some(17),
        requested_rows: 5,
        received_at: Some(AS_OF.parse().unwrap()),
        bids: vec![DepthLevel {
            price: PRICE.parse().unwrap(),
            size: "1.2500".parse().unwrap(),
        }],
        asks: Vec::new(),
    };
    frames.publish(frame.clone());
    let depth = get(&app, book, StatusCode::OK).await;
    assert_private_access(&depth, "nasdaq-totalview");
    assert_eq!(depth["venue"], "NASDAQ");
    assert_eq!(depth["requested_rows"], 5);
    assert_eq!(depth["returned_rows"], 1);
    assert_eq!(depth["market_data_type"], Value::Null);
    assert_eq!(depth["bids"], json!([{ "price": PRICE, "size": "1.2500" }]));
    assert!(depth.get("request_id").is_none());

    frame.state = DepthState::Reset;
    frames.publish(frame.clone());
    let reset = get(&app, book, StatusCode::OK).await;
    assert_eq!(reset["state"], "reset");
    assert_eq!(reset["received_at"], Value::Null);
    assert_eq!(reset["returned_rows"], 0);
    assert_eq!(reset["bids"], json!([]));
    frame.state = DepthState::Receiving;
    frame.permissions.eds_confirmed = false;
    frames.publish(frame);
    let unconfirmed = get(&app, book, StatusCode::OK).await;
    assert_eq!(unconfirmed["state"], "permission_unconfirmed");
    assert_eq!(unconfirmed["bids"], json!([]));

    let products = get(&app, "/api/v1/market/products?source=ibkr", StatusCode::OK).await;
    assert_eq!(products["items"], json!([]));
    get(
        &app,
        "/api/v1/market/wal?source=ibkr&product=nasdaq-totalview",
        StatusCode::NOT_FOUND,
    )
    .await;
    assert!(fs::read_dir(root.path()).unwrap().next().is_none());
}

#[tokio::test]
async fn exposure_uses_f9d_scenarios_walls_coverage_and_exact_valuation() {
    let root = tempfile::tempdir().unwrap();
    // Spot=100, OI=10, gamma=0.01 and multiplier=100 make 1000 USD per 1% move.
    let chain: OptionChainSnapshot = serde_json::from_value(json!({
        "underlying_id": "SYNTH.XNAS", "as_of": AS_OF, "session_date": "2025-03-07",
        "spot": {"value": "100", "observed_at": AS_OF}, "universe_complete": true,
        "contracts": [
            {"instrument_id": "SYNTH-1.OPRA", "right": "call", "expiry": "2025-03-07",
             "strike": "100.0000", "multiplier": "100",
             "open_interest": {"value": "10", "observed_at": "2025-03-07T16:01:00Z", "business_date": null},
             "gamma": {"value": "0.01", "observed_at": AS_OF}},
            {"instrument_id": "SYNTH-2.OPRA", "right": "put", "expiry": "2025-03-14",
             "strike": "105", "multiplier": "100", "open_interest": null,
             "gamma": {"value": "0.02", "observed_at": AS_OF}}
        ]
    })).unwrap();
    let input = Arc::new(ExposureInput {
        chain,
        timing: TimingPolicy {
            max_spot_age_seconds: 120,
            max_gamma_age_seconds: 120,
            max_spot_gamma_skew_seconds: 120,
        },
        gamma_grid: None,
    });
    let source: Arc<ExposureSource> = Arc::new(move |id, as_of| {
        (id == &input.chain.underlying_id && as_of.is_none_or(|as_of| as_of == input.chain.as_of))
            .then(|| input.clone())
    });
    let app = app(
        root.path(),
        MarketReadSources {
            exposure: Some(licensed(source)),
            ..Default::default()
        },
    );
    let path = "/api/v1/options/exposure?underlying_id=SYNTH.XNAS";
    let body = get(
        &app,
        &format!("{path}&scenario=dealer_short&as_of={AS_OF}"),
        StatusCode::OK,
    )
    .await;
    assert_private_access(&body, "opra");
    let report = &body["report"];
    assert_eq!(report["as_of"], AS_OF);
    assert_eq!(report["model_version"], "gex-v1");
    assert_eq!(report["scenario"], "dealer_short");
    assert_eq!(report["zero_dte"]["totals"]["gex"], "-1000");
    assert_eq!(report["later"]["totals"]["gex"], Value::Null);
    assert_eq!(
        report["all"]["walls"]["most_negative"],
        json!({"strike": "100", "exposure": "-1000"})
    );
    assert_eq!(
        report["all"]["coverage"]["total_open_interest"],
        Value::Null
    );
    assert_eq!(report["all"]["coverage"]["partial"], true);
    assert_eq!(
        report["all"]["coverage"]["max_oi_gamma_skew"],
        json!({"secs": 60, "nanos": 0})
    );
    assert_eq!(
        report["all"]["flip"],
        json!({"status": "unavailable", "reason": "no_gamma_grid"})
    );

    assert_eq!(
        get(&app, path, StatusCode::OK).await["report"]["scenario"],
        "absolute"
    );
    get(
        &app,
        &format!("{path}&scenario=unknown"),
        StatusCode::BAD_REQUEST,
    )
    .await;
    get(
        &app,
        &format!("{path}&as_of=2025-03-07T16:00:00Z"),
        StatusCode::NOT_FOUND,
    )
    .await;
    assert!(fs::read_dir(root.path()).unwrap().next().is_none());
}

#[tokio::test]
async fn historical_chain_retains_f12_outcomes_and_exact_nanosecond_and_decimal_text() {
    let root = tempfile::tempdir().unwrap();
    // Use the first closed bar from the existing, registered synthetic F12 fixture.
    let mut bars: Vec<TradeBar> = serde_json::from_str(include_str!(
        "../../../tests/fixtures/option-history/ibkr-trade-bars.json"
    ))
    .unwrap();
    let bar = bars.remove(
        bars.iter()
            .position(|bar| bar.data.open_ts.get() == START)
            .unwrap(),
    );
    let bar_json = serde_json::to_value(&bar).unwrap();
    let window = BarWindow {
        start: START.into(),
        end: (START + 2 * MINUTE).into(),
    };
    let input = Arc::new(HistoryInput {
        underlying_con_id: 8,
        window,
        as_of: AS_OF.parse().unwrap(),
        contracts: [
            Ok(OptionBars::Available(vec![bar])),
            Ok(OptionBars::NoBars),
            Ok(OptionBars::Expired),
            Err(std::io::Error::other("synthetic private provider detail").into()),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, bars)| ContractHistory {
            instrument_id: format!("SYNTH-OPT-{index}.OPRA").parse().unwrap(),
            bars,
        })
        .collect(),
    });
    let source: Arc<HistorySource> = Arc::new(move |id, window| {
        (id == input.underlying_con_id && window == input.window).then(|| input.clone())
    });
    let app = app(
        root.path(),
        MarketReadSources {
            history: Some(licensed(source)),
            ..Default::default()
        },
    );
    let path = format!(
        "/api/v1/options/history?underlying_con_id=8&start={START}&end={}",
        window.end.get()
    );
    let first = get(&app, &format!("{path}&limit=2"), StatusCode::OK).await;
    assert_private_access(&first, "opra");
    assert_eq!(first["start"], START.to_string());
    assert_eq!(first["end"], window.end.get().to_string());
    assert_eq!(first["next_offset"], 2);
    assert_eq!(first["items"][0]["result"]["status"], "available");
    assert_eq!(first["items"][0]["result"]["bars"], json!([bar_json]));
    assert_eq!(
        first["items"][0]["result"]["bars"][0]["close_ts"],
        (START + MINUTE).to_string()
    );
    assert_eq!(
        first["items"][0]["result"]["bars"][0]["data"]["close"],
        "1.300000000000000001"
    );
    assert_eq!(
        first["items"][0]["result"]["bars"][0]["data"]["volume"],
        "9007199254740993"
    );
    assert_eq!(first["items"][1]["result"], json!({"status": "no_bars"}));
    let second = get(&app, &format!("{path}&offset=2"), StatusCode::OK).await;
    assert_eq!(second["next_offset"], Value::Null);
    assert_eq!(second["items"][0]["result"], json!({"status": "expired"}));
    assert_eq!(
        second["items"][1]["result"],
        json!({"status": "request_failed"})
    );

    let limit = "/api/v1/options/history?underlying_con_id=8&start=0&end=";
    get(&app, &format!("{limit}{}", u64::MAX), StatusCode::NOT_FOUND).await;
    get(
        &app,
        &format!("{limit}18446744073709551616"),
        StatusCode::BAD_REQUEST,
    )
    .await;
    get(&app, &format!("{limit}00"), StatusCode::BAD_REQUEST).await;
    assert!(fs::read_dir(root.path()).unwrap().next().is_none());
}

#[tokio::test]
async fn standalone_options_requires_licensed_owner_and_exposes_only_read_routes() {
    let root = tempfile::tempdir().unwrap();
    let app = app(root.path(), MarketReadSources::default());
    for path in [
        "/api/v1/options/chain?underlying_con_id=8",
        "/api/v1/market/subscriptions",
        "/api/v1/market/subscriptions/coverage?con_id=20&start=2025-03-07T16:00:00Z&end=2025-03-07T16:02:00Z",
        "/api/v1/market/book?symbol=AAPL",
        "/api/v1/options/exposure?underlying_id=SYNTH.XNAS",
        "/api/v1/options/history?underlying_con_id=8&start=1&end=2",
    ] {
        let body = get(&app, path, StatusCode::SERVICE_UNAVAILABLE).await;
        assert_eq!(body["error"]["code"], "market_read_source_unavailable");
        let write = app
            .clone()
            .oneshot(Request::post(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(write.status(), StatusCode::METHOD_NOT_ALLOWED, "{path}");
    }
    assert!(fs::read_dir(root.path()).unwrap().next().is_none());
}

#[test]
fn batch3_openapi_matches_routes_and_typed_schemas() {
    let spec = serde_json::to_value(ApiDoc::openapi()).unwrap();
    for (path, schema) in [
        ("/api/v1/market/bars", "ChartBars"),
        ("/api/v1/indicators", "IndicatorCatalog"),
        ("/api/v1/indicators/values", "ChartValues"),
        ("/api/v1/market/binance/status", "BinanceReadStatus"),
        ("/api/v1/market/book", "DepthBook"),
        ("/api/v1/market/subscriptions", "SubscriptionBudget"),
        (
            "/api/v1/market/subscriptions/coverage",
            "SubscriptionCoverage",
        ),
        ("/api/v1/options/chain", "OptionChainPage"),
        ("/api/v1/options/exposure", "OptionExposureResponse"),
        ("/api/v1/options/history", "OptionHistoryPage"),
    ] {
        let route = &spec["paths"][path];
        assert_eq!(route.as_object().unwrap().len(), 1);
        assert_eq!(
            route["get"]["responses"]["200"]["content"]["application/json"]["schema"]["$ref"],
            format!("#/components/schemas/{schema}"),
            "{path}"
        );
    }
    let schemas = &spec["components"]["schemas"];
    assert_eq!(
        schemas["DepthLevel"]["properties"]["price"]["$ref"],
        "#/components/schemas/DecimalText"
    );
    assert_eq!(
        schemas["OptionChainContract"]["properties"]["strike"]["$ref"],
        "#/components/schemas/DecimalText"
    );
    assert_eq!(
        schemas["OptionMinuteBar"]["properties"]["close_ts"]["$ref"],
        "#/components/schemas/U64Text"
    );
    assert_eq!(
        schemas["ExposureView"]["properties"]["coverage"]["$ref"],
        "#/components/schemas/ExposureCoverage"
    );
    assert_eq!(schemas["Coverage"]["type"], "string");
    let history = &spec["paths"]["/api/v1/options/history"]["get"]["parameters"];
    let end = history
        .as_array()
        .unwrap()
        .iter()
        .find(|param| param["name"] == "end")
        .unwrap();
    assert_eq!(end["schema"]["type"], "string");
    assert!(end["schema"]["pattern"].as_str().is_some());
    assert!(spec["paths"].get("/api/v1/options/flow").is_none());
}
