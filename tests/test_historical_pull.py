"""Deterministic OHLCV is generated in tests. All providers are replayed or stubbed."""

from datetime import date, datetime, timezone
from decimal import Decimal
from io import StringIO
import json
from pathlib import Path
from subprocess import CompletedProcess
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import patch

from fertile_finance.collector_store import open_store
from fertile_finance.historical_equity import EQUITY_INSTRUMENTS, equity_daily_closes
from fertile_finance.historical_pull import EquityDaily, IbkrMinute, NS_PER_MINUTE, main, pull_bars
from nautilus_trader.persistence.catalog.parquet import ParquetDataCatalog


MINUTE = NS_PER_MINUTE
FIRST = 28_546_201 * MINUTE
INSTRUMENT = "SPY.ARCA"
CHECKPOINT = "fertile_finance.historical_pull.atomic_json"
EXPECTED_IBKR_PRECISION = {
    "input": "ibapi-3.2.0-binary64",
    "price_increment": "0.0001",
    "volume_increment": "0.00000001",
    "rounding": "nearest_ties_even",
}


def bar(ts, instrument=INSTRUMENT):
    return {"instrument": instrument, "ts_ns": ts, "open": "10", "high": "11", "low": "10",
            "close": "10.0000000000000012", "volume": "100.0000000000000001"}


def utc_ns(day, hour=0, minute=0):
    return int(datetime(day.year, day.month, day.day, hour, minute, tzinfo=timezone.utc).timestamp()) * 10**9


class ReplayBars:
    name = "f11-test-replay"
    spec = "1-MINUTE-LAST"
    window_ns = 4 * MINUTE

    def __init__(self, rows, *, interrupt_at=None):
        self.rows = set(rows)
        self.interrupt_at = interrupt_at
        self.calls = []

    def instrument(self, symbol):
        return EQUITY_INSTRUMENTS[symbol]

    def expected(self, start, end):
        return range((start + MINUTE - 1) // MINUTE * MINUTE, end + 1, MINUTE)

    def backfill(self, symbol, start, end):
        self.calls.append((symbol, start, end))
        if self.interrupt_at is not None and start == self.interrupt_at:
            raise ConnectionError("synthetic interruption")
        return [bar(ts, self.instrument(symbol)) for ts in sorted(self.rows, reverse=True) if start <= ts <= end]


class HistoricalPullTests(unittest.TestCase):
    def setUp(self):
        directory = TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.rows = [FIRST + n * MINUTE for n in range(8)]
        self.catalog = self.root / "catalog/historical/f11-test-replay/1-MINUTE-LAST"
        self.state = self.root / "finance-state/historical/f11-test-replay/1-MINUTE-LAST"

    def pull(self, source, **kwargs):
        return pull_bars(self.root, source, ["SPY"], self.rows[0], self.rows[-1],
                         now_ns=self.rows[-1], **kwargs)

    def read_rows(self):
        return sorted(ParquetDataCatalog(self.catalog).bars(), key=lambda row: row.ts_event)

    def test_resume_from_cursor_and_repair_behind_watermark(self):
        limited = ReplayBars([self.rows[0], self.rows[2], self.rows[3]], interrupt_at=self.rows[4])
        with self.assertRaisesRegex(ConnectionError, "synthetic interruption"):
            self.pull(limited)
        self.assertEqual(limited.calls, [("SPY", self.rows[0], self.rows[3]), ("SPY", self.rows[4], self.rows[7])])
        cursor, = self.state.glob("SPY*.json")
        window_end = self.rows[4] - 1
        self.assertEqual(json.loads(cursor.read_text()), {"scanned_through_ns": window_end,
                                                         "windows_with_gaps": [[self.rows[0], window_end]]})
        resumed = ReplayBars(set(self.rows) - {self.rows[1]})
        incomplete = self.pull(resumed)
        self.assertEqual(resumed.calls, [("SPY", self.rows[1], self.rows[1]), ("SPY", self.rows[4], self.rows[7])])
        self.assertEqual(incomplete["status"], "gaps_remaining")
        self.assertEqual(incomplete["instruments"][INSTRUMENT]["gaps"],
                         [{"start_ns": self.rows[1], "end_ns": self.rows[1], "missing": 1}])
        self.assertEqual(incomplete["instruments"][INSTRUMENT]["watermark_ns"], self.rows[-1])
        replay = ReplayBars(self.rows)
        self.assertEqual(self.pull(replay)["status"], "complete")
        self.assertEqual(replay.calls, [("SPY", self.rows[1], self.rows[1])])
        self.assertEqual(self.pull(replay)["status"], "complete")
        self.assertEqual(len(replay.calls), 1)
        self.assertEqual([row.ts_event for row in self.read_rows()], self.rows)
        self.assertTrue(all(row.close.as_decimal() == Decimal("10.0000000000000012") and
                            row.volume.as_decimal() == Decimal("100.0000000000000001")
                            for row in self.read_rows()))

    def test_crash_between_parquet_and_scan_cursor_does_not_request_again(self):
        replay = ReplayBars(self.rows)
        with patch(CHECKPOINT, side_effect=OSError("synthetic checkpoint failure")):
            with self.assertRaisesRegex(OSError, "synthetic checkpoint failure"):
                self.pull(replay)
        self.assertEqual([row.ts_event for row in self.read_rows()], self.rows[:4])
        self.assertEqual(self.pull(replay)["status"], "complete")
        self.assertEqual(replay.calls, [("SPY", self.rows[0], self.rows[3]), ("SPY", self.rows[4], self.rows[7])])
        self.assertEqual([row.ts_event for row in self.read_rows()], self.rows)

    def test_rescan_fills_earlier_and_inter_file_gaps(self):
        with open_store(self.catalog, self.state, ReplayBars.spec, ReplayBars.name, historical=True) as store:
            store.merge_history([bar(self.rows[1]), bar(self.rows[3])])
            store.merge_history([bar(self.rows[6])])
        replay = ReplayBars(self.rows)
        self.assertEqual(self.pull(replay, rescan=True)["status"], "complete")
        self.assertEqual([row.ts_event for row in self.read_rows()], self.rows)
        self.assertEqual(replay.calls, [("SPY", self.rows[0], self.rows[2]), ("SPY", self.rows[4], self.rows[7])])

    def test_batch_rejects_out_of_range_bar_before_publication(self):
        replay = ReplayBars(self.rows)
        with patch.object(replay, "backfill", return_value=[bar(self.rows[-1])]):
            with self.assertRaisesRegex(ValueError, "outside"):
                self.pull(replay)
        self.assertEqual(self.read_rows(), [])

    def test_cli_requires_opt_in_and_disabled_binance_neither_calls_nor_writes(self):
        options = ["--data-root", str(self.root / "untouched"), "--start", "2024-01-01", "--end", "2024-01-02"]
        with (patch("fertile_finance.historical_pull.CryptoBars", side_effect=AssertionError("provider construction")),
              patch("fertile_finance.historical_pull.import_binance_trades", side_effect=AssertionError("archive request")),
              patch("sys.stdout", new_callable=StringIO) as output):
            self.assertEqual(main(["binance", "--allow-download", *options]), 0)
            self.assertEqual(main(["binance-trades", "--allow-download", *options]), 0)
        self.assertEqual([json.loads(line)["status"] for line in output.getvalue().splitlines()],
                         ["disabled_by_profile", "disabled_by_profile"])
        with patch("sys.stderr", new_callable=StringIO), self.assertRaises(SystemExit) as denied:
            main(["yahoo-daily", *options])
        self.assertEqual(denied.exception.code, 2)
        self.assertEqual(list(self.root.iterdir()), [])

    def test_invalid_root_and_symlink_fail_before_fetch(self):
        checkout = Path(__file__).resolve().parents[1]
        replay = ReplayBars(self.rows)
        for root in (Path("relative-data"), checkout / "history-data"):
            with self.assertRaises(ValueError):
                pull_bars(root, replay, ["SPY"], *self.rows[::7], now_ns=self.rows[-1])
        with TemporaryDirectory() as outside:
            (self.root / "catalog").symlink_to(outside, target_is_directory=True)
            with self.assertRaisesRegex(ValueError, "inside the configured data root"):
                self.pull(replay)
            self.assertEqual(list(Path(outside).iterdir()), [])
        self.assertEqual(replay.calls, [])

    def test_nine_equities_schedule_and_ibkr_minute_cli_bounds(self):
        # Explicit market-calendar examples; OHLCV and CLI return values are synthetic.
        early = date(2024, 11, 29)
        holiday = date(2024, 11, 28)
        start, end = utc_ns(holiday), utc_ns(early, 23, 59)
        early_close = utc_ns(early, 18)
        self.assertEqual(equity_daily_closes(start, end), {early: early_close})
        calls = []

        def fake_rust(cmd, **kwargs):
            calls.append(cmd)
            self.assertEqual(kwargs["env"]["FERTILE_DATA_ROOT"], str(self.root))
            self.assertEqual(cmd[:3], ["fake-ib", "historical-minute", "--allow-download"])
            self.assertEqual(int(cmd[-2]) * 10**9, utc_ns(early, 14, 30))
            self.assertEqual(int(cmd[-1]) * 10**9, early_close)
            instrument = EQUITY_INSTRUMENTS[cmd[3]]
            minute_rows = [bar(ts, instrument) | {"close": "10.0123", "volume": "100.00000001"}
                           for ts in range(utc_ns(early, 14, 31), early_close + 1, MINUTE)]
            response = {"precision": EXPECTED_IBKR_PRECISION, "bars": minute_rows}
            return CompletedProcess(cmd, 0, json.dumps(response).encode())

        report = pull_bars(self.root, IbkrMinute("fake-ib", self.root, run=fake_rust),
                           EQUITY_INSTRUMENTS, start, end, now_ns=end)
        self.assertEqual(report["status"], "complete")
        self.assertEqual([cmd[3] for cmd in calls], list(EQUITY_INSTRUMENTS))
        catalog = self.root / "catalog/historical/ibkr-minute/1-MINUTE-LAST"
        state = self.root / "finance-state/historical/ibkr-minute/1-MINUTE-LAST"
        rows = ParquetDataCatalog(catalog).bars()
        self.assertEqual(len(rows), 9 * 210)
        self.assertTrue(all(row.close.as_decimal() == Decimal("10.0123")
                            and row.volume.as_decimal() == Decimal("100.00000001") for row in rows))
        provenance = json.loads((catalog / ".finance-collector/source.json").read_text())
        self.assertEqual(provenance["source_metadata"], {"decimal_policy": EXPECTED_IBKR_PRECISION})
        self.assertEqual(json.loads((state / "catalog.json").read_text()), provenance)

    def test_ibkr_rejects_old_or_mismatched_binary_before_bar_publication(self):
        at = utc_ns(date(2024, 11, 29), 14, 31)
        bars = [bar(at) | {"close": "10.0123", "volume": "100.00000001"}]
        catalog = self.root / "catalog/historical/ibkr-minute/1-MINUTE-LAST"
        for payload in (bars, {"precision": EXPECTED_IBKR_PRECISION | {"price_increment": "0.01"}, "bars": bars}):
            with self.subTest(payload=payload):
                def fake_rust(cmd, **kwargs):
                    return CompletedProcess(cmd, 0, json.dumps(payload).encode())

                provider = IbkrMinute("fake-ib", self.root, run=fake_rust)
                with self.assertRaisesRegex(ValueError, "precision policy does not match"):
                    pull_bars(self.root, provider, ["SPY"], at, at, now_ns=at)
                self.assertEqual(ParquetDataCatalog(catalog).bars(), [])

    def test_ibkr_refuses_to_relabel_legacy_catalog_before_fetch(self):
        at = utc_ns(date(2024, 11, 29), 14, 31)
        catalog = self.root / "catalog/historical/ibkr-minute/1-MINUTE-LAST"
        state = self.root / "finance-state/historical/ibkr-minute/1-MINUTE-LAST"
        with open_store(catalog, state, "1-MINUTE-LAST", "ibkr-minute", historical=True) as store:
            store.merge_history([bar(at)])

        def fake_rust(*args, **kwargs):
            raise AssertionError("unlabelled legacy catalog must be rejected before requesting")

        provider = IbkrMinute("fake-ib", self.root, run=fake_rust)
        with self.assertRaisesRegex(ValueError, "different source metadata"):
            pull_bars(self.root, provider, ["SPY"], at, at, now_ns=at)
        provenance = json.loads((catalog / ".finance-collector/source.json").read_text())
        self.assertNotIn("source_metadata", provenance)
        self.assertEqual([row.ts_event for row in ParquetDataCatalog(catalog).bars()], [at])

    def test_equity_daily_does_not_request_an_open_session(self):
        day = date(2024, 11, 29)

        class DailyReplay:
            def fetch(self, symbol, start, end):
                raise AssertionError("open daily session fetched")

        report = pull_bars(self.root, EquityDaily(DailyReplay()), ["SPY"], utc_ns(day), utc_ns(day, 23),
                           now_ns=utc_ns(day, 17, 59))
        self.assertEqual(report["instruments"][INSTRUMENT]["watermark_ns"], None)


if __name__ == "__main__":
    unittest.main()
