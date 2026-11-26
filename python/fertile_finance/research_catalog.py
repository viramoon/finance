"""Import reviewed XNYS daily OHLCV into a separate, immutable Nautilus catalog."""

import argparse
import csv
from dataclasses import dataclass, replace
from datetime import date
from decimal import Decimal, InvalidOperation
from enum import StrEnum
from functools import lru_cache
import hashlib
import io
import json
import os
from pathlib import Path
import re
import tempfile
from threading import Lock
from typing import TYPE_CHECKING

import exchange_calendars
import pandas as pd
from nautilus_trader.model.currencies import USD
from nautilus_trader.model.data import Bar, BarType
from nautilus_trader.model.identifiers import InstrumentId, Symbol
from nautilus_trader.model.instruments import Equity
from nautilus_trader.model.objects import FIXED_PRECISION, Price, Quantity
from nautilus_trader.persistence.catalog.parquet import ParquetDataCatalog

from .collector_store import DECIMAL
from .epoch import EPOCH, require_bar_timestamps, unix_nanos
from .research_factors import FactorDefinition

if TYPE_CHECKING:
    from .factor_evaluation import FactorEvaluation


ID = re.compile(r"[A-Z][A-Z0-9-]{0,15}\.XNYS")
SOURCE = re.compile(r"[A-Za-z0-9][A-Za-z0-9 ._()/-]{0,119}")
LATEST = date(2100, 12, 31)
CENT = Decimal("0.01")
FIELDS = ["date", "open", "high", "low", "close", "volume"]
MANIFEST = "dataset.json"
RAW_BASIS = "raw_unadjusted"
RAW_ACTIONS = "cash_dividends_excluded_split_free_reviewed"
YAHOO_SPLIT_BASIS = "yahoo_chart_split_normalized"
YAHOO_SPLIT_ACTIONS = "cash_dividends_excluded_yahoo_split_normalized_reviewed"
YAHOO_LINEAGE = "yahoo-lineage.json"


class ResearchError(ValueError):
    def __init__(self, code, message):
        self.code = code
        super().__init__(message)


class ResearchState(StrEnum):
    RESEARCH = "research"
    OUT_OF_SAMPLE_VALIDATION = "out_of_sample_validation"
    OBSERVE_ONLY = "observe_only"
    BACKTEST_SELECTABLE = "backtest_selectable"


_RESEARCH_ADVANCE = {
    ResearchState.RESEARCH: ResearchState.OUT_OF_SAMPLE_VALIDATION,
    ResearchState.OUT_OF_SAMPLE_VALIDATION: ResearchState.OBSERVE_ONLY,
    ResearchState.OBSERVE_ONLY: ResearchState.BACKTEST_SELECTABLE,
}
_RESEARCH_RETURN = {destination: origin for origin, destination in _RESEARCH_ADVANCE.items()}


@dataclass(frozen=True)
class ResearchTransition:
    previous: ResearchState
    current: ResearchState
    reason: str
    evidence_ref: str | None


@dataclass(frozen=True)
class RegisteredFactor:
    definition: FactorDefinition
    state: ResearchState = ResearchState.RESEARCH
    history: tuple[ResearchTransition, ...] = ()


class FactorResearchCatalog:
    """Versioned research approvals for an offline caller's catalog instance."""

    def __init__(self):
        self._entries: dict[tuple[str, str], RegisteredFactor] = {}
        self._lock = Lock()

    def register(self, definition: FactorDefinition) -> RegisteredFactor:
        key = (definition.factor_id, definition.version)
        with self._lock:
            existing = self._entries.get(key)
            if existing is not None:
                if existing.definition != definition:
                    raise ResearchError("factor_version_conflict", "Register changed factor metadata under a new version")
                return existing
            entry = RegisteredFactor(definition)
            self._entries[key] = entry
            return entry

    def get(self, factor_id: str, version: str) -> RegisteredFactor:
        with self._lock:
            return self._get(factor_id, version)

    def _get(self, factor_id: str, version: str) -> RegisteredFactor:
        try:
            return self._entries[(factor_id, version)]
        except KeyError:
            raise ResearchError("unknown_factor_version", "Register this factor version first") from None

    def transition(
        self,
        factor_id: str,
        version: str,
        state: ResearchState,
        *,
        reason: str,
        evidence_ref: str | None = None,
        report: "FactorEvaluation | None" = None,
    ) -> RegisteredFactor:
        if not isinstance(state, ResearchState) or not isinstance(reason, str) or not reason.strip():
            raise ResearchError("invalid_research_transition", "Specify a research state and review reason")
        if evidence_ref is not None and (not isinstance(evidence_ref, str) or not evidence_ref.strip()):
            raise ResearchError("invalid_research_evidence", "Use a nonempty reference to versioned review evidence")
        with self._lock:
            entry = self._get(factor_id, version)
            advancing = _RESEARCH_ADVANCE.get(entry.state) == state
            if not advancing and _RESEARCH_RETURN.get(entry.state) != state:
                raise ResearchError("invalid_research_transition", f"Cannot move from {entry.state} to {state}")
            if advancing and evidence_ref is None:
                raise ResearchError("invalid_research_evidence", "Record the frozen protocol or reviewed report reference")
            if advancing and state in {ResearchState.OBSERVE_ONLY, ResearchState.BACKTEST_SELECTABLE}:
                _require_out_of_sample_evidence(entry, state, report)
            transition = ResearchTransition(entry.state, state, reason, evidence_ref)
            updated = replace(entry, state=state, history=(*entry.history, transition))
            self._entries[(factor_id, version)] = updated
            return updated

    def select_for_backtest(self, factor_id: str, version: str) -> FactorDefinition:
        entry = self.get(factor_id, version)
        if entry.state != ResearchState.BACKTEST_SELECTABLE:
            raise ResearchError("factor_not_selectable", f"Factor version is in {entry.state}")
        return entry.definition


def _require_out_of_sample_evidence(
    entry: RegisteredFactor, state: ResearchState, report: "FactorEvaluation | None",
) -> None:
    from .factor_evaluation import FactorEvaluation

    if not isinstance(report, FactorEvaluation) or report.definition != entry.definition:
        raise ResearchError("invalid_research_evidence", "Supply an evaluation for this exact factor and data version")
    breadth = max(
        (period.ic_pairs[1] for window in report.windows if window.window.phase == "out_of_sample"
         for period in window.periods if period.ic[1] is not None),
        default=0,
    )
    if not breadth:
        raise ResearchError("invalid_research_evidence", "Supply a usable out-of-sample rank IC")
    if state == ResearchState.BACKTEST_SELECTABLE and breadth <= 9:
        raise ResearchError("insufficient_research_breadth", "Selection requires a reviewed cross section beyond nine assets")


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


def price(value, *, precision=2):
    granularity = "cent granularity" if precision == 2 else f"up to {precision} decimal places"
    message = f"Price must be positive decimal USD at {granularity}"
    if not DECIMAL.fullmatch(value) or len(value) > max(16, 8 + precision):
        raise ResearchError("invalid_bar", message)
    try:
        amount = Decimal(value)
        if 0 < amount <= 1_000_000 and amount == amount.quantize(Decimal((0, (1,), -precision))):
            return Price.from_str(format(amount, f".{precision}f"))
    except InvalidOperation:
        pass
    raise ResearchError("invalid_bar", message)


def parse_rows(raw, kind, *, fractional_before=None):
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
        precision = FIXED_PRECISION if fractional_before is not None and day < fractional_before else 2
        prices = []
        for key in FIELDS[1:5]:
            try:
                parsed = price(row[key], precision=precision)
            except ResearchError as exc:
                raise ResearchError(exc.code, f"{key} on {day}: {exc}") from exc
            if fractional_before is not None and precision != FIXED_PRECISION:
                # Keep a single storage precision for every bar in the adjusted dataset.
                parsed = Price.from_raw(parsed.raw, FIXED_PRECISION)
            prices.append(parsed)
        o, h, low, close = (p.as_decimal() for p in prices)
        if not low <= min(o, close) <= max(o, close) <= h:
            raise ResearchError("invalid_bar", f"Invalid OHLC range for {day}")
        result[day] = (*prices, Quantity.from_int(int(volume)))
    if not result:
        raise ResearchError("no_data", "CSV has no daily bars")
    return result


def _reviewed_split_cutoff(lineage, instrument, asset_type, csv_sha256):
    if (
        not isinstance(lineage, dict)
        or lineage.get("series_id") != instrument
        or lineage.get("asset_type") != asset_type
        or lineage.get("sha256") != csv_sha256
        or lineage.get("price_basis") != "yahoo_chart_quote"
        or lineage.get("output_price_basis") != YAHOO_SPLIT_BASIS
        or lineage.get("corporate_actions") != YAHOO_SPLIT_ACTIONS
    ):
        raise ResearchError("invalid_source", "Yahoo split lineage does not match the CSV, instrument, or price basis")
    evidence = lineage.get("split_review_evidence")
    snapshots = lineage.get("adjustment_as_of")
    events = lineage.get("split_events")
    if (not isinstance(evidence, str) or not evidence.strip()
            or not isinstance(snapshots, list) or len(snapshots) != 1
            or not isinstance(events, list) or not events):
        raise ResearchError("invalid_source", "Yahoo split lineage requires review evidence, one snapshot date and split events")
    snapshot = iso_date(snapshots[0])
    requested_start = iso_date(lineage.get("requested_start") or lineage.get("first_session"))
    latest = requested_start
    for event in events:
        if (not isinstance(event, dict)
                or any(type(event.get(leg)) is not int or event[leg] <= 0 for leg in ("numerator", "denominator"))
                or type(event.get("source_timestamp")) is not int or event["source_timestamp"] < 0
                or not (event.get("source_id") is None or isinstance(event["source_id"], str))):
            raise ResearchError("invalid_source", "Invalid event in Yahoo split lineage")
        day = iso_date(event.get("session"))
        if not latest <= day <= snapshot:
            raise ResearchError("invalid_source", f"Out-of-order or out-of-snapshot Yahoo split date {day}")
        latest = day
    return latest, snapshot


def import_catalog(root, csv_path, *, instrument, source, kind, asset_type, yahoo_split_lineage=None):
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
    csv_sha256 = hashlib.sha256(raw).hexdigest()
    lineage_raw = None
    fractional_before = None
    if yahoo_split_lineage is not None:
        lineage_raw = yahoo_split_lineage.read_bytes()
        lineage = json.loads(lineage_raw)
        fractional_before, snapshot = _reviewed_split_cutoff(lineage, instrument, asset_type, csv_sha256)
    rows = parse_rows(raw, kind, fractional_before=fractional_before)
    start, end = min(rows), max(rows)
    if lineage_raw is not None and (
        end > snapshot or (lineage.get("first_session"), lineage.get("last_session"), lineage.get("observations"))
        != (str(start), str(end), len(rows))
    ):
        raise ResearchError("invalid_source", "Yahoo split lineage session coverage does not match the CSV")
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
        "source_sha256": csv_sha256,
        "data_kind": kind, "calendar": "XNYS", "first_session": str(start), "last_session": str(end),
        "observations": len(bars), "price_basis": RAW_BASIS if lineage_raw is None else YAHOO_SPLIT_BASIS,
        "corporate_actions": RAW_ACTIONS if lineage_raw is None else YAHOO_SPLIT_ACTIONS,
    }
    require_bar_timestamps(bars)
    # Readers only discover the final directory. Never replace a published dataset.
    with tempfile.TemporaryDirectory(prefix=".import-", dir=root) as work:
        staging = Path(work) / "dataset"
        staging.mkdir()
        catalog = ParquetDataCatalog(staging)
        catalog.write_data([cash_equity(instrument)])
        catalog.write_data(bars)
        if lineage_raw is not None:
            (staging / YAHOO_LINEAGE).write_bytes(lineage_raw)
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
    if metadata["calendar"] != "XNYS" or (metadata["price_basis"], metadata["corporate_actions"]) not in {
        (RAW_BASIS, RAW_ACTIONS), (YAHOO_SPLIT_BASIS, YAHOO_SPLIT_ACTIONS),
    } or metadata["instrument"] != instrument or metadata["asset_type"] not in {"stock", "etf"} or (
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
    review = parser.add_mutually_exclusive_group(required=True)
    review.add_argument("--raw-split-free-reviewed", action="store_true",
                        help="Confirm raw (without dividend adjustments) OHLCV and no splits in the supplied span")
    review.add_argument("--yahoo-split-normalized-lineage", type=Path, metavar="JSON",
                        help="Reviewed converter lineage for Yahoo OHLCV on one split-normalized share basis")
    args = parser.parse_args()
    try:
        print(json.dumps(import_catalog(research_root(), args.csv, instrument=args.instrument,
                                       source=args.source, kind=args.kind, asset_type=args.asset_type,
                                       yahoo_split_lineage=args.yahoo_split_normalized_lineage)))
    except (OSError, ValueError) as exc:
        parser.exit(1, f"{exc}\n")


if __name__ == "__main__":
    main()
