"""Run an F2 compiled StrategyBundle on Nautilus' simulated cash account."""

from collections import defaultdict
from collections.abc import Sequence
from dataclasses import dataclass, field
from datetime import date
from decimal import Decimal, ROUND_HALF_UP, localcontext
from fractions import Fraction
from itertools import groupby

from .portfolio_backtest import (
    DataGap, FillSignal, OrderSide, PortfolioAsset, PortfolioConfig, Price, Quantity, ResearchError,
    UniverseSnapshot, _ExpectedFill, _MAX_SHARES, _PortfolioStrategy, _cash_after_order,
    _native_cash, _performance, _simulate, audit_logical_fills,
)
from .portfolio_inputs import prepare_cash_equity_inputs
from .research_catalog import CENT
from .research_decimal import EXACT_RESEARCH_CONTEXT, RESEARCH_CONTEXT
from .strategy_allocation import Claim, Limit, allocate
from .strategy_indicators import IndicatorEvaluator, evaluate_indicator

from nautilus_trader.backtest.models import FeeModel


@dataclass(frozen=True)
class ConfigStrategyResult:
    initial_cash: Decimal
    cash: Decimal
    equity: dict[date, Decimal | None]
    positions: dict[str, Decimal]
    total_orders: int
    total_positions: int
    fills: tuple[dict, ...]
    attribution: tuple[dict, ...]
    sleeves: tuple[dict, ...]
    gaps: tuple[DataGap, ...]
    windows: list[dict]


@dataclass
class _Sleeve:
    recipe: dict
    budget: Fraction
    priority: int
    cash: Decimal
    maximum_gross: Fraction
    maximum_asset: Fraction
    components: tuple[str, ...]
    shares: dict[str, int] = field(default_factory=dict)
    first_fill: dict[str, int] = field(default_factory=dict)
    exited: set[str] = field(default_factory=set)


@dataclass(frozen=True)
class _Intent:
    quantity: int
    day: date
    ts: int
    reason: str


@dataclass(frozen=True)
class _ExternalOrder:
    quantity: int
    price: Price
    commission: Decimal


@dataclass(frozen=True)
class _ProjectedFill:
    quantity: int
    netted: int
    external: int
    close: Decimal
    price: Decimal
    commission: Decimal

    @property
    def cash_delta(self):
        value = self.netted * self.close + self.external * self.price
        return (-value if self.quantity > 0 else value) - self.commission

    @property
    def slippage(self):
        return self.external * abs(self.price - self.close)


@dataclass(frozen=True)
class _Settlement:
    fills: dict[Claim, _ProjectedFill]
    balances: dict[str, Fraction]


def _last_passing_quantity(low, high, feasible):
    """Tighten an integer interval, keeping the known passing lower end."""
    while high - low > 1:
        midpoint = (low + high) // 2
        if feasible(midpoint):
            low = midpoint
        else:
            high = midpoint
    return low


def _affordable_cents(upper, feasible):
    """Find a feasible whole-cent headroom below a failed projection."""
    cent = Fraction(CENT)
    return _last_passing_quantity(0, int(upper / cent) + 1, lambda qty: feasible(qty * cent)) * cent


def _refill_quantity(current, requested, crossing, fits, fits_affected):
    if fits(requested):
        return requested
    if current < crossing:
        # Below the crossing, each extra buy adds close-price exposure and
        # saves at most that amount in sell costs. The affected caps and
        # buyer's cash tighten, while untouched exposure ratios improve.
        internal_max = _last_passing_quantity(current, min(crossing + 1, requested), fits_affected)
        if fits(internal_max):
            current = internal_max
        if current < crossing:
            return current
    return _last_passing_quantity(current, requested, fits)


class _CashEquityCosts(FeeModel):
    def __init__(self, execution):
        self.commission_rate = Decimal(execution["commission_bps"]) / 10_000
        self.slippage_rate = Decimal(execution["slippage_bps"]) / 10_000

    def prices(self, price):
        mid = price.as_decimal()
        sell = max(CENT, (mid * (1 - self.slippage_rate)).quantize(CENT, rounding=ROUND_HALF_UP))
        buy = (mid * (1 + self.slippage_rate)).quantize(CENT, rounding=ROUND_HALF_UP)
        return Price.from_decimal(sell), Price.from_decimal(buy)

    def commission(self, shares, price):
        return (shares * price * self.commission_rate).quantize(CENT, rounding=ROUND_HALF_UP)

    def get_commission(self, order, fill_qty, fill_px, instrument):
        return _native_cash(self.commission(fill_qty.as_decimal(), fill_px.as_decimal()))


class _ConfiguredStrategy(_PortfolioStrategy):
    def __init__(self, inputs, config, plan, start, evaluator, costs):
        super().__init__(inputs, config, research_details=True)
        spec = plan["bundle"]["spec"]
        self.first_decision = start
        self.reentry = spec["policy"]["same_session_reentry"]
        self.factors = spec["factors"]
        self.modules = spec["modules"]
        self.evaluate = evaluator
        self.costs = costs
        self.history = {}
        self.factor_values = {}
        self.days = {day: index for index, day in enumerate(inputs.closes)}
        self.gross_cap = Fraction(spec["portfolio"]["max_gross_weight"])
        self.asset_cap = Fraction(spec["portfolio"]["max_weight_per_asset"])
        self.sleeves = {}
        self.pending = {}
        self.attribution = []
        for row in plan["sleeves"]:
            name = row["recipe"]
            recipe = spec["recipes"][name]
            budget = Decimal(row["budget_weight"])
            risk_caps = [Fraction(self.modules[key]["params"]["max_gross_weight"]) for key in recipe["risk"]]
            sizing = self.modules.get(recipe.get("sizing"))
            cap = Fraction(sizing["params"]["max_weight_per_asset"]) if sizing else Fraction(1)
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
            self.sleeves[name] = _Sleeve(
                recipe, Fraction(budget), row["priority"], config.initial_cash * budget,
                min(Fraction(budget), *risk_caps) if risk_caps else Fraction(budget), cap,
                tuple(sorted(components | {plan["execution"]})),
            )
        self.priorities = {key: sleeve.priority for key, sleeve in self.sleeves.items()}
        self.budgets = {key: sleeve.budget for key, sleeve in self.sleeves.items()}
        self.residual = config.initial_cash - sum(sleeve.cash for sleeve in self.sleeves.values())

    def _mark(self, day, holdings):
        bars = self.inputs.bars[day]
        if any(shares and key not in bars for key, shares in holdings.items()):
            return None
        if not any(key in bars for key in self.inputs.members[day]) and not any(holdings.values()):
            return None
        with localcontext(EXACT_RESEARCH_CONTEXT, prec=80):
            return self.cash + sum(shares * bars[key].close.as_decimal() for key, shares in holdings.items() if shares)

    def _update_history(self, day):
        bars = self.inputs.bars[day]
        previous = self.history
        self.history = {
            key: previous.get(key, []) for key in self.inputs.members[day] if key in bars
        }
        for key, history in self.history.items():
            history.append(bars[key])
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
                value = self.evaluate(history, component["use"], component["params"])
                if value is not None and (not isinstance(value, Decimal) or not value.is_finite()):
                    raise ResearchError("invalid_factor", "Configured indicator must return a finite Decimal or None")
            self.factor_values[cache_key] = value
        return self.factor_values[factor, key]

    def _filter_allows(self, recipe, bar):
        return bar is not None and all(
            not self.modules[key]["params"]["require_positive_volume"] or bar.volume.raw > 0
            for key in recipe["filters"]
        )

    def _entry_targets(self, sleeve, day, equity):
        recipe = sleeve.recipe
        exit_module = self.modules[recipe["exit"]]
        entry = self.modules[recipe["entry"]]
        targets, reasons = dict(sleeve.shares), {}
        for key, shares in sleeve.shares.items():
            if not shares:
                continue
            if exit_module["use"] == "exit.holding_sessions@1.0.0":
                exits = self.days[day] - sleeve.first_fill[key] >= exit_module["params"]["sessions"]
            else:
                parameters = exit_module["params"]
                observed = self._value(parameters["factor"], key)
                exits = observed is not None and observed < Decimal(parameters["threshold"])
            if exits:
                targets[key], reasons[key] = 0, "recipe_exit"
        if equity is None or not sleeve.budget:
            return targets, reasons
        parameters = entry["params"]
        threshold = Decimal(parameters["threshold"])
        bars = self.inputs.bars[day]
        candidates = []
        for key in sorted(self.inputs.members[day]):
            if sleeve.shares.get(key) or (key in sleeve.exited and not self.reentry):
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

    def _weight_targets(self, sleeve, day, equity):
        targets, reasons = dict(sleeve.shares), {}
        if equity is None or not sleeve.budget:
            return targets, reasons
        recipe = sleeve.recipe
        parameters = self.modules[recipe["allocator"]]["params"]
        bars = self.inputs.bars[day]
        values = {key: self._value(parameters["factor"], key) for key in sorted(self.inputs.members[day])}
        ranked = sorted((key for key, value in values.items() if value is not None), key=values.__getitem__, reverse=True)
        eligible = [key for key in ranked if sleeve.shares.get(key) or self._filter_allows(recipe, bars[key])]
        selected = set(eligible[:parameters["top_n"]])
        value = Fraction(equity) * sleeve.maximum_gross / len(selected) if selected else Fraction()
        for key in ranked:
            current = sleeve.shares.get(key, 0)
            target = value // Fraction(bars[key].close.as_decimal()) if key in selected else 0
            add_risk = self._filter_allows(recipe, bars[key]) and (key not in sleeve.exited or self.reentry)
            targets[key] = target if add_risk else min(target, current)
            reasons[key] = "target_weight"
        return targets, reasons

    def _limits(self, claims, prices, holdings, equity):
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

    def mark_and_signal(self, event):
        day = event.date
        equity = self._mark(day, self.shares)
        self.equity[day] = equity
        self._update_history(day)
        if day < self.first_decision:
            return
        present = self.inputs.bars[day]
        held = {key for key, qty in self.shares.items() if qty}
        for key in sorted(self.inputs.members[day] | held):
            if key not in present:
                self.gaps.append(DataGap(day, key, "valuation_price" if key in held else "missing_price"))
            elif present[key].volume.raw == 0:
                self.gaps.append(DataGap(day, key, "zero_volume"))

        holdings, targets, reasons = {}, {}, {}
        for recipe, sleeve in self.sleeves.items():
            target_fn = self._entry_targets if sleeve.recipe["type"] == "entry_exit" else self._weight_targets
            target, recipe_reasons = target_fn(sleeve, day, equity)
            for key, shares in sleeve.shares.items():
                if shares:
                    holdings[key, recipe] = shares
                    if key not in self.inputs.members[day]:
                        target[key], recipe_reasons[key] = 0, "universe_exit"
            targets.update({(key, recipe): qty for key, qty in target.items()})
            reasons.update({(key, recipe): reason for key, reason in recipe_reasons.items()})

        # Apply reductions that remain outstanding before sizing any new risk.
        for key, pending in self.pending.items():
            if pending.quantity < 0:
                targets[key] = min(targets[key], holdings[key] + pending.quantity)
        retained = {key: min(qty, holdings[key]) for key, qty in targets.items() if qty and key in holdings}
        if equity is not None and retained:
            prices = {key: Fraction(bar.close.as_decimal()) for key, bar in self.inputs.bars[day].items()}
            allowed, limits = allocate(retained, prices, self.priorities, self.budgets,
                                       self._limits(retained, prices, {}, equity))
            for key, reason in limits.items():
                targets[key], reasons[key] = allowed[key], reason

        for key in sorted(set(targets) | self.pending.keys()):
            current = holdings.get(key, 0)
            change = targets.get(key, current) - current
            existing = self.pending.get(key)
            if not change:
                if existing is not None:
                    self._audit(key, existing, day, event.ts_event, status="cancelled", reason="deferred_entry_invalidated")
                    del self.pending[key]
                continue
            if existing is None or existing.quantity != change:
                self.pending[key] = _Intent(change, day, event.ts_event, reasons.get(key, "deferred_reduction"))
        for key in sorted({key for (_, key), value in self.factor_values.items() if value is None}):
            self.gaps.append(DataGap(day, key, "missing_factor"))

    def _execution_candidates(self, event):
        day = event.date
        executable = {}
        retained = {}
        for key, intent in sorted(self.pending.items()):
            instrument, _ = key
            quote = self.quotes.get(instrument)
            if intent.quantity > 0 and instrument not in self.inputs.members[day]:
                self._audit(key, intent, day, event.ts_event, status="rejected", reason="execution_inactive")
            elif quote is None or quote.ts_event != event.ts_event:
                self._audit(key, intent, day, event.ts_event, status="deferred", reason="execution_price_missing")
                retained[key] = intent
            else:
                executable[key] = intent
        self.pending = retained
        return executable

    def _project_fills(self, accepted, closes):
        by_instrument = defaultdict(list)
        for key, quantity in accepted.items():
            if quantity:
                by_instrument[key[0]].append(key)

        fills, external_orders = {}, {}
        for instrument, keys in sorted(by_instrument.items()):
            close = closes[instrument]
            buys = {key: accepted[key] for key in keys if accepted[key] > 0}
            sells = {key: -accepted[key] for key in keys if accepted[key] < 0}
            bought, sold = sum(buys.values()), sum(sells.values())
            net = bought - sold
            matched = min(bought, sold)
            netted = {}
            if matched:
                for claims, total in ((buys, bought), (sells, sold)):
                    if matched == total:
                        netted.update(claims)
                    else:
                        split, _ = allocate(
                            claims, {instrument: Fraction(close)}, self.priorities, self.budgets,
                            [Limit("internal_liquidity", Fraction(matched), dict.fromkeys(claims, Fraction(1)))],
                        )
                        netted.update(split)
            external = {key: abs(accepted[key]) - netted.get(key, 0) for key in keys}

            price, commissions = close, {}
            if net:
                quote = self.quotes[instrument]
                execution_price = quote.ask_price if net > 0 else quote.bid_price
                price = execution_price.as_decimal()
                commission = self.costs.commission(abs(net), price)
                external_orders[instrument] = _ExternalOrder(net, execution_price, commission)
                commissions = self._split_commission(external, price)
            for key in keys:
                fills[key] = _ProjectedFill(
                    accepted[key], netted.get(key, 0), external[key], close, price if external[key] else close,
                    commissions.get(key, Decimal(0)),
                )
        return fills, external_orders

    def _split_commission(self, external, price):
        def priority(key):
            return self.priorities[key[1]]

        keys = sorted((key for key, qty in external.items() if qty), key=lambda key: (priority(key), key))
        commissions = {}
        cumulative = paid_cents = 0
        for _, tier in groupby(keys, key=priority):
            claims = tuple(tier)
            quantity = sum(external[key] for key in claims)
            cumulative += quantity
            cumulative_cents = int(self.costs.commission(cumulative, price) / CENT)
            fee = cumulative_cents - paid_cents
            paid_cents = cumulative_cents
            cents = {key: fee * external[key] // quantity for key in claims}
            remainder = fee - sum(cents.values())
            commissions.update({key: (cents[key] + (index < remainder)) * CENT for index, key in enumerate(claims)})
        return commissions

    def _execution_risk_breach(self, holdings, prices, equity, fills, *, affected=None):
        after = holdings.copy()
        cost = Fraction()
        for key, fill in fills.items():
            after[key] = after.get(key, 0) + fill.quantity
            cost += Fraction(fill.commission + fill.slippage)
        after = {key: qty for key, qty in after.items() if qty}
        for limit in self._limits(after, prices, {}, equity - cost):
            if affected is not None and affected not in limit.unit_costs:
                continue
            used = sum(after[key] * price for key, price in limit.unit_costs.items())
            if used > limit.available:
                return limit.reason
        return None

    def _clip_cash(self, recipe, projected, additions, prices, cash_headroom, claim_caps, settle):
        buys = {key: projected.fills[key] for key, qty in additions.items() if key[1] == recipe and qty}
        external = {key: fill.external for key, fill in buys.items() if fill.external}
        after_internal = projected.balances[recipe] + sum(
            qty * Fraction(buys[key].price) + Fraction(buys[key].commission) for key, qty in external.items()
        )
        if after_internal < 0:
            internal_cost = sum(fill.netted * Fraction(fill.close) for fill in buys.values())
            cash_headroom[recipe] = max(Fraction(), internal_cost + after_internal)
            return

        # The ask is a lower bound on external cost. Use it to reflow the
        # proportional proposal, then always test its actual net order fee.
        admitted, _ = allocate(
            external, prices, self.priorities, self.budgets,
            [Limit("sleeve_cash", after_internal, {key: Fraction(buys[key].price) for key in external})],
        )
        if admitted != external:
            for key, qty in admitted.items():
                if qty < external[key]:
                    claim_caps[key] = buys[key].netted + qty
            return

        # If all the asks fit, the deficit comes from the rounded fee. Clip
        # the last external claim, pricing each trial as an actual net order.
        key = max(external)
        internal = buys[key].netted

        def fits(qty):
            return settle(additions | {key: qty}).balances[recipe] >= 0

        claim_caps[key] = _last_passing_quantity(internal, additions[key], fits) if fits(internal) else internal

    def _settle(self, quantities, closes):
        fills, _ = self._project_fills(quantities, closes)
        balances = {name: Fraction(sleeve.cash) for name, sleeve in self.sleeves.items()}
        for key, fill in fills.items():
            balances[key[1]] += Fraction(fill.cash_delta)
        return _Settlement(fills, balances)

    def _execution_rejection(self, holdings, prices, equity, projected, *, affected=None):
        balances = projected.balances.values() if affected is None else (projected.balances[affected[1]],)
        if any(cash < 0 for cash in balances):
            return "sleeve_cash"
        if any(fill.quantity > 0 for fill in projected.fills.values()):
            return self._execution_risk_breach(holdings, prices, equity, projected.fills, affected=affected)
        return None

    def _choose_executions(self, day, candidates, closes):
        reductions = {key: row.quantity for key, row in candidates.items() if row.quantity < 0}
        buys = {key: row.quantity for key, row in candidates.items() if row.quantity > 0}
        equity = self._mark(day, self.shares) if buys else None
        if equity is None:
            fills, orders = self._project_fills(reductions, closes)
            return fills, orders, dict.fromkeys(buys, "execution_valuation_missing")

        equity = Fraction(equity)
        prices = {key: Fraction(price) for key, price in closes.items()}
        holdings = {
            (instrument, recipe): shares
            for recipe, sleeve in self.sleeves.items() for instrument, shares in sleeve.shares.items()
        }
        cash_at_close = {name: Fraction(sleeve.cash) for name, sleeve in self.sleeves.items()}
        supplied = defaultdict(int)
        for (instrument, recipe), quantity in reductions.items():
            cash_at_close[recipe] -= quantity * prices[instrument]
            supplied[instrument] -= quantity
        tiers = defaultdict(dict)
        for key, qty in sorted(buys.items()):
            tiers[self.priorities[key[1]]][key] = qty

        fixed = reductions.copy()
        for _, claims in sorted(tiers.items()):
            admitted = self._allocate_tier(
                claims, fixed, closes, prices, equity, holdings, cash_at_close, supplied,
            )
            fixed.update(admitted)

        # Attribute the first omitted share against the final batch. Trial
        # limits can stop binding after other claims or fees change.
        reasons = {}
        for key, requested in sorted(buys.items()):
            accepted = fixed.get(key, 0)
            if accepted < requested:
                extra = self._settle(fixed | {key: accepted + 1}, closes)
                reason = self._execution_rejection(holdings, prices, equity, extra)
                reasons[key] = reason or "allocation_order"
        fills, orders = self._project_fills(fixed, closes)
        return fills, orders, reasons

    def _allocate_tier(self, claims, fixed, closes, prices, equity, holdings, cash_at_close, supplied):
        """Admit this priority tier with priced reductions and earlier tiers fixed."""
        retained = holdings.copy()
        for key, quantity in fixed.items():
            retained[key] = retained.get(key, 0) + quantity
        by_sleeve = defaultdict(dict)
        for key in claims:
            by_sleeve[key[1]][key] = prices[key[0]]
        cash_headroom = {recipe: cash_at_close[recipe] for recipe in by_sleeve}
        claim_caps = {}

        def settle(additions):
            return self._settle(fixed | additions, closes)

        def rejection(projected, affected=None):
            return self._execution_rejection(holdings, prices, equity, projected, affected=affected)

        def project(risk_headroom):
            limits = self._limits(claims, prices, retained, risk_headroom)
            limits.extend(Limit("sleeve_cash", cash_headroom[recipe], uses) for recipe, uses in by_sleeve.items())
            limits.extend(Limit("sleeve_cash", Fraction(qty), {key: Fraction(1)})
                          for key, qty in sorted(claim_caps.items()))
            additions, _ = allocate(claims, prices, self.priorities, self.budgets, limits)
            return additions, settle(additions)

        def notional(additions):
            return sum(qty * prices[key[0]] for key, qty in additions.items())

        headroom = equity
        current = project(headroom)
        while True:
            additions, projected = current
            if rejection(projected) is None:
                break
            insolvent = next((recipe for recipe in sorted(cash_headroom) if projected.balances[recipe] < 0), None)
            if insolvent is not None:
                self._clip_cash(insolvent, projected, additions, prices, cash_headroom, claim_caps, settle)
                current = project(headroom)
            else:
                headroom = _affordable_cents(headroom, lambda limit: rejection(project(limit)[1]) is None)
                current = project(headroom)
                # A smaller bound may leave an expensive residual sell. Try
                # the matched portion of the old tier as a separate candidate.
                matches = dict.fromkeys(claims, 0)
                matches.update((key, fill.netted) for key, fill in projected.fills.items() if key in claims)
                if notional(matches) >= notional(current[0]):
                    crossed = settle(matches)
                    if rejection(crossed) is None:
                        current = matches, crossed

        # Revisit clipped claims in stable order after removing trial bounds.
        # Freeze peers on each trial and price the resulting net orders again.
        for key, requested in claims.items():
            current_qty = additions[key]
            if current_qty == requested:
                continue

            def fits(qty, affected=None):
                return rejection(settle(additions | {key: qty}), affected) is None

            instrument, _ = key
            other_buys = sum(qty for (asset, _), qty in (fixed | additions).items() if asset == instrument and qty > 0)
            cross = supplied[instrument] - (other_buys - current_qty)
            additions[key] = _refill_quantity(current_qty, requested, cross, fits, lambda qty: fits(qty, key))

        return {key: qty for key, qty in additions.items() if qty}

    def _audit(self, key, intent, day, ts, *, status, reason, **details):
        instrument, recipe = key
        self.attribution.append({
            "recipe": recipe, "instrument_id": instrument,
            "signal_date": intent.day, "signal_ts": str(intent.ts),
            "execution_date": day, "execution_ts": str(ts) if ts is not None else None, "phase": "execution",
            "status": status, "reason": reason, "intent_reason": intent.reason,
            "requested_quantity": Decimal(intent.quantity), "components": self.sleeves[recipe].components,
            **details,
        })

    def _native_fill(self, instrument, order, event, intent):
        size = abs(order.quantity)
        if size > _MAX_SHARES:
            raise ResearchError("invalid_parameters", "Order size exceeds Nautilus Quantity range")
        side = OrderSide.BUY if order.quantity > 0 else OrderSide.SELL
        qty = Quantity.from_int(size)
        cash_after = _cash_after_order(
            self.inputs.assets[instrument], qty, order.price, side, self.cash, event.date, order.commission,
        )
        if cash_after < 0:
            raise ResearchError("execution_audit_failed", "Planned external fill would borrow cash")
        plan = _ExpectedFill(
            instrument, intent.day, intent.ts, event.date, event.ts_event, side, size,
            ((qty, order.price),), cash_after, order.commission,
        )
        return str(self._submit_fill(plan, qty))

    def execute(self, event):
        for sleeve in self.sleeves.values():
            sleeve.exited.clear()
        day = event.date
        candidates = self._execution_candidates(event)
        if not candidates:
            return
        closes = {key: bar.close.as_decimal() for key, bar in self.inputs.bars[day].items()}
        fills, orders, reasons = self._choose_executions(day, candidates, closes)
        first_intents = {}
        for key, fill in sorted(fills.items()):
            if fill.external:
                instrument = key[0]
                previous = first_intents.get(instrument)
                if previous is None or candidates[key].ts < previous.ts:
                    first_intents[instrument] = candidates[key]
        native = {}
        for instrument in sorted(orders, key=lambda key: (orders[key].quantity > 0, key)):
            native[instrument] = self._native_fill(instrument, orders[instrument], event, first_intents[instrument])

        with localcontext(EXACT_RESEARCH_CONTEXT, prec=80):
            self._apply_fills(event, candidates, fills, reasons, native)

    def _apply_fills(self, event, candidates, fills, reasons, native):
        day = event.date
        for key, intent in sorted(candidates.items()):
            instrument, recipe = key
            fill = fills.get(key)
            if fill is None:
                self._audit(key, intent, day, event.ts_event, status="rejected", reason=reasons[key],
                            accepted_quantity=Decimal(0))
                continue
            quantity = fill.quantity
            sleeve = self.sleeves[recipe]
            old = sleeve.shares.get(instrument, 0)
            new = old + quantity
            if new:
                sleeve.shares[instrument] = new
                if not old:
                    sleeve.first_fill[instrument] = self.days[day]
            else:
                del sleeve.shares[instrument]
                del sleeve.first_fill[instrument]
                sleeve.exited.add(instrument)
            order_id = native[instrument] if fill.external else None
            sleeve.cash += fill.cash_delta
            status = "clipped" if quantity != intent.quantity else "accepted"
            self._audit(
                key, intent, day, event.ts_event, status=status,
                reason=reasons.get(key, "internal_netting" if fill.netted else "native_fill"),
                accepted_quantity=Decimal(quantity), netted_quantity=Decimal(fill.netted),
                external_quantity=Decimal(fill.external), cash_delta=fill.cash_delta, commission=fill.commission,
                slippage_cost=fill.slippage, close_price=fill.close, execution_price=fill.price, order_id=order_id,
            )

        holdings = defaultdict(int)
        for sleeve in self.sleeves.values():
            if sleeve.cash < 0 or any(quantity <= 0 for quantity in sleeve.shares.values()):
                raise ResearchError("execution_audit_failed", "A virtual sleeve borrowed cash or held a short position")
            for key, quantity in sleeve.shares.items():
                holdings[key] += quantity
        if (holdings != {key: qty for key, qty in self.shares.items() if qty}
                or self.cash != self.residual + sum(sleeve.cash for sleeve in self.sleeves.values())):
            raise ResearchError("execution_audit_failed", "Virtual positions or cash disagree with Nautilus")


def run_config_strategy_backtest(
    *, plan: dict, assets: Sequence[PortfolioAsset], universe: Sequence[UniverseSnapshot], start: date, end: date,
    history_start: date | None = None, indicator_evaluator: IndicatorEvaluator | None = None,
) -> ConfigStrategyResult:
    """Replay a trusted compiler plan with fully pinned point-in-time inputs."""
    if plan.get("plan_version") != "fertile.finance/strategy-plan/v1":
        raise ResearchError("unsupported_strategy_plan", "Supply a trusted version 1 compiled strategy plan")
    with localcontext(RESEARCH_CONTEXT, prec=80):
        spec = plan["bundle"]["spec"]
        cash = Decimal(spec["portfolio"]["initial_cash"]["amount"])
        config = PortfolioConfig(fill_timing="next_close", initial_cash=cash)
        if history_start is None:
            history_start = min(start, min((row.date for row in universe), default=start))
        inputs = prepare_cash_equity_inputs(assets, start=history_start, end=end, universe=universe, min_assets=1)
        closes = {day: ts for day, ts in inputs.closes.items() if day >= start}
        if len(closes) < 2:
            raise ResearchError("no_data", "Require at least two XNYS sessions in the requested interval")
        if len(inputs.closes) - len(closes) < spec["clock"]["warmup_sessions"]:
            raise ResearchError("insufficient_history", "Replay does not cover the required preceding warmup sessions")
        costs = _CashEquityCosts(spec["execution"])
        evaluator = evaluate_indicator if indicator_evaluator is None else indicator_evaluator
        strategy = _ConfiguredStrategy(inputs, config, plan, start, evaluator, costs)
        strategy, positions, orders, total_positions = _simulate(
            inputs, config, strategy=strategy, execution_prices=costs.prices, fee_model=costs, quote_nonmembers=True,
        )
        signals = {
            str(order_id): FillSignal(row.signal_date, row.signal_ts, row.fill_date)
            for order_id, row in strategy.orders.items()
        }
        audited = audit_logical_fills(strategy.raw_fills, signals)
        if len(audited) != orders:
            raise ResearchError("execution_audit_failed", "Expected one logical fill per configured order")
        fills = tuple({
            "instrument_id": str(row.fill.instrument_id), "signal_date": row.signal_date, "signal_ts": str(row.signal_ts),
            "fill_date": row.fill_date, "fill_ts": str(row.fill_ts),
            "side": "buy" if row.fill.side == OrderSide.BUY else "sell", "quantity": row.fill.quantity,
            "price": row.fill.parts[0].price, "commission": row.fill.commission,
            "order_id": str(row.fill.order_id), "trade_id": str(row.fill.parts[0].trade_id),
        } for row in audited)
        for key, intent in sorted(strategy.pending.items()):
            strategy._audit(key, intent, None, None, status="unfilled", reason="run_ended")
        equity = {day: strategy.equity[day] for day in closes}
        windows, _ = _performance(equity, closes, audited, None, carried_valuation_dates=set())
        sleeves = tuple({
            "recipe": name, "cash": sleeve.cash, "priority": sleeve.priority,
            "positions": {key: Decimal(quantity) for key, quantity in sorted(sleeve.shares.items())},
        } for name, sleeve in sorted(strategy.sleeves.items()))
        return ConfigStrategyResult(
            cash, strategy.cash, equity, positions, orders, total_positions, fills, tuple(strategy.attribution),
            sleeves, tuple(strategy.gaps), windows,
        )
