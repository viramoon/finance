"""Validate source timestamps before handing them to Nautilus unsigned fields."""

from collections.abc import Callable, Iterable
from dataclasses import dataclass
from datetime import date

import pandas as pd
from nautilus_trader.core.datetime import as_utc_index


EPOCH = date(1970, 1, 1)
MAX_UNIX_NS = 2**63 - 1


def unix_nanos(value):
    if type(value) is not int or not 0 <= value <= MAX_UNIX_NS:
        raise ValueError("expected Unix nanoseconds at or after the epoch, within int64")
    return value


@dataclass(frozen=True)
class DroppedTimestamps:
    count: int
    earliest_ns: int | None
    latest_ns: int | None


def filter_pre_epoch[T](
    rows: Iterable[T],
    *,
    timestamp: Callable[[T], int],
    mode: str = "reject",
) -> tuple[list[T], DroppedTimestamps]:
    """Check signed UTC nanoseconds and preserve the original order of accepted rows."""
    if mode not in {"reject", "drop"}:
        raise ValueError("mode must be 'reject' or 'drop'")
    accepted = []
    count = 0
    earliest = latest = None
    for row in rows:
        when = timestamp(row)
        if type(when) is int and -MAX_UNIX_NS - 1 <= when < 0 and mode == "drop":
            if earliest is None:
                earliest = latest = when
            else:
                earliest, latest = min(earliest, when), max(latest, when)
            count += 1
        else:
            unix_nanos(when)
            accepted.append(row)
    return accepted, DroppedTimestamps(count, earliest, latest)


def require_bar_timestamps(bars):
    """Reject invalid or already wrapped event/init times on native Bar objects."""
    for bar in bars:
        unix_nanos(bar.ts_event)
        unix_nanos(bar.ts_init)


def process_bar_frame(process, frame, *, mode="reject", ts_init_delta=0, **kwargs):
    """Guard a DatetimeIndex before invoking a bound Nautilus wrangler process method."""
    if not isinstance(frame.index, pd.DatetimeIndex) or frame.index.hasnans:
        raise ValueError("bar data must have a DatetimeIndex without NaT")
    unix_nanos(ts_init_delta)
    # Use Nautilus's UTC and nanosecond normalization before reading signed values.
    frame = as_utc_index(frame)
    ns = frame.index.asi8.tolist()
    keep, dropped = filter_pre_epoch(range(len(ns)), timestamp=ns.__getitem__, mode=mode)
    if not keep:
        return [], dropped
    unix_nanos(max(ns[i] for i in keep) + ts_init_delta)
    bars = process(frame.iloc[keep] if dropped.count else frame, ts_init_delta=ts_init_delta, **kwargs)
    require_bar_timestamps(bars)
    return bars, dropped
