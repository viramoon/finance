use std::{error::Error, fmt};

use serde::{Deserialize, Deserializer};
use serde_json::{error::Category, value::RawValue};
use serde_path_to_error::Segment;

use crate::{Capability, Event, Instrument, ValidationError, decode_object};

/// A stable category for a decoding or validation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    InvalidJson,
    InvalidType,
    InvalidDecimal,
    InvalidInteger,
    OutOfRange,
    InvalidValue,
    MissingField,
    UnknownField,
    DuplicateField,
    ConstraintViolation,
}

impl ErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidJson => "invalid_json",
            Self::InvalidType => "invalid_type",
            Self::InvalidDecimal => "invalid_decimal",
            Self::InvalidInteger => "invalid_integer",
            Self::OutOfRange => "out_of_range",
            Self::InvalidValue => "invalid_value",
            Self::MissingField => "missing_field",
            Self::UnknownField => "unknown_field",
            Self::DuplicateField => "duplicate_field",
            Self::ConstraintViolation => "constraint_violation",
        }
    }
}

/// A JSON decode failure exposing a field as an RFC 6901 JSON Pointer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError {
    pub(crate) kind: ErrorKind,
    pub(crate) path: String,
    pub(crate) reason: String,
}

impl DecodeError {
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// Empty means the failure applies to the complete document.
    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }

    fn syntax(error: serde_json::Error) -> Self {
        Self {
            kind: ErrorKind::InvalidJson,
            path: String::new(),
            reason: error.to_string(),
        }
    }
}

impl fmt::Display for DecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} at {}: {}",
            self.kind.as_str(),
            if self.path.is_empty() {
                "<root>"
            } else {
                &self.path
            },
            self.reason,
        )
    }
}

impl Error for DecodeError {}

impl From<ValidationError> for DecodeError {
    fn from(error: ValidationError) -> Self {
        Self {
            kind: error.kind(),
            path: error.path().to_owned(),
            reason: error.reason().to_owned(),
        }
    }
}

mod sealed {
    use super::DecodeError;

    pub trait Sealed: Sized {
        fn decode(text: &str) -> Result<Self, DecodeError>;
    }
}

/// One of the supported top-level protocol records.
pub trait ProtocolJson: sealed::Sealed {}

impl ProtocolJson for Instrument {}
impl ProtocolJson for Capability {}
impl ProtocolJson for Event {}

impl sealed::Sealed for Instrument {
    fn decode(text: &str) -> Result<Self, DecodeError> {
        decode_at(text, "")
    }
}

impl sealed::Sealed for Capability {
    fn decode(text: &str) -> Result<Self, DecodeError> {
        decode_at(text, "").map_err(|mut error| {
            if error.kind == ErrorKind::InvalidType
                && error.path == "/max_depth"
                && depth_has_integer_spelling(text)
            {
                error.kind = ErrorKind::OutOfRange;
                error.reason = "value exceeds unsigned 32-bit range".to_owned();
            }
            error
        })
    }
}

// serde_json normally reports numeric tokens beyond u64 as floats. Revisit the
// failed depth field's spelling so a large integer still reports a range error.
fn depth_has_integer_spelling(text: &str) -> bool {
    #[derive(Deserialize)]
    struct Depth<'a> {
        #[serde(borrow)]
        max_depth: &'a RawValue,
    }

    let Ok(record) = serde_json::from_str::<Depth<'_>>(text) else {
        return false;
    };
    let value = record.max_depth.get();
    let digits = value.strip_prefix('-').unwrap_or(value);
    value != "-0" && !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
}

impl sealed::Sealed for Event {
    fn decode(text: &str) -> Result<Self, DecodeError> {
        Self::decode_record(text)
    }
}

/// Decode one complete JSON record with a field pointer and stable error kind.
pub fn decode_json<T: ProtocolJson>(text: &str) -> Result<T, DecodeError> {
    T::decode(text).map_err(|error| {
        // A structural error can stop the typed reader before later malformed
        // JSON. Where there are both, report malformed JSON consistently.
        match serde_json::from_str::<&RawValue>(text) {
            Ok(_) => error,
            Err(syntax) => DecodeError::syntax(syntax),
        }
    })
}

pub(crate) struct Object<T>(pub(crate) T);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Object<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        decode_object(deserializer).map(Self)
    }
}

pub(crate) fn decode_at<'de, T: Deserialize<'de>>(
    text: &'de str,
    prefix: &str,
) -> Result<T, DecodeError> {
    let mut json = serde_json::Deserializer::from_str(text);
    let value = serde_path_to_error::deserialize(&mut json)
        .map_err(|error| located_error(error, prefix))?;
    json.end().map_err(DecodeError::syntax)?;
    Ok(value)
}

fn located_error(
    error: serde_path_to_error::Error<serde_json::Error>,
    prefix: &str,
) -> DecodeError {
    let mut path = prefix.to_owned();
    let mut last_key = None;
    for segment in error.path() {
        match segment {
            Segment::Seq { index } => {
                path.push('/');
                path.push_str(&index.to_string());
                last_key = None;
            }
            Segment::Map { key } => {
                push_segment(&mut path, key);
                last_key = Some(key.as_str());
            }
            // Serde's synthetic variant and unknown segments are not JSON keys.
            Segment::Enum { .. } | Segment::Unknown => {}
        }
    }

    let inner = error.inner();
    let display = inner.to_string();
    let location = format!(" at line {} column {}", inner.line(), inner.column());
    let message = display.strip_suffix(&location).unwrap_or(&display);
    if inner.classify() != Category::Data {
        // A raw JSON token may be structurally valid yet fail Serde's native
        // numeric or Unicode representation. decode_json verifies its syntax
        // separately before keeping this field-level diagnostic.
        let kind = match message {
            "number out of range" => ErrorKind::InvalidType,
            "unexpected end of hex escape"
            | "lone leading surrogate in hex escape"
            | "invalid unicode code point" => ErrorKind::InvalidValue,
            _ => return DecodeError::syntax(error.into_inner()),
        };
        return DecodeError {
            kind,
            path,
            reason: message.to_owned(),
        };
    }
    if let Some((kind, relative, reason)) = read_validation_error(message) {
        path.push_str(relative);
        return DecodeError {
            kind,
            path,
            reason: reason.to_owned(),
        };
    }
    let (kind, field) = classify_serde(message);
    if let Some(field) = field
        && last_key != Some(field)
    {
        push_segment(&mut path, field);
    }
    DecodeError {
        kind,
        path,
        reason: message.to_owned(),
    }
}

// Serde's structural errors reach serde_json as text. Decode its standard
// error prefixes and keep the original text as the human-readable reason.
fn classify_serde(message: &str) -> (ErrorKind, Option<&str>) {
    if let Some(field) = message
        .strip_prefix("missing field `")
        .and_then(|text| text.strip_suffix('`'))
    {
        (ErrorKind::MissingField, Some(field))
    } else if let Some(field) = message
        .strip_prefix("duplicate field `")
        .and_then(|text| text.strip_suffix('`'))
    {
        (ErrorKind::DuplicateField, Some(field))
    } else if let Some(text) = message.strip_prefix("unknown field `") {
        let field = text
            .strip_suffix("`, there are no fields")
            .or_else(|| text.rsplit_once("`, expected ").map(|(field, _)| field));
        (ErrorKind::UnknownField, field)
    } else if message.starts_with("invalid type:") {
        (ErrorKind::InvalidType, None)
    } else if message.starts_with("invalid value: integer") && message.ends_with("expected u32") {
        (ErrorKind::OutOfRange, None)
    } else {
        (ErrorKind::InvalidValue, None)
    }
}

pub(crate) fn push_segment(pointer: &mut String, segment: &str) {
    pointer.push('/');
    pointer.push_str(&segment.replace('~', "~0").replace('/', "~1"));
}

const VALIDATION_SUFFIX: &str = " [market protocol ";

/// Transport our typed errors across serde_json's string-only custom error API.
/// A scalar parser supplies its location through the Serde field stack.
pub(crate) fn serde_validation_error<E: serde::de::Error>(error: ValidationError) -> E {
    E::custom(SerdeValidation(error))
}

struct SerdeValidation(ValidationError);

impl fmt::Display for SerdeValidation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}{VALIDATION_SUFFIX}{}:{}]",
            self.0,
            self.0.kind().as_str(),
            self.0.path()
        )
    }
}

fn read_validation_error(message: &str) -> Option<(ErrorKind, &str, &str)> {
    let (description, suffix) = message.rsplit_once(VALIDATION_SUFFIX)?;
    let (kind, path) = suffix.strip_suffix(']')?.split_once(':')?;
    let kind = match kind {
        "invalid_decimal" => ErrorKind::InvalidDecimal,
        "invalid_integer" => ErrorKind::InvalidInteger,
        "out_of_range" => ErrorKind::OutOfRange,
        "invalid_value" => ErrorKind::InvalidValue,
        "constraint_violation" => ErrorKind::ConstraintViolation,
        _ => return None,
    };
    let (_, reason) = description.split_once(": ")?;
    Some((kind, path, reason))
}
