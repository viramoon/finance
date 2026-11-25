"""Summarize native fills for same-currency cash equity research."""

from collections import defaultdict
from collections.abc import Iterable, Mapping
from dataclasses import dataclass, field
from datetime import date
from decimal import Decimal, localcontext

from .research_decimal import EXACT_RESEARCH_CONTEXT, RESEARCH_CONTEXT

# Native fixed-point types compute their scale when first imported.
with localcontext(RESEARCH_CONTEXT):
    from nautilus_trader.model.enums import OrderSide
    from nautilus_trader.model.events import OrderFilled
    from nautilus_trader.model.identifiers import AccountId, ClientOrderId, InstrumentId, TradeId
    from nautilus_trader.model.objects import Currency

    from .epoch import unix_nanos


@dataclass(frozen=True)
class FillPart:
    trade_id: TradeId
    fill_ts: int
    quantity: Decimal
    price: Decimal
    commission: Decimal


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
    parts: tuple[FillPart, ...]
    raw_fills: tuple[OrderFilled, ...] = field(compare=False, repr=False)

    @property
    def average_price(self) -> Decimal:
        """Volume-weighted price for display, without rounding to a price tick."""
        try:
            with localcontext(RESEARCH_CONTEXT, prec=28):
                return self.notional / self.quantity
        except ArithmeticError as exc:
            raise ValueError(f"Cannot represent average price for order {self.order_id}") from exc

    @property
    def position_delta(self) -> Decimal:
        return self.quantity if self.side == OrderSide.BUY else self.quantity.copy_negate()

    @property
    def cash_delta(self) -> Decimal:
        try:
            with localcontext(EXACT_RESEARCH_CONTEXT, prec=80):
                proceeds = self.notional.copy_negate() if self.side == OrderSide.BUY else self.notional
                return proceeds - self.commission
        except ArithmeticError as exc:
            raise ValueError(f"Cash delta for order {self.order_id} exceeds exact Decimal range") from exc


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
    # Native fixed-point products and sums must remain exact.
    try:
        with localcontext(EXACT_RESEARCH_CONTEXT, prec=80):
            parts = tuple(
                FillPart(event.trade_id, event.ts_event, event.last_qty.as_decimal(),
                         event.last_px.as_decimal(), event.commission.as_decimal())
                for event in group
            )
            quantity = sum((part.quantity for part in parts), Decimal(0))
            notional = sum((part.quantity * part.price for part in parts), Decimal(0))
            commission = sum((part.commission for part in parts), Decimal(0))
    except ArithmeticError as exc:
        raise ValueError(f"Fills for order {first.client_order_id} exceed exact Decimal range") from exc
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
        parts=parts,
        raw_fills=tuple(group),
    )
