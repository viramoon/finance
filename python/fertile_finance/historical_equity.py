"""Closed US cash-session bars and the Yahoo daily history source."""

from datetime import date, datetime
from time import time_ns
from zoneinfo import ZoneInfo

from .download_yahoo_daily import fetch_yahoo_daily
from .epoch import EPOCH, unix_nanos
from .research_catalog import LATEST, nyse
from .yahoo_daily import parse_yahoo_daily


EQUITY_INSTRUMENTS = {
    "QQQ": "QQQ.XNAS",
    "SPY": "SPY.ARCA",
    "AAPL": "AAPL.XNAS",
    "MSFT": "MSFT.XNAS",
    "NVDA": "NVDA.XNAS",
    "AMZN": "AMZN.XNAS",
    "GOOGL": "GOOGL.XNAS",
    "META": "META.XNAS",
    "TSLA": "TSLA.XNAS",
}
_ETFS = {"QQQ", "SPY"}
_NEW_YORK = ZoneInfo("America/New_York")
_OHLCV = ("open", "high", "low", "close", "volume")


def equity_sessions(start: date, end: date):
    """Yield date, UTC open ns and UTC close ns for inclusive New York dates."""
    if type(start) is not date or type(end) is not date or not EPOCH <= start <= end <= LATEST:
        raise ValueError(f"Expected ordered New York session dates from {EPOCH} through {LATEST}")
    schedule = nyse().schedule.loc[str(start):str(end), ["open", "close"]]
    for label, opens, closes in schedule.itertuples():
        yield label.date(), unix_nanos(opens.value), unix_nanos(closes.value)


def _ny_date(timestamp_ns):
    return datetime.fromtimestamp(unix_nanos(timestamp_ns) // 1_000_000_000, tz=_NEW_YORK).date()


def equity_daily_closes(start_ns: int, end_ns: int) -> dict[date, int]:
    """Return the sessions with a scheduled daily close in the inclusive UTC range."""
    start, end = _ny_date(start_ns), _ny_date(end_ns)
    if start_ns > end_ns:
        raise ValueError("Expected an ordered daily bar range")
    return {
        day: close for day, _, close in equity_sessions(max(EPOCH, start), max(EPOCH, end))
        if start_ns <= close <= end_ns
    }


class YahooDailySource:
    """Fetch exact source OHLCV and date each bar at its cash session close."""

    def __init__(self, *, fetch=fetch_yahoo_daily, now_ns=time_ns):
        self._fetch = fetch
        self._now_ns = now_ns

    def fetch(self, symbol: str, start_ns: int, end_ns: int) -> list[dict]:
        if symbol not in EQUITY_INSTRUMENTS:
            raise ValueError(f"Unsupported US equity symbol: {symbol}")
        instrument = EQUITY_INSTRUMENTS[symbol]
        if unix_nanos(start_ns) > unix_nanos(end_ns):
            raise ValueError("Expected an ordered daily bar range")
        observed_ns = unix_nanos(self._now_ns())
        through_ns = min(end_ns, observed_ns)
        if start_ns > through_ns:
            return []
        expected = equity_daily_closes(start_ns, through_ns)
        if not expected:
            return []
        raw, _ = self._fetch(symbol, next(iter(expected)), next(reversed(expected)), allow_empty=True)
        if raw is None:
            return []
        history = parse_yahoo_daily(raw, adjustment_as_of=_ny_date(observed_ns))
        asset_type = "etf" if symbol in _ETFS else "stock"
        if history.series_id != instrument or history.asset_type != asset_type:
            raise ValueError(f"Yahoo chart identity does not match {instrument} ({asset_type})")
        return [
            {
                "instrument": instrument,
                "ts_ns": expected[row.session],
                **{field: format(getattr(row, field), "f") for field in _OHLCV},
            }
            for row in history.bars
            if row.session in expected and not row.is_missing_quote
        ]
