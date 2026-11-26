from contextlib import closing, contextmanager, redirect_stderr, redirect_stdout
import csv
from datetime import date
from decimal import Decimal
import io
import json
import os
from pathlib import Path
import signal
import sqlite3
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

from fertile_finance import isolated_historical_job as runner
from fertile_finance.isolated_historical_job import managed_child
from fertile_finance.research_catalog import import_catalog, sessions


def prepare_snapshot(directory):
    source = directory / "quotes.csv"
    research = directory / "research"
    days = sessions(date(2024, 11, 25), date(2024, 12, 2))
    with source.open("w", encoding="utf-8", newline="") as stream:
        writer = csv.writer(stream)
        writer.writerow(("date", "open", "high", "low", "close", "volume"))
        for day, price in zip(days, (100, 100, 110, 120, 120), strict=True):
            writer.writerow((day, price, price, price, price, 100))
    manifest = import_catalog(research, source, instrument="IBM.XNYS", source="Isolated job fixture",
                              kind="fixture", asset_type="stock")
    return research, manifest


def command_args(api_bin, data, research, *extra):
    return [
        "--api-bin", str(api_bin), "--data-root", str(data), "--research-root", str(research),
        "--instrument", "IBM.XNYS", "--start", "2024-11-25", "--end", "2024-12-02",
        "--fast-period", "2", "--slow-period", "3", "--shares", "10", *extra,
    ]


def assert_window_contract(test, job, source_sha256):
    test.assertEqual(job["status"], "succeeded")
    test.assertEqual(job["request"]["strategy"], "SmaCrossover")
    test.assertEqual(job["summary"]["research"]["dataset"]["source_sha256"], source_sha256)
    windows = job["summary"]["research"]["windows"]
    test.assertEqual([row["window"] for row in windows], ["50y", "10y", "5y", "longest_sample"])
    test.assertEqual([row["requested_years"] for row in windows], [50, 10, 5, None])
    for row in windows[:3]:
        test.assertEqual(row["coverage"], "insufficient")
        test.assertIsNone(row["metrics"])
    longest = windows[-1]
    test.assertEqual(longest["coverage"], "full")
    test.assertEqual(longest["rf_source"], "rf=0")
    test.assertEqual(longest["metrics"]["turnover_pct"], "1.2006")
    for value in longest["metrics"].values():
        test.assertIsInstance(value, str)
        test.assertTrue(Decimal(value).is_finite())


SLEEPING_WORKER = """
from pathlib import Path
import os
import subprocess
import sys
import tempfile
import time

from fertile_finance import worker


def launch(root, row, timeout, memory_mb, *, isolated=False):
    scratch = tempfile.TemporaryDirectory(dir=root, prefix='run-test-')
    parent = os.getpid()
    process = subprocess.Popen(
        [sys.executable, '-c', 'import signal; signal.pause()'],
        start_new_session=True, preexec_fn=lambda: worker.die_with_parent(parent),
    )
    print(f'engine={process.pid}', flush=True)
    return worker.Running(process, Path(scratch.name) / 'result.json', scratch, time.monotonic() + timeout)


worker.launch = launch
worker.main()
"""


class IsolatedHistoricalJobTests(unittest.TestCase):
    def setUp(self):
        scratch = tempfile.TemporaryDirectory(dir="/tmp")
        self.addCleanup(scratch.cleanup)
        self.root = Path(scratch.name)
        self.data = self.root / "data"
        self.data.mkdir()
        self.research, self.manifest = prepare_snapshot(self.root)
        self.queue = self.data / "finance-state/backtests/jobs.sqlite3"
        self.children = []

    @contextmanager
    def observe_children(self, command, env, data_root):
        self.assertEqual(env["FERTILE_DATA_ROOT"], str(self.data))
        self.assertEqual(env["FERTILE_RESEARCH_ROOT"], str(self.research))
        self.assertNotIn("HTTP_PROXY", env)
        self.assertNotIn("FINANCE_CONFIG_DIR", env)
        if command[1] == "serve-isolated":
            command = [sys.executable, "-c", "import signal; print('127.0.0.1:52961', flush=True); signal.pause()"]
        with managed_child(command, env, data_root) as (child, log):
            self.children.append(child)
            yield child, log

    def queue_transport(self, _port, method, path, expected, *, payload=None, key=None):
        # The transport boundary is a test double. It hands the real worker the
        # same request and queue schema used by the HTTP handlers.
        with closing(sqlite3.connect(self.queue)) as db:
            if method == "POST":
                self.assertEqual((path, expected), ("/api/v1/backtests", 202))
                self.assertTrue(key)
                with db:
                    job_id = db.execute("INSERT INTO jobs(idem,request) VALUES (?,?)",
                                        (key, json.dumps(payload))).lastrowid
                return {"id": job_id}
            self.assertEqual((method, expected), ("GET", 200))
            job_id = int(path.rsplit("/", 1)[1])
            status, raw_request, raw_summary = db.execute(
                "SELECT status,request,summary FROM jobs WHERE id=?", (job_id,),
            ).fetchone()
        return {"id": job_id, "status": status, "request": json.loads(raw_request),
                "summary": json.loads(raw_summary) if raw_summary else None}

    def run_cli(self, *, transport=None, supervise=None):
        output, errors = io.StringIO(), io.StringIO()
        args = command_args(sys.executable, self.data, self.research, "--port", "52961")
        with patch.object(runner, "managed_child", side_effect=supervise or self.observe_children), \
             patch.object(runner, "request_json", side_effect=transport or self.queue_transport) as request, \
             patch.dict(os.environ, {
                 "FERTILE_DATA_ROOT": str(self.root / "unpassed-data"),
                 "FERTILE_RESEARCH_ROOT": str(self.root / "unpassed-research"),
                 "FINANCE_CONFIG_DIR": str(self.root / "unpassed-config"),
                 "HTTP_PROXY": "http://127.0.0.1:52962",
             }), redirect_stdout(output), redirect_stderr(errors):
            code = runner.main(args)
        for child in self.children:
            self.assertIsNotNone(child.returncode)
            with self.assertRaises(ProcessLookupError):
                os.kill(child.pid, 0)
        for name in ("unpassed-data", "unpassed-research", "unpassed-config"):
            self.assertFalse((self.root / name).exists())
        return code, output.getvalue(), errors.getvalue(), request

    def test_isolated_worker_persists_four_windows_with_decimal_metrics(self):
        code, output, errors, _ = self.run_cli()
        self.assertEqual(code, 0, errors)
        self.assertEqual(len(self.children), 2)
        job = json.loads(output)
        assert_window_contract(self, job, self.manifest["source_sha256"])
        with closing(sqlite3.connect(self.queue)) as db:
            self.assertEqual(db.execute("SELECT status,attempt FROM jobs").fetchall(), [("succeeded", 1)])
        self.assertEqual(list(self.queue.parent.glob("run-*")), [])

    def test_http_error_cleans_both_processes(self):
        def unavailable(*args, **kwargs):
            raise runner.RunnerError("HTTP 422: no_data")

        code, output, errors, _ = self.run_cli(transport=unavailable)
        self.assertEqual(code, 1)
        self.assertEqual(output, "")
        self.assertIn("HTTP 422: no_data", errors)
        self.assertEqual(len(self.children), 2)

    def test_failed_bind_acknowledgment_never_submits(self):
        @contextmanager
        def misconfigured_api(command, env, data_root):
            command = [sys.executable, "-c", "import signal; print('wrong port', flush=True); signal.pause()"]
            with managed_child(command, env, data_root) as (child, log):
                self.children.append(child)
                yield child, log

        code, _, errors, request = self.run_cli(supervise=misconfigured_api)
        self.assertEqual(code, 1)
        self.assertIn("did not acknowledge isolated startup", errors)
        self.assertEqual(len(self.children), 1)
        request.assert_not_called()

    def test_sigterm_cleans_api_worker_and_active_engine(self):
        @contextmanager
        def supervise(command, env, data_root):
            if command[1:3] == ["-m", "fertile_finance.worker"]:
                command = [sys.executable, "-c", SLEEPING_WORKER, *command[3:]]
            with self.observe_children(command, env, data_root) as (child, log):
                if command[1] == "-c":
                    self.worker_log = log
                yield child, log

        def interrupt(*args, **kwargs):
            accepted = self.queue_transport(*args, **kwargs)
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                for entry in os.pread(self.worker_log.fileno(), 512, 0).splitlines(keepends=True):
                    if entry.startswith(b"engine=") and entry.endswith(b"\n"):
                        self.engine_pid = int(entry.split(b"=")[1])
                        os.kill(os.getpid(), signal.SIGTERM)
                        return accepted
                time.sleep(0.01)
            self.fail("isolated worker did not start the engine test double")

        code, output, errors, _ = self.run_cli(transport=interrupt, supervise=supervise)
        self.assertEqual(code, 128 + signal.SIGTERM)
        self.assertEqual(output, "")
        self.assertIn("interrupted by SIGTERM", errors)
        self.assertEqual(len(self.children), 2)
        with self.assertRaises(ProcessLookupError):
            os.kill(self.engine_pid, 0)
        self.assertEqual(list(self.queue.parent.glob("run-*")), [])
        with closing(sqlite3.connect(self.queue)) as db:
            self.assertEqual(db.execute("SELECT status,error FROM jobs").fetchall(), [("failed", "worker_stopped")])

    def test_queue_symlink_or_hardlink_is_rejected_before_spawn(self):
        outside = self.root / "outside-queue"
        outside.mkdir()
        state = self.data / "finance-state"
        state.symlink_to(outside, target_is_directory=True)
        code, _, errors, request = self.run_cli()
        self.assertEqual(code, 1)
        self.assertIn("refusing symlink", errors)
        self.assertEqual(len(self.children), 0)
        request.assert_not_called()
        self.assertEqual(list(outside.iterdir()), [])

        state.unlink()
        self.queue.parent.mkdir(parents=True)
        original = self.root / "outside.sqlite3"
        original.write_bytes(b"unused")
        os.link(original, self.queue)
        code, _, errors, request = self.run_cli()
        self.assertEqual(code, 1)
        self.assertIn("refusing linked file", errors)
        self.assertEqual(len(self.children), 0)
        request.assert_not_called()
        self.assertEqual(original.read_bytes(), b"unused")


if __name__ == "__main__":
    unittest.main()
