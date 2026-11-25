"""Load locally archived Ken French factors and FRED daily observations."""

import csv
from dataclasses import dataclass
from datetime import date
from decimal import Decimal, InvalidOperation
import io
import json
from pathlib import Path
import re
from zipfile import ZipFile, is_zipfile

from .historical_types import EpochDiscard, EpochPolicy, guard_epoch


FRED_SERIES_ID = re.compile(r"[A-Z0-9][A-Z0-9_]*")
_FRENCH_COLUMNS = ("Mkt-RF", "SMB", "HML", "RF")
_FRENCH_MOMENTUM = ("Mom",)
_FRENCH_MISSING = (Decimal("-99.99"), Decimal("-999"))


@dataclass(frozen=True)
class FrenchFactor:
    session: date
    mkt_rf: Decimal | None
    smb: Decimal | None
    hml: Decimal | None
    rf: Decimal | None
    mom: Decimal | None = None
    available_on: date | None = None


@dataclass(frozen=True)
class FrenchDaily:
    rows: tuple[FrenchFactor, ...]
    dropped_pre_epoch: EpochDiscard


@dataclass(frozen=True)
class FrenchRF:
    session: date
    rf: Decimal | None
    available_on: date | None


@dataclass(frozen=True)
class FrenchRiskFree:
    rows: tuple[FrenchRF, ...]
    dropped_pre_epoch: EpochDiscard


@dataclass(frozen=True)
class FredObservation:
    observation_date: date
    value: Decimal | None
    realtime_start: date | None = None
    realtime_end: date | None = None


@dataclass(frozen=True)
class FredDaily:
    series_id: str
    rows: tuple[FredObservation, ...]
    dropped_pre_epoch: EpochDiscard


def _french_text(path: str | Path) -> str:
    path = Path(path)
    if not is_zipfile(path):
        return path.read_text(encoding="utf-8-sig")
    with ZipFile(path) as archive:
        tables = [item for item in archive.infolist()
                  if not item.is_dir() and Path(item.filename).suffix.lower() in {".csv", ".txt"}]
        if len(tables) != 1:
            raise ValueError("Ken French ZIP must contain exactly one CSV or TXT table")
        return archive.read(tables[0]).decode("utf-8-sig")


def _decimal(token: str | Decimal, source: str) -> Decimal:
    if not isinstance(token, (str, Decimal)):
        raise ValueError(f"Invalid {source} decimal: {token!r}")
    try:
        value = Decimal(token)
    except InvalidOperation:
        raise ValueError(f"Invalid {source} decimal: {token!r}") from None
    if not value.is_finite():
        raise ValueError(f"Invalid {source} decimal: {token!r}")
    return value


def _french_return(token: str) -> Decimal | None:
    if not token:
        return None
    percent = _decimal(token, "Ken French")
    if percent in _FRENCH_MISSING:
        return None
    sign, digits, exponent = percent.as_tuple()
    return Decimal((sign, digits, exponent - 2))


def _daily_french_table(path: str | Path, accepted_headers: tuple[tuple[str, ...], ...]):
    lines = iter(_french_text(path).splitlines())
    for line in lines:
        csv_format = "," in line
        fields = [field.strip() for field in next(csv.reader([line]))] if csv_format else line.split()
        if fields and fields[0].lower() in {"", "date"}:
            fields = fields[1:]
        columns = tuple(fields)
        if columns in accepted_headers:
            break
    else:
        raise ValueError("Ken French archive lacks the expected daily factor header")

    table = {}
    for line in lines:
        if not line.strip():
            continue
        if line.strip().lower().startswith("copyright"):
            break
        fields = [field.strip() for field in next(csv.reader([line]))] if csv_format else line.split()
        if len(fields) != len(columns) + 1 or not re.fullmatch(r"[0-9]{8}", fields[0]):
            raise ValueError("Expected Ken French daily data with YYYYMMDD dates")
        session = date.fromisoformat(fields[0])
        if session in table:
            raise ValueError(f"Duplicate Ken French session: {session}")
        table[session] = tuple(_french_return(field) for field in fields[1:])
    if not table:
        raise ValueError("Ken French daily table has no observations")
    return columns, table


def load_ken_french(
    factors_path: str | Path,
    *,
    momentum_path: str | Path | None = None,
    pre_epoch: EpochPolicy = "reject",
) -> FrenchDaily:
    """Load daily factor fractions by session, with unknown publication dates."""
    combined = _FRENCH_COLUMNS + _FRENCH_MOMENTUM
    columns, factors = _daily_french_table(factors_path, (_FRENCH_COLUMNS, combined))
    if columns == combined and momentum_path is not None:
        raise ValueError("Momentum was supplied in two Ken French tables")
    momentum = {}
    if momentum_path is not None:
        _, momentum = _daily_french_table(momentum_path, (_FRENCH_MOMENTUM,))
    rows = []
    for session in sorted(factors.keys() | momentum.keys()):
        mkt_rf, smb, hml, rf, *embedded_mom = factors.get(session, (None, None, None, None))
        (mom,) = momentum.get(session, embedded_mom or (None,))
        rows.append(FrenchFactor(session, mkt_rf, smb, hml, rf, mom))
    kept, dropped = guard_epoch(rows, date_of=lambda row: row.session, policy=pre_epoch)
    return FrenchDaily(kept, dropped)


def load_ken_french_rf(factors_path: str | Path, *, pre_epoch: EpochPolicy = "reject") -> FrenchRiskFree:
    """Load the daily fractional risk-free return and its availability field."""
    result = load_ken_french(factors_path, pre_epoch=pre_epoch)
    return FrenchRiskFree(tuple(FrenchRF(row.session, row.rf, row.available_on) for row in result.rows),
                          result.dropped_pre_epoch)


def _iso_date(token: str) -> date:
    if not isinstance(token, str) or not re.fullmatch(r"[0-9]{4}-[0-9]{2}-[0-9]{2}", token):
        raise ValueError(f"Expected a FRED date in YYYY-MM-DD format: {token!r}")
    return date.fromisoformat(token)


def _fred_value(token: str | Decimal) -> Decimal | None:
    return None if token in ("", ".") else _decimal(token, "FRED")


def _fred_csv(content: str, series_id: str) -> list[FredObservation]:
    reader = csv.reader(io.StringIO(content, newline=""), strict=True)
    header = next(reader, [])
    if len(header) != 2 or header[0] not in {"observation_date", "DATE"} or header[1] != series_id:
        raise ValueError(f"Expected FRED graph CSV for {series_id}")
    rows = []
    for fields in reader:
        if not fields:
            continue
        if len(fields) != 2:
            raise ValueError("Expected a date and value in each FRED CSV row")
        day, value = fields
        rows.append(FredObservation(_iso_date(day), _fred_value(value.strip())))
    return rows


def _fred_json(content: str) -> list[FredObservation]:
    payload = json.loads(content, parse_float=Decimal, parse_int=Decimal, parse_constant=Decimal)
    if not isinstance(payload, dict) or not isinstance(payload.get("observations"), list):
        raise ValueError("Expected a FRED series observations JSON response")
    if payload.get("units", "lin") != "lin" or payload.get("output_type", 1) != 1:
        raise ValueError("Expected FRED observations by realtime period in native units")
    rows = []
    for item in payload["observations"]:
        if not isinstance(item, dict) or not {"date", "value", "realtime_start", "realtime_end"} <= item.keys():
            raise ValueError("Expected a date, value and realtime period in each FRED JSON observation")
        day = _iso_date(item["date"])
        start = _iso_date(item["realtime_start"])
        end = _iso_date(item["realtime_end"])
        if start > end:
            raise ValueError(f"Invalid FRED realtime period for {day}")
        rows.append(FredObservation(day, _fred_value(item["value"]), start, end))
    return rows


def load_fred(path: str | Path, *, series_id: str, pre_epoch: EpochPolicy = "reject") -> FredDaily:
    """Load a daily FRED archive in its source units, with one vintage per date."""
    if not isinstance(series_id, str) or not FRED_SERIES_ID.fullmatch(series_id):
        raise ValueError("Use a FRED series ID containing uppercase letters, digits or underscores")
    content = Path(path).read_text(encoding="utf-8-sig")
    try:
        rows = _fred_json(content) if content.lstrip().startswith("{") else _fred_csv(content, series_id)
    except csv.Error as exc:
        raise ValueError("Malformed FRED graph CSV") from exc
    if not rows:
        raise ValueError(f"FRED series {series_id} has no observations")
    days = {row.observation_date for row in rows}
    if len(days) != len(rows):
        raise ValueError("FRED archive must contain at most one vintage per observation date")
    rows.sort(key=lambda row: row.observation_date)
    kept, dropped = guard_epoch(rows, date_of=lambda row: row.observation_date, policy=pre_epoch)
    return FredDaily(series_id, kept, dropped)
