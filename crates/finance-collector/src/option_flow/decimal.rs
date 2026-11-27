use finance_market_protocol::DecimalText;
use rust_decimal::Decimal;

fn decimal_text(mut mantissa: i128, mut scale: u32) -> Option<DecimalText> {
    while scale > 0 && mantissa % 10 == 0 {
        mantissa /= 10;
        scale -= 1;
    }
    Decimal::try_from_i128_with_scale(mantissa, scale)
        .ok()?
        .to_string()
        .parse()
        .ok()
}

fn aligned_mantissas(lhs: &DecimalText, rhs: &DecimalText) -> Option<(i128, i128, u32)> {
    let lhs = lhs.value().normalize();
    let rhs = rhs.value().normalize();
    let scale = lhs.scale().max(rhs.scale());
    let align = |value: Decimal| {
        value
            .mantissa()
            .checked_mul(10_i128.pow(scale - value.scale()))
    };
    Some((align(lhs)?, align(rhs)?, scale))
}

pub(super) fn exact_difference(lhs: &DecimalText, rhs: &DecimalText) -> Option<DecimalText> {
    let (lhs, rhs, scale) = aligned_mantissas(lhs, rhs)?;
    decimal_text(lhs.checked_sub(rhs)?, scale)
}

pub(super) fn exact_mean(lhs: &DecimalText, rhs: &DecimalText) -> Option<DecimalText> {
    let (lhs, rhs, scale) = aligned_mantissas(lhs, rhs)?;
    let sum = lhs.checked_add(rhs)?;
    if sum % 2 == 0 {
        decimal_text(sum / 2, scale)
    } else {
        decimal_text(sum.checked_mul(5)?, scale + 1)
    }
}

pub(super) fn exact_premium(
    price: &DecimalText,
    size: &DecimalText,
    multiplier: &DecimalText,
) -> Option<DecimalText> {
    let factors = [price.value(), size.value(), multiplier.value()];
    if factors.iter().any(Decimal::is_zero) {
        return "0".parse().ok();
    }
    let mut scale = factors.iter().map(Decimal::scale).sum::<u32>();
    let mut mantissas = factors.map(|value| value.mantissa());
    // Cancel trailing decimal zeros before multiplying. Decimal::checked_mul can
    // yield a rounded result when the intermediate has more than 28 places.
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
    let mantissa = mantissas.into_iter().try_fold(1_i128, i128::checked_mul)?;
    decimal_text(mantissa, scale)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_exact_wire_decimals_are_returned() {
        let decimal = |value: &str| value.parse::<DecimalText>().unwrap();
        assert_eq!(
            exact_premium(
                &decimal("9007199254740993"),
                &decimal("0.0020"),
                &decimal("500")
            ),
            Some(decimal("9007199254740993")),
        );
        assert_eq!(
            exact_premium(
                &decimal("0.000000000000001"),
                &decimal("0.000000000000001"),
                &decimal("1")
            ),
            None,
        );
        assert_eq!(
            exact_difference(
                &decimal("9007199254740993.25"),
                &decimal("9007199254740993.01")
            ),
            Some(decimal("0.24")),
        );
        assert_eq!(
            exact_difference(
                &decimal("9999999999999999999999999999"),
                &decimal("0.000000000000000000000000001")
            ),
            None,
        );
    }
}
