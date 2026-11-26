use finance_ib::{ConnectionManager, IbConfig, IbTransport, historical::MinuteRequest};

#[tokio::test]
#[ignore = "requires an authenticated local paper Gateway and an authorized history window"]
async fn opt_in_paper_historical_minute() {
    assert_eq!(
        std::env::var("IB_HISTORY_PAPER_TEST").as_deref(),
        Ok("1"),
        "explicit opt-in required"
    );
    let root = std::path::PathBuf::from(std::env::var_os("FERTILE_DATA_ROOT").unwrap());
    let start = std::env::var("IB_HISTORY_START_UNIX").unwrap();
    let end = std::env::var("IB_HISTORY_END_UNIX").unwrap();
    let request = MinuteRequest::parse("SPY", &start, &end).unwrap();
    let config_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config");
    let config =
        IbConfig::from_layers(&config_dir, "cloudbox", |key| std::env::var(key).ok()).unwrap();
    let bars = ConnectionManager::new(config, IbTransport)
        .unwrap()
        .historical_minute(request, &root)
        .await
        .unwrap();
    assert!(!bars.is_empty());
}
