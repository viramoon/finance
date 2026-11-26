"""Single-asset daily SMA research on the source-built Nautilus backtest engine."""

from collections import Counter, deque
from dataclasses import dataclass
from datetime import datetime, timezone
from decimal import Decimal, ROUND_CEILING, ROUND_FLOOR
from itertools import pairwise

from nautilus_trader.backtest.engine import BacktestEngine
from nautilus_trader.backtest.models import FixedFeeModel
from nautilus_trader.config import BacktestEngineConfig, LoggingConfig
from nautilus_trader.model.currencies import USD
from nautilus_trader.model.data import Bar, QuoteTick
from nautilus_trader.model.enums import AccountType, OmsType, OrderSide, TimeInForce
from nautilus_trader.model.objects import Money, Price, Quantity
from nautilus_trader.trading.strategy import Strategy

from .backtest_sample import timestamp
from .epoch import require_bar_timestamps, unix_nanos
from .research_catalog import CENT, ResearchError, iso_date, load_interval
from .research_windows import coverage_windows as daily_coverage_windows, metrics


STRATEGY = "SmaCrossover"
STARTING_CASH = Decimal("100000.00")
COMMISSION = Decimal("1.00")
SLIPPAGE_BPS = 5
RF = Decimal(0)


def coverage_windows(equity, end):
    """Compatibility entry point for callers expecting the three fixed windows."""
    return daily_coverage_windows(equity, end)[:3]


def design_price(close, side):
    slip = (close * SLIPPAGE_BPS / 10_000).quantize(CENT, rounding=ROUND_CEILING)
    if side == OrderSide.BUY:
        return (close + slip).quantize(CENT, rounding=ROUND_CEILING)
    return max(CENT, (close - slip).quantize(CENT, rounding=ROUND_FLOOR))


@dataclass
class ExpectedFill:
    signal: int
    session: int
    size: int
    px: Decimal
    volume: int
    side: OrderSide
    filled: int = 0


class DailySma(Strategy):
    def __init__(self, instrument, bars, days, fast, slow, shares):
        super().__init__()
        self.instrument = instrument
        self.data_by_close = {bar.ts_event: bar for bar in bars}
        self.days = days
        self.fast = fast
        self.history = deque(maxlen=slow)
        self.shares = shares
        self.bar_type = bars[0].bar_type
        self.intent = None
        self.shares_held = 0
        self.cash = STARTING_CASH
        self.equity = {}
        self.traded_notional = {}
        self.orders = {}
        self.fill_ids = set()
        self.recent_fills = deque(maxlen=20)
        self.recent_blocked = deque(maxlen=20)
        self.audit = Counter()

    def on_start(self):
        self.subscribe_quote_ticks(self.instrument)
        self.subscribe_bars(self.bar_type)

    def block(self, signal, session, volume, reason):
        self.audit[reason] += 1
        self.recent_blocked.append({
            "signal_session": str(self.days[signal]), "execution_session": str(self.days[session]),
            "bar_volume": str(volume), "requested_shares": self.shares, "reason": reason,
        })

    def on_quote_tick(self, tick):
        if self.intent is None:
            return
        signal, target = self.intent
        self.intent = None
        quantity = target - self.shares_held
        if not quantity:
            return
        side = OrderSide.BUY if quantity > 0 else OrderSide.SELL
        size = abs(quantity)
        bar = self.data_by_close[tick.ts_event]
        volume = int(bar.volume.as_decimal())
        if size > volume:
            self.block(signal, tick.ts_event, volume, "insufficient_volume")
            return
        px = (tick.ask_price if side == OrderSide.BUY else tick.bid_price).as_decimal()
        if side == OrderSide.BUY and px * size + COMMISSION > self.cash:
            self.block(signal, tick.ts_event, volume, "insufficient_cash")
            return
        order = self.order_factory.market(
            self.instrument, side, Quantity.from_int(size), time_in_force=TimeInForce.FOK,
        )
        self.orders[order.client_order_id] = ExpectedFill(signal, tick.ts_event, size, px, volume, side)
        self.submit_order(order)

    def on_bar(self, bar):
        if self.intent is not None:
            # No quote is published for a zero-volume day. Keep the bar for the
            # signal/equity and explicitly account for the blocked order intent.
            signal, target = self.intent
            if bar.volume.as_decimal() != 0:
                raise ResearchError("execution_audit_failed", "Missing simulated closing quote")
            if target != self.shares_held:
                self.block(signal, bar.ts_event, 0, "zero_volume")
            self.intent = None
        close = bar.close.as_decimal()
        if bar.volume.as_decimal() == 0:
            self.audit["zero_volume_bars"] += 1
        self.history.append(close)
        if len(self.history) == self.history.maxlen:
            day = self.days[bar.ts_event]
            self.equity[day] = self.cash + self.shares_held * close
            fast_sum = sum(list(self.history)[-self.fast:])
            target = self.shares if fast_sum * len(self.history) > sum(self.history) * self.fast else 0
            self.intent = (bar.ts_event, target)

    def on_order_filled(self, event):
        plan = self.orders[event.client_order_id]
        key = (event.client_order_id, event.trade_id)
        quantity = event.last_qty.as_decimal()
        price = event.last_px.as_decimal()
        fee = event.commission.as_decimal()
        # FOK over a single quoted level implies exactly one complete fill.
        if (key in self.fill_ids or plan.filled or plan.signal >= plan.session
                or event.ts_event != plan.session or not 0 < quantity == plan.size <= plan.volume
                or price != plan.px or event.order_side != plan.side or fee != COMMISSION):
            raise ResearchError("execution_audit_failed", "Duplicate, early, oversize or mispriced engine fill")
        self.fill_ids.add(key)
        plan.filled = int(quantity)
        change = plan.filled if plan.side == OrderSide.BUY else -plan.filled
        self.shares_held += change
        self.traded_notional[self.days[event.ts_event]] = quantity * price
        self.cash -= change * price + fee
        if self.cash < 0 or self.shares_held not in (0, self.shares):
            raise ResearchError("execution_audit_failed", "Simulated position or cash is inconsistent")
        self.recent_fills.append({
            "order_id": str(event.client_order_id), "signal_session": str(self.days[plan.signal]),
            "execution_session": str(self.days[event.ts_event]), "executed_at": timestamp(event.ts_event),
            "side": "buy" if change > 0 else "sell", "shares": int(quantity),
            "price": format(price, ".2f"), "commission": format(fee, ".2f"),
        })


def simulate(instrument, bars, days, fast, slow, shares):
    require_bar_timestamps(bars)
    engine = BacktestEngine(BacktestEngineConfig(logging=LoggingConfig(bypass_logging=True)))
    strategy = DailySma(instrument.id, bars, days, fast, slow, shares)
    try:
        engine.add_venue(
            venue=instrument.id.venue, oms_type=OmsType.NETTING, account_type=AccountType.CASH,
            starting_balances=[Money(STARTING_CASH, USD)], base_currency=USD,
            fee_model=FixedFeeModel(Money(COMMISSION, USD)), bar_execution=False, trade_execution=False,
            liquidity_consumption=True,
        )
        engine.add_instrument(instrument)
        feed = []
        for bar in bars:
            time = bar.ts_event
            volume = int(bar.volume.as_decimal())
            close = bar.close.as_decimal()
            if volume:
                size = Quantity.from_int(min(1000, volume))
                feed.append(QuoteTick(instrument.id, Price.from_str(str(design_price(close, OrderSide.SELL))),
                                      Price.from_str(str(design_price(close, OrderSide.BUY))), size, size, time, time))
            # Process the executable closing quote before evaluating this bar's signal.
            feed.append(Bar(bar.bar_type, bar.open, bar.high, bar.low, bar.close, bar.volume,
                            time, unix_nanos(time + 1)))
        engine.add_data(feed)
        engine.add_strategy(strategy)
        engine.run()
        result = engine.get_result()
        native_orders = {order.client_order_id: order.filled_qty.as_decimal()
                         for order in engine.cache.orders()}
        audited_orders = {key: Decimal(plan.size) for key, plan in strategy.orders.items()}
        if (len(strategy.orders) != result.total_orders
                or native_orders != audited_orders
                or any(plan.filled != plan.size for plan in strategy.orders.values())):
            raise ResearchError("execution_audit_failed", "An engine order did not produce exactly its audited fill")
        return result, strategy
    finally:
        engine.dispose()


def run_research(root, request):
    def request_day(value):
        try:
            instant = datetime.fromisoformat(value)
            if instant.tzinfo is not None and instant.utcoffset().total_seconds() == 0:
                day = instant.date()
                if instant == datetime.combine(day, datetime.min.time(), timezone.utc):
                    return iso_date(day.isoformat())
        except (TypeError, ValueError):
            pass
        raise ResearchError("invalid_date", "Research dates must be UTC midnight")

    if request.get("strategy") != STRATEGY:
        raise ResearchError("invalid_parameters", "Unsupported research strategy")
    start, end = (request_day(request[key]) for key in ("start", "end"))
    params = request["params"]
    fast, slow, shares = (params.get(key) for key in ("fast_period", "slow_period", "shares"))
    if any(type(value) is not int for value in (fast, slow, shares)) or not (
        2 <= fast < slow <= 250 and 1 <= shares <= 1000 and start < end
    ):
        raise ResearchError("invalid_parameters", "Require 2 <= fast < slow <= 250 and 1..1000 shares")
    metadata, instrument, bars, days = load_interval(root, request["instrument"], start, end)
    if len(bars) < slow + 1:
        raise ResearchError("insufficient_history", f"Require at least {slow + 1} consecutive XNYS sessions")
    result, strategy = simulate(instrument, bars, days, fast, slow, shares)
    ending_equity = strategy.equity[days[bars[-1].ts_event]]
    actual_start, actual_end = days[bars[0].ts_event], days[bars[-1].ts_event]
    turnover = {
        day: strategy.traded_notional.get(day, Decimal(0)) / strategy.equity[prior]
        for prior, day in pairwise(sorted(strategy.equity))
    }
    report = {
        "dataset": metadata, "requested_start": str(start), "requested_end": str(end),
        "actual_start": str(actual_start), "actual_end": str(actual_end), "observations": len(bars),
        "warmup_sessions": slow, "performance_start": str(min(strategy.equity)),
        "initial_equity": format(STARTING_CASH, ".2f"), "ending_equity": format(ending_equity, ".2f"),
        "commission_per_order": format(COMMISSION, ".2f"), "slippage_bps": SLIPPAGE_BPS,
        "risk_free_rate": str(RF), "execution": "signal_at_close_next_session_close",
        "metrics": metrics(strategy.equity),
        "windows": daily_coverage_windows(strategy.equity, actual_end, turnover=turnover),
        "audit": {
            "fills": len(strategy.fill_ids), "zero_volume_bars": strategy.audit["zero_volume_bars"],
            "blocked_zero_volume": strategy.audit["zero_volume"],
            "blocked_insufficient_volume": strategy.audit["insufficient_volume"],
            "blocked_insufficient_cash": strategy.audit["insufficient_cash"],
            "recent_fills": list(strategy.recent_fills), "recent_blocked": list(strategy.recent_blocked),
        },
    }
    return {
        "run_id": result.run_id, "strategy": STRATEGY, "instrument": request["instrument"],
        "start": timestamp(bars[0].ts_event), "end": timestamp(bars[-1].ts_event),
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "pnl": format(ending_equity - STARTING_CASH, ".2f"), "currency": "USD",
        "total_orders": result.total_orders, "total_positions": result.total_positions, "research": report,
    }
