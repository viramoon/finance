use finance_ib::{ConnectionManager, IbConfig, IbTransport};
use std::{process::ExitCode, time::Instant};
#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .json()
        .with_writer(std::io::stderr)
        .init();
    if std::env::args().skip(1).collect::<Vec<_>>() != ["probe"] {
        eprintln!("usage: finance-ib probe");
        return ExitCode::FAILURE;
    }
    let start = Instant::now();
    let result = async {
        ConnectionManager::new(IbConfig::load()?, IbTransport)?
            .probe()
            .await
    }
    .await;
    match result {
        Ok((time, accounts)) => {
            println!(
                "{}",
                serde_json::json!({"server_time_unix":time,"paper_accounts":accounts,"elapsed_ms":start.elapsed().as_millis()})
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!(
                "IB paper probe failed after {} ms: {e}",
                start.elapsed().as_millis()
            );
            ExitCode::FAILURE
        }
    }
}
