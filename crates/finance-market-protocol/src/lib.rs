//! Finance market protocol wire types and validation.
//!
//! The wire contract is independent of collector runtime and Nautilus types.

mod diagnostics;
mod event;
mod instrument;
mod scalar;
pub mod schema_export;

pub use diagnostics::{DecodeError, ErrorKind, ProtocolJson, decode_json};
pub use event::*;
pub use instrument::*;
pub use scalar::*;

use std::{error::Error, fmt};

use serde::{Deserialize, Deserializer};

/// The location and reason of a protocol validation failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError {
    field: String,
    path: String,
    kind: ErrorKind,
    reason: &'static str,
}

impl ValidationError {
    fn new(field: impl Into<String>, reason: &'static str) -> Self {
        let field = field.into();
        let mut path = String::new();
        if !field.is_empty() {
            for segment in field.split('.') {
                diagnostics::push_segment(&mut path, segment);
            }
        }
        Self {
            field,
            path,
            kind: ErrorKind::ConstraintViolation,
            reason,
        }
    }

    fn at_root(field: &'static str, kind: ErrorKind, reason: &'static str) -> Self {
        Self {
            field: field.to_owned(),
            path: String::new(),
            kind,
            reason,
        }
    }

    /// A dotted field path, including zero-based array indexes where relevant.
    pub fn field(&self) -> &str {
        &self.field
    }

    /// RFC 6901 JSON Pointer. An empty string refers to the whole record.
    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    pub fn reason(&self) -> &str {
        self.reason
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

fn require_schema_version(condition: bool, reason: &'static str) -> Result<(), ValidationError> {
    require(condition, "schema_version", reason).map_err(|mut error| {
        error.kind = ErrorKind::InvalidValue;
        error
    })
}

fn required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(deserializer)
}

// Require an object at every record boundary while preserving duplicate keys.
fn decode_object<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    decode_map::<D, T, false>(deserializer)
}

fn decode_tagged_object<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    decode_map::<D, T, true>(deserializer)
}

fn decode_map<'de, D, T, const STRING_KIND: bool>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    use serde::de::{MapAccess, Visitor, value::MapAccessDeserializer};
    use std::marker::PhantomData;

    struct ObjectVisitor<T, const STRING_KIND: bool>(PhantomData<T>);

    impl<'de, T: Deserialize<'de>, const STRING_KIND: bool> Visitor<'de>
        for ObjectVisitor<T, STRING_KIND>
    {
        type Value = T;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a JSON object")
        }

        fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<T, A::Error> {
            if STRING_KIND {
                let map = StringKindMap {
                    map,
                    read_kind: false,
                };
                T::deserialize(MapAccessDeserializer::new(map))
            } else {
                T::deserialize(MapAccessDeserializer::new(map))
            }
        }
    }

    deserializer.deserialize_map(ObjectVisitor::<T, STRING_KIND>(PhantomData))
}

// The tag in a Serde adjacent enum also accepts external unit-enum syntax.
// Check the kind's string type before forwarding it to that enum decoder.
struct StringKindMap<A> {
    map: A,
    read_kind: bool,
}

impl<'de, A: serde::de::MapAccess<'de>> serde::de::MapAccess<'de> for StringKindMap<A> {
    type Error = A::Error;

    fn next_key_seed<K: serde::de::DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Self::Error> {
        use serde::de::IntoDeserializer;

        let Some(key) = self.map.next_key::<String>()? else {
            return Ok(None);
        };
        self.read_kind = key == "kind";
        seed.deserialize(key.into_deserializer()).map(Some)
    }

    fn next_value_seed<V: serde::de::DeserializeSeed<'de>>(
        &mut self,
        seed: V,
    ) -> Result<V::Value, Self::Error> {
        use serde::de::IntoDeserializer;

        if self.read_kind {
            let tag = self.map.next_value::<String>()?;
            seed.deserialize(tag.into_deserializer())
        } else {
            self.map.next_value_seed(seed)
        }
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
        $(decode = $decode:ident;)?
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
                let Wire { $($field),* } = wire_object!(@decoder $($decode)?)(deserializer)?;
                let value = Self { $($field),* };
                $(value.$validate().map_err(crate::diagnostics::serde_validation_error)?;)?
                Ok(value)
            }
        }
    };
    (@decoder $decode:ident) => { crate::$decode };
    (@decoder) => { crate::decode_object };
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
