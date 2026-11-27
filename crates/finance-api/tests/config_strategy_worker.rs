//! Cross-language acceptance through the real HTTP router and isolated worker.
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use finance_api::{AppState, router};
use rusqlite::Connection;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    env, fs,
    path::Path,
    process::{Child, Command},
    thread,
    time::{Duration, Instant},
};
use tower::ServiceExt;

const JOBS: &str = "/api/v1/backtests";
const ENTRY_EXIT: &str =
    include_str!("../../../strategies/fixtures/strategy-bundle/v1/entry-exit.yaml");
const TARGET_WEIGHTS: &str =
    include_str!("../../../strategies/fixtures/strategy-bundle/v1/target-weights.yaml");

async fn request(
    app: &Router,
    method: &str,
    path: &str,
    key: &str,
    body: Option<Value>,
    expected: StatusCode,
) -> Value {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("Content-Type", "application/json")
                .header("Idempotency-Key", key)
                .body(body.map_or_else(Body::empty, |value| Body::from(value.to_string())))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let data = to_bytes(response.into_body(), 4 << 20).await.unwrap();
    assert_eq!(status, expected, "{}", String::from_utf8_lossy(&data));
    serde_json::from_slice(&data).unwrap()
}

fn job(source: Value) -> Value {
    json!({
        "strategy": "StrategyBundle",
        "start": "2024-03-01T00:00:00Z",
        "end": "2024-04-15T00:00:00Z",
        "source": source
    })
}

fn inline(content: &str) -> Value {
    job(json!({"kind":"inline", "format":"yaml", "content":content}))
}

async fn submit(app: &Router, key: &str, job: Value) -> Value {
    request(app, "POST", JOBS, key, Some(job), StatusCode::ACCEPTED).await
}

fn decimal(value: &Value) -> Decimal {
    value.as_str().unwrap().parse().unwrap()
}

fn python_command(data_root: &Path, research_root: &Path) -> Command {
    let python = env::var_os("FINANCE_STRATEGY_TEST_PYTHON").expect("source-built Python path");
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut command = Command::new(python);
    command
        .current_dir(&repo)
        .env("PYTHONPATH", repo.join("python"))
        .env("FERTILE_DATA_ROOT", data_root)
        .env("FERTILE_RESEARCH_ROOT", research_root)
        .env("OPENBLAS_NUM_THREADS", "1")
        .env("OMP_NUM_THREADS", "1");
    command
}

fn generate_replay(command: &mut Command, data_root: &Path, flags: &[&str]) {
    let fixture = command
        .arg("tests/config_strategy_replay_fixture.py")
        .arg(data_root)
        .args(flags)
        .output()
        .unwrap();
    assert!(
        fixture.status.success(),
        "{}",
        String::from_utf8_lossy(&fixture.stderr)
    );
}

struct Worker(Child);

impl Worker {
    fn start(command: &mut Command, log: &Path) -> Self {
        let stdout = fs::File::create(log).unwrap();
        Self(
            command
                .args([
                    "-m",
                    "fertile_finance.worker",
                    "--isolated",
                    "--timeout",
                    "30",
                ])
                .stdout(stdout.try_clone().unwrap())
                .stderr(stdout)
                .spawn()
                .unwrap(),
        )
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

async fn await_job(app: &Router, job: &Value, worker: &mut Worker, log: &Path) -> Value {
    await_job_status(app, job, worker, log, "succeeded").await
}

async fn await_job_status(
    app: &Router,
    job: &Value,
    worker: &mut Worker,
    log: &Path,
    expected_status: &str,
) -> Value {
    let id = job["id"].as_i64().unwrap();
    let path = format!("{JOBS}/{id}");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let result = request(app, "GET", &path, "", None, StatusCode::OK).await;
        match result["status"].as_str().unwrap() {
            status if status == expected_status => return result,
            "queued" | "running"
                if worker.0.try_wait().unwrap().is_none() && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(50));
            }
            _ => panic!(
                "job {id}: {result}\nworker log:\n{}",
                fs::read_to_string(log).unwrap_or_default()
            ),
        }
    }
}

#[tokio::test]
#[ignore = "requires the source-built Nautilus Python environment; exercised by Nautilus CI"]
async fn unsupported_registered_indicator_reports_strategy_runtime_invalid() {
    let temp = tempfile::tempdir().unwrap();
    let data_root = temp.path().join("data");
    let research_root = temp.path().join("research");
    fs::create_dir(&research_root).unwrap();
    generate_replay(
        &mut python_command(&data_root, &research_root),
        &data_root,
        &[],
    );
    let app = router(AppState {
        data_root: data_root.clone(),
        research_root: research_root.clone(),
    });
    let bundle = ENTRY_EXIT
        .replace("factor.close_return@1.0.0", "indicator.bollinger@1.0.0")
        .replace(
            "params: {lookback_sessions: 20}",
            "params: {period: 20}\n      output: upper",
        );
    let job = submit(&app, "unsupported-indicator", inline(&bundle)).await;

    let log = temp.path().join("worker.log");
    let mut worker = Worker::start(&mut python_command(&data_root, &research_root), &log);
    let failed = await_job_status(&app, &job, &mut worker, &log, "failed").await;
    assert_eq!(failed["attempt"], 1);
    assert_eq!(failed["error"], "strategy_runtime_invalid");
    assert!(failed["summary"].is_null());
}

#[tokio::test]
#[ignore = "requires the source-built Nautilus Python environment; exercised by Nautilus CI"]
async fn configured_recipes_survive_retry_and_execute_in_the_real_worker() {
    let temp = tempfile::tempdir().unwrap();
    let data_root = temp.path().join("data");
    let research_root = temp.path().join("research");
    fs::create_dir(&research_root).unwrap();
    generate_replay(
        &mut python_command(&data_root, &research_root),
        &data_root,
        &[],
    );

    let app = router(AppState {
        data_root: data_root.clone(),
        research_root: research_root.clone(),
    });
    let registered = request(
        &app,
        "POST",
        "/api/v1/strategies",
        "",
        Some(json!({"format":"yaml", "content":ENTRY_EXIT})),
        StatusCode::OK,
    )
    .await;
    let registered_job = job(json!({
        "kind":"registered", "id":registered["id"], "version":registered["version"]
    }));
    let (entry, retry) = tokio::join!(
        submit(&app, "entry-exit", registered_job.clone()),
        submit(
            &app,
            "entry-exit",
            inline(&format!("# Equivalent document\n{ENTRY_EXIT}"))
        ),
    );
    assert_eq!(entry, retry);
    let targets = submit(&app, "targets", inline(TARGET_WEIGHTS)).await;
    let conflict_bundle = ENTRY_EXIT
        .replace("version: \"1.0.0\"", "version: \"1.0.1\"")
        .replace(
            "- {recipe: A1_B2, budget_weight: \"1.00\", priority: 10}",
            "- {recipe: A, budget_weight: \"0.6\", priority: 20}\n      - {recipe: A1_B2, budget_weight: \"0.4\", priority: 10}",
        )
        .replace("    max_weight_per_asset: \"0.25\"", "    max_weight_per_asset: \"0.10\"");
    let conflict = submit(&app, "conflict", inline(&conflict_bundle)).await;

    let queue = data_root.join("finance-state/backtests");
    let db = Connection::open(queue.join("jobs.sqlite3")).unwrap();
    let count: i64 = db
        .query_row("SELECT count(*) FROM jobs", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 3);
    db.execute(
        "UPDATE jobs SET status='running', attempt=1 WHERE id=?1",
        [entry["id"].as_i64().unwrap()],
    )
    .unwrap();
    drop(db);
    // A restarted worker must read the transactionally pinned replay bytes.
    let source = queue.join(
        "strategy-inputs/example.reviewed-us-daily@1.0.0/example.us-cash-equities@1.0.0.json",
    );
    let snapshot_sha256 = format!("{:x}", Sha256::digest(fs::read(&source).unwrap()));
    fs::write(source, b"provider file replaced after submission").unwrap();
    assert_eq!(submit(&app, "entry-exit", registered_job).await, entry);

    let log = temp.path().join("worker.log");
    let mut worker = Worker::start(&mut python_command(&data_root, &research_root), &log);
    let entry = await_job(&app, &entry, &mut worker, &log).await;
    let targets = await_job(&app, &targets, &mut worker, &log).await;
    let conflict = await_job(&app, &conflict, &mut worker, &log).await;
    assert_eq!(entry["attempt"], 2);
    assert_eq!(targets["attempt"], 1);
    assert_eq!(conflict["attempt"], 1);
    for completed in [&entry, &targets, &conflict] {
        let summary = &completed["summary"];
        assert_eq!(summary["strategy"], "StrategyBundle");
        assert!(summary["total_orders"].as_u64().unwrap() > 0);
        let manifest = &summary["run_manifest"];
        let report = &summary["strategy_report"];
        assert_eq!(
            manifest["content_sha256"],
            completed["request"]["content_sha256"]
        );
        assert_eq!(manifest["data"]["snapshot_sha256"], snapshot_sha256);
        assert_eq!(manifest["data"]["partitions"].as_array().unwrap().len(), 3);
        assert_eq!(manifest["availability"]["excluded_late_bars"], 2);
        assert_eq!(manifest["interval"]["last_session"], "2024-04-12");
        let mark = report["pnl_as_of"].as_str().unwrap();
        assert_eq!(
            decimal(&summary["pnl"]),
            decimal(&report["equity"][mark]) - decimal(&report["initial_cash"])
        );
        let sleeve_cash: Decimal = report["sleeves"]
            .as_array()
            .unwrap()
            .iter()
            .map(|sleeve| decimal(&sleeve["cash"]))
            .sum();
        assert_eq!(sleeve_cash, decimal(&report["cash"]));
    }

    let entry_report = &entry["summary"]["strategy_report"];
    let entry_fills = entry_report["fills"].as_array().unwrap();
    let bbb: Vec<_> = entry_fills
        .iter()
        .filter(|fill| fill["instrument_id"] == "BBB.XNYS")
        .collect();
    assert_eq!(bbb[0]["side"], "buy");
    assert_eq!(bbb[0]["signal_date"], "2024-03-01");
    assert_eq!(bbb[0]["fill_date"], "2024-03-04");
    assert_eq!(
        decimal(&bbb[0]["price"]),
        "162.08".parse::<Decimal>().unwrap()
    );
    assert!(decimal(&bbb[0]["commission"]) > Decimal::ZERO);
    assert!(bbb[0]["fill_ts"].is_string());
    let entry_attributions = entry_report["attribution"].as_array().unwrap();
    let timed_exit = entry_attributions
        .iter()
        .find(|row| {
            row["instrument_id"] == "BBB.XNYS"
                && row["intent_reason"] == "recipe_exit"
                && row["status"] == "accepted"
        })
        .expect("B2 must produce a filled holding-period exit");
    let exit = bbb
        .iter()
        .find(|fill| fill["order_id"] == timed_exit["order_id"])
        .unwrap();
    assert_eq!(exit["side"], "sell");
    assert_eq!(exit["signal_date"], "2024-03-18");
    assert_eq!(exit["fill_date"], "2024-03-20");
    for (instrument, when) in [("AAA.XNYS", "2024-03-04"), ("BBB.XNYS", "2024-03-19")] {
        assert!(
            entry_attributions.iter().any(|row| {
                row["instrument_id"] == instrument
                    && row["execution_date"] == when
                    && row["status"] == "deferred"
                    && row["reason"] == "execution_price_missing"
            }),
            "missing delayed-price attribution for {instrument} on {when}"
        );
    }

    let target_attributions = targets["summary"]["strategy_report"]["attribution"]
        .as_array()
        .unwrap();
    assert!(target_attributions.iter().any(|row| {
        row["recipe"] == "topFive"
            && row["intent_reason"] == "target_weight"
            && row
                .get("external_quantity")
                .is_some_and(|quantity| decimal(quantity) > Decimal::ZERO)
    }));

    let conflict_attributions = conflict["summary"]["strategy_report"]["attribution"]
        .as_array()
        .unwrap();
    let first_bbb: Vec<_> = conflict_attributions
        .iter()
        .filter(|row| row["instrument_id"] == "BBB.XNYS" && row["execution_date"] == "2024-03-04")
        .collect();
    assert!(
        first_bbb
            .iter()
            .any(|row| row["recipe"] == "A1_B2"
                && decimal(&row["accepted_quantity"]) > Decimal::ZERO)
    );
    assert!(first_bbb.iter().any(|row| row["recipe"] == "A"
        && row["status"] == "rejected"
        && row["reason"] == "portfolio_asset_limit"));
}

#[tokio::test]
#[ignore = "requires the source-built Nautilus Python environment; exercised by Nautilus CI"]
async fn target_weights_rank_truncate_and_rebalance_to_equal_notional() {
    let temp = tempfile::tempdir().unwrap();
    let data_root = temp.path().join("data");
    let research_root = temp.path().join("research");
    fs::create_dir(&research_root).unwrap();
    generate_replay(
        &mut python_command(&data_root, &research_root),
        &data_root,
        &["--target-weights"],
    );
    let app = router(AppState {
        data_root: data_root.clone(),
        research_root: research_root.clone(),
    });
    let bundle = TARGET_WEIGHTS
        .replace("top_n: 5", "top_n: 2")
        .replace("amount: \"100000\"", "amount: \"10000\"")
        .replace(
            "max_weight_per_asset: \"0.25\"",
            "max_weight_per_asset: \"1\"",
        )
        .replace("commission_bps: \"2\"", "commission_bps: \"0\"")
        .replace("slippage_bps: \"5\"", "slippage_bps: \"0\"");
    let mut submission = inline(&bundle);
    submission["end"] = json!("2024-03-08T00:00:00Z");
    let job = submit(&app, "rank-and-rebalance", submission).await;

    let log = temp.path().join("worker.log");
    let mut worker = Worker::start(&mut python_command(&data_root, &research_root), &log);
    let completed = await_job(&app, &job, &mut worker, &log).await;
    let summary = &completed["summary"];
    let report = &summary["strategy_report"];
    let fills: Vec<_> = report["fills"]
        .as_array()
        .unwrap()
        .iter()
        .map(|fill| {
            (
                fill["instrument_id"].as_str().unwrap(),
                fill["side"].as_str().unwrap(),
                fill["signal_date"].as_str().unwrap(),
                fill["fill_date"].as_str().unwrap(),
                decimal(&fill["quantity"]),
                decimal(&fill["quantity"]) * decimal(&fill["price"]),
            )
        })
        .collect();
    let leg = |instrument, side, signal, executed, shares| {
        (
            instrument,
            side,
            signal,
            executed,
            Decimal::from(shares),
            Decimal::from(5000),
        )
    };
    assert_eq!(
        fills,
        [
            leg("BBB.XNYS", "buy", "2024-03-01", "2024-03-04", 50),
            leg("CCC.XNYS", "buy", "2024-03-01", "2024-03-04", 100),
            leg("BBB.XNYS", "sell", "2024-03-05", "2024-03-06", 50),
            leg("AAA.XNYS", "buy", "2024-03-05", "2024-03-06", 20),
        ]
    );
    assert_eq!(
        report["positions"],
        json!({"AAA.XNYS":"20", "CCC.XNYS":"100"})
    );
    assert_eq!(decimal(&report["cash"]), Decimal::ZERO);
    assert_eq!(decimal(&summary["pnl"]), Decimal::ZERO);
}
