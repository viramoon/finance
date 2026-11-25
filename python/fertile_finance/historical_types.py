"""Exact source records shared by the historical daily adapters and catalog."""

from collections.abc import Callable, Iterable
from dataclasses import dataclass
from datetime import date
from decimal import Context, Decimal, ROUND_HALF_EVEN, localcontext
from typing import ClassVar, Literal


EPOCH = date(1970, 1, 1)
EpochPolicy = Literal["reject", "drop"]
ADJUSTMENT_CONTEXT = Context(prec=34, rounding=ROUND_HALF_EVEN)


@dataclass(frozen=True)
class EpochDiscard:
    count: int = 0
    first: date | None = None
    last: date | None = None


def guard_epoch[T](
    rows: Iterable[T], *, date_of: Callable[[T], date], policy: EpochPolicy,
) -> tuple[tuple[T, ...], EpochDiscard]:
    """Reject or count pre-epoch dates before conversion to Nautilus nanoseconds."""
    if policy not in {"reject", "drop"}:
        raise ValueError("pre_epoch must be 'reject' or 'drop'")
    kept = []
    count = 0
    first = last = None
    for row in rows:
        day = date_of(row)
        if day >= EPOCH:
            kept.append(row)
        elif policy == "reject":
            raise ValueError(f"Pre-epoch observation {day} is before 1970-01-01; use pre_epoch='drop' to discard it")
        else:
            count += 1
            first = day if first is None else min(first, day)
            last = day if last is None else max(last, day)
    return tuple(kept), EpochDiscard(count, first, last)


@dataclass(frozen=True)
class AdjustedDailyBar:
    session: date
    open: Decimal | None
    high: Decimal | None
    low: Decimal | None
    close: Decimal | None
    volume: Decimal | None
    adjustment_as_of: date
    ts_event: int | None
    price_basis: ClassVar[str] = "yahoo_chart_adjusted_close_ratio"

    @property
    def ts_init(self) -> int | None:
        return self.ts_event


@dataclass(frozen=True)
class DailyBar:
    session: date
    open: Decimal | None
    high: Decimal | None
    low: Decimal | None
    close: Decimal | None
    volume: Decimal | None
    adjustment_factor: Decimal | None
    adjusted_close: Decimal | None
    adjustment_as_of: date
    ts_event: int | None = None
    price_basis: ClassVar[str] = "yahoo_chart_quote"

    @property
    def is_missing_quote(self) -> bool:
        return all(value is None for value in (self.open, self.high, self.low, self.close, self.volume))

    @property
    def ts_init(self) -> int | None:
        return self.ts_event

    def adjusted(self) -> AdjustedDailyBar | None:
        """Apply the archived factor to OHLC and leave source volume intact."""
        if self.adjustment_factor is None:
            return None
        with localcontext(ADJUSTMENT_CONTEXT):
            open_, high, low, close = (
                None if price is None else price * self.adjustment_factor
                for price in (self.open, self.high, self.low, self.close)
            )
        return AdjustedDailyBar(
            self.session, open_, high, low, close, self.volume, self.adjustment_as_of, self.ts_event,
        )


@dataclass(frozen=True)
class Dividend:
    session: date
    source_timestamp: int
    amount: Decimal
    source_id: str | None = None


@dataclass(frozen=True)
class Split:
    session: date
    source_timestamp: int
    numerator: int
    denominator: int
    source_id: str | None = None
