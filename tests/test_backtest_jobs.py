from pathlib import Path
from tempfile import TemporaryDirectory
import json
import unittest
from unittest.mock import patch

from fertile_finance.backtest_job import execute
from fertile_finance.worker import claim, open_queue, recover

REQUEST = dict(
    strategy="DemoStrategy",
    instrument="6EH4.XCME",
    start="2024-01-01T23:02:00Z",
    end="2024-01-01T23:06:00Z",
    params={},
)


class JobTests(unittest.TestCase):
    def test_crash_recovery_is_bounded(self):
        with TemporaryDirectory() as tmp, open_queue(Path(tmp)) as db, \
                patch("fertile_finance.worker.clock_ms", return_value=0) as clock:
            directory = Path(tmp)
            with db:
                db.execute("INSERT INTO jobs(idem,request) VALUES ('one','{}')")
            row = claim(db, directory, 1)
            self.assertEqual(row.attempt, 1)
            row.guard.close()
            clock.return_value += 1_000
            recover(db, directory)
            row = claim(db, directory, 1)
            self.assertEqual(row.attempt, 2)
            row.guard.close()
            clock.return_value += 1_000
            recover(db, directory)
            self.assertIsNone(claim(db, directory, 1))
            self.assertEqual(
                db.execute("SELECT status,attempt,error FROM jobs").fetchone(),
                ("failed", 2, "worker_lost_twice"),
            )

    def test_actual_source_engine_runs_requested_window(self):
        with TemporaryDirectory() as tmp:
            target = Path(tmp) / "summary.json"
            execute(REQUEST, target)
            result = json.loads(target.read_text())
            self.assertEqual(result["instrument"], REQUEST["instrument"])
            self.assertEqual(result["start"], "2024-01-01T23:02:00.000000000Z")
            self.assertEqual(result["end"], "2024-01-01T23:06:00.000000000Z")
            self.assertIsInstance(result["pnl"], str)
            self.assertIsInstance(result["total_orders"], int)
            self.assertEqual(result["currency"], "USD")
