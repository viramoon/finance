import json
import os
from pathlib import Path
from shutil import copyfile
import subprocess
import sys
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import patch
from fertile_finance.backtest_sample import atomic_write, timestamp


class AtomicWriteTests(unittest.TestCase):
    def test_replace_and_failed_replace_preserves_previous(self):
        with TemporaryDirectory() as directory:
            target = Path(directory) / "state/latest.json"
            atomic_write(target, {"pnl": "12345678901234567890.12345678"})
            original = target.read_bytes()
            with patch("fertile_finance.backtest_sample.os.replace", side_effect=OSError("rename failed")):
                with self.assertRaises(OSError):
                    atomic_write(target, {"pnl": "0.00"})
            self.assertEqual(target.read_bytes(), original)
            self.assertEqual(list(target.parent.glob("*.tmp")), [])
            atomic_write(target, {"pnl": "-0.01"})
            self.assertEqual(json.loads(target.read_text())["pnl"], "-0.01")

    def test_timestamp_keeps_nanoseconds(self):
        self.assertEqual(timestamp(1704153600123456789), "2024-01-02T00:00:00.123456789Z")


class SmokeTests(unittest.TestCase):
    def test_smoke_rejects_pre_epoch_before_adding_bars(self):
        source = Path(__file__).resolve().parents[1]
        example = Path("vendor/nautilus_trader/examples/backtest/example_01_load_bars_from_custom_csv")
        csv = example / "6EH4.XCME_1min_bars.csv"
        with TemporaryDirectory() as directory:
            root = Path(directory)
            repo = root / "repo"
            build = root / "build"
            build.mkdir()
            (build / "venv").symlink_to(sys.prefix, target_is_directory=True)
            for path in (
                "scripts/smoke.sh", "scripts/verify-source.py", "vendor/nautilus_trader/pyproject.toml",
                "python/fertile_finance/__init__.py", "python/fertile_finance/backtest_sample.py",
                "python/fertile_finance/epoch.py", example / "run_example.py", example / "strategy.py", csv,
            ):
                target = repo / path
                target.parent.mkdir(parents=True, exist_ok=True)
                copyfile(source / path, target)
            env = dict(os.environ, FINANCE_BUILD_ROOT=str(build), FERTILE_DATA_ROOT=str(root / "data"),
                       PYTHONDONTWRITEBYTECODE="1")

            def smoke():
                return subprocess.run(
                    ["bash", str(repo / "scripts/smoke.sh")], cwd=root, env=env, text=True,
                    stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=60,
                )

            valid = smoke()
            self.assertEqual(valid.returncode, 0, valid.stdout)
            self.assertIn("PASS upstream backtest example: 9 bars", valid.stdout)
            csv_path = repo / csv
            unchanged = csv_path.read_text()
            _, first_bar = unchanged.split("\n", 1)
            first_time, _ = first_bar.split(";", 1)
            csv_path.write_text(unchanged.replace(first_time, "1969-12-31 00:00:00", 1))

            rejected = smoke()
            self.assertNotEqual(rejected.returncode, 0, rejected.stdout)
            self.assertIn("expected Unix nanoseconds at or after the epoch", rejected.stdout)
            self.assertNotRegex(rejected.stdout, r"Added \d+ .* Bar elements")
            self.assertNotIn("OutOfBoundsDatetime", rejected.stdout)
            self.assertNotIn("PASS upstream backtest example", rejected.stdout)
            print("Smoke fixture: 9 bars processed; pre-epoch input rejected before engine.add_data()")
