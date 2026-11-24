use crate::{Error, Result};
use serde::Deserialize;
use serde_yaml::Value;
use std::{env, fs, path::Path, time::Duration};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IbConfig {
    pub address: String,
    pub collector_client_id: i32,
    pub probe_client_id: i32,
    pub trader_client_id: i32,
    pub account_prefix: String,
    pub connect_timeout_ms: u64,
    pub heartbeat_ms: u64,
    pub read_timeout_ms: u64,
    pub backoff_initial_ms: u64,
    pub backoff_max_ms: u64,
    pub jitter_ms: u64,
}

impl IbConfig {
    pub fn load() -> Result<Self> {
        let dir = env::var_os("FINANCE_CONFIG_DIR").unwrap_or_else(|| "config".into());
        let profile = env::var("FERTILE_PROFILE").unwrap_or_else(|_| "cloudbox".into());
        Self::from_layers(Path::new(&dir), &profile, |name| env::var(name).ok())
    }

    pub fn from_layers(
        dir: &Path,
        profile: &str,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Self> {
        if !matches!(profile, "cloudbox" | "stable") {
            return Err(Error::Config("invalid FERTILE_PROFILE".into()));
        }
        let section = |name: &str| -> Result<serde_yaml::Mapping> {
            let bytes = fs::read(dir.join(format!("{name}.yaml")))
                .map_err(|e| Error::Config(e.to_string()))?;
            let doc: Value =
                serde_yaml::from_slice(&bytes).map_err(|e| Error::Config(e.to_string()))?;
            match doc.get("ib") {
                None => Ok(Default::default()),
                Some(Value::Mapping(values)) => Ok(values.clone()),
                _ => Err(Error::Config("ib must be a mapping".into())),
            }
        };
        let mut values = section("base")?;
        values.extend(section(profile)?);
        for (key, name) in [
            ("address", "IB_GATEWAY_ADDRESS"),
            ("account_prefix", "IB_PAPER_ACCOUNT_PREFIX"),
            ("collector_client_id", "IB_COLLECTOR_CLIENT_ID"),
            ("probe_client_id", "IB_PROBE_CLIENT_ID"),
            ("trader_client_id", "IB_TRADER_CLIENT_ID"),
            ("connect_timeout_ms", "IB_CONNECT_TIMEOUT_MS"),
            ("read_timeout_ms", "IB_READ_TIMEOUT_MS"),
            ("heartbeat_ms", "IB_HEARTBEAT_MS"),
            ("backoff_initial_ms", "IB_BACKOFF_INITIAL_MS"),
            ("backoff_max_ms", "IB_BACKOFF_MAX_MS"),
            ("jitter_ms", "IB_JITTER_MS"),
        ] {
            if let Some(raw) = env(name) {
                let value = if matches!(key, "address" | "account_prefix") {
                    Value::String(raw)
                } else {
                    Value::Number(
                        raw.parse::<u64>()
                            .map_err(|_| Error::Config(format!("invalid {name}")))?
                            .into(),
                    )
                };
                values.insert(Value::String(key.into()), value);
            }
        }
        let cfg: Self = serde_yaml::from_value(Value::Mapping(values))
            .map_err(|e| Error::Config(e.to_string()))?;
        cfg.validate()?;
        Ok(cfg)
    }
    pub fn validate(&self) -> Result<()> {
        let (host, port) = self
            .address
            .rsplit_once(':')
            .ok_or_else(|| Error::Config("expected host:paper_port".into()))?;
        let port: u16 = port
            .parse()
            .map_err(|_| Error::Config("invalid IB port".into()))?;
        if host.is_empty() || port == 0 || matches!(port, 4001 | 7496) {
            return Err(Error::Config("live or invalid IB port rejected".into()));
        }
        if !self.account_prefix.starts_with("DU")
            || !self
                .account_prefix
                .bytes()
                .all(|b| b.is_ascii_alphanumeric())
        {
            return Err(Error::Config(
                "paper account prefix must start with DU".into(),
            ));
        }
        let ids = [
            self.collector_client_id,
            self.probe_client_id,
            self.trader_client_id,
        ];
        if ids.iter().any(|id| *id <= 0) || ids[0] == ids[1] || ids[1] == ids[2] || ids[0] == ids[2]
        {
            return Err(Error::Config(
                "IB client IDs must be positive and distinct".into(),
            ));
        }
        if [
            self.connect_timeout_ms,
            self.heartbeat_ms,
            self.read_timeout_ms,
            self.backoff_initial_ms,
            self.backoff_max_ms,
        ]
        .iter()
        .any(|v| *v == 0 || *v > 86_400_000)
            || self.backoff_initial_ms > self.backoff_max_ms
            || self.jitter_ms > self.backoff_max_ms
        {
            return Err(Error::Config("invalid IB timers".into()));
        }
        Ok(())
    }
    pub fn backoff(&self, attempts: u32, jitter: u64) -> Duration {
        let ms = self
            .backoff_initial_ms
            .saturating_mul(1u64 << attempts.min(63))
            .saturating_add(jitter.min(self.jitter_ms))
            .min(self.backoff_max_ms);
        Duration::from_millis(ms)
    }
}
