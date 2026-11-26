//! Offline validation and compilation for the declarative StrategyBundle v1 format.

mod bindings;
mod catalog;
mod compile;
mod parse;
mod registry;
mod schema;

pub use bindings::{Binding, BindingError, MemoryBindings, SqliteBindings, VersionBindings};
pub use catalog::{Availability, Bars, Catalog, Universe, example_catalog};
pub(crate) use compile::legacy_sma_parameters_valid;
pub use compile::{
    CompiledComponent, CompiledPlan, CompiledSleeve, ConflictRules, ResolvedInputs, compile,
    compile_legacy_sma,
};
pub use parse::{InputFormat, MAX_BYTES};
pub use registry::{Component, Input, Output, Warmup, component, components};
pub use schema::bundle_schema;

use parse::Parsed;
use serde::Serialize;
use serde_json::Value;
use std::sync::LazyLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Location {
    pub line: usize,
    pub column: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Diagnostic {
    /// JSON Pointer-shaped path. `<key>` stands for a redacted mapping key.
    /// The empty string identifies the document root.
    pub path: String,
    pub code: &'static str,
    pub message: String,
    pub location: Location,
}

impl std::fmt::Display for Diagnostic {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} at {}:{} ({}): {}",
            self.code, self.location.line, self.location.column, self.path, self.message
        )
    }
}

impl std::error::Error for Diagnostic {}

fn diagnostic(
    parsed: &Parsed,
    path: &str,
    code: &'static str,
    message: impl Into<String>,
) -> Diagnostic {
    let (path, location) = parsed.diagnostic_location(path);
    Diagnostic {
        path,
        code,
        message: message.into(),
        location,
    }
}

fn child_path(parent: &str, key: &str) -> String {
    format!("{parent}/{}", key.replace('~', "~0").replace('/', "~1"))
}

enum PathSegment {
    Key(String),
    Index(usize),
}

fn redacted_path<'a>(segments: impl IntoIterator<Item = &'a PathSegment>) -> String {
    static SCHEMA: LazyLock<Value> = LazyLock::new(bundle_schema);
    let mut scopes = vec![&*SCHEMA];
    let mut path = String::new();
    for segment in segments {
        let mut known_field = false;
        let mut next = Vec::new();
        for scope in scopes {
            // Follow all declared branches. Syntax errors can precede reading a discriminator.
            let alternatives = scope["oneOf"]
                .as_array()
                .map_or(std::slice::from_ref(scope), Vec::as_slice);
            for rule in alternatives {
                match segment {
                    PathSegment::Key(key) => {
                        if let Some(field) = rule["properties"].get(key) {
                            known_field = true;
                            next.push(field);
                        } else if rule["additionalProperties"].is_object() {
                            next.push(&rule["additionalProperties"]);
                        }
                    }
                    PathSegment::Index(_) => {
                        if let Some(items) = rule.get("items") {
                            next.push(items);
                        }
                    }
                }
            }
        }
        path = match segment {
            PathSegment::Key(key) if known_field => child_path(&path, key),
            PathSegment::Key(_) => child_path(&path, "<key>"),
            PathSegment::Index(index) => child_path(&path, &index.to_string()),
        };
        scopes = next;
    }
    path
}

/// Export the same read-only descriptors used by the structural and semantic passes.
pub fn component_registry() -> Value {
    registry::component_registry()
}
