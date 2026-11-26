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
