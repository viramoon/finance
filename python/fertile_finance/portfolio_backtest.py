"""Point-in-time XNYS portfolio research using one Nautilus BacktestEngine."""

from collections import defaultdict
from collections.abc import Sequence
from dataclasses import dataclass
from datetime import date
from decimal import Decimal
from typing import Literal

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

from .backtest_research import coverage_windows, metrics, percent
from .portfolio_inputs import (
    DataGap,
    FactorValue,
    PortfolioAsset,
    TargetWeights,
    UniverseSnapshot,
    prepare_portfolio,
)
from .research_catalog import CENT, ResearchError
from .research_metrics import RiskFreeSeries, excess_returns, sharpe_ratio


FillTiming = Literal["next_open", "next_close"]
_CLOCK_CLIENT = ClientId("PORTFOLIO-CLOCK")
_QUOTE_SIZE = Quantity.from_int(int(QUANTITY_MAX))


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
            if cash == cash.quantize(CENT):
                Money.from_decimal(cash.quantize(CENT), USD)
            else:
                raise ValueError("Sub-cent amount")
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
    longest_sample: dict
    risk_free: dict


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
    filled: bool = False


class _PortfolioStrategy(Strategy):
    def __init__(self, inputs, config):
        super().__init__()
        self.inputs = inputs
        self.settings = config
        self.cash = config.initial_cash
        self.shares = defaultdict(int)
        self.equity = {}
        self.gaps = []
        self.rebalances = []
        self.fills = []
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

    def mark_and_signal(self, event):
        day = event.date
        equity = self.cash
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
            if equity is not None:
                equity += shares * bar.close.as_decimal()

        due = len(self.equity) % self.settings.rebalance_every == 0
        self.equity[day] = equity
        if not due:
            return
        targets, gaps = self.inputs.resolve_targets(day, fill_missing=self.settings.fill_missing)
        self.gaps.extend(gaps)
        self.rebalances.append(RebalanceAudit(day, event.ts_event, targets))
        if equity is None:
            return

        changes = {}
        held = {key for key, shares in self.shares.items() if shares}
        for key in sorted(set(targets) | held):
            bar, _ = self.inputs.signal_bar(day, key, fill_missing=self.settings.fill_missing)
            if bar is None:
                continue
            price = bar.close.as_decimal()
            current = self.shares[key]
            target = targets.get(key, Decimal(0))
            if abs(current * price - target * equity) <= self.settings.weight_drift_threshold * equity:
                continue
            quantity = int(equity * target // price) - current
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
                size = min(change, int(self.cash // price))
                if size < change:
                    self.gaps.append(DataGap(day, key, "execution_cash"))
                side = OrderSide.BUY
            else:
                size = -change
                side = OrderSide.SELL
            if not size:
                continue
            if size > _QUOTE_SIZE.as_decimal():
                raise ResearchError("invalid_parameters", "Order size exceeds Nautilus Quantity range")
            order = self.order_factory.market(
                quote.instrument_id, side, Quantity.from_int(size), time_in_force=TimeInForce.FOK,
            )
            plan = _ExpectedFill(key, signal_day, signal_ts, day, event.ts_event, side, size, price)
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
        self.cash -= change * price
        if self.cash < 0 or self.shares[plan.instrument_id] < 0:
            raise ResearchError("execution_audit_failed", "Portfolio incurred borrowing or a short position")
        self.fills.append(FillAudit(
            plan.instrument_id, plan.signal_date, plan.signal_ts, plan.fill_date, event.ts_event,
            "buy" if change > 0 else "sell", size, price, str(event.client_order_id), str(event.trade_id),
        ))


def _simulate(inputs, config):
    engine = BacktestEngine(BacktestEngineConfig(logging=LoggingConfig(bypass_logging=True)))
    strategy = _PortfolioStrategy(inputs, config)
    venue = next(iter(inputs.assets.values())).id.venue
    try:
        engine.add_venue(
            venue=venue, oms_type=OmsType.NETTING, account_type=AccountType.CASH, base_currency=USD,
            starting_balances=[Money.from_decimal(config.initial_cash.quantize(CENT), USD)],
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
                if key in inputs.members[day] and bar.volume.as_decimal() > 0:
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


def _performance(equity, closes, fills, rf):
    days = list(closes)
    observed = {day: value for day, value in equity.items() if value is not None}
    closes_with_equity = [(day, closes[day], equity[day]) for day in days]
    daily_excess = dict(zip(days[1:], excess_returns(closes_with_equity, rf), strict=True))
    daily_turnover = defaultdict(Decimal)
    for fill in fills:
        daily_turnover[fill.fill_date] += fill.quantity * fill.price / equity[fill.signal_date]

    def annotate(row, window_days):
        row["observations"] = len(window_days)
        row["risk_free"] = risk_free
        if row["metrics"] is not None:
            # The first mark is the return and turnover baseline for this window.
            sharpe = sharpe_ratio([daily_excess[day] for day in window_days[1:]])
            row["metrics"]["sharpe"] = sharpe
            row["metrics"]["turnover"] = sum((daily_turnover[day] for day in window_days[1:]), Decimal(0))
        return row

    risk_free = {"source": rf.source, "basis": rf.basis} if rf is not None else {"source": "rf=0", "basis": "daily"}
    windows = coverage_windows(observed, days[-1])
    for row in windows:
        start = date.fromisoformat(row["requested_start"])
        annotate(row, [day for day in observed if day >= start])
    full = len(observed) == len(days) and len(days) >= 2
    first, last = min(observed, default=None), max(observed, default=None)
    actual_years = format(Decimal((last - first).days) / Decimal("365.2425"), ".4f") if first else None
    longest = annotate({
        "requested_start": str(days[0]), "calendar_start": str(days[0]),
        "actual_start": str(first) if first else None, "actual_end": str(last) if last else None,
        "actual_years": actual_years, "expected_sessions": len(days),
        "missing_sessions": len(days) - len(observed), "coverage": "full" if full else "insufficient",
        "missing_pct": percent(Decimal(len(days) - len(observed)) / len(days)),
        "metrics": metrics(observed) if full else None,
    }, list(observed))
    return windows, longest, risk_free


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
    inputs = prepare_portfolio(
        assets, start=start, end=end, universe=universe, target_weights=targets, factors=factors, top_n=top_n,
    )
    strategy, positions, orders, total_positions = _simulate(inputs, config)
    windows, longest, risk_free = _performance(strategy.equity, inputs.closes, strategy.fills, rf)
    return PortfolioResult(
        config.fill_timing, config.fill_missing, strategy.equity, positions, tuple(strategy.fills),
        tuple(strategy.rebalances), tuple(strategy.gaps), orders, total_positions, windows, longest, risk_free,
    )
