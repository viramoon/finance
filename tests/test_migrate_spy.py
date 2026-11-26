from contextlib import redirect_stderr, redirect_stdout
from datetime import date, datetime, timezone
from decimal import Decimal
from io import StringIO
import json
from pathlib import Path
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import patch

from nautilus_trader.model.data import Bar, BarType
from nautilus_trader.model.objects import Price, Quantity
from nautilus_trader.persistence.catalog.parquet import ParquetDataCatalog

from fertile_finance.historical_catalog import HistoricalCatalog
from fertile_finance.migrate_spy import main


ARCHIVE_FIXTURE = Path(__file__).parent / "fixtures" / "historical" / "yahoo_ibm.json"
BAR_TYPE = BarType.from_str("SPY.ARCA-1-DAY-LAST-EXTERNAL")
DATES = tuple(date.fromisoformat(day) for day in ("2024-01-03", "2024-07-02", "2024-07-03", "2024-11-29"))


def native_timestamp(day, hour):
    epoch = date(1970, 1, 1)
    return ((day - epoch).days * 86400 + hour * 3600) * 10**9


def contents(path):
    return {entry.relative_to(path): entry.read_bytes() for entry in path.rglob("*") if entry.is_file()}


class MigrateSpyTests(unittest.TestCase):
    def setUp(self):
        temporary = TemporaryDirectory(dir="/tmp")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.legacy = self.root / "legacy"
        self.target = self.root / "isolated-target"
        self.archive = self.root / "spy.json"
        self.archive.write_text(
            ARCHIVE_FIXTURE.read_text(encoding="utf-8")
            .replace('"IBM"', '"SPY"').replace('"NYQ"', '"PCX"').replace('"EQUITY"', '"ETF"'),
            encoding="utf-8",
        )

    def write_legacy(self, days=DATES, hours=None):
        hours = (21,) * len(days) if hours is None else hours
        price = Price.from_str("150.00")
        bars = [Bar(BAR_TYPE, price, price, price, price, Quantity.from_int(100), when, when)
                for when in map(native_timestamp, days, hours)]
        self.legacy.mkdir()
        ParquetDataCatalog(self.legacy).write_data(bars)

    def call(self, *args):
        stdout, stderr = StringIO(), StringIO()
        with redirect_stdout(stdout), redirect_stderr(stderr):
            code = main([
                "--source-catalog", str(self.legacy), "--target-data-root", str(self.target),
                "--yahoo-json", str(self.archive), "--adjustment-as-of", "2024-12-03", *args,
            ])
        return code, stdout.getvalue(), stderr.getvalue()

    def test_dry_run_detects_legacy_timestamps_without_creating_or_writing_anything(self):
        self.write_legacy()
        before = contents(self.root)
        paths = sorted(self.root.rglob("*"))
        code, stdout, stderr = self.call("--dry-run")
        self.assertEqual(code, 0, stderr)
        self.assertEqual(contents(self.root), before)
        self.assertEqual(sorted(self.root.rglob("*")), paths)
        report = json.loads(stdout)
        self.assertEqual(report["status"], "would_migrate")
        self.assertEqual(report["source"]["catalog"], str(self.legacy))
        self.assertEqual(report["source"]["instrument"], "SPY.ARCA")
        self.assertEqual(report["source"]["first"], "2024-01-03")
        self.assertEqual(report["source"]["last"], "2024-11-29")
        self.assertEqual(report["source"]["observations"], 4)
        self.assertGreater(report["source"]["missing_sessions"], 0)
        self.assertGreater(report["target"]["missing_sessions"], 0)
        expected = {
            "standard_time": "2024-01-03T21:00:00Z",
            "daylight_time": "2024-07-02T20:00:00Z",
            "early_close": "2024-07-03T17:00:00Z",
        }
        self.assertEqual({kind: row["xnys_close"] for kind, row in report["session_close_samples"].items()}, expected)
        self.assertTrue(all(row["legacy_ts_event"].endswith("T21:00:00Z")
                            for row in report["session_close_samples"].values()))

    def test_formal_migration_publishes_quotes_adjustments_actions_and_xnys_timestamps(self):
        self.write_legacy()
        source_files = contents(self.legacy)
        code, stdout, stderr = self.call()
        self.assertEqual(code, 0, stderr)
        self.assertEqual(contents(self.legacy), source_files)
        report = json.loads(stdout)
        self.assertEqual(report["status"], "migrated")
        target = report["target"]
        self.assertEqual((target["observations"], target["dividends"], target["splits"]), (4, 1, 1))
        self.assertEqual(target["sessions_with_quotes"], 4)
        self.assertEqual(target["series_path"], str(self.target / "catalog/historical/yahoo/SPY.ARCA"))
        found = HistoricalCatalog(self.target).read_yahoo("SPY.ARCA")
        actual = [datetime.fromtimestamp(row.ts_event // 10**9, timezone.utc).isoformat() for row in found.bars]
        self.assertEqual(actual, [
            "2024-01-03T21:00:00+00:00", "2024-07-02T20:00:00+00:00",
            "2024-07-03T17:00:00+00:00", "2024-11-29T18:00:00+00:00",
        ])
        self.assertTrue(all(row.ts_init == row.ts_event for row in found.bars))
        self.assertEqual(found.bars[0].close, Decimal("150.1234567890123456"))
        self.assertEqual(found.bars[0].adjusted_close, Decimal("75.0617283945061728"))
        self.assertEqual(found.bars[0].adjustment_factor, Decimal("0.5"))
        self.assertEqual(found.dividends[0].amount, Decimal("0.212345678901234567"))
        self.assertEqual(found.splits[0].session, date(2024, 7, 3))

        target_files = contents(self.target)
        code, _, stderr = self.call("--dry-run")
        self.assertEqual(code, 1)
        self.assertIn("Target SPY series already exists", stderr)
        self.assertEqual(contents(self.target), target_files)

    def test_single_standard_session_does_not_prove_legacy_format(self):
        self.write_legacy(DATES[:1])
        code, stdout, stderr = self.call("--dry-run")
        self.assertEqual((code, stdout), (1, ""))
        self.assertIn("Insufficient evidence", stderr)
        self.assertIn("daylight_time, early_close", stderr)
        self.assertFalse(self.target.exists())

    def test_rejects_series_that_already_uses_xnys_summer_close(self):
        self.write_legacy(hours=(21, 20, 21, 21))
        code, _, stderr = self.call("--dry-run")
        self.assertEqual(code, 1)
        self.assertIn("2024-07-02 has a ts_event other than 21:00 UTC", stderr)
        self.assertFalse(self.target.exists())

    def test_dry_run_rejects_archive_without_quotes_for_legacy_session(self):
        self.write_legacy()
        source = self.archive.read_text(encoding="utf-8")
        for field in ("77.35", "80.45", "76.20", "79.35"):
            source = source.replace(field, "null")
        self.archive.write_text(source.replace("34567890, 0", "34567890, null"), encoding="utf-8")
        code, _, stderr = self.call("--dry-run")
        self.assertEqual(code, 1)
        self.assertIn("Yahoo archive lacks source OHLCV for 1 legacy sessions; examples: 2024-11-29", stderr)
        self.assertFalse(self.target.exists())

    def test_refuses_target_that_aliases_the_source_catalog(self):
        self.write_legacy()
        before = contents(self.root)
        self.target.mkdir()
        (self.target / "catalog").symlink_to(self.legacy, target_is_directory=True)
        code, _, stderr = self.call()
        self.assertEqual(code, 1)
        self.assertIn("Target catalog path must be a real directory", stderr)
        self.assertEqual(contents(self.root), before)

    def test_dry_run_rejects_target_below_a_regular_file(self):
        self.write_legacy()
        blocked = self.root / "regular-file"
        blocked.write_text("existing input", encoding="utf-8")
        self.target = blocked / "data"
        before = contents(self.root)
        code, stdout, stderr = self.call("--dry-run")
        self.assertEqual((code, stdout), (1, ""))
        self.assertIn(f"Cannot create the target catalog under {blocked}", stderr)
        self.assertEqual(contents(self.root), before)

    def test_failed_import_leaves_new_series_absent_and_can_retry(self):
        self.write_legacy()
        with patch.object(ParquetDataCatalog, "write_data", return_value=None):
            code, _, stderr = self.call()
        self.assertEqual(code, 1)
        self.assertIn("Nautilus write verification failed", stderr)
        self.assertEqual(list((self.target / "catalog/historical/yahoo").iterdir()), [])
        code, _, stderr = self.call()
        self.assertEqual(code, 0, stderr)


if __name__ == "__main__":
    unittest.main()
