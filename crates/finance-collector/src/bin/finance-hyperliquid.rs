use std::{
    env,
    ffi::{OsStr, OsString},
    fs,
    path::PathBuf,
    process::Stdio,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail, ensure};
use finance_collector::hyperliquid::{Config, Hyperliquid, RejectedFrame, TransportEvent};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, BufReader},
    process::{Child, Command},
    signal::unix::{SignalKind, signal},
    time::{Instant, MissedTickBehavior, interval, sleep},
};

const USAGE: &str = "usage: finance-hyperliquid CONFIG [--replay FILE]";

fn now() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_nanos()
        .try_into()?)
}

fn start_transport(python: &OsStr, config: &Config) -> Result<Child> {
    Command::new(python)
        .args(["-m", "fertile_finance.hyperliquid_transport"])
        .args(config.coins.iter().map(|coin| coin.as_str()))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("start public Hyperliquid transport")
}

struct Collector {
    config: Config,
    root: PathBuf,
    adapter: Option<Hyperliquid>,
}

impl Collector {
    fn maintain(&mut self) -> Result<()> {
        if let Some(adapter) = &mut self.adapter {
            adapter.expire_display(now()?);
            adapter.ingress().flush()?;
        }
        Ok(())
    }

    async fn ingest(&mut self, input: impl AsyncRead + Unpin, replay: bool) -> Result<()> {
        let mut lines = BufReader::new(input).lines();
        let mut tick = interval(Duration::from_millis(100));
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut needs_metadata = true;
        let mut discard_session = false;
        loop {
            let line = tokio::select! {
                line = lines.next_line() => line?,
                _ = tick.tick(), if !replay => {
                    self.maintain()?;
                    continue;
                }
            };
            let Some(line) = line else {
                ensure!(replay && !needs_metadata, "Hyperliquid transport ended");
                return Ok(());
            };
            let event: TransportEvent = serde_json::from_str(&line).map_err(|error| {
                anyhow::anyhow!("invalid transport message at column {}", error.column())
            })?;
            if needs_metadata {
                let TransportEvent::Metadata { data, receive_ts } = event else {
                    bail!("Hyperliquid transport must supply metadata first");
                };
                if let Some(adapter) = &self.adapter {
                    adapter.verify_metadata(&data)?;
                } else {
                    self.adapter = Some(Hyperliquid::open(
                        &self.root,
                        self.config.clone(),
                        &data,
                        receive_ts.get(),
                    )?);
                }
                needs_metadata = false;
                continue;
            }
            if discard_session {
                match event {
                    TransportEvent::Connected { .. } => discard_session = false,
                    TransportEvent::Frame { .. } | TransportEvent::Disconnected { .. } => continue,
                    TransportEvent::Metadata { .. } => bail!("unexpected second metadata message"),
                }
            }
            let frame_time = match &event {
                TransportEvent::Frame { receive_ts, .. } => Some(receive_ts.get()),
                _ => None,
            };
            let adapter = self.adapter.as_mut().expect("metadata initialized adapter");
            if let Err(error) = adapter.apply(event) {
                let Some(received) = frame_time.filter(|_| error.is::<RejectedFrame>()) else {
                    return Err(error);
                };
                adapter.disconnect(received)?;
                eprintln!("Hyperliquid public WebSocket retry: {error}");
                if !replay {
                    return Ok(());
                }
                discard_session = true;
            }
        }
    }

    async fn run_live(&mut self, python: OsString) -> Result<()> {
        let mut delay = 1;
        loop {
            let started = Instant::now();
            let mut child = start_transport(&python, &self.config)?;
            let output = child
                .stdout
                .take()
                .context("missing Hyperliquid transport stdout")?;
            let result = self.ingest(output, false).await;
            let stopped = child.kill().await;
            result?;
            stopped.context("stop rejected Hyperliquid transport")?;
            if started.elapsed() >= Duration::from_secs(30) {
                delay = 1;
            }
            for _ in 0..delay {
                sleep(Duration::from_secs(1)).await;
                self.maintain()?;
            }
            delay = (delay * 2).min(30);
        }
    }

    fn finish(&mut self, replay: bool) -> Result<()> {
        if let Some(adapter) = &mut self.adapter {
            if !replay {
                adapter.disconnect(now()?)?;
            }
            adapter.ingress().flush()?;
        }
        Ok(())
    }
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
    let mut collector = Collector {
        config,
        root,
        adapter: None,
    };
    let mut term = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let result = tokio::select! {
        _ = term.recv() => Ok(()),
        _ = interrupt.recv() => Ok(()),
        result = async {
            if let Some(path) = &replay {
                collector.ingest(tokio::fs::File::open(path).await?, true).await
            } else {
                let python = env::var_os("FINANCE_PYTHON")
                    .context("set FINANCE_PYTHON to the source-built Nautilus Python")?;
                collector.run_live(python).await
            }
        } => result,
    };
    result.and(collector.finish(replay.is_some()))
}
