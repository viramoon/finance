use finance_api::indicators::{IndicatorSpec, registry as indicator_registry};
use finance_api::strategy_bundle::{
    Availability, Binding, BindingError, Catalog, CompiledPlan, Diagnostic, InputFormat, Location,
    MAX_BYTES, MemoryBindings, VersionBindings, bundle_schema, compile, component_registry,
    example_catalog,
};
use serde_json::{Value, json};
use std::{ffi::OsStr, fs, path::Path, process::Command};

macro_rules! fixture {
    ($name:literal) => {
        include_str!(concat!(
            "../../../strategies/fixtures/strategy-bundle/v1/",
            $name
        ))
    };
}

const ENTRY_EXIT: &str = fixture!("entry-exit.yaml");
const TARGET_WEIGHTS: &str = fixture!("target-weights.yaml");
const PRIVATE_MARKER: &str = "synthprivatetoken";
const PRIVATE_NUMBER: &str = "927561734286153592";
const PRIVATE_BUDGET_DIGITS: &str = "27561734286153592";

fn compile_yaml(source: &str) -> Result<CompiledPlan<'static>, Diagnostic> {
    compile(
        source.as_bytes(),
        InputFormat::Yaml,
        &example_catalog(),
        &mut MemoryBindings::default(),
    )
}

fn assert_diagnostic(
    result: Result<CompiledPlan<'_>, Diagnostic>,
    source: &str,
    code: &str,
    path: &str,
    at: &str,
) -> Diagnostic {
    let error = result.expect_err(code);
    assert_eq!(error.code, code, "{error}");
    assert_eq!(error.path, path, "{error}");
    assert!(!error.message.trim().is_empty());
    let (before, _) = source.split_once(at).expect("diagnostic source marker");
    let line = before.bytes().filter(|byte| *byte == b'\n').count() + 1;
    let column = before.rsplit('\n').next().unwrap().chars().count() + 1;
    assert_eq!(error.location, Location { line, column }, "{error}");
    error
}

fn assert_private_text_absent(output: &str, private: &[&str]) {
    for text in private {
        assert!(!output.contains(text), "diagnostic exposed synthetic input");
    }
}

fn assert_redacted_diagnostic(error: &Diagnostic, private: &[&str]) {
    for rendering in [
        serde_json::to_string(error).unwrap(),
        error.to_string(),
        format!("{error:?}"),
    ] {
        assert_private_text_absent(&rendering, private);
    }
}

fn assert_redacted_compile_failure(
    file: &Path,
    source: &str,
    format: InputFormat,
    expected: (&str, &str, &str),
    private: &[&str],
) {
    let (code, path, at) = expected;
    let result = compile(
        source.as_bytes(),
        format,
        &example_catalog(),
        &mut MemoryBindings::default(),
    );
    let error = assert_diagnostic(result, source, code, path, at);
    assert_redacted_diagnostic(&error, private);

    fs::write(file, source).unwrap();
    let format = match format {
        InputFormat::Yaml => "yaml",
        InputFormat::Json => "json",
    };
    let output = Command::new(env!("CARGO_BIN_EXE_finance-strategy"))
        .args(["compile", "--format", format])
        .arg(file)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let diagnostic = std::str::from_utf8(&output.stderr).unwrap();
    assert_private_text_absent(diagnostic, private);
    assert_eq!(
        serde_json::from_str::<Value>(diagnostic).unwrap(),
        serde_json::to_value(error).unwrap()
    );
}

#[test]
fn compiles_a_b_and_a1_b2_from_the_complete_example() {
    for (recipe, entry, exit) in [("A", "A1", "A2"), ("B", "B1", "B2"), ("A1_B2", "A1", "B2")] {
        let source = ENTRY_EXIT.replace("recipe: A1_B2,", &format!("recipe: {recipe},"));
        let plan = compile_yaml(&source).unwrap();
        let spec = &plan.bundle["spec"];
        assert_eq!(plan.plan_version, "fertile.finance/strategy-plan/v1");
        assert_eq!(plan.sleeves.len(), 1);
        assert_eq!(plan.sleeves[0].recipe, recipe);
        assert_eq!(plan.sleeves[0].budget_weight, "1");
        assert_eq!(spec["recipes"][recipe]["entry"], entry);
        assert_eq!(spec["recipes"][recipe]["exit"], exit);
        assert_eq!(spec["portfolio"]["initial_cash"]["amount"], "100000");
        assert_eq!(spec["modules"]["A2"]["params"]["threshold"], "0");
        assert_eq!(plan.inputs.universe.calendar, "XNYS");
        assert_eq!(plan.inputs.bars.available_at, Availability::SessionClose);
        assert_eq!(plan.execution, "execution.next_close_cash_equity@1.0.0");
    }
}

fn with_indicator(id: &str, params: &Value, output: Option<&str>) -> String {
    let selection = output.map_or_else(String::new, |name| format!("\n      output: {name}"));
    ENTRY_EXIT.replace(
        "use: \"indicator.rsi_wilder@1.0.0\"\n      params: {period: 14, field: close}",
        &format!("use: {id:?}\n      params: {params}{selection}"),
    )
}

#[test]
fn compiles_every_registered_indicator_with_shared_warmup_and_output_selection() {
    let registry = component_registry();
    let components = registry["components"].as_array().unwrap();
    let cases = [
        ("sma", json!({"period": 4}), None, 3),
        ("ema", json!({"period": 4}), None, 3),
        (
            "rsi_wilder",
            json!({"period": 4, "field": "close"}),
            None,
            4,
        ),
        ("bollinger", json!({"period": 4}), Some("lower"), 3),
        (
            "macd",
            json!({"fast_period": 8, "slow_period": 20, "signal_period": 7}),
            Some("histogram"),
            25,
        ),
        ("atr", json!({"period": 4}), None, 3),
        ("vwap", json!({}), None, 0),
        ("close_return", json!({"period": 4}), None, 4),
        ("realized_volatility", json!({"period": 4}), None, 4),
        ("volume_sma", json!({"period": 4}), Some("value"), 3),
    ];
    assert_eq!(cases.len(), indicator_registry().len());
    for (name, params, output, warmup) in cases {
        let id = format!("indicator.{name}@1.0.0");
        let core = IndicatorSpec::new(&id, &params).unwrap();
        assert_eq!(core.prior_bars(), warmup, "{id}");
        let declaration = components.iter().find(|value| value["id"] == id).unwrap();
        assert_eq!(declaration["outputs"], json!(core.definition().outputs));

        let plan = compile_yaml(&with_indicator(&id, &params, output)).unwrap();
        let factor = plan
            .factors
            .iter()
            .find(|factor| factor.local_id == "rsi14")
            .unwrap();
        assert_eq!(factor.component, id);
        assert_eq!(factor.kind, "indicator");
        assert_eq!(factor.available_at, Availability::SessionClose);
        assert_eq!(factor.warmup_sessions, warmup);
        assert_eq!(factor.output.as_deref(), output);
        let entry = plan
            .modules
            .iter()
            .find(|module| module.local_id == "B1")
            .unwrap();
        assert_eq!(entry.warmup_sessions, warmup);
        let factor = serde_json::to_value(factor).unwrap();
        if let Some(output) = output {
            assert_eq!(factor["output"], output);
        } else {
            assert!(factor.get("output").is_none());
        }
    }
}

#[test]
fn macd_requires_a_registered_output_valid_period_order_and_enough_prior_sessions() {
    const ID: &str = "indicator.macd@1.0.0";
    let valid = json!({"fast_period": 8, "slow_period": 20, "signal_period": 7});
    let missing = compile_yaml(&with_indicator(ID, &valid, None)).unwrap_err();
    assert_eq!(missing.code, "REQUIRED_FIELD");
    assert_eq!(missing.path, "/spec/factors/<key>/output");
    let invalid = with_indicator(ID, &valid, Some("private_column"));
    let error = assert_diagnostic(
        compile_yaml(&invalid),
        &invalid,
        "INVALID_VALUE",
        "/spec/factors/<key>/output",
        "private_column",
    );
    assert_redacted_diagnostic(&error, &["private_column"]);
    let equal_periods = with_indicator(
        ID,
        &json!({"fast_period": 20, "slow_period": 20, "signal_period": 7}),
        Some("signal"),
    );
    assert_diagnostic(
        compile_yaml(&equal_periods),
        &equal_periods,
        "INVALID_VALUE",
        "/spec/factors/<key>/params/fast_period",
        "20,\"signal_period\"",
    );
    let insufficient = with_indicator(ID, &valid, Some("macd"))
        .replace("warmup_sessions: 30", "warmup_sessions: 24");
    assert_diagnostic(
        compile_yaml(&insufficient),
        &insufficient,
        "INSUFFICIENT_WARMUP",
        "/spec/clock/warmup_sessions",
        "24\n    missing_input",
    );
}

#[test]
fn compiles_target_weights_from_registered_factor_allocator() {
    let plan = compile_yaml(TARGET_WEIGHTS).unwrap();
    let recipe = &plan.bundle["spec"]["recipes"]["topFive"];
    assert_eq!(recipe["type"], "target_weights");
    assert_eq!(recipe["allocator"], "top_five");
    assert_eq!(plan.sleeves[0].recipe, "topFive");
    let allocator = plan
        .modules
        .iter()
        .find(|item| item.local_id == "top_five")
        .unwrap();
    assert_eq!(allocator.component, "allocator.top_n@1.0.0");
    assert_eq!(allocator.depends_on, ["mom20"]);
    assert_eq!(allocator.warmup_sessions, 20);
}

#[test]
fn reports_closed_structure_types_and_parameter_bounds_at_yaml_values() {
    for (from, to, code, path, at) in [
        (
            "warmup_sessions: 30",
            "warmup_sessions: \"30\"",
            "TYPE_MISMATCH",
            "/spec/clock/warmup_sessions",
            "\"30\"\n    missing_input",
        ),
        (
            "timeframe: 1d",
            "timeframe: 1d\n    unexpected: value",
            "UNKNOWN_FIELD",
            "/spec/data/<key>",
            "value\n    price_basis",
        ),
        (
            "lookback_sessions: 20",
            "lookback_sessions: 2521",
            "OUT_OF_RANGE",
            "/spec/factors/<key>/params/lookback_sessions",
            "2521}",
        ),
        (
            "lookback_sessions: 20",
            "lookback_sessions: 20, expression: ignored",
            "UNKNOWN_FIELD",
            "/spec/factors/<key>/params/<key>",
            "ignored}",
        ),
        (
            "threshold: \"0.02\"",
            "threshold: \"2e-2\"",
            "INVALID_VALUE",
            "/spec/modules/<key>/params/threshold",
            "\"2e-2\"",
        ),
        (
            "threshold: \"0.02\"",
            "threshold: 0.02",
            "TYPE_MISMATCH",
            "/spec/modules/<key>/params/threshold",
            "0.02}",
        ),
        (
            "tie_break: stable_instrument_id",
            "tie_break: random",
            "INVALID_VALUE",
            "/spec/policy/tie_break",
            "random\n",
        ),
    ] {
        let source = ENTRY_EXIT.replacen(from, to, 1);
        assert_diagnostic(compile_yaml(&source), &source, code, path, at);
    }
}

#[test]
fn recipe_branches_reject_each_others_fields() {
    let source = ENTRY_EXIT.replacen(
        "type: entry_exit, entry:",
        "type: entry_exit, allocator: Extra, entry:",
        1,
    );
    assert_diagnostic(
        compile_yaml(&source),
        &source,
        "UNKNOWN_FIELD",
        "/spec/recipes/<key>/allocator",
        "Extra,",
    );
    for field in ["entry", "exit", "sizing"] {
        let source = TARGET_WEIGHTS.replacen(
            "allocator: top_five",
            &format!("allocator: top_five\n      {field}: Extra"),
            1,
        );
        assert_diagnostic(
            compile_yaml(&source),
            &source,
            "UNKNOWN_FIELD",
            &format!("/spec/recipes/<key>/{field}"),
            "Extra\n",
        );
    }
}

#[test]
fn reports_bad_references_slots_cycles_budget_warmup_and_cash_constraints() {
    for (from, to, code, path, at) in [
        (
            "factor.close_return@1.0.0",
            "factor.close_return@9.0.0",
            "UNKNOWN_COMPONENT",
            "/spec/factors/<key>/use",
            "\"factor.close_return@9.0.0\"",
        ),
        (
            "entry: A1, exit: A2",
            "entry: A2, exit: A2",
            "SLOT_MISMATCH",
            "/spec/recipes/<key>/entry",
            "A2, exit: A2",
        ),
        (
            "entry: A1, exit: A2",
            "entry: Missing, exit: A2",
            "UNKNOWN_MODULE",
            "/spec/recipes/<key>/entry",
            "Missing, exit: A2",
        ),
        (
            "factor: mom20, threshold",
            "factor: absent, threshold",
            "UNKNOWN_FACTOR",
            "/spec/modules/<key>/params/factor",
            "absent, threshold",
        ),
        (
            "params: {lookback_sessions: 20}",
            "params: {lookback_sessions: 20}\n      depends_on: [mom20]",
            "DEPENDENCY_CYCLE",
            "/spec/factors/<key>/depends_on/0",
            "mom20]\n",
        ),
        (
            "budget_weight: \"1.00\", priority: 10",
            "budget_weight: \"-0.01\", priority: 10",
            "BUDGET_OUT_OF_RANGE",
            "/spec/portfolio/sleeves/0/budget_weight",
            "\"-0.01\"",
        ),
        (
            "budget_weight: \"1.00\", priority: 10}",
            "budget_weight: \"0.6\", priority: 10}\n      - {recipe: B, budget_weight: \"0.5\", priority: 5}",
            "BUDGET_EXCEEDED",
            "/spec/portfolio/sleeves/1/budget_weight",
            "\"0.5\"",
        ),
        (
            "warmup_sessions: 30",
            "warmup_sessions: 19",
            "INSUFFICIENT_WARMUP",
            "/spec/clock/warmup_sessions",
            "19\n    missing_input",
        ),
        (
            "allow_borrow: false",
            "allow_borrow: true",
            "UNSUPPORTED_SHORT",
            "/spec/modules/<key>/params/allow_borrow",
            "true}\n\n  recipes",
        ),
        (
            "initial_cash: {currency: USD",
            "initial_cash: {currency: EUR",
            "INCOMPATIBLE_CURRENCY",
            "/spec/portfolio/initial_cash/currency",
            "EUR, amount",
        ),
        (
            "amount: \"100000.00\"",
            "amount: \"100000.001\"",
            "OUT_OF_RANGE",
            "/spec/portfolio/initial_cash/amount",
            "\"100000.001\"",
        ),
    ] {
        let source = ENTRY_EXIT.replacen(from, to, 1);
        assert_diagnostic(compile_yaml(&source), &source, code, path, at);
    }
}

#[test]
fn rejects_delayed_bars_and_catalog_calendar_mismatch() {
    let future = fixture!("future-data.yaml");
    assert_diagnostic(
        compile_yaml(future),
        future,
        "FUTURE_DATA",
        "/spec/data/bars",
        "\"example.delayed-us-daily@1.0.0\"",
    );
    let alternate = ENTRY_EXIT.replacen("calendar: XNYS", "calendar: XNAS", 1);
    let catalog = Catalog {
        calendars: &["XNYS", "XNAS"],
        ..example_catalog()
    };
    assert_diagnostic(
        compile(
            alternate.as_bytes(),
            InputFormat::Yaml,
            &catalog,
            &mut MemoryBindings::default(),
        ),
        &alternate,
        "INCOMPATIBLE_CALENDAR",
        "/spec/universe/ref",
        "\"example.us-cash-equities@1.0.0\"",
    );
}

#[test]
fn validates_and_resolves_dependencies_in_topological_order() {
    let source = ENTRY_EXIT
        .replacen(
            "params: {lookback_sessions: 20}",
            "params: {lookback_sessions: 20}\n      depends_on: [rsi14]",
            1,
        )
        .replacen("period: 14", "period: 25", 1);
    let plan = compile_yaml(&source).unwrap();
    assert_eq!(plan.factors[0].local_id, "rsi14");
    assert_eq!(plan.factors[1].local_id, "mom20");
    assert_eq!(plan.factors[1].depends_on, ["rsi14"]);
    assert_eq!(plan.factors[1].warmup_sessions, 25);
    assert_eq!(plan.modules[0].local_id, "A1");
    assert_eq!(plan.modules[0].depends_on, ["mom20"]);
    assert_eq!(plan.modules[0].warmup_sessions, 25);
}

#[test]
fn rejects_unsafe_yaml_and_duplicate_json_fixtures_at_the_recorded_location() {
    for (source, code, path, at) in [
        (
            fixture!("duplicate-key.txt"),
            "DUPLICATE_KEY",
            "/metadata/id",
            "id: example.second",
        ),
        (
            fixture!("yaml-anchor.txt"),
            "YAML_ANCHOR",
            "/metadata/id",
            "&bundle_id",
        ),
        (
            fixture!("yaml-alias.txt"),
            "YAML_ALIAS",
            "/metadata/title",
            "*bundle_title",
        ),
        (
            fixture!("yaml-tag.txt"),
            "YAML_TAG",
            "/metadata/id",
            "!load",
        ),
        (
            fixture!("yaml-multiple-documents.txt"),
            "MULTIPLE_DOCUMENTS",
            "",
            "---",
        ),
    ] {
        assert_diagnostic(compile_yaml(source), source, code, path, at);
    }
    let json_duplicates = fixture!("json-duplicate.json");
    assert_diagnostic(
        compile(
            json_duplicates.as_bytes(),
            InputFormat::Json,
            &example_catalog(),
            &mut MemoryBindings::default(),
        ),
        json_duplicates,
        "DUPLICATE_KEY",
        "/metadata/id",
        r#""\u0069d""#,
    );
    let oversized = " ".repeat(MAX_BYTES + 1);
    assert_diagnostic(
        compile_yaml(&oversized),
        &oversized,
        "INPUT_TOO_LARGE",
        "",
        " ",
    );
}

#[test]
fn preserves_deterministic_policy_priorities_and_plan_bytes() {
    let source = ENTRY_EXIT.replacen(
        "{recipe: A1_B2, budget_weight: \"1.00\", priority: 10}",
        "{recipe: B, budget_weight: \"0.25\", priority: 20}\n      - {recipe: A1_B2, budget_weight: \"0.50\", priority: 5}\n      - {recipe: A, budget_weight: \"0.25\", priority: 5}",
        1,
    );
    let first = compile_yaml(&source).unwrap();
    let second = compile_yaml(&source).unwrap();
    assert_eq!(first, second);
    assert_eq!(
        serde_json::to_vec(&first).unwrap(),
        serde_json::to_vec(&second).unwrap()
    );
    assert_eq!(first.content_sha256.len(), 64);
    assert_eq!(first.source_sha256.len(), 64);
    let sleeves: Vec<_> = first
        .sleeves
        .iter()
        .map(|s| (s.recipe.as_str(), s.priority, s.budget_weight.as_str()))
        .collect();
    assert_eq!(
        sleeves,
        [("A", 5, "0.25"), ("A1_B2", 5, "0.5"), ("B", 20, "0.25")]
    );
    let rules = first.conflict_rules;
    assert_eq!(
        rules.phase_order,
        [
            "check_data_calendar_and_account_constraints",
            "apply_global_risk_forced_reductions_and_exits",
            "apply_filters_then_entries_and_targets",
            "record_netting_rejections_deferrals_and_simulated_fills",
        ]
    );
    assert_eq!(rules.on_entry_exit_collision, "exit_first");
    assert!(!rules.same_session_reentry);
    assert_eq!(rules.on_missing_execution_price, "defer_and_report");
    assert_eq!(rules.missing_input, "block_new_risk");
    assert_eq!(
        rules.target_constraints_order,
        ["sleeve_budgets", "global_gross_limit", "global_asset_limit"]
    );
    assert_eq!(rules.position_change_order, ["reduce", "increase"]);
    assert_eq!(rules.priority, "ascending_when_constrained");
    assert_eq!(rules.equal_priority, "proportional_to_budget_weight");
    assert_eq!(
        rules.whole_lot_remainder_order,
        ["instrument_id", "recipe_id"]
    );
    assert_eq!(rules.duplicate_intents, "apply_once");
}

#[test]
fn binds_validated_versions_and_accepts_the_same_canonical_content_as_json() {
    let mut bindings = MemoryBindings::default();
    let catalog = example_catalog();
    let mut compile_bound =
        |source: &str, format| compile(source.as_bytes(), format, &catalog, &mut bindings);
    let invalid = ENTRY_EXIT.replacen("warmup_sessions: 30", "warmup_sessions: 0", 1);
    assert_eq!(
        compile_bound(&invalid, InputFormat::Yaml).unwrap_err().code,
        "INSUFFICIENT_WARMUP"
    );

    let first = compile_bound(ENTRY_EXIT, InputFormat::Yaml).unwrap();
    let reformatted = ENTRY_EXIT
        .replacen("apiVersion:", "# another representation\napiVersion:", 1)
        .replacen("\"100000.00\"", "\"100000.0\"", 1)
        .replacen("warmup_sessions: 30", "warmup_sessions: 3e1", 1);
    let reformatted = compile_bound(&reformatted, InputFormat::Yaml).unwrap();
    assert_eq!(first.content_sha256, reformatted.content_sha256);
    assert_ne!(first.source_sha256, reformatted.source_sha256);
    assert_eq!(first.bundle, reformatted.bundle);

    let json = serde_json::to_string_pretty(&first.bundle).unwrap();
    let json = compile_bound(&json, InputFormat::Json).unwrap();
    assert_eq!(first.content_sha256, json.content_sha256);
    assert_ne!(first.source_sha256, json.source_sha256);

    let changed = ENTRY_EXIT.replacen("\"0.02\"", "\"0.03\"", 1);
    let conflict = assert_diagnostic(
        compile_bound(&changed, InputFormat::Yaml),
        &changed,
        "VERSION_CONFLICT",
        "/metadata/version",
        "\"1.0.0\"",
    );
    let changed_plan = compile_yaml(&changed).unwrap();
    assert_redacted_diagnostic(
        &conflict,
        &[
            &first.content_sha256,
            &first.source_sha256,
            &changed_plan.content_sha256,
            &changed_plan.source_sha256,
        ],
    );
    let new_version = changed.replacen("version: \"1.0.0\"", "version: \"1.0.1\"", 1);
    assert_ne!(
        first.content_sha256,
        compile_bound(&new_version, InputFormat::Yaml)
            .unwrap()
            .content_sha256
    );
    assert_eq!(first, compile_bound(ENTRY_EXIT, InputFormat::Yaml).unwrap());
}

#[test]
fn schema_and_registry_snapshots_match_the_exporters() {
    let artifacts = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../strategies/schema");
    let schema = bundle_schema();
    assert_eq!(
        schema["$schema"],
        "https://json-schema.org/draft/2020-12/schema"
    );
    for (name, value) in [
        ("strategy-bundle-v1.schema.json", schema),
        ("component-registry-v1.json", component_registry()),
    ] {
        let path = artifacts.join(name);
        let file =
            fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        let expected = serde_json::to_string_pretty(&value).unwrap() + "\n";
        assert_eq!(file, expected, "regenerate {name} with finance-strategy");
        assert_eq!(serde_json::from_str::<Value>(&file).unwrap(), value);
    }
}

#[test]
fn documented_entry_exit_example_is_the_test_fixture() {
    let doc = include_str!("../../../docs/strategy-bundle.md");
    let (_, after_fence) = doc.split_once("```yaml\n").unwrap();
    let (documented, _) = after_fence.split_once("```\n").unwrap();
    assert_eq!(documented, ENTRY_EXIT);
}

#[test]
fn redacts_input_fields_references_and_numbers_in_library_and_cli_failures() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("strategy.yaml");
    for (before, after, code, path, at) in [
        (
            "calendar: XNYS",
            "calendar: synthprivatetoken.calendar",
            "UNKNOWN_CATALOG_REF",
            "/spec/clock/calendar",
            "synthprivatetoken.calendar",
        ),
        (
            "example.us-cash-equities@1.0.0",
            "synthprivatetoken.universe@7.3.9",
            "UNKNOWN_CATALOG_REF",
            "/spec/universe/ref",
            "\"synthprivatetoken.universe@7.3.9\"",
        ),
        (
            "example.reviewed-us-daily@1.0.0",
            "synthprivatetoken.bars@7.3.9",
            "UNKNOWN_CATALOG_REF",
            "/spec/data/bars",
            "\"synthprivatetoken.bars@7.3.9\"",
        ),
        (
            "mom20:\n      use: \"factor.close_return@1.0.0\"",
            "synthprivatetoken:\n      use: \"factor.synthprivatetoken@7.3.9\"",
            "UNKNOWN_COMPONENT",
            "/spec/factors/<key>/use",
            "\"factor.synthprivatetoken@7.3.9\"",
        ),
        (
            "factor: mom20, threshold",
            "factor: synthprivatetoken, threshold",
            "UNKNOWN_FACTOR",
            "/spec/modules/<key>/params/factor",
            "synthprivatetoken, threshold",
        ),
        (
            "A:     {type: entry_exit, entry: A1, exit: A2",
            "synthprivatetoken: {type: entry_exit, entry: synthprivatetoken, exit: A2",
            "UNKNOWN_MODULE",
            "/spec/recipes/<key>/entry",
            "synthprivatetoken, exit: A2",
        ),
        (
            "recipe: A1_B2,",
            "recipe: synthprivatetoken,",
            "UNKNOWN_RECIPE",
            "/spec/portfolio/sleeves/0/recipe",
            "synthprivatetoken,",
        ),
        (
            "params: {lookback_sessions: 20}",
            "params: {lookback_sessions: 20, \"synthprivatetoken~/\\u0085\": synthprivatetoken}",
            "UNKNOWN_FIELD",
            "/spec/factors/<key>/params/<key>",
            "synthprivatetoken}",
        ),
    ] {
        let source = ENTRY_EXIT.replacen(before, after, 1);
        assert_redacted_compile_failure(
            &file,
            &source,
            InputFormat::Yaml,
            (code, path, at),
            &[PRIVATE_MARKER, r"\u0085"],
        );
    }

    let source = ENTRY_EXIT.replacen(
        "warmup_sessions: 30",
        &format!("warmup_sessions: {PRIVATE_NUMBER}"),
        1,
    );
    assert_redacted_compile_failure(
        &file,
        &source,
        InputFormat::Yaml,
        (
            "OUT_OF_RANGE",
            "/spec/clock/warmup_sessions",
            PRIVATE_NUMBER,
        ),
        &[PRIVATE_NUMBER],
    );

    let source = ENTRY_EXIT.replacen(
        "budget_weight: \"1.00\", priority: 10}",
        "budget_weight: \"0.7\", priority: 10}\n      - {recipe: B, budget_weight: \"0.827561734286153592\", priority: 5}",
        1,
    );
    assert_redacted_compile_failure(
        &file,
        &source,
        InputFormat::Yaml,
        (
            "BUDGET_EXCEEDED",
            "/spec/portfolio/sleeves/1/budget_weight",
            "\"0.827561734286153592\"",
        ),
        &[PRIVATE_BUDGET_DIGITS],
    );
}

#[test]
fn redacts_parser_errors_before_schema_validation() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("strategy");
    for (source, format, code, path, at) in [
        (
            "spec:\n  \"synthprivatetoken~/\\u001b\":\n    id: synthprivatetoken-first\n    id: synthprivatetoken-second\n",
            InputFormat::Yaml,
            "DUPLICATE_KEY",
            "/spec/<key>/<key>",
            "id: synthprivatetoken-second",
        ),
        (
            "spec:\n  portfolio:\n    sleeves:\n      \"927561734286153592\": synthprivatetoken-first\n      \"927561734286153592\": synthprivatetoken-second\n",
            InputFormat::Yaml,
            "DUPLICATE_KEY",
            "/spec/portfolio/sleeves/<key>",
            "\"927561734286153592\": synthprivatetoken-second",
        ),
        (
            r#"{"spec":{"portfolio":{"sleeves":[{"927561734286153592":"synthprivatetoken","927561734286153592":2}]}}}"#,
            InputFormat::Json,
            "DUPLICATE_KEY",
            "/spec/portfolio/sleeves/0/<key>",
            r#""927561734286153592":2"#,
        ),
        (
            r#"{"metadata":{"synthprivatetoken~/\u0085":1,"synthprivatetoken~/\u0085": 2}}"#,
            InputFormat::Json,
            "DUPLICATE_KEY",
            "/metadata/<key>",
            r#""synthprivatetoken~/\u0085": 2"#,
        ),
        (
            "metadata:\n  title: \"synthprivatetoken \\q\"\n",
            InputFormat::Yaml,
            "YAML_SYNTAX",
            "/metadata/title",
            "\\q\"",
        ),
        (
            r#"{"metadata":{"title": synthprivatetoken}}"#,
            InputFormat::Json,
            "JSON_SYNTAX",
            "/metadata/title",
            "synthprivatetoken",
        ),
        (
            r#"{"metadata":{"$ref":"https://synthprivatetoken.invalid"}}"#,
            InputFormat::Json,
            "EXTERNAL_REFERENCE",
            "/metadata/<key>",
            r#""$ref""#,
        ),
    ] {
        assert_redacted_compile_failure(
            &file,
            source,
            format,
            (code, path, at),
            &[PRIVATE_MARKER, PRIVATE_NUMBER, r"\u001b", r"\u0085"],
        );
    }
}

#[test]
fn binding_backend_error_text_stays_out_of_the_public_diagnostic() {
    struct BackendWithPrivateError;

    impl VersionBindings for BackendWithPrivateError {
        fn bind(&mut self, _: &str, _: &str, _: &str) -> Result<Binding, BindingError> {
            Err(std::io::Error::other(PRIVATE_MARKER).into())
        }
    }

    let error = assert_diagnostic(
        compile(
            ENTRY_EXIT.as_bytes(),
            InputFormat::Yaml,
            &example_catalog(),
            &mut BackendWithPrivateError,
        ),
        ENTRY_EXIT,
        "BINDING_FAILED",
        "/metadata/version",
        "\"1.0.0\"",
    );
    assert_redacted_diagnostic(&error, &[PRIVATE_MARKER]);
}

#[test]
fn cli_outputs_plan_json_and_preserves_bindings_across_invocations() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("strategy.yaml");
    let bindings = directory.path().join("versions.sqlite3");
    let run = || {
        Command::new(env!("CARGO_BIN_EXE_finance-strategy"))
            .args(["compile", "--bindings"])
            .arg(&bindings)
            .arg(&config)
            .output()
            .unwrap()
    };

    fs::write(&config, ENTRY_EXIT).unwrap();
    let first = run();
    assert!(first.status.success());
    assert!(first.stderr.is_empty());
    let expected = serde_json::to_value(compile_yaml(ENTRY_EXIT).unwrap()).unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&first.stdout).unwrap(),
        expected
    );
    assert_eq!(run().stdout, first.stdout);

    let changed = ENTRY_EXIT.replacen("\"0.02\"", "\"0.03\"", 1).replacen(
        "title: \"日线动量入场与持有期退出\"",
        &format!("title: \"{PRIVATE_MARKER}\""),
        1,
    );
    fs::write(&config, &changed).unwrap();
    let conflict = run();
    assert_eq!(conflict.status.code(), Some(2));
    assert!(conflict.stdout.is_empty());
    let changed_plan = compile_yaml(&changed).unwrap();
    let private = [
        PRIVATE_MARKER,
        expected["content_sha256"].as_str().unwrap(),
        expected["source_sha256"].as_str().unwrap(),
        &changed_plan.content_sha256,
        &changed_plan.source_sha256,
    ];
    let stderr = std::str::from_utf8(&conflict.stderr).unwrap();
    assert_private_text_absent(stderr, &private);
    let diagnostic: Value = serde_json::from_str(stderr).unwrap();
    assert_eq!(diagnostic["path"], "/metadata/version");
    assert_eq!(diagnostic["code"], "VERSION_CONFLICT");
}

#[test]
fn cli_setup_errors_omit_local_file_and_binding_paths() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("strategy.yaml");
    let missing_parent = directory.path().join(PRIVATE_MARKER);
    let bindings = missing_parent.join("versions.sqlite3");
    fs::write(&config, ENTRY_EXIT).unwrap();
    let binding_path = bindings.as_os_str();
    let binding_options = [OsStr::new("--bindings"), binding_path, config.as_os_str()];
    for options in [&[binding_path][..], &binding_options[..]] {
        let output = Command::new(env!("CARGO_BIN_EXE_finance-strategy"))
            .arg("compile")
            .args(options)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        let diagnostic = std::str::from_utf8(&output.stderr).unwrap();
        assert!(!diagnostic.trim().is_empty());
        assert_private_text_absent(diagnostic, &[PRIVATE_MARKER]);
    }
}
