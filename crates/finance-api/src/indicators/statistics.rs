use std::cmp::Ordering;

use num_bigint_dig::{BigInt, Sign};
use num_integer::Integer;
use num_traits::{Signed, ToPrimitive, Zero};
use rust_decimal::Decimal;

use super::{
    IndicatorError, OUTPUT_DECIMAL_PLACES,
    math::{
        Numeric, internal_mantissa, rounded_quotient, rounded_ratio, rounded_scaled_ratio,
        scaled_mantissa,
    },
};

fn checked_return_ratio(close: Decimal, base: Decimal) -> Numeric<Option<(BigInt, BigInt)>> {
    if base.is_zero() {
        return Ok(None);
    }
    let base = scaled_mantissa(base);
    let change = scaled_mantissa(close) - &base;
    let (change, base) = if base.is_negative() {
        (-change, -base)
    } else {
        (change, base)
    };
    if change.abs() > &base * Decimal::MAX.mantissa() {
        return Err(IndicatorError::NumericOverflow);
    }
    Ok(Some((change, base)))
}

pub(super) fn close_return(close: Decimal, base: Decimal) -> Numeric<Option<Decimal>> {
    checked_return_ratio(close, base)?
        .map(|(change, base)| rounded_ratio(&change, &base))
        .transpose()
}

/// Single-bar return in internal decimal units, for realized volatility only.
pub(super) fn single_return(close: Decimal, base: Decimal) -> Numeric<Option<BigInt>> {
    Ok(checked_return_ratio(close, base)?
        .map(|(change, base)| rounded_quotient(&(change * internal_mantissa(Decimal::ONE)), &base)))
}

#[derive(Default)]
struct Moments {
    sum: BigInt,
    sum_squares: BigInt,
}

impl Moments {
    fn add(&mut self, value: &BigInt) {
        self.sum += value;
        self.sum_squares += value * value;
    }

    fn variance_numerator(&self, count: usize) -> BigInt {
        &self.sum_squares * count - &self.sum * &self.sum
    }
}

pub(super) fn bollinger(
    closes: impl Iterator<Item = Decimal>,
    count: usize,
) -> Numeric<[Decimal; 3]> {
    let mut moments = Moments::default();
    for close in closes {
        moments.add(&scaled_mantissa(close));
    }
    let middle = rounded_scaled_ratio(&moments.sum, &BigInt::from(count))?;
    let width_squared = moments.variance_numerator(count) * 4_u8;
    if width_squared.is_zero() {
        return Ok([middle; 3]);
    }

    // In input units each band is (sum ± sqrt(width_squared)) / count.
    let headroom = scaled_mantissa(Decimal::MAX) * count - moments.sum.abs();
    if width_squared > &headroom * &headroom {
        return Err(IndicatorError::NumericOverflow);
    }
    let width_floor = floor_sqrt(&width_squared);
    let width_ceil = &width_floor + u8::from(&width_floor * &width_floor != width_squared);
    let width_doubled_squared = width_squared * 4_u8;
    let upper = rounded_band(
        &moments.sum,
        &width_floor,
        &width_doubled_squared,
        count,
        false,
    )?;
    let lower = rounded_band(
        &moments.sum,
        &-width_ceil,
        &width_doubled_squared,
        count,
        true,
    )?;
    Ok([middle, upper, lower])
}

fn rounded_band(
    sum: &BigInt,
    offset_floor: &BigInt,
    width_doubled_squared: &BigInt,
    count: usize,
    subtract: bool,
) -> Numeric {
    let numerator_floor = sum + offset_floor;
    let doubled_sum = sum * 2_u8;
    rounded_dispersion(|scale| {
        let denominator = BigInt::from(count) * 10_u128.pow(Decimal::MAX_SCALE - scale);
        let floor = numerator_floor.div_floor(&denominator);
        // Compare the signed root to the midpoint by sign and then squaring.
        // The integer floor of the root is sufficient for the quotient above,
        // while this comparison retains its fractional part without rounding.
        let midpoint = (&floor * 2_u8 + 1_u8) * denominator - &doubled_sum;
        let side = match (subtract, midpoint.sign()) {
            (true, Sign::Plus) => Ordering::Less,
            (false, Sign::Minus) => Ordering::Greater,
            (true, _) => (&midpoint * &midpoint).cmp(width_doubled_squared),
            (false, _) => width_doubled_squared.cmp(&(&midpoint * &midpoint)),
        };
        round_from_floor(floor, side)
    })
}

pub(super) fn sample_deviation<'a>(
    samples: impl Iterator<Item = Option<&'a BigInt>>,
    count: usize,
) -> Numeric<Option<Decimal>> {
    let mut moments = Moments::default();
    for sample in samples {
        let Some(sample) = sample else {
            return Ok(None);
        };
        moments.add(sample);
    }
    let limit = internal_mantissa(Decimal::MAX);
    if moments.sum.abs() > limit {
        return Err(IndicatorError::NumericOverflow);
    }
    let numerator = moments.variance_numerator(count);
    let degrees = count * (count - 1);
    if numerator > (&limit * &limit) * degrees {
        return Err(IndicatorError::NumericOverflow);
    }
    let root_floor = floor_sqrt(&(&numerator / degrees));
    let doubled_root_squared = numerator * 4_u8;
    let internal_unit = internal_mantissa(Decimal::ONE);
    rounded_dispersion(|scale| {
        let quantum = &internal_unit / 10_u64.pow(scale);
        let floor = &root_floor / &quantum;
        let midpoint = (&floor * 2_u8 + 1_u8) * quantum;
        let side = doubled_root_squared.cmp(&((&midpoint * &midpoint) * degrees));
        round_from_floor(floor, side)
    })
    .map(Some)
}

fn floor_sqrt(value: &BigInt) -> BigInt {
    if value.is_zero() {
        return BigInt::zero();
    }
    // Use an integer initial bound to avoid the BigInt square root's f64 seed.
    // The first Newton estimate that would no longer decrease gives the root.
    let mut root = BigInt::from(1_u8) << value.bits().div_ceil(2);
    loop {
        let next = (&root + value / &root) >> 1;
        if next >= root {
            return root;
        }
        root = next;
    }
}

fn round_from_floor(floor: BigInt, side: Ordering) -> BigInt {
    if side == Ordering::Greater || (side == Ordering::Equal && floor.is_odd()) {
        floor + 1_u8
    } else {
        floor
    }
}

fn rounded_dispersion(coefficient_at_scale: impl Fn(u32) -> BigInt) -> Numeric {
    for scale in (0..=OUTPUT_DECIMAL_PLACES).rev() {
        if let Some(coefficient) = coefficient_at_scale(scale).to_i128()
            && let Ok(value) = Decimal::try_from_i128_with_scale(coefficient, scale)
        {
            return Ok(value);
        }
    }
    Err(IndicatorError::NumericOverflow)
}
