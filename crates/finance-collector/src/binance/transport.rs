use std::{env, process::Stdio};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, ChildStdout, Command},
};

use super::{
    Config,
    wire::{ExchangeInfo, Interest, Market, Snapshot, StreamMessage},
};

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum Frame {
    Metadata {
        market: Market,
        data: ExchangeInfo,
    },
    Stream {
        market: Market,
        received: u64,
        data: StreamMessage,
    },
    Snapshot {
        market: Market,
        symbol: String,
        received: u64,
        data: Snapshot,
    },
    Interest {
        received: u64,
        data: Interest,
    },
    Failure {
        status: Option<u16>,
        retry_after: Option<u64>,
    },
}

pub(super) struct Transport {
    child: Child,
    output: BufReader<ChildStdout>,
    partial: Vec<u8>,
}

impl Transport {
    pub(super) fn start(config: &Config) -> Result<Self> {
        let mut command =
            Command::new(env::var_os("FINANCE_PYTHON").unwrap_or_else(|| "python3".into()));
        command.args(["-m", "fertile_finance.binance_transport"]);
        if config.spot.is_some() {
            command.arg("--spot");
        }
        if let Some(perpetual) = &config.perpetual {
            command.arg("--perpetual").args(&perpetual.symbols);
        }
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("start Binance public transport")?;
        let output = BufReader::new(child.stdout.take().context("missing transport pipe")?);
        Ok(Self {
            child,
            output,
            partial: Vec::new(),
        })
    }

    pub(super) async fn next(&mut self) -> Result<Frame> {
        use tokio::io::AsyncReadExt;
        const MAX_FRAME: u64 = 4 * 1024 * 1024;
        // read_until retains bytes in the caller's buffer when select! cancels it.
        let remaining = MAX_FRAME - self.partial.len() as u64;
        (&mut self.output)
            .take(remaining)
            .read_until(b'\n', &mut self.partial)
            .await?;
        let bytes = std::mem::take(&mut self.partial);
        ensure!(
            bytes.ends_with(b"\n"),
            "Binance transport closed or frame too large"
        );
        // Never include upstream payloads in diagnostics.
        serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("invalid Binance transport frame"))
    }

    pub(super) async fn close(&mut self) -> Result<()> {
        self.child
            .kill()
            .await
            .context("stop Binance public transport")
    }
}
