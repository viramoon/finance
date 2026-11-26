//! In-memory USD option-chain analytics, independent of feeds and storage.

mod decimal;
mod flip;
mod projection;

use std::{collections::HashSet, error::Error, fmt};

use chrono::{DateTime, NaiveDate, TimeDelta, Utc};
use finance_market_protocol::{DecimalText, InstrumentId};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use decimal::{add_exact, contract_exposure};
use flip::PreparedGammaGrid;
use projection::{ContractExposure, project};

pub use flip::{
    ContractGammaGrid, ContractVolatility, FlipCrossing, FlipUnavailableReason, GammaFlip,
    GammaGrid, GammaGridProvenance,
};
pub use projection::{
    Coverage, Exclusions, ExpiryExposure, ExposureTotals, ExposureView, GammaWalls,
    ObservationRange, StrikeExposure, Wall,
};

pub const MODEL_VERSION: &str = "gex-v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OptionRight {
    Call,
    Put,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DealerScenario {
    Absolute,
    DealerShort,
    DealerLong,
    CallLongPutShort,
}

impl DealerScenario {
    fn weight(self, right: OptionRight) -> Decimal {
        match (self, right) {
            (Self::DealerShort, _) | (Self::CallLongPutShort, OptionRight::Put) => {
                Decimal::NEGATIVE_ONE
            }
            _ => Decimal::ONE,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecimalObservation {
    pub value: DecimalText,
    pub observed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenInterestObservation {
    pub value: DecimalText,
    pub observed_at: DateTime<Utc>,
    /// Business date provided by the source, when confirmed.
    pub business_date: Option<NaiveDate>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OptionContractSnapshot {
    pub instrument_id: InstrumentId,
    pub right: OptionRight,
    pub expiry: NaiveDate,
    pub strike: DecimalText,
    pub multiplier: DecimalText,
    pub open_interest: Option<OpenInterestObservation>,
    /// Unit gamma for a long option, using the same convention across the chain.
    pub gamma: Option<DecimalObservation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OptionChainSnapshot {
    pub underlying_id: InstrumentId,
    pub as_of: DateTime<Utc>,
    /// The valuation session from the product's exchange calendar.
    pub session_date: NaiveDate,
    pub spot: DecimalObservation,
    /// True when the roster includes every candidate, including unobserved contracts.
    pub universe_complete: bool,
    pub contracts: Vec<OptionContractSnapshot>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimingPolicy {
    pub max_spot_age_seconds: u32,
    pub max_gamma_age_seconds: u32,
    pub max_spot_gamma_skew_seconds: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExposureReport {
    pub scenario: DealerScenario,
    pub model_version: &'static str,
    pub underlying_id: InstrumentId,
    pub as_of: DateTime<Utc>,
    pub session_date: NaiveDate,
    pub spot: DecimalObservation,
    pub timing: TimingPolicy,
    pub all: ExposureView,
    pub zero_dte: ExposureView,
    pub later: ExposureView,
    pub gamma_grid: Option<GammaGridProvenance>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExposureError {
    InvalidSpot(&'static str),
    InvalidContract {
        instrument_id: InstrumentId,
        reason: &'static str,
    },
    InvalidGrid(&'static str),
    Arithmetic(&'static str),
}

impl fmt::Display for ExposureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSpot(reason) => write!(f, "invalid spot: {reason}"),
            Self::InvalidContract {
                instrument_id,
                reason,
            } => write!(
                f,
                "invalid option contract {}: {reason}",
                instrument_id.as_str()
            ),
            Self::InvalidGrid(reason) => write!(f, "invalid gamma grid: {reason}"),
            Self::Arithmetic(reason) => write!(f, "exposure arithmetic: {reason}"),
        }
    }
}

impl Error for ExposureError {}

/// Evaluate a chain under one positioning assumption and a caller-chosen quote timing policy.
pub fn evaluate(
    snapshot: &OptionChainSnapshot,
    scenario: DealerScenario,
    timing: TimingPolicy,
    gamma_grid: Option<&GammaGrid>,
) -> Result<ExposureReport, ExposureError> {
    let spot = snapshot.spot.value.value();
    if spot <= Decimal::ZERO {
        return Err(ExposureError::InvalidSpot("price must be positive"));
    }
    if snapshot.spot.observed_at > snapshot.as_of {
        return Err(ExposureError::InvalidSpot("observation is after valuation"));
    }
    let spot_stale = snapshot.as_of - snapshot.spot.observed_at
        > TimeDelta::seconds(i64::from(timing.max_spot_age_seconds));

    let mut ids = HashSet::new();
    let mut contracts = Vec::with_capacity(snapshot.contracts.len());
    for contract in &snapshot.contracts {
        validate_contract(contract, snapshot)?;
        if !ids.insert(&contract.instrument_id) {
            return Err(invalid_contract(contract, "duplicate instrument ID"));
        }
        contracts.push(contract);
    }
    contracts.sort_unstable_by_key(|contract| contract.instrument_id.as_str());

    let grid = gamma_grid
        .map(|grid| PreparedGammaGrid::new(snapshot, grid))
        .transpose()?;
    let rows = contracts
        .into_iter()
        .map(|contract| {
            let exclusions = exclusions(contract, snapshot, timing, spot_stale);
            let absolute = match (&contract.open_interest, &contract.gamma) {
                (Some(oi), Some(gamma)) if exclusions == Exclusions::default() => {
                    Some(contract_exposure(
                        oi.value.value(),
                        contract.multiplier.value(),
                        gamma.value.value(),
                        spot,
                    )?)
                }
                _ => None,
            };
            Ok(ContractExposure {
                contract,
                absolute,
                exclusions,
            })
        })
        .collect::<Result<Vec<_>, ExposureError>>()?;

    let is_zero_dte = |row: &&ContractExposure<'_>| row.contract.expiry == snapshot.session_date;
    Ok(ExposureReport {
        scenario,
        model_version: MODEL_VERSION,
        underlying_id: snapshot.underlying_id.clone(),
        as_of: snapshot.as_of,
        session_date: snapshot.session_date,
        spot: snapshot.spot.clone(),
        timing,
        all: project(rows.iter(), snapshot, scenario, grid.as_ref())?,
        zero_dte: project(
            rows.iter().filter(is_zero_dte),
            snapshot,
            scenario,
            grid.as_ref(),
        )?,
        later: project(
            rows.iter().filter(|row| !is_zero_dte(row)),
            snapshot,
            scenario,
            grid.as_ref(),
        )?,
        gamma_grid: grid.as_ref().map(PreparedGammaGrid::provenance),
    })
}

fn invalid_contract(contract: &OptionContractSnapshot, reason: &'static str) -> ExposureError {
    ExposureError::InvalidContract {
        instrument_id: contract.instrument_id.clone(),
        reason,
    }
}

fn validate_contract(
    contract: &OptionContractSnapshot,
    snapshot: &OptionChainSnapshot,
) -> Result<(), ExposureError> {
    if contract.expiry < snapshot.session_date {
        return Err(invalid_contract(
            contract,
            "expiry precedes valuation session",
        ));
    }
    if contract.strike.value() <= Decimal::ZERO || contract.multiplier.value() <= Decimal::ZERO {
        return Err(invalid_contract(
            contract,
            "strike and multiplier must be positive",
        ));
    }
    if let Some(oi) = &contract.open_interest {
        let value = oi.value.value();
        if value < Decimal::ZERO || !value.fract().is_zero() {
            return Err(invalid_contract(
                contract,
                "OI must be a nonnegative whole contract count",
            ));
        }
        if oi.observed_at > snapshot.as_of {
            return Err(invalid_contract(
                contract,
                "OI observation is after valuation",
            ));
        }
        if oi
            .business_date
            .is_some_and(|date| date > snapshot.session_date)
        {
            return Err(invalid_contract(
                contract,
                "OI business date is after valuation session",
            ));
        }
    }
    if let Some(gamma) = &contract.gamma {
        if gamma.value.value() < Decimal::ZERO {
            return Err(invalid_contract(
                contract,
                "long-option unit gamma must be nonnegative",
            ));
        }
        if gamma.observed_at > snapshot.as_of {
            return Err(invalid_contract(
                contract,
                "gamma observation is after valuation",
            ));
        }
    }
    Ok(())
}

fn exclusions(
    contract: &OptionContractSnapshot,
    snapshot: &OptionChainSnapshot,
    timing: TimingPolicy,
    spot_stale: bool,
) -> Exclusions {
    let gamma = contract.gamma.as_ref();
    Exclusions {
        missing_open_interest: usize::from(contract.open_interest.is_none()),
        missing_gamma: usize::from(gamma.is_none()),
        stale_spot: usize::from(spot_stale),
        stale_gamma: usize::from(gamma.is_some_and(|observation| {
            snapshot.as_of - observation.observed_at
                > TimeDelta::seconds(i64::from(timing.max_gamma_age_seconds))
        })),
        spot_gamma_skew: usize::from(gamma.is_some_and(|observation| {
            (snapshot.spot.observed_at - observation.observed_at).abs()
                > TimeDelta::seconds(i64::from(timing.max_spot_gamma_skew_seconds))
        })),
    }
}
