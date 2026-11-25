"""Point-in-time XNYS portfolio research using one Nautilus BacktestEngine."""

from collections import defaultdict
from collections.abc import Sequence
from dataclasses import dataclass
from datetime import date
from decimal import Decimal, ROUND_DOWN, localcontext
from fractions import Fraction
from typing import Literal

from .research_decimal import EXACT_RESEARCH_CONTEXT, RESEARCH_CONTEXT

# Nautilus computes its fixed-point Decimal scale when loading its native types.
with localcontext(RESEARCH_CONTEXT):
    from nautilus_trader.backtest.engine import BacktestEngine
    from nautilus_trader.backtest.models import PerContractFeeModel
    from nautilus_trader.config import BacktestEngineConfig, LoggingConfig
    from nautilus_trader.core.data import Data
    from nautilus_trader.model.currencies import USD
    from nautilus_trader.model.data import CustomData, DataType, QuoteTick
    from nautilus_trader.model.enums import AccountType, OmsType, OrderSide, TimeInForce
    from nautilus_trader.model.identifiers import ClientId
    from nautilus_trader.model.objects import QUANTITY_MAX, Money, Quantity
    from nautilus_trader.trading.strategy import Strategy

    from .fill_audit import FillSignal, audit_logical_fills
    from .portfolio_inputs import (
        DataGap,
        FactorValue,
        PortfolioAsset,
        TargetFunction,
        TargetWeights,
        UniverseSnapshot,
        prepare_portfolio,
    )
    from .research_catalog import ResearchError
    from .research_metrics import RiskFreeSeries, daily_risk_free
    from .research_windows import coverage_windows


FillTiming = Literal["next_open", "next_close"]
_CLOCK_CLIENT = ClientId("PORTFOLIO-CLOCK")
_MAX_SHARES = int(QUANTITY_MAX)
_QUOTE_SIZE = Quantity.from_int(_MAX_SHARES)


def _native_cash(amount):
    with localcontext(EXACT_RESEARCH_CONTEXT):
        money = Money.from_decimal(amount, USD)
        if money.as_decimal() != amount:
            raise ValueError("USD balance is not representable at cent precision")
    return money


def _cash_after_order(instrument, quantity, price, side, cash, day):
    with localcontext(EXACT_RESEARCH_CONTEXT):
        description = f"{instrument.id} {day}: {quantity} shares at {price} USD"
        try:
            exact = quantity.as_decimal() * price.as_decimal()
            projected = cash - exact if side == OrderSide.BUY else cash + exact
        except ArithmeticError as exc:
            raise ResearchError("native_cash_precision", f"{description}: cash arithmetic exceeds exact range") from exc
        try:
            native = instrument.notional_value(quantity, price).as_decimal()
            # USD risk and settlement rewrap the notional in Money. The pinned
            # constructor rounds either sign symmetrically to currency precision.
            rewrapped = Money(native, USD).as_decimal()
        except (ArithmeticError, ValueError) as exc:
            raise ResearchError(
                "native_cash_precision", f"{description}: {exact} USD is outside native notional range",
            ) from exc
        if native != exact or rewrapped != exact:
            raise ResearchError(
                "native_cash_precision",
                f"{description}: exact notional {exact} USD, native {native} USD, cash impact magnitude {rewrapped} USD",
            )
        try:
            _native_cash(projected)
        except (ArithmeticError, ValueError) as exc:
            raise ResearchError(
                "native_cash_precision", f"{description}: projected cash {projected} USD is outside native balance range",
            ) from exc
    return projected


@dataclass(frozen=True)
class PortfolioConfig:
    fill_timing: FillTiming = "next_open"
    rebalance_every: int = 1
    weight_drift_threshold: Decimal = Decimal(0)
    initial_cash: Decimal = Decimal("100000.00")
    fill_missing: bool = False

    def __post_init__(self):
        if self.fill_timing not in ("next_open", "next_close"):
            raise ResearchError("invalid_parameters", "fill_timing must be next_open or next_close")
        if type(self.rebalance_every) is not int or self.rebalance_every < 1:
            raise ResearchError("invalid_parameters", "rebalance_every must be a positive session count")
        threshold = self.weight_drift_threshold
        if not isinstance(threshold, Decimal) or not threshold.is_finite() or not 0 <= threshold <= 1:
            raise ResearchError("invalid_parameters", "weight_drift_threshold must be a Decimal in [0, 1]")
        cash = self.initial_cash
        if not isinstance(cash, Decimal) or not cash.is_finite() or cash <= 0:
            raise ResearchError("invalid_parameters", "initial_cash must be positive Decimal USD")
        try:
            _native_cash(cash)
        except (ArithmeticError, ValueError) as exc:
            raise ResearchError("invalid_parameters", "initial_cash must fit in Nautilus Money at cent precision") from exc
        if type(self.fill_missing) is not bool:
            raise ResearchError("invalid_parameters", "fill_missing must be a boolean")


@dataclass(frozen=True)
class FillAudit:
    instrument_id: str
    signal_date: date
    signal_ts: int
    fill_date: date
    fill_ts: int
    side: Literal["buy", "sell"]
    quantity: Decimal
    price: Decimal
    order_id: str
    trade_id: str

    @property
    def notional(self) -> Decimal:
        with localcontext(EXACT_RESEARCH_CONTEXT):
            return self.quantity * self.price


@dataclass(frozen=True)
class RebalanceAudit:
    date: date
    signal_ts: int
    targets: dict[str, Decimal]


@dataclass(frozen=True)
class PortfolioResult:
    fill_timing: FillTiming
    fill_missing: bool
    equity: dict[date, Decimal | None]
    positions: dict[str, Decimal]
    fills: tuple[FillAudit, ...]
    rebalances: tuple[RebalanceAudit, ...]
    gaps: tuple[DataGap, ...]
    total_orders: int
    total_positions: int
    windows: list[dict]
    risk_free: dict

    @property
    def longest_sample(self) -> dict:
        """The longest_sample row in the unified windows report."""
        return self.windows[-1]


class _SessionEvent(Data):
    def __init__(self, day, when, *, closing):
        self.date = day
        self.when = when
        self.closing = closing

    @property
    def ts_event(self):
        return self.when

    @property
    def ts_init(self):
        return self.when


_SESSION_EVENT = DataType(_SessionEvent)


@dataclass
class _ExpectedFill:
    instrument_id: str
    signal_date: date
    signal_ts: int
    fill_date: date
    fill_ts: int
    side: OrderSide
    quantity: int
    price: Decimal
    cash_after: Decimal
    filled: bool = False


class _PortfolioStrategy(Strategy):
    def __init__(self, inputs, config, *, research_details):
        super().__init__()
        self.inputs = inputs
        self.settings = config
        self.cash = config.initial_cash
        self.shares = defaultdict(int)
        self.equity = {}
        self.carried_valuation_dates = set()
        self.gaps = []
        self.rebalances = []
        self.fills = []
        self.raw_fills = [] if research_details else None
        self.weights = {} if research_details else None
        self.orders = {}
        self.quotes = {}
        self.pending = None

    def on_start(self):
        self.subscribe_data(_SESSION_EVENT, client_id=_CLOCK_CLIENT)
        for asset in self.inputs.assets.values():
            self.subscribe_quote_ticks(asset.id)

    def on_quote_tick(self, tick):
        self.quotes[str(tick.instrument_id)] = tick

    def on_data(self, event):
        if event.closing == (self.settings.fill_timing == "next_close"):
            self.execute(event)
        if event.closing:
            self.mark_and_signal(event)

    def _mark_equity(self, day):
        equity = self.cash
        marked_positions = {} if self.weights is not None else None
        members = self.inputs.members[day]
        for key, shares in sorted(self.shares.items()):
            if not shares:
                continue
            if key not in members:
                self.gaps.append(DataGap(day, key, "valuation_inactive"))
                equity = None
                continue
            bar, source_day = self.inputs.signal_bar(day, key, fill_missing=self.settings.fill_missing)
            if bar is None:
                self.gaps.append(DataGap(day, key, "valuation_price"))
                equity = None
                continue
            if source_day != day:
                self.gaps.append(DataGap(day, key, "valuation_price", source_day))
                self.carried_valuation_dates.add(day)
            if equity is not None:
                value = shares * bar.close.as_decimal()
                equity += value
                if marked_positions is not None:
                    marked_positions[key] = value

        # A cash balance alone is not a market observation. A carried holding
        # mark can still be used by the strategy and is excluded from coverage.
        if day not in self.carried_valuation_dates and not any(
            self.inputs.signal_bar(day, key)[0] is not None for key in self.inputs.bars[day]
        ):
            equity = None

        if marked_positions is not None:
            with localcontext(RESEARCH_CONTEXT):
                self.weights[day] = None if equity is None else {
                    key: value / equity for key, value in marked_positions.items()
                }
        return equity

    def mark_and_signal(self, event):
        day = event.date
        try:
            with localcontext(EXACT_RESEARCH_CONTEXT):
                equity = self._mark_equity(day)
        except ArithmeticError as exc:
            raise ResearchError("valuation_precision", f"Portfolio equity for {day} exceeds exact Decimal range") from exc
        due = len(self.equity) % self.settings.rebalance_every == 0
        self.equity[day] = equity
        if not due:
            return
        targets, gaps = self.inputs.resolve_targets(day, fill_missing=self.settings.fill_missing, equity=equity)
        self.gaps.extend(gaps)
        # A repeating equal weight is rounded only for the public Decimal audit.
        with localcontext(RESEARCH_CONTEXT, prec=28, rounding=ROUND_DOWN):
            audit_targets = {
                key: Decimal(weight.numerator) / Decimal(weight.denominator)
                if isinstance(weight, Fraction) else weight
                for key, weight in targets.items()
            }
        self.rebalances.append(RebalanceAudit(day, event.ts_event, audit_targets))
        if equity is None:
            return

        exact_equity = Fraction(equity)
        drift_limit = exact_equity * Fraction(self.settings.weight_drift_threshold)
        changes = {}
        held = {key for key, shares in self.shares.items() if shares}
        for key in sorted(set(targets) | held):
            bar, _ = self.inputs.signal_bar(day, key, fill_missing=self.settings.fill_missing)
            if bar is None:
                continue
            price = Fraction(bar.close.as_decimal())
            current = self.shares[key]
            target_value = exact_equity * Fraction(targets.get(key, 0))
            if abs(current * price - target_value) <= drift_limit:
                continue
            quantity = target_value // price - current
            if quantity:
                changes[key] = quantity
        self.pending = (day, event.ts_event, changes)

    def execute(self, event):
        if self.pending is None:
            return
        signal_day, signal_ts, changes = self.pending
        self.pending = None
        day = event.date
        # Simulated cash from confirmed sales is available to buys at the same event.
        for key in sorted(changes, key=lambda key: (changes[key] > 0, key)):
            if key not in self.inputs.members[day]:
                self.gaps.append(DataGap(day, key, "execution_inactive"))
                continue
            quote = self.quotes.get(key)
            if quote is None or quote.ts_event != event.ts_event:
                bar = self.inputs.bars[day].get(key)
                kind = "execution_zero_volume" if bar is not None else "execution_price"
                self.gaps.append(DataGap(day, key, kind))
                continue
            change = changes[key]
            price = quote.bid_price.as_decimal()
            if change > 0:
                size = min(change, Fraction(self.cash) // Fraction(price))
                if size < change:
                    self.gaps.append(DataGap(day, key, "execution_cash"))
                side = OrderSide.BUY
            else:
                size = -change
                side = OrderSide.SELL
            if not size:
                continue
            if size > _MAX_SHARES:
                raise ResearchError("invalid_parameters", "Order size exceeds Nautilus Quantity range")
            quantity = Quantity.from_int(size)
            cash_after = _cash_after_order(
                self.inputs.assets[key], quantity, quote.bid_price, side, self.cash, day,
            )
            order = self.order_factory.market(
                quote.instrument_id, side, quantity, time_in_force=TimeInForce.FOK,
            )
            plan = _ExpectedFill(key, signal_day, signal_ts, day, event.ts_event, side, size, price, cash_after)
            self.orders[order.client_order_id] = plan
            self.submit_order(order)
            if not plan.filled:
                raise ResearchError("execution_audit_failed", f"Expected an immediate full fill of {key} at {day}")

    def on_order_filled(self, event):
        plan = self.orders.get(event.client_order_id)
        size = event.last_qty.as_decimal()
        price = event.last_px.as_decimal()
        if (plan is None or plan.filled or str(event.instrument_id) != plan.instrument_id
                or not plan.signal_ts < plan.fill_ts == event.ts_event or plan.quantity != size
                or plan.side != event.order_side or plan.price != price or event.commission.as_decimal() != 0):
            raise ResearchError("execution_audit_failed", "Unexpected time, size, price or fee for portfolio fill")
        plan.filled = True
        change = plan.quantity if plan.side == OrderSide.BUY else -plan.quantity
        self.shares[plan.instrument_id] += change
        self.cash = plan.cash_after
        if self.cash < 0 or self.shares[plan.instrument_id] < 0:
            raise ResearchError("execution_audit_failed", "Portfolio incurred borrowing or a short position")
        self.fills.append(FillAudit(
            plan.instrument_id, plan.signal_date, plan.signal_ts, plan.fill_date, event.ts_event,
            "buy" if change > 0 else "sell", size, price, str(event.client_order_id), str(event.trade_id),
        ))
        if self.raw_fills is not None:
            self.raw_fills.append(event)


def _simulate(inputs, config, *, research_details=False):
    engine = BacktestEngine(BacktestEngineConfig(logging=LoggingConfig(bypass_logging=True)))
    strategy = _PortfolioStrategy(inputs, config, research_details=research_details)
    venue = next(iter(inputs.assets.values())).id.venue
    try:
        engine.add_venue(
            venue=venue, oms_type=OmsType.NETTING, account_type=AccountType.CASH, base_currency=USD,
            starting_balances=[_native_cash(config.initial_cash)],
            fee_model=PerContractFeeModel(Money.from_decimal(Decimal(0), USD)),
            bar_execution=False, trade_execution=False, use_message_queue=False,
        )
        for instrument in inputs.assets.values():
            engine.add_instrument(instrument)
        bars_by_asset = defaultdict(list)
        quotes_by_asset = defaultdict(list)
        clocks = []
        for day, close in inputs.closes.items():
            opening = inputs.opens[day]
            execution_at_close = config.fill_timing == "next_close"
            for key, bar in sorted(inputs.bars[day].items()):
                bars_by_asset[key].append(bar)
                if key in inputs.members[day] and bar.volume.raw > 0:
                    when = close if execution_at_close else opening
                    price = bar.close if execution_at_close else bar.open
                    quotes_by_asset[key].append(QuoteTick(
                        inputs.assets[key].id, price, price, _QUOTE_SIZE, _QUOTE_SIZE, when, when,
                    ))
            if not execution_at_close:
                clocks.append(CustomData(_SESSION_EVENT, _SessionEvent(day, opening, closing=False)))
            clocks.append(CustomData(_SESSION_EVENT, _SessionEvent(day, close, closing=True)))

        # Stable time sorting puts both real bars and all executable quotes before the session marker.
        for stream in (*bars_by_asset.values(), *quotes_by_asset.values()):
            engine.add_data(stream, sort=False)
        engine.add_data(clocks, client_id=_CLOCK_CLIENT, sort=False)
        engine.sort_data()
        engine.add_strategy(strategy)
        engine.run()
        result = engine.get_result()
        native_orders = {order.client_order_id: order.filled_qty.as_decimal() for order in engine.cache.orders()}
        audited_orders = {key: plan.quantity for key, plan in strategy.orders.items()}
        open_positions = engine.cache.positions_open()
        positions = {str(position.instrument_id): position.quantity.as_decimal()
                     for position in open_positions if position.is_long}
        account = engine.cache.account_for_venue(venue)
        held = {key: shares for key, shares in strategy.shares.items() if shares}
        if (set(inputs.closes) != strategy.equity.keys() or native_orders != audited_orders
                or result.total_orders != len(strategy.fills) or positions != held
                or len(positions) != len(open_positions)
                or account.balance_total(USD).as_decimal() != strategy.cash):
            raise ResearchError("execution_audit_failed", "Engine orders, positions, cash or session marks disagree")
        return strategy, positions, result.total_orders, result.total_positions
    finally:
        engine.dispose()


def _performance(equity, closes, fills, rf, *, carried_valuation_dates):
    observed = {day: value for day, value in equity.items()
                if value is not None and day not in carried_valuation_dates}
    aligned_rf = daily_risk_free(closes, rf) if rf is not None else None
    risk_free = {"source": rf.source, "basis": rf.basis} if rf is not None else {"source": "rf=0", "basis": "daily"}
    turnover = dict.fromkeys(closes, Decimal(0))
    with localcontext(RESEARCH_CONTEXT):
        for fill in fills:
            turnover[fill.fill_date] += fill.notional / equity[fill.signal_date]
        windows = coverage_windows(
            observed, next(reversed(closes)), rf=aligned_rf, rf_source=rf.source if rf is not None else None,
            turnover=turnover, longest_through_end=True,
        )
    return windows, risk_free


def run_portfolio_backtest(
    *,
    assets: Sequence[PortfolioAsset],
    universe: Sequence[UniverseSnapshot],
    start: date,
    end: date,
    config: PortfolioConfig = PortfolioConfig(),
    targets: Sequence[TargetWeights] | None = None,
    factors: Sequence[FactorValue] | None = None,
    top_n: int | None = None,
    rf: RiskFreeSeries | None = None,
) -> PortfolioResult:
    """Run a long-only cash portfolio, selecting by target weights or a ranked factor."""
    result, _, _ = _run_portfolio_backtest(
        assets=assets, universe=universe, start=start, end=end, config=config,
        targets=targets, factors=factors, top_n=top_n, rf=rf,
    )
    return result


def _run_portfolio_backtest(
    *, assets, universe, start, end, config, targets=None, factors=None, top_n=None, rf=None,
    target_fn: TargetFunction | None = None, research_details=False,
):
    # The engine invokes Python callbacks synchronously and also converts native
    # fixed-point objects through Decimal on this thread.
    with localcontext(RESEARCH_CONTEXT):
        inputs = prepare_portfolio(
            assets, start=start, end=end, universe=universe, target_weights=targets, factors=factors, top_n=top_n,
            target_fn=target_fn, min_assets=1 if research_details else 2,
        )
        strategy, positions, orders, total_positions = _simulate(inputs, config, research_details=research_details)
        logical_fills = ()
        if strategy.raw_fills is not None:
            signals = {
                fill.order_id: FillSignal(fill.signal_date, fill.signal_ts, fill.fill_date)
                for fill in strategy.fills
            }
            logical_fills = audit_logical_fills(strategy.raw_fills, signals)
        windows, risk_free = _performance(
            strategy.equity, inputs.closes, logical_fills if research_details else strategy.fills, rf,
            carried_valuation_dates=strategy.carried_valuation_dates,
        )
        result = PortfolioResult(
            config.fill_timing, config.fill_missing, strategy.equity, positions, tuple(strategy.fills),
            tuple(strategy.rebalances), tuple(strategy.gaps), orders, total_positions, windows, risk_free,
        )
        return result, strategy.weights, logical_fills
