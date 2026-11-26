use std::{
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail, ensure};
use finance_collector::hyperliquid::{Config, Hyperliquid, TransportEvent};
use tokio::{
    io::{AsyncRead, BufReader},
    process::{Child, Command},
    signal::unix::{SignalKind, signal},
    time::{MissedTickBehavior, interval},
};

const USAGE: &str = "usage: finance-hyperliquid CONFIG [--replay FILE]";

fn now() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_nanos()
        .try_into()?)
}

fn start_transport(python: OsString, config: &Config) -> Result<Child> {
    Command::new(python)
        .args(["-m", "fertile_finance.hyperliquid_transport"])
        .args(config.coins.iter().map(|coin| coin.as_str()))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("start public Hyperliquid transport")
}

async fn ingest(
    input: impl AsyncRead + Unpin,
    config: Config,
    root: &Path,
    replay: bool,
) -> Result<()> {
    use tokio::io::AsyncBufReadExt;

    let mut lines = BufReader::new(input).lines();
    let mut adapter: Option<Hyperliquid> = None;
    let mut config = Some(config);
    let mut tick = interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut term = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let result: Result<()> = async {
        loop {
            let line = tokio::select! {
                line = lines.next_line() => line?,
                _ = term.recv() => break,
                _ = interrupt.recv() => break,
                _ = tick.tick(), if !replay => {
                    if let Some(adapter) = &mut adapter {
                        adapter.expire_display(now()?);
                        adapter.ingress().flush()?;
                    }
                    continue;
                }
            };
            let Some(line) = line else {
                ensure!(replay && adapter.is_some(), "Hyperliquid transport ended");
                break;
            };
            let event: TransportEvent = serde_json::from_str(&line).map_err(|error| {
                anyhow::anyhow!("invalid transport message at column {}", error.column())
            })?;
            if let Some(adapter) = &mut adapter {
                adapter.apply(event)?;
            } else if let TransportEvent::Metadata { data, receive_ts } = event {
                let settings = config.take().context("missing Hyperliquid configuration")?;
                adapter = Some(Hyperliquid::open(root, settings, &data, receive_ts.get())?);
            } else {
                bail!("Hyperliquid transport must supply metadata first");
            }
        }
        Ok(())
    }
    .await;
    let cleanup = if let Some(adapter) = &mut adapter {
        (|| -> Result<()> {
            if !replay {
                adapter.disconnect(now()?)?;
            }
            adapter.ingress().flush()?;
            Ok(())
        })()
    } else {
        Ok(())
    };
    result.and(cleanup)
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = env::args_os().skip(1);
    let config_path = args.next().context(USAGE)?;
    let replay = match args.next() {
        None => None,
        Some(flag) if flag == "--replay" => Some(PathBuf::from(args.next().context(USAGE)?)),
        _ => bail!(USAGE),
    };
    ensure!(args.next().is_none(), USAGE);
    let config: Config = serde_yaml::from_slice(&fs::read(config_path)?)
        .context("invalid Hyperliquid configuration")?;
    config.validate()?;
    let root = PathBuf::from(
        env::var_os("FERTILE_DATA_ROOT").context("set FERTILE_DATA_ROOT for Hyperliquid")?,
    );
    ensure!(
        !config.policy.record_local || root.is_dir(),
        "WAL data root must be an existing directory"
    );
    if let Some(path) = replay {
        return ingest(tokio::fs::File::open(path).await?, config, &root, true).await;
    }
    let mut child = start_transport(
        env::var_os("FINANCE_PYTHON")
            .context("set FINANCE_PYTHON to the source-built Nautilus Python")?,
        &config,
    )?;
    let output = child
        .stdout
        .take()
        .context("missing Hyperliquid transport stdout")?;
    let result = ingest(output, config, &root, false).await;
    let _ = child.kill().await;
    result
}
