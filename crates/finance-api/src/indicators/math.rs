use num_bigint_dig::BigInt;
use num_integer::Integer;
use num_traits::{Signed, ToPrimitive, Zero};
use rust_decimal::Decimal;

use super::{IndicatorError, OUTPUT_DECIMAL_PLACES};

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

/// Represent the entire Decimal input range in common, exact 10^-28 units.
pub(super) fn scaled_mantissa(value: Decimal) -> BigInt {
    BigInt::from(value.mantissa()) * 10_u128.pow(Decimal::MAX_SCALE - value.scale())
}

pub(super) fn rounded_ratio(numerator: &BigInt, denominator: &BigInt) -> Numeric {
    rounded_output(numerator * 10_u64.pow(OUTPUT_DECIMAL_PLACES), denominator)
}

pub(super) fn rounded_scaled_ratio(numerator: &BigInt, denominator: &BigInt) -> Numeric {
    let factor = 10_u128.pow(Decimal::MAX_SCALE - OUTPUT_DECIMAL_PLACES);
    rounded_output(numerator.clone(), &(denominator * factor))
}

/// `numerator / denominator` is expressed in output quanta, with a positive denominator.
fn rounded_output(numerator: BigInt, denominator: &BigInt) -> Numeric {
    let (mut coefficient, remainder) = numerator.div_rem(denominator);
    let twice_remainder = remainder.abs() * 2_u32;
    if twice_remainder > *denominator || (twice_remainder == *denominator && coefficient.is_odd()) {
        coefficient += if numerator.is_negative() { -1 } else { 1 };
    }
    let mut scale = OUTPUT_DECIMAL_PLACES;
    let ten = BigInt::from(10);
    loop {
        if let Some(coefficient) = coefficient.to_i128()
            && let Ok(value) = Decimal::try_from_i128_with_scale(coefficient, scale)
        {
            return Ok(value);
        }
        // Remove only trailing zeros. Reducing scale further would change
        // the already rounded result instead of representing it exactly.
        if scale == 0 {
            return Err(IndicatorError::NumericOverflow);
        }
        let (shorter, remainder) = coefficient.div_rem(&ten);
        if !remainder.is_zero() {
            return Err(IndicatorError::NumericOverflow);
        }
        coefficient = shorter;
        scale -= 1;
    }
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
