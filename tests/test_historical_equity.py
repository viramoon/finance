"""Offline Yahoo chart replays. Quote rows use fixed synthetic constants below."""

from calendar import timegm
from datetime import date, datetime, time
from decimal import Decimal
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import Mock, patch
from zoneinfo import ZoneInfo

from nautilus_trader.persistence.catalog.parquet import ParquetDataCatalog

from fertile_finance import download_yahoo_daily
from fertile_finance.historical_equity import YahooDailySource, equity_daily_closes, equity_sessions
from fertile_finance.historical_pull import EquityDaily, pull_bars


SYMBOLS = ("QQQ", "SPY", "AAPL", "MSFT", "NVDA", "AMZN", "GOOGL", "META", "TSLA")
EXACT_CLOSE = "101.1234567890123456"
NEW_YORK = ZoneInfo("America/New_York")
JULY_2 = date(2024, 7, 2)
JULY_3 = date(2024, 7, 3)
JULY_4 = date(2024, 7, 4)
JULY_5 = date(2024, 7, 5)


def ns(value):
    return timegm(datetime.fromisoformat(value).utctimetuple()) * 1_000_000_000


def chart(symbol, days, *, missing=(), venue=None, asset=None):
    timestamps = [timegm(datetime.combine(day, time(9, 30), NEW_YORK).utctimetuple()) for day in days]
    quote = {
        field: [None if day in missing else value for day in days]
        for field, value in {
            "open": 100, "high": 102, "low": 99, "close": "synthetic-close", "volume": 1234567,
        }.items()
    }
    data = {"chart": {"error": None, "result": [{
        "meta": {
            "symbol": symbol,
            "exchangeTimezoneName": "America/New_York",
            "dataGranularity": "1d",
            "currency": "USD",
            "exchangeName": venue or ("PCX" if symbol == "SPY" else "NMS"),
            "instrumentType": asset or ("ETF" if symbol in {"QQQ", "SPY"} else "EQUITY"),
        },
        "timestamp": timestamps,
        "indicators": {"quote": [quote], "adjclose": [{"adjclose": [50] * len(days)}]},
    }]}}
    return json.dumps(data).replace('"synthetic-close"', EXACT_CLOSE).encode()


def chart_error(code, description):
    return json.dumps({"chart": {"result": None, "error": {"code": code, "description": description}}}).encode()


class HistoricalEquityTests(unittest.TestCase):
    def setUp(self):
        self.http = self.enterContext(patch.object(download_yahoo_daily, "HTTPSConnection"))
        self.http.side_effect = AssertionError("unexpected unscripted HTTP request")

    def replay(self, payload, *, status=200):
        http = Mock()
        reply = Mock(status=status)
        reply.read.return_value = payload
        reply.getheader.return_value = None
        http.getresponse.return_value = reply
        self.http.side_effect = [http]
        return http

    def test_nine_equities_replay_with_exact_source_ohlcv(self):
        close_ns = ns("2024-07-03T17:00:00+00:00")
        source = YahooDailySource(now_ns=lambda: close_ns)
        for symbol in SYMBOLS:
            with self.subTest(symbol=symbol):
                http = self.replay(chart(symbol, [JULY_3]))
                self.assertEqual(source.fetch(symbol, close_ns, close_ns), [{
                    "instrument": f"{symbol}.{'ARCA' if symbol == 'SPY' else 'XNAS'}",
                    "ts_ns": close_ns, "open": "100", "high": "102", "low": "99",
                    "close": EXACT_CLOSE, "volume": "1234567",
                }])
                method, target = http.request.call_args.args
                self.assertEqual(method, "GET")
                self.assertTrue(target.startswith(f"/v8/finance/chart/{symbol}?"))
                http.close.assert_called_once_with()
        self.assertEqual(self.http.call_count, 9)

    def test_yahoo_replay_round_trips_through_collector_parquet(self):
        close_ns = ns("2024-07-03T17:00:00+00:00")
        self.replay(chart("AAPL", [JULY_3]))
        provider = EquityDaily(YahooDailySource(now_ns=lambda: close_ns))
        root = Path(self.enterContext(tempfile.TemporaryDirectory(dir="/tmp")))
        report = pull_bars(root, provider, ["AAPL"], close_ns, close_ns, now_ns=close_ns)
        self.assertEqual(report["status"], "complete")
        self.assertEqual(report["instruments"]["AAPL.XNAS"]["watermark_ns"], close_ns)
        catalog = root / "catalog" / "historical" / provider.name / provider.spec
        (bar,) = ParquetDataCatalog(catalog).bars(bar_types=["AAPL.XNAS-1-DAY-LAST-EXTERNAL"])
        self.assertEqual((bar.ts_event, bar.ts_init), (close_ns, close_ns))
        self.assertEqual(bar.close.as_decimal(), Decimal(EXACT_CLOSE))
        self.assertEqual(bar.volume.as_decimal(), Decimal(1234567))

    def test_calendar_uses_dst_and_early_close_and_skips_holidays(self):
        self.assertEqual(list(equity_sessions(JULY_2, JULY_5)), [
            (JULY_2, ns("2024-07-02T13:30:00+00:00"), ns("2024-07-02T20:00:00+00:00")),
            (JULY_3, ns("2024-07-03T13:30:00+00:00"), ns("2024-07-03T17:00:00+00:00")),
            (JULY_5, ns("2024-07-05T13:30:00+00:00"), ns("2024-07-05T20:00:00+00:00")),
        ])
        self.assertEqual(list(equity_sessions(date(2024, 11, 28), date(2024, 12, 1))), [
            (date(2024, 11, 29), ns("2024-11-29T14:30:00+00:00"), ns("2024-11-29T18:00:00+00:00")),
        ])
        opens = ns("2024-07-03T13:30:00+00:00")
        close = ns("2024-07-03T17:00:00+00:00")
        self.assertEqual(equity_daily_closes(opens, close - 1), {})
        self.assertEqual(equity_daily_closes(close, close), {JULY_3: close})

    def test_unclosed_and_holiday_windows_make_no_request(self):
        now = ns("2024-07-03T16:59:59+00:00")
        source = YahooDailySource(now_ns=lambda: now)
        self.assertEqual(source.fetch("QQQ", ns("2024-07-03T00:00:00+00:00"), now + 10**9), [])
        source = YahooDailySource(now_ns=lambda: ns("2024-07-05T00:00:00+00:00"))
        self.assertEqual(source.fetch("QQQ", ns("2024-07-04T00:00:00+00:00"),
                                      ns("2024-07-04T23:59:59+00:00")), [])
        with self.assertRaisesRegex(ValueError, "Unsupported US equity symbol"):
            source.fetch("IBM", 0, 0)
        self.http.assert_not_called()

    def test_missing_quotes_and_out_of_range_rows_remain_unpublished(self):
        now = ns("2024-07-05T19:59:59+00:00")
        source = YahooDailySource(now_ns=lambda: now)
        # Simulate a provider that includes a null quote, holiday quote and an in-flight day's quote.
        self.replay(chart("SPY", [JULY_4, JULY_5, JULY_3, JULY_2], missing=[JULY_3]))
        rows = source.fetch("SPY", ns("2024-07-02T00:00:00+00:00"), now + 10**9)
        self.assertEqual([bar["ts_ns"] for bar in rows], [ns("2024-07-02T20:00:00+00:00")])

    def test_wrong_listing_or_asset_class_is_rejected(self):
        close_ns = ns("2024-07-03T17:00:00+00:00")
        source = YahooDailySource(now_ns=lambda: close_ns)
        for payload in (chart("SPY", [JULY_3], venue="NMS"), chart("SPY", [JULY_3], asset="EQUITY")):
            with self.subTest(payload=payload):
                self.replay(payload)
                with self.assertRaisesRegex(ValueError, r"identity does not match SPY\.ARCA \(etf\)"):
                    source.fetch("SPY", close_ns, close_ns)

    def test_empty_windows_allow_later_replay_to_publish(self):
        first_ns = ns("2024-05-31T20:00:00+00:00")
        last_ns = ns("2024-07-03T17:00:00+00:00")
        first = self.replay(chart_error("Not Found", "No data found, symbol may be delisted"), status=404)
        second = self.replay(chart("AAPL", [JULY_3]))
        self.http.side_effect = [first, second]
        provider = EquityDaily(YahooDailySource(now_ns=lambda: last_ns))
        root = Path(self.enterContext(tempfile.TemporaryDirectory(dir="/tmp")))
        report = pull_bars(root, provider, ["AAPL"], first_ns, last_ns, now_ns=last_ns)
        self.assertEqual(report["status"], "gaps_remaining")
        status = report["instruments"]["AAPL.XNAS"]
        self.assertEqual(status["scanned_through_ns"], last_ns)
        self.assertEqual(status["gaps"][0]["start_ns"], first_ns)
        catalog = root / "catalog" / "historical" / provider.name / provider.spec
        self.assertEqual([bar.ts_event for bar in ParquetDataCatalog(catalog).bars()], [last_ns])

    def test_successful_empty_chart_and_known_no_data_error(self):
        close_ns = ns("2024-07-03T17:00:00+00:00")
        source = YahooDailySource(now_ns=lambda: close_ns)
        for payload in (chart("AAPL", []), chart_error(
            "Not Found", "Data doesn't exist for startDate = 1719964800, endDate = 1720051200",
        )):
            with self.subTest(payload=payload):
                self.replay(payload)
                self.assertEqual(source.fetch("AAPL", close_ns, close_ns), [])

    def test_authorization_and_unrecognized_not_found_fail(self):
        close_ns = ns("2024-07-03T17:00:00+00:00")
        source = YahooDailySource(now_ns=lambda: close_ns)
        for status, payload in (
            (401, chart_error("Unauthorized", "permission required")),
            (200, chart_error("Unauthorized", "permission required")),
            (403, chart_error("Not Found", "No data found, symbol may be delisted")),
            (404, chart_error("Not Found", "permission required")),
            (404, b"not a Yahoo chart"),
        ):
            with self.subTest(status=status, payload=payload):
                http = self.replay(payload, status=status)
                with self.assertRaisesRegex(download_yahoo_daily.DownloadError, f"HTTP {status}") as error:
                    source.fetch("AAPL", close_ns, close_ns)
                self.assertNotIn("permission required", str(error.exception))
                if status in {401, 403}:
                    http.getresponse.return_value.read.assert_not_called()

    def test_raw_archive_keeps_nonempty_requirement(self):
        root = Path(self.enterContext(tempfile.TemporaryDirectory(dir="/tmp")))
        output = root / "archive.json"
        for status, payload in ((200, chart("AAPL", [])),
                                (404, chart_error("Not Found", "No data found, symbol may be delisted"))):
            with self.subTest(status=status):
                http = self.replay(payload, status=status)
                with self.assertRaisesRegex(download_yahoo_daily.DownloadError, f"HTTP {status}"):
                    download_yahoo_daily.download_yahoo_daily("AAPL", JULY_3, JULY_3, output)
                self.assertFalse(output.exists())
                if status == 404:
                    http.getresponse.return_value.read.assert_not_called()
