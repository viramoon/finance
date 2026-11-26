use std::sync::LazyLock;

use num_bigint_dig::BigInt;
use num_integer::Integer;
use num_traits::{Signed, ToPrimitive, Zero};
use rust_decimal::Decimal;

use super::{IndicatorError, OUTPUT_DECIMAL_PLACES};

pub(super) type Numeric<T = Decimal> = Result<T, IndicatorError>;

/// Represent the entire Decimal input range in common, exact 10^-28 units.
pub(super) fn scaled_mantissa(value: Decimal) -> BigInt {
    BigInt::from(value.mantissa()) * 10_u128.pow(Decimal::MAX_SCALE - value.scale())
}

pub(super) fn rounded_ratio(numerator: &BigInt, denominator: &BigInt) -> Numeric {
    output_from_coefficient(rounded_quotient(
        &(numerator * 10_u64.pow(OUTPUT_DECIMAL_PLACES)),
        denominator,
    ))
}

pub(super) fn rounded_scaled_ratio(numerator: &BigInt, denominator: &BigInt) -> Numeric {
    let factor = 10_u128.pow(Decimal::MAX_SCALE - OUTPUT_DECIMAL_PLACES);
    output_from_coefficient(rounded_quotient(numerator, &(denominator * factor)))
}

/// Round a rational to an integer, with a positive denominator and ties to even.
pub(super) fn rounded_quotient(numerator: &BigInt, denominator: &BigInt) -> BigInt {
    let (mut coefficient, remainder) = numerator.div_rem(denominator);
    let twice_remainder = remainder.abs() * 2_u32;
    if twice_remainder > *denominator || (twice_remainder == *denominator && coefficient.is_odd()) {
        coefficient += if numerator.is_negative() { -1 } else { 1 };
    }
    coefficient
}

pub(super) fn output_from_coefficient(mut coefficient: BigInt) -> Numeric {
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

pub(super) const INTERNAL_DECIMAL_PLACES: u32 = 64;

/// Promote a Decimal to the internal grid without rounding.
pub(super) fn internal_mantissa(value: Decimal) -> BigInt {
    scaled_mantissa(value) * 10_u128.pow(INTERNAL_DECIMAL_PLACES - Decimal::MAX_SCALE)
}

/// Round an internal sample directly to the output grid.
pub(super) fn internal_output(mantissa: &BigInt) -> Numeric {
    static DIVISOR: LazyLock<BigInt> = LazyLock::new(|| {
        num_traits::Pow::pow(
            &BigInt::from(10_u8),
            INTERNAL_DECIMAL_PLACES - OUTPUT_DECIMAL_PLACES,
        )
    });
    output_from_coefficient(rounded_quotient(mantissa, &DIVISOR))
}

/// Hold a 64-place seed sum until the first complete window, then the running average.
#[derive(Debug, Clone, Default)]
pub(super) struct Average {
    count: usize,
    value: BigInt,
}

impl Average {
    pub(super) fn ema(&mut self, sample: &BigInt, period: usize) -> Option<&BigInt> {
        self.update(sample, period, 2, period + 1)
    }

    pub(super) fn wilder(&mut self, sample: &BigInt, period: usize) -> Option<&BigInt> {
        self.update(sample, period, 1, period)
    }

    fn update(
        &mut self,
        sample: &BigInt,
        period: usize,
        weight: usize,
        denominator: usize,
    ) -> Option<&BigInt> {
        if self.count < period {
            self.value += sample;
            self.count += 1;
            if self.count < period {
                return None;
            }
            self.value = rounded_quotient(&self.value, &BigInt::from(period));
        } else {
            // Combine both weighted terms before quantizing to the internal grid.
            let numerator = &self.value * (denominator - weight) + sample * weight;
            self.value = rounded_quotient(&numerator, &BigInt::from(denominator));
        }
        Some(&self.value)
    }
}
