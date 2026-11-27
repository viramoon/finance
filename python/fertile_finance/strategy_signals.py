"""Generate close-time StrategyBundle targets from explicit state snapshots."""

from collections import defaultdict
from collections.abc import Mapping
from dataclasses import dataclass
from datetime import date
from decimal import Decimal, localcontext
from fractions import Fraction
from graphlib import TopologicalSorter
from typing import TYPE_CHECKING

from .portfolio_inputs import DataGap, ResearchError
from .research_decimal import EXACT_RESEARCH_CONTEXT
from .strategy_allocation import Claim, Limit, allocate
from .strategy_indicators import IndicatorEvaluator, RustIndicatorEvaluator
from .strategy_plan import BUNDLE_PLAN_VERSION, plan_decimal

if TYPE_CHECKING:
    from nautilus_trader.model.data import Bar


@dataclass(frozen=True, slots=True)
class SleeveState:
    """Virtually attributed holdings, their holding age, and this session's exits."""

    shares: Mapping[str, int]
    sessions_held: Mapping[str, int]
    exited: frozenset[str]


@dataclass(frozen=True, slots=True)
class BundleSession:
    """The bars, membership and portfolio snapshot visible at a session close."""

    day: date
    open_ts: int
    ts_event: int
    bars: Mapping[str, "Bar"]
    members: frozenset[str]
    cash: Decimal
    sleeves: Mapping[str, SleeveState]
    pending_reductions: Mapping[Claim, int]
    emit: bool


@dataclass(frozen=True, slots=True)
class BundleSignal:
    """Absolute per-sleeve targets before execution-time pricing and allocation."""

    equity: Decimal | None
    holdings: dict[Claim, int]
    targets: dict[Claim, int]
    reasons: dict[Claim, str]
    gaps: tuple[DataGap, ...]


@dataclass(frozen=True, slots=True)
class _SleeveRules:
    recipe: dict
    budget: Fraction
    priority: int
    maximum_gross: Fraction
    maximum_asset: Fraction
    components: tuple[str, ...]


def mark_equity(cash, bars, members, holdings):
    if any(shares and key not in bars for key, shares in holdings.items()):
        return None
    if not any(key in bars for key in members) and not any(holdings.values()):
        return None
    with localcontext(EXACT_RESEARCH_CONTEXT, prec=80):
        return cash + sum(shares * bars[key].close.as_decimal() for key, shares in holdings.items() if shares)


class BundleSignalEvaluator:
    """Keep indicator history and accept the portfolio state at each closed session."""

    def __init__(self, plan: dict, *, indicator_evaluator: IndicatorEvaluator | None = None):
        if plan.get("plan_version") != BUNDLE_PLAN_VERSION:
            raise ResearchError("unsupported_strategy_plan", "Supply a trusted version 1 compiled strategy plan")
        spec = plan["bundle"]["spec"]
        self.reentry = spec["policy"]["same_session_reentry"]
        self.factors = spec["factors"]
        self.modules = spec["modules"]
        self.indicator_evaluator = indicator_evaluator
        self.indicator_backend = RustIndicatorEvaluator() if indicator_evaluator is None else None
        # External observations also load compiler plans from a local file.
        # Reject malformed cyclic graphs before an observation traverses them.
        TopologicalSorter({name: component.get("depends_on", ()) for name, component in self.factors.items()}).prepare()
        self.history = {}
        self.session_times = {}
        self.factor_values = {}
        self.gross_cap = Fraction(plan_decimal(spec["portfolio"]["max_gross_weight"]))
        self.asset_cap = Fraction(plan_decimal(spec["portfolio"]["max_weight_per_asset"]))
        self.sleeves = {}
        for row in plan["sleeves"]:
            name = row["recipe"]
            recipe = spec["recipes"][name]
            budget = Fraction(plan_decimal(row["budget_weight"]))
            risk_caps = [Fraction(plan_decimal(self.modules[key]["params"]["max_gross_weight"]))
                         for key in recipe["risk"]]
            sizing = self.modules.get(recipe.get("sizing"))
            cap = Fraction(plan_decimal(sizing["params"]["max_weight_per_asset"])) if sizing else Fraction(1)
            component_keys = [recipe[key] for key in ("entry", "exit", "sizing", "allocator") if key in recipe]
            component_keys += recipe["filters"] + recipe["risk"]
            components = {self.modules[key]["use"] for key in component_keys}
            factor_ids = [self.modules[key]["params"]["factor"] for key in component_keys
                          if "factor" in self.modules[key]["params"]]
            audited_factors = set()
            while factor_ids:
                factor_id = factor_ids.pop()
                if factor_id in audited_factors:
                    continue
                audited_factors.add(factor_id)
                component = self.factors[factor_id]
                components.add(component["use"])
                factor_ids.extend(component.get("depends_on", ()))
            self.sleeves[name] = _SleeveRules(
                recipe, budget, row["priority"], min(budget, *risk_caps) if risk_caps else budget, cap,
                tuple(sorted(components | {plan["execution"]})),
            )
        self.priorities = {key: sleeve.priority for key, sleeve in self.sleeves.items()}
        self.budgets = {key: sleeve.budget for key, sleeve in self.sleeves.items()}

    def close(self):
        if self.indicator_backend is not None:
            self.indicator_backend.close()

    def _update_history(self, session):
        bars = session.bars
        previous = self.history
        self.history = {
            key: previous.get(key, []) for key in session.members if key in bars
        }
        for key, history in self.history.items():
            history.append(bars[key])
        if self.indicator_backend is not None:
            for key in previous.keys() - self.history.keys():
                self.indicator_backend.discard(key)
            self.session_times[session.ts_event] = (session.day, session.open_ts)
        self.factor_values.clear()

    def _value(self, factor, key):
        history = self.history.get(key)
        pending = [factor]
        # The compiler rejects cycles. Walk the dependency graph without using
        # the Python call stack and cache each observation for this close.
        while pending:
            current = pending.pop()
            cache_key = (current, key)
            if cache_key in self.factor_values:
                continue
            component = self.factors[current]
            dependencies = component.get("depends_on", ())
            unresolved = [dep for dep in dependencies if (dep, key) not in self.factor_values]
            if history and unresolved:
                pending.append(current)
                pending.extend(reversed(unresolved))
                continue
            value = None
            if history and all(self.factor_values[dep, key] is not None for dep in dependencies):
                if self.indicator_backend is None:
                    value = self.indicator_evaluator(history, component["use"], component["params"])
                else:
                    value = self.indicator_backend.value(
                        key, current, history, component["use"], component["params"],
                        component.get("output", "value"), self.session_times,
                    )
                if value is not None and (not isinstance(value, Decimal) or not value.is_finite()):
                    raise ResearchError("invalid_factor", "Configured indicator must return a finite Decimal or None")
            self.factor_values[cache_key] = value
        return self.factor_values[factor, key]

    def _filter_allows(self, recipe, bar):
        return bar is not None and all(
            not self.modules[key]["params"]["require_positive_volume"] or bar.volume.raw > 0
            for key in recipe["filters"]
        )

    def _entry_targets(self, sleeve, state, session, equity):
        recipe = sleeve.recipe
        exit_module = self.modules[recipe["exit"]]
        entry = self.modules[recipe["entry"]]
        targets, reasons = dict(state.shares), {}
        for key, shares in state.shares.items():
            if not shares:
                continue
            if exit_module["use"] == "exit.holding_sessions@1.0.0":
                exits = state.sessions_held[key] >= exit_module["params"]["sessions"]
            else:
                parameters = exit_module["params"]
                observed = self._value(parameters["factor"], key)
                exits = observed is not None and observed < plan_decimal(parameters["threshold"])
            if exits:
                targets[key], reasons[key] = 0, "recipe_exit"
        if equity is None or not sleeve.budget:
            return targets, reasons
        parameters = entry["params"]
        threshold = plan_decimal(parameters["threshold"])
        bars = session.bars
        candidates = []
        for key in sorted(session.members):
            if state.shares.get(key) or (key in state.exited and not self.reentry):
                continue
            if not self._filter_allows(recipe, bars.get(key)):
                continue
            observed = self._value(parameters["factor"], key)
            if observed is None:
                continue
            is_above = entry["use"] == "entry.above_when_flat@1.0.0"
            if (is_above and observed > threshold) or (not is_above and observed < threshold):
                candidates.append(key)
        if candidates:
            held_value = sum(shares * Fraction(bars[key].close.as_decimal())
                             for key, shares in targets.items() if shares)
            budget = max(Fraction(), Fraction(equity) * sleeve.maximum_gross - held_value)
            value = min(budget / len(candidates), Fraction(equity) * sleeve.maximum_asset)
            for key in candidates:
                targets[key] = value // Fraction(bars[key].close.as_decimal())
                reasons[key] = "recipe_entry"
        return targets, reasons

    def _weight_targets(self, sleeve, state, session, equity):
        targets, reasons = dict(state.shares), {}
        if equity is None or not sleeve.budget:
            return targets, reasons
        recipe = sleeve.recipe
        parameters = self.modules[recipe["allocator"]]["params"]
        bars = session.bars
        values = {key: self._value(parameters["factor"], key) for key in sorted(session.members)}
        ranked = sorted((key for key, value in values.items() if value is not None), key=values.__getitem__, reverse=True)
        eligible = [key for key in ranked if state.shares.get(key) or self._filter_allows(recipe, bars[key])]
        selected = set(eligible[:parameters["top_n"]])
        value = Fraction(equity) * sleeve.maximum_gross / len(selected) if selected else Fraction()
        for key in ranked:
            current = state.shares.get(key, 0)
            target = value // Fraction(bars[key].close.as_decimal()) if key in selected else 0
            add_risk = self._filter_allows(recipe, bars[key]) and (key not in state.exited or self.reentry)
            targets[key] = target if add_risk else min(target, current)
            reasons[key] = "target_weight"
        return targets, reasons

    def limits(self, claims, prices, holdings, equity):
        equity = Fraction(equity)
        uses = {key: prices[key[0]] for key in claims}
        used_by_asset = defaultdict(Fraction)
        used_by_sleeve = defaultdict(Fraction)
        for (instrument, recipe), quantity in holdings.items():
            value = prices[instrument] * quantity
            used_by_asset[instrument] += value
            used_by_sleeve[recipe] += value
        limits = [Limit("portfolio_gross_limit", max(Fraction(), equity * self.gross_cap - sum(used_by_asset.values())), uses)]
        for instrument in sorted({key[0] for key in claims}):
            limits.append(Limit(
                "portfolio_asset_limit", max(Fraction(), equity * self.asset_cap - used_by_asset[instrument]),
                {key: cost for key, cost in uses.items() if key[0] == instrument},
            ))
        for recipe in sorted({key[1] for key in claims}):
            sleeve = self.sleeves[recipe]
            limits.append(Limit(
                "sleeve_gross_limit", max(Fraction(), equity * sleeve.maximum_gross - used_by_sleeve[recipe]),
                {key: cost for key, cost in uses.items() if key[1] == recipe},
            ))
            if sleeve.maximum_asset < 1:
                for key in (key for key in claims if key[1] == recipe):
                    limit = equity * sleeve.maximum_asset - holdings.get(key, 0) * uses[key]
                    limits.append(Limit("sleeve_asset_limit", max(Fraction(), limit), {key: uses[key]}))
        return limits

    def evaluate(self, session: BundleSession) -> BundleSignal:
        day, bars, members = session.day, session.bars, session.members
        holdings = {(key, recipe): shares for recipe, state in session.sleeves.items()
                    for key, shares in state.shares.items() if shares}
        held = defaultdict(int)
        for (instrument, _), shares in holdings.items():
            held[instrument] += shares
        equity = mark_equity(session.cash, bars, members, held)
        self._update_history(session)
        if not session.emit:
            return BundleSignal(equity, holdings, {}, {}, ())

        gaps = []
        for key in sorted(members | held.keys()):
            if key not in bars:
                gaps.append(DataGap(day, key, "valuation_price" if key in held else "missing_price"))
            elif bars[key].volume.raw == 0:
                gaps.append(DataGap(day, key, "zero_volume"))

        targets, reasons = {}, {}
        for recipe, sleeve in self.sleeves.items():
            state = session.sleeves[recipe]
            target_fn = self._entry_targets if sleeve.recipe["type"] == "entry_exit" else self._weight_targets
            target, recipe_reasons = target_fn(sleeve, state, session, equity)
            for key, shares in state.shares.items():
                if shares and key not in members:
                    target[key], recipe_reasons[key] = 0, "universe_exit"
            targets.update({(key, recipe): qty for key, qty in target.items()})
            reasons.update({(key, recipe): reason for key, reason in recipe_reasons.items()})

        # Apply reductions that remain outstanding before sizing any new risk.
        for key, reduction in session.pending_reductions.items():
            targets[key] = min(targets[key], holdings[key] + reduction)
        retained = {key: min(qty, holdings[key]) for key, qty in targets.items() if qty and key in holdings}
        if equity is not None and retained:
            prices = {key: Fraction(bar.close.as_decimal()) for key, bar in bars.items()}
            allowed, limits = allocate(retained, prices, self.priorities, self.budgets,
                                       self.limits(retained, prices, {}, equity))
            for key, reason in limits.items():
                targets[key], reasons[key] = allowed[key], reason

        for key in sorted({key for (_, key), value in self.factor_values.items() if value is None}):
            gaps.append(DataGap(day, key, "missing_factor"))
        return BundleSignal(equity, holdings, targets, reasons, tuple(gaps))
