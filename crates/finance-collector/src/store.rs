use crate::source::{Bar, Watermarks};
use anyhow::{Context, Result, bail};
use std::{
    env,
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
};

pub struct Store {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    pub marks: Watermarks,
}
impl Store {
    pub async fn start(catalog: &Path, state: &Path, spec: &str, source: &str) -> Result<Self> {
        let python = env::var_os("FINANCE_PYTHON").unwrap_or_else(|| {
            let base = env::var_os("FINANCE_BUILD_ROOT")
                .unwrap_or_else(|| OsString::from("/home/box/fertile/cache/finance"));
            PathBuf::from(base).join("venv/bin/python").into_os_string()
        });
        let mut child = Command::new(python)
            .args(["-u", "python/fertile_finance/collector_store.py"])
            .arg(catalog)
            .arg(state)
            .arg(spec)
            .arg(source)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("start Nautilus catalog writer")?;
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        let mut writer = Self {
            child,
            input,
            output,
            marks: Watermarks::new(),
        };
        writer.ack().await?;
        Ok(writer)
    }
    async fn ack(&mut self) -> Result<()> {
        let mut response = String::new();
        if self.output.read_line(&mut response).await? == 0 {
            bail!("catalog writer closed its output");
        }
        self.marks = serde_json::from_str(&response).context("invalid writer acknowledgement")?;
        Ok(())
    }
    pub async fn flush(&mut self, bars: &mut Vec<Bar>) -> Result<()> {
        if bars.is_empty() {
            return Ok(());
        }
        let mut data = serde_json::to_vec(bars)?;
        data.push(b'\n');
        self.input.write_all(&data).await?;
        self.input.flush().await?;
        self.ack().await?;
        bars.clear();
        Ok(())
    }
    pub fn check_process(&mut self) -> Result<()> {
        if let Some(code) = self.child.try_wait()? {
            bail!("catalog writer failed ({code}); restart collector to reconcile durable files");
        }
        Ok(())
    }
    pub async fn close(mut self) -> Result<()> {
        drop(self.input);
        let code = self.child.wait().await?;
        anyhow::ensure!(code.success(), "writer exited: {code}");
        Ok(())
    }
}
