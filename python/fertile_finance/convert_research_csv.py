"""Export Yahoo source quotes as CSV for the reviewed official research catalog."""

import argparse
import csv
from decimal import Decimal
import hashlib
import io
import json
import os
from pathlib import Path
from tempfile import NamedTemporaryFile
from time import time_ns

from .collector_store import fsync_dir
from .historical_catalog import HistoricalCatalog
from .research_catalog import FIELDS, ID, iso_date, parse_rows, sessions
from .yahoo_daily import load_yahoo_daily


def _in_range(day, start, end):
    return (start is None or start <= day) and (end is None or day <= end)


def _source_prices(row):
    for field in FIELDS[1:5]:
        value = getattr(row, field)
        if value is None or not value.is_finite() or not 0 < value <= 1_000_000:
            raise ValueError(f"Invalid source quote {field} on {row.session}")
        cents = format(value, ".2f")
        if Decimal(cents) != value:
            raise ValueError(f"Source quote {field} on {row.session} cannot be represented exactly in cents")
        yield cents


def _render(rows):
    buffer = io.StringIO(newline="")
    writer = csv.writer(buffer, lineterminator="\n")
    writer.writerow(FIELDS)
    for row in rows:
        volume = row.volume
        if (volume is None or not volume.is_finite() or not 0 <= volume < 10**12
                or volume != volume.to_integral_value()):
            raise ValueError(f"Source quote volume on {row.session} must be a whole share count of at most 12 digits")
        writer.writerow((row.session, *_source_prices(row), int(volume)))
    raw = buffer.getvalue().encode("utf-8")
    parse_rows(raw, "historical")
    return raw


def _publish(output, raw):
    with NamedTemporaryFile("wb", dir=output.parent, prefix=".research-csv-", delete=False) as handle:
        staging = Path(handle.name)
        try:
            handle.write(raw)
            handle.flush()
            os.fsync(handle.fileno())
            # Publish a complete CSV without replacing an existing file or symlink.
            os.link(staging, output)
        except FileExistsError:
            raise FileExistsError(f"Output already exists: {output}") from None
        finally:
            staging.unlink()
    fsync_dir(output.parent)


def _export(source, args, *, source_kind, source_path):
    split_days = sorted({event.session for event in source.splits if _in_range(event.session, args.start, args.end)})
    if split_days:
        dates = ", ".join(map(str, split_days))
        raise ValueError(f"Yahoo splits on {dates}; select a split-free range with --start and --end")
    if not ID.fullmatch(source.series_id) or source.asset_type not in {"stock", "etf"} or source.currency != "USD":
        raise ValueError(f"{source.series_id} is ineligible for the official catalog; choose an XNYS-listed USD stock or ETF")

    selected = sorted((row for row in source.bars if _in_range(row.session, args.start, args.end)),
                      key=lambda row: row.session)
    if not selected:
        raise ValueError("No Yahoo daily observations in the selected interval")
    calendar = sessions(selected[0].session, selected[-1].session)
    rows = []
    missing_quote_rows = 0
    skipped_missing_non_sessions = 0
    now = time_ns()
    for row in selected:
        when = calendar.get(row.session)
        if when is None:
            if row.is_missing_quote and row.adjusted_close is None:
                skipped_missing_non_sessions += 1
                continue
            raise ValueError(f"Yahoo daily bar date {row.session} is not an XNYS session")
        if when > now:
            raise ValueError(f"Historical session has not closed: {row.session}")
        if row.is_missing_quote:
            missing_quote_rows += 1
            continue
        rows.append(row)
    if not rows:
        raise ValueError("No complete Yahoo quote OHLCV in the selected interval")

    first, last = rows[0].session, rows[-1].session
    raw = _render(rows)
    _publish(args.output, raw)
    return {
        "source": "Yahoo Finance chart",
        "source_kind": source_kind,
        "source_path": str(source_path),
        "series_id": source.series_id,
        "asset_type": source.asset_type,
        "price_basis": rows[0].price_basis,
        "adjustment_as_of": sorted({str(row.adjustment_as_of) for row in rows}),
        "requested_start": args.start.isoformat() if args.start else None,
        "requested_end": args.end.isoformat() if args.end else None,
        "first_session": str(first),
        "last_session": str(last),
        "observations": len(rows),
        "missing_sessions": sum(first <= day <= last for day in calendar) - len(rows),
        "missing_quote_rows": missing_quote_rows,
        "skipped_missing_non_sessions": skipped_missing_non_sessions,
        "sha256": hashlib.sha256(raw).hexdigest(),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    inputs = parser.add_mutually_exclusive_group(required=True)
    inputs.add_argument("--yahoo-json", type=Path, help="Saved Yahoo chart daily JSON")
    inputs.add_argument("--historical-data-root", type=Path, help="Absolute data root for the source HistoricalCatalog")
    parser.add_argument("--adjustment-as-of", type=iso_date, metavar="YYYY-MM-DD", help="Snapshot date of --yahoo-json")
    parser.add_argument("--series-id", help="Yahoo series ID in --historical-data-root, such as IBM.XNYS")
    parser.add_argument("--start", type=iso_date, metavar="YYYY-MM-DD", help="Inclusive selection start")
    parser.add_argument("--end", type=iso_date, metavar="YYYY-MM-DD", help="Inclusive selection end")
    parser.add_argument("--output", required=True, type=Path, metavar="CSV", help="New CSV path; JSON lineage goes to stdout")
    args = parser.parse_args()

    if args.yahoo_json is not None and (args.adjustment_as_of is None or args.series_id is not None):
        parser.error("--yahoo-json requires --adjustment-as-of and excludes --series-id")
    if args.historical_data_root is not None and (args.series_id is None or args.adjustment_as_of is not None):
        parser.error("--historical-data-root requires --series-id and excludes --adjustment-as-of")
    if (args.start is None) != (args.end is None) or (args.start is not None and args.end < args.start):
        parser.error("Supply both --start and --end as an ordered inclusive interval")

    try:
        if args.yahoo_json is not None:
            source_path = args.yahoo_json.resolve(strict=True)
            source = load_yahoo_daily(source_path, adjustment_as_of=args.adjustment_as_of)
            lineage = _export(source, args, source_kind="yahoo_archive", source_path=source_path)
        else:
            catalog = HistoricalCatalog(args.historical_data_root)
            source = catalog.read_yahoo(args.series_id, start=args.start, end=args.end)
            source_path = catalog.root / "historical" / "yahoo" / source.series_id
            lineage = _export(source, args, source_kind="historical_catalog", source_path=source_path)
        print(json.dumps(lineage, sort_keys=True))
    except (OSError, ValueError) as exc:
        parser.exit(1, f"{exc}\n")


if __name__ == "__main__":
    main()
