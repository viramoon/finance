use finance_ib::{ConnectionManager, IbConfig, IbTransport};
#[tokio::test]
#[ignore = "requires an authenticated local paper Gateway and IB_LIVE_TEST=1"]
async fn opt_in_paper_probe() {
    assert_eq!(
        std::env::var("IB_LIVE_TEST").as_deref(),
        Ok("1"),
        "explicit opt-in required"
    );
    let config_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config");
    let cfg =
        IbConfig::from_layers(&config_dir, "cloudbox", |key| std::env::var(key).ok()).unwrap();
    let (timestamp, accounts) = ConnectionManager::new(cfg, IbTransport)
        .unwrap()
        .probe()
        .await
        .unwrap();
    assert!(timestamp > 0 && !accounts.is_empty());
}

#[test]
#[ignore = "requires an authenticated local read-only paper Gateway and IB_LIVE_TEST=1"]
fn opt_in_paper_entitlements() {
    assert!(
        std::env::var_os("IB_LIVE_TEST").as_deref() == Some(std::ffi::OsStr::new("1")),
        "explicit opt-in required"
    );
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_finance-ib"));
    command
        .current_dir(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
        .arg("entitlements");
    if let Some(raw) = std::env::var_os("IB_OPRA_TEST_CON_ID") {
        let con_id = raw.to_str().and_then(|raw| raw.parse::<i32>().ok());
        assert!(
            con_id.is_some_and(|id| id > 0),
            "invalid IB_OPRA_TEST_CON_ID"
        );
        command.arg("--opra-con-id").arg(raw);
    }
    let output = command.output().unwrap();
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    println!("{report}");
    assert!(output.status.success(), "paper probe could not complete");
}
