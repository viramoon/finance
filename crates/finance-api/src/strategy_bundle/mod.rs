//! Offline validation and compilation for the declarative StrategyBundle v1 format.

mod bindings;
mod catalog;
mod compile;
mod parse;
mod registry;
mod schema;

pub use bindings::{Binding, BindingError, MemoryBindings, SqliteBindings, VersionBindings};
pub use catalog::{Availability, Bars, Catalog, Universe, example_catalog};
pub use compile::{
    CompiledComponent, CompiledPlan, CompiledSleeve, ConflictRules, ResolvedInputs, compile,
};
pub use parse::{InputFormat, MAX_BYTES};
pub use registry::{Component, Input, Output, Warmup, component, components};
pub use schema::bundle_schema;

use parse::Parsed;
use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Location {
    pub line: usize,
    pub column: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Diagnostic {
    /// RFC 6901 JSON Pointer. The empty string identifies the document root.
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
    Diagnostic {
        path: path.to_owned(),
        code,
        message: message.into(),
        location: parsed.location(path),
    }
}

fn child_path(parent: &str, key: &str) -> String {
    format!("{parent}/{}", key.replace('~', "~0").replace('/', "~1"))
}

/// Export the same read-only descriptors used by the structural and semantic passes.
pub fn component_registry() -> Value {
    registry::component_registry()
}
