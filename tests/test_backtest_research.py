import csv
from datetime import date, datetime, timezone
from decimal import Decimal
from pathlib import Path
from tempfile import TemporaryDirectory
import unittest

from nautilus_trader.model.data import Bar
from nautilus_trader.model.objects import Price, Quantity
from nautilus_trader.persistence.catalog.parquet import ParquetDataCatalog

from fertile_finance.backtest_research import coverage_windows, run_research
from fertile_finance.collector_store import open_store
from fertile_finance.research_catalog import ResearchError, bar_type, import_catalog, sessions


def fixture_csv(path, rows):
    with path.open("w", newline="") as handle:
        writer = csv.writer(handle)
        writer.writerow(["date", "open", "high", "low", "close", "volume"])
        writer.writerows((day, px, px, px, px, volume) for day, px, volume in rows)


class BacktestResearch(unittest.TestCase):
    def setUp(self):
        self.tmp = TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.csv = self.root / "input.csv"
        self.catalog = self.root / "catalog"

    def load(self, rows, instrument="TEST.XNYS"):
        fixture_csv(self.csv, rows)
        return import_catalog(self.catalog, self.csv, instrument=instrument, source="Deterministic fixture",
                              kind="fixture", asset_type="stock")

    def job(self, *, start="2024-11-25", end="2024-12-05", instrument="TEST.XNYS"):
        return dict(instrument=instrument, start=start+"T00:00:00Z", end=end+"T00:00:00Z",
                    strategy="SmaCrossover", params=dict(fast_period=2, slow_period=3, shares=10))

    def test_response_audits_fills_after_a_different_session_signal(self):
        days = list(sessions(date(2024, 11, 25), date(2024, 12, 5)))
        fixtures = zip(days, [90, 100, 130, 130, 140, 150, 30, 60],
                      [100, 100, 100, 0, 2, 100, 100, 100], strict=True)
        manifest = self.load(fixtures)
        self.assertEqual(manifest["observations"], 8)
        self.assertEqual(len(ParquetDataCatalog(self.catalog / "TEST.XNYS").bars()), 8)
        out = run_research(self.catalog, self.job())
        report = out["research"]
        audit = report["audit"]
        self.assertEqual(report["performance_start"], "2024-11-27")
        self.assertEqual(out["pnl"], "-903.10")
        self.assertEqual(audit["zero_volume_bars"], 1)
        self.assertEqual(audit["blocked_zero_volume"], 1)
        self.assertEqual(audit["blocked_insufficient_volume"], 1)
        self.assertEqual(audit["fills"], out["total_orders"])
        self.assertEqual(audit["fills"], 2)
        self.assertEqual(len({fill["order_id"] for fill in audit["recent_fills"]}), 2)
        self.assertEqual(
            [(fill["side"], fill["signal_session"], fill["execution_session"], fill["shares"], fill["price"])
             for fill in audit["recent_fills"]],
            [("buy", "2024-12-02", "2024-12-03", 10, "150.08"),
             ("sell", "2024-12-04", "2024-12-05", 10, "59.97")],
        )

    def test_date_calendar_and_failures(self):
        during_dst = sessions(date(2024, 7, 3), date(2024, 7, 4))
        self.assertEqual(set(during_dst), {date(2024, 7, 3)})
        self.assertEqual(datetime.fromtimestamp(during_dst[date(2024, 7, 3)] // 10**9, timezone.utc).hour, 17)
        for day, hour in [(date(2024, 7, 5), 20), (date(2024, 11, 29), 18), (date(2024, 12, 2), 21)]:
            self.assertEqual(datetime.fromtimestamp(sessions(day, day)[day] // 10**9, timezone.utc).hour, hour)
        with self.assertRaisesRegex(ResearchError, "XNYS session"):
            self.load([("2024-11-28", 100, 100)])  # Thanksgiving
        with self.assertRaisesRegex(ResearchError, "stock or ETF"):
            self.load([("2024-11-25", 100, 100)], instrument="^GSPC.XNYS")
        with self.assertRaisesRegex(ResearchError, "calendar date"):
            self.load([("1969-12-31", 100, 100)])
        self.load([("2024-11-25", 100, 100), ("2024-11-27", 100, 100)])
        with self.assertRaisesRegex(ResearchError, "No research catalog"):
            run_research(self.catalog, self.job(instrument="NONE.XNYS"))
        with self.assertRaisesRegex(ResearchError, "missing XNYS sessions"):
            run_research(self.catalog, self.job())
        with self.assertRaisesRegex(ResearchError, "at least 4 consecutive"):
            run_research(self.catalog, self.job(start="2024-11-24", end="2024-11-25"))

    def test_runtime_epoch_guard_and_parquet_readback(self):
        kind = bar_type("TEST.XNYS")
        fields = [Price.from_str("100.00")] * 4 + [Quantity.from_int(1)]
        with self.assertRaises(OverflowError) as before:
            Bar(kind, *fields, -1, -1)
        print("Pinned Nautilus negative ts_event constructor:", type(before.exception).__name__)
        with open_store(self.catalog, self.root / "state", "1-DAY-LAST") as store:
            def row(ns):
                return dict(instrument="TEST.XNYS", ts_ns=ns,
                            open="100", high="100", low="100", close="100", volume="1")
            with self.assertRaisesRegex(ValueError, "at or after the epoch"):
                store.append([row(-1), row(0)])
            self.assertFalse(ParquetDataCatalog(self.catalog).bars())
            store.append([row(0), row(1)])
        with open_store(self.catalog, self.root / "state", "1-DAY-LAST") as store:
            self.assertEqual(store.marks["TEST.XNYS"], 1)
        self.assertEqual([bar.ts_event for bar in ParquetDataCatalog(self.catalog).bars()], [0, 1])

    def test_window_boundaries_and_missing_days(self):
        end = date(2026, 11, 20)
        sample = {day: Decimal(100) + n for n, day in
                  enumerate(sessions(date(1993, 1, 29), end))}
        rows = coverage_windows(sample, end)
        self.assertEqual([row["coverage"] for row in rows], ["insufficient", "full", "full"])
        self.assertEqual(rows[0]["actual_start"], "1993-01-29")
        self.assertIsNone(rows[0]["metrics"])
        self.assertGreater(rows[0]["missing_sessions"], 4000)
        sample.pop(date(2024, 7, 5))
        self.assertEqual([row["coverage"] for row in coverage_windows(sample, end)], ["insufficient"]*3)
        days_50 = list(sessions(date(1976, 11, 20), end))
        sample = {day: Decimal(100) + n for n, day in enumerate(days_50[5:])}
        self.assertEqual([row["coverage"] for row in coverage_windows(sample, end)], ["full"]*3)
        sample.pop(days_50[5])
        self.assertEqual(coverage_windows(sample, end)[0]["coverage"], "insufficient")
