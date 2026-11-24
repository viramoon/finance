use std::{
    net::TcpListener,
    path::Path,
    process::Command,
    time::{Duration, Instant},
};

#[test]
fn silent_tcp_peer_cannot_block_probe() {
    // A listening socket that neither accepts nor speaks the IB handshake.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let start = Instant::now();
    let output = Command::new(env!("CARGO_BIN_EXE_finance-ib"))
        .arg("probe")
        .env(
            "IB_GATEWAY_ADDRESS",
            listener.local_addr().unwrap().to_string(),
        )
        .env("IB_CONNECT_TIMEOUT_MS", "80")
        .env("FERTILE_PROFILE", "cloudbox")
        .env(
            "FINANCE_CONFIG_DIR",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config"),
        )
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("timed out"));
    assert!(start.elapsed() < Duration::from_secs(3));
}
