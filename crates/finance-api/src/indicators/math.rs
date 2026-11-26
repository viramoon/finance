use rust_decimal::Decimal;

use super::IndicatorError;

pub(super) type Numeric<T = Decimal> = Result<T, IndicatorError>;

pub(super) fn add(a: Decimal, b: Decimal) -> Numeric {
    a.checked_add(b).ok_or(IndicatorError::NumericOverflow)
}

pub(super) fn sub(a: Decimal, b: Decimal) -> Numeric {
    a.checked_sub(b).ok_or(IndicatorError::NumericOverflow)
}

pub(super) fn mul(a: Decimal, b: Decimal) -> Numeric {
    a.checked_mul(b).ok_or(IndicatorError::NumericOverflow)
}

pub(super) fn div(a: Decimal, b: Decimal) -> Numeric {
    a.checked_div(b).ok_or(IndicatorError::NumericOverflow)
}

fn sqrt(value: Decimal) -> Numeric {
    if value.is_zero() {
        return Ok(Decimal::ZERO);
    }
    // Start above the root. Stop at convergence or when Decimal rounding would
    // make a final iteration larger. The divided difference avoids overflow.
    let mut current = value.max(Decimal::ONE);
    for _ in 0..128 {
        let next = add(
            current,
            div(sub(div(value, current)?, current)?, Decimal::TWO)?,
        )?;
        if next >= current {
            return Ok(current);
        }
        current = next;
    }
    Err(IndicatorError::NumericOverflow)
}

pub(super) fn return_ratio(close: Decimal, previous: Decimal) -> Numeric<Option<Decimal>> {
    if previous.is_zero() {
        Ok(None)
    } else {
        Ok(Some(div(sub(close, previous)?, previous)?))
    }
}

pub(super) fn deviation(
    mut samples: impl Iterator<Item = Decimal> + Clone,
    mean: Decimal,
    divisor: usize,
) -> Numeric {
    // Center and scale before squaring so small differences around large
    // prices retain precision, including differences near the output rounding.
    let scale = samples.clone().try_fold(Decimal::ZERO, |scale, value| {
        Ok(scale.max(sub(value, mean)?.abs()))
    })?;
    if scale.is_zero() {
        return Ok(Decimal::ZERO);
    }
    let squared = samples.try_fold(Decimal::ZERO, |total, value| {
        let delta = div(sub(value, mean)?, scale)?;
        add(total, mul(delta, delta)?)
    })?;
    mul(scale, sqrt(div(squared, Decimal::from(divisor))?)?)
}

/// Seed an exponential or Wilder average with a whole window of samples.
#[derive(Debug, Clone, Default)]
pub(super) struct Average {
    count: usize,
    total: Decimal,
    value: Option<Decimal>,
}

impl Average {
    pub(super) fn ema(&mut self, sample: Decimal, period: usize) -> Numeric<Option<Decimal>> {
        self.update(sample, period, Decimal::TWO, period + 1)
    }

    pub(super) fn wilder(&mut self, sample: Decimal, period: usize) -> Numeric<Option<Decimal>> {
        self.update(sample, period, Decimal::ONE, period)
    }

    fn update(
        &mut self,
        sample: Decimal,
        period: usize,
        weight: Decimal,
        denominator: usize,
    ) -> Numeric<Option<Decimal>> {
        if let Some(value) = self.value {
            // value + alpha * (sample - value) preserves a repeated flat value
            // and uses no rounded output as the next step's input.
            let alpha = div(weight, Decimal::from(denominator))?;
            self.value = Some(add(value, mul(alpha, sub(sample, value)?)?)?);
        } else {
            self.total = add(self.total, sample)?;
            self.count += 1;
            if self.count == period {
                self.value = Some(div(self.total, Decimal::from(period))?);
            }
        }
        Ok(self.value)
    }
}
