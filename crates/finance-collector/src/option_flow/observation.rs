//! Sampled, request-scoped OPRA L1 clues awaiting historical verification.

use std::collections::{HashMap, VecDeque};

use chrono::{DateTime, NaiveDate, TimeDelta, Utc};
use finance_ib::market::{
    ConfirmedContract, ContractKind, LiquidHours, MarketDataType, MarketEvent, MarketNotice,
    Product, QuoteField, RequestKey,
};
use finance_market_protocol::{DecimalText, U64Text};
use rust_decimal::Decimal;
use serde::{Serialize, Serializer};

use super::decimal::{exact_difference, exact_mean, exact_premium};
use crate::line_budget::{LineMessage, LineUpdate};

pub const OBSERVATION_RULE_VERSION: &str = "ibkr_l1_observation_v2";
const SEGMENT_MINUTES: i64 = 30;
const MAX_VOLUME_WINDOW: TimeDelta = TimeDelta::seconds(30);
const QUOTE_FRESHNESS: TimeDelta = TimeDelta::seconds(5);
const BASELINE_RETENTION: TimeDelta = TimeDelta::days(7);
const MIN_BASELINE_SAMPLES: usize = 3;
const MAX_BASELINE_SAMPLES: usize = 32;
const MAX_BASELINE_COHORTS: usize = 1024;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct BaselineCohort {
    pub underlying_con_id: i32,
    pub expiry: NaiveDate,
    /// Zero-based 30-minute segment since the start of this liquid session.
    pub session_segment: u16,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct L1Sample {
    pub value: DecimalText,
    pub received_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationStatus {
    PendingVerification,
}

/// Exact contracts-per-millisecond ratio for the typical rate.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct BaselineVolumeRate {
    #[serde(serialize_with = "serialize_rate_numerator")]
    pub contracts: u128,
    pub elapsed_millis: u32,
}

fn serialize_rate_numerator<S: Serializer>(
    contracts: &u128,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.collect_str(contracts)
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct VolumeComparison {
    pub elapsed_millis: u32,
    pub baseline_samples: usize,
    pub typical_rate: Option<BaselineVolumeRate>,
    pub elevated: Option<bool>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct VolumeIncrease {
    pub earlier_cumulative_volume: L1Sample,
    pub later_cumulative_volume: L1Sample,
    pub delta_contracts: DecimalText,
    pub comparison: VolumeComparison,
    pub sampled_last_price: Option<L1Sample>,
    pub sampled_last_size: Option<L1Sample>,
    /// Hypothetical amount using one sampled price for the entire volume change.
    pub estimated_premium: Option<DecimalText>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct QuoteWidening {
    pub bid: L1Sample,
    pub ask: L1Sample,
    pub bid_size: L1Sample,
    pub ask_size: L1Sample,
    pub spread: DecimalText,
    pub typical_spread: DecimalText,
    pub baseline_samples: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "evidence", rename_all = "snake_case")]
pub enum ObservationEvidence {
    VolumeIncrease(Box<VolumeIncrease>),
    QuoteWidening(Box<QuoteWidening>),
}

/// No trade ID, direction, or participant is implied by these local samples.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FlowObservation {
    pub rule_version: &'static str,
    pub status: ObservationStatus,
    pub con_id: i32,
    pub generation: U64Text,
    pub cohort: BaselineCohort,
    pub multiplier: DecimalText,
    pub observed_at: DateTime<Utc>,
    pub observation: ObservationEvidence,
}

impl FlowObservation {
    /// Endpoints of locally sampled evidence. These are receipt times.
    pub fn window(&self) -> (DateTime<Utc>, DateTime<Utc>) {
        let start = match &self.observation {
            ObservationEvidence::VolumeIncrease(evidence) => {
                evidence.earlier_cumulative_volume.received_at
            }
            ObservationEvidence::QuoteWidening(evidence) => evidence
                .bid
                .received_at
                .min(evidence.ask.received_at)
                .min(evidence.bid_size.received_at)
                .min(evidence.ask_size.received_at),
        };
        (start, self.observed_at)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct ObservationCounts {
    pub observations: u64,
    pub volume_increases: u64,
    pub elevated_volume: u64,
    pub quote_widenings: u64,
    pub reader_lags: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ObservationSummary {
    pub as_of: DateTime<Utc>,
    pub last_observed_at: Option<DateTime<Utc>>,
    pub counts: ObservationCounts,
}

#[derive(PartialEq, Eq)]
struct Context {
    cohort: BaselineCohort,
    session_start: DateTime<Utc>,
    multiplier: DecimalText,
}

impl Context {
    fn at(contract: &ConfirmedContract, received_at: DateTime<Utc>) -> Option<Self> {
        let ContractKind::Option(option) = &contract.kind else {
            return None;
        };
        if option.underlying_con_id <= 0 || option.multiplier.value() <= 0.into() {
            return None;
        }
        let session_start = contract
            .liquid_hours
            .iter()
            .find_map(|interval| match *interval {
                LiquidHours::Open { start, end } if (start..end).contains(&received_at) => {
                    Some(start)
                }
                _ => None,
            })?;
        let segment = (received_at - session_start).num_minutes() / SEGMENT_MINUTES;
        Some(Self {
            cohort: BaselineCohort {
                underlying_con_id: option.underlying_con_id,
                expiry: option.expiry,
                session_segment: u16::try_from(segment).ok()?,
            },
            session_start,
            multiplier: option.multiplier.clone(),
        })
    }
}

#[derive(Default)]
struct QuoteSide {
    price: Option<L1Sample>,
    size: Option<L1Sample>,
}

struct BookSample {
    bid: L1Sample,
    ask: L1Sample,
    bid_size: L1Sample,
    ask_size: L1Sample,
    spread: DecimalText,
}

#[derive(Default)]
struct Samples {
    cumulative: Option<L1Sample>,
    last_price: Option<L1Sample>,
    last_size: Option<L1Sample>,
    bid: QuoteSide,
    ask: QuoteSide,
    last_book: Option<(DecimalText, DecimalText)>,
}

#[derive(Default)]
struct LineState {
    generation: u64,
    realtime: bool,
    context: Option<Context>,
    last_receipt_at: Option<DateTime<Utc>>,
    samples: Samples,
}

#[derive(Clone, Copy)]
struct VolumeRate {
    contracts: u128,
    elapsed_millis: u32,
}

fn compare_rates(lhs: VolumeRate, rhs: VolumeRate) -> std::cmp::Ordering {
    // The 28-digit wire quantity and at most 30,000 milliseconds fit u128 here.
    let lhs_scaled = lhs.contracts * u128::from(rhs.elapsed_millis);
    let rhs_scaled = rhs.contracts * u128::from(lhs.elapsed_millis);
    lhs_scaled.cmp(&rhs_scaled)
}

impl BaselineVolumeRate {
    fn median(sorted: &[VolumeRate]) -> Option<Self> {
        if sorted.len() < MIN_BASELINE_SAMPLES {
            return None;
        }
        let upper = sorted[sorted.len() / 2];
        let lower = sorted[(sorted.len() - 1) / 2];
        if compare_rates(lower, upper).is_eq() {
            return Some(Self {
                contracts: upper.contracts,
                elapsed_millis: upper.elapsed_millis,
            });
        }
        // Both observations have positive durations of at most 30,000 ms.
        // The denominator is at most 1,800,000,000 and the numerator is < 6×10³².
        Some(Self {
            contracts: lower.contracts * u128::from(upper.elapsed_millis)
                + upper.contracts * u128::from(lower.elapsed_millis),
            elapsed_millis: 2 * lower.elapsed_millis * upper.elapsed_millis,
        })
    }

    fn is_elevated(self, current: VolumeRate) -> Option<bool> {
        // Cross multiplication remains below 2 × 6×10³² × 30,000.
        (self.contracts > 0).then(|| {
            current.contracts * u128::from(self.elapsed_millis)
                >= 2 * self.contracts * u128::from(current.elapsed_millis)
        })
    }
}

#[derive(Default)]
struct Baseline {
    last_at: Option<DateTime<Utc>>,
    volume: VecDeque<(DateTime<Utc>, VolumeRate)>,
    spread: VecDeque<(DateTime<Utc>, DecimalText)>,
}

fn preceding<T: Clone>(
    history: &mut VecDeque<(DateTime<Utc>, T)>,
    at: DateTime<Utc>,
    current: T,
) -> Vec<T> {
    history.retain(|(received_at, _)| at - *received_at <= BASELINE_RETENTION);
    let preceding = history
        .iter()
        .filter(|(received_at, _)| *received_at < at)
        .map(|(_, sample)| sample.clone())
        .collect();
    if history.len() == MAX_BASELINE_SAMPLES {
        history.pop_front();
    }
    history.push_back((at, current));
    preceding
}

/// Retains sampled state across callbacks within one generation and learns a
/// bounded rolling baseline. Callers reset the stream after receiver lag.
#[derive(Default)]
pub struct ObservationEngine {
    lines: HashMap<RequestKey, LineState>,
    baselines: HashMap<BaselineCohort, Baseline>,
    counts: ObservationCounts,
    last_observed_at: Option<DateTime<Utc>>,
}

impl ObservationEngine {
    pub fn reader_lagged(&mut self) {
        self.lines.clear();
        self.counts.reader_lags += 1;
    }

    pub fn summary(&self, as_of: DateTime<Utc>) -> ObservationSummary {
        ObservationSummary {
            as_of: self.last_observed_at.map_or(as_of, |at| at.max(as_of)),
            last_observed_at: self.last_observed_at,
            counts: self.counts,
        }
    }

    pub fn apply(
        &mut self,
        update: &LineUpdate,
        contract: Option<&ConfirmedContract>,
    ) -> Option<FlowObservation> {
        if update.key.product != Product::Opra || update.key.con_id <= 0 {
            return None;
        }
        let LineMessage::Event(event) = &update.message else {
            if self.lines.get(&update.key).is_some_and(|line| {
                line.generation < update.generation
                    || (line.generation == update.generation
                        && update.message == LineMessage::Closed)
            }) {
                self.lines.remove(&update.key);
            }
            return None;
        };
        let Some(contract) = contract.filter(|contract| {
            contract.con_id == update.key.con_id && contract.product() == Product::Opra
        }) else {
            if self
                .lines
                .get(&update.key)
                .is_some_and(|line| line.generation <= update.generation)
            {
                self.lines.remove(&update.key);
            }
            return None;
        };
        let line = self.lines.entry(update.key).or_default();
        if line.generation > update.generation {
            return None;
        }
        if line.generation != update.generation {
            *line = LineState {
                generation: update.generation,
                ..Default::default()
            };
        }
        match event {
            MarketEvent::DataType(kind) => {
                let realtime = *kind == Some(MarketDataType::Realtime);
                if line.realtime != realtime {
                    line.samples = Samples::default();
                    line.context = None;
                }
                line.realtime = realtime;
                return None;
            }
            MarketEvent::Notice(
                MarketNotice::NotEntitled | MarketNotice::CompetingSession | MarketNotice::Pacing,
            ) => {
                *line = LineState {
                    generation: update.generation,
                    ..Default::default()
                };
                return None;
            }
            _ => {}
        }
        let MarketEvent::Quote {
            field,
            value,
            delayed: false,
            received_at,
        } = event
        else {
            return None;
        };
        let at = *received_at;
        if !line.realtime || line.last_receipt_at.is_some_and(|previous| previous > at) {
            return None;
        }
        line.last_receipt_at = Some(at);
        let context = Context::at(contract, at);
        if line.context != context {
            line.context = context;
            line.samples = Samples::default();
        }
        let context = line.context.as_ref()?;
        let evidence = match field {
            QuoteField::LastPrice | QuoteField::LastSize => {
                let target = if *field == QuoteField::LastPrice {
                    &mut line.samples.last_price
                } else {
                    &mut line.samples.last_size
                };
                *target = positive_sample(value, at);
                return None;
            }
            QuoteField::CumulativeVolume => {
                let (rate, earlier, later) = line.samples.volume_rate(value, at)?;
                let baseline = baseline_at(&mut self.baselines, context.cohort, at);
                let evidence = line.samples.volume_evidence(
                    rate,
                    earlier,
                    later,
                    &context.multiplier,
                    baseline,
                )?;
                self.counts.volume_increases += 1;
                self.counts.elevated_volume +=
                    u64::from(evidence.comparison.elevated == Some(true));
                ObservationEvidence::VolumeIncrease(Box::new(evidence))
            }
            _ => {
                let book = line.samples.update_quote(*field, value, at)?;
                let baseline = baseline_at(&mut self.baselines, context.cohort, at);
                let mut prior = preceding(&mut baseline.spread, at, book.spread.clone());
                if prior.len() < MIN_BASELINE_SAMPLES {
                    return None;
                }
                prior.sort_unstable_by_key(DecimalText::value);
                let upper = &prior[prior.len() / 2];
                let lower = &prior[(prior.len() - 1) / 2];
                let typical_spread = exact_mean(lower, upper)?;
                // Doubling the 28-digit wire coefficient still fits a Decimal.
                if book.spread.value() < typical_spread.value() * Decimal::TWO {
                    return None;
                }
                self.counts.quote_widenings += 1;
                ObservationEvidence::QuoteWidening(Box::new(QuoteWidening {
                    bid: book.bid,
                    ask: book.ask,
                    bid_size: book.bid_size,
                    ask_size: book.ask_size,
                    spread: book.spread,
                    typical_spread,
                    baseline_samples: prior.len(),
                }))
            }
        };
        self.counts.observations += 1;
        self.last_observed_at = self.last_observed_at.max(Some(at));
        Some(FlowObservation {
            rule_version: OBSERVATION_RULE_VERSION,
            status: ObservationStatus::PendingVerification,
            con_id: update.key.con_id,
            generation: update.generation.into(),
            cohort: context.cohort,
            multiplier: context.multiplier.clone(),
            observed_at: at,
            observation: evidence,
        })
    }
}

fn baseline_at(
    baselines: &mut HashMap<BaselineCohort, Baseline>,
    cohort: BaselineCohort,
    at: DateTime<Utc>,
) -> &mut Baseline {
    if baselines.len() == MAX_BASELINE_COHORTS
        && !baselines.contains_key(&cohort)
        && let Some((&oldest, _)) = baselines.iter().min_by_key(|(_, entry)| entry.last_at)
    {
        baselines.remove(&oldest);
    }
    let baseline = baselines.entry(cohort).or_default();
    baseline.last_at = baseline.last_at.max(Some(at));
    baseline
}

fn positive_sample(value: &Option<DecimalText>, at: DateTime<Utc>) -> Option<L1Sample> {
    value
        .as_ref()
        .filter(|value| value.value() > 0.into())
        .map(|value| L1Sample {
            value: value.clone(),
            received_at: at,
        })
}

fn whole_count(value: &DecimalText) -> Option<u128> {
    let value = value.value().normalize();
    if value.scale() != 0 {
        return None;
    }
    u128::try_from(value.mantissa()).ok()
}

fn fresh(sample: &L1Sample, at: DateTime<Utc>) -> bool {
    (TimeDelta::zero()..=QUOTE_FRESHNESS).contains(&(at - sample.received_at))
}

impl Samples {
    fn volume_rate(
        &mut self,
        value: &Option<DecimalText>,
        at: DateTime<Utc>,
    ) -> Option<(VolumeRate, L1Sample, L1Sample)> {
        let Some((value, count)) = value
            .as_ref()
            .and_then(|value| whole_count(value).map(|count| (value, count)))
        else {
            self.cumulative = None;
            return None;
        };
        let later = L1Sample {
            value: value.clone(),
            received_at: at,
        };
        let earlier = self.cumulative.replace(later.clone())?;
        let elapsed = at - earlier.received_at;
        let elapsed_millis = u32::try_from(elapsed.num_milliseconds()).ok()?;
        if elapsed > MAX_VOLUME_WINDOW {
            return None;
        }
        let contracts = count.checked_sub(whole_count(&earlier.value)?)?;
        Some((
            VolumeRate {
                contracts,
                elapsed_millis,
            },
            earlier,
            later,
        ))
    }

    fn volume_evidence(
        &self,
        rate: VolumeRate,
        earlier: L1Sample,
        later: L1Sample,
        multiplier: &DecimalText,
        baseline: &mut Baseline,
    ) -> Option<VolumeIncrease> {
        let at = later.received_at;
        let mut prior = if rate.elapsed_millis > 0 {
            preceding(&mut baseline.volume, at, rate)
        } else {
            Vec::new()
        };
        if rate.contracts == 0 {
            return None;
        }
        prior.sort_unstable_by(|lhs, rhs| compare_rates(*lhs, *rhs));
        let typical_rate = BaselineVolumeRate::median(&prior);
        let elevated = typical_rate.and_then(|typical| typical.is_elevated(rate));
        let delta_contracts = rate.contracts.to_string().parse().ok()?;
        let recent_in_window =
            |sample: &&L1Sample| sample.received_at >= earlier.received_at && fresh(sample, at);
        let sampled_last_price = self.last_price.as_ref().filter(recent_in_window).cloned();
        let sampled_last_size = self.last_size.as_ref().filter(recent_in_window).cloned();
        let estimated_premium = sampled_last_price
            .as_ref()
            .and_then(|price| exact_premium(&price.value, &delta_contracts, multiplier));
        Some(VolumeIncrease {
            earlier_cumulative_volume: earlier,
            later_cumulative_volume: later,
            delta_contracts,
            comparison: VolumeComparison {
                elapsed_millis: rate.elapsed_millis,
                baseline_samples: prior.len(),
                typical_rate,
                elevated,
            },
            sampled_last_price,
            sampled_last_size,
            estimated_premium,
        })
    }

    fn update_quote(
        &mut self,
        field: QuoteField,
        value: &Option<DecimalText>,
        at: DateTime<Utc>,
    ) -> Option<BookSample> {
        let target = match field {
            QuoteField::BidPrice => &mut self.bid.price,
            QuoteField::BidSize => &mut self.bid.size,
            QuoteField::AskPrice => &mut self.ask.price,
            QuoteField::AskSize => &mut self.ask.size,
            _ => return None,
        };
        *target = positive_sample(value, at);
        let bid = self.bid.price.as_ref().filter(|price| fresh(price, at))?;
        let ask = self.ask.price.as_ref().filter(|price| fresh(price, at))?;
        let bid_size = self.bid.size.as_ref().filter(|size| fresh(size, at))?;
        let ask_size = self.ask.size.as_ref().filter(|size| fresh(size, at))?;
        if bid.value.value() >= ask.value.value()
            || self.last_book.as_ref().is_some_and(|(old_bid, old_ask)| {
                old_bid.value() == bid.value.value() && old_ask.value() == ask.value.value()
            })
        {
            return None;
        }
        let spread = exact_difference(&ask.value, &bid.value)?;
        self.last_book = Some((bid.value.clone(), ask.value.clone()));
        Some(BookSample {
            bid: bid.clone(),
            ask: ask.clone(),
            bid_size: bid_size.clone(),
            ask_size: ask_size.clone(),
            spread,
        })
    }
}

#[cfg(test)]
mod tests;
