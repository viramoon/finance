//! Read-only Binance public feed and F3 producer.
mod book;
mod transport;
mod wire;

use std::{collections::BTreeSet, path::Path};

use anyhow::{Result, ensure};
use serde::Deserialize;

use crate::market_ingress::ProductPolicy;
use wire::{CONTEXT_PRODUCT, PERP_PRODUCT, SOURCE, SPOT_PRODUCT, SYMBOLS};

mod runner;
#[cfg(test)]
mod tests;
pub use runner::{Observer, Status, run};

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    enabled: bool,
    spot: Option<Access>,
    perpetual: Option<Perpetual>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Perpetual {
    symbols: Vec<String>,
    market: Access,
    context: Access,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Access {
    license: finance_market_protocol::Token,
    display_live: bool,
    record_local: bool,
    derive_non_display: bool,
    backup_personal_drive: bool,
}

impl Access {
    fn policy(&self, product: &str) -> Result<ProductPolicy> {
        ensure!(
            self.display_live || self.record_local,
            "product must permit display or recording"
        );
        ensure!(
            !self.backup_personal_drive || self.record_local,
            "backup requires local recording"
        );
        Ok(ProductPolicy {
            source: SOURCE.parse()?,
            product: product.parse()?,
            license: self.license.clone(),
            display_live: self.display_live,
            record_local: self.record_local,
            derive_non_display: self.derive_non_display,
            backup_personal_drive: self.backup_personal_drive,
        })
    }
}

impl Config {
    /// A missing product profile is disabled. All permission flags are explicit on enabled products.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(serde_yaml::from_str(&text)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error.into()),
        }
    }

    fn policies(&self) -> Result<Vec<ProductPolicy>> {
        ensure!(
            self.spot.is_some() || self.perpetual.is_some(),
            "enabled Binance profile needs at least one product"
        );
        let mut policies = Vec::new();
        if let Some(access) = &self.spot {
            policies.push(access.policy(SPOT_PRODUCT)?);
        }
        if let Some(perp) = &self.perpetual {
            let selected: BTreeSet<_> = perp.symbols.iter().map(String::as_str).collect();
            ensure!(
                !selected.is_empty()
                    && selected.len() == perp.symbols.len()
                    && selected.iter().all(|symbol| SYMBOLS.contains(symbol)),
                "perpetual symbols must be a unique nonempty subset of BTCUSDT, ETHUSDT, SOLUSDT"
            );
            policies.push(perp.market.policy(PERP_PRODUCT)?);
            policies.push(perp.context.policy(CONTEXT_PRODUCT)?);
        }
        Ok(policies)
    }
}
