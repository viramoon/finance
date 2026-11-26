//! Finance market protocol v1.0 wire types and validation.
//!
//! The wire contract is independent of collector runtime and Nautilus types.

mod event;
mod instrument;
mod scalar;

pub use event::*;
pub use instrument::*;
pub use scalar::*;

use std::{error::Error, fmt};

use serde::{Deserialize, Deserializer};

/// The location and reason of a protocol validation failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError {
    field: String,
    reason: &'static str,
}

impl ValidationError {
    fn new(field: impl Into<String>, reason: &'static str) -> Self {
        Self {
            field: field.into(),
            reason,
        }
    }

    /// A dotted field path, including zero-based array indexes where relevant.
    pub fn field(&self) -> &str {
        &self.field
    }
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.field, self.reason)
    }
}

impl Error for ValidationError {}

fn require(
    condition: bool,
    field: impl Into<String>,
    reason: &'static str,
) -> Result<(), ValidationError> {
    if condition {
        Ok(())
    } else {
        Err(ValidationError::new(field, reason))
    }
}

fn required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(deserializer)
}

// Serde's struct visitor also accepts positional arrays. Require an object at
// every wire record boundary while keeping MapAccess and its duplicate keys.
struct ObjectOnly<T>(T);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for ObjectOnly<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::{MapAccess, Visitor, value::MapAccessDeserializer};
        use std::marker::PhantomData;

        struct ObjectVisitor<T>(PhantomData<T>);

        impl<'de, T: Deserialize<'de>> Visitor<'de> for ObjectVisitor<T> {
            type Value = ObjectOnly<T>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a JSON object")
            }

            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
                T::deserialize(MapAccessDeserializer::new(map)).map(ObjectOnly)
            }
        }

        deserializer.deserialize_map(ObjectVisitor(PhantomData))
    }
}

// The field list drives both the public type/schema and the private decoder.
macro_rules! wire_object {
    (
        $(#[$attr:meta])*
        pub struct $name:ident {
            $(
                $(#[serde($($serde:tt)*)])?
                $(#[schema($($schema:tt)*)])?
                pub $field:ident: $ty:ty,
            )*
        }
        $(validate = $validate:ident;)?
    ) => {
        $(#[$attr])*
        #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, utoipa::ToSchema)]
        #[serde(deny_unknown_fields)]
        pub struct $name {
            $(
                $(#[serde($($serde)*)])?
                $(#[schema($($schema)*)])?
                pub $field: $ty,
            )*
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                #[derive(serde::Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Wire {
                    $(
                        $(#[serde($($serde)*)])?
                        $field: $ty,
                    )*
                }
                let crate::ObjectOnly(Wire { $($field),* }) =
                    serde::Deserialize::deserialize(deserializer)?;
                let value = Self { $($field),* };
                $(value.$validate().map_err(serde::de::Error::custom)?;)?
                Ok(value)
            }
        }
    };
}

use wire_object;

// Serde's default enum decoder also permits externally tagged objects. Unit
// wire tags are only strings; keep their spellings with the enum definition.
macro_rules! wire_enum {
    (
        $(#[doc = $doc:literal])?
        pub enum $name:ident {
            $($variant:ident = $tag:literal,)*
        }
    ) => {
        $(#[doc = $doc])?
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
        pub enum $name {
            $(
                #[serde(rename = $tag)]
                $variant,
            )*
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let tag = String::deserialize(deserializer)?;
                match tag.as_str() {
                    $($tag => Ok(Self::$variant),)*
                    _ => Err(serde::de::Error::unknown_variant(&tag, &[$($tag),*])),
                }
            }
        }

        impl utoipa::PartialSchema for $name {
            fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
                use utoipa::openapi::schema::{ObjectBuilder, Type};
                let schema = ObjectBuilder::new()
                    .schema_type(Type::String)
                    .enum_values(Some([$($tag),*]));
                $(let schema = schema.description(Some($doc.trim()));)?
                schema.into()
            }
        }

        impl utoipa::ToSchema for $name {}
    };
}

use wire_enum;
