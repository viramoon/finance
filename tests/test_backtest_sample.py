import json
from pathlib import Path
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
