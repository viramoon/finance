//! Versioned decimal indicators for one chronological, consistently adjusted bar series.
//!
//! Callers resolve ordering, point-in-time availability, revisions and the exchange
//! trading day before supplying bars. Recreate an engine and replay its complete
//! input prefix after restart or correction to reproduce recursive values exactly.

mod algorithm;
mod math;
mod registry;

use std::{
    collections::{BTreeMap, VecDeque},
    error::Error,
    fmt,
};

use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::{Decimal, RoundingStrategy};
use serde::Serialize;
use utoipa::ToSchema;

use algorithm::{Algorithm, Observation, Output};
use math::{Numeric, return_ratio};
use registry::Kind;
pub use registry::{
    IndicatorDefinition, IndicatorParameter, IndicatorSpec, MAX_PERIOD, OUTPUT_DECIMAL_PLACES,
    ParameterRule, RSI_ID, registry,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bar {
    pub open_ts: DateTime<Utc>,
    pub end_ts: DateTime<Utc>,
    /// Exchange trading-day label, including the applicable overnight-session convention.
    pub trading_day: NaiveDate,
    pub open: Decimal,
    pub high: Decimal,
    pub low: Decimal,
    pub close: Decimal,
    pub volume: Decimal,
}

impl Bar {
    pub fn validate(&self) -> Result<(), IndicatorError> {
        if self.open_ts >= self.end_ts
            || self.low > self.open.min(self.close)
            || self.high < self.open.max(self.close)
            || self.volume < Decimal::ZERO
        {
            return Err(IndicatorError::InvalidBar);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ValueStatus {
    Warmup,
    Ready,
    Undefined,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndicatorValue {
    pub status: ValueStatus,
    pub provisional: bool,
    /// Rounded at the declared output precision. Empty during warmup or when undefined.
    pub values: BTreeMap<&'static str, Decimal>,
}

impl IndicatorValue {
    fn new(output: Output, provisional: bool) -> Self {
        let (status, values) = match output {
            Output::Warmup => (ValueStatus::Warmup, BTreeMap::new()),
            Output::Undefined => (ValueStatus::Undefined, BTreeMap::new()),
            Output::Scalar(value) => (ValueStatus::Ready, Self::rounded([("value", value)])),
            Output::Channels(channels) => (ValueStatus::Ready, Self::rounded(channels)),
        };
        Self {
            status,
            provisional,
            values,
        }
    }

    fn rounded(
        values: impl IntoIterator<Item = (&'static str, Decimal)>,
    ) -> BTreeMap<&'static str, Decimal> {
        values
            .into_iter()
            .map(|(name, value)| {
                let rounded = value.round_dp_with_strategy(
                    OUTPUT_DECIMAL_PLACES,
                    RoundingStrategy::MidpointNearestEven,
                );
                (name, rounded.normalize())
            })
            .collect()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EvaluationMode {
    Closed,
    Advance,
    Preview,
}

impl EvaluationMode {
    fn emits_value(self) -> bool {
        self != Self::Advance
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndicatorError {
    UnknownIndicator,
    InvalidParameters,
    MacdPeriodOrder,
    InvalidBar,
    OutOfOrder,
    NumericOverflow,
}

impl fmt::Display for IndicatorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::UnknownIndicator => "unknown indicator or version",
            Self::InvalidParameters => "invalid indicator parameters",
            Self::MacdPeriodOrder => "fast_period must be smaller than slow_period",
            Self::InvalidBar => "invalid bar time bounds, OHLC or volume",
            Self::OutOfOrder => "bar overlaps the previous bar or its trading day regressed",
            Self::NumericOverflow => "indicator arithmetic exceeded the supported Decimal range",
        })
    }
}

impl Error for IndicatorError {}

/// Incremental evaluator for a single bar series and price basis.
pub struct IndicatorEngine {
    algorithms: Vec<(IndicatorSpec, Algorithm)>,
    history: VecDeque<Observation>,
    history_limit: usize,
    needs_returns: bool,
}

impl IndicatorEngine {
    pub fn new(specs: &[IndicatorSpec]) -> Self {
        let history_limit = specs
            .iter()
            .map(|spec| match spec.kind() {
                Kind::Sma | Kind::VolumeSma | Kind::Return | Kind::Volatility => spec.period,
                Kind::Bollinger => spec.period - 1,
                _ => 1,
            })
            .max()
            .unwrap_or(1)
            .max(1);
        Self {
            algorithms: specs
                .iter()
                .map(|spec| (spec.clone(), Algorithm::new(spec)))
                .collect(),
            history: VecDeque::with_capacity(history_limit),
            history_limit,
            needs_returns: specs.iter().any(|spec| spec.kind() == Kind::Volatility),
        }
    }

    /// Commit a closed observation. An error leaves every algorithm unchanged.
    pub fn push_closed(&mut self, bar: &Bar) -> Result<Vec<IndicatorValue>, IndicatorError> {
        self.commit(bar, EvaluationMode::Closed)
    }

    /// Evaluate and commit a closed observation without materializing the output.
    /// Performs the same calculations and checks as `push_closed`.
    pub fn advance_closed(&mut self, bar: &Bar) -> Result<(), IndicatorError> {
        self.commit(bar, EvaluationMode::Advance).map(|_| ())
    }

    fn commit(&mut self, bar: &Bar, mode: EvaluationMode) -> Numeric<Vec<IndicatorValue>> {
        let next = self.evaluate(bar, mode)?;
        for ((_, state), update) in self.algorithms.iter_mut().zip(next.algorithms) {
            *state = update;
        }
        if self.history.len() == self.history_limit {
            self.history.pop_front();
        }
        self.history.push_back(next.observation);
        Ok(next.values)
    }

    /// Compute one provisional update with no effect on the committed series.
    pub fn preview(&self, bar: &Bar) -> Result<Vec<IndicatorValue>, IndicatorError> {
        Ok(self.evaluate(bar, EvaluationMode::Preview)?.values)
    }

    fn evaluate(&self, bar: &Bar, mode: EvaluationMode) -> Numeric<Evaluation> {
        bar.validate()?;
        let previous = self.history.back();
        if previous.is_some_and(|previous| {
            bar.open_ts < previous.bar.end_ts || bar.trading_day < previous.bar.trading_day
        }) {
            return Err(IndicatorError::OutOfOrder);
        }
        let single_return = match previous {
            Some(previous) if self.needs_returns => return_ratio(bar.close, previous.bar.close)?,
            _ => None,
        };
        let observation = Observation {
            bar: *bar,
            single_return,
        };
        let mut values = Vec::new();
        if mode.emits_value() {
            values.reserve(self.algorithms.len());
        }
        let mut algorithms = Vec::with_capacity(self.algorithms.len());
        // State is small and copied independently of the shared history so an
        // invalid observation cannot partially advance any indicator.
        for (spec, state) in &self.algorithms {
            let mut state = state.clone();
            let output = state.update(spec, &observation, &self.history)?;
            if mode.emits_value() {
                values.push(IndicatorValue::new(output, mode == EvaluationMode::Preview));
            }
            algorithms.push(state);
        }
        Ok(Evaluation {
            observation,
            algorithms,
            values,
        })
    }
}

struct Evaluation {
    observation: Observation,
    algorithms: Vec<Algorithm>,
    values: Vec<IndicatorValue>,
}

/// Evaluate closed bars in order through the incremental state machine.
pub fn batch<'a>(
    specs: &[IndicatorSpec],
    bars: impl IntoIterator<Item = &'a Bar>,
) -> Result<Vec<Vec<IndicatorValue>>, IndicatorError> {
    let mut engine = IndicatorEngine::new(specs);
    bars.into_iter()
        .map(|bar| engine.push_closed(bar))
        .collect()
}
