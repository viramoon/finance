use serde::Serialize;
use serde_json::Value;
use utoipa::ToSchema;

use super::IndicatorError;

pub const MAX_PERIOD: u64 = 2520;
pub const OUTPUT_DECIMAL_PLACES: u32 = 12;
pub const RSI_ID: &str = "indicator.rsi_wilder@1.0.0";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ParameterRule {
    Period { min: u64, max: u64 },
    TextLiteral { value: &'static str },
}

#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
pub struct IndicatorParameter {
    pub name: &'static str,
    pub rule: ParameterRule,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct IndicatorDefinition {
    /// Stable component identifier with an explicit algorithm version.
    pub id: &'static str,
    pub description: &'static str,
    pub parameters: &'static [IndicatorParameter],
    pub inputs: &'static [&'static str],
    pub outputs: &'static [&'static str],
    pub unit: &'static str,
    /// Number of earlier observed bars required before the current bar can yield a value.
    pub warmup: &'static str,
    pub seed: Option<&'static str>,
    pub ddof: Option<u32>,
    pub reset: Option<&'static str>,
    pub missing_bar: &'static str,
    pub price_basis: &'static str,
    pub rounding: &'static str,
    pub decimal_places: u32,
    #[serde(skip)]
    #[schema(ignore)]
    kind: Kind,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(super) enum Kind {
    Sma,
    Ema,
    Rsi,
    Bollinger,
    Macd,
    Atr,
    Vwap,
    Return,
    Volatility,
    VolumeSma,
}

const fn period(name: &'static str, min: u64) -> IndicatorParameter {
    IndicatorParameter {
        name,
        rule: ParameterRule::Period {
            min,
            max: MAX_PERIOD,
        },
    }
}

const PERIOD: &[IndicatorParameter] = &[period("period", 1)];
const RSI_PARAMS: &[IndicatorParameter] = &[
    period("period", 2),
    IndicatorParameter {
        name: "field",
        rule: ParameterRule::TextLiteral { value: "close" },
    },
];
const MACD_PARAMS: &[IndicatorParameter] = &[
    period("fast_period", 1),
    period("slow_period", 2),
    period("signal_period", 1),
];

const PRICE: &str = "quote_currency_per_base_unit";
const COMMON: IndicatorDefinition = IndicatorDefinition {
    id: "",
    description: "",
    parameters: PERIOD,
    inputs: &["close"],
    outputs: &["value"],
    unit: PRICE,
    warmup: "period - 1",
    seed: None,
    ddof: None,
    reset: None,
    missing_bar: "Skip absent bars. Do not synthesize observations or backfill by carrying forward.",
    price_basis: "Use one caller-supplied price basis per series; do not adjust prices or volume in the indicator.",
    rounding: "Round output to nearest with ties to even. Display rounding does not feed back into indicator state.",
    decimal_places: OUTPUT_DECIMAL_PLACES,
    kind: Kind::Sma,
};

static REGISTRY: &[IndicatorDefinition] = &[
    IndicatorDefinition {
        id: "indicator.sma@1.0.0",
        description: "Simple moving average (MA) of close prices.",
        ..COMMON
    },
    IndicatorDefinition {
        id: "indicator.ema@1.0.0",
        description: "Exponential moving average of closes with alpha 2/(period + 1).",
        seed: Some("Arithmetic mean of the first period closes."),
        kind: Kind::Ema,
        ..COMMON
    },
    IndicatorDefinition {
        id: RSI_ID,
        description: "Wilder RSI uses close-to-close changes and alpha 1/period. It is undefined until the series has a gain or a loss.",
        parameters: RSI_PARAMS,
        unit: "rsi_points_0_to_100",
        warmup: "period",
        seed: Some("Arithmetic means of the first period gains and losses (period + 1 closes)."),
        kind: Kind::Rsi,
        ..COMMON
    },
    IndicatorDefinition {
        id: "indicator.bollinger@1.0.0",
        description: "Bollinger bands use the SMA of closes and two population standard deviations on either side.",
        outputs: &["middle", "upper", "lower"],
        ddof: Some(0),
        rounding: "Round the middle to 12 places, ties to even. Round each band edge to the greatest Decimal-encodable scale up to 12 places, ties to even.",
        kind: Kind::Bollinger,
        ..COMMON
    },
    IndicatorDefinition {
        id: "indicator.macd@1.0.0",
        description: "Fast EMA minus slow EMA, with its signal EMA and the histogram of their difference. Fast period must be smaller than slow period.",
        parameters: MACD_PARAMS,
        outputs: &["macd", "signal", "histogram"],
        warmup: "slow_period + signal_period - 2",
        seed: Some("Each EMA seeds with the arithmetic mean of its first full period of inputs."),
        kind: Kind::Macd,
        ..COMMON
    },
    IndicatorDefinition {
        id: "indicator.atr@1.0.0",
        description: "Wilder average true range, using high minus low for the first bar and including the prior close thereafter.",
        inputs: &["high", "low", "close"],
        seed: Some("Arithmetic mean of the first period true ranges, then alpha 1/period."),
        kind: Kind::Atr,
        ..COMMON
    },
    IndicatorDefinition {
        id: "indicator.vwap@1.0.0",
        description: "Session volume-weighted mean of (high + low + close)/3. Undefined until cumulative volume is positive.",
        parameters: &[],
        inputs: &["high", "low", "close", "volume", "trading_day"],
        warmup: "0",
        reset: Some("At a change of the exchange trading-day label supplied by the caller."),
        kind: Kind::Vwap,
        ..COMMON
    },
    IndicatorDefinition {
        id: "indicator.close_return@1.0.0",
        description: "Simple (current close minus prior close) / prior close over period observed bars. A zero base close is undefined.",
        unit: "return_ratio",
        warmup: "period",
        kind: Kind::Return,
        ..COMMON
    },
    IndicatorDefinition {
        id: "indicator.realized_volatility@1.0.0",
        description: "Unannualized sample standard deviation of period consecutive single-bar simple returns. A zero base close makes its return window undefined.",
        parameters: &[period("period", 2)],
        unit: "return_ratio",
        warmup: "period",
        ddof: Some(1),
        rounding: "Round single-bar returns to 64 places, ties to even. Round the output to the greatest Decimal-encodable scale up to 12 places, ties to even.",
        kind: Kind::Volatility,
        ..COMMON
    },
    IndicatorDefinition {
        id: "indicator.volume_sma@1.0.0",
        description: "Simple moving average of observed bar volume.",
        inputs: &["volume"],
        unit: "base_quantity",
        kind: Kind::VolumeSma,
        ..COMMON
    },
];

pub fn registry() -> &'static [IndicatorDefinition] {
    REGISTRY
}

/// Validated indicator reference and algorithm parameters.
#[derive(Debug, Clone)]
pub struct IndicatorSpec {
    definition: &'static IndicatorDefinition,
    pub(super) period: usize,
    pub(super) fast_period: usize,
    pub(super) signal_period: usize,
}

impl IndicatorSpec {
    pub fn new(id: &str, params: &Value) -> Result<Self, IndicatorError> {
        let definition = registry()
            .iter()
            .find(|definition| definition.id == id)
            .ok_or(IndicatorError::UnknownIndicator)?;
        let params = params
            .as_object()
            .ok_or(IndicatorError::InvalidParameters)?;
        if params.len() != definition.parameters.len() {
            return Err(IndicatorError::InvalidParameters);
        }
        let valid = definition.parameters.iter().all(|parameter| {
            let Some(value) = params.get(parameter.name) else {
                return false;
            };
            match parameter.rule {
                ParameterRule::Period { min, max } => {
                    value.as_u64().is_some_and(|n| (min..=max).contains(&n))
                }
                ParameterRule::TextLiteral { value: literal } => value.as_str() == Some(literal),
            }
        });
        if !valid {
            return Err(IndicatorError::InvalidParameters);
        }
        let period = |name| params.get(name).and_then(Value::as_u64).unwrap_or(0) as usize;
        let (period, fast_period, signal_period) = match definition.kind {
            Kind::Macd => {
                let slow = period("slow_period");
                let fast = period("fast_period");
                if fast >= slow {
                    return Err(IndicatorError::MacdPeriodOrder);
                }
                (slow, fast, period("signal_period"))
            }
            _ => (period("period"), 0, 0),
        };
        Ok(Self {
            definition,
            period,
            fast_period,
            signal_period,
        })
    }

    pub fn definition(&self) -> &'static IndicatorDefinition {
        self.definition
    }

    pub(super) fn kind(&self) -> Kind {
        self.definition.kind
    }

    pub(crate) fn observations_per_update(&self) -> usize {
        match self.kind() {
            Kind::Bollinger | Kind::Volatility => self.period,
            _ => 1,
        }
    }

    /// Earlier observed bars required in addition to the bar being evaluated.
    pub fn prior_bars(&self) -> u64 {
        let prior = match self.kind() {
            Kind::Sma | Kind::Ema | Kind::Bollinger | Kind::Atr | Kind::VolumeSma => {
                self.period - 1
            }
            Kind::Rsi | Kind::Return | Kind::Volatility => self.period,
            Kind::Macd => self.period + self.signal_period - 2,
            Kind::Vwap => 0,
        };
        prior as u64
    }
}
