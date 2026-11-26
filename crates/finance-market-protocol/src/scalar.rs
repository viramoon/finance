use std::{fmt, str::FromStr};

use rust_decimal::Decimal;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use utoipa::{
    PartialSchema, ToSchema,
    openapi::{
        RefOr,
        schema::{ObjectBuilder, Schema, Type},
    },
};

use crate::{ErrorKind, ValidationError, diagnostics::serde_validation_error, wire_enum};

wire_enum! {
    pub enum SchemaVersion {
        V1 = "1.0",
    }
}

const END: &str = r"(?![\s\S])";
const UPPER_ID_PATTERN: &str = r"^[A-Z0-9][A-Z0-9_-]{0,31}(?![\s\S])";
const CURRENCY_PATTERN: &str = r"^[A-Z0-9]{1,16}(?![\s\S])";
const TOKEN_PATTERN: &str = r"^[a-z0-9][a-z0-9_.-]{0,63}(?![\s\S])";
const INSTRUMENT_ID_PATTERN: &str = concat!(
    r"^(?=[A-Z0-9_.-]{1,96}\.[A-Z0-9][A-Z0-9_-]{0,31}(?![\s\S]))",
    r"[A-Z0-9][A-Z0-9_-]*(?:\.[A-Z0-9][A-Z0-9_-]*)*\.[A-Z0-9][A-Z0-9_-]{0,31}(?![\s\S])"
);
const DECIMAL_PATTERN: &str = concat!(
    r"^(?=-?(?:[0-9][.]?){1,28}(?![\s\S]))",
    r"-?(?:0|[1-9][0-9]*)(?:\.[0-9]+)?(?![\s\S])"
);
const MAX_DECIMAL_DIGITS: usize = 28;

fn string_schema(pattern: Option<&str>, min_length: Option<usize>) -> RefOr<Schema> {
    ObjectBuilder::new()
        .schema_type(Type::String)
        .pattern(pattern)
        .min_length(min_length)
        .into()
}

fn upper_digit(byte: u8) -> bool {
    byte.is_ascii_uppercase() || byte.is_ascii_digit()
}

fn upper_segment(text: &str) -> bool {
    let mut bytes = text.bytes();
    bytes.next().is_some_and(upper_digit)
        && bytes.all(|byte| upper_digit(byte) || matches!(byte, b'_' | b'-'))
}

fn upper_id(text: &str) -> bool {
    text.len() <= 32 && upper_segment(text)
}

fn instrument_id(text: &str) -> bool {
    text.rsplit_once('.').is_some_and(|(local, venue)| {
        local.len() <= 96 && local.split('.').all(upper_segment) && upper_id(venue)
    })
}

fn currency(text: &str) -> bool {
    (1..=16).contains(&text.len()) && text.bytes().all(upper_digit)
}

fn lower_digit(byte: u8) -> bool {
    byte.is_ascii_lowercase() || byte.is_ascii_digit()
}

fn token(text: &str) -> bool {
    let mut bytes = text.bytes();
    text.len() <= 64
        && bytes.next().is_some_and(lower_digit)
        && bytes.all(|byte| lower_digit(byte) || matches!(byte, b'_' | b'-' | b'.'))
}

macro_rules! text_type {
    ($name:ident, $valid:expr, $reason:literal, $pattern:expr, $min_length:expr) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl FromStr for $name {
            type Err = ValidationError;

            fn from_str(text: &str) -> Result<Self, Self::Err> {
                Self::try_from(text.to_owned())
            }
        }

        impl TryFrom<String> for $name {
            type Error = ValidationError;

            fn try_from(text: String) -> Result<Self, Self::Error> {
                if $valid(&text) {
                    Ok(Self(text))
                } else {
                    Err(ValidationError::at_root(
                        stringify!($name),
                        ErrorKind::InvalidValue,
                        $reason,
                    ))
                }
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let text = String::deserialize(deserializer)?;
                text.try_into().map_err(serde_validation_error)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl PartialSchema for $name {
            fn schema() -> RefOr<Schema> {
                string_schema($pattern, $min_length)
            }
        }

        impl ToSchema for $name {}
    };
}

text_type!(
    InstrumentId,
    instrument_id,
    "expected LOCAL.VENUE in uppercase ASCII",
    Some(INSTRUMENT_ID_PATTERN),
    None
);
text_type!(
    Venue,
    upper_id,
    "expected an uppercase ASCII venue identifier",
    Some(UPPER_ID_PATTERN),
    None
);
text_type!(
    CalendarId,
    upper_id,
    "expected an uppercase ASCII calendar identifier",
    Some(UPPER_ID_PATTERN),
    None
);
text_type!(
    Currency,
    currency,
    "expected 1 to 16 uppercase ASCII letters or digits",
    Some(CURRENCY_PATTERN),
    None
);
text_type!(
    Token,
    token,
    "expected a lowercase ASCII protocol token",
    Some(TOKEN_PATTERN),
    None
);
text_type!(
    NonEmptyText,
    |text: &str| !text.is_empty(),
    "expected a nonempty string",
    None,
    Some(1)
);

impl InstrumentId {
    pub fn venue_suffix(&self) -> &str {
        // The grammar guarantees a final separator and a complete venue token.
        self.0.rsplit_once('.').expect("validated instrument ID").1
    }
}

/// A non-exponent decimal whose validated wire lexeme is preserved unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecimalText {
    text: String,
    value: Decimal,
}

impl DecimalText {
    pub fn as_str(&self) -> &str {
        &self.text
    }

    pub fn value(&self) -> Decimal {
        self.value
    }

    pub(crate) fn is_multiple_of(&self, step: &Self) -> bool {
        let value = self.value.mantissa();
        let step_value = step.value.mantissa();
        if step_value == 0 {
            return false;
        }

        if self.value.scale() > step.value.scale() {
            let factor = 10_i128.pow(self.value.scale() - step.value.scale());
            return value % factor == 0 && (value / factor) % step_value == 0;
        }

        // First reduce the mantissa modulo the step. At most 28 wire digits
        // fit in either mantissa, so each modular multiplication fits i128.
        let mut remainder = value % step_value;
        for _ in self.value.scale()..step.value.scale() {
            remainder = (remainder * 10) % step_value;
        }
        remainder == 0
    }
}

fn canonical_digits(text: &str) -> bool {
    !text.is_empty()
        && (text.len() == 1 || !text.starts_with('0'))
        && text.bytes().all(|byte| byte.is_ascii_digit())
}

impl TryFrom<String> for DecimalText {
    type Error = ValidationError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        let digits = text.strip_prefix('-').unwrap_or(&text);
        let (integer, fraction) = digits.split_once('.').unwrap_or((digits, ""));
        if integer.len() + fraction.len() > MAX_DECIMAL_DIGITS
            || !canonical_digits(integer)
            || (digits.contains('.') && fraction.is_empty())
            || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(ValidationError::at_root(
                "DecimalText",
                ErrorKind::InvalidDecimal,
                "expected plain decimal text with at most 28 digits and no exponent",
            ));
        }

        let value = Decimal::from_str_exact(&text).map_err(|_| {
            ValidationError::at_root(
                "DecimalText",
                ErrorKind::InvalidDecimal,
                "decimal cannot be represented exactly",
            )
        })?;
        Ok(Self { text, value })
    }
}

impl FromStr for DecimalText {
    type Err = ValidationError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::try_from(text.to_owned())
    }
}

impl<'de> Deserialize<'de> for DecimalText {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.try_into().map_err(serde_validation_error)
    }
}

impl Serialize for DecimalText {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl PartialSchema for DecimalText {
    fn schema() -> RefOr<Schema> {
        string_schema(Some(DECIMAL_PATTERN), None)
    }
}

impl ToSchema for DecimalText {}

/// Unsigned 64-bit position or nanosecond instant encoded as canonical text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct U64Text(u64);

impl U64Text {
    pub fn get(self) -> u64 {
        self.0
    }
}

impl From<u64> for U64Text {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

impl FromStr for U64Text {
    type Err = ValidationError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        if !canonical_digits(text) {
            return Err(ValidationError::at_root(
                "U64Text",
                ErrorKind::InvalidInteger,
                "expected canonical unsigned integer text",
            ));
        }
        text.parse::<u64>().map(Self).map_err(|_| {
            ValidationError::at_root(
                "U64Text",
                ErrorKind::OutOfRange,
                "value exceeds unsigned 64-bit range",
            )
        })
    }
}

impl<'de> Deserialize<'de> for U64Text {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde_validation_error)
    }
}

impl Serialize for U64Text {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&self.0)
    }
}

impl PartialSchema for U64Text {
    fn schema() -> RefOr<Schema> {
        // Cover shorter canonical values, then each prefix below u64::MAX
        // followed by any suffix of the same remaining width.
        let upper = u64::MAX.to_string();
        let mut alternatives = vec!["0".to_owned(), "[1-9][0-9]{0,18}".to_owned()];
        for (index, digit) in upper.bytes().enumerate() {
            let lower = if index == 0 { b'1' } else { b'0' };
            if digit > lower {
                let prefix = &upper[..index];
                let lower = char::from(lower);
                let last = char::from(digit - 1);
                let remaining = upper.len() - index - 1;
                alternatives.push(format!("{prefix}[{lower}-{last}][0-9]{{{remaining}}}"));
            }
        }
        alternatives.push(upper);
        let pattern = format!("^(?:{}){END}", alternatives.join("|"));
        string_schema(Some(&pattern), None)
    }
}

impl ToSchema for U64Text {}
