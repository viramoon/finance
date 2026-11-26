use std::process::Command;

#[test]
fn historical_minute_requires_explicit_authorization_before_config() {
    let root = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_finance-ib"))
        .args(["historical-minute", "SPY", "1735917000", "1735917120"])
        .env_clear()
        .env("FERTILE_DATA_ROOT", root.path())
        .env("FINANCE_CONFIG_DIR", root.path().join("missing-config"))
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("historical-minute --allow-download SYMBOL START_UNIX END_UNIX")
    );
    assert!(root.path().read_dir().unwrap().next().is_none());
}
