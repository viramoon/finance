use std::{fs, path::Path, sync::Arc};

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header::CACHE_CONTROL},
};
use chrono::{DateTime, TimeDelta, Utc};
use finance_api::{
    ApiDoc, AppState,
    market_options::{
        FlowObservationSnapshot, FlowObservationSource, FlowVerificationSource, MarketReadSources,
        OptionFlowSource, OptionFlowWindow,
    },
    router_with_market_reads,
};
use finance_collector::{
    line_budget::{BudgetStatus, CoverageInterval, CoverageReport, SchedulerPhase},
    market_ingress::ProductPolicy,
    option_flow::{
        HistoricalFlow, ObservedFlow,
        evidence::{
            ConditionMapping, MappedCondition, TradeAction, TradeLink, VerificationPolicy,
            verify_history,
        },
        observation::{
            BaselineCohort, BaselineVolumeRate, FlowObservation, L1Sample,
            OBSERVATION_RULE_VERSION, ObservationEvidence, ObservationStatus, QuoteWidening,
            VolumeComparison, VolumeIncrease,
        },
    },
};
use finance_ib::{
    historical::option_ticks::{
        HistoricalBatch, HistoricalIssue, HistoricalIssueReason, HistoricalKind,
        HistoricalOptionTicks, HistoricalTick, HistoricalWindow, HistoryQuote, HistoryTrade,
    },
    market::{
        ConfirmedContract, ContractKind, OptionContract, OptionRight, Product, RequestKey,
        RequestParameters,
    },
};
use serde_json::{Value, json};
use tower::ServiceExt;
use utoipa::OpenApi;

const CON_ID: i32 = 42;
// All fixtures below use invented quotes, IDs, condition mappings and a fixed UTC clock.
const PRICE: &str = "1.000000000000000001";
const LARGE_QUANTITY: &str = "9007199254740993";
const START: &str = "2025-03-07T16:00:00Z";
const BASE: &str = "/api/v1/options/flow";

fn at(second: i64) -> DateTime<Utc> {
    START.parse::<DateTime<Utc>>().unwrap() + TimeDelta::seconds(second)
}

fn window() -> OptionFlowWindow {
    OptionFlowWindow {
        con_id: CON_ID,
        start: at(0),
        end: at(12),
    }
}

fn path(kind: &str, extra: &str) -> String {
    format!("{BASE}/{kind}?con_id={CON_ID}&start={START}&end=2025-03-07T16:00:12Z{extra}")
}

fn policy(product: &str, record_local: bool) -> ProductPolicy {
    ProductPolicy {
        source: "ibkr".parse().unwrap(),
        product: product.parse().unwrap(),
        license: format!("synthetic-{product}").parse().unwrap(),
        display_live: true,
        record_local,
        derive_non_display: false,
        backup_personal_drive: false,
    }
}

fn coverage() -> CoverageReport {
    CoverageReport {
        key: RequestKey {
            con_id: CON_ID,
            product: Product::Opra,
            parameters: RequestParameters::default(),
        },
        last_observed_at: Some(at(3)),
        covered: vec![],
        uncovered: vec![CoverageInterval {
            start: at(0),
            end: at(3),
        }],
        unknown: vec![],
    }
}

fn sample(value: &str, second: i64) -> L1Sample {
    L1Sample {
        value: value.parse().unwrap(),
        received_at: at(second),
    }
}

fn observations() -> FlowObservationSnapshot {
    let volume = ObservedFlow {
        access: policy("opra", false),
        clue: FlowObservation {
            rule_version: OBSERVATION_RULE_VERSION,
            status: ObservationStatus::PendingVerification,
            con_id: CON_ID,
            generation: u64::MAX.into(),
            cohort: BaselineCohort {
                underlying_con_id: 7,
                expiry: at(0).date_naive(),
                session_segment: 3,
            },
            multiplier: "25".parse().unwrap(),
            observed_at: at(3),
            observation: ObservationEvidence::VolumeIncrease(Box::new(VolumeIncrease {
                earlier_cumulative_volume: sample("50", 0),
                later_cumulative_volume: sample("9007199254741043", 3),
                delta_contracts: LARGE_QUANTITY.parse().unwrap(),
                comparison: VolumeComparison {
                    elapsed_millis: 3_000,
                    baseline_samples: 3,
                    typical_rate: Some(BaselineVolumeRate {
                        contracts: 9007199254740993,
                        elapsed_millis: 6_000,
                    }),
                    elevated: Some(true),
                },
                sampled_last_price: Some(sample(PRICE, 3)),
                sampled_last_size: None,
                estimated_premium: None,
            })),
        },
        coverage: Some(coverage()),
        budget: BudgetStatus {
            phase: SchedulerPhase::Online,
            as_of: at(3),
            account_lines: 100,
            reserve_lines: 30,
            stock_lines: 9,
            hot_lines: 1,
            rotation_lines: 1,
            confirmed_options: 2,
            observed_options: 1,
            discovery_complete: false,
        },
    };
    let mut quote = volume.clone();
    quote.coverage = None;
    quote.clue.observation = ObservationEvidence::QuoteWidening(Box::new(QuoteWidening {
        bid: sample("1", 3),
        ask: sample("1.2", 3),
        bid_size: sample("3", 3),
        ask_size: sample("4", 3),
        spread: "0.2".parse().unwrap(),
        typical_spread: "0.1".parse().unwrap(),
        baseline_samples: 3,
    }));
    FlowObservationSnapshot {
        as_of: at(12),
        records: vec![volume, quote],
    }
}

fn observation_source(
    snapshot: FlowObservationSnapshot,
) -> OptionFlowSource<FlowObservationSource> {
    let snapshot = Arc::new(snapshot);
    OptionFlowSource::new(
        policy("opra", false),
        Arc::new(move |key| (key == window()).then(|| snapshot.clone())),
    )
}

fn batch(kind: HistoricalKind) -> HistoricalBatch {
    HistoricalBatch {
        kind,
        requested_start: at(if kind == HistoricalKind::Trades {
            0
        } else {
            -1
        }),
        obtained_at: at(12),
        provider_request_id: Some(99),
        page: 1,
    }
}

fn verification(record_local: bool, expired: bool) -> HistoricalFlow {
    let contract = ConfirmedContract {
        con_id: CON_ID,
        symbol: "SYNTH".into(),
        primary_exchange: None,
        liquid_hours: vec![],
        kind: ContractKind::Option(OptionContract {
            expiry: at(if expired { -86_400 } else { 0 }).date_naive(),
            strike: "4".parse().unwrap(),
            right: OptionRight::Call,
            multiplier: "25".parse().unwrap(),
            underlying_con_id: 7,
        }),
    };
    let mut ticks = HistoricalOptionTicks::default();
    let mut links = vec![];
    if expired {
        ticks.issues = [HistoricalKind::Trades, HistoricalKind::BidAsk]
            .into_iter()
            .map(|kind| HistoricalIssue {
                kind,
                start: batch(kind).requested_start,
                end: at(12),
                observed_at: at(12),
                reason: HistoricalIssueReason::ExpiredContract,
            })
            .collect();
    } else {
        let trade = |second, occurrence, code: &str| HistoricalTick {
            row: HistoryTrade {
                occurred_at: at(second),
                price: PRICE.parse().unwrap(),
                quantity: "2.125".parse().unwrap(),
                exchange: "SYNTH_EXCHANGE".into(),
                special_conditions: code.into(),
                past_limit: false,
                unreported: false,
            },
            batch: batch(HistoricalKind::Trades),
            occurrence,
        };
        ticks.trades = vec![
            trade(4, 1, ""),
            trade(4, 2, ""),
            trade(8, 1, "SYNTH_CORRECTION"),
        ];
        ticks.quotes.push(HistoricalTick {
            row: HistoryQuote {
                occurred_at: at(3),
                bid: Some("0.99".parse().unwrap()),
                ask: Some(PRICE.parse().unwrap()),
                bid_quantity: Some(LARGE_QUANTITY.parse().unwrap()),
                ask_quantity: Some("4".parse().unwrap()),
                bid_past_low: false,
                ask_past_high: false,
            },
            batch: batch(HistoricalKind::BidAsk),
            occurrence: 1,
        });
        ticks.issues.push(HistoricalIssue {
            kind: HistoricalKind::Trades,
            start: at(9),
            end: at(12),
            observed_at: at(12),
            reason: HistoricalIssueReason::Pacing,
        });
        links = vec![
            TradeLink {
                occurrence: (&ticks.trades[0]).into(),
                action: TradeAction::Original {
                    native_id: "SYNTH_ORIGINAL".parse().unwrap(),
                },
            },
            TradeLink {
                occurrence: (&ticks.trades[2]).into(),
                action: TradeAction::Correction {
                    target_native_id: "SYNTH_ORIGINAL".parse().unwrap(),
                    revision: u64::MAX.into(),
                },
            },
        ];
    }
    let mapping = ConditionMapping::new(
        "synthetic-map-v1".parse().unwrap(),
        [("SYNTH_CORRECTION".to_owned(), MappedCondition::Correction)].into(),
    )
    .unwrap();
    let verification_policy =
        VerificationPolicy::new(TimeDelta::seconds(2)).with_condition_mapping(mapping);
    let evidence = verify_history(&contract, &ticks, at(13), &verification_policy, &links).unwrap();
    HistoricalFlow {
        access: policy("opra-history", record_local),
        window: HistoricalWindow::new(at(0), at(12), at(-1), 1).unwrap(),
        evidence,
        observation_coverage: Some(coverage()),
        durable_through: record_local.then(|| u64::MAX.into()),
    }
}

fn verification_source(result: HistoricalFlow) -> OptionFlowSource<FlowVerificationSource> {
    let access = result.access.clone();
    let result = Arc::new(result);
    OptionFlowSource::new(
        access,
        Arc::new(move |key| (key == window()).then(|| result.clone())),
    )
}

fn app(root: &Path, sources: MarketReadSources) -> Router {
    router_with_market_reads(
        AppState {
            data_root: root.into(),
            research_root: root.join("research"),
        },
        sources,
    )
}

async fn get(app: &Router, uri: &str, status: StatusCode) -> Value {
    let response = app
        .clone()
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.headers()[CACHE_CONTROL], "private, no-store");
    assert_eq!(response.status(), status, "{uri}");
    let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn observations_preserve_pending_clues_exact_values_coverage_and_equal_times() {
    let root = tempfile::tempdir().unwrap();
    let app = app(
        root.path(),
        MarketReadSources {
            flow_observations: Some(observation_source(observations())),
            ..Default::default()
        },
    );
    let first = get(&app, &path("observations", "&limit=1"), StatusCode::OK).await;
    let second = get(&app, &path("observations", "&offset=1"), StatusCode::OK).await;
    assert_eq!(first["access"], json!(policy("opra", false)));
    assert_eq!(first["as_of"], json!(at(12)));
    assert_eq!(first["next_offset"], 1);
    assert!(second["next_offset"].is_null());
    let row = &first["items"][0];
    let clue = &row["clue"];
    assert_eq!(clue["status"], "pending_verification");
    assert_eq!(clue["generation"], u64::MAX.to_string());
    assert_eq!(clue["multiplier"], "25");
    assert_eq!(clue["observation"]["kind"], "volume_increase");
    let volume = &clue["observation"]["evidence"];
    assert_eq!(volume["delta_contracts"], LARGE_QUANTITY);
    assert_eq!(volume["sampled_last_price"]["value"], PRICE);
    assert_eq!(
        volume["comparison"]["typical_rate"]["contracts"],
        LARGE_QUANTITY
    );
    assert!(clue.get("trade_id").is_none());
    assert_eq!(row["budget"]["phase"], "online");
    assert_eq!(row["coverage"]["access"], first["access"]);
    assert_eq!(row["coverage"]["uncovered"][0]["start"], json!(at(0)));
    let second_row = &second["items"][0];
    assert_eq!(second_row["clue"]["observed_at"], clue["observed_at"]);
    assert_eq!(second_row["clue"]["observation"]["kind"], "quote_widening");
    assert!(second_row["coverage"].is_null());
    assert!(fs::read_dir(root.path()).unwrap().next().is_none());
}

#[tokio::test]
async fn verification_pages_preserve_occurrences_corrections_quotes_and_evidence_watermark() {
    let root = tempfile::tempdir().unwrap();
    let app = app(
        root.path(),
        MarketReadSources {
            flow_observations: Some(observation_source(observations())),
            flow_verifications: Some(verification_source(verification(true, false))),
            ..Default::default()
        },
    );
    let first = get(&app, &path("verification", "&limit=2"), StatusCode::OK).await;
    let second = get(&app, &path("verification", "&offset=2"), StatusCode::OK).await;
    assert_eq!(first["access"], json!(policy("opra-history", true)));
    assert_eq!(
        first["observation_coverage"]["access"],
        json!(policy("opra", false))
    );
    assert_eq!(first["quote_start"], json!(at(-1)));
    assert_eq!(first["verified_at"], json!(at(13)));
    assert_eq!(first["durable_through"], u64::MAX.to_string());
    assert_eq!(first["counts"]["verified"], 2);
    assert_eq!(first["counts"]["unknown"], 1);
    assert_eq!(first["counts"]["pending_intervals"], 1);
    assert_eq!(first["issues"][0]["reason"], "pacing");
    assert_eq!(first["issues"], second["issues"]);
    assert_eq!(first["counts"], second["counts"]);
    assert_eq!(first["next_offset"], 2);
    assert!(second["next_offset"].is_null());
    assert_eq!(first["trades"][0]["status"], "superseded");
    assert_eq!(
        first["trades"][0]["trade"]["row"],
        first["trades"][1]["trade"]["row"]
    );
    assert_eq!(first["trades"][0]["trade"]["occurrence"], 1);
    let active = &first["trades"][1];
    assert_eq!(active["trade"]["occurrence"], 2);
    assert_eq!(active["trade"]["row"]["price"], PRICE);
    assert_eq!(active["trade"]["row"]["quantity"], "2.125");
    assert_eq!(active["trade"]["batch"]["obtained_at"], json!(at(12)));
    assert_eq!(active["condition"]["meaning"]["status"], "empty");
    assert_eq!(active["inference"]["direction"], "estimated_buy");
    assert_eq!(
        active["inference"]["quote"]["quote"]["row"]["bid_quantity"],
        LARGE_QUANTITY
    );
    assert_eq!(active["premium"], "53.125000000000000053125");
    assert_eq!(active["identity"], "unknown");
    let correction = &second["trades"][0];
    assert_eq!(
        correction["link"]["action"]["revision"],
        u64::MAX.to_string()
    );
    assert_eq!(
        correction["condition"]["mapping_version"],
        "synthetic-map-v1"
    );
    assert_eq!(
        correction["condition"]["meaning"],
        json!({"status": "mapped", "condition": "correction"})
    );
    assert_eq!(correction["inference"]["reason"], "stale_quote");
    assert_eq!(correction["inference"]["direction"], "unknown");
    assert!(fs::read_dir(root.path()).unwrap().next().is_none());
}

#[tokio::test]
async fn expired_report_and_display_only_history_have_no_durable_position_or_unlicensed_coverage() {
    let root = tempfile::tempdir().unwrap();
    for expired in [false, true] {
        let app = app(
            root.path(),
            MarketReadSources {
                flow_verifications: Some(verification_source(verification(false, expired))),
                ..Default::default()
            },
        );
        let report = get(&app, &path("verification", ""), StatusCode::OK).await;
        assert_eq!(report["access"], json!(policy("opra-history", false)));
        assert!(report["observation_coverage"].is_null());
        assert!(report["durable_through"].is_null());
        if expired {
            assert_eq!(report["counts"]["unavailable_intervals"], 2);
            assert_eq!(report["issues"][0]["reason"], "expired_contract");
            assert_eq!(report["issues"][1]["kind"], "bid_ask");
            assert_eq!(report["trades"], json!([]));
        } else {
            assert_eq!(report["counts"]["verified"], 2);
            assert_eq!(report["trades"].as_array().unwrap().len(), 3);
        }
    }
    assert!(fs::read_dir(root.path()).unwrap().next().is_none());
}

#[tokio::test]
async fn invalid_or_unlicensed_sources_cannot_serve_results() {
    let root = tempfile::tempdir().unwrap();
    let mut denied = policy("opra-history", true);
    denied.display_live = false;
    denied.derive_non_display = true;
    let mut forged_history = verification(false, false);
    forged_history.durable_through = Some(u64::MAX.into());
    let mut forged_observations = observations();
    forged_observations.records[0].access.license = "incorrect-license".parse().unwrap();
    let forged_app = app(
        root.path(),
        MarketReadSources {
            flow_observations: Some(observation_source(forged_observations)),
            flow_verifications: Some(verification_source(forged_history)),
            ..Default::default()
        },
    );
    for kind in ["observations", "verification"] {
        let body = get(
            &forged_app,
            &path(kind, ""),
            StatusCode::INTERNAL_SERVER_ERROR,
        )
        .await;
        assert_eq!(
            body,
            json!({"error": {"code": "market_read_failed", "message": "Could not read market data"}})
        );
    }
    let denial_sources = MarketReadSources {
        flow_observations: Some(OptionFlowSource::new(
            policy("opra-history", false),
            Arc::new(|_| panic!("an invalid product must be rejected before lookup")),
        )),
        flow_verifications: Some(OptionFlowSource::new(
            denied,
            Arc::new(|_| panic!("a non-display product must be rejected before lookup")),
        )),
        ..Default::default()
    };
    let denied_app = app(root.path(), denial_sources);
    for kind in ["observations", "verification"] {
        let body = get(&denied_app, &path(kind, ""), StatusCode::NOT_FOUND).await;
        assert_eq!(body["error"]["code"], "market_read_not_found");
    }
    assert!(fs::read_dir(root.path()).unwrap().next().is_none());
}

#[tokio::test]
async fn flow_routes_validate_queries_and_only_read_published_windows() {
    let root = tempfile::tempdir().unwrap();
    let standalone = app(root.path(), MarketReadSources::default());
    for kind in ["observations", "verification"] {
        let uri = path(kind, "");
        let body = get(&standalone, &uri, StatusCode::SERVICE_UNAVAILABLE).await;
        assert_eq!(body["error"]["code"], "market_read_source_unavailable");
        assert_eq!(
            standalone
                .clone()
                .oneshot(Request::post(&uri).body(Body::empty()).unwrap())
                .await
                .unwrap()
                .status(),
            StatusCode::METHOD_NOT_ALLOWED
        );
        for invalid in [
            path(kind, "&limit=0"),
            path(kind, "&limit=101"),
            path(kind, "&fetch=true"),
            uri.replace("con_id=42", "con_id=-1"),
            uri.replace(START, "2025-03-07T16:00:13Z"),
        ] {
            let body = get(&standalone, &invalid, StatusCode::BAD_REQUEST).await;
            assert_eq!(body["error"]["code"], "market_read_query_invalid");
        }
    }
    let subsecond = path("verification", "").replace(START, "2025-03-07T16:00:00.001Z");
    get(&standalone, &subsecond, StatusCode::BAD_REQUEST).await;
    let cross_date =
        path("verification", "").replace("2025-03-07T16:00:12Z", "2025-03-08T16:00:12Z");
    get(&standalone, &cross_date, StatusCode::BAD_REQUEST).await;

    let connected = app(
        root.path(),
        MarketReadSources {
            flow_observations: Some(observation_source(observations())),
            flow_verifications: Some(verification_source(verification(true, false))),
            ..Default::default()
        },
    );
    for kind in ["observations", "verification"] {
        let absent = path(kind, "").replace("16:00:12Z", "16:00:11Z");
        get(&connected, &absent, StatusCode::NOT_FOUND).await;
    }
    let last = path("verification", &format!("&offset={}", usize::MAX));
    let empty = get(&connected, &last, StatusCode::OK).await;
    assert_eq!(empty["trades"], json!([]));
    assert!(empty["next_offset"].is_null());
    assert!(fs::read_dir(root.path()).unwrap().next().is_none());
}

#[test]
fn batch4_openapi_exposes_only_typed_flow_reads_with_exact_numeric_schemas() {
    let spec = serde_json::to_value(ApiDoc::openapi()).unwrap();
    for (kind, schema) in [
        ("observations", "OptionFlowObservationPage"),
        ("verification", "OptionFlowVerificationPage"),
    ] {
        let route = &spec["paths"][format!("{BASE}/{kind}")];
        assert_eq!(route.as_object().unwrap().len(), 1);
        let operation = &route["get"];
        assert_eq!(
            operation["responses"]["200"]["content"]["application/json"]["schema"]["$ref"],
            format!("#/components/schemas/{schema}")
        );
        for status in ["400", "404", "500", "503"] {
            assert_eq!(
                operation["responses"][status]["content"]["application/json"]["schema"]["$ref"],
                "#/components/schemas/ErrorBody"
            );
        }
        let params = operation["parameters"].as_array().unwrap();
        for bound in ["start", "end"] {
            let param = params.iter().find(|param| param["name"] == bound).unwrap();
            assert_eq!(param["required"], true);
            assert_eq!(param["schema"]["format"], "date-time");
        }
    }
    let schemas = &spec["components"]["schemas"];
    assert_eq!(
        schemas["FlowObservation"]["properties"]["generation"]["$ref"],
        "#/components/schemas/U64Text"
    );
    assert_eq!(
        schemas["BaselineVolumeRate"]["properties"]["contracts"]["type"],
        "string"
    );
    assert_eq!(
        schemas["TradeAction"]["oneOf"][1]["properties"]["revision"]["$ref"],
        "#/components/schemas/U64Text"
    );
    for field in ["price", "quantity"] {
        assert_eq!(
            schemas["HistoricalTick_HistoryTrade"]["properties"]["row"]["properties"][field]["$ref"],
            "#/components/schemas/DecimalText"
        );
    }
    let bid = &schemas["HistoricalTick_HistoryQuote"]["properties"]["row"]["properties"]["bid"];
    assert!(
        bid["oneOf"]
            .as_array()
            .unwrap()
            .contains(&json!({"$ref": "#/components/schemas/DecimalText"}))
    );
    let page = &schemas["OptionFlowVerificationPage"]["properties"];
    let watermark = page["durable_through"]["oneOf"].as_array().unwrap();
    assert!(watermark.contains(&json!({"type": "null"})));
    assert!(watermark.contains(&schemas["U64Text"]));
    assert_eq!(page["access"]["$ref"], "#/components/schemas/ProductPolicy");
    assert_eq!(
        page["issues"]["items"]["$ref"],
        "#/components/schemas/HistoricalIssue"
    );
}
