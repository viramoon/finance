"""Exclusive worker ownership; SQLite transitions and result commit are atomic."""

import argparse
from dataclasses import dataclass
import fcntl
import json
import os
from pathlib import Path
import resource
import signal
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time

from .backtest_job import die_with_parent

REPO = Path(__file__).resolve().parents[2]
SCHEMA = REPO / "sql/backtest-queue.sql"


def open_queue(directory: Path):
    directory.mkdir(exist_ok=True)
    db = sqlite3.connect(
        directory / "jobs.sqlite3", timeout=5, isolation_level="IMMEDIATE"
    )
    db.executescript(SCHEMA.read_text())
    return db


def recover(db):
    # The lifetime lock ensures no live worker owns these claims.
    with db:
        db.execute("""UPDATE jobs SET status=CASE attempt WHEN 1 THEN 'queued' ELSE 'failed' END,
                      error=CASE attempt WHEN 1 THEN NULL ELSE 'worker_lost_twice' END
                      WHERE status='running'""")


def claim(db):
    with db:
        db.execute("BEGIN IMMEDIATE")
        row = db.execute(
            "SELECT id,request,attempt FROM jobs WHERE status='queued' ORDER BY id LIMIT 1"
        ).fetchone()
        if row:
            db.execute(
                "UPDATE jobs SET status='running', attempt=attempt+1 WHERE id=?",
                (row[0],),
            )
    return row


@dataclass
class Running:
    process: subprocess.Popen
    result: Path
    scratch: tempfile.TemporaryDirectory
    deadline: float


def launch(root, row, timeout, memory_mb):
    job_id, request, attempt = row
    scratch = tempfile.TemporaryDirectory(
        dir=root, prefix=f"run-{job_id}-{attempt + 1}-"
    )
    work = Path(scratch.name)
    (work / "request.json").write_text(request)
    result = work / "result.json"
    # The spawned uv may exec Python directly or remain as its parent.
    worker_pid = os.getpid()

    def limit_child():
        die_with_parent(worker_pid)
        limit = memory_mb * 1024 * 1024
        resource.setrlimit(resource.RLIMIT_AS, (limit, limit))

    env = dict(
        os.environ,
        FINANCE_WORKER_PID=str(worker_pid),
        OPENBLAS_NUM_THREADS="1",
        OMP_NUM_THREADS="1",
        RAYON_NUM_THREADS="2",
        UV_NO_PROGRESS="1",
    )
    with (work / "execution.log").open("w") as log:
        process = subprocess.Popen(
            [
                "uv",
                "run",
                "--no-sync",
                "--active",
                "python",
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
        )
    print(f"job={job_id} attempt={attempt + 1} running pid={process.pid}", flush=True)
    return Running(process, result, scratch, time.monotonic() + timeout)


def finish(db, job_id, status, summary=None, error=None):
    with db:
        db.execute(
            "UPDATE jobs SET status=?,summary=?,error=? WHERE id=? AND status='running'",
            (status, summary, error, job_id),
        )
    print(f"job={job_id} {status}", flush=True)


def reap(db, jobs):
    for job_id, job in list(jobs.items()):
        code = job.process.poll()
        if code is None and time.monotonic() < job.deadline:
            continue
        if code is None:
            try:
                os.killpg(job.process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            job.process.wait()
            finish(db, job_id, "timed_out", error="job_timeout")
        elif code:
            # The job reads only local market data and configuration. Forward
            # its final diagnostic lines rather than accumulating unbounded logs.
            log = job.result.parent / "execution.log"
            with log.open("rb") as file:
                file.seek(max(0, log.stat().st_size - 2048))
                print(file.read().decode(errors="replace"), file=sys.stderr, flush=True)
            finish(db, job_id, "failed", error="engine_failed")
        else:
            try:
                result = json.loads(job.result.read_text())
                research_error = result.get("research_error")
                if research_error is not None and research_error not in {
                    "invalid_instrument", "no_data", "invalid_date", "invalid_source", "invalid_bar",
                    "missing_sessions", "insufficient_history", "invalid_parameters", "execution_audit_failed",
                }:
                    raise ValueError("invalid research error")
                if research_error is None and not isinstance(result["pnl"], str):
                    raise ValueError("invalid pnl")
            except (OSError, ValueError, KeyError, TypeError):
                finish(db, job_id, "failed", error="invalid_result")
            else:
                if research_error is not None:
                    finish(db, job_id, "failed", error=research_error)
                else:
                    finish(db, job_id, "succeeded", json.dumps(result))
        job.scratch.cleanup()
        del jobs[job_id]


def run_worker(root, concurrency, timeout, memory_mb):
    directory = root / "finance-state/backtests"
    (root / "finance-state").mkdir(exist_ok=True)
    directory.mkdir(exist_ok=True)
    with (directory / "worker.lock").open("a+") as lock, open_queue(directory) as db:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        recover(db)
        # Interrupted attempts write only here; their output was never committed.
        for path in directory.glob("run-*-*-*"):
            if path.is_dir() and not path.is_symlink():
                shutil.rmtree(path)
        jobs = {}
        stopping = False

        def stop(*_):
            nonlocal stopping
            stopping = True

        signal.signal(signal.SIGTERM, stop)
        signal.signal(signal.SIGINT, stop)
        print("finance-backtest-worker ready", flush=True)
        while not stopping or jobs:
            reap(db, jobs)
            while not stopping and len(jobs) < concurrency:
                row = claim(db)
                if row is None:
                    break
                try:
                    jobs[row[0]] = launch(directory, row, timeout, memory_mb)
                except (OSError, subprocess.SubprocessError) as exc:
                    print(f"job launch failed: {exc}", file=sys.stderr)
                    finish(db, row[0], "failed", error="launch_failed")
            time.sleep(0.1)


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
    args = parser.parse_args()
    root = Path(os.getenv("FERTILE_DATA_ROOT", "/home/box/fertile/data"))
    if (
        not root.is_absolute()
        or not 1 <= args.concurrency <= 16
        or not 1 <= args.timeout <= 3600
        or not 128 <= args.memory_mb <= 65536
    ):
        parser.error(
            "invalid data root, concurrency (1..16), timeout (1..3600) or memory cap (128..65536 MiB)"
        )
    run_worker(root, args.concurrency, args.timeout, args.memory_mb)


if __name__ == "__main__":
    main()
