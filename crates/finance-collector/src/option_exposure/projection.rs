use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

use chrono::{DateTime, NaiveDate, Utc};
use finance_market_protocol::DecimalText;
use rust_decimal::Decimal;
use serde::Serialize;

use super::{
    DealerScenario, ExposureError, FlipUnavailableReason, GammaFlip, OptionChainSnapshot,
    OptionContractSnapshot,
    decimal::{add_exact, decimal_text},
    flip::PreparedGammaGrid,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExposureView {
    pub totals: ExposureTotals,
    pub by_strike: Vec<StrikeExposure>,
    pub by_expiry: Vec<ExpiryExposure>,
    pub walls: GammaWalls,
    pub coverage: Coverage,
    pub flip: GammaFlip,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExposureTotals {
    pub absolute: Option<DecimalText>,
    pub gex: Option<DecimalText>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StrikeExposure {
    pub strike: DecimalText,
    pub totals: ExposureTotals,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExpiryExposure {
    pub expiry: NaiveDate,
    pub totals: ExposureTotals,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Wall {
    pub strike: DecimalText,
    pub exposure: DecimalText,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct GammaWalls {
    pub largest_absolute: Option<Wall>,
    pub largest_positive: Option<Wall>,
    pub most_negative: Option<Wall>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Coverage {
    pub universe_complete: bool,
    pub candidate_contracts: usize,
    pub valid_contracts: usize,
    pub partial: bool,
    pub eligible_open_interest: DecimalText,
    pub total_open_interest: Option<DecimalText>,
    pub exclusions: Exclusions,
    pub open_interest_dates: Vec<NaiveDate>,
    pub unconfirmed_open_interest_dates: usize,
    pub observed: ObservationRange,
    pub open_interest_observed: Option<ObservationRange>,
    pub gamma_observed: Option<ObservationRange>,
    pub max_oi_gamma_skew: Option<Duration>,
}

/// Independent counts. A contract may contribute to several exclusion reasons.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Exclusions {
    pub missing_open_interest: usize,
    pub missing_gamma: usize,
    pub stale_spot: usize,
    pub stale_gamma: usize,
    pub spot_gamma_skew: usize,
}

impl Exclusions {
    fn include(&mut self, other: Self) {
        self.missing_open_interest += other.missing_open_interest;
        self.missing_gamma += other.missing_gamma;
        self.stale_spot += other.stale_spot;
        self.stale_gamma += other.stale_gamma;
        self.spot_gamma_skew += other.spot_gamma_skew;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ObservationRange {
    pub earliest: DateTime<Utc>,
    pub latest: DateTime<Utc>,
}

impl ObservationRange {
    fn at(observation: DateTime<Utc>) -> Self {
        Self {
            earliest: observation,
            latest: observation,
        }
    }

    fn include(&mut self, observation: DateTime<Utc>) {
        self.earliest = self.earliest.min(observation);
        self.latest = self.latest.max(observation);
    }
}

pub(super) struct ContractExposure<'a> {
    pub(super) contract: &'a OptionContractSnapshot,
    pub(super) absolute: Option<Decimal>,
    pub(super) exclusions: Exclusions,
}

pub(super) fn project<'a>(
    rows: impl Iterator<Item = &'a ContractExposure<'a>> + Clone,
    snapshot: &OptionChainSnapshot,
    scenario: DealerScenario,
    grid: Option<&PreparedGammaGrid<'_>>,
) -> Result<ExposureView, ExposureError> {
    let oi_complete =
        snapshot.universe_complete && rows.clone().all(|row| row.contract.open_interest.is_some());
    let mut coverage = CoverageAccumulator::new(snapshot.spot.observed_at, oi_complete);
    let mut total = Totals::default();
    let mut strikes = BTreeMap::<_, Totals>::new();
    let mut expiries = BTreeMap::<_, Totals>::new();
    let mut eligible = Vec::new();
    for row in rows {
        coverage.include(row)?;
        let strike = strikes.entry(row.contract.strike.value()).or_default();
        let expiry = expiries.entry(row.contract.expiry).or_default();
        if let Some(absolute) = row.absolute {
            let gex = absolute * scenario.weight(row.contract.right);
            total.add(absolute, gex)?;
            strike.add(absolute, gex)?;
            expiry.add(absolute, gex)?;
            eligible.push(row.contract);
        }
    }
    let by_strike = strikes
        .into_iter()
        .map(|(strike, totals)| {
            Ok(StrikeExposure {
                strike: decimal_text(strike)?,
                totals: totals.finish(scenario)?,
            })
        })
        .collect::<Result<Vec<_>, ExposureError>>()?;
    let by_expiry = expiries
        .into_iter()
        .map(|(expiry, totals)| {
            Ok(ExpiryExposure {
                expiry,
                totals: totals.finish(scenario)?,
            })
        })
        .collect::<Result<Vec<_>, ExposureError>>()?;
    let flip = match (scenario, grid) {
        (DealerScenario::Absolute, _) => GammaFlip::Unavailable {
            reason: FlipUnavailableReason::NotDirectionalScenario,
        },
        (_, None) => GammaFlip::Unavailable {
            reason: FlipUnavailableReason::NoGammaGrid,
        },
        (_, Some(grid)) => grid.evaluate(scenario, &eligible)?,
    };
    Ok(ExposureView {
        totals: total.finish(scenario)?,
        walls: walls(&by_strike),
        by_strike,
        by_expiry,
        coverage: coverage.finish(snapshot.universe_complete, eligible.len())?,
        flip,
    })
}

#[derive(Default)]
struct Totals {
    absolute: Option<Decimal>,
    gex: Option<Decimal>,
}

impl Totals {
    fn add(&mut self, absolute: Decimal, gex: Decimal) -> Result<(), ExposureError> {
        self.absolute = Some(add_exact(self.absolute.unwrap_or_default(), absolute)?);
        self.gex = Some(add_exact(self.gex.unwrap_or_default(), gex)?);
        Ok(())
    }

    fn finish(self, scenario: DealerScenario) -> Result<ExposureTotals, ExposureError> {
        let gex = match scenario {
            DealerScenario::Absolute => None,
            _ => self.gex.map(decimal_text).transpose()?,
        };
        Ok(ExposureTotals {
            absolute: self.absolute.map(decimal_text).transpose()?,
            gex,
        })
    }
}

fn walls(strikes: &[StrikeExposure]) -> GammaWalls {
    let mut walls = GammaWalls::default();
    for row in strikes {
        if let Some(absolute) = &row.totals.absolute {
            update_wall(&mut walls.largest_absolute, &row.strike, absolute, false);
        }
        if let Some(gex) = &row.totals.gex {
            if gex.value() > Decimal::ZERO {
                update_wall(&mut walls.largest_positive, &row.strike, gex, false);
            } else if gex.value() < Decimal::ZERO {
                update_wall(&mut walls.most_negative, &row.strike, gex, true);
            }
        }
    }
    walls
}

fn update_wall(
    wall: &mut Option<Wall>,
    strike: &DecimalText,
    exposure: &DecimalText,
    negative: bool,
) {
    let value = exposure.value();
    if !value.is_zero()
        && wall.as_ref().is_none_or(|wall| {
            if negative {
                value < wall.exposure.value()
            } else {
                value > wall.exposure.value()
            }
        })
    {
        *wall = Some(Wall {
            strike: strike.clone(),
            exposure: exposure.clone(),
        });
    }
}

struct CoverageAccumulator {
    candidate_contracts: usize,
    eligible_oi: Decimal,
    total_oi: Option<Decimal>,
    exclusions: Exclusions,
    dates: BTreeSet<NaiveDate>,
    unconfirmed_dates: usize,
    observed: ObservationRange,
    oi_observed: Option<ObservationRange>,
    gamma_observed: Option<ObservationRange>,
    max_oi_gamma_skew: Option<Duration>,
}

impl CoverageAccumulator {
    fn new(spot_observed: DateTime<Utc>, oi_complete: bool) -> Self {
        Self {
            candidate_contracts: 0,
            eligible_oi: Decimal::ZERO,
            total_oi: oi_complete.then_some(Decimal::ZERO),
            exclusions: Exclusions::default(),
            dates: BTreeSet::new(),
            unconfirmed_dates: 0,
            observed: ObservationRange::at(spot_observed),
            oi_observed: None,
            gamma_observed: None,
            max_oi_gamma_skew: None,
        }
    }

    fn include(&mut self, row: &ContractExposure<'_>) -> Result<(), ExposureError> {
        self.candidate_contracts += 1;
        self.exclusions.include(row.exclusions);
        if let Some(oi) = &row.contract.open_interest {
            if row.absolute.is_some() {
                self.eligible_oi = add_exact(self.eligible_oi, oi.value.value())?;
            }
            if let Some(total) = &mut self.total_oi {
                *total = add_exact(*total, oi.value.value())?;
            }
            if let Some(date) = oi.business_date {
                self.dates.insert(date);
            } else {
                self.unconfirmed_dates += 1;
            }
            self.observed.include(oi.observed_at);
            self.oi_observed
                .get_or_insert_with(|| ObservationRange::at(oi.observed_at))
                .include(oi.observed_at);
        }
        if let Some(gamma) = &row.contract.gamma {
            self.observed.include(gamma.observed_at);
            self.gamma_observed
                .get_or_insert_with(|| ObservationRange::at(gamma.observed_at))
                .include(gamma.observed_at);
            if let Some(oi) = &row.contract.open_interest {
                let skew = (gamma.observed_at - oi.observed_at).abs();
                // The absolute difference of two UTC instants is nonnegative.
                self.max_oi_gamma_skew = self.max_oi_gamma_skew.max(skew.to_std().ok());
            }
        }
        Ok(())
    }

    fn finish(
        self,
        universe_complete: bool,
        valid_contracts: usize,
    ) -> Result<Coverage, ExposureError> {
        Ok(Coverage {
            universe_complete,
            candidate_contracts: self.candidate_contracts,
            valid_contracts,
            partial: !universe_complete || valid_contracts != self.candidate_contracts,
            eligible_open_interest: decimal_text(self.eligible_oi)?,
            total_open_interest: self.total_oi.map(decimal_text).transpose()?,
            exclusions: self.exclusions,
            open_interest_dates: self.dates.into_iter().collect(),
            unconfirmed_open_interest_dates: self.unconfirmed_dates,
            observed: self.observed,
            open_interest_observed: self.oi_observed,
            gamma_observed: self.gamma_observed,
            max_oi_gamma_skew: self.max_oi_gamma_skew,
        })
    }
}
