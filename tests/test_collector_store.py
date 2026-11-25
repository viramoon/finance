"""Requires the source-built finance venv, just like the collector process."""

from decimal import Decimal
import json
import os
from pathlib import Path
import subprocess
import sys
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import patch

from fertile_finance.collector_store import NS_PER_DAY, open_store
from nautilus_trader.persistence.catalog.parquet import ParquetDataCatalog


class CatalogStorage(unittest.TestCase):
    def test_script_writer_handshake_without_pythonpath(self):
        repo = Path(__file__).resolve().parents[1]
        env = os.environ.copy()
        env.pop("PYTHONPATH", None)
        with TemporaryDirectory() as directory:
            root = Path(directory)
            catalog = root / "catalog"
            state = root / "state"
            row = dict(instrument="SPY.XNYS", ts_ns=0, open="4", high="4", low="4", close="4", volume="1")
            child = subprocess.run(
                [sys.executable, "-u", "python/fertile_finance/collector_store.py",
                 str(catalog), str(state), "1-MINUTE-LAST", "replay"],
                cwd=repo, env=env, input=json.dumps([row]) + "\n",
                capture_output=True, text=True, timeout=20,
            )
            self.assertEqual(child.returncode, 0, child.stderr)
            self.assertEqual([json.loads(line) for line in child.stdout.splitlines()], [{}, {"SPY.XNYS": 0}])
            self.assertEqual([bar.ts_event for bar in ParquetDataCatalog(catalog).bars()], [0])

    def test_replay_catalog_cannot_be_reused_for_ib(self):
        with TemporaryDirectory() as directory:
            root = Path(directory)
            with open_store(root / "catalog", root / "state", "1-MINUTE-LAST", "replay"):
                pass
            with self.assertRaisesRegex(ValueError, "another source"):
                with open_store(root / "catalog", root / "state", "1-MINUTE-LAST", "ib"):
                    pass
            with self.assertRaisesRegex(ValueError, "without matching provenance"):
                with open_store(root / "other-catalog", root / "state", "1-MINUTE-LAST", "ib"):
                    pass

    def test_catalog_readback_restart_and_exact_prices(self):
        with TemporaryDirectory() as root:
            catalog = Path(root) / "catalog"
            state = Path(root) / "state"
            def row(ts, symbol="SPY.XNYS", close="4.000000000123"):
                return dict(instrument=symbol, open="4", high="4.1", low="4",
                            close=close, volume="103.0123456789", ts_ns=ts)
            bars = [row(NS_PER_DAY - 1), row(NS_PER_DAY), row(NS_PER_DAY + 1, "XYZ.XNYS")]
            with open_store(catalog, state, "1-MINUTE-LAST") as store:
                with self.assertRaises(BlockingIOError), open_store(catalog, state, "1-MINUTE-LAST"):
                    pass
                # Simulate process loss between immutable parquet commits and checkpoint.
                with patch("fertile_finance.collector_store.checkpoint", side_effect=OSError("disk")):
                    with self.assertRaises(OSError):
                        store.append([bars[1], bars[0], bars[0], bars[2]])
                self.assertEqual((state / "watermarks.json").read_text().strip(), "{}")
            with open_store(catalog, state, "1-MINUTE-LAST") as store:
                self.assertEqual(store.marks, {"SPY.XNYS": NS_PER_DAY, "XYZ.XNYS": NS_PER_DAY + 1})
                store.append(bars)
                with self.assertRaises(ValueError):
                    store.append([row(NS_PER_DAY + 100, close=4.0)])
            self.assertEqual(len(list((catalog / "data/bar").glob("*/*.parquet"))), 3)
            restored = ParquetDataCatalog(catalog).bars()
            self.assertEqual(sorted(b.ts_event for b in restored), sorted(r["ts_ns"] for r in bars))
            for bar in restored:
                self.assertEqual(bar.close.as_decimal(), Decimal("4.000000000123"))
                self.assertEqual(bar.volume.as_decimal(), Decimal("103.0123456789"))


if __name__ == "__main__":
    unittest.main()
