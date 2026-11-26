import csv
from dataclasses import replace
from datetime import date, datetime, timezone
from decimal import Decimal
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import patch

from nautilus_trader.persistence.catalog.parquet import ParquetDataCatalog

from fertile_finance.backtest_job import execute
from fertile_finance.historical_catalog import HistoricalCatalog
from fertile_finance.research_catalog import sessions
from fertile_finance.yahoo_daily import load_yahoo_daily


REPO = Path(__file__).resolve().parents[1]
FIXTURE = REPO / "tests" / "fixtures" / "historical" / "yahoo_ibm.json"
AS_OF = date(2024, 12, 3)
IBM_2024_START = date(2024, 1, 2)
IBM_2024_END = date(2024, 12, 31)


def ibm_2024_archive(path, *, first_open="162.8300018310547"):
    """Construct a complete fixture with the issue's first open as an exact JSON token."""
    chart = json.loads(FIXTURE.read_bytes())
    source = chart["chart"]["result"][0]
    calendar = sessions(IBM_2024_START, IBM_2024_END)
    source["timestamp"] = [close // 10**9 for close in calendar.values()]
    source["events"] = {}
    # Vary prices so the official SMA and performance code see a changing NAV.
    cyclic_closes = [200 - abs(index % 80 - 40) for index in range(1, len(calendar))]
    source["indicators"] = {
        "quote": [{
            "open": ["first-open", *cyclic_closes],
            "high": ["first-high", *(close + 2 for close in cyclic_closes)],
            "low": [160, *(close - 2 for close in cyclic_closes)],
            "close": [163, *cyclic_closes],
            "volume": [1000] * len(calendar),
        }],
        "adjclose": [{"adjclose": [150] * len(calendar)}],
    }
    # An error of exactly 0.00002 USD is inside the inclusive tolerance.
    payload = json.dumps(chart).replace('"first-open"', first_open).replace('"first-high"', "165.00998")
    path.write_text(payload, encoding="utf-8")


class ConvertResearchCSV(unittest.TestCase):
    def setUp(self):
        directory = TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.csv = self.root / "candidate.csv"
        self.historical_root = self.root / "history"
        self.research_root = self.root / "research"
        self.env = dict(os.environ, PYTHONPATH=str(REPO / "python"),
                        FERTILE_DATA_ROOT=str(self.root / "unused-data"),
                        FERTILE_RESEARCH_ROOT=str(self.root / "unused-research"))

    def run_module(self, name, *args, env=None):
        return subprocess.run([sys.executable, "-m", f"fertile_finance.{name}", *map(str, args)],
                              capture_output=True, text=True, env=self.env if env is None else env,
                              timeout=30, check=False)

    def archive_args(self):
        return ("--yahoo-json", FIXTURE, "--adjustment-as-of", AS_OF)

    def catalog_args(self):
        return ("--historical-data-root", self.historical_root, "--series-id", "IBM.XNYS")

    def convert(self, *args):
        return self.run_module("convert_research_csv", *args, "--output", self.csv)

    def test_yahoo_subrange_exports_source_quote_and_imports_official_csv(self):
        result = self.convert(*self.archive_args(), "--start", "2024-07-01", "--end", "2024-07-02")
        self.assertEqual(result.returncode, 0, result.stderr)
        lineage = json.loads(result.stdout)
        self.assertEqual(lineage["source_kind"], "yahoo_archive")
        self.assertEqual(lineage["source_path"], str(FIXTURE))
        self.assertEqual(lineage["price_basis"], "yahoo_chart_quote")
        self.assertEqual(lineage["adjustment_as_of"], [str(AS_OF)])
        self.assertEqual((lineage["first_session"], lineage["last_session"], lineage["observations"]),
                         ("2024-07-02", "2024-07-02", 1))
        self.assertEqual(lineage["sha256"], hashlib.sha256(self.csv.read_bytes()).hexdigest())
        self.assertFalse((self.root / "unused-data").exists())
        self.assertFalse((self.root / "unused-research").exists())
        with self.csv.open(encoding="utf-8", newline="") as stream:
            self.assertEqual(list(csv.reader(stream)), [
                ["date", "open", "high", "low", "close", "volume"],
                ["2024-07-02", "149.00", "151.00", "147.00", "150.00", "23456789"],
            ])

        imported = self.run_module(
            "research_catalog", self.csv, "--instrument", lineage["series_id"], "--asset-type", lineage["asset_type"],
            "--kind", "fixture", "--source", lineage["source"], "--raw-split-free-reviewed",
            env={**self.env, "FERTILE_RESEARCH_ROOT": str(self.research_root)},
        )
        self.assertEqual(imported.returncode, 0, imported.stderr)
        self.assertEqual(json.loads(imported.stdout)["source_sha256"], lineage["sha256"])
        (bar,) = ParquetDataCatalog(self.research_root / "IBM.XNYS").bars()
        self.assertEqual(bar.close.as_decimal(), Decimal("150"))

    def test_catalog_subrange_preserves_mixed_snapshot_lineage(self):
        catalog = HistoricalCatalog(self.historical_root)
        batch = load_yahoo_daily(FIXTURE, adjustment_as_of=AS_OF)
        catalog.write_yahoo(batch)
        updated_as_of = date(2024, 12, 4)
        updated = replace(batch.bars[0], open=Decimal("148.12"), close=Decimal("150.12"),
                          adjustment_as_of=updated_as_of)
        catalog.write_yahoo(replace(batch, bars=(updated,), dividends=(), splits=(), adjustment_as_of=updated_as_of),
                            mode="replace")

        result = self.convert(*self.catalog_args(), "--start", "2024-01-03", "--end", "2024-07-02")
        self.assertEqual(result.returncode, 0, result.stderr)
        lineage = json.loads(result.stdout)
        self.assertEqual(lineage["source_kind"], "historical_catalog")
        self.assertEqual(Path(lineage["source_path"]), catalog.root / "historical/yahoo/IBM.XNYS")
        self.assertEqual(lineage["adjustment_as_of"], [str(AS_OF), str(updated_as_of)])
        self.assertEqual((lineage["first_session"], lineage["last_session"], lineage["observations"]),
                         ("2024-01-03", "2024-07-02", 2))
        self.assertGreater(lineage["missing_sessions"], 0)
        with self.csv.open(encoding="utf-8", newline="") as stream:
            rows = list(csv.DictReader(stream))
        self.assertEqual([row["close"] for row in rows], ["150.12", "150.00"])
        self.assertEqual([row["open"] for row in rows], ["148.12", "149.00"])

    def test_both_inputs_reject_selected_splits_even_without_a_quote_on_the_split_date(self):
        batch = load_yahoo_daily(FIXTURE, adjustment_as_of=AS_OF)
        split_day = batch.splits[0].session
        HistoricalCatalog(self.historical_root).write_yahoo(
            replace(batch, bars=tuple(row for row in batch.bars if row.session != split_day)),
        )
        for source in (self.archive_args(), self.catalog_args()):
            with self.subTest(source=source[0]):
                for interval in ((), ("--start", "2024-07-02", "--end", str(split_day))):
                    result = self.convert(*source, *interval)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn(f"Yahoo splits on {split_day}", result.stderr)
                    self.assertFalse(self.csv.exists())

    def test_missing_session_quotes_are_counted_without_substituting_adjusted_close(self):
        chart = json.loads(FIXTURE.read_bytes())
        source = chart["chart"]["result"][0]
        source["timestamp"] = [int(datetime(2024, 7, day, 14, tzinfo=timezone.utc).timestamp()) for day in range(1, 6)]
        source["events"] = {}
        source["indicators"] = {
            "quote": [{field: [100, None, 110, None, 120] for field in ("open", "high", "low", "close")}
                      | {"volume": [500, None, 100, None, 0]}],
            "adjclose": [{"adjclose": [90, 95, 99, None, 108]}],
        }
        archive = self.root / "sparse.json"
        archive.write_text(json.dumps(chart), encoding="utf-8")

        result = self.convert("--yahoo-json", archive, "--adjustment-as-of", AS_OF)
        self.assertEqual(result.returncode, 0, result.stderr)
        lineage = json.loads(result.stdout)
        self.assertEqual((lineage["observations"], lineage["missing_sessions"], lineage["missing_quote_rows"],
                          lineage["skipped_missing_non_sessions"]), (3, 1, 1, 1))
        with self.csv.open(encoding="utf-8", newline="") as stream:
            rows = list(csv.DictReader(stream))
        self.assertEqual([(row["date"], row["close"], row["volume"]) for row in rows], [
            ("2024-07-01", "100.00", "500"), ("2024-07-03", "110.00", "100"), ("2024-07-05", "120.00", "0"),
        ])

    def test_fractional_cent_source_is_rejected_and_existing_output_is_preserved(self):
        subrange = ("--start", "2024-01-03", "--end", "2024-01-03")
        result = self.convert(*self.archive_args(), *subrange)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Source quote open on 2024-01-03 cannot be represented exactly in cents", result.stderr)
        self.assertFalse(self.csv.exists())

        self.csv.write_bytes(b"existing content")
        result = self.convert(*self.archive_args(), "--start", "2024-07-02", "--end", "2024-07-02")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Output already exists", result.stderr)
        self.assertEqual(self.csv.read_bytes(), b"existing content")
        self.assertEqual(list(self.root.glob(".research-csv-*")), [])

    def test_ibm_2024_documented_example_converts_imports_and_runs_official_job(self):
        archive = self.root / "ibm-2024.json"
        ibm_2024_archive(archive)
        batch = load_yahoo_daily(archive, adjustment_as_of=IBM_2024_END)
        HistoricalCatalog(self.historical_root).write_yahoo(batch)
        interval = ("--start", IBM_2024_START, "--end", IBM_2024_END)
        from_archive = self.convert("--yahoo-json", archive, "--adjustment-as-of", IBM_2024_END, *interval)
        self.assertEqual(from_archive.returncode, 0, from_archive.stderr)
        archive_csv = self.csv.read_bytes()
        archive_lineage = json.loads(from_archive.stdout)
        self.csv.unlink()

        from_catalog = self.convert(*self.catalog_args(), *interval)
        self.assertEqual(from_catalog.returncode, 0, from_catalog.stderr)
        lineage = json.loads(from_catalog.stdout)
        self.assertEqual(self.csv.read_bytes(), archive_csv)
        self.assertEqual(lineage["sha256"], archive_lineage["sha256"])
        self.assertEqual(lineage["sha256"], hashlib.sha256(archive_csv).hexdigest())
        self.assertEqual((archive_lineage["normalized_price_count"], lineage["normalized_price_count"]), (2, 2))
        self.assertEqual((lineage["first_session"], lineage["last_session"], lineage["observations"],
                          lineage["missing_sessions"]), ("2024-01-02", "2024-12-31", 252, 0))
        self.assertEqual(archive_csv.decode().splitlines()[1], "2024-01-02,162.83,165.01,160.00,163.00,1000")

        imported = self.run_module(
            "research_catalog", self.csv, "--instrument", "IBM.XNYS", "--asset-type", "stock",
            "--kind", "fixture", "--source", "Yahoo Finance chart IBM fixture", "--raw-split-free-reviewed",
            env={**self.env, "FERTILE_RESEARCH_ROOT": str(self.research_root)},
        )
        self.assertEqual(imported.returncode, 0, imported.stderr)
        manifest = json.loads(imported.stdout)
        self.assertEqual((manifest["source_sha256"], manifest["observations"]), (lineage["sha256"], 252))

        request = {
            "strategy": "SmaCrossover", "instrument": "IBM.XNYS",
            "start": "2024-01-02T00:00:00Z", "end": "2024-12-31T00:00:00Z",
            "params": {"fast_period": 10, "slow_period": 30, "shares": 1},
        }
        target = self.root / "result.json"
        with patch.dict(os.environ, {"FERTILE_RESEARCH_ROOT": str(self.research_root),
                                    "FERTILE_DATA_ROOT": str(self.root / "unused-data")}):
            execute(request, target)
        summary = json.loads(target.read_text())["research"]
        self.assertEqual(summary["dataset"], manifest)
        self.assertEqual((summary["actual_start"], summary["actual_end"], summary["observations"]),
                         ("2024-01-02", "2024-12-31", 252))
        self.assertEqual(summary["risk_free_rate"], "0")
        windows = summary["windows"]
        self.assertEqual([row["window"] for row in windows], ["50y", "10y", "5y", "longest_sample"])
        for window in windows[:3]:
            self.assertEqual(window["coverage"], "insufficient")
            self.assertIsNone(window["metrics"])
        longest = windows[-1]
        self.assertEqual((longest["coverage"], longest["rf_source"], longest["observations"]), ("full", "rf=0", 223))
        self.assertTrue(Decimal(longest["metrics"]["sharpe"]).is_finite())
        self.assertFalse((self.root / "unused-data").exists())

    def test_ibm_2024_quote_outside_cent_tolerance_fails_without_csv(self):
        archive = self.root / "ibm-2024-off-tick.json"
        ibm_2024_archive(archive, first_open="162.83002000000001")
        result = self.convert("--yahoo-json", archive, "--adjustment-as-of", IBM_2024_END,
                              "--start", IBM_2024_START, "--end", IBM_2024_END)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Source quote open on 2024-01-02", result.stderr)
        self.assertEqual(result.stdout, "")
        self.assertFalse(self.csv.exists())
