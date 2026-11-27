"""Local process tests. The engine stub emits fixed JSON for generated jobs."""

from contextlib import closing
from dataclasses import replace
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
from tempfile import TemporaryDirectory
import time
import unittest
from unittest.mock import patch

from fertile_finance import worker


ENGINE = """
import json
import os
from pathlib import Path
import sys
import time

request, result = map(Path, sys.argv[1:])
job = json.loads(request.read_text())['id']
queue = request.parent.parent
guard = queue / f'lease-{job}.lock'
descriptors = (os.readlink(fd) for fd in Path('/proc/self/fd').iterdir() if fd.is_symlink())
assert str(guard) in descriptors, 'engine did not inherit its execution lock'
with (queue / 'fixture-executions.jsonl').open('a') as audit:
    audit.write(json.dumps([job, os.getppid(), os.getpid()]) + '\\n')
while not (queue / 'fixture-gate').exists():
    time.sleep(0.01)
result.write_text(json.dumps({'pnl': '0'}))
"""


INDICATOR_ENGINE = """
from contextlib import closing
import os
from fertile_finance import __path__ as package_paths

package_paths.append(os.environ["FINANCE_TEST_INDICATOR_SOURCE"])
from fertile_finance.strategy_indicators import RustIndicatorEvaluator

with closing(RustIndicatorEvaluator()) as evaluator:
    evaluator._request({"action": "start", "series": 0, "id": "indicator.sma@1.0.0",
                        "params": {"period": 2}, "output": "value"})
"""

INDICATOR_HELPER = """
import json
import os
from pathlib import Path
import sys
import time

assert sys.argv[1:] == ["evaluate-indicators"]
assert json.loads(sys.stdin.readline())["action"] == "start"
root = Path(__file__).parent
ready = root / "helper-ready"
pending = ready.with_suffix(".tmp")
pending.write_text(json.dumps([os.getpid(), os.getppid()]))
pending.replace(ready)
# Deliberately outlive the engine without depending on pipe EOF or parent-death signals.
while not (root / "helper-release").exists():
    time.sleep(0.01)
"""


class WorkerLeaseTests(unittest.TestCase):
    def setUp(self):
        root = self.enterContext(TemporaryDirectory(dir="/tmp"))
        self.root = Path(root)
        self.directory = self.root / "finance-state" / "backtests"
        self.directory.parent.mkdir()
        self.db = self.enterContext(closing(worker.open_queue(self.directory)))
        self.log_paths = []

    def insert_jobs(self, count):
        with self.db:
            self.db.executemany("INSERT INTO jobs(idem,request) VALUES (?,?)", (
                (f"fixture-{job_id}", json.dumps({"id": job_id}))
                for job_id in range(1, count + 1)
            ))

    def test_renewed_lease_and_stale_owner_fencing(self):
        self.insert_jobs(1)
        with closing(worker.open_queue(self.directory)) as other, patch.object(worker, "clock_ms") as clock:
            clock.return_value = 0
            first = worker.claim(self.db, self.directory, 1)
            self.addCleanup(first.guard.close)
            clock.return_value = 500
            self.assertEqual(worker.renew(self.db, [first], 1), [])
            clock.return_value = 1_000
            worker.recover(other, self.directory)
            self.assertIsNone(worker.claim(other, self.directory, 1))
            self.assertEqual(other.execute("SELECT expires_at_ms FROM job_leases").fetchone(), (1_500,))

            first.guard.close()
            clock.return_value = 1_500
            worker.recover(other, self.directory)
            second = worker.claim(other, self.directory, 1)
            self.addCleanup(second.guard.close)
            self.assertEqual(second.attempt, 2)
            self.assertEqual(worker.renew(self.db, [first], 1), [first.job_id])
            self.assertFalse(worker.finish(self.db, first, "succeeded", summary='{"pnl":"1"}'))
            stale_attempt = replace(first, guard=second.guard)
            self.assertEqual(worker.renew(self.db, [stale_attempt], 1), [first.job_id])
            self.assertFalse(worker.finish(self.db, stale_attempt, "succeeded", summary='{"pnl":"1"}'))
            self.assertTrue(worker.finish(other, second, "succeeded", summary='{"pnl":"2"}'))
            self.assertEqual(self.db.execute("SELECT status,summary FROM jobs").fetchone(),
                             ("succeeded", '{"pnl":"2"}'))
            self.assertEqual(self.db.execute("SELECT count(*) FROM job_leases").fetchone(), (0,))

    def test_held_guard_allows_late_renewal_and_completion(self):
        self.insert_jobs(1)
        with closing(worker.open_queue(self.directory)) as other, patch.object(worker, "clock_ms") as clock:
            clock.return_value = 0
            row = worker.claim(self.db, self.directory, 1)
            self.addCleanup(row.guard.close)

            clock.return_value = 1_100
            worker.recover(other, self.directory)
            self.assertIsNone(worker.claim(other, self.directory, 1))
            self.assertEqual(worker.renew(self.db, [row], 1), [])
            self.assertEqual(other.execute("SELECT expires_at_ms FROM job_leases").fetchone(), (2_100,))

            clock.return_value = 2_200
            worker.recover(other, self.directory)
            self.assertIsNone(worker.claim(other, self.directory, 1))
            self.assertTrue(worker.finish(self.db, row, "succeeded", summary='{"pnl":"0"}'))
            self.assertEqual(other.execute("SELECT status,attempt FROM jobs").fetchone(), ("succeeded", 1))

    def test_expired_lease_cannot_overlap_inherited_engine_guard(self):
        self.insert_jobs(1)
        with closing(worker.open_queue(self.directory)) as other, patch.object(worker, "clock_ms") as clock:
            clock.return_value = 0
            first = worker.claim(self.db, self.directory, 1)
            self.addCleanup(first.guard.close)
            self.assertEqual(first.attempt, 1)
            engine = subprocess.Popen([sys.executable, "-c", "import signal; signal.pause()"],
                                      pass_fds=(first.guard.fileno(),))
            try:
                first.guard.close()
                clock.return_value = 1_000
                worker.recover(other, self.directory)
                self.assertIsNone(worker.claim(other, self.directory, 1))
                self.assertEqual(other.execute("SELECT status,attempt FROM jobs").fetchone(), ("running", 1))
            finally:
                engine.kill()
                engine.wait(timeout=5)
            worker.recover(other, self.directory)
            second = worker.claim(other, self.directory, 1)
            self.addCleanup(second.guard.close)
            self.assertEqual(second.attempt, 2)

    def test_expired_lease_waits_for_indicator_helper_after_engine_kill(self):
        self.insert_jobs(1)
        self.prepare_engine(INDICATOR_ENGINE)
        helper = self.root / "indicator-helper"
        helper.write_text(f"#!{sys.executable}\n{INDICATOR_HELPER}")
        helper.chmod(0o700)
        ready = self.root / "helper-ready"
        release = self.root / "helper-release"

        def helper_exited(pid):
            try:
                return Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()[0] == "Z"
            except FileNotFoundError:
                return True

        def release_helper():
            release.touch()
            if ready.exists():
                pid, _ = json.loads(ready.read_text())
                self.wait_for(lambda: helper_exited(pid))

        env = {
            "FINANCE_STRATEGY_BIN": str(helper),
            "FINANCE_TEST_INDICATOR_SOURCE": str(Path(worker.__file__).parent),
        }
        with closing(worker.open_queue(self.directory)) as other, patch.object(worker, "clock_ms") as clock:
            clock.return_value = 0
            first = worker.claim(self.db, self.directory, 1)
            self.addCleanup(first.guard.close)
            with patch.object(worker, "REPO", self.engine_root), patch.dict(os.environ, env):
                engine = worker.launch(self.directory, first, timeout=10, memory_mb=8192)
            self.addCleanup(engine.scratch.cleanup)
            self.addCleanup(worker.kill_job, engine.process)
            self.addCleanup(release_helper)

            self.wait_for(ready.is_file)
            helper_pid, engine_pid = json.loads(ready.read_text())
            self.assertEqual(engine_pid, engine.process.pid)
            first.guard.close()
            engine.process.kill()
            self.assertEqual(engine.process.wait(timeout=5), -signal.SIGKILL)
            self.assertFalse(helper_exited(helper_pid))

            clock.return_value = 1_000
            worker.recover(other, self.directory)
            self.assertEqual(other.execute("SELECT status,attempt FROM jobs").fetchone(), ("running", 1))
            self.assertIsNone(worker.claim(other, self.directory, 1))

            release_helper()
            worker.recover(other, self.directory)
            second = worker.claim(other, self.directory, 1)
            self.assertIsNotNone(second)
            self.addCleanup(second.guard.close)
            self.assertEqual(second.attempt, 2)

    def test_migrates_existing_queue_and_recovers_legacy_running_rows(self):
        self.insert_jobs(3)
        with self.db:
            self.db.execute("DROP TABLE job_leases")
            self.db.execute("UPDATE jobs SET status='running',attempt=id WHERE id IN (1,2)")
            self.db.execute("UPDATE jobs SET status='succeeded',summary='{}',attempt=1 WHERE id=3")
        for attempt in (1, 2):
            (self.directory / f"run-{attempt}-{attempt}-interrupted").mkdir()
        with closing(worker.open_queue(self.directory)) as reopened:
            worker.recover(reopened, self.directory)
            self.assertEqual(reopened.execute("SELECT status,attempt,summary,error FROM jobs ORDER BY id").fetchall(), [
                ("queued", 1, None, None), ("failed", 2, None, "worker_lost_twice"), ("succeeded", 1, "{}", None),
            ])
            self.assertEqual(list(self.directory.glob("run-*")), [])

    def test_queued_job_replaces_lease_left_by_rollback(self):
        self.insert_jobs(1)
        with self.db:
            self.db.execute("UPDATE jobs SET attempt=1 WHERE id=1")
            self.db.execute("INSERT INTO job_leases(job_id,expires_at_ms) VALUES (1,9000)")
        with patch.object(worker, "clock_ms", return_value=0):
            row = worker.claim(self.db, self.directory, 1)
        self.addCleanup(row.guard.close)
        self.assertEqual(row.attempt, 2)
        self.assertEqual(self.db.execute("SELECT expires_at_ms FROM job_leases").fetchone(), (1_000,))

    def prepare_engine(self, script=ENGINE):
        self.engine_root = self.root / "engine"
        package = self.engine_root / "fertile_finance"
        package.mkdir(parents=True)
        (package / "__init__.py").touch()
        (package / "backtest_job.py").write_text(script)
        self.audit = self.directory / "fixture-executions.jsonl"
        self.gate = self.directory / "fixture-gate"

    def rows(self):
        if not self.audit.exists():
            return []
        return [json.loads(line) for line in self.audit.read_text().splitlines()]

    def wait_for(self, predicate):
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            if predicate():
                return
            time.sleep(0.02)
        self.fail("worker condition timed out\n" + "\n".join(path.read_text() for path in self.log_paths))

    def start_worker(self):
        log = self.root / f"worker-{len(self.log_paths)}.log"
        self.log_paths.append(log)
        with log.open("w") as stream:
            process = subprocess.Popen(
                [sys.executable, "-c", (
                    "from pathlib import Path; import sys; from fertile_finance import worker; "
                    "worker.REPO = Path(sys.argv.pop(1)); worker.main()"
                ), str(self.engine_root), "--concurrency", "1", "--timeout", "10", "--lease-seconds", "2"],
                env={"FERTILE_DATA_ROOT": str(self.root), "PYTHONPATH": str(worker.REPO / "python")},
                stdout=stream, stderr=subprocess.STDOUT,
            )

        def stop():
            # Failed assertions can leave a worker stopped or a stub at the gate.
            self.gate.touch()
            process.send_signal(signal.SIGCONT)
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)

        self.addCleanup(stop)
        self.wait_for(lambda: log.read_text().startswith("finance-backtest-worker ready\n"))
        return process

    def test_two_workers_execute_each_queued_job_once(self):
        self.insert_jobs(6)
        self.prepare_engine()
        children = [self.start_worker(), self.start_worker()]

        self.wait_for(lambda: len(self.rows()) >= 2)
        self.assertEqual({parent for _, parent, _ in self.rows()}, {child.pid for child in children})
        # Both current executions keep their claims across a missed heartbeat.
        with self.db:
            self.db.execute("UPDATE job_leases SET expires_at_ms=0")
        self.gate.touch()
        self.wait_for(lambda: self.db.execute("SELECT count(*) FROM jobs WHERE status='succeeded'").fetchone() == (6,))
        self.assertCountEqual([job_id for job_id, _, _ in self.rows()], range(1, 7))
        self.assertEqual(self.db.execute("SELECT DISTINCT attempt FROM jobs").fetchall(), [(1,)])
        self.assertEqual(self.db.execute("SELECT count(*) FROM job_leases").fetchone(), (0,))
        self.assertEqual(list(self.directory.glob("lease-*.lock")), [])

    def test_expired_lease_is_recovered_by_other_worker_after_owner_kill(self):
        self.insert_jobs(1)
        self.prepare_engine()
        holder = self.start_worker()
        self.wait_for(lambda: len(self.rows()) == 1)
        _, _, original_engine = self.rows()[0]

        def process_state(pid):
            try:
                return Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()[0]
            except FileNotFoundError:
                return None

        holder.send_signal(signal.SIGSTOP)
        self.wait_for(lambda: process_state(holder.pid) == "T")
        with self.db:
            self.db.execute("UPDATE job_leases SET expires_at_ms=0")
        follower = self.start_worker()
        self.assertEqual(self.db.execute("SELECT status,attempt FROM jobs").fetchone(), ("running", 1))

        holder.kill()
        holder.wait(timeout=5)
        self.wait_for(lambda: len(self.rows()) == 2)
        self.assertIn(process_state(original_engine), ("Z", None))
        self.assertEqual([parent for _, parent, _ in self.rows()], [holder.pid, follower.pid])
        self.gate.touch()
        self.wait_for(lambda: self.db.execute("SELECT status,attempt FROM jobs").fetchone() == ("succeeded", 2))
        self.assertEqual(self.db.execute("SELECT count(*) FROM job_leases").fetchone(), (0,))
        self.assertEqual(list(self.directory.glob("run-*")), [])

    def test_running_cancellation_stops_engine_and_releases_lease(self):
        self.insert_jobs(2)
        self.prepare_engine()
        self.start_worker()
        self.wait_for(lambda: len(self.rows()) == 1)
        _, _, engine = self.rows()[0]
        with self.db:
            self.db.execute(
                "UPDATE jobs SET status='failed',error=? WHERE id=1",
                (worker.CANCELLED_ERROR,),
            )
        self.wait_for(lambda: len(self.rows()) == 2)
        self.assertFalse(Path(f"/proc/{engine}").exists())
        self.assertFalse(worker.guard_path(self.directory, 1).exists())
        self.assertEqual(list(self.directory.glob("run-1-*")), [])
        self.gate.touch()
        self.wait_for(lambda: self.db.execute("SELECT status FROM jobs WHERE id=2").fetchone() == ("succeeded",))
        self.assertEqual(
            self.db.execute("SELECT status,attempt,summary,error FROM jobs WHERE id=1").fetchone(),
            ("failed", 1, None, worker.CANCELLED_ERROR),
        )
        self.assertEqual(self.db.execute("SELECT count(*) FROM job_leases").fetchone(), (0,))

    def test_cancellation_fences_completion_and_recovers_abandoned_scratch(self):
        self.insert_jobs(2)
        with closing(worker.open_queue(self.directory)) as other, patch.object(worker, "clock_ms", return_value=0) as clock:
            first = worker.claim(self.db, self.directory, 1)
            second = worker.claim(self.db, self.directory, 1)
            self.addCleanup(first.guard.close)
            self.addCleanup(second.guard.close)
            scratch = self.directory / "run-2-1-synthetic"
            scratch.mkdir()
            with other:
                other.execute("UPDATE jobs SET status='failed',error=?", (worker.CANCELLED_ERROR,))

            self.assertFalse(worker.finish(self.db, first, "succeeded", summary='{"pnl":"1"}'))
            clock.return_value = 1000
            worker.recover(other, self.directory)
            self.assertTrue(scratch.is_dir())
            second.guard.close()
            worker.recover(other, self.directory)
            self.assertFalse(scratch.exists())
            self.assertIsNone(worker.claim(self.db, self.directory, 1))
            self.assertEqual(other.execute("SELECT status,summary,error FROM jobs ORDER BY id").fetchall(),
                             [("failed", None, worker.CANCELLED_ERROR)] * 2)
            self.assertEqual(other.execute("SELECT count(*) FROM job_leases").fetchone(), (0,))


if __name__ == "__main__":
    unittest.main()
