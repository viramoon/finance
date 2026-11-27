use serde_json::{Value, json};
use std::process::Command;

#[test]
fn cli_compiles_a_deterministic_legacy_sma_plan() {
    let run = || {
        Command::new(env!("CARGO_BIN_EXE_finance-strategy"))
            .args(["compile-legacy-sma", "TEST.XNYS", "2", "3", "10"])
            .output()
            .unwrap()
    };
    let first = run();
    assert!(first.status.success());
    assert!(first.stderr.is_empty());
    assert_eq!(run().stdout, first.stdout);
    let plan: Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(
        plan,
        json!({
            "content_sha256": "ac3016ed6fc3cbb89f897599c0bf506edfc7fdea2045afeb5b4c20bd17385564",
            "instrument_id": "TEST.XNYS",
            "parameters": { "fast_period": 2, "slow_period": 3, "shares": 10 },
            "plan_version": "fertile.finance/sma-crossover-plan/v1",
            "strategy": "SmaCrossover",
        })
    );
}

#[test]
fn cli_rejects_invalid_legacy_sma_instruments_and_parameters() {
    for values in [
        ["TEST.XNAS", "2", "3", "10"],
        ["TEST.XNYS", "3", "3", "10"],
        ["TEST.XNYS", "2", "251", "10"],
        ["TEST.XNYS", "2", "3", "0"],
        ["TEST.XNYS", "2", "3", "1001"],
        ["TEST.XNYS", "2", "3", "1.0"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_finance-strategy"))
            .arg("compile-legacy-sma")
            .args(values)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
    }
}
