"""Decode pinned, locally supplied cash-equity session replay data."""

from collections import defaultdict
from dataclasses import dataclass
from datetime import date, timedelta
import hashlib
import json
import re

from .portfolio_inputs import PortfolioAsset, UniverseSnapshot
from .research_catalog import ResearchError, ID, bar_type, cash_equity, iso_date, nyse, price
from .strategy_plan import SHA256, canonical_sha256

from nautilus_trader.model.data import Bar
from nautilus_trader.model.objects import Quantity

from .epoch import unix_nanos


REPLAY_SCHEMA = "fertile.finance/strategy-replay/v1"
MAX_REPLAY_BYTES = 32 * 1024 * 1024
_UNIX_NANOS = re.compile(r"(?:0|[1-9][0-9]{0,18})")
_VOLUME = re.compile(r"[0-9]{1,12}")
_PRICES = ("open", "high", "low", "close")


@dataclass(frozen=True)
class StrategyReplay:
    assets: tuple[PortfolioAsset, ...]
    membership: tuple[UniverseSnapshot, ...]
    history_start: date
    start: date
    end: date
    data: dict
    calendar: dict
    availability: dict


def _unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("Duplicate replay field")
        result[key] = value
    return result


def _object(value, fields):
    if not isinstance(value, dict) or value.keys() != set(fields):
        raise ValueError("Invalid replay object fields")
    return value


def _array(value):
    if not isinstance(value, list):
        raise ValueError("Expected replay array")
    return value


def _nanos(text):
    if not isinstance(text, str) or not _UNIX_NANOS.fullmatch(text):
        raise ValueError("Expected an exact Unix nanosecond string")
    return unix_nanos(int(text))


def _session_time(schedule, day, field):
    return unix_nanos(schedule.at[day.isoformat(), field].value)


def _membership(rows, instruments, schedule):
    snapshots = {}
    for row in _array(rows):
        row = _object(row, ("date", "first_available_ts", "members"))
        day = iso_date(row["date"])
        available = _nanos(row["first_available_ts"])
        members = _array(row["members"])
        if (day in snapshots or available > _session_time(schedule, day, "open")
                or any(type(key) is not str or key not in instruments for key in members)
                or len(members) != len(set(members))):
            raise ValueError("Invalid point-in-time membership")
        snapshots[day] = UniverseSnapshot(day, available, tuple(sorted(members)))
    return tuple(snapshots[day] for day in sorted(snapshots))


def _bars(rows, instruments, schedule, first, last, replay_end):
    bars = defaultdict(list)
    digest_rows = defaultdict(list)
    seen = set()
    late = 0
    for row in _array(rows):
        row = _object(row, ("instrument_id", "date", "first_available_ts", *_PRICES, "volume"))
        key, day = row["instrument_id"], iso_date(row["date"])
        if key not in instruments or not first <= day <= last or (key, day) in seen:
            raise ValueError("Unknown instrument, out-of-coverage bar or duplicate bar")
        seen.add((key, day))
        volume = row["volume"]
        if not isinstance(volume, str) or not _VOLUME.fullmatch(volume):
            raise ValueError("Expected a whole-share volume string")
        prices = tuple(price(row[field]) for field in _PRICES)
        opening, high, low, close = (item.raw for item in prices)
        if not low <= min(opening, close) <= max(opening, close) <= high:
            raise ValueError("Invalid OHLC bounds")
        quantity = Quantity.from_int(int(volume))
        when = _session_time(schedule, day, "close")
        available = _nanos(row["first_available_ts"])
        if available < when:
            raise ValueError("Closed bar cannot be available before its session close")
        digest_rows[key].append(row)
        if day <= replay_end:
            if available == when:
                bars[key].append(Bar(instruments[key], *prices, quantity, when, when))
            else:
                # The v1 replay consumes the exact row available at the decision close.
                # A late release cannot create a retroactive signal or execution quote.
                late += 1
    return bars, digest_rows, late


def _decode(raw, digest, plan, start, end):
    if not isinstance(digest, str) or not SHA256.fullmatch(digest) or len(raw) > MAX_REPLAY_BYTES:
        raise ValueError("Invalid replay size or digest")
    if hashlib.sha256(raw).hexdigest() != digest:
        raise ValueError("Pinned replay digest mismatch")

    spec = plan["bundle"]["spec"]
    metadata = {
        "schema": REPLAY_SCHEMA,
        "bars_ref": spec["data"]["bars"],
        "universe_ref": spec["universe"]["ref"],
        "calendar": spec["clock"]["calendar"],
        "currency": spec["data"]["currency"],
        "timeframe": spec["data"]["timeframe"],
        "price_basis": spec["data"]["price_basis"],
    }
    if plan["inputs"]["bars"]["id"] != metadata["bars_ref"] or (
        plan["inputs"]["universe"]["id"] != metadata["universe_ref"]
    ):
        raise ValueError("Replay references differ from the compiled plan")
    doc = _object(json.loads(raw.decode("utf-8"), object_pairs_hook=_unique_object), (
        *metadata, "first_session", "last_session", "instruments", "membership", "bars",
    ))
    if any(doc[key] != value for key, value in metadata.items()):
        raise ValueError("Replay metadata differs from the compiled plan")

    calendar = nyse()
    schedule = calendar.schedule
    first, last = iso_date(doc["first_session"]), iso_date(doc["last_session"])
    if first > last:
        raise ValueError("Invalid replay coverage")
    for day in (first, last):
        _session_time(schedule, day, "close")
    requested = schedule.loc[str(start):str(end - timedelta(days=1))]
    if len(requested) < 2:
        raise ResearchError("no_data", "Require at least two sessions in the requested interval")
    replay_start, replay_end = requested.index[0].date(), requested.index[-1].date()
    if not first <= replay_start <= replay_end <= last:
        raise ResearchError("strategy_snapshot_range", "Requested sessions are outside the replay coverage")

    keys = _array(doc["instruments"])
    if not keys or any(type(key) is not str or not ID.fullmatch(key) for key in keys) or len(set(keys)) != len(keys):
        raise ValueError("Expected unique USD XNYS cash-equity IDs")
    instruments = {key: bar_type(key) for key in sorted(keys)}
    snapshots = _membership(doc["membership"], instruments, schedule)
    if not snapshots or snapshots[0].date > first or snapshots[-1].date > last:
        raise ValueError("Membership must cover the replay range")
    bars, digest_rows, late = _bars(doc["bars"], instruments, schedule, first, last, replay_end)
    assets = tuple(
        PortfolioAsset(cash_equity(key), tuple(sorted(bars[key], key=lambda bar: bar.ts_event)), True)
        for key in instruments
    )
    history = schedule.loc[str(first):str(replay_end)]
    session_clocks = [
        (label.date().isoformat(), str(opening.value), str(closing.value))
        for label, opening, closing in zip(history.index, history["open"], history["close"], strict=True)
    ]
    data = {
        **metadata,
        "snapshot_sha256": digest,
        "first_session": str(first),
        "last_session": str(last),
        "partitions": [
            {
                "instrument_id": key,
                "rows": len(digest_rows[key]),
                "sha256": canonical_sha256(sorted(digest_rows[key], key=lambda row: row["date"])),
            }
            for key in instruments
        ],
        "membership_sha256": canonical_sha256([
            (str(snapshot.date), str(snapshot.available_ts), snapshot.members) for snapshot in snapshots
        ]),
        "instrument_master": {"ids": list(instruments), "price_increment": "0.01", "lot_size": "1"},
    }
    calendar_manifest = {
        "id": calendar.name,
        "timezone": str(calendar.tz),
        "sessions_sha256": canonical_sha256(session_clocks),
        "first_session": session_clocks[0][0],
        "last_session": session_clocks[-1][0],
    }
    availability = {
        "timestamp_unit": "utc_unix_nanoseconds",
        "membership": "first_available_ts_at_or_before_effective_session_open",
        "bars": "first_available_ts_equal_to_session_close",
        "excluded_late_bars": late,
    }
    return StrategyReplay(
        assets, tuple(snapshot for snapshot in snapshots if snapshot.date <= replay_end),
        first, replay_start, replay_end, data, calendar_manifest, availability,
    )


def load_strategy_replay(raw, digest, plan, start, end):
    try:
        return _decode(raw, digest, plan, start, end)
    except ResearchError as exc:
        if exc.code in {"no_data", "strategy_snapshot_range"}:
            raise
        raise ResearchError("strategy_snapshot_invalid", "Invalid local replay data") from None
    except (ArithmeticError, ValueError, TypeError, KeyError, RecursionError):
        raise ResearchError("strategy_snapshot_invalid", "Invalid local replay data") from None
