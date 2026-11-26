use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

use crate::indicators::IndicatorError;

use super::catalog::is_historical_research;
use super::parse::{Parsed, parse};
use super::{
    Availability, Bars, Binding, Catalog, Component, Diagnostic, InputFormat, Universe,
    VersionBindings, child_path, component, diagnostic, schema,
};

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct CompiledPlan<'a> {
    pub plan_version: &'static str,
    pub content_sha256: String,
    pub source_sha256: String,
    pub bundle: Value,
    pub inputs: ResolvedInputs<'a>,
    pub factors: Vec<CompiledComponent>,
    pub modules: Vec<CompiledComponent>,
    pub execution: &'static str,
    pub sleeves: Vec<CompiledSleeve>,
    pub conflict_rules: ConflictRules,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct ResolvedInputs<'a> {
    pub universe: Universe<'a>,
    pub bars: Bars<'a>,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct CompiledComponent {
    pub local_id: String,
    pub component: &'static str,
    pub kind: &'static str,
    /// Explicit output selection for an indicator used as a scalar factor.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    pub depends_on: Vec<String>,
    pub warmup_sessions: u64,
    pub available_at: Availability,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct CompiledSleeve {
    pub recipe: String,
    pub priority: u64,
    pub budget_weight: String,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct ConflictRules {
    pub decision_snapshot: &'static str,
    pub phase_order: [&'static str; 4],
    pub on_entry_exit_collision: &'static str,
    pub same_session_reentry: bool,
    pub on_missing_execution_price: &'static str,
    pub missing_input: &'static str,
    pub target_constraints_order: [&'static str; 3],
    pub position_change_order: [&'static str; 2],
    pub priority: &'static str,
    pub equal_priority: &'static str,
    pub whole_lot_remainder_order: [&'static str; 2],
    pub duplicate_intents: &'static str,
    pub sleeve_accounting: &'static str,
}

impl ConflictRules {
    fn v1(same_session_reentry: bool) -> Self {
        Self {
            decision_snapshot: "frozen_data_cash_and_positions",
            phase_order: [
                "check_data_calendar_and_account_constraints",
                "apply_global_risk_forced_reductions_and_exits",
                "apply_filters_then_entries_and_targets",
                "record_netting_rejections_deferrals_and_simulated_fills",
            ],
            on_entry_exit_collision: "exit_first",
            same_session_reentry,
            on_missing_execution_price: "defer_and_report",
            missing_input: "block_new_risk",
            target_constraints_order: [
                "sleeve_budgets",
                "global_gross_limit",
                "global_asset_limit",
            ],
            position_change_order: ["reduce", "increase"],
            priority: "ascending_when_constrained",
            equal_priority: "proportional_to_budget_weight",
            whole_lot_remainder_order: ["instrument_id", "recipe_id"],
            duplicate_intents: "apply_once",
            sleeve_accounting: "virtual_sleeves_net_by_instrument_and_apportion_costs",
        }
    }
}

/// Compile against trusted catalog metadata and bind fully validated content to its version.
pub fn compile<'a>(
    bytes: &[u8],
    format: InputFormat,
    catalog: &Catalog<'a>,
    bindings: &mut dyn VersionBindings,
) -> Result<CompiledPlan<'a>, Diagnostic> {
    let mut parsed = parse(bytes, format)?;
    schema::validate(&parsed)?;
    if is_historical_research(string(&parsed.value["metadata"]["id"])) {
        return Err(diagnostic(
            &parsed,
            "/metadata/id",
            "RESEARCH_ONLY_STRATEGY",
            "This strategy ID is reserved for historical research and cannot compile to a backtest plan.",
        ));
    }

    let spec = &parsed.value["spec"];
    let inputs = resolve_inputs(&parsed, spec, catalog)?;
    let engine = string(&spec["engine"]);
    let asset = inputs.universe.asset;
    let warmup = integer(&spec["clock"]["warmup_sessions"]);
    let factors = resolve_factors(
        &parsed,
        spec,
        engine,
        asset,
        warmup,
        inputs.bars.available_at,
    )?;
    let modules = resolve_modules(&parsed, spec, engine, asset, &factors)?;
    validate_recipes(&parsed, spec)?;
    validate_portfolio(&parsed, spec)?;
    let execution = resolve_execution(&parsed, spec, engine, asset)?;
    let conflict_rules =
        ConflictRules::v1(spec["policy"]["same_session_reentry"].as_bool().unwrap());

    schema::normalize(&mut parsed.value);
    parsed.value.sort_all_objects();
    let canonical = serde_json::to_vec(&parsed.value).expect("JSON tree is serializable");
    let content_sha256 = sha256(&canonical);
    let metadata = &parsed.value["metadata"];
    let binding = bindings
        .bind(
            string(&metadata["id"]),
            string(&metadata["version"]),
            &content_sha256,
        )
        .map_err(|_| {
            diagnostic(
                &parsed,
                "/metadata/version",
                "BINDING_FAILED",
                "Could not store the strategy version binding.",
            )
        })?;
    if matches!(binding, Binding::Conflict { .. }) {
        return Err(diagnostic(
            &parsed,
            "/metadata/version",
            "VERSION_CONFLICT",
            "Different content is already bound to this strategy ID and version. Choose a new version to change the content.",
        ));
    }

    Ok(CompiledPlan {
        plan_version: "fertile.finance/strategy-plan/v1",
        content_sha256,
        source_sha256: sha256(bytes),
        sleeves: ordered_sleeves(&parsed.value["spec"]["portfolio"]["sleeves"]),
        bundle: parsed.value,
        inputs,
        factors,
        modules,
        execution,
        conflict_rules,
    })
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn string(value: &Value) -> &str {
    value.as_str().expect("validated string")
}

fn integer(value: &Value) -> u64 {
    value.as_u64().expect("validated non-negative integer")
}

fn object(value: &Value) -> &Map<String, Value> {
    value.as_object().expect("validated object")
}

fn array(value: &Value) -> &[Value] {
    value.as_array().expect("validated array")
}

fn decimal(value: &Value) -> Decimal {
    Decimal::from_str_exact(string(value)).expect("validated exact decimal")
}

fn resolve_inputs<'a>(
    parsed: &Parsed,
    spec: &Value,
    catalog: &Catalog<'a>,
) -> Result<ResolvedInputs<'a>, Diagnostic> {
    let calendar = string(&spec["clock"]["calendar"]);
    if !catalog.calendars.contains(&calendar) {
        return Err(diagnostic(
            parsed,
            "/spec/clock/calendar",
            "UNKNOWN_CATALOG_REF",
            "The calendar reference must identify a registered calendar.",
        ));
    }
    let universe_id = string(&spec["universe"]["ref"]);
    let universe = catalog
        .universes
        .iter()
        .find(|u| u.id == universe_id)
        .copied()
        .ok_or_else(|| {
            diagnostic(
                parsed,
                "/spec/universe/ref",
                "UNKNOWN_CATALOG_REF",
                "The universe reference must identify a registered universe.",
            )
        })?;
    let bars_id = string(&spec["data"]["bars"]);
    let bars = catalog
        .bars
        .iter()
        .find(|b| b.id == bars_id)
        .copied()
        .ok_or_else(|| {
            diagnostic(
                parsed,
                "/spec/data/bars",
                "UNKNOWN_CATALOG_REF",
                "The bar reference must identify a registered bar series.",
            )
        })?;
    for (path, actual) in [
        ("/spec/universe/ref", universe.calendar),
        ("/spec/data/bars", bars.calendar),
    ] {
        if actual != calendar {
            return Err(diagnostic(
                parsed,
                path,
                "INCOMPATIBLE_CALENDAR",
                "The registered input must use the decision calendar.",
            ));
        }
    }
    let currency = string(&spec["data"]["currency"]);
    if currency != "USD" || currency != universe.currency || currency != bars.currency {
        return Err(diagnostic(
            parsed,
            "/spec/data/currency",
            "INCOMPATIBLE_CURRENCY",
            "The cash equity engine requires USD for the data currency, registered universe, and registered bar series.",
        ));
    }
    if spec["portfolio"]["initial_cash"]["currency"] != currency {
        return Err(diagnostic(
            parsed,
            "/spec/portfolio/initial_cash/currency",
            "INCOMPATIBLE_CURRENCY",
            "Initial cash must use the bar currency.",
        ));
    }
    if universe.asset != bars.asset {
        return Err(diagnostic(
            parsed,
            "/spec/data/bars",
            "INCOMPATIBLE_ASSET",
            "The bar series and universe must declare the same asset type.",
        ));
    }
    if !universe.point_in_time {
        return Err(diagnostic(
            parsed,
            "/spec/universe/ref",
            "MEMBERSHIP_NOT_POINT_IN_TIME",
            "The registered universe must provide point-in-time membership.",
        ));
    }
    check_availability(
        parsed,
        "/spec/universe/ref",
        universe.available_at,
        Availability::SessionOpen,
    )?;
    check_availability(
        parsed,
        "/spec/data/bars",
        bars.available_at,
        Availability::SessionClose,
    )?;
    for (field, actual) in [
        ("timeframe", bars.timeframe),
        ("price_basis", bars.price_basis),
    ] {
        if spec["data"][field] != actual {
            return Err(diagnostic(
                parsed,
                &child_path("/spec/data", field),
                "INCOMPATIBLE_DATA",
                "The configured value must match the bar series registration.",
            ));
        }
    }
    Ok(ResolvedInputs { universe, bars })
}

fn check_availability(
    parsed: &Parsed,
    path: &str,
    available_at: Availability,
    deadline: Availability,
) -> Result<(), Diagnostic> {
    if available_at > deadline {
        return Err(diagnostic(
            parsed,
            path,
            "FUTURE_DATA",
            "The input becomes available after the required session cutoff.",
        ));
    }
    Ok(())
}

fn registered(
    parsed: &Parsed,
    path: &str,
    id: &str,
    engine: &str,
    asset: &str,
) -> Result<&'static Component, Diagnostic> {
    let definition = component(id).ok_or_else(|| {
        diagnostic(
            parsed,
            path,
            "UNKNOWN_COMPONENT",
            "The component reference must identify a registered component and version.",
        )
    })?;
    if !definition.engines.contains(&engine) || !definition.assets.contains(&asset) {
        return Err(diagnostic(
            parsed,
            path,
            "INCOMPATIBLE_COMPONENT",
            "The selected component must support the configured asset type and engine.",
        ));
    }
    if definition.supports_short {
        return Err(diagnostic(
            parsed,
            path,
            "UNSUPPORTED_SHORT",
            "The selected component can introduce short exposure in the long-only cash engine.",
        ));
    }
    Ok(definition)
}

fn slot_mismatch(parsed: &Parsed, path: &str) -> Diagnostic {
    diagnostic(
        parsed,
        path,
        "SLOT_MISMATCH",
        "The referenced component kind does not match this slot.",
    )
}

fn resolve_factors(
    parsed: &Parsed,
    spec: &Value,
    engine: &str,
    asset: &str,
    warmup: u64,
    bars_availability: Availability,
) -> Result<Vec<CompiledComponent>, Diagnostic> {
    let factors = object(&spec["factors"]);
    let mut graph = BTreeMap::new();
    for (key, value) in factors {
        let path = child_path("/spec/factors", key);
        let use_path = child_path(&path, "use");
        let definition = registered(parsed, &use_path, string(&value["use"]), engine, asset)?;
        if !matches!(definition.kind, "factor" | "indicator") {
            return Err(slot_mismatch(parsed, &use_path));
        }
        let mut dependencies = BTreeSet::new();
        for (index, dependency) in value["depends_on"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
        {
            let dependency = string(dependency);
            let dependency_path = format!("{path}/depends_on/{index}");
            if !factors.contains_key(dependency) {
                return Err(diagnostic(
                    parsed,
                    &dependency_path,
                    "UNKNOWN_FACTOR",
                    "The factor reference must identify a factor in spec.factors.",
                ));
            }
            if !dependencies.insert(dependency) {
                return Err(diagnostic(
                    parsed,
                    &dependency_path,
                    "DUPLICATE_REFERENCE",
                    "Each factor may appear once in a dependency list.",
                ));
            }
        }
        graph.insert(key.as_str(), (definition, dependencies));
    }

    let mut indegree: BTreeMap<_, _> = graph
        .iter()
        .map(|(&id, (_, deps))| (id, deps.len()))
        .collect();
    let mut dependents: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (&id, (_, deps)) in &graph {
        for &dep in deps {
            dependents.entry(dep).or_default().push(id);
        }
    }
    let mut ready: BTreeSet<_> = indegree
        .iter()
        .filter_map(|(&id, &count)| (count == 0).then_some(id))
        .collect();
    let mut resolved = BTreeMap::<&str, (Availability, u64)>::new();
    let mut order = Vec::with_capacity(graph.len());
    while let Some(id) = ready.pop_first() {
        let (definition, deps) = &graph[id];
        let path = child_path("/spec/factors", id);
        let factor = &factors[id];
        let mut available_at = definition.availability.max(bars_availability);
        let mut warmup_sessions = definition
            .checked_warmup_sessions(&factor["params"])
            .map_err(|error| match error {
                IndicatorError::MacdPeriodOrder => diagnostic(
                    parsed,
                    &format!("{path}/params/fast_period"),
                    "INVALID_VALUE",
                    "The MACD fast period must be smaller than the slow period.",
                ),
                _ => diagnostic(
                    parsed,
                    &format!("{path}/params"),
                    "INVALID_VALUE",
                    "The indicator parameters do not match its registered definition.",
                ),
            })?;
        for dependency in deps {
            let (dependency_time, dependency_warmup) = resolved[dependency];
            available_at = available_at.max(dependency_time);
            warmup_sessions = warmup_sessions.max(dependency_warmup);
        }
        check_availability(
            parsed,
            &format!("{path}/use"),
            available_at,
            Availability::SessionClose,
        )?;
        if warmup_sessions > warmup {
            return Err(diagnostic(
                parsed,
                "/spec/clock/warmup_sessions",
                "INSUFFICIENT_WARMUP",
                "The configured warmup must cover the prior sessions required by every factor.",
            ));
        }
        resolved.insert(id, (available_at, warmup_sessions));
        order.push(CompiledComponent {
            local_id: id.to_owned(),
            component: definition.id,
            kind: definition.kind,
            output: factor["output"].as_str().map(str::to_owned),
            depends_on: deps.iter().map(|id| (*id).to_owned()).collect(),
            warmup_sessions,
            available_at,
        });
        for &dependent in dependents.get(id).into_iter().flatten() {
            let remaining = indegree.get_mut(dependent).expect("registered factor");
            *remaining -= 1;
            if *remaining == 0 {
                ready.insert(dependent);
            }
        }
    }

    if let Some(mut id) = indegree
        .iter()
        .find_map(|(&id, &count)| (count > 0).then_some(id))
    {
        // Every blocked node still has a blocked prerequisite. Following those
        // edges until one repeats locates an edge in the actual cycle.
        let next = |id| {
            *graph[id]
                .1
                .iter()
                .find(|dep| indegree[*dep] > 0)
                .expect("blocked prerequisite")
        };
        let mut visited = BTreeSet::new();
        while visited.insert(id) {
            id = next(id);
        }
        let dependency = next(id);
        let index = array(&factors[id]["depends_on"])
            .iter()
            .position(|dep| dep == dependency)
            .expect("cycle edge");
        return Err(diagnostic(
            parsed,
            &format!("{}/depends_on/{index}", child_path("/spec/factors", id)),
            "DEPENDENCY_CYCLE",
            "The dependency creates a cycle in the factor graph.",
        ));
    }
    Ok(order)
}

fn resolve_modules(
    parsed: &Parsed,
    spec: &Value,
    engine: &str,
    asset: &str,
    factors: &[CompiledComponent],
) -> Result<Vec<CompiledComponent>, Diagnostic> {
    let factors: BTreeMap<_, _> = factors
        .iter()
        .map(|factor| (factor.local_id.as_str(), factor))
        .collect();
    let mut modules = Vec::new();
    for (id, module) in object(&spec["modules"]) {
        let path = child_path("/spec/modules", id);
        let use_path = child_path(&path, "use");
        let definition = registered(parsed, &use_path, string(&module["use"]), engine, asset)?;
        let kind = string(&module["kind"]);
        if kind != definition.kind {
            return Err(slot_mismatch(parsed, &use_path));
        }
        let params = &module["params"];
        let mut available_at = definition.availability;
        let mut warmup_sessions = definition.warmup_sessions(params);
        let mut depends_on = Vec::new();
        for parameter in definition.factor_parameters {
            let factor_id = string(&params[*parameter]);
            let factor = factors.get(factor_id).ok_or_else(|| {
                diagnostic(
                    parsed,
                    &child_path(&format!("{path}/params"), parameter),
                    "UNKNOWN_FACTOR",
                    "The factor reference must identify a factor in spec.factors.",
                )
            })?;
            available_at = available_at.max(factor.available_at);
            warmup_sessions = warmup_sessions.max(factor.warmup_sessions);
            depends_on.push(factor_id.to_owned());
        }
        check_availability(parsed, &use_path, available_at, Availability::SessionClose)?;
        validate_module_parameters(parsed, &path, definition, params)?;
        modules.push(CompiledComponent {
            local_id: id.clone(),
            component: definition.id,
            kind: definition.kind,
            output: None,
            depends_on,
            warmup_sessions,
            available_at,
        });
    }
    Ok(modules)
}

fn validate_module_parameters(
    parsed: &Parsed,
    path: &str,
    definition: &Component,
    params: &Value,
) -> Result<(), Diagnostic> {
    match definition.id {
        "sizing.equal_weight_capped@1.0.0" => check_weight(
            parsed,
            &format!("{path}/params/max_weight_per_asset"),
            &params["max_weight_per_asset"],
        ),
        "risk.long_only_cash@1.0.0" => {
            if params["allow_borrow"] == true {
                return Err(diagnostic(
                    parsed,
                    &format!("{path}/params/allow_borrow"),
                    "UNSUPPORTED_SHORT",
                    "The long-only cash engine prohibits borrowing and short positions.",
                ));
            }
            check_weight(
                parsed,
                &format!("{path}/params/max_gross_weight"),
                &params["max_gross_weight"],
            )
        }
        _ => Ok(()),
    }
}

fn validate_recipes(parsed: &Parsed, spec: &Value) -> Result<(), Diagnostic> {
    let modules = object(&spec["modules"]);
    let check_slot = |path: &str, reference: &Value, expected: &str| {
        let id = string(reference);
        let module = modules.get(id).ok_or_else(|| {
            diagnostic(
                parsed,
                path,
                "UNKNOWN_MODULE",
                "The module reference must identify a module in spec.modules.",
            )
        })?;
        let kind = string(&module["kind"]);
        if kind != expected {
            return Err(slot_mismatch(parsed, path));
        }
        Ok(())
    };
    for (id, recipe) in object(&spec["recipes"]) {
        let path = child_path("/spec/recipes", id);
        for slot in ["entry", "exit", "sizing", "allocator"] {
            if let Some(reference) = recipe.get(slot) {
                check_slot(&child_path(&path, slot), reference, slot)?;
            }
        }
        for (slot, kind) in [("filters", "filter"), ("risk", "risk")] {
            let mut listed = BTreeSet::new();
            for (index, reference) in array(&recipe[slot]).iter().enumerate() {
                let reference_path = format!("{path}/{slot}/{index}");
                check_slot(&reference_path, reference, kind)?;
                if !listed.insert(string(reference)) {
                    return Err(diagnostic(
                        parsed,
                        &reference_path,
                        "DUPLICATE_REFERENCE",
                        "Each module may appear once in this list.",
                    ));
                }
            }
        }
    }
    Ok(())
}

fn check_weight(parsed: &Parsed, path: &str, value: &Value) -> Result<(), Diagnostic> {
    if !(Decimal::ZERO..=Decimal::ONE).contains(&decimal(value)) {
        return Err(diagnostic(
            parsed,
            path,
            "OUT_OF_RANGE",
            "A cash weight must be between 0 and 1 inclusive.",
        ));
    }
    Ok(())
}

fn validate_portfolio(parsed: &Parsed, spec: &Value) -> Result<(), Diagnostic> {
    let portfolio = &spec["portfolio"];
    let amount = decimal(&portfolio["initial_cash"]["amount"]);
    if amount <= Decimal::ZERO || amount.normalize().scale() > 2 {
        return Err(diagnostic(
            parsed,
            "/spec/portfolio/initial_cash/amount",
            "OUT_OF_RANGE",
            "Initial USD cash must be positive and exactly representable in cents.",
        ));
    }
    for field in ["max_gross_weight", "max_weight_per_asset"] {
        check_weight(
            parsed,
            &child_path("/spec/portfolio", field),
            &portfolio[field],
        )?;
    }
    let mut allocated = Decimal::ZERO;
    let mut recipes = BTreeSet::new();
    for (index, sleeve) in array(&portfolio["sleeves"]).iter().enumerate() {
        let path = format!("/spec/portfolio/sleeves/{index}");
        let recipe = string(&sleeve["recipe"]);
        if spec["recipes"].get(recipe).is_none() {
            return Err(diagnostic(
                parsed,
                &format!("{path}/recipe"),
                "UNKNOWN_RECIPE",
                "The recipe reference must identify a recipe in spec.recipes.",
            ));
        }
        if !recipes.insert(recipe) {
            return Err(diagnostic(
                parsed,
                &format!("{path}/recipe"),
                "DUPLICATE_REFERENCE",
                "Each recipe may have one sleeve.",
            ));
        }
        let weight = decimal(&sleeve["budget_weight"]);
        let weight_path = format!("{path}/budget_weight");
        if weight < Decimal::ZERO {
            return Err(diagnostic(
                parsed,
                &weight_path,
                "BUDGET_OUT_OF_RANGE",
                "A sleeve budget must be non-negative.",
            ));
        }
        allocated += weight;
        if allocated > Decimal::ONE {
            return Err(diagnostic(
                parsed,
                &weight_path,
                "BUDGET_EXCEEDED",
                "The total of all sleeve budgets must be at most 1.",
            ));
        }
    }
    Ok(())
}

fn resolve_execution(
    parsed: &Parsed,
    spec: &Value,
    engine: &str,
    asset: &str,
) -> Result<&'static str, Diagnostic> {
    let execution = &spec["execution"];
    let model_path = "/spec/execution/model";
    let definition = registered(
        parsed,
        model_path,
        string(&execution["model"]),
        engine,
        asset,
    )?;
    if definition.kind != "execution" {
        return Err(slot_mismatch(parsed, model_path));
    }
    if definition.availability != Availability::NextSessionClose {
        return Err(diagnostic(
            parsed,
            model_path,
            "INCOMPATIBLE_EXECUTION_CLOCK",
            "The execution model must use the next session close.",
        ));
    }
    for field in ["commission_bps", "slippage_bps"] {
        if !(Decimal::ZERO..=Decimal::from(10_000)).contains(&decimal(&execution[field])) {
            return Err(diagnostic(
                parsed,
                &child_path("/spec/execution", field),
                "OUT_OF_RANGE",
                "Basis points must be between 0 and 10000 inclusive.",
            ));
        }
    }
    Ok(definition.id)
}

fn ordered_sleeves(sleeves: &Value) -> Vec<CompiledSleeve> {
    let mut sleeves: Vec<_> = array(sleeves)
        .iter()
        .map(|sleeve| CompiledSleeve {
            recipe: string(&sleeve["recipe"]).to_owned(),
            priority: integer(&sleeve["priority"]),
            budget_weight: string(&sleeve["budget_weight"]).to_owned(),
        })
        .collect();
    sleeves.sort_unstable_by(|left, right| {
        left.priority
            .cmp(&right.priority)
            .then_with(|| left.recipe.cmp(&right.recipe))
    });
    sleeves
}
