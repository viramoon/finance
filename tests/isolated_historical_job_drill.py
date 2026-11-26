"""Run the isolated CLI against the real HTTP API on disposable loopback ports."""

from contextlib import closing
import json
import os
from pathlib import Path
import re
import socket
import sqlite3
import subprocess
import sys
import tempfile
import unittest

from fertile_finance.isolated_historical_job import available_port
from test_isolated_historical_job import assert_window_contract, command_args, prepare_snapshot


class IsolatedHistoricalJobHttpTests(unittest.TestCase):
    def setUp(self):
        scratch = tempfile.TemporaryDirectory(dir="/tmp")
        self.addCleanup(scratch.cleanup)
        self.root = Path(scratch.name)
        self.data = self.root / "data"
        self.data.mkdir()
        self.research, self.manifest = prepare_snapshot(self.root)
        self.queue = self.data / "finance-state/backtests/jobs.sqlite3"

    def run_cli(self, *extra):
        return subprocess.run(
            [sys.executable, "-m", "fertile_finance.isolated_historical_job",
             *command_args(os.environ["FINANCE_API_BIN"], self.data, self.research, "--job-timeout", "30", *extra)],
            env={"PYTHONPATH": str(Path(__file__).resolve().parents[1] / "python"),
                 "PYTHONDONTWRITEBYTECODE": "1",
                 "FERTILE_DATA_ROOT": str(self.root / "unpassed-data"),
                 "FERTILE_RESEARCH_ROOT": str(self.root / "unpassed-research"),
                 "FINANCE_CONFIG_DIR": str(self.root / "unpassed-config")},
            capture_output=True, text=True, timeout=60,
        )

    def assert_released(self, port):
        with socket.socket() as sock:
            sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            sock.bind(("127.0.0.1", port))

    def test_http_success_releases_explicit_port_and_persists_four_windows(self):
        port = available_port()
        result = self.run_cli("--port", str(port))
        self.assertEqual(result.returncode, 0, result.stderr)
        job = json.loads(result.stdout)
        assert_window_contract(self, job, self.manifest["source_sha256"])
        with closing(sqlite3.connect(self.queue)) as db:
            self.assertEqual(db.execute("SELECT status,attempt FROM jobs").fetchall(), [("succeeded", 1)])
        self.assertFalse((self.root / "unpassed-data").exists())
        self.assertFalse((self.root / "unpassed-research").exists())
        self.assert_released(port)

    def test_rejected_http_job_releases_automatic_port(self):
        result = self.run_cli("--instrument", "ABSENT.XNYS")
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("HTTP 422", result.stderr)
        match = re.search(r"127\.0\.0\.1:(\d+)", result.stderr)
        self.assertIsNotNone(match, result.stderr)
        port = int(match[1])
        self.assertGreaterEqual(port, 49152)
        self.assert_released(port)

    def test_occupied_port_never_receives_requests(self):
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", available_port()))
            listener.listen()
            result = self.run_cli("--port", str(listener.getsockname()[1]))
            self.assertEqual(result.returncode, 1, result.stderr)
            self.assertIn("finance-api exited", result.stderr)
            self.assertFalse(self.queue.exists())
            listener.setblocking(False)
            with self.assertRaises(BlockingIOError):
                listener.accept()


if __name__ == "__main__":
    unittest.main()
