"""Lease queued jobs across local workers and fence result commits by attempt."""

import argparse
from contextlib import closing, nullcontext
from dataclasses import dataclass
import fcntl
import json
import os
from pathlib import Path
import re
import resource
import signal
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time
from typing import BinaryIO

from .backtest_job import STRATEGY_JOB_ERRORS, die_with_parent

REPO = Path(__file__).resolve().parents[2]
SCHEMA = REPO / "sql/backtest-queue.sql"
DEFAULT_LEASE_SECONDS = 15
CANCELLED_ERROR = "job_cancelled"
_EXECUTION_ARTIFACT = re.compile(r"(?:lease-([1-9][0-9]{0,18})\.lock|run-([1-9][0-9]{0,18})-([0-2])-.+)")


def open_queue(directory: Path):
    directory.mkdir(exist_ok=True)
    db = sqlite3.connect(
        directory / "jobs.sqlite3", timeout=5, isolation_level="IMMEDIATE"
    )
    db.execute("PRAGMA foreign_keys=ON")
    db.executescript(SCHEMA.read_text())
    return db


def clock_ms():
    return time.time_ns() // 1_000_000


def guard_path(directory, job_id):
    return directory / f"lease-{job_id}.lock"


def open_guard(path, *, create=False):
    def open_local(name, flags):
        flags |= os.O_NOFOLLOW
        if create:
            flags |= os.O_CREAT
        return os.open(name, flags, 0o600)

    return open(path, "rb", opener=open_local)


def recovery_candidates(db, directory):
    now = clock_ms()
    candidates = {job_id for (job_id,) in db.execute(
        """SELECT id FROM jobs LEFT JOIN job_leases ON job_id=id
           WHERE status='running' AND (expires_at_ms IS NULL OR expires_at_ms<=?1)
           UNION ALL
           SELECT job_id FROM job_leases CROSS JOIN jobs ON id=job_id
           WHERE expires_at_ms<=?1 AND status='failed' AND error=?2""",
        (now, CANCELLED_ERROR),
    )}
    # Older cancelled jobs may have left execution files without a lease.
    # Discover them from the files so recovery never scans cancelled history.
    artifact_jobs = {}
    with os.scandir(directory) as entries:
        for entry in entries:
            match = _EXECUTION_ARTIFACT.fullmatch(entry.name)
            if not match:
                continue
            guard_id, scratch_id, attempt = match.groups()
            job_id = int(guard_id or scratch_id)
            if job_id >= 2**63 or job_id in candidates:
                continue
            if guard_id and entry.is_file(follow_symlinks=False):
                artifact_jobs.setdefault(job_id, set()).add(None)
            if scratch_id and entry.is_dir(follow_symlinks=False):
                artifact_jobs.setdefault(job_id, set()).add(int(attempt))
    for job_id, attempts in artifact_jobs.items():
        row = db.execute(
            """SELECT attempt FROM jobs LEFT JOIN job_leases ON job_id=id
               WHERE id=?1 AND status='failed' AND error=?2 AND job_id IS NULL""",
            (job_id, CANCELLED_ERROR),
        ).fetchone()
        if row and (None in attempts or row[0] in attempts):
            candidates.add(job_id)
    return candidates


def recover(db, directory):
    candidates = recovery_candidates(db, directory)
    if not candidates:
        return
    # The shared lifetime lock excludes the old single-worker format. Recheck
    # each candidate inside the write transaction before inspecting its lock.
    with db:
        db.execute("BEGIN IMMEDIATE")
        now = clock_ms()
        for job_id in sorted(candidates):
            row = db.execute(
                """SELECT attempt, status FROM jobs LEFT JOIN job_leases ON job_id=id
                   WHERE id=?1 AND (status='running' OR (status='failed' AND error=?2))
                     AND (expires_at_ms IS NULL OR expires_at_ms<=?3)""",
                (job_id, CANCELLED_ERROR, now),
            ).fetchone()
            if row is None:
                continue
            attempt, status = row
            path = guard_path(directory, job_id)
            try:
                guard = open_guard(path)
            except FileNotFoundError:
                guard = None
            with guard if guard is not None else nullcontext():
                if guard is not None:
                    try:
                        fcntl.flock(guard, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    except BlockingIOError:
                        # A paused worker or surviving execution process can still run.
                        continue
                for scratch in directory.glob(f"run-{job_id}-{attempt}-*"):
                    if scratch.is_dir() and not scratch.is_symlink():
                        shutil.rmtree(scratch)
                path.unlink(missing_ok=True)
                if status == "running":
                    db.execute(
                        """UPDATE jobs SET status=CASE attempt WHEN 1 THEN 'queued' ELSE 'failed' END,
                           error=CASE attempt WHEN 1 THEN NULL ELSE 'worker_lost_twice' END
                           WHERE id=?""",
                        (job_id,),
                    )
                db.execute("DELETE FROM job_leases WHERE job_id=?", (job_id,))


@dataclass
class Claim:
    job_id: int
    request: str
    attempt: int
    guard: BinaryIO


def claim(db, directory, lease_seconds):
    queued = "SELECT id,request,attempt FROM jobs WHERE status='queued' ORDER BY id LIMIT 1"
    if db.execute(queued).fetchone() is None:
        return None
    guard = None
    try:
        with db:
            db.execute("BEGIN IMMEDIATE")
            row = db.execute(queued).fetchone()
            if row is None:
                return None
            job_id, request, previous_attempt = row
            guard = open_guard(guard_path(directory, job_id), create=True)
            fcntl.flock(guard, fcntl.LOCK_EX | fcntl.LOCK_NB)
            db.execute(
                "UPDATE jobs SET status='running', attempt=attempt+1 WHERE id=?",
                (job_id,),
            )
            db.execute(
                # An older worker can requeue a job after rollback while
                # leaving the previous lease row. The queued state and guard
                # are authoritative for this new attempt.
                """INSERT INTO job_leases(job_id,expires_at_ms) VALUES (?,?)
                   ON CONFLICT(job_id) DO UPDATE SET expires_at_ms=excluded.expires_at_ms""",
                (job_id, clock_ms() + lease_seconds * 1_000),
            )
    except BaseException:
        if guard is not None:
            guard.close()
        raise
    return Claim(job_id, request, previous_attempt + 1, guard)


def renew(db, claims, lease_seconds):
    lost = []
    with db:
        db.execute("BEGIN IMMEDIATE")
        expires_at = clock_ms() + lease_seconds * 1_000
        for job in claims:
            if job.guard.closed:
                lost.append(job.job_id)
                continue
            # Expiry allows recovery only once the guard is free. A delayed
            # owner with the same attempt and the guard can still renew.
            update = db.execute(
                """UPDATE job_leases SET expires_at_ms=? WHERE job_id=?
                   AND EXISTS (SELECT 1 FROM jobs WHERE id=job_id AND status='running' AND attempt=?)""",
                (expires_at, job.job_id, job.attempt),
            )
            if not update.rowcount:
                lost.append(job.job_id)
    return lost


@dataclass
class Running:
    process: subprocess.Popen
    result: Path
    scratch: tempfile.TemporaryDirectory
    deadline: float


def launch(root, claim, timeout, memory_mb):
    scratch = tempfile.TemporaryDirectory(
        dir=root, prefix=f"run-{claim.job_id}-{claim.attempt}-"
    )
    work = Path(scratch.name)
    (work / "request.json").write_text(claim.request)
    result = work / "result.json"
    worker_pid = os.getpid()
    guard_fd = claim.guard.fileno()

    def limit_child():
        die_with_parent(worker_pid)
        limit = memory_mb * 1024 * 1024
        resource.setrlimit(resource.RLIMIT_AS, (limit, limit))

    env = dict(
        os.environ,
        FINANCE_WORKER_PID=str(worker_pid),
        FINANCE_JOB_GUARD_FD=str(guard_fd),
        OPENBLAS_NUM_THREADS="1",
        OMP_NUM_THREADS="1",
        RAYON_NUM_THREADS="2",
    )
    with (work / "execution.log").open("w") as log:
        process = subprocess.Popen(
            [
                sys.executable,
                "-m",
                "fertile_finance.backtest_job",
                str(work / "request.json"),
                str(result),
            ],
            cwd=REPO,
            env=env,
            stdout=log,
            stderr=subprocess.STDOUT,
            start_new_session=True,
            preexec_fn=limit_child,
            # The engine also passes this descriptor to its indicator helper.
            pass_fds=(guard_fd,),
        )
    return Running(process, result, scratch, time.monotonic() + timeout)


def cancelled(db, claim):
    return db.execute(
        "SELECT 1 FROM jobs WHERE id=? AND attempt=? AND status='failed' AND error=?",
        (claim.job_id, claim.attempt, CANCELLED_ERROR),
    ).fetchone() is not None


def finish(db, claim, status, summary=None, error=None):
    if claim.guard.closed:
        return False
    with db:
        db.execute("BEGIN IMMEDIATE")
        update = db.execute(
            """UPDATE jobs SET status=?,summary=?,error=? WHERE id=? AND status='running'
               AND attempt=? AND EXISTS (
                   SELECT 1 FROM job_leases WHERE job_id=jobs.id
               )""",
            (status, summary, error, claim.job_id, claim.attempt),
        )
        was_cancelled = not update.rowcount and cancelled(db, claim)
        if update.rowcount or was_cancelled:
            db.execute("DELETE FROM job_leases WHERE job_id=?", (claim.job_id,))
            Path(claim.guard.name).unlink(missing_ok=True)
    outcome = status if update.rowcount else ("cancelled" if was_cancelled else "lease_lost")
    print(f"job={claim.job_id} {outcome}", flush=True)
    return bool(update.rowcount)


def kill_job(process):
    if process.poll() is None:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    process.wait()


def reap(db, jobs):
    for job_id, (row, job) in list(jobs.items()):
        was_cancelled = cancelled(db, row)
        code = job.process.poll()
        if not was_cancelled and code is None and time.monotonic() < job.deadline:
            continue
        del jobs[job_id]
        with row.guard:
            if was_cancelled:
                kill_job(job.process)
                status, summary, error = "failed", None, CANCELLED_ERROR
            else:
                status, summary, error = job_result(job, code)
            job.scratch.cleanup()
            finish(db, row, status, summary, error)


def job_result(job, code):
    if code is None:
        kill_job(job.process)
        return "timed_out", None, "job_timeout"
    if code:
        # The job reads only local market data and configuration. Forward
        # its final diagnostic lines rather than accumulating unbounded logs.
        log = job.result.parent / "execution.log"
        with log.open("rb") as file:
            file.seek(max(0, log.stat().st_size - 2048))
            print(file.read().decode(errors="replace"), file=sys.stderr, flush=True)
        return "failed", None, "engine_failed"
    try:
        result = json.loads(job.result.read_text())
        if not isinstance(result, dict):
            raise ValueError("invalid result")
        research_error = result.get("research_error")
        strategy_error = result.get("strategy_error")
        if research_error is not None and research_error not in {
            "invalid_instrument", "no_data", "invalid_date", "invalid_source", "invalid_bar",
            "missing_sessions", "insufficient_history", "invalid_parameters", "execution_audit_failed",
        }:
            raise ValueError("invalid research error")
        if strategy_error is not None and (research_error is not None or strategy_error not in STRATEGY_JOB_ERRORS):
            raise ValueError("invalid strategy error")
        job_error = research_error or strategy_error
        if job_error is None and not isinstance(result["pnl"], str):
            raise ValueError("invalid pnl")
        if result.get("strategy") == "StrategyBundle" and not all(
            isinstance(result.get(key), dict) for key in ("run_manifest", "strategy_report")
        ):
            raise ValueError("invalid configured backtest report")
    except (OSError, ValueError, KeyError, TypeError):
        return "failed", None, "invalid_result"
    if job_error is not None:
        return "failed", None, job_error
    return "succeeded", json.dumps(result), None


def run_worker(
    root, concurrency, timeout, memory_mb, *, isolated=False, lease_seconds=DEFAULT_LEASE_SECONDS,
):
    directory = root / "finance-state/backtests"
    (root / "finance-state").mkdir(exist_ok=True)
    directory.mkdir(exist_ok=True)
    with (directory / "worker.lock").open("a+") as lock, closing(open_queue(directory)) as db:
        # Current workers share this lock. A previous-version worker takes it
        # exclusively and would otherwise reclaim every running job it sees.
        fcntl.flock(lock, fcntl.LOCK_SH | fcntl.LOCK_NB)
        recover(db, directory)
        jobs = {}
        stopping = False

        def stop(*_):
            nonlocal stopping
            stopping = True

        signal.signal(signal.SIGTERM, stop)
        signal.signal(signal.SIGINT, stop)
        print("finance-backtest-worker ready", flush=True)
        heartbeat_interval = lease_seconds / 3
        recovery_interval = min(1, heartbeat_interval)
        next_heartbeat = time.monotonic() + heartbeat_interval
        next_recovery = time.monotonic() + recovery_interval
        try:
            while not stopping or (jobs and not isolated):
                if time.monotonic() >= next_heartbeat:
                    if jobs:
                        lost = renew(db, (row for row, _ in jobs.values()), lease_seconds)
                        for job_id in lost:
                            row, job = jobs.pop(job_id)
                            with row.guard:
                                kill_job(job.process)
                                job.scratch.cleanup()
                                if cancelled(db, row):
                                    finish(db, row, "failed", error=CANCELLED_ERROR)
                                else:
                                    print(f"job={job_id} lease_lost", flush=True)
                    next_heartbeat = time.monotonic() + heartbeat_interval
                reap(db, jobs)
                if not stopping and time.monotonic() >= next_recovery:
                    recover(db, directory)
                    next_recovery = time.monotonic() + recovery_interval
                while not stopping and len(jobs) < concurrency:
                    row = claim(db, directory, lease_seconds)
                    if row is None:
                        break
                    try:
                        job = launch(directory, row, timeout, memory_mb)
                    except (OSError, subprocess.SubprocessError) as exc:
                        print(f"job launch failed: {exc}", file=sys.stderr)
                        with row.guard:
                            finish(db, row, "failed", error="launch_failed")
                    else:
                        jobs[row.job_id] = (row, job)
                        print(f"job={row.job_id} attempt={row.attempt} running pid={job.process.pid}", flush=True)
                time.sleep(0.1)
        finally:
            for row, job in jobs.values():
                with row.guard:
                    kill_job(job.process)
                    job.scratch.cleanup()
                    if isolated and stopping:
                        finish(db, row, "failed", error="worker_stopped")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--concurrency", default=os.getenv("FINANCE_WORKER_CONCURRENCY", "1"), type=int
    )
    parser.add_argument(
        "--timeout", default=os.getenv("FINANCE_JOB_TIMEOUT_SECONDS", "600"), type=int
    )
    parser.add_argument(
        "--memory-mb", default=os.getenv("FINANCE_JOB_MEMORY_MB", "8192"), type=int
    )
    parser.add_argument(
        "--lease-seconds",
        default=os.getenv("FINANCE_WORKER_LEASE_SECONDS", str(DEFAULT_LEASE_SECONDS)),
        type=int,
    )
    parser.add_argument(
        "--isolated", action="store_true",
        help="require explicit data roots and stop active jobs on shutdown",
    )
    args = parser.parse_args()
    if args.isolated and not all(
        Path(os.getenv(name, "")).is_absolute()
        for name in ("FERTILE_DATA_ROOT", "FERTILE_RESEARCH_ROOT")
    ):
        parser.error("isolated worker requires explicit absolute data and research roots")
    root = Path(os.getenv("FERTILE_DATA_ROOT", "/home/box/fertile/data"))
    if (
        not root.is_absolute()
        or not 1 <= args.concurrency <= 16
        or not 1 <= args.timeout <= 3600
        or not 1 <= args.lease_seconds <= 3600
        or not 128 <= args.memory_mb <= 65536
    ):
        parser.error(
            "invalid data root, concurrency (1..16), timeout or lease (1..3600 s), or memory cap (128..65536 MiB)"
        )
    run_worker(
        root, args.concurrency, args.timeout, args.memory_mb,
        isolated=args.isolated, lease_seconds=args.lease_seconds,
    )


if __name__ == "__main__":
    main()
