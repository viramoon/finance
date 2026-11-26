"""Reimport legacy fixed-21:00 UTC SPY daily bars into a separate data root."""

import argparse
from dataclasses import asdict, replace
from datetime import date, datetime, timedelta, timezone
import json
import os
from pathlib import Path
import sys
from tempfile import TemporaryDirectory

from nautilus_trader.model.data import BarType
from nautilus_trader.model.enums import BarAggregation
from nautilus_trader.persistence.catalog.parquet import ParquetDataCatalog

from .collector_store import NS_PER_DAY, ensure_dir, fsync_dir
from .epoch import EPOCH, unix_nanos
from .historical_catalog import HistoricalCatalog, WriteReport
from .research_catalog import nyse, sessions
from .yahoo_daily import load_yahoo_daily


_DEFAULT_BAR_TYPE = "SPY.ARCA-1-DAY-LAST-EXTERNAL"
_SERIES = "SPY.ARCA"
_NS_PER_SECOND = 10**9
_FIXED_CLOSE_NS = 21 * 3600 * _NS_PER_SECOND
_DAYLIGHT_CLOSE_NS = 20 * 3600 * _NS_PER_SECOND


def _legacy_days(source, bar_type):
    if (
        bar_type.instrument_id.symbol.value != "SPY"
        or bar_type.spec.step != 1
        or bar_type.spec.aggregation != BarAggregation.DAY
    ):
        raise ValueError("The legacy bar type must identify a one-day SPY series")
    if not source.is_dir():
        raise ValueError(f"Legacy Nautilus catalog directory does not exist: {source}")
    bars = ParquetDataCatalog(source).bars(bar_types=[str(bar_type)])
    if not bars:
        raise ValueError(f"No {bar_type} bars found in legacy catalog {source}")
    days = set()
    for bar in bars:
        day_offset, time_ns = divmod(unix_nanos(bar.ts_event), NS_PER_DAY)
        day = EPOCH + timedelta(days=day_offset)
        if time_ns != _FIXED_CLOSE_NS:
            raise ValueError(f"Legacy bar on {day} has a ts_event other than 21:00 UTC")
        if day in days:
            raise ValueError(f"Duplicate legacy SPY session: {day}")
        days.add(day)
    return days


def _utc_close(timestamp_ns):
    close = datetime.fromtimestamp(timestamp_ns // _NS_PER_SECOND, tz=timezone.utc)
    return close.isoformat().replace("+00:00", "Z")


def _legacy_samples(days, calendar):
    early_closes = {day.date() for day in nyse().early_closes}
    samples = {}
    for day in sorted(days):
        close = calendar.get(day)
        if close is None:
            raise ValueError(f"Legacy SPY date {day} is not an XNYS session")
        if day in early_closes:
            session_type = "early_close"
        elif close % NS_PER_DAY == _DAYLIGHT_CLOSE_NS:
            session_type = "daylight_time"
        elif close % NS_PER_DAY == _FIXED_CLOSE_NS:
            session_type = "standard_time"
        else:
            continue
        samples.setdefault(session_type, {
            "date": day,
            "legacy_ts_event": f"{day}T21:00:00Z",
            "xnys_close": _utc_close(close),
        })
    required = {"standard_time", "daylight_time", "early_close"}
    if missing := required - samples.keys():
        raise ValueError("Insufficient evidence of fixed 21:00 UTC bars; missing legacy XNYS samples for "
                         + ", ".join(sorted(missing)))
    return samples


def _target_series(source, target_root):
    if source.is_relative_to(target_root) or target_root.is_relative_to(source):
        raise ValueError("Target data root and legacy catalog must be separate directories")
    if target_root.exists() and not target_root.is_dir():
        raise ValueError(f"Target data root is not a directory: {target_root}")
    parent = target_root
    for name in ("catalog", "historical", "yahoo"):
        parent /= name
        if parent.is_symlink() or (parent.exists() and not parent.is_dir()):
            raise ValueError(f"Target catalog path must be a real directory: {parent}")
    series = parent / _SERIES
    if series.exists() or series.is_symlink():
        raise ValueError(f"Target SPY series already exists: {series}")
    while not parent.exists():
        parent = parent.parent
    if not parent.is_dir() or not os.access(parent, os.W_OK | os.X_OK):
        raise ValueError(f"Cannot create the target catalog under {parent}")
    return series


def _import_batch(archive, adjustment_as_of, legacy_days, calendar):
    batch = load_yahoo_daily(archive, adjustment_as_of=adjustment_as_of)
    if batch.series_id != _SERIES or batch.asset_type != "etf":
        raise ValueError(f"Expected Yahoo USD ETF archive for {_SERIES}; found {batch.series_id}")

    first, last = min(legacy_days), max(legacy_days)
    bars = tuple(row for row in batch.bars if first <= row.session <= last)
    observed = set()
    for row in bars:
        if row.session not in calendar and (not row.is_missing_quote or row.adjusted_close is not None):
            raise ValueError(f"Yahoo daily bar date {row.session} is not an XNYS session")
        if not row.is_missing_quote:
            observed.add(row.session)
    if missing := sorted(legacy_days - observed):
        samples = ", ".join(str(day) for day in missing[:5])
        raise ValueError(f"Yahoo archive lacks source OHLCV for {len(missing)} legacy sessions; examples: {samples}")
    return replace(
        batch,
        bars=bars,
        dividends=tuple(event for event in batch.dividends if first <= event.session <= last),
        splits=tuple(event for event in batch.splits if first <= event.session <= last),
    )


def _write_new_series(series, batch):
    ensure_dir(series.parent)
    # Stage on the target filesystem so a failed import leaves the final series absent.
    with TemporaryDirectory(prefix=".migrate-spy-", dir=series.parent) as staging:
        staging_root = Path(staging)
        report = HistoricalCatalog(staging_root).write_yahoo(batch)
        published = staging_root / "catalog" / "historical" / "yahoo" / _SERIES
        published.rename(series)
        fsync_dir(series.parent)
    return report


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-catalog", type=Path, required=True, help="legacy Nautilus Parquet catalog directory")
    parser.add_argument("--bar-type", default=_DEFAULT_BAR_TYPE, help=f"legacy one-day SPY bar type (default: {_DEFAULT_BAR_TYPE})")
    parser.add_argument("--target-data-root", type=Path, required=True, help="separate root for a new catalog/historical Yahoo series")
    parser.add_argument("--yahoo-json", type=Path, required=True, help="saved Yahoo SPY daily chart JSON with source quotes and actions")
    parser.add_argument("--adjustment-as-of", type=date.fromisoformat, required=True, help="archive snapshot date (YYYY-MM-DD)")
    parser.add_argument("--dry-run", action="store_true", help="validate inputs and report the planned import without writing")
    args = parser.parse_args(argv)

    try:
        source = args.source_catalog.resolve()
        target_root = args.target_data_root.resolve()
        series = _target_series(source, target_root)
        bar_type = BarType.from_str(args.bar_type)
        legacy_days = _legacy_days(source, bar_type)
        first, last = min(legacy_days), max(legacy_days)
        calendar = sessions(first, last)
        samples = _legacy_samples(legacy_days, calendar)
        batch = _import_batch(args.yahoo_json, args.adjustment_as_of, legacy_days, calendar)
        bars = [row for row in batch.bars if row.session in calendar]
        if args.dry_run:
            report = WriteReport(
                series_id=batch.series_id, first=bars[0].session, last=bars[-1].session, observations=len(bars),
                dividends=len(batch.dividends), splits=len(batch.splits), dropped_pre_epoch=batch.dropped_pre_epoch,
                missing_quote_rows=sum(row.is_missing_quote for row in bars),
                skipped_missing_non_sessions=len(batch.bars) - len(bars),
            )
        else:
            report = _write_new_series(series, batch)
    except (OSError, ValueError, RuntimeError, OverflowError) as exc:
        print(f"Cannot migrate SPY: {exc}", file=sys.stderr)
        return 1

    print(json.dumps({
        "status": "would_migrate" if args.dry_run else "migrated",
        "source": {
            "catalog": source, "instrument": str(bar_type.instrument_id), "bar_type": str(bar_type),
            "first": first, "last": last, "observations": len(legacy_days),
            "expected_sessions": len(calendar), "missing_sessions": len(calendar) - len(legacy_days),
        },
        "target": {
            "data_root": target_root, "series_path": series, **asdict(report),
            "expected_sessions": len(calendar), "missing_sessions": len(calendar) - report.observations,
            "sessions_with_quotes": report.observations - report.missing_quote_rows,
            "missing_adjusted_close_rows": sum(row.adjusted_close is None for row in bars),
        },
        "yahoo_json": args.yahoo_json.resolve(),
        "adjustment_as_of": batch.adjustment_as_of,
        "session_close_samples": samples,
    }, indent=2, default=str))
    return 0


if __name__ == "__main__":
    sys.exit(main())
