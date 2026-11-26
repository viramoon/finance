use finance_ib::{
    ConnectionManager, IbConfig, IbTransport,
    historical::{MINUTE_DECIMAL_POLICY, MinuteRequest},
};
use std::{env, path::PathBuf, process::ExitCode, time::Instant};

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .json()
        .with_writer(std::io::stderr)
        .init();
    let args: Vec<String> = env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["probe"] => probe().await,
        ["historical-minute", "--allow-download", symbol, start, end] => {
            historical_minute(symbol, start, end).await
        }
        _ => {
            eprintln!(
                "usage: finance-ib probe | historical-minute --allow-download SYMBOL START_UNIX END_UNIX"
            );
            ExitCode::FAILURE
        }
    }
}

async fn probe() -> ExitCode {
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

async fn historical_minute(symbol: &str, start: &str, end: &str) -> ExitCode {
    let result = async {
        let request = MinuteRequest::parse(symbol, start, end)?;
        let root = env::var_os("FERTILE_DATA_ROOT").ok_or_else(|| {
            finance_ib::Error::Config("historical-minute requires FERTILE_DATA_ROOT".into())
        })?;
        ConnectionManager::new(IbConfig::load()?, IbTransport)?
            .historical_minute(request, &PathBuf::from(root))
            .await
    }
    .await;
    match result {
        Ok(bars) => {
            println!(
                "{}",
                serde_json::json!({"precision": MINUTE_DECIMAL_POLICY, "bars": bars})
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("IB paper historical-minute failed: {error}");
            ExitCode::FAILURE
        }
    }
}
