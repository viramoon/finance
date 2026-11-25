"""Versioned daily research data under the configured Nautilus catalog root."""

from collections.abc import Iterable
from dataclasses import dataclass
from datetime import date
import fcntl
import json
import os
from pathlib import Path
import re
import shutil
from typing import Literal
from uuid import uuid4

from nautilus_trader.persistence.catalog.parquet import ParquetDataCatalog

from .collector_store import NS_PER_DAY, ensure_dir, fsync_dir
from .epoch import EPOCH, unix_nanos
from .historical_records import (
    FredDailyObservation, KenFrenchDaily, YahooDividend, YahooIndexBar, YahooSourceBar, YahooSplit,
    daily_bar, dividend, encode_record, fred_observation, french_factor, split,
)
from .historical_types import DailyBar, Dividend, EpochDiscard, Split, guard_epoch
from .research_catalog import sessions
from .research_factors import FRED_SERIES_ID, FredDaily, FrenchDaily
from .yahoo_daily import YahooDaily


_YAHOO_ID = re.compile(r"[A-Z0-9][A-Z0-9.-]{0,31}\.(XNYS|ARCA|XNAS|XASE|BATS|INDEX)")
_GENERATION = re.compile(r"generation-[0-9a-f]{32}")
WriteMode = Literal["append", "replace"]


@dataclass(frozen=True)
class WriteReport:
    series_id: str
    first: date
    last: date
    observations: int
    dividends: int
    splits: int
    dropped_pre_epoch: EpochDiscard
    missing_quote_rows: int = 0
    skipped_missing_non_sessions: int = 0


@dataclass(frozen=True)
class YahooHistory:
    series_id: str
    symbol: str
    asset_type: str
    currency: str
    bars: tuple[DailyBar, ...]
    dividends: tuple[Dividend, ...]
    splits: tuple[Split, ...]

    @property
    def is_reference(self) -> bool:
        return self.asset_type == "index"


def _bounds(start, end):
    for day in (start, end):
        if day is not None and type(day) is not date:
            raise TypeError("Historical date bounds must be datetime.date values")
    if start is not None and end is not None and end < start:
        raise ValueError("Historical end must be on or after start")


def _day(row):
    day = row.observation_date if isinstance(row, FredDailyObservation) else row.session
    return date.fromisoformat(day)


def _read_rows(catalog, record_types):
    return {
        record_type: [wrapped.data for wrapped in catalog.custom_data(record_type)]
        for record_type in record_types
    }


def _dates(groups):
    return [_day(row) for rows in groups.values() for row in rows]


def _midnight(day):
    return unix_nanos((day - EPOCH).days * NS_PER_DAY)


def _yahoo_bar_type(series_id):
    return YahooIndexBar if series_id.endswith(".INDEX") else YahooSourceBar


def _within(day, start, end):
    return (start is None or start <= day) and (end is None or day <= end)


def _current(series):
    try:
        target = (series / "current").readlink()
    except FileNotFoundError:
        return None
    snapshot = series / target
    if not _GENERATION.fullmatch(str(target)) or snapshot.is_symlink() or not snapshot.is_dir():
        raise ValueError(f"Invalid current historical snapshot for {series.name}")
    return snapshot


def _manifest(snapshot):
    return json.loads((snapshot / "dataset.json").read_text(encoding="utf-8"))


def _fsync_generation(generation):
    for parent, _, files in os.walk(generation, topdown=False):
        parent = Path(parent)
        for name in files:
            with (parent / name).open("rb") as handle:
                os.fsync(handle.fileno())
        fsync_dir(parent)


def _publish(series, previous, manifest, groups):
    # The live snapshot is never modified. Orphans from failed or superseded writes
    # can be removed while the exclusive lock keeps readers out.
    for child in series.iterdir():
        if child != previous and _GENERATION.fullmatch(child.name) and child.is_dir() and not child.is_symlink():
            shutil.rmtree(child)
    # Stable catalog paths avoid accumulating Nautilus catalog instances per revision.
    staging = series / ".staging"
    pending = series / ".pending-current"
    pending.unlink(missing_ok=True)
    if staging.exists():
        shutil.rmtree(staging)
    staging.mkdir()
    unpublished = staging
    published = False
    try:
        catalog = ParquetDataCatalog(staging)
        catalog.write_data([row for rows in groups.values() for row in rows])
        for record_type, actual in _read_rows(catalog, groups).items():
            expected = groups[record_type]
            if [row.to_dict() for row in actual] != [row.to_dict() for row in expected]:
                raise RuntimeError(f"Nautilus write verification failed for {record_type.__name__}")
        (staging / "dataset.json").write_text(json.dumps(manifest, sort_keys=True) + "\n", encoding="utf-8")
        _fsync_generation(staging)
        generation = series / f"generation-{uuid4().hex}"
        staging.rename(generation)
        unpublished = generation
        fsync_dir(series)
        pending.symlink_to(generation.name)
        os.replace(pending, series / "current")
        published = True
        fsync_dir(series)
    finally:
        if not published:
            pending.unlink(missing_ok=True)
            shutil.rmtree(unpublished)


class HistoricalCatalog:
    """Read and write independently published Yahoo, French and FRED series."""

    def __init__(self, data_root: Path | None = None):
        root = Path(os.environ.get("FERTILE_DATA_ROOT", "/home/box/fertile/data")) if data_root is None else Path(data_root)
        if not root.is_absolute():
            raise ValueError("Historical data_root must be absolute")
        self.root = root / "catalog"

    def _series(self, source, series_id):
        if source == "yahoo" and not _YAHOO_ID.fullmatch(series_id):
            raise ValueError("Expected a Yahoo catalog ID such as IBM.XNYS or GSPC.INDEX")
        if source == "fred" and not FRED_SERIES_ID.fullmatch(series_id):
            raise ValueError("Expected a FRED series ID such as DGS10")
        return self.root / "historical" / source / series_id

    def _write(
        self, source, series_id, metadata, groups, dropped, mode, start, end,
        *, missing_quote_rows=0, skipped_missing_non_sessions=0,
    ):
        if mode not in {"append", "replace"}:
            raise ValueError("mode must be 'append' or 'replace'")
        _bounds(start, end)
        primary = next(iter(groups.values()))
        if not primary:
            message = f"No observations to write for {series_id}; discarded pre-epoch: {dropped}"
            if skipped_missing_non_sessions:
                message += f"; skipped empty non-session rows: {skipped_missing_non_sessions}"
            raise ValueError(message)
        observations = [_day(row) for row in primary]
        if len(set(observations)) != len(observations):
            raise ValueError(f"Duplicate observation date in {series_id}")
        incoming_days = _dates(groups)
        start = min(incoming_days) if start is None else start
        end = max(incoming_days) if end is None else end
        if min(incoming_days) < start or max(incoming_days) > end:
            raise ValueError(f"Incoming {series_id} data falls outside the write range")

        manifest = dict(metadata, schema=1, source=source, series_id=series_id)
        series = self._series(source, series_id)
        ensure_dir(series)
        with (series / ".lock").open("a") as handle:
            fcntl.flock(handle, fcntl.LOCK_EX)
            previous = _current(series)
            if previous is not None:
                if _manifest(previous) != manifest:
                    raise ValueError(f"Historical provenance differs for {series_id}")
                existing = _read_rows(ParquetDataCatalog(series / "current"), groups)
                old_days = _dates(existing)
                if mode == "append" and old_days and start <= max(old_days) and min(old_days) <= end:
                    raise ValueError(f"Append range overlaps the stored range for {series_id}; use replace")
                merged = {
                    kind: [row for row in existing[kind] if not _within(_day(row), start, end)] + incoming
                    for kind, incoming in groups.items()
                }
            else:
                merged = groups
            for rows in merged.values():
                rows.sort(key=lambda row: row.ts_init)
            _publish(series, previous, manifest, merged)

        return WriteReport(
            series_id, min(observations), max(observations), len(primary),
            len(groups.get(YahooDividend, ())), len(groups.get(YahooSplit, ())), dropped,
            missing_quote_rows=missing_quote_rows, skipped_missing_non_sessions=skipped_missing_non_sessions,
        )

    def _read(self, source, series_id, record_types, start, end):
        _bounds(start, end)
        series = self._series(source, series_id)
        with (series / ".lock").open("r") as handle:
            fcntl.flock(handle, fcntl.LOCK_SH)
            snapshot = _current(series)
            if snapshot is None:
                raise FileNotFoundError(f"No historical catalog for {series_id}")
            manifest = _manifest(snapshot)
            if (manifest.get("schema"), manifest.get("source"), manifest.get("series_id")) != (1, source, series_id):
                raise ValueError(f"Invalid historical provenance for {series_id}")
            groups = _read_rows(ParquetDataCatalog(series / "current"), record_types)
        return manifest, {
            kind: [row for row in rows if _within(_day(row), start, end)] for kind, rows in groups.items()
        }

    def write_yahoo(
        self, batch: YahooDaily, *, mode: WriteMode = "append", start: date | None = None, end: date | None = None,
    ) -> WriteReport:
        bar_type = _yahoo_bar_type(batch.series_id)
        if (batch.asset_type == "index") != (bar_type is YahooIndexBar):
            raise ValueError("Yahoo index data must use an INDEX reference ID")
        bars, _ = guard_epoch(batch.bars, date_of=lambda row: row.session, policy="reject")
        calendar = sessions(min(row.session for row in bars), max(row.session for row in bars)) if bars else {}
        quote_records = []
        missing_quote_rows = 0
        skipped_missing_non_sessions = 0
        for row in bars:
            when = calendar.get(row.session)
            missing_quote = row.is_missing_quote
            if when is None:
                if missing_quote and row.adjusted_close is None:
                    skipped_missing_non_sessions += 1
                    continue
                raise ValueError(f"Yahoo daily bar date {row.session} is not an XNYS session")
            missing_quote_rows += missing_quote
            quote_records.append(encode_record(bar_type, row, when))

        def action_records(record_type, events: Iterable[Dividend | Split]):
            events, _ = guard_epoch(events, date_of=lambda row: row.session, policy="reject")
            return [encode_record(record_type, row, unix_nanos(row.source_timestamp * 10**9)) for row in events]

        metadata = dict(
            symbol=batch.symbol, asset_type=batch.asset_type, currency=batch.currency,
            calendar="XNYS", price_basis=DailyBar.price_basis, is_reference=batch.asset_type == "index",
        )
        groups = {
            bar_type: quote_records,
            YahooDividend: action_records(YahooDividend, batch.dividends),
            YahooSplit: action_records(YahooSplit, batch.splits),
        }
        return self._write(
            "yahoo", batch.series_id, metadata, groups, batch.dropped_pre_epoch, mode, start, end,
            missing_quote_rows=missing_quote_rows, skipped_missing_non_sessions=skipped_missing_non_sessions,
        )

    def read_yahoo(self, series_id: str, *, start: date | None = None, end: date | None = None) -> YahooHistory:
        bar_type = _yahoo_bar_type(series_id)
        manifest, groups = self._read("yahoo", series_id, (bar_type, YahooDividend, YahooSplit), start, end)
        return YahooHistory(
            series_id, manifest["symbol"], manifest["asset_type"], manifest["currency"],
            tuple(map(daily_bar, groups[bar_type])), tuple(map(dividend, groups[YahooDividend])),
            tuple(map(split, groups[YahooSplit])),
        )

    def write_french(
        self, batch: FrenchDaily, *, mode: WriteMode = "append", start: date | None = None, end: date | None = None,
    ) -> WriteReport:
        rows, _ = guard_epoch(batch.rows, date_of=lambda row: row.session, policy="reject")
        groups = {KenFrenchDaily: [encode_record(KenFrenchDaily, row, _midnight(row.session)) for row in rows]}
        return self._write("french", "daily", {"units": "daily_fraction"}, groups, batch.dropped_pre_epoch, mode, start, end)

    def read_french(self, *, start: date | None = None, end: date | None = None) -> FrenchDaily:
        _, groups = self._read("french", "daily", (KenFrenchDaily,), start, end)
        return FrenchDaily(tuple(map(french_factor, groups[KenFrenchDaily])), EpochDiscard())

    def write_fred(
        self, batch: FredDaily, *, mode: WriteMode = "append", start: date | None = None, end: date | None = None,
    ) -> WriteReport:
        rows, _ = guard_epoch(batch.rows, date_of=lambda row: row.observation_date, policy="reject")
        groups = {FredDailyObservation: [
            encode_record(FredDailyObservation, row, _midnight(row.observation_date)) for row in rows
        ]}
        return self._write("fred", batch.series_id, {"units": "source"}, groups, batch.dropped_pre_epoch, mode, start, end)

    def read_fred(self, series_id: str, *, start: date | None = None, end: date | None = None) -> FredDaily:
        _, groups = self._read("fred", series_id, (FredDailyObservation,), start, end)
        return FredDaily(series_id, tuple(map(fred_observation, groups[FredDailyObservation])), EpochDiscard())
