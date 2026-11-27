use std::{env, path::PathBuf};

use anyhow::{Result, ensure};
use finance_collector::binance::{self, Config, Observer, Status};
use tokio::{
    signal::unix::{SignalKind, signal},
    sync::watch,
};

struct Log;

impl Observer for Log {
    fn status(&mut self, status: Status) {
        // Structured operational status. Market payloads stay inside F3.
        eprintln!(
            "{}",
            serde_json::to_string(&status).expect("serializable Binance status")
        );
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .json()
        .with_writer(std::io::stderr)
        .init();
    let profile = env::var("FERTILE_PROFILE").unwrap_or_else(|_| "cloudbox".into());
    ensure!(
        matches!(profile.as_str(), "cloudbox" | "stable"),
        "unsupported profile"
    );
    let config_dir = env::var_os("FINANCE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| "config".into());
    let path = env::var_os("FINANCE_BINANCE_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| config_dir.join("binance").join(format!("{profile}.yaml")));
    let config = Config::load(&path)?;
    let root = env::var_os("FERTILE_DATA_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| "/home/box/fertile/data".into());

    let (shutdown, stop) = watch::channel(false);
    let mut term = signal(SignalKind::terminate())?;
    let signal = tokio::spawn(async move {
        tokio::select! {
            _ = term.recv() => {},
            _ = tokio::signal::ctrl_c() => {},
        }
        let _ = shutdown.send(true);
    });
    let result = binance::run(&config, &root, stop, &mut Log).await;
    signal.abort();
    result.map(|_| ())
}
