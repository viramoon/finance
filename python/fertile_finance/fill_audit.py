"""Summarize native fills for same-currency cash equity research."""

from collections import defaultdict
from collections.abc import Iterable, Mapping
from dataclasses import dataclass
from datetime import date
from decimal import Decimal, localcontext

from nautilus_trader.model.enums import OrderSide
from nautilus_trader.model.events import OrderFilled
from nautilus_trader.model.identifiers import AccountId, ClientOrderId, InstrumentId, TradeId
from nautilus_trader.model.objects import Currency

from .epoch import unix_nanos


@dataclass(frozen=True)
class LogicalFill:
    account_id: AccountId
    order_id: ClientOrderId
    instrument_id: InstrumentId
    side: OrderSide
    currency: Currency
    fill_ts: int
    quantity: Decimal
    notional: Decimal
    commission: Decimal
    raw_fills: tuple[OrderFilled, ...]

    @property
    def average_price(self) -> Decimal:
        """Volume-weighted price for display, without rounding to a price tick."""
        with localcontext() as context:
            context.prec = 28
            return self.notional / self.quantity

    @property
    def position_delta(self) -> Decimal:
        return self.quantity if self.side == OrderSide.BUY else self.quantity.copy_negate()

    @property
    def cash_delta(self) -> Decimal:
        with localcontext() as context:
            context.prec = 80
            proceeds = self.notional.copy_negate() if self.side == OrderSide.BUY else self.notional
            return proceeds - self.commission


@dataclass(frozen=True)
class FillSignal:
    signal_date: date
    signal_ts: int
    fill_date: date


@dataclass(frozen=True)
class AuditedFill(FillSignal):
    fill: LogicalFill

    @property
    def fill_ts(self) -> int:
        return self.fill.fill_ts

    @property
    def notional(self) -> Decimal:
        return self.fill.notional


def audit_logical_fills(
    events: Iterable[OrderFilled], signals: Mapping[str, FillSignal],
) -> tuple[AuditedFill, ...]:
    """Attach audited signal times to one run's grouped native executions."""
    fills = merge_order_fills(events)
    if {str(fill.order_id) for fill in fills} != signals.keys():
        raise ValueError("Native fills and audited order IDs disagree")
    audited = []
    for fill in fills:
        signal = signals[str(fill.order_id)]
        if unix_nanos(signal.signal_ts) >= fill.fill_ts:
            raise ValueError(f"Signal must precede the fill for order {fill.order_id}")
        audited.append(AuditedFill(signal.signal_date, signal.signal_ts, signal.fill_date, fill))
    return tuple(audited)


def merge_order_fills(fills: Iterable[OrderFilled]) -> tuple[LogicalFill, ...]:
    """Group a run's fills by account, client order ID, and event timestamp."""
    groups: dict[tuple[AccountId, ClientOrderId, int], list[OrderFilled]] = defaultdict(list)
    orders: dict[tuple[AccountId, ClientOrderId], tuple[InstrumentId, OrderSide, Currency]] = {}
    seen: set[tuple[AccountId, ClientOrderId, TradeId]] = set()
    for fill in fills:
        fill_ts = unix_nanos(fill.ts_event)
        order_key = (fill.account_id, fill.client_order_id)
        identity = (fill.instrument_id, fill.order_side, fill.currency)
        trade_key = (*order_key, fill.trade_id)
        if trade_key in seen:
            raise ValueError(f"Duplicate trade {fill.trade_id} for order {fill.client_order_id}")
        if orders.setdefault(order_key, identity) != identity or fill.commission.currency != fill.currency:
            raise ValueError(f"Inconsistent fill instrument, side, or currency for order {fill.client_order_id}")
        seen.add(trade_key)
        groups[(*order_key, fill_ts)].append(fill)

    return tuple(_summarize(group) for group in sorted(groups.values(), key=lambda group: group[0].ts_event))


def _summarize(group: list[OrderFilled]) -> LogicalFill:
    first = group[0]
    with localcontext() as context:
        # Keep products of high-precision fixed-point quantities and prices exact.
        context.prec = 80
        amounts = [fill.last_qty.as_decimal() for fill in group]
        quantity = sum(amounts, Decimal(0))
        notional = sum(
            (amount * fill.last_px.as_decimal() for amount, fill in zip(amounts, group, strict=True)),
            Decimal(0),
        )
        commission = sum((fill.commission.as_decimal() for fill in group), Decimal(0))
    return LogicalFill(
        account_id=first.account_id,
        order_id=first.client_order_id,
        instrument_id=first.instrument_id,
        side=first.order_side,
        currency=first.currency,
        fill_ts=first.ts_event,
        quantity=quantity,
        notional=notional,
        commission=commission,
        raw_fills=tuple(group),
    )
