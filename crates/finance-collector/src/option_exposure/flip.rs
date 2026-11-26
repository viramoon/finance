use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet, hash_map::Entry},
};

use chrono::{DateTime, Utc};
use finance_market_protocol::{DecimalText, InstrumentId, NonEmptyText};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use super::{
    DealerScenario, ExposureError, OptionChainSnapshot, OptionContractSnapshot, add_exact,
    contract_exposure,
};

/// Revalued unit gamma under a named model at a common valuation instant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GammaGrid {
    pub as_of: DateTime<Utc>,
    pub model: NonEmptyText,
    pub annual_risk_free_rate: DecimalText,
    pub annual_dividend_yield: DecimalText,
    pub spots: Vec<DecimalText>,
    pub contracts: Vec<ContractGammaGrid>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContractGammaGrid {
    pub instrument_id: InstrumentId,
    /// Annualized IV held fixed for this contract over the spot grid.
    pub implied_volatility: DecimalText,
    /// Unsigned unit gamma recomputed at each of the common hypothetical spots.
    pub gammas: Vec<DecimalText>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GammaGridProvenance {
    pub as_of: DateTime<Utc>,
    pub model: NonEmptyText,
    pub annual_risk_free_rate: DecimalText,
    pub annual_dividend_yield: DecimalText,
    pub spots: Vec<DecimalText>,
    pub implied_volatilities: Vec<ContractVolatility>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContractVolatility {
    pub instrument_id: InstrumentId,
    pub implied_volatility: DecimalText,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum GammaFlip {
    Unavailable {
        reason: FlipUnavailableReason,
    },
    /// No reversal was detected within the sampled spot range for the eligible subset.
    NoFlip,
    Crossings {
        crossings: Vec<FlipCrossing>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FlipUnavailableReason {
    NotDirectionalScenario,
    NoGammaGrid,
    NoEligibleContracts,
    MissingContractGamma,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FlipCrossing {
    /// Adjacent opposite-sign samples, or the extent of a sampled zero run between opposite signs.
    pub lower_spot: DecimalText,
    pub upper_spot: DecimalText,
}

pub(super) struct PreparedGammaGrid<'a> {
    grid: &'a GammaGrid,
    by_id: HashMap<&'a InstrumentId, &'a ContractGammaGrid>,
}

impl<'a> PreparedGammaGrid<'a> {
    pub(super) fn new(
        snapshot: &OptionChainSnapshot,
        grid: &'a GammaGrid,
    ) -> Result<Self, ExposureError> {
        if grid.as_of != snapshot.as_of {
            return Err(ExposureError::InvalidGrid(
                "valuation time must match the option chain",
            ));
        }
        if grid.model.as_str().trim().is_empty() {
            return Err(ExposureError::InvalidGrid("pricing model is blank"));
        }
        if grid.spots.len() < 2
            || grid.spots[0].value() <= Decimal::ZERO
            || grid
                .spots
                .windows(2)
                .any(|pair| pair[0].value() >= pair[1].value())
        {
            return Err(ExposureError::InvalidGrid(
                "at least two positive, strictly increasing spot prices are required",
            ));
        }

        let chain_ids: HashSet<_> = snapshot
            .contracts
            .iter()
            .map(|contract| &contract.instrument_id)
            .collect();
        let mut by_id = HashMap::with_capacity(grid.contracts.len());
        for contract in &grid.contracts {
            if !chain_ids.contains(&contract.instrument_id) {
                return Err(ExposureError::InvalidGrid(
                    "contract is outside the option chain",
                ));
            }
            let Entry::Vacant(entry) = by_id.entry(&contract.instrument_id) else {
                return Err(ExposureError::InvalidGrid("duplicate contract"));
            };
            if contract.implied_volatility.value() <= Decimal::ZERO {
                return Err(ExposureError::InvalidGrid("IV must be positive"));
            }
            if contract.gammas.len() != grid.spots.len()
                || contract
                    .gammas
                    .iter()
                    .any(|gamma| gamma.value() < Decimal::ZERO)
            {
                return Err(ExposureError::InvalidGrid(
                    "each contract must have one nonnegative gamma per spot price",
                ));
            }
            entry.insert(contract);
        }
        Ok(Self { grid, by_id })
    }

    pub(super) fn provenance(&self) -> GammaGridProvenance {
        let mut implied_volatilities: Vec<_> = self
            .grid
            .contracts
            .iter()
            .map(|contract| ContractVolatility {
                instrument_id: contract.instrument_id.clone(),
                implied_volatility: contract.implied_volatility.clone(),
            })
            .collect();
        implied_volatilities.sort_by(|a, b| a.instrument_id.as_str().cmp(b.instrument_id.as_str()));
        GammaGridProvenance {
            as_of: self.grid.as_of,
            model: self.grid.model.clone(),
            annual_risk_free_rate: self.grid.annual_risk_free_rate.clone(),
            annual_dividend_yield: self.grid.annual_dividend_yield.clone(),
            spots: self.grid.spots.clone(),
            implied_volatilities,
        }
    }

    pub(super) fn evaluate(
        &self,
        scenario: DealerScenario,
        eligible: &[&OptionContractSnapshot],
    ) -> Result<GammaFlip, ExposureError> {
        if eligible.is_empty() {
            return Ok(GammaFlip::Unavailable {
                reason: FlipUnavailableReason::NoEligibleContracts,
            });
        }
        if eligible
            .iter()
            .any(|contract| !self.by_id.contains_key(&contract.instrument_id))
        {
            return Ok(GammaFlip::Unavailable {
                reason: FlipUnavailableReason::MissingContractGamma,
            });
        }

        let mut exposures = vec![Decimal::ZERO; self.grid.spots.len()];
        for &contract in eligible {
            let oi = contract
                .open_interest
                .as_ref()
                .expect("eligible contracts have observed OI")
                .value
                .value();
            let multiplier = contract.multiplier.value();
            let weight = scenario.weight(contract.right);
            let gammas = &self.by_id[&contract.instrument_id].gammas;
            for ((total, spot), gamma) in exposures.iter_mut().zip(&self.grid.spots).zip(gammas) {
                let exposure = contract_exposure(oi, multiplier, gamma.value(), spot.value())?;
                *total = add_exact(*total, exposure * weight)?;
            }
        }
        let crossings = find_crossings(&exposures, &self.grid.spots);
        Ok(if crossings.is_empty() {
            GammaFlip::NoFlip
        } else {
            GammaFlip::Crossings { crossings }
        })
    }
}

fn find_crossings(exposures: &[Decimal], spots: &[DecimalText]) -> Vec<FlipCrossing> {
    let mut crossings = Vec::new();
    let mut previous: Option<(usize, Ordering)> = None;
    for (index, exposure) in exposures.iter().enumerate() {
        let sign = exposure.cmp(&Decimal::ZERO);
        if sign == Ordering::Equal {
            continue;
        }
        if let Some((last_index, last_sign)) = previous
            && sign != last_sign
        {
            let (lower, upper) = if last_index + 1 == index {
                (last_index, index)
            } else {
                (last_index + 1, index - 1)
            };
            crossings.push(FlipCrossing {
                lower_spot: spots[lower].clone(),
                upper_spot: spots[upper].clone(),
            });
        }
        previous = Some((index, sign));
    }
    crossings
}
