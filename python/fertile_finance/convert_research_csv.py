"""Export Yahoo source quotes as CSV for the reviewed official research catalog."""

import argparse
import csv
from decimal import Context, Decimal, ROUND_HALF_EVEN, localcontext
import hashlib
import io
import json
import os
from pathlib import Path
from tempfile import NamedTemporaryFile
from time import time_ns

from nautilus_trader.model.objects import FIXED_PRECISION

from .collector_store import fsync_dir
from .historical_catalog import HistoricalCatalog
from .research_catalog import (
    CENT, FIELDS, ID, YAHOO_SPLIT_ACTIONS, YAHOO_SPLIT_BASIS, iso_date, parse_rows, sessions,
)
from .yahoo_daily import load_yahoo_daily


CENT_TOLERANCE = Decimal("0.00002")
CENT_CONTEXT = Context(prec=34, rounding=ROUND_HALF_EVEN)
NATIVE_QUANTUM = Decimal((0, (1,), -FIXED_PRECISION))


def _in_range(day, start, end):
    return (start is None or start <= day) and (end is None or day <= end)


def _source_prices(row, *, preserve_fractional):
    for field in FIELDS[1:5]:
        value = getattr(row, field)
        if value is None or not value.is_finite() or not 0 < value <= 1_000_000:
            raise ValueError(f"Invalid source quote {field} on {row.session}")
        with localcontext(CENT_CONTEXT):
            cents = value.quantize(CENT)
            if preserve_fractional:
                exact = value.quantize(NATIVE_QUANTUM)
                if exact != value:
                    raise ValueError(
                        f"Source quote {field} on {row.session} exceeds {FIXED_PRECISION} decimal places"
                    )
                rendered = format(exact.normalize(), "f")
            elif cents <= 0 or not cents - CENT_TOLERANCE <= value <= cents + CENT_TOLERANCE:
                raise ValueError(
                    f"Source quote {field} on {row.session} cannot be represented exactly in cents "
                    f"within USD {CENT_TOLERANCE} tolerance"
                )
            else:
                rendered = format(cents, "f")
        yield rendered, not preserve_fractional and cents != value, preserve_fractional and cents != value


def _render(rows, fractional_before):
    buffer = io.StringIO(newline="")
    writer = csv.writer(buffer, lineterminator="\n")
    writer.writerow(FIELDS)
    normalized_count = 0
    preserved_subcent_count = 0
    for row in rows:
        volume = row.volume
        if (volume is None or not volume.is_finite() or not 0 <= volume < 10**12
                or volume != volume.to_integral_value()):
            raise ValueError(f"Source quote volume on {row.session} must be a whole share count of at most 12 digits")
        preserve_fractional = fractional_before is not None and row.session < fractional_before
        prices, normalized, preserved = zip(*_source_prices(row, preserve_fractional=preserve_fractional))
        normalized_count += sum(normalized)
        preserved_subcent_count += sum(preserved)
        writer.writerow((row.session, *prices, int(volume)))
    raw = buffer.getvalue().encode("utf-8")
    parse_rows(raw, "historical", fractional_before=fractional_before)
    return raw, normalized_count, preserved_subcent_count


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
    evidence = args.yahoo_split_normalized_evidence
    if split_days and evidence is None:
        dates = ", ".join(map(str, split_days))
        raise ValueError(f"Yahoo splits on {dates}; select a split-free range or supply reviewed split-basis evidence")
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
    snapshots = sorted({row.adjustment_as_of for row in rows})
    split_events = []
    fractional_before = None
    if evidence is not None:
        if len(snapshots) != 1 or snapshots[0] < last:
            raise ValueError("Reviewed Yahoo split normalization requires one snapshot date on or after the selected quotes")
        snapshot = snapshots[0]
        if split_days and split_days[-1] > snapshot:
            raise ValueError(f"Yahoo split on {split_days[-1]} is later than the reviewed snapshot date {snapshot}")
        split_events = [
            {
                "session": str(event.session),
                "source_timestamp": event.source_timestamp,
                "source_id": event.source_id,
                "numerator": event.numerator,
                "denominator": event.denominator,
            }
            for event in sorted(source.splits, key=lambda event: (event.session, event.source_timestamp,
                                                                  event.source_id or ""))
            if (args.start or first) <= event.session <= snapshot
        ]
        if not split_events:
            raise ValueError("Reviewed Yahoo split normalization requires a recorded split through the snapshot date")
        fractional_before = iso_date(split_events[-1]["session"])

    raw, normalized_count, preserved_subcent_count = _render(rows, fractional_before)
    _publish(args.output, raw)
    return {
        "source": "Yahoo Finance chart",
        "source_kind": source_kind,
        "source_path": str(source_path),
        "series_id": source.series_id,
        "asset_type": source.asset_type,
        "price_basis": rows[0].price_basis,
        "output_price_basis": YAHOO_SPLIT_BASIS if evidence is not None else "cent_normalized_yahoo_chart_quote",
        "adjustment_as_of": list(map(str, snapshots)),
        "cent_tolerance_usd": str(CENT_TOLERANCE),
        "normalized_price_count": normalized_count,
        "preserved_subcent_price_count": preserved_subcent_count,
        "split_events": split_events,
        "split_review_evidence": evidence,
        "corporate_actions": YAHOO_SPLIT_ACTIONS if evidence is not None else None,
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
    parser.add_argument("--yahoo-split-normalized-evidence",
                        help="Public review reference for Yahoo quotes and volume on a single split-normalized share basis")
    parser.add_argument("--output", required=True, type=Path, metavar="CSV", help="New CSV path; JSON lineage goes to stdout")
    args = parser.parse_args()

    if args.yahoo_json is not None and (args.adjustment_as_of is None or args.series_id is not None):
        parser.error("--yahoo-json requires --adjustment-as-of and excludes --series-id")
    if args.historical_data_root is not None and (args.series_id is None or args.adjustment_as_of is not None):
        parser.error("--historical-data-root requires --series-id and excludes --adjustment-as-of")
    if (args.start is None) != (args.end is None) or (args.start is not None and args.end < args.start):
        parser.error("Supply both --start and --end as an ordered inclusive interval")
    if args.yahoo_split_normalized_evidence is not None and not args.yahoo_split_normalized_evidence.strip():
        parser.error("Provide the source and review reference for Yahoo split normalization")

    try:
        if args.yahoo_json is not None:
            source_path = args.yahoo_json.resolve(strict=True)
            source = load_yahoo_daily(source_path, adjustment_as_of=args.adjustment_as_of)
            lineage = _export(source, args, source_kind="yahoo_archive", source_path=source_path)
        else:
            catalog = HistoricalCatalog(args.historical_data_root)
            # A later split can explain the units of earlier quotes in this snapshot.
            source_end = args.end if args.yahoo_split_normalized_evidence is None else None
            source = catalog.read_yahoo(args.series_id, start=args.start, end=source_end)
            source_path = catalog.root / "historical" / "yahoo" / source.series_id
            lineage = _export(source, args, source_kind="historical_catalog", source_path=source_path)
        print(json.dumps(lineage, sort_keys=True))
    except (OSError, ValueError) as exc:
        parser.exit(1, f"{exc}\n")


if __name__ == "__main__":
    main()
