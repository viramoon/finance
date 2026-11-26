"""Offline inputs and provenance for the factor momentum study."""

from calendar import monthrange
from collections import defaultdict
import csv
from datetime import date, datetime, time
from decimal import Decimal, localcontext
from hashlib import sha256
from io import BytesIO, StringIO
import json
from math import prod
from pathlib import Path
import re
from zipfile import ZipFile

from fertile_finance.research_catalog import sessions
from fertile_finance.research_decimal import RESEARCH_CONTEXT


type Rows = dict[date, dict[str, Decimal | None]]

_FF5 = ("Mkt-RF", "SMB", "HML", "RMW", "CMA", "RF")
_FF3 = ("Mkt-RF", "SMB", "HML", "RF")
_ARCHIVES = (
    ("F-F_Research_Data_5_Factors_2x3", _FF5, _FF5[:-1]),
    ("F-F_Research_Data_Factors", _FF3, ("RF",)),
    ("F-F_Momentum_Factor", ("Mom",), ("Mom",)),
    ("F-F_ST_Reversal_Factor", ("ST_Rev",), ("ST_Rev",)),
    ("F-F_LT_Reversal_Factor", ("LT_Rev",), ("LT_Rev",)),
)
_COMPARISONS = {
    "TSMOM12": "2026-11-23-tsmom-mkt",
    "vol_managed": "2026-11-24-vol-managed-mkt",
    "faber_sma10": "2026-11-25-faber-sma10-mkt",
}
_MISSING = (Decimal("-99.99"), Decimal("-999"))


def _month_end(day: date) -> date:
    return day.replace(day=monthrange(day.year, day.month)[1])


def _fraction(percent: str) -> Decimal | None:
    if not percent:
        return None
    value = Decimal(percent)
    if not value.is_finite():
        raise ValueError(f"Non-finite Ken French return {percent!r}")
    if value in _MISSING:
        return None
    sign, digits, exponent = value.as_tuple()
    return Decimal((sign, digits, exponent - 2))


def _french_table(content: bytes, columns: tuple[str, ...], frequency: str) -> tuple[Rows, str, str]:
    with ZipFile(BytesIO(content)) as archive:
        tables = [item for item in archive.infolist()
                  if not item.is_dir() and Path(item.filename).suffix.lower() in {".csv", ".txt"}]
        if len(tables) != 1:
            raise ValueError("Expected exactly one Ken French table per ZIP")
        member = tables[0].filename
        source = archive.read(tables[0]).decode("utf-8-sig")

    vintage = re.search(r"\b(\d{6}) CRSP database\b", source)
    if vintage is None:
        raise ValueError(f"Missing CRSP vintage in {member}")
    reader = csv.reader(StringIO(source, newline=""))
    for row in reader:
        if tuple(field.strip() for field in row) == ("", *columns):
            break
    else:
        raise ValueError(f"Missing expected columns {columns!r} in {member}")

    rows = {}
    previous = date.min
    date_pattern = r"[0-9]{6}" if frequency == "monthly" else r"[0-9]{8}"
    for row in reader:
        fields = [field.strip() for field in row]
        if not fields or fields == [""]:
            continue
        label = fields[0]
        if label.lower().startswith("copyright") or (
            frequency == "monthly" and label.lower().startswith("annual factors")
        ):
            break
        if len(fields) != len(columns) + 1 or not re.fullmatch(date_pattern, label):
            raise ValueError(f"Invalid {frequency} row in {member}: {row!r}")
        day = _month_end(date.fromisoformat(label + "01")) if frequency == "monthly" else date.fromisoformat(label)
        if day <= previous:
            raise ValueError(f"Ken French dates must increase strictly in {member}: {day}")
        rows[day] = dict(zip(columns, map(_fraction, fields[1:]), strict=True))
        previous = day
    if not rows:
        raise ValueError(f"Empty Ken French table in {member}")
    return rows, vintage[1], member


def _coverage(days: list[date]) -> dict:
    return {
        "first": min(days).isoformat() if days else None,
        "last": max(days).isoformat() if days else None,
        "observations": len(days),
    }


def _french_coverage(rows: Rows, columns: tuple[str, ...]) -> dict:
    return {
        **_coverage(list(rows)),
        "series": {
            column: {
                **_coverage([day for day, row in rows.items() if row[column] is not None]),
                "missing": sum(row[column] is None for row in rows.values()),
            }
            for column in columns
        },
    }


def load_french_snapshot(raw_dir: Path) -> tuple[Rows, Rows, dict]:
    """Verify the manifest's exact bytes, then read separate monthly and daily tables."""
    manifest_path = raw_dir / "MANIFEST.txt"
    manifest_bytes = manifest_path.read_bytes()
    manifest = manifest_bytes.decode("utf-8")
    timestamp = re.search(r"^downloaded_at_utc:\s+(\S+)", manifest, re.MULTILINE)
    if timestamp is None:
        raise ValueError("Missing Ken French download timestamp")
    entries = {}
    for digest, size, name, url in re.findall(
        r"^([0-9a-f]{64})\s+(\d+)\s+(\S+\.zip)\s+(https://\S+)$", manifest, re.MULTILINE
    ):
        if name in entries:
            raise ValueError(f"Duplicate Ken French manifest entry {name}")
        entries[name] = {"sha256": digest, "bytes": int(size), "url": url}

    datasets: dict[str, Rows] = {"monthly": {}, "daily": {}}
    files = {}
    source_columns: dict[str, dict[str, str]] = {"monthly": {}, "daily": {}}
    for frequency, suffix in (("monthly", "_CSV.zip"), ("daily", "_daily_CSV.zip")):
        selected = datasets[frequency]
        for stem, columns, keep in _ARCHIVES:
            name = stem + suffix
            if name not in entries:
                raise ValueError(f"Missing Ken French manifest entry {name}")
            expected = entries[name]
            payload = (raw_dir / name).read_bytes()
            if len(payload) != expected["bytes"] or sha256(payload).hexdigest() != expected["sha256"]:
                raise ValueError(f"Ken French archive differs from manifest: {name}")
            rows, vintage, member = _french_table(payload, columns, frequency)
            files[name] = {
                **expected, "frequency": frequency, "member": member, "crsp_vintage": vintage,
                **_french_coverage(rows, columns),
            }
            source_columns[frequency].update(dict.fromkeys(keep, name))
            for day, row in rows.items():
                selected.setdefault(day, {}).update({column: row[column] for column in keep})
    if set(files) != set(entries):
        raise ValueError(f"Unexpected Ken French archives in manifest: {sorted(set(entries) - set(files))}")
    vintages = {file["crsp_vintage"] for file in files.values()}
    if len(vintages) != 1:
        raise ValueError(f"Mixed CRSP vintages: {sorted(vintages)}")
    # Materialize unavailable columns without filling them from other dates or series.
    for frequency, selected in datasets.items():
        datasets[frequency] = {
            day: {column: row.get(column) for column in source_columns[frequency]}
            for day, row in sorted(selected.items())
        }
    provenance = {
        "downloaded_at_utc": timestamp[1],
        "manifest_sha256": sha256(manifest_bytes).hexdigest(),
        "crsp_vintage": vintages.pop(),
        "files": files,
        "source_by_column": source_columns,
        "observation_labels": {"monthly": "calendar month end", "daily": "source session date"},
        "available_on": None,
        "missing_markers": ["", "-99.99", "-999"],
    }
    return datasets["monthly"], datasets["daily"], provenance


def _comparison_months(content: bytes) -> tuple[dict[date, Decimal], dict]:
    reader = csv.DictReader(StringIO(content.decode("utf-8-sig"), newline=""))
    if reader.fieldnames is None or not {"", "strategy_ret"} <= set(reader.fieldnames):
        raise ValueError("Expected comparison timestamps and strategy_ret")
    daily = {}
    previous = date.min
    for row in reader:
        when = datetime.fromisoformat(row[""])
        day = when.date()
        if when.utcoffset() is None or when.utcoffset().total_seconds() != 0 or when.time() != time(21):
            raise ValueError(f"Expected 21:00 UTC comparison bar: {when}")
        if day <= previous:
            raise ValueError(f"Comparison dates must increase strictly: {day}")
        previous = day
        cell = row["strategy_ret"].strip()
        value = Decimal(cell) if cell else None
        if value is not None and (not value.is_finite() or value < -1):
            raise ValueError(f"Invalid comparison strategy_ret on {day}: {cell!r}")
        daily[day] = value
    if not daily:
        raise ValueError("No comparison rows")

    calendar_days = sessions(min(daily).replace(day=1), _month_end(max(daily)))
    if set(daily) - calendar_days.keys():
        raise ValueError("Comparison input has non-XNYS dates")
    missing_labels = sorted(calendar_days.keys() - daily.keys())
    missing_returns = [day for day, value in daily.items() if value is None]
    rejected_months = {_month_end(day) for day in (*missing_labels, *missing_returns)}
    monthly_groups = defaultdict(list)
    for day, value in daily.items():
        month = _month_end(day)
        if month not in rejected_months:
            monthly_groups[month].append(value)
    with localcontext(RESEARCH_CONTEXT):
        monthly = {month: prod(1 + value for value in values) - 1 for month, values in monthly_groups.items()}

    return monthly, {
        "source_bar_time": "21:00 UTC",
        "daily_rows": _coverage(list(daily)),
        "daily_returns": _coverage([day for day, value in daily.items() if value is not None]),
        "missing_session_labels": [day.isoformat() for day in missing_labels],
        "missing_strategy_returns": [day.isoformat() for day in missing_returns],
        "excluded_months": [month.isoformat() for month in sorted(rejected_months)],
        "monthly_returns": _coverage(list(monthly)),
    }


def load_comparisons(snapshot_dir: Path, *, source_dir: Path | None = None) -> tuple[Rows, dict]:
    """Aggregate read-only research sources or read the verified monthly derivative."""
    monthly_path = snapshot_dir / "monthly_returns.csv"
    metadata_path = snapshot_dir / "provenance.json"
    if source_dir is not None:
        sources = {}
        by_month = defaultdict(dict)
        for name, slug in _COMPARISONS.items():
            relative_path = Path(slug) / "results" / "performance_returns.csv"
            payload = (source_dir / relative_path).read_bytes()
            monthly, coverage = _comparison_months(payload)
            sources[name] = {
                "source_file": relative_path.as_posix(),
                "sha256": sha256(payload).hexdigest(),
                "bytes": len(payload),
                **coverage,
            }
            for month, value in monthly.items():
                by_month[month][name] = value
        snapshot_dir.mkdir(parents=True, exist_ok=True)
        with monthly_path.open("w", encoding="utf-8", newline="") as handle:
            writer = csv.writer(handle, lineterminator="\n")
            writer.writerow(("month", *_COMPARISONS))
            for month, values in sorted(by_month.items()):
                writer.writerow((month.isoformat(), *(values.get(name, "") for name in _COMPARISONS)))
        metadata = {
            "aggregation": "product(1 + daily strategy_ret) - 1, complete XNYS calendar months",
            "decimal_precision": RESEARCH_CONTEXT.prec,
            "monthly_snapshot_sha256": sha256(monthly_path.read_bytes()).hexdigest(),
            "sources": sources,
        }
        metadata_path.write_text(json.dumps(metadata, indent=2, sort_keys=True) + "\n", encoding="utf-8")

    result = {}
    metadata = json.loads(metadata_path.read_text(encoding="utf-8"))
    encoded = monthly_path.read_bytes()
    if sha256(encoded).hexdigest() != metadata["monthly_snapshot_sha256"]:
        raise ValueError("Comparison monthly snapshot differs from its provenance")
    reader = csv.DictReader(StringIO(encoded.decode("utf-8"), newline=""))
    if reader.fieldnames != ["month", *_COMPARISONS] or set(metadata["sources"]) != set(_COMPARISONS):
        raise ValueError("Unexpected columns in the comparison monthly snapshot")
    previous = date.min
    for row in reader:
        month = date.fromisoformat(row["month"])
        if month != _month_end(month) or month <= previous:
            raise ValueError(f"Unexpected date in the comparison monthly snapshot: {month}")
        result[month] = {name: Decimal(row[name]) if row[name] else None for name in _COMPARISONS}
        previous = month
    for name in _COMPARISONS:
        coverage = _coverage([month for month, row in result.items() if row[name] is not None])
        if coverage != metadata["sources"][name]["monthly_returns"]:
            raise ValueError(f"Comparison monthly coverage differs for {name}")
    return result, metadata
