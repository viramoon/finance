"""Read USD research day bars and corporate actions from saved Yahoo chart JSON."""

from dataclasses import dataclass
from datetime import date, datetime, time, timedelta, timezone
from decimal import Decimal, localcontext
import json
from pathlib import Path
import re
from typing import Literal
from urllib.parse import quote, urlencode
from zoneinfo import ZoneInfo

from .historical_types import ADJUSTMENT_CONTEXT, DailyBar, Dividend, EpochDiscard, Split, guard_epoch


_EXCHANGE_TZ = ZoneInfo("America/New_York")
_UNIX_EPOCH = datetime(1970, 1, 1, tzinfo=timezone.utc)
_SYMBOL = re.compile(r"\^?[A-Z0-9][A-Z0-9-]{0,31}")
_VENUES = {
    "NYQ": "XNYS", "NYSE": "XNYS", "PCX": "ARCA",
    "NMS": "XNAS", "NGM": "XNAS", "NCM": "XNAS",
    "ASE": "XASE", "BATS": "BATS",
}
_ASSETS = {"EQUITY": "stock", "ETF": "etf", "INDEX": "index"}
_QUOTE_FIELDS = ("open", "high", "low", "close", "volume")


@dataclass(frozen=True)
class YahooDaily:
    symbol: str
    asset_type: Literal["stock", "etf", "index"]
    currency: str
    listing_venue: str | None
    adjustment_as_of: date
    bars: tuple[DailyBar, ...]
    dividends: tuple[Dividend, ...]
    splits: tuple[Split, ...]
    dropped_pre_epoch: EpochDiscard

    @property
    def series_id(self) -> str:
        if self.asset_type == "index":
            return f"{self.symbol.removeprefix('^')}.INDEX"
        return f"{self.symbol}.{self.listing_venue}"


def _midnight_seconds(day: date) -> int:
    midnight = datetime.combine(day, time.min, _EXCHANGE_TZ)
    delta = midnight - _UNIX_EPOCH
    return delta.days * 86400 + delta.seconds


def yahoo_chart_url(symbol: str, start: date, end: date) -> str:
    """Build a daily chart URL for an inclusive range of New York dates."""
    if not isinstance(symbol, str) or not _SYMBOL.fullmatch(symbol):
        raise ValueError("Invalid Yahoo chart symbol")
    if type(start) is not date or type(end) is not date or not start <= end < date.max:
        raise ValueError("Expected an ordered inclusive Yahoo date range")
    query = urlencode({
        "period1": _midnight_seconds(start),
        "period2": _midnight_seconds(end + timedelta(days=1)),
        "interval": "1d",
        "events": "div,split",
        "includeAdjustedClose": "true",
    })
    return f"https://query1.finance.yahoo.com/v8/finance/chart/{quote(symbol, safe='')}?{query}"


def _object(value, field):
    if not isinstance(value, dict):
        raise ValueError(f"Expected a Yahoo object for {field}")
    return value


def _single(value, field):
    if not isinstance(value, list) or len(value) != 1:
        raise ValueError(f"Expected one Yahoo object for {field}")
    return _object(value[0], field)


def _invalid_constant(value):
    raise ValueError(f"Invalid numeric constant in Yahoo chart JSON: {value}")


def _unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"Duplicate key in Yahoo chart JSON: {key}")
        result[key] = value
    return result


def _session(value) -> date:
    if type(value) is int:
        try:
            return datetime.fromtimestamp(value, tz=_EXCHANGE_TZ).date()
        except (OSError, OverflowError, ValueError):
            pass
    raise ValueError("Expected an integer Yahoo Unix timestamp in seconds")


def _number(value, field) -> Decimal:
    if type(value) in (int, Decimal):
        number = Decimal(value)
        if number.is_finite():
            return number
    raise ValueError(f"Expected a finite Yahoo number for {field}")


def _split_leg(value, field) -> int:
    number = _number(value, field)
    if number <= 0 or number != number.to_integral_value():
        raise ValueError(f"Expected a positive whole number for Yahoo split {field}")
    return int(number)


def _column(values, size, field):
    if not isinstance(values, list) or len(values) != size:
        raise ValueError(f"Yahoo {field} column length does not match timestamps")
    return values


def _quote_rows(indicators, timestamp_count, dated_rows, asset_type, adjustment_as_of):
    quote_data = _single(indicators.get("quote"), "quote")
    columns = [_column(quote_data.get(field), timestamp_count, field) for field in _QUOTE_FIELDS]
    adjusted = indicators.get("adjclose")
    adjusted_values = None
    if adjusted is not None:
        adjusted_data = _single(adjusted, "adjclose")
        adjusted_values = _column(adjusted_data.get("adjclose"), timestamp_count, "adjclose")

    reference = asset_type == "index"
    rows = []
    for session, index in sorted(dated_rows):
        o, h, low, close, volume = (
            None if reference and column[index] is None else _number(column[index], f"{session} {field}")
            for field, column in zip(_QUOTE_FIELDS, columns, strict=True)
        )
        prices = (o, h, low, close)
        complete = all(value is not None and value > 0 for value in prices)
        if complete:
            if not low <= min(o, close) <= max(o, close) <= h:
                raise ValueError(f"Invalid Yahoo OHLC range for {session}")
        elif not reference or close == 0 or any(value is not None and value < 0 for value in prices):
            raise ValueError(f"Invalid Yahoo OHLC prices for {session}")
        if volume is not None and (volume < 0 or volume != volume.to_integral_value()):
            raise ValueError(f"Yahoo volume must be a nonnegative whole number for {session}")

        adjusted_value = None if adjusted_values is None else adjusted_values[index]
        adjusted_close, factor = None, None
        if adjusted_value is not None or not reference:
            adjusted_close = _number(adjusted_value, f"{session} adjclose")
            if adjusted_close < 0 or (not reference and adjusted_close == 0):
                raise ValueError(f"Yahoo adjusted close must be positive for {session}")
        if adjusted_close and close:
            with localcontext(ADJUSTMENT_CONTEXT):
                factor = adjusted_close / close

        rows.append(DailyBar(session, o, h, low, close, volume, factor, adjusted_close, adjustment_as_of))

    if len({row.session for row in rows}) != len(rows):
        raise ValueError("Duplicate Yahoo daily session")
    return tuple(rows)


def _event_dates(events, category):
    dated = []
    for source_id, entry in _object(events.get(category, {}), category).items():
        if not source_id:
            raise ValueError(f"Empty source ID for Yahoo {category} event")
        event = _object(entry, category)
        timestamp = event.get("date")
        dated.append((_session(timestamp), timestamp, source_id, event))
    return dated


def _event_key(row):
    return row[:3]


def load_yahoo_daily(
    path: str | Path,
    *,
    adjustment_as_of: date,
    pre_epoch: Literal["reject", "drop"] = "reject",
) -> YahooDaily:
    """Read the saved response as quoted OHLCV, factor and ex-date events.

    Supply the archive snapshot date as adjustment_as_of. Yahoo quotes themselves
    may already reflect split normalization. The factor is adjusted close divided
    by source close and may incorporate later actions than an observation date.
    """
    if type(adjustment_as_of) is not date:
        raise ValueError("Yahoo adjustment_as_of must be an explicit snapshot date")
    document = json.loads(
        Path(path).read_bytes(), parse_float=Decimal, parse_constant=_invalid_constant, object_pairs_hook=_unique_object,
    )
    chart = _object(_object(document, "response").get("chart"), "chart")
    if chart.get("error") is not None:
        raise ValueError("Yahoo chart reported a provider error")
    result = _single(chart.get("result"), "result")
    meta = _object(result.get("meta"), "meta")
    if meta.get("exchangeTimezoneName") != _EXCHANGE_TZ.key or meta.get("dataGranularity") != "1d":
        raise ValueError("Expected Yahoo daily candles on the New York session calendar")

    symbol = meta.get("symbol")
    instrument_type = meta.get("instrumentType")
    if not isinstance(symbol, str) or not _SYMBOL.fullmatch(symbol):
        raise ValueError("Invalid Yahoo chart symbol")
    if not isinstance(instrument_type, str) or instrument_type not in _ASSETS:
        raise ValueError("Yahoo chart must describe a stock, ETF or index")
    asset_type = _ASSETS[instrument_type]
    if symbol.startswith("^") and asset_type != "index":
        raise ValueError("Yahoo index symbol cannot identify an equity")
    currency = meta.get("currency")
    if currency != "USD":
        raise ValueError("Expected a USD Yahoo chart")
    exchange = meta.get("exchangeName")
    listing_venue = None
    if asset_type != "index":
        if not isinstance(exchange, str) or exchange not in _VENUES:
            raise ValueError("Unsupported Yahoo listing venue")
        listing_venue = _VENUES[exchange]

    timestamps = result.get("timestamp")
    if not isinstance(timestamps, list) or not timestamps:
        raise ValueError("Yahoo chart has no daily timestamps")
    dated_rows = [(_session(timestamp), index) for index, timestamp in enumerate(timestamps)]
    events = _object(result.get("events", {}), "events")

    def date_of(row):
        return row[0]

    dated_rows, dropped_bars = guard_epoch(dated_rows, date_of=date_of, policy=pre_epoch)
    dividend_events, dropped_dividends = guard_epoch(
        _event_dates(events, "dividends"), date_of=date_of, policy=pre_epoch,
    )
    split_events, dropped_splits = guard_epoch(
        _event_dates(events, "splits"), date_of=date_of, policy=pre_epoch,
    )
    drops = [entry for entry in (dropped_bars, dropped_dividends, dropped_splits) if entry.count]
    discarded = EpochDiscard(
        sum(entry.count for entry in drops),
        min((entry.first for entry in drops), default=None),
        max((entry.last for entry in drops), default=None),
    )
    for records in (dated_rows, dividend_events, split_events):
        if records and max(row[0] for row in records) > adjustment_as_of:
            raise ValueError("Yahoo adjustment_as_of precedes a bar or corporate action")

    bars = _quote_rows(
        _object(result.get("indicators"), "indicators"), len(timestamps), dated_rows, asset_type, adjustment_as_of,
    )
    dividends = []
    for session, timestamp, source_id, event in sorted(dividend_events, key=_event_key):
        amount = _number(event.get("amount"), f"{session} dividend amount")
        if amount <= 0:
            raise ValueError(f"Yahoo dividend amount must be positive for {session}")
        dividends.append(Dividend(session, timestamp, amount, source_id))
    splits = tuple(
        Split(session, timestamp, _split_leg(event.get("numerator"), "numerator"),
              _split_leg(event.get("denominator"), "denominator"), source_id)
        for session, timestamp, source_id, event in sorted(split_events, key=_event_key)
    )
    return YahooDaily(
        symbol, asset_type, currency, listing_venue, adjustment_as_of, bars, tuple(dividends), splits, discarded,
    )
