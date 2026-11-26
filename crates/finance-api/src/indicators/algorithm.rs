use std::{collections::VecDeque, iter};

use chrono::NaiveDate;
use rust_decimal::Decimal;

use super::{
    Bar, IndicatorSpec,
    math::{Average, Numeric, add, deviation, div, mul, return_ratio, sub},
    registry::Kind,
};

#[derive(Debug, Clone, Copy)]
pub(super) struct Observation {
    pub(super) bar: Bar,
    pub(super) single_return: Option<Decimal>,
}

pub(super) enum Output {
    Warmup,
    Undefined,
    Scalar(Decimal),
    Channels([(&'static str, Decimal); 3]),
}

#[derive(Debug, Clone, Default)]
pub(super) struct Rsi {
    count: usize,
    gains: Decimal,
    losses: Decimal,
}

#[derive(Debug, Clone)]
pub(super) enum Algorithm {
    Sma(Decimal),
    Ema(Average),
    Rsi(Rsi),
    Bollinger,
    Macd {
        fast: Average,
        slow: Average,
        signal: Average,
    },
    Atr(Average),
    Vwap {
        day: Option<NaiveDate>,
        average: Decimal,
        volume: Decimal,
    },
    Return,
    Volatility,
    VolumeSma(Decimal),
}

impl Algorithm {
    pub(super) fn new(spec: &IndicatorSpec) -> Self {
        match spec.kind() {
            Kind::Sma => Self::Sma(Decimal::ZERO),
            Kind::Ema => Self::Ema(Average::default()),
            Kind::Rsi => Self::Rsi(Rsi::default()),
            Kind::Bollinger => Self::Bollinger,
            Kind::Macd => Self::Macd {
                fast: Average::default(),
                slow: Average::default(),
                signal: Average::default(),
            },
            Kind::Atr => Self::Atr(Average::default()),
            Kind::Vwap => Self::Vwap {
                day: None,
                average: Decimal::ZERO,
                volume: Decimal::ZERO,
            },
            Kind::Return => Self::Return,
            Kind::Volatility => Self::Volatility,
            Kind::VolumeSma => Self::VolumeSma(Decimal::ZERO),
        }
    }

    pub(super) fn requires_state_update(&self) -> bool {
        !matches!(self, Self::Bollinger | Self::Return | Self::Volatility)
    }

    pub(super) fn update(
        &mut self,
        spec: &IndicatorSpec,
        next: &Observation,
        history: &VecDeque<Observation>,
    ) -> Numeric<Output> {
        let period = spec.period;
        let bar = &next.bar;
        let previous = history.back().map(|observation| observation.bar.close);
        let lagged = history
            .len()
            .checked_sub(period)
            .and_then(|index| history.get(index));
        let value = match self {
            Self::Sma(total) => scalar(moving_mean(
                total,
                bar.close,
                lagged.map(|row| row.bar.close),
                history.len(),
                period,
            )?),
            Self::Ema(average) => scalar(average.ema(bar.close, period)?),
            Self::Rsi(state) => state.update(previous, bar.close, period)?,
            Self::Bollinger => bollinger(history, bar.close, period)?,
            Self::Macd { fast, slow, signal } => macd(fast, slow, signal, bar.close, spec)?,
            Self::Atr(average) => {
                let true_range = sub(
                    bar.high.max(previous.unwrap_or(bar.high)),
                    bar.low.min(previous.unwrap_or(bar.low)),
                )?;
                scalar(average.wilder(true_range, period)?)
            }
            Self::Vwap {
                day,
                average,
                volume,
            } => vwap(day, average, volume, bar)?,
            Self::Return => match lagged {
                Some(base) => return_ratio(bar.close, base.bar.close)?
                    .map_or(Output::Undefined, Output::Scalar),
                None => Output::Warmup,
            },
            Self::Volatility => realized_volatility(history, next.single_return, period)?,
            Self::VolumeSma(total) => scalar(moving_mean(
                total,
                bar.volume,
                lagged.map(|row| row.bar.volume),
                history.len(),
                period,
            )?),
        };
        Ok(value)
    }
}

fn scalar(value: Option<Decimal>) -> Output {
    value.map_or(Output::Warmup, Output::Scalar)
}

fn moving_mean(
    total: &mut Decimal,
    sample: Decimal,
    outgoing: Option<Decimal>,
    preceding: usize,
    period: usize,
) -> Numeric<Option<Decimal>> {
    *total = add(sub(*total, outgoing.unwrap_or(Decimal::ZERO))?, sample)?;
    if preceding + 1 < period {
        Ok(None)
    } else {
        Ok(Some(div(*total, Decimal::from(period))?))
    }
}

impl Rsi {
    fn update(
        &mut self,
        previous_close: Option<Decimal>,
        close: Decimal,
        period: usize,
    ) -> Numeric<Output> {
        let Some(previous_close) = previous_close else {
            return Ok(Output::Warmup);
        };
        let change = sub(close, previous_close)?;
        let gain = change.max(Decimal::ZERO);
        let loss = change.min(Decimal::ZERO).abs();
        // The gain/loss ratio is invariant to their common period divisor.
        // Retain period-scaled Wilder state so the seed cannot lose tiny moves
        // by dividing both totals into the minimum Decimal quantum.
        if self.count < period {
            self.count += 1;
            self.gains = add(self.gains, gain)?;
            self.losses = add(self.losses, loss)?;
            if self.count < period {
                return Ok(Output::Warmup);
            }
        } else {
            let n = Decimal::from(period);
            self.gains = add(sub(self.gains, div(self.gains, n)?)?, gain)?;
            self.losses = add(sub(self.losses, div(self.losses, n)?)?, loss)?;
        }
        let total = add(self.gains, self.losses)?;
        if total.is_zero() {
            return Ok(Output::Undefined);
        }
        Ok(Output::Scalar(mul(
            div(self.gains, total)?,
            Decimal::ONE_HUNDRED,
        )?))
    }
}

fn bollinger(history: &VecDeque<Observation>, close: Decimal, period: usize) -> Numeric<Output> {
    if history.len() + 1 < period {
        return Ok(Output::Warmup);
    }
    let closes = history
        .iter()
        .skip(history.len() + 1 - period)
        .map(|row| row.bar.close)
        .chain(iter::once(close));
    let mean = mean(closes.clone(), period)?;
    let width = mul(deviation(closes, mean, period)?, Decimal::TWO)?;
    Ok(Output::Channels([
        ("middle", mean),
        ("upper", add(mean, width)?),
        ("lower", sub(mean, width)?),
    ]))
}

fn macd(
    fast: &mut Average,
    slow: &mut Average,
    signal: &mut Average,
    close: Decimal,
    spec: &IndicatorSpec,
) -> Numeric<Output> {
    let (Some(fast), Some(slow)) = (
        fast.ema(close, spec.fast_period)?,
        slow.ema(close, spec.period)?,
    ) else {
        return Ok(Output::Warmup);
    };
    let line = sub(fast, slow)?;
    let Some(signal) = signal.ema(line, spec.signal_period)? else {
        return Ok(Output::Warmup);
    };
    Ok(Output::Channels([
        ("macd", line),
        ("signal", signal),
        ("histogram", sub(line, signal)?),
    ]))
}

fn vwap(
    day: &mut Option<NaiveDate>,
    average: &mut Decimal,
    volume: &mut Decimal,
    bar: &Bar,
) -> Numeric<Output> {
    if *day != Some(bar.trading_day) {
        *day = Some(bar.trading_day);
        *average = Decimal::ZERO;
        *volume = Decimal::ZERO;
    }
    if !bar.volume.is_zero() {
        let price = div(add(add(bar.high, bar.low)?, bar.close)?, Decimal::from(3))?;
        let total = add(*volume, bar.volume)?;
        // Keep the weighted mean directly. A price-times-volume product can
        // underflow for valid small lots even when the mean is representable.
        *average = if volume.is_zero() {
            price
        } else {
            add(
                *average,
                mul(sub(price, *average)?, div(bar.volume, total)?)?,
            )?
        };
        *volume = total;
    }
    if volume.is_zero() {
        Ok(Output::Undefined)
    } else {
        Ok(Output::Scalar(*average))
    }
}

fn realized_volatility(
    history: &VecDeque<Observation>,
    current_return: Option<Decimal>,
    period: usize,
) -> Numeric<Output> {
    if history.len() < period {
        return Ok(Output::Warmup);
    }
    let returns = history
        .iter()
        .skip(history.len() + 1 - period)
        .map(|row| row.single_return)
        .chain(iter::once(current_return));
    if returns.clone().any(|value| value.is_none()) {
        return Ok(Output::Undefined);
    }
    let returns = returns.flatten();
    let mean = mean(returns.clone(), period)?;
    Ok(Output::Scalar(deviation(returns, mean, period - 1)?))
}

fn mean(mut samples: impl Iterator<Item = Decimal>, count: usize) -> Numeric {
    div(samples.try_fold(Decimal::ZERO, add)?, Decimal::from(count))
}
