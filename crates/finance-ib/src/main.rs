use finance_ib::{
    ConnectionManager, IbConfig, IbTransport,
    entitlements::{ProbeOptions, USAGE, probe_paper},
};
use std::{ffi::OsStr, process::ExitCode, time::Instant};

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    match args.as_slice() {
        [command] if command == "probe" => connection_probe().await,
        [command, rest @ ..] if command == "entitlements" => entitlement_probe(rest).await,
        _ => {
            eprintln!("usage: finance-ib probe | {USAGE}");
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
    if std::env::var_os("IB_LIVE_TEST").as_deref() != Some(OsStr::new("1")) {
        eprintln!("the private paper probe requires IB_LIVE_TEST=1");
        return ExitCode::FAILURE;
    }
    // This command emits only its allowlisted report. Leave SDK logging disabled.
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
    tracing_subscriber::fmt()
        .json()
        .with_writer(std::io::stderr)
        .init();
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
