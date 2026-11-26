//! Closed structural rules shared by the compiler and the JSON Schema exporter.

use regex::Regex;
use rust_decimal::Decimal;
use serde_json::{Map, Value, json};
use std::borrow::Cow;
use std::sync::LazyLock;

use super::parse::Parsed;
use super::registry::{MAX_PERIOD, component, components};
use super::{Component, Diagnostic, child_path, diagnostic};

const VERSION_PATTERN: &str = r"(0|[1-9][0-9]{0,9})(\.(0|[1-9][0-9]{0,9})){2}";
const VERSIONED_ID: &str = r"[a-z][a-z0-9_-]{0,62}(\.[a-z][a-z0-9_-]{0,62}){0,7}";

struct Pattern {
    body: String,
    regex: Regex,
    expectation: &'static str,
}

impl Pattern {
    fn new(body: impl Into<String>, expectation: &'static str) -> Self {
        let body = body.into();
        let regex = Regex::new(&format!(r"\A(?:{body})\z")).expect("valid internal pattern");
        Self {
            body,
            regex,
            expectation,
        }
    }

    fn schema(&self) -> String {
        // ECMA-262's $ can also match before a trailing line terminator.
        format!(r"^(?:{})(?![\s\S])", self.body)
    }
}

static LOCAL_ID: LazyLock<Pattern> = LazyLock::new(|| {
    Pattern::new(
        "[A-Za-z][A-Za-z0-9_-]{0,63}",
        "a local identifier (1–64 ASCII characters)",
    )
});
static STRATEGY_ID: LazyLock<Pattern> =
    LazyLock::new(|| Pattern::new(VERSIONED_ID, "a lowercase strategy identifier"));
static VERSION: LazyLock<Pattern> =
    LazyLock::new(|| Pattern::new(VERSION_PATTERN, "a three-part numeric release version"));
static VERSIONED_REFERENCE: LazyLock<Pattern> = LazyLock::new(|| {
    Pattern::new(
        format!("{VERSIONED_ID}@{VERSION_PATTERN}"),
        "a registered local identifier with an explicit release version",
    )
});
static CATALOG_REFERENCE: LazyLock<Pattern> = LazyLock::new(|| {
    Pattern::new(
        "[A-Za-z0-9][A-Za-z0-9._@-]{0,255}",
        "an opaque local catalog identifier",
    )
});
static CALENDAR: LazyLock<Pattern> = LazyLock::new(|| {
    Pattern::new(
        "[A-Za-z0-9][A-Za-z0-9._-]{0,127}",
        "an opaque local calendar identifier",
    )
});
static CURRENCY: LazyLock<Pattern> =
    LazyLock::new(|| Pattern::new("[A-Z]{3}", "a three-letter uppercase currency code"));
static DECIMAL: LazyLock<Pattern> = LazyLock::new(|| {
    Pattern::new(
        r"-?(0|[1-9][0-9]{0,9})(\.[0-9]{1,18})?",
        "a decimal string with up to 10 integer and 18 fractional digits, without an exponent",
    )
});

#[derive(Debug, Clone, Copy)]
pub(super) enum TextFormat {
    LocalId,
    StrategyId,
    Version,
    VersionedReference,
    CatalogReference,
    Calendar,
    Currency,
    Decimal,
}

impl TextFormat {
    fn pattern(self) -> &'static Pattern {
        match self {
            Self::LocalId => &LOCAL_ID,
            Self::StrategyId => &STRATEGY_ID,
            Self::Version => &VERSION,
            Self::VersionedReference => &VERSIONED_REFERENCE,
            Self::CatalogReference => &CATALOG_REFERENCE,
            Self::Calendar => &CALENDAR,
            Self::Currency => &CURRENCY,
            Self::Decimal => &DECIMAL,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Field {
    name: &'static str,
    rule: Rule<'static>,
    required: bool,
}

pub(super) const fn field(name: &'static str, rule: Rule<'static>) -> Field {
    Field {
        name,
        rule,
        required: true,
    }
}

const fn optional(name: &'static str, rule: Rule<'static>) -> Field {
    Field {
        name,
        rule,
        required: false,
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) enum Placement {
    Factor,
    Module,
}

impl Placement {
    fn accepts(self, kind: &str) -> bool {
        match self {
            Self::Factor => matches!(kind, "factor" | "indicator"),
            Self::Module => MODULE_KINDS.contains(&kind),
        }
    }

    fn fields(self, definition: Option<&Component>) -> Cow<'static, [Field]> {
        let fields = match self {
            Self::Factor => FACTOR_FIELDS,
            Self::Module => MODULE_FIELDS,
        };
        if let Some(indicator) = definition.and_then(|item| item.indicator) {
            let mut fields = fields.to_vec();
            let rule = Rule::Choices(indicator.outputs);
            fields.push(if indicator.outputs.len() > 1 {
                field("output", rule)
            } else {
                optional("output", rule)
            });
            Cow::Owned(fields)
        } else {
            Cow::Borrowed(fields)
        }
    }

    fn resolve(self, value: &Value) -> Option<&'static Component> {
        let resolved = component(value["use"].as_str()?)?;
        let expected_kind = match self {
            Self::Factor => resolved.kind,
            Self::Module => value["kind"].as_str()?,
        };
        // Unknown IDs and slot mismatches belong to the semantic pass.
        (self.accepts(resolved.kind) && expected_kind == resolved.kind).then_some(resolved)
    }

    fn validate(self, parsed: &Parsed, value: &Value, path: &str) -> Result<(), Diagnostic> {
        let definition = self.resolve(value);
        Rule::Record(&self.fields(definition)).validate(parsed, value, path)?;
        if let Some(definition) = definition {
            Rule::Record(&definition.parameters).validate(
                parsed,
                &value["params"],
                &child_path(path, "params"),
            )?;
        }
        Ok(())
    }

    fn schema(self) -> Value {
        let branches: Vec<_> = components()
            .iter()
            .filter(|item| self.accepts(item.kind))
            .map(|item| {
                let mut branch = Rule::Record(&self.fields(Some(item))).json_schema();
                branch["properties"]["use"] = json!({ "const": item.id });
                branch["properties"]["params"] = item.parameter_schema();
                if matches!(self, Self::Module) {
                    branch["properties"]["kind"] = json!({ "const": item.kind });
                }
                branch
            })
            .collect();
        json!({ "oneOf": branches })
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) enum Rule<'a> {
    Object,
    Boolean,
    Integer {
        min: u64,
        max: u64,
    },
    String {
        min: usize,
        max: usize,
    },
    Text(TextFormat),
    Literal(&'static str),
    Choices(&'static [&'static str]),
    Array {
        item: &'static Rule<'static>,
        min: usize,
    },
    Map {
        value: &'static Rule<'static>,
        min: usize,
    },
    Record(&'a [Field]),
    Component(Placement),
    Recipe,
    ExecutionModel,
}

impl Rule<'_> {
    fn validate(self, parsed: &Parsed, value: &Value, path: &str) -> Result<(), Diagnostic> {
        let error = |code, message: String| diagnostic(parsed, path, code, message);
        let mismatch = |expected: &str| error("TYPE_MISMATCH", format!("expected {expected}"));
        let object = || value.as_object().ok_or_else(|| mismatch("an object"));
        let string = || value.as_str().ok_or_else(|| mismatch("a string"));
        match self {
            Self::Object => {
                object()?;
            }
            Self::Boolean => {
                value.as_bool().ok_or_else(|| mismatch("a boolean"))?;
            }
            Self::Integer { min, max } => {
                let in_range = match (value.as_u64(), value.as_i64()) {
                    (Some(number), _) => (min..=max).contains(&number),
                    (None, Some(_)) => false,
                    _ => return Err(mismatch("an integer")),
                };
                if !in_range {
                    return Err(error(
                        "OUT_OF_RANGE",
                        format!("expected an integer from {min} to {max}"),
                    ));
                }
            }
            Self::String { min, max } => {
                if !(min..=max).contains(&string()?.chars().count()) {
                    return Err(error(
                        "OUT_OF_RANGE",
                        format!("expected {min}–{max} characters"),
                    ));
                }
            }
            Self::Text(format) => {
                let pattern = format.pattern();
                if !pattern.regex.is_match(string()?) {
                    return Err(error(
                        "INVALID_VALUE",
                        format!("expected {}", pattern.expectation),
                    ));
                }
            }
            Self::Literal(expected) => {
                if string()? != expected {
                    return Err(error("INVALID_VALUE", format!("expected {expected:?}")));
                }
            }
            Self::Choices(choices) => {
                if !choices.contains(&string()?) {
                    return Err(error(
                        "INVALID_VALUE",
                        format!("expected one of {}", choices.join(", ")),
                    ));
                }
            }
            Self::Array { item, min } => {
                let array = value.as_array().ok_or_else(|| mismatch("an array"))?;
                if array.len() < min {
                    return Err(error(
                        "OUT_OF_RANGE",
                        format!("expected at least {min} array item(s)"),
                    ));
                }
                for (index, element) in array.iter().enumerate() {
                    item.validate(parsed, element, &child_path(path, &index.to_string()))?;
                }
            }
            Self::Map { value: item, min } => {
                let object = object()?;
                if object.len() < min {
                    return Err(error(
                        "OUT_OF_RANGE",
                        format!("expected at least {min} named field(s)"),
                    ));
                }
                for (name, element) in object {
                    let child = child_path(path, name);
                    if !LOCAL_ID.regex.is_match(name) {
                        return Err(diagnostic(
                            parsed,
                            &child,
                            "INVALID_VALUE",
                            format!("expected {}", LOCAL_ID.expectation),
                        ));
                    }
                    item.validate(parsed, element, &child)?;
                }
            }
            Self::Record(fields) => {
                let object = object()?;
                for name in object.keys() {
                    if !fields.iter().any(|field| field.name == name) {
                        return Err(diagnostic(
                            parsed,
                            &child_path(path, name),
                            "UNKNOWN_FIELD",
                            "Unexpected field. Use a field declared for this object.",
                        ));
                    }
                }
                for field in fields {
                    let child = child_path(path, field.name);
                    match object.get(field.name) {
                        Some(element) => field.rule.validate(parsed, element, &child)?,
                        None if field.required => {
                            return Err(diagnostic(
                                parsed,
                                &child,
                                "REQUIRED_FIELD",
                                format!("field {:?} is required", field.name),
                            ));
                        }
                        None => {}
                    }
                }
            }
            Self::Component(placement) => placement.validate(parsed, value, path)?,
            Self::Recipe => {
                let object = object()?;
                let type_path = child_path(path, "type");
                match object.get("type") {
                    Some(kind) => RECIPE_TYPES.validate(parsed, kind, &type_path)?,
                    None => {
                        return Err(diagnostic(
                            parsed,
                            &type_path,
                            "REQUIRED_FIELD",
                            "recipe type is required",
                        ));
                    }
                }
                recipe_rule(value).validate(parsed, value, path)?;
            }
            Self::ExecutionModel => VERSIONED_REF.validate(parsed, value, path)?,
        }
        Ok(())
    }

    pub(super) fn json_schema(self) -> Value {
        match self {
            Self::Object => json!({ "type": "object" }),
            Self::Boolean => json!({ "type": "boolean" }),
            Self::Integer { min, max } => {
                json!({ "type": "integer", "minimum": min, "maximum": max })
            }
            Self::String { min, max } => {
                json!({ "type": "string", "minLength": min, "maxLength": max })
            }
            Self::Text(format) => json!({ "type": "string", "pattern": format.pattern().schema() }),
            Self::Literal(expected) => json!({ "const": expected }),
            Self::Choices(choices) => json!({ "enum": choices }),
            Self::Array { item, min } => {
                json!({ "type": "array", "items": item.json_schema(), "minItems": min })
            }
            Self::Map { value, min } => json!({
                "type": "object",
                "propertyNames": { "pattern": LOCAL_ID.schema() },
                "additionalProperties": value.json_schema(),
                "minProperties": min,
            }),
            Self::Record(fields) => {
                let properties: Map<_, _> = fields
                    .iter()
                    .map(|field| (field.name.into(), field.rule.json_schema()))
                    .collect();
                let required: Vec<_> = fields
                    .iter()
                    .filter(|field| field.required)
                    .map(|field| field.name)
                    .collect();
                json!({
                    "type": "object",
                    "properties": properties,
                    "required": required,
                    "additionalProperties": false,
                })
            }
            Self::Component(placement) => placement.schema(),
            Self::Recipe => json!({
                "oneOf": [ENTRY_EXIT.json_schema(), TARGET_WEIGHTS.json_schema()],
            }),
            Self::ExecutionModel => {
                let models: Vec<_> = components()
                    .iter()
                    .filter(|item| item.kind == "execution")
                    .map(|item| item.id)
                    .collect();
                json!({ "enum": models })
            }
        }
    }

    fn normalize(self, value: &mut Value) {
        match self {
            Self::Text(TextFormat::Decimal) => {
                let decimal =
                    Decimal::from_str_exact(value.as_str().expect("validated decimal string"))
                        .expect("validated decimal fits exact representation");
                *value = json!(decimal.normalize().to_string());
            }
            Self::Array { item, .. } => {
                for element in value.as_array_mut().expect("validated array") {
                    item.normalize(element);
                }
            }
            Self::Map { value: item, .. } => {
                for element in value.as_object_mut().expect("validated map").values_mut() {
                    item.normalize(element);
                }
            }
            Self::Record(fields) => {
                for field in fields {
                    if let Some(element) = value.get_mut(field.name) {
                        field.rule.normalize(element);
                    }
                }
            }
            Self::Component(placement) => {
                if let Some(definition) = placement.resolve(value) {
                    Rule::Record(&definition.parameters).normalize(&mut value["params"]);
                }
            }
            Self::Recipe => recipe_rule(value).normalize(value),
            _ => {}
        }
    }
}

const LOCAL: Rule<'static> = Rule::Text(TextFormat::LocalId);
const VERSIONED_REF: Rule<'static> = Rule::Text(TextFormat::VersionedReference);
const CURRENCY_CODE: Rule<'static> = Rule::Text(TextFormat::Currency);
const DECIMAL_STRING: Rule<'static> = Rule::Text(TextFormat::Decimal);
const LOCAL_LIST: Rule<'static> = Rule::Array {
    item: &LOCAL,
    min: 0,
};
const MODULE_KINDS: &[&str] = &[
    "allocator",
    "entry",
    "exit",
    "filter",
    "regime",
    "risk",
    "sizing",
];
const FACTOR_FIELDS: &[Field] = &[
    field("use", VERSIONED_REF),
    field("params", Rule::Object),
    optional("depends_on", LOCAL_LIST),
];
const MODULE_FIELDS: &[Field] = &[
    field("kind", Rule::Choices(MODULE_KINDS)),
    field("use", VERSIONED_REF),
    field("params", Rule::Object),
];

const RECIPE_TYPES: Rule<'static> = Rule::Choices(&["entry_exit", "target_weights"]);
const FILTERS: Field = field("filters", LOCAL_LIST);
const RISK: Field = field("risk", LOCAL_LIST);
const ENTRY_EXIT: Rule<'static> = Rule::Record(&[
    field("type", Rule::Literal("entry_exit")),
    field("entry", LOCAL),
    field("exit", LOCAL),
    FILTERS,
    field("sizing", LOCAL),
    RISK,
]);
const TARGET_WEIGHTS: Rule<'static> = Rule::Record(&[
    field("type", Rule::Literal("target_weights")),
    field("allocator", LOCAL),
    FILTERS,
    RISK,
]);

fn recipe_rule(value: &Value) -> Rule<'static> {
    match value["type"].as_str().expect("validated recipe type") {
        "entry_exit" => ENTRY_EXIT,
        "target_weights" => TARGET_WEIGHTS,
        _ => unreachable!("validated recipe type"),
    }
}

const SLEEVE: Rule<'static> = Rule::Record(&[
    field("recipe", LOCAL),
    field("budget_weight", DECIMAL_STRING),
    field(
        "priority",
        Rule::Integer {
            min: 0,
            max: i32::MAX as u64,
        },
    ),
]);

pub(super) const LOT_SIZE_POLICY: Field =
    field("lot_size_policy", Rule::Literal("instrument_master"));
pub(super) const COMMISSION_BPS: Field = field("commission_bps", DECIMAL_STRING);
pub(super) const SLIPPAGE_BPS: Field = field("slippage_bps", DECIMAL_STRING);

const ROOT: Rule<'static> = Rule::Record(&[
    field("apiVersion", Rule::Literal("fertile.finance/strategy/v1")),
    field("kind", Rule::Literal("StrategyBundle")),
    field(
        "metadata",
        Rule::Record(&[
            field("id", Rule::Text(TextFormat::StrategyId)),
            field("version", Rule::Text(TextFormat::Version)),
            field("title", Rule::String { min: 1, max: 256 }),
        ]),
    ),
    field(
        "spec",
        Rule::Record(&[
            field("engine", Rule::Literal("nautilus_cash_equity")),
            field("mode", Rule::Literal("backtest")),
            field(
                "universe",
                Rule::Record(&[
                    field("ref", Rule::Text(TextFormat::CatalogReference)),
                    field("membership", Rule::Literal("point_in_time")),
                ]),
            ),
            field(
                "data",
                Rule::Record(&[
                    field("bars", Rule::Text(TextFormat::CatalogReference)),
                    field("timeframe", Rule::Literal("1d")),
                    field("price_basis", Rule::Literal("raw_split_free_reviewed")),
                    field("currency", CURRENCY_CODE),
                ]),
            ),
            field(
                "clock",
                Rule::Record(&[
                    field("calendar", Rule::Text(TextFormat::Calendar)),
                    field("decide_on", Rule::Literal("session_close")),
                    field("earliest_fill", Rule::Literal("next_session_close")),
                    field(
                        "warmup_sessions",
                        Rule::Integer {
                            min: 0,
                            max: MAX_PERIOD,
                        },
                    ),
                    field("missing_input", Rule::Literal("block_new_risk")),
                ]),
            ),
            field(
                "factors",
                Rule::Map {
                    value: &Rule::Component(Placement::Factor),
                    min: 0,
                },
            ),
            field(
                "modules",
                Rule::Map {
                    value: &Rule::Component(Placement::Module),
                    min: 0,
                },
            ),
            field(
                "recipes",
                Rule::Map {
                    value: &Rule::Recipe,
                    min: 1,
                },
            ),
            field(
                "portfolio",
                Rule::Record(&[
                    field(
                        "initial_cash",
                        Rule::Record(&[
                            field("currency", CURRENCY_CODE),
                            field("amount", DECIMAL_STRING),
                        ]),
                    ),
                    field(
                        "sleeves",
                        Rule::Array {
                            item: &SLEEVE,
                            min: 1,
                        },
                    ),
                    field("allocation", Rule::Literal("fixed_budgets")),
                    field("residual", Rule::Literal("cash")),
                    field("netting", Rule::Literal("by_instrument")),
                    field("max_gross_weight", DECIMAL_STRING),
                    field("max_weight_per_asset", DECIMAL_STRING),
                ]),
            ),
            field(
                "policy",
                Rule::Record(&[
                    field("on_entry_exit_collision", Rule::Literal("exit_first")),
                    field("same_session_reentry", Rule::Boolean),
                    field(
                        "on_missing_execution_price",
                        Rule::Literal("defer_and_report"),
                    ),
                    field("tie_break", Rule::Literal("stable_instrument_id")),
                ]),
            ),
            field(
                "execution",
                Rule::Record(&[
                    field("model", Rule::ExecutionModel),
                    LOT_SIZE_POLICY,
                    COMMISSION_BPS,
                    SLIPPAGE_BPS,
                ]),
            ),
        ]),
    ),
]);

pub(super) fn validate(parsed: &Parsed) -> Result<(), Diagnostic> {
    ROOT.validate(parsed, &parsed.value, "")
}

pub(super) fn normalize(value: &mut Value) {
    ROOT.normalize(value);
}

pub fn bundle_schema() -> Value {
    let mut schema = ROOT.json_schema();
    schema["$schema"] = json!("https://json-schema.org/draft/2020-12/schema");
    schema["title"] = json!("StrategyBundle v1");
    schema
}

#[cfg(test)]
mod tests {
    use super::super::registry::Warmup;
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn exported_component_schemas_match_registry() {
        let schema = bundle_schema();
        let spec = &schema["properties"]["spec"]["properties"];
        let mut exported = BTreeSet::new();
        for map in ["factors", "modules"] {
            let branches = spec[map]["additionalProperties"]["oneOf"]
                .as_array()
                .unwrap();
            for branch in branches {
                let fields = &branch["properties"];
                let id = fields["use"]["const"].as_str().unwrap();
                let named = component(id).unwrap();
                assert!(exported.insert(id), "{id}");
                assert_eq!(fields["params"], named.parameter_schema(), "{id}");
                assert_eq!(fields["params"]["additionalProperties"], false, "{id}");
                assert_eq!(branch["additionalProperties"], false, "{id}");
                match map {
                    "factors" => assert!(matches!(named.kind, "factor" | "indicator"), "{id}"),
                    "modules" => assert_eq!(fields["kind"]["const"], named.kind, "{id}"),
                    _ => unreachable!(),
                }
            }
        }
        let execution = &spec["execution"]["properties"];
        for id in execution["model"]["enum"].as_array().unwrap() {
            let id = id.as_str().unwrap();
            let named = component(id).unwrap();
            assert!(exported.insert(id), "{id}");
            assert_eq!(named.kind, "execution");
            let execution_params = named.parameter_schema();
            let parameter_fields = execution_params["properties"].as_object().unwrap();
            assert_eq!(
                execution.as_object().unwrap().len(),
                parameter_fields.len() + 1
            );
            for (name, field) in parameter_fields {
                assert_eq!(&execution[name], field, "{id}: {name}");
            }
        }
        assert_eq!(exported, components().iter().map(|item| item.id).collect());
        assert_eq!(exported.len(), components().len(), "duplicate registry IDs");
        for named in components() {
            assert!(VERSIONED_REFERENCE.regex.is_match(named.id), "{}", named.id);
            let parameters = named.parameter_schema();
            for &field in named.factor_parameters {
                assert_eq!(
                    parameters["properties"][field],
                    LOCAL.json_schema(),
                    "{}",
                    named.id
                );
            }
            if let Warmup::Parameter { name } = named.warmup {
                assert_eq!(
                    parameters["properties"][name]["type"], "integer",
                    "{}",
                    named.id
                );
                assert!(parameters["properties"][name]["maximum"].as_u64().unwrap() <= MAX_PERIOD);
            }
        }
    }

    #[test]
    fn decimal_constraints_preserve_exact_scale_and_canonical_zero() {
        let pattern = TextFormat::Decimal.pattern();
        for value in [
            "0",
            "-0.00",
            "9999999999.999999999999999999",
            "-0.000000000000000001",
        ] {
            assert!(pattern.regex.is_match(value), "{value}");
            let json = &mut json!(value);
            DECIMAL_STRING.normalize(json);
            assert!(pattern.regex.is_match(json.as_str().unwrap()), "{json}");
        }
        for value in [
            "00.15",
            "2.",
            ".1",
            "1e-3",
            "+1",
            "0.0\n",
            "0.0000000000000000001",
        ] {
            assert!(!pattern.regex.is_match(value), "{value}");
        }
        let mut value = json!("-0.000");
        DECIMAL_STRING.normalize(&mut value);
        assert_eq!(value, "0");
    }

    #[test]
    fn recipe_schema_uses_two_closed_branches() {
        let schema = bundle_schema();
        let recipes =
            &schema["properties"]["spec"]["properties"]["recipes"]["additionalProperties"];
        let branches = recipes["oneOf"].as_array().unwrap();
        assert_eq!(branches.len(), 2);
        assert_eq!(branches[0]["properties"]["type"]["const"], "entry_exit");
        assert!(branches[0]["properties"].get("allocator").is_none());
        assert_eq!(branches[1]["properties"]["type"]["const"], "target_weights");
        for field in ["entry", "exit", "sizing"] {
            assert!(branches[1]["properties"].get(field).is_none(), "{field}");
        }
        assert!(
            branches
                .iter()
                .all(|branch| branch["additionalProperties"] == false)
        );
    }
}
