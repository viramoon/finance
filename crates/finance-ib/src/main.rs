use finance_ib::{
    ConnectionManager, IbConfig, IbTransport,
    entitlements::{ProbeOptions, USAGE, probe_paper},
    historical::{MINUTE_DECIMAL_POLICY, MinuteRequest},
};
use std::{env, ffi::OsStr, path::PathBuf, process::ExitCode, time::Instant};

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<_> = env::args_os().skip(1).collect();
    if let [command, rest @ ..] = args.as_slice()
        && command == "entitlements"
    {
        // The private probe emits only its allowlisted report. Leave SDK logging disabled.
        return entitlement_probe(rest).await;
    }
    tracing_subscriber::fmt()
        .json()
        .with_writer(std::io::stderr)
        .init();
    let args: Option<Vec<_>> = args.iter().map(|arg| arg.to_str()).collect();
    match args.as_deref() {
        Some(["probe"]) => connection_probe().await,
        Some(["historical-minute", "--allow-download", symbol, start, end]) => {
            historical_minute(symbol, start, end).await
        }
        _ => {
            eprintln!(
                "usage: finance-ib probe | historical-minute --allow-download SYMBOL START_UNIX END_UNIX | {USAGE}"
            );
            ExitCode::FAILURE
        }
    }
}

async fn entitlement_probe(args: &[std::ffi::OsString]) -> ExitCode {
    if args == ["--help"] {
        println!("usage: {USAGE}\nRequires IB_LIVE_TEST=1 and a read-only paper Gateway.");
        return ExitCode::SUCCESS;
    }
    let Some(options) = ProbeOptions::parse(args) else {
        eprintln!("usage: {USAGE}");
        return ExitCode::FAILURE;
    };
    if env::var_os("IB_LIVE_TEST").as_deref() != Some(OsStr::new("1")) {
        eprintln!("the private paper probe requires IB_LIVE_TEST=1");
        return ExitCode::FAILURE;
    }
    let report = probe_paper(options).await;
    println!(
        "{}",
        serde_json::to_string(&report).expect("serializable report")
    );
    if report.failure.is_some() {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

async fn connection_probe() -> ExitCode {
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
