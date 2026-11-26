"""Run an F2 compiled StrategyBundle on Nautilus' simulated cash account."""

from collections import defaultdict
from collections.abc import Sequence
from dataclasses import dataclass, field
from datetime import date
from decimal import Decimal, ROUND_HALF_UP, localcontext
from fractions import Fraction
from math import ceil

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
            components.update(
                self.factors[self.modules[key]["params"]["factor"]]["use"]
                for key in component_keys if "factor" in self.modules[key]["params"]
            )
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
        cache_key = (factor, key)
        if cache_key not in self.factor_values:
            component = self.factors[factor]
            history = self.history.get(key)
            value = self.evaluate(history, component["use"], component["params"]) if history else None
            if value is not None and (not isinstance(value, Decimal) or not value.is_finite()):
                raise ResearchError("invalid_factor", "Configured indicator must return a finite Decimal or None")
            self.factor_values[cache_key] = value
        return self.factor_values[cache_key]

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

    def _choose_executions(self, day, candidates, prices):
        reductions = {key: row.quantity for key, row in candidates.items() if row.quantity < 0}
        buys = {key: row.quantity for key, row in candidates.items() if row.quantity > 0}
        if not buys:
            return reductions, {}
        equity = self._mark(day, self.shares)
        if equity is None:
            return reductions, dict.fromkeys(buys, "execution_valuation_missing")

        holdings = {(instrument, recipe): shares + reductions.get((instrument, recipe), 0)
                    for recipe, sleeve in self.sleeves.items() for instrument, shares in sleeve.shares.items() if shares}
        limits = self._limits(buys, prices, holdings, equity)
        balance = {name: Fraction(sleeve.cash) for name, sleeve in self.sleeves.items()}
        # Reserve the worst-case cash for external execution. Internal crossing
        # costs less, and the released cash is available at the next decision.
        cent = Fraction(CENT)
        rate = Fraction(self.costs.commission_rate)
        for (instrument, recipe), quantity in reductions.items():
            proceeds = -quantity * Fraction(self.quotes[instrument].bid_price.as_decimal())
            balance[recipe] += proceeds - ceil(proceeds * rate / cent) * cent
        for recipe in sorted({key[1] for key in buys}):
            costs = {key: Fraction(self.quotes[key[0]].ask_price.as_decimal()) * (1 + rate)
                     for key in buys if key[1] == recipe}
            rounding_reserve = len(costs) * cent if rate else Fraction()
            limits.append(Limit("sleeve_cash", max(Fraction(), balance[recipe] - rounding_reserve), costs))
        additions, reasons = allocate(buys, prices, self.priorities, self.budgets, limits)
        return reductions | additions, reasons

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

    def _native_fill(self, instrument, quantity, event, intent):
        quote = self.quotes[instrument]
        size = abs(quantity)
        if size > _MAX_SHARES:
            raise ResearchError("invalid_parameters", "Order size exceeds Nautilus Quantity range")
        side, price = ((OrderSide.BUY, quote.ask_price) if quantity > 0
                       else (OrderSide.SELL, quote.bid_price))
        qty = Quantity.from_int(size)
        commission = self.costs.commission(size, price.as_decimal())
        cash_after = _cash_after_order(
            self.inputs.assets[instrument], qty, price, side, self.cash, event.date, commission,
        )
        if cash_after < 0:
            raise ResearchError("execution_audit_failed", "Planned external fill would borrow cash")
        plan = _ExpectedFill(
            instrument, intent.day, intent.ts, event.date, event.ts_event, side, size,
            ((qty, price),), cash_after, commission,
        )
        return str(self._submit_fill(plan, qty)), price.as_decimal(), commission

    def execute(self, event):
        for sleeve in self.sleeves.values():
            sleeve.exited.clear()
        day = event.date
        candidates = self._execution_candidates(event)
        if not candidates:
            return
        prices = {key: Fraction(bar.close.as_decimal()) for key, bar in self.inputs.bars[day].items()}
        accepted, reasons = self._choose_executions(day, candidates, prices)
        totals = defaultdict(int)
        by_instrument = defaultdict(list)
        for key, quantity in sorted(accepted.items()):
            if quantity:
                totals[key[0]] += quantity
                by_instrument[key[0]].append(key)

        external, internal = {}, {}
        native = {}
        allocated_commissions = {}
        for instrument in sorted(totals, key=lambda key: (totals[key] > 0, key)):
            keys = by_instrument[instrument]
            net = totals[instrument]
            matched = sum(accepted[key] for key in keys if accepted[key] > 0) - max(0, net)
            unmatched_buy = unmatched_sell = matched
            for key in keys:
                quantity = accepted[key]
                if quantity > 0:
                    internal[key] = min(quantity, unmatched_buy)
                    unmatched_buy -= internal[key]
                else:
                    internal[key] = min(-quantity, unmatched_sell)
                    unmatched_sell -= internal[key]
                external[key] = abs(quantity) - internal[key]
            if net:
                ext = [key for key in keys if external[key]]
                first = min((candidates[key] for key in ext), key=lambda row: row.ts)
                native[instrument] = self._native_fill(instrument, net, event, first)
                fee = int(native[instrument][2] / CENT)
                cents = {key: fee * external[key] // abs(net) for key in ext}
                remainder = fee - sum(cents.values())
                allocated_commissions.update({
                    key: (cents[key] + (index < remainder)) * CENT for index, key in enumerate(ext)
                })

        with localcontext(EXACT_RESEARCH_CONTEXT, prec=80):
            self._apply_fills(event, candidates, accepted, reasons, internal, external, native, allocated_commissions)

    def _apply_fills(self, event, candidates, accepted, reasons, internal, external, native, allocated_commissions):
        day = event.date
        for key, intent in sorted(candidates.items()):
            instrument, recipe = key
            quantity = accepted.get(key, 0)
            if not quantity:
                self._audit(key, intent, day, event.ts_event, status="rejected", reason=reasons[key],
                            accepted_quantity=Decimal(0))
                continue
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
            close = self.inputs.bars[day][instrument].close.as_decimal()
            order_id, fill_price, _ = native[instrument] if external[key] else (None, close, Decimal(0))
            commission = allocated_commissions.get(key, Decimal(0))
            value = internal[key] * close + external[key] * fill_price
            cash_delta = (-value if quantity > 0 else value) - commission
            sleeve.cash += cash_delta
            status = "clipped" if quantity != intent.quantity else "accepted"
            self._audit(
                key, intent, day, event.ts_event, status=status,
                reason=reasons.get(key, "internal_netting" if internal[key] else "native_fill"),
                accepted_quantity=Decimal(quantity), netted_quantity=Decimal(internal[key]),
                external_quantity=Decimal(external[key]), cash_delta=cash_delta, commission=commission,
                slippage_cost=external[key] * abs(fill_price - close), close_price=close,
                execution_price=fill_price, order_id=order_id,
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
