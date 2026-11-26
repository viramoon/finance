use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header::CACHE_CONTROL},
};
use finance_api::{ApiDoc, AppState, router};
use serde_json::{Value, json};
use std::{env, fs, path::Path, process::Command};
use tower::ServiceExt;
use utoipa::OpenApi;

const SNAPSHOT: &str = "finance-state/factor-research/read-v1.json";
const PRECISE: &str = "0.1234567890123456789012345678901234";
const AS_OF: &str = "2026-02-14T16:00:00Z";

fn app(data_root: &Path) -> Router {
    router(AppState {
        data_root: data_root.into(),
        research_root: data_root.join("unused-research"),
    })
}

async fn get(router: &Router, path: &str) -> (StatusCode, Value) {
    let response = router
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    assert_eq!(response.headers()[CACHE_CONTROL], "private, no-store");
    let body = to_bytes(response.into_body(), 65536).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

fn write_snapshot(root: &Path, snapshot: &Value) {
    let file = root.join(SNAPSHOT);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(file, serde_json::to_vec(snapshot).unwrap()).unwrap();
}

fn access() -> Value {
    json!({"license":"Synthetic fixture", "record_local":true,
           "derive_non_display":true, "redistribute":false})
}

fn factor(version: &str) -> Value {
    json!({"factor_id":"synthetic.factor", "version":version, "state":"observe_only",
        "access":access(), "tradable":false, "selectable_for_backtest":false,
        "details":{"kind":"factor", "definition":"Fixed synthetic score",
            "parameters":{"decimal":{"kind":"decimal", "value":PRECISE},
                          "max_integer":{"kind":"integer", "value":u64::MAX.to_string()}},
            "unit":"score", "asset_scope":["cash_equity"], "data_version":"synthetic-data-v1",
            "availability_rule":"Explicit publication time", "missing_policy":"exclude",
            "warmup_sessions":"2", "history":[]}})
}

fn historical() -> Value {
    json!({"factor_id":"research.factor-momentum", "version":"1.0.0", "state":"research_only",
        "access":access(), "tradable":false, "selectable_for_backtest":false,
        "details":{"kind":"historical_research", "source_commit":"0123456789abcdef0123456789abcdef01234567",
            "summary_ref":"strategies/synthetic-momentum/results/summary.json",
            "decision":{"verdict":"synthetic failed gate", "passed":false, "cost_bps":"25",
                        "rule":"Strictly positive spread", "comparisons":{"longest_sample":{
                             "main_sharpe":PRECISE, "static_sharpe":"1.00000", "difference":"-0.8",
                             "outperformance":false, "flip":null}}}}})
}

fn evaluation(id: &str, version: &str) -> Value {
    let distribution = json!({"count":1, "mean":PRECISE, "sample_std":null, "ratio":null});
    let portfolio = json!({"periods":1, "gross_observations":1, "net_observations":1,
        "turnover_observations":1, "cumulative_gross_return":"0.5", "cumulative_net_return":PRECISE,
        "total_turnover":"1.0", "total_fee_fraction":"0.0025"});
    json!({"factor_id":"synthetic.factor", "version":version, "data_version":"synthetic-data-v1",
        "evaluation_id":id, "access":access(), "peer_factors":[], "cost_bps":PRECISE, "quantile_count":2,
        "windows":[{"window":{"name":"fit", "phase":"train", "start":"2026-01-05", "end":"2026-01-06"},
            "factor_coverage":"1.0", "scored_coverage":"0.5", "ic":{"1":distribution}, "redundancy":{},
            "quantiles":[portfolio,portfolio], "benchmark":portfolio,
            "gross_information":distribution, "net_information":distribution}]})
}

fn snapshot() -> Value {
    json!({"schema_version":1, "as_of":AS_OF, "factors":[factor("2.0.0"), historical(), factor("1.0.0")],
           "evaluations":[evaluation("second", "1.0.0"), evaluation("fit", "2.0.0"), evaluation("first", "1.0.0")]})
}

#[tokio::test]
async fn factor_read_filters_pages_and_preserves_research_state_and_precision() {
    let dir = tempfile::tempdir().unwrap();
    write_snapshot(dir.path(), &snapshot());
    let app = app(dir.path());
    let (status, page) = get(&app, "/api/v1/factors?limit=1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["schema_version"], 1);
    assert_eq!(page["as_of"], AS_OF);
    assert_eq!(page["items"], json!([historical()]));
    assert_eq!(page["next_cursor"], "research.factor-momentum@1.0.0");

    let (status, page) = get(
        &app,
        "/api/v1/factors?factor_id=synthetic.factor&version=2.0.0&after=research.factor-momentum%401.0.0",
    ).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["items"], json!([factor("2.0.0")]));
    assert_eq!(page["next_cursor"], Value::Null);

    let path = "/api/v1/factors/synthetic.factor/evaluations";
    let (status, page) = get(&app, &format!("{path}?version=1.0.0&limit=1")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["items"], json!([evaluation("first", "1.0.0")]));
    assert_eq!(page["next_cursor"], "1.0.0@first");
    let (status, page) = get(
        &app,
        &format!("{path}?version=1.0.0&limit=1&after=1.0.0%40first"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["items"], json!([evaluation("second", "1.0.0")]));
    assert_eq!(page["next_cursor"], Value::Null);
    let (status, page) = get(&app, "/api/v1/factors/research.factor-momentum/evaluations").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["items"], json!([]));
    let (status, page) = get(&app, &format!("{path}?version=3.0.0")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(page["error"]["code"], "factor_not_found");
}

#[tokio::test]
async fn factor_read_without_snapshot_is_empty_and_never_writes() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(dir.path());
    for path in [
        "/api/v1/factors",
        "/api/v1/factors/synthetic.factor/evaluations",
    ] {
        let (status, body) = get(&app, path).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({"schema_version":1,"as_of":null,"items":[],"next_cursor":null})
        );
    }
    assert_eq!(dir.path().read_dir().unwrap().count(), 0);
}

#[tokio::test]
async fn factor_read_rejects_invalid_queries_and_disallowed_or_malformed_snapshots() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(dir.path());
    for path in [
        "/api/v1/factors?factor_id=..%2fsecret",
        "/api/v1/factors?version=01.0.0",
        "/api/v1/factors?limit=0",
        "/api/v1/factors?limit=101",
        "/api/v1/factors?after=bad",
        "/api/v1/factors?unknown=true",
        "/api/v1/factors/%ff/evaluations",
        "/api/v1/factors/%2fother/evaluations",
        "/api/v1/factors/synthetic.factor/evaluations?after=fit%401.0.0",
    ] {
        let (status, body) = get(&app, path).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}");
        assert_eq!(body["error"]["code"], "invalid_factor_query");
    }

    for (location, malformed) in [
        ("/schema_version", json!(2)),
        ("/factors/0/access/record_local", json!(false)),
        ("/factors/0/access/derive_non_display", json!(false)),
        ("/factors/0/access/redistribute", json!(true)),
        ("/factors/1/selectable_for_backtest", json!(true)),
        ("/factors/1/details/summary_ref", json!("../../private")),
        ("/evaluations/0/cost_bps", json!(0.125)),
        ("/evaluations/0/cost_bps", json!("1E-3")),
        ("/evaluations/0/cost_bps", json!("NaN")),
        ("/evaluations/0/data_version", json!("different")),
    ] {
        let mut snapshot = snapshot();
        *snapshot.pointer_mut(location).unwrap() = malformed;
        write_snapshot(dir.path(), &snapshot);
        let (status, body) = get(&app, "/api/v1/factors/synthetic.factor/evaluations").await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{location}");
        assert_eq!(body["error"]["code"], "factor_snapshot_invalid");
        assert!(body.get("items").is_none());
        assert!(!body.to_string().contains("Synthetic fixture"));
    }
}

#[tokio::test]
async fn factor_read_requires_permission_for_each_redundancy_key_in_its_evaluation() {
    let dir = tempfile::tempdir().unwrap();
    let app = app(dir.path());
    let mut valid = snapshot();
    let peer = json!({
        "factor_id": "synthetic.peer",
        "version": "1.0.0",
        "data_version": "synthetic-peer-data",
        "access": access(),
    });
    let mut second_window = valid["evaluations"][0]["windows"][0].clone();
    second_window["window"] = json!({
        "name": "validation", "phase": "validation", "start": "2026-01-07", "end": "2026-01-08"
    });
    let redundancy = json!({
        "synthetic.peer@1.0.0": {"count": 2, "mean": "-1.0000", "sample_std": null, "ratio": null}
    });
    second_window["redundancy"] = redundancy.clone();
    valid["evaluations"][0]["windows"]
        .as_array_mut()
        .unwrap()
        .push(second_window);
    valid["evaluations"][0]["peer_factors"] = json!([peer]);
    // A separate evaluation retains the same authorized peer in every case.
    valid["evaluations"][1]["peer_factors"] = json!([peer]);

    write_snapshot(dir.path(), &valid);
    let evaluations = "/api/v1/factors/synthetic.factor/evaluations";
    let (status, body) = get(&app, evaluations).await;
    assert_eq!(status, StatusCode::OK);
    let report = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["evaluation_id"] == "second")
        .unwrap();
    assert_eq!(report["windows"][1]["redundancy"], redundancy);
    assert_eq!(report["peer_factors"][0], peer);

    let mut wrong_version = peer.clone();
    wrong_version["version"] = json!("2.0.0");
    let mut denied = peer.clone();
    denied["access"]["derive_non_display"] = json!(false);
    let mut conflicting = peer.clone();
    conflicting["data_version"] = json!("other-synthetic-peer-data");
    for (case, peers) in [
        ("missing from this evaluation", json!([])),
        ("wrong version", json!([wrong_version])),
        ("denied permission", json!([denied])),
        ("duplicate identity", json!([peer, conflicting])),
    ] {
        let mut invalid = valid.clone();
        invalid["evaluations"][0]["peer_factors"] = peers;
        write_snapshot(dir.path(), &invalid);
        for path in [evaluations, "/api/v1/factors"] {
            let (status, body) = get(&app, path).await;
            assert_eq!(
                status,
                StatusCode::INTERNAL_SERVER_ERROR,
                "{case} at {path}"
            );
            assert_eq!(body["error"]["code"], "factor_snapshot_invalid");
            assert!(body.get("items").is_none());
            assert!(!body.to_string().contains("synthetic.peer"));
        }
    }
}

#[test]
fn factor_read_openapi_exports_typed_versions_decimal_and_limits() {
    let spec = serde_json::to_value(ApiDoc::openapi()).unwrap();
    let schemas = &spec["components"]["schemas"];
    assert_eq!(schemas["FactorDecimal"]["type"], "string");
    assert_eq!(
        schemas["FactorDecimal"]["pattern"],
        r"^-?(?:0|[1-9][0-9]*)(?:\.[0-9]+)?$"
    );
    assert_eq!(
        schemas["FactorReadPage"]["properties"]["schema_version"]["minimum"],
        1
    );
    assert_eq!(
        schemas["FactorReadPage"]["properties"]["schema_version"]["maximum"],
        1
    );
    for path in ["/api/v1/factors", "/api/v1/factors/{id}/evaluations"] {
        let operation = &spec["paths"][path]["get"];
        let limit = operation["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["name"] == "limit")
            .unwrap();
        assert_eq!(limit["schema"]["minimum"], 1);
        assert_eq!(limit["schema"]["maximum"], 100);
    }
}

#[tokio::test]
#[ignore = "requires the F8 source environment via FINANCE_F8_TEST_PYTHON"]
async fn python_factor_publication_is_read_by_rust_contract() {
    let python = env::var_os("FINANCE_F8_TEST_PYTHON").expect("set FINANCE_F8_TEST_PYTHON");
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let data = tempfile::tempdir().unwrap();
    let result = Command::new(python)
        .env("PYTHONPATH", env::join_paths([repo.join("python"), repo.join("tests")]).unwrap())
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env("FERTILE_DATA_ROOT", data.path())
        .env("FERTILE_RESEARCH_ROOT", data.path().join("unused-research"))
        .arg("-c")
        .arg(
            r#"
from dataclasses import replace
from decimal import Decimal
from pathlib import Path
import sys

from fertile_finance.factor_evaluation import Distribution
from fertile_finance.factor_read import FactorReadEvaluation, FactorReadPermission, publish_factor_read_snapshot
from fertile_finance.research_catalog import FactorResearchCatalog
from test_factor_read import AS_OF, PRECISE, synthetic_history
from test_factor_research_catalog import definition, report_for

root = Path(sys.argv[1])
fixture_repo = root / 'fixture-repo'
synthetic_history(fixture_repo)
factor = replace(definition(), parameters={'large': 2**64 - 1})
peer = replace(factor, factor_id='fixture.peer', license='Synthetic evaluation peer', data_version='synthetic-peer-v1')
peer_key = (peer.factor_id, peer.version)
report = report_for(factor)
last_window = replace(report.windows[-1], redundancy={
    '@'.join(peer_key): Distribution(count=2, mean=Decimal('-1.0000'), sample_std=None, ratio=None),
})
report = replace(report, cost_bps=PRECISE, peer_definitions=(peer,),
                 windows=(*report.windows[:-1], last_window))
permissions = {
    (factor.factor_id, factor.version): FactorReadPermission(factor.license, True, True),
    peer_key: FactorReadPermission(peer.license, True, True),
    ('research.factor-momentum', '1.0.0'): FactorReadPermission('Synthetic history', True, True),
}
publish_factor_read_snapshot(
    root, as_of=AS_OF, factors=[FactorResearchCatalog().register(factor)],
    evaluations=[FactorReadEvaluation('fit-1', report)], historical_repo_root=fixture_repo,
    permissions=permissions,
)
"#,
        )
        .arg(data.path())
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );

    let app = app(data.path());
    let (status, factors) = get(&app, "/api/v1/factors").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(factors["as_of"], AS_OF);
    assert_eq!(
        factors["items"][0]["details"]["parameters"]["large"]["value"],
        u64::MAX.to_string()
    );
    let historical = &factors["items"][1];
    assert_eq!(historical["state"], "research_only");
    assert_eq!(
        historical["details"]["decision"]["comparisons"]["longest_sample"]["main_sharpe"],
        PRECISE
    );
    let (status, evaluations) = get(&app, "/api/v1/factors/fixture.review/evaluations").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(evaluations["items"][0]["cost_bps"], PRECISE);
    assert_eq!(
        evaluations["items"][0]["access"]["license"],
        "Synthetic test fixture"
    );
    assert_eq!(
        evaluations["items"][0]["windows"][2]["window"]["phase"],
        "out_of_sample"
    );
    assert_eq!(
        evaluations["items"][0]["peer_factors"],
        json!([{
            "factor_id": "fixture.peer", "version": "1.0.0", "data_version": "synthetic-peer-v1",
            "access": {
                "license": "Synthetic evaluation peer", "record_local": true,
                "derive_non_display": true, "redistribute": false
            }
        }])
    );
    assert_eq!(
        evaluations["items"][0]["windows"][2]["redundancy"]["fixture.peer@1.0.0"],
        json!({"count": 2, "mean": "-1.0000", "sample_std": null, "ratio": null})
    );
}
