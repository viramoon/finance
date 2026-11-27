//! Offline budget model for Databento OPRA historical data.

use std::{
    collections::HashMap,
    num::{NonZeroU32, NonZeroU64},
};

use anyhow::{Context, Result, ensure};
use chrono::NaiveDate;
use finance_market_protocol::{InstrumentId, NonEmptyText, Token};
use rust_decimal::Decimal;
use serde::Deserialize;

pub const DATASET: &str = "OPRA.PILLAR";

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SchemaBudget {
    pub bytes_per_contract_day: NonZeroU64,
    pub usd_micros_per_gb: NonZeroU32,
}

/// Caller-supplied planning assumptions from a dated price source.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EstimateConfig {
    pub price_source: NonEmptyText,
    pub price_as_of: NaiveDate,
    pub schemas: HashMap<Token, SchemaBudget>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Status {
    Disabled,
    EstimateOnly,
}

#[derive(Debug)]
pub struct EstimateRequest {
    /// Expected contract counts per underlying, supplied by the caller.
    pub contracts_by_underlying: HashMap<InstrumentId, NonZeroU32>,
    pub start_date: NaiveDate,
    pub end_date_exclusive: NaiveDate,
    pub schema: Token,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Estimate {
    pub dataset: &'static str,
    pub schema: Token,
    pub calendar_days: u64,
    pub contract_count: u64,
    pub estimated_bytes: u64,
    pub estimated_cost_usd: Decimal,
    pub budget: SchemaBudget,
    pub price_source: NonEmptyText,
    pub price_as_of: NaiveDate,
}

/// This adapter contains only estimation configuration. There is no transport.
#[derive(Debug, Default)]
pub struct DatabentoOpra {
    config: Option<EstimateConfig>,
}

impl DatabentoOpra {
    pub fn new(config: Option<EstimateConfig>) -> Result<Self> {
        if let Some(config) = &config {
            ensure!(
                !config.price_source.as_str().trim().is_empty(),
                "a Databento price source is required"
            );
            ensure!(!config.schemas.is_empty(), "no Databento schema budgets");
        }
        Ok(Self { config })
    }

    pub fn status(&self) -> Status {
        if self.config.is_some() {
            Status::EstimateOnly
        } else {
            Status::Disabled
        }
    }

    pub fn estimate(&self, request: &EstimateRequest) -> Result<Estimate> {
        let config = self.config.as_ref().context("Databento OPRA is disabled")?;
        let calendar_days = (request.end_date_exclusive - request.start_date).num_days();
        ensure!(calendar_days > 0, "empty or reversed estimate date range");
        let calendar_days = u64::try_from(calendar_days)?;
        ensure!(
            !request.contracts_by_underlying.is_empty(),
            "at least one underlying is required"
        );
        let budget = *config
            .schemas
            .get(&request.schema)
            .context("no budget configured for the requested OPRA schema")?;
        let contract_count = request
            .contracts_by_underlying
            .values()
            .try_fold(0u64, |count, next| count.checked_add(u64::from(next.get())))
            .context("estimated contract count overflows")?;
        let estimated_bytes = contract_count
            .checked_mul(calendar_days)
            .and_then(|contract_days| {
                contract_days.checked_mul(budget.bytes_per_contract_day.get())
            })
            .context("estimated byte count overflows")?;

        // Decimal GB and microdollars give a scale of 9 + 6. The product of
        // a u64 and a u32 fits exactly in Decimal's 96-bit mantissa.
        let mantissa = i128::from(estimated_bytes) * i128::from(budget.usd_micros_per_gb.get());
        Ok(Estimate {
            dataset: DATASET,
            schema: request.schema.clone(),
            calendar_days,
            contract_count,
            estimated_bytes,
            estimated_cost_usd: Decimal::from_i128_with_scale(mantissa, 15),
            budget,
            price_source: config.price_source.clone(),
            price_as_of: config.price_as_of,
        })
    }
}
