"""Run an F2 compiled StrategyBundle on Nautilus' simulated cash account."""

from collections import defaultdict
from collections.abc import Callable, Sequence
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
from .strategy_fee_search import largest_cash_feasible_fee_total
from .strategy_indicators import IndicatorEvaluator
from .strategy_signals import BundleSession, BundleSignal, BundleSignalEvaluator, SleeveState, mark_equity

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
    cash: Decimal
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
    def __init__(self, inputs, config, plan, start, evaluator, costs, *, signal_observer=None):
        super().__init__(inputs, config, research_details=True)
        self.first_decision = start
        self.signals = BundleSignalEvaluator(plan, indicator_evaluator=evaluator)
        self.priorities = self.signals.priorities
        self.budgets = self.signals.budgets
        self.signal_observer = signal_observer
        self.costs = costs
        self.days = {day: index for index, day in enumerate(inputs.closes)}
        self.sleeves = {
            row["recipe"]: _Sleeve(config.initial_cash * Decimal(row["budget_weight"])) for row in plan["sleeves"]
        }
        self.pending = {}
        self.attribution = []
        self.residual = config.initial_cash - sum(sleeve.cash for sleeve in self.sleeves.values())

    def mark_and_signal(self, event):
        day = event.date
        session = BundleSession(
            day, event.ts_event, self.inputs.bars[day], self.inputs.members[day], self.cash,
            {
                name: SleeveState(
                    dict(sleeve.shares),
                    {key: self.days[day] - first for key, first in sleeve.first_fill.items()},
                    frozenset(sleeve.exited),
                )
                for name, sleeve in self.sleeves.items()
            },
            {key: intent.quantity for key, intent in self.pending.items() if intent.quantity < 0},
            day >= self.first_decision,
        )
        signal = self.signals.evaluate(session)
        self.equity[day] = signal.equity
        if session.emit:
            self.gaps.extend(signal.gaps)
            for key in sorted(set(signal.targets) | self.pending.keys()):
                current = signal.holdings.get(key, 0)
                change = signal.targets.get(key, current) - current
                existing = self.pending.get(key)
                if not change:
                    if existing is not None:
                        self._audit(key, existing, day, event.ts_event,
                                    status="cancelled", reason="deferred_entry_invalidated")
                        del self.pending[key]
                    continue
                if existing is None or existing.quantity != change:
                    self.pending[key] = _Intent(
                        change, day, event.ts_event, signal.reasons.get(key, "deferred_reduction"),
                    )
        if self.signal_observer is not None:
            self.signal_observer(session, signal)

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
        for limit in self.signals.limits(after, prices, {}, equity - cost):
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

    def _largest_fixed_match(self, key, low, high, projected):
        """Check exact cent shares where the internal allocations stay fixed."""
        instrument, recipe = key
        priority = self.priorities[recipe]
        buyer = projected.fills[key]
        cent = Fraction(CENT)
        prefix = 0
        peers = []
        focal_index = 0
        changing = {recipe}
        for other, fill in sorted(projected.fills.items()):
            if other == key or other[0] != instrument or not fill.external:
                continue
            peer = other[1]
            if self.priorities[peer] < priority:
                prefix += fill.external
            else:
                peer_cents = (projected.balances[peer] + Fraction(fill.commission)) // cent
                peers.append((fill.external, peer_cents))
                focal_index += other < key
                changing.add(peer)
        if any(balance < 0 for name, balance in projected.balances.items() if name not in changing):
            return None

        ask = Fraction(buyer.price) / cent
        available = projected.balances[recipe] + buyer.external * Fraction(buyer.price) + Fraction(buyer.commission)
        offset = sum(qty for qty, _ in peers) - buyer.netted
        total = largest_cash_feasible_fee_total(
            low + offset, high + offset, ask * Fraction(self.costs.commission_rate), prefix,
            tuple(peers), focal_index, focal=(available // cent, int(ask)),
        )
        return None if total is None else total - offset

    def _refill_quantity(self, key, current, requested, crossing, trial, rejection, fits_risk):
        """Maximize this single buy with all other chosen quantities fixed."""
        largest = trial(requested)
        if rejection(largest) is None:
            return requested

        fill = largest.fills[key]
        wallet = largest.balances[key[1]] - Fraction(fill.cash_delta)
        upper = min(requested - 1, wallet // Fraction(fill.close))
        external_low = max(current + 1, crossing + 1)
        if upper >= external_low:
            # In the net-buy region, increasing aggregate cost reduces equity
            # while marked exposure rises. Cash splits need not be monotone.
            high = _last_passing_quantity(external_low - 1, upper + 1, lambda qty: fits_risk(trial(qty)))
            if high >= external_low:
                projected = trial(high)
                if rejection(projected) is None:
                    return high
                # Above the variable buyer's rounded matched count, its
                # continuous matching target is uncapped. All matches stay
                # fixed as more external shares are added in this range.
                fixed_low = max(external_low, projected.fills[key].netted + 1)
                if fixed_low <= high:
                    best = self._largest_fixed_match(key, fixed_low, high, projected)
                    if best is None:
                        high = fixed_low - 1
                    elif rejection(trial(best)) is None:
                        return best
                for qty in range(high, external_low - 1, -1):
                    if rejection(trial(qty)) is None:
                        return qty

        upper = min(upper, crossing)
        if upper <= current:
            return current
        # Below the crossing, the buyer pays the close. Increased buys add
        # exposure and save at most the same amount in aggregate sell costs.
        # Check the affected limits as upper bounds, then settle all sleeves.
        high = _last_passing_quantity(current, upper + 1, lambda qty: rejection(trial(qty), key) is None)
        if high == current:
            return current
        projected = trial(high)
        if rejection(projected) is None:
            return high
        if not fits_risk(projected):
            return current
        sellers = sum(other[0] == key[0] and fill.quantity < 0 for other, fill in projected.fills.items())
        if sellers > 1:
            # Multiple sellers can exchange a matched share or a fee cent as
            # the crossing changes. Test feasible-risk candidates in descending order.
            for qty in range(high - 1, current, -1):
                projected = trial(qty)
                if not fits_risk(projected):
                    break
                if rejection(projected) is None:
                    return qty
        return current

    def _choose_executions(self, day, candidates, closes):
        reductions = {key: row.quantity for key, row in candidates.items() if row.quantity < 0}
        buys = {key: row.quantity for key, row in candidates.items() if row.quantity > 0}
        equity = mark_equity(self.cash, self.inputs.bars[day], self.inputs.members[day], self.shares) if buys else None
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
            limits = self.signals.limits(claims, prices, retained, risk_headroom)
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

            def trial(qty):
                return settle(additions | {key: qty})

            def fits_risk(projected):
                return self._execution_risk_breach(holdings, prices, equity, projected.fills) is None

            instrument, _ = key
            other_buys = sum(qty for (asset, _), qty in (fixed | additions).items() if asset == instrument and qty > 0)
            cross = supplied[instrument] - (other_buys - current_qty)
            additions[key] = self._refill_quantity(key, current_qty, requested, cross, trial, rejection, fits_risk)

        return {key: qty for key, qty in additions.items() if qty}

    def _audit(self, key, intent, day, ts, *, status, reason, **details):
        instrument, recipe = key
        self.attribution.append({
            "recipe": recipe, "instrument_id": instrument,
            "signal_date": intent.day, "signal_ts": str(intent.ts),
            "execution_date": day, "execution_ts": str(ts) if ts is not None else None, "phase": "execution",
            "status": status, "reason": reason, "intent_reason": intent.reason,
            "requested_quantity": Decimal(intent.quantity), "components": self.signals.sleeves[recipe].components,
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
    signal_observer: Callable[[BundleSession, BundleSignal], None] | None = None,
) -> ConfigStrategyResult:
    """Replay pinned point-in-time inputs and optionally observe each close-time decision."""
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
        strategy = _ConfiguredStrategy(
            inputs, config, plan, start, indicator_evaluator, costs, signal_observer=signal_observer,
        )
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
            "recipe": name, "cash": sleeve.cash, "priority": strategy.priorities[name],
            "positions": {key: Decimal(quantity) for key, quantity in sorted(sleeve.shares.items())},
        } for name, sleeve in sorted(strategy.sleeves.items()))
        return ConfigStrategyResult(
            cash, strategy.cash, equity, positions, orders, total_positions, fills, tuple(strategy.attribution),
            sleeves, tuple(strategy.gaps), windows,
        )
