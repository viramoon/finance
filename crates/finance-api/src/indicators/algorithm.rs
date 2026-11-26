use std::{collections::VecDeque, iter};

use chrono::NaiveDate;
use num_bigint_dig::{BigInt, Sign};
use num_traits::Zero;
use rust_decimal::Decimal;

use super::{
    Bar, IndicatorSpec,
    math::{
        Average, Numeric, add, deviation, div, mul, return_ratio, rounded_ratio,
        rounded_scaled_ratio, scaled_mantissa, sub,
    },
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
    gains: BigInt,
    losses: BigInt,
    last_ratio: Option<Decimal>,
}

#[derive(Debug, Clone)]
pub(super) enum Algorithm {
    Sma(BigInt),
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
        weighted_prices: BigInt,
        volume: BigInt,
    },
    Return,
    Volatility,
    VolumeSma(BigInt),
}

impl Algorithm {
    pub(super) fn new(spec: &IndicatorSpec) -> Self {
        match spec.kind() {
            Kind::Sma => Self::Sma(BigInt::zero()),
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
                weighted_prices: BigInt::zero(),
                volume: BigInt::zero(),
            },
            Kind::Return => Self::Return,
            Kind::Volatility => Self::Volatility,
            Kind::VolumeSma => Self::VolumeSma(BigInt::zero()),
        }
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
                weighted_prices,
                volume,
            } => vwap(day, weighted_prices, volume, bar)?,
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
    total: &mut BigInt,
    sample: Decimal,
    outgoing: Option<Decimal>,
    preceding: usize,
    period: usize,
) -> Numeric<Option<Decimal>> {
    if let Some(outgoing) = outgoing {
        *total -= scaled_mantissa(outgoing);
    }
    *total += scaled_mantissa(sample);
    if preceding + 1 < period {
        Ok(None)
    } else {
        rounded_scaled_ratio(total, &BigInt::from(period)).map(Some)
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
        let flat = close == previous_close;
        if self.count < period {
            self.count += 1;
        } else {
            // Period-scaled Wilder totals use 64 fractional digits. Each update
            // truncates by less than one such unit, and the preceding error
            // decays geometrically with the totals.
            let period = period as u32;
            for total in [&mut self.gains, &mut self.losses] {
                *total *= period - 1;
                *total /= period;
            }
            // Both totals undergo the same mathematical decay on flat bars.
            // Preserve that ratio while advancing the totals for the next move.
            if flat {
                return Ok(self.last_ratio.map_or(Output::Undefined, Output::Scalar));
            }
        }
        if !flat {
            const RSI_INPUT_SCALE: i128 = 10_i128.pow(64 - Decimal::MAX_SCALE);
            let change =
                (scaled_mantissa(close) - scaled_mantissa(previous_close)) * RSI_INPUT_SCALE;
            match change.sign() {
                Sign::Plus => self.gains += change,
                Sign::Minus => self.losses -= change,
                Sign::NoSign => {}
            }
        }
        if self.count < period {
            return Ok(Output::Warmup);
        }
        let total = &self.gains + &self.losses;
        self.last_ratio = if total.is_zero() {
            None
        } else {
            Some(rounded_ratio(&(&self.gains * 100u8), &total)?)
        };
        Ok(self.last_ratio.map_or(Output::Undefined, Output::Scalar))
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
    weighted_prices: &mut BigInt,
    volume: &mut BigInt,
    bar: &Bar,
) -> Numeric<Output> {
    if *day != Some(bar.trading_day) {
        *day = Some(bar.trading_day);
        weighted_prices.set_zero();
        volume.set_zero();
    }
    if !bar.volume.is_zero() {
        let quantity = scaled_mantissa(bar.volume);
        let price_sum =
            scaled_mantissa(bar.high) + scaled_mantissa(bar.low) + scaled_mantissa(bar.close);
        // Keep the factor of three in the denominator and the quantity in
        // integer units so neither a typical price nor a weight is rounded.
        *weighted_prices += price_sum * &quantity;
        *volume += quantity;
    }
    if volume.is_zero() {
        Ok(Output::Undefined)
    } else {
        rounded_scaled_ratio(weighted_prices, &(&*volume * 3_u32)).map(Output::Scalar)
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
