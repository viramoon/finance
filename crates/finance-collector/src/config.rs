use anyhow::{Result, ensure};
use serde::Deserialize;
use std::{
    collections::BTreeSet,
    env, fs,
    net::SocketAddr,
    path::{Path, PathBuf},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub bind: SocketAddr,
    pub source: String,
    pub instruments: Vec<String>,
    pub bar_spec: String,
    pub max_age_seconds: u32,
    pub flush_ms: u32,
    pub shutdown_seconds: u32,
    pub history_chunk_seconds: u32,
    pub catalog: PathBuf,
    pub replay: ReplaySettings,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplaySettings {
    pub fixture: PathBuf,
    pub interval_ms: u32,
    pub limit: Option<u32>,
}
impl Settings {
    pub fn load() -> Result<Self> {
        let profile = env::var("FERTILE_PROFILE").unwrap_or_else(|_| "cloudbox".into());
        ensure!(
            matches!(profile.as_str(), "cloudbox" | "stable"),
            "unsupported profile"
        );
        let dir = env::var_os("FINANCE_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| "config".into());
        let path = env::var_os("FINANCE_COLLECTOR_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| dir.join("collector").join(format!("{profile}.yaml")));
        let mut cfg: Self = serde_yaml::from_str(&fs::read_to_string(path)?)?;
        if let Ok(bind) = env::var("FINANCE_COLLECTOR_BIND") {
            cfg.bind = bind.parse()?;
        }
        ensure!(
            (1..=60_000).contains(&cfg.flush_ms)
                && cfg.max_age_seconds > 0
                && cfg.shutdown_seconds > 0,
            "invalid collector timing"
        );
        ensure!(
            (1..=86400).contains(&cfg.history_chunk_seconds),
            "invalid backfill chunk"
        );
        ensure!(
            cfg.replay.interval_ms >= 100 && !cfg.instruments.is_empty(),
            "invalid replay or instruments"
        );
        ensure!(
            cfg.instruments.iter().all(|s| s.contains('.')
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')))
                && cfg.instruments.iter().collect::<BTreeSet<_>>().len() == cfg.instruments.len(),
            "invalid/duplicate instrument ID"
        );
        ensure!(
            !cfg.bar_spec.is_empty()
                && cfg
                    .bar_spec
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-'),
            "invalid bar spec"
        );
        Ok(cfg)
    }
    pub fn catalog(&self, data: &Path) -> PathBuf {
        data.join(&self.catalog)
    }
}
