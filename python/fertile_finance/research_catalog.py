"""Import reviewed XNYS daily OHLCV into a separate, immutable Nautilus catalog."""

import argparse
import csv
from datetime import date
from decimal import Decimal, InvalidOperation
from functools import lru_cache
import hashlib
import io
import json
import os
from pathlib import Path
import re
import tempfile

import exchange_calendars
import pandas as pd
from nautilus_trader.model.currencies import USD
from nautilus_trader.model.data import Bar, BarType
from nautilus_trader.model.identifiers import InstrumentId, Symbol
from nautilus_trader.model.instruments import Equity
from nautilus_trader.model.objects import Price, Quantity
from nautilus_trader.persistence.catalog.parquet import ParquetDataCatalog

from .collector_store import DECIMAL, unix_nanos


ID = re.compile(r"[A-Z][A-Z0-9-]{0,15}\.XNYS")
SOURCE = re.compile(r"[A-Za-z0-9][A-Za-z0-9 ._()/-]{0,119}")
EPOCH = date(1970, 1, 1)
LATEST = date(2100, 12, 31)
CENT = Decimal("0.01")
FIELDS = ["date", "open", "high", "low", "close", "volume"]
MANIFEST = "dataset.json"


class ResearchError(ValueError):
    def __init__(self, code, message):
        self.code = code
        super().__init__(message)


def research_root():
    root = Path(os.environ.get("FERTILE_RESEARCH_ROOT", "/home/box/fertile/data/research"))
    if not root.is_absolute():
        raise ValueError("FERTILE_RESEARCH_ROOT must be absolute")
    return root


def iso_date(value):
    try:
        day = date.fromisoformat(value)
        if str(day) == value and EPOCH <= day <= LATEST:
            return day
    except (TypeError, ValueError):
        pass
    raise ResearchError("invalid_date", "Use a calendar date from 1970-01-01 through 2100-12-31")


@lru_cache(maxsize=1)
def nyse():
    return exchange_calendars.get_calendar("XNYS", start=str(EPOCH), end=str(LATEST))


def sessions(start, end):
    return {label.date(): unix_nanos(close.value) for label, close
            in nyse().schedule.loc[str(start):str(end), "close"].items()}


def cash_equity(instrument):
    return Equity(
        InstrumentId.from_str(instrument), Symbol(instrument.split(".")[0]), USD,
        price_precision=2, price_increment=Price.from_str("0.01"),
        lot_size=Quantity.from_int(1), ts_event=0, ts_init=0,
    )


def bar_type(instrument):
    return BarType.from_str(f"{instrument}-1-DAY-LAST-EXTERNAL")


def price(value):
    if not DECIMAL.fullmatch(value) or len(value) > 16:
        raise ResearchError("invalid_bar", "Price must be positive decimal USD at cent granularity")
    try:
        amount = Decimal(value)
        if 0 < amount <= 1_000_000 and amount == amount.quantize(CENT):
            return Price.from_str(format(amount, ".2f"))
    except InvalidOperation:
        pass
    raise ResearchError("invalid_bar", "Price must be positive decimal USD at cent granularity")


def parse_rows(raw, kind):
    rows = csv.DictReader(io.StringIO(raw.decode("utf-8"), newline=""))
    if rows.fieldnames != FIELDS:
        raise ResearchError("invalid_bar", "Expected CSV columns " + ",".join(FIELDS))
    result = {}
    today = pd.Timestamp.now(tz="UTC").date()
    for row in rows:
        if row.keys() != set(FIELDS) or None in row.values():
            raise ResearchError("invalid_bar", "Missing or extra CSV column")
        day = iso_date(row["date"])
        if day in result:
            raise ResearchError("invalid_bar", f"Duplicate session {day}")
        if kind == "historical" and day > today:
            raise ResearchError("invalid_date", f"Historical session is in the future: {day}")
        volume = row["volume"]
        if not re.fullmatch(r"[0-9]{1,12}", volume):
            raise ResearchError("invalid_bar", "Volume must be a nonnegative whole share count")
        prices = tuple(price(row[key]) for key in FIELDS[1:5])
        o, h, low, close = (p.as_decimal() for p in prices)
        if not low <= min(o, close) <= max(o, close) <= h:
            raise ResearchError("invalid_bar", f"Invalid OHLC range for {day}")
        result[day] = (*prices, Quantity.from_int(int(volume)))
    if not result:
        raise ResearchError("no_data", "CSV has no daily bars")
    return result


def import_catalog(root, csv_path, *, instrument, source, kind, asset_type):
    if not ID.fullmatch(instrument):
        raise ResearchError("invalid_instrument", "Choose an XNYS-listed USD stock or ETF, such as IBM.XNYS")
    if not SOURCE.fullmatch(source) or kind not in {"historical", "fixture"} or asset_type not in {"stock", "etf"}:
        raise ResearchError("invalid_source", "Specify a provider name, data kind and tradable asset type")
    root.mkdir(parents=True, exist_ok=True)
    destination = root / instrument
    if destination.exists() or destination.is_symlink():
        raise FileExistsError(f"Dataset already exists: {instrument}")
    with csv_path.open("rb") as source_file:
        limit = 8 * 1024 * 1024
        raw = source_file.read(limit + 1)
    if len(raw) > limit:
        raise ResearchError("invalid_source", "CSV must fit within 8 MiB")
    rows = parse_rows(raw, kind)
    start, end = min(rows), max(rows)
    calendar = sessions(start, end)
    now = pd.Timestamp.now(tz="UTC").value
    kind_of_bar = bar_type(instrument)
    bars = []
    for day, values in sorted(rows.items()):
        when = calendar.get(day)
        if when is None:
            raise ResearchError("invalid_session", f"{day} is not an XNYS session")
        if kind == "historical" and when > now:
            raise ResearchError("invalid_session", f"Historical session has not closed: {day}")
        bars.append(Bar(kind_of_bar, *values, when, when))

    metadata = {
        "instrument": instrument, "asset_type": asset_type, "source": source,
        "source_sha256": hashlib.sha256(raw).hexdigest(),
        "data_kind": kind, "calendar": "XNYS", "first_session": str(start), "last_session": str(end),
        "observations": len(bars), "price_basis": "raw_unadjusted",
        "corporate_actions": "cash_dividends_excluded_split_free_reviewed",
    }
    # Readers only discover the final directory. Never replace a published dataset.
    with tempfile.TemporaryDirectory(prefix=".import-", dir=root) as work:
        staging = Path(work) / "dataset"
        staging.mkdir()
        catalog = ParquetDataCatalog(staging)
        catalog.write_data([cash_equity(instrument)])
        catalog.write_data(bars)
        (staging / MANIFEST).write_text(json.dumps(metadata, ensure_ascii=False) + "\n")
        staging.rename(destination)
    return metadata


def load_interval(root, instrument, start, end):
    if not ID.fullmatch(instrument):
        raise ResearchError("invalid_instrument", "Only configured XNYS stocks and ETFs are supported")
    path = root / instrument
    if path.is_symlink():
        raise ResearchError("invalid_source", "Catalog must be a concrete dataset directory")
    try:
        metadata = json.loads((path / MANIFEST).read_text())
    except FileNotFoundError:
        raise ResearchError("no_data", "No research catalog for that instrument") from None
    if (metadata["calendar"], metadata["price_basis"], metadata["corporate_actions"]) != (
        "XNYS", "raw_unadjusted", "cash_dividends_excluded_split_free_reviewed",
    ) or metadata["instrument"] != instrument or metadata["asset_type"] not in {"stock", "etf"} or (
        metadata["data_kind"] not in {"historical", "fixture"}
    ):
        raise ResearchError("invalid_source", "Catalog provenance or instrument does not match the research profile")
    catalog = ParquetDataCatalog(path)
    expected = sessions(start, end)
    if not expected:
        raise ResearchError("no_data", "The requested range contains no XNYS session")
    dates = {when: day for day, when in expected.items()}
    bars = catalog.bars(bar_types=[str(bar_type(instrument))],
                        start=min(dates), end=max(dates))
    if not bars:
        raise ResearchError("no_data", "No catalog bars in the requested range")
    observed = set()
    for bar in bars:
        when = unix_nanos(bar.ts_event)
        if when not in dates or when != bar.ts_init or when in observed:
            raise ResearchError("invalid_bar", "Expected one unique catalog bar at the official session close")
        observed.add(when)
    if missing := sorted(dates[when] for when in dates.keys() - observed):
        raise ResearchError("missing_sessions", f"{len(missing)} missing XNYS sessions from {missing[0]} to {missing[-1]}")
    (equity,) = catalog.instruments(instrument_ids=[instrument])
    if not isinstance(equity, Equity) or equity.quote_currency != USD or equity.price_increment.as_decimal() != CENT:
        raise ResearchError("invalid_instrument", "Catalog instrument must be a cent-priced cash equity in USD")
    return metadata, equity, sorted(bars, key=lambda bar: bar.ts_event), dates


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("csv", type=Path)
    parser.add_argument("--instrument", required=True)
    parser.add_argument("--source", required=True, help="Public provider label, with no URL credentials or secrets")
    parser.add_argument("--kind", choices=("historical", "fixture"), required=True)
    parser.add_argument("--asset-type", choices=("stock", "etf"), required=True)
    parser.add_argument("--raw-split-free-reviewed", required=True, action="store_true",
                        help="Confirm raw (without dividend adjustments) OHLCV and no splits in the supplied span")
    args = parser.parse_args()
    try:
        print(json.dumps(import_catalog(research_root(), args.csv, instrument=args.instrument,
                                       source=args.source, kind=args.kind, asset_type=args.asset_type)))
    except (OSError, ValueError) as exc:
        parser.exit(1, f"{exc}\n")


if __name__ == "__main__":
    main()
