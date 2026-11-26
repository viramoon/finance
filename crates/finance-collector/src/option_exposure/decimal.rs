use finance_market_protocol::DecimalText;
use rust_decimal::Decimal;

use super::ExposureError;

fn unrepresentable() -> ExposureError {
    ExposureError::Arithmetic("exact value exceeds decimal capacity")
}

fn from_parts(mut mantissa: i128, mut scale: u32) -> Result<Decimal, ExposureError> {
    while scale > 0 && mantissa % 10 == 0 {
        mantissa /= 10;
        scale -= 1;
    }
    Decimal::try_from_i128_with_scale(mantissa, scale).map_err(|_| unrepresentable())
}

pub(super) fn add_exact(lhs: Decimal, rhs: Decimal) -> Result<Decimal, ExposureError> {
    let lhs = lhs.normalize();
    let rhs = rhs.normalize();
    let scale = lhs.scale().max(rhs.scale());
    let align = |value: Decimal| {
        value
            .mantissa()
            .checked_mul(10_i128.pow(scale - value.scale()))
            .ok_or_else(unrepresentable)
    };
    let sum = align(lhs)?
        .checked_add(align(rhs)?)
        .ok_or_else(unrepresentable)?;
    from_parts(sum, scale)
}

pub(super) fn contract_exposure(
    oi: Decimal,
    multiplier: Decimal,
    gamma: Decimal,
    spot: Decimal,
) -> Result<Decimal, ExposureError> {
    let factors = [oi, multiplier, gamma, spot, spot];
    if factors.iter().any(Decimal::is_zero) {
        return Ok(Decimal::ZERO);
    }

    // Cancel powers of ten across all factors before multiplication. This applies
    // the 1% scale as part of the same product without rounding an intermediate.
    let mut scale = factors.iter().map(Decimal::scale).sum::<u32>() + 2;
    let mut mantissas = factors.map(|value| value.mantissa());
    while scale > 0 {
        let Some(two) = mantissas.iter().position(|value| value % 2 == 0) else {
            break;
        };
        let Some(five) = mantissas.iter().position(|value| value % 5 == 0) else {
            break;
        };
        mantissas[two] /= 2;
        mantissas[five] /= 5;
        scale -= 1;
    }

    let mantissa = mantissas
        .into_iter()
        .try_fold(1_i128, i128::checked_mul)
        .ok_or_else(unrepresentable)?;
    from_parts(mantissa, scale)
}

pub(super) fn decimal_text(value: Decimal) -> Result<DecimalText, ExposureError> {
    value
        .normalize()
        .to_string()
        .parse()
        .map_err(|_| ExposureError::Arithmetic("exact value exceeds F1 DecimalText capacity"))
}
