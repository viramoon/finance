"""Submit one official historical backtest using a private loopback API and worker."""

import argparse
from contextlib import contextmanager
from datetime import date
import errno
from http.client import HTTPConnection, HTTPException
import json
import os
from pathlib import Path
import secrets
import signal
import socket
import subprocess
import sys
from tempfile import TemporaryFile
import time
from uuid import uuid4

from .backtest_job import die_with_parent

REPO = Path(__file__).resolve().parents[2]
HOST = "127.0.0.1"
JOBS = "/api/v1/backtests"


class RunnerError(RuntimeError):
    pass


class Interrupted(Exception):
    def __init__(self, signum):
        self.signum = signum


def existing_directory(value):
    path = Path(value)
    if not path.is_absolute() or ".." in path.parts:
        raise argparse.ArgumentTypeError("must be an existing absolute directory")
    for component in (*reversed(path.parents), path):
        if component.is_symlink():
            raise argparse.ArgumentTypeError(f"directory path cannot contain a symlink: {component}")
    if not path.is_dir():
        raise argparse.ArgumentTypeError("must be an existing absolute directory")
    return path


def check_queue_paths(data_root):
    state = data_root / "finance-state"
    queue = state / "backtests"
    for directory in (state, queue):
        if directory.is_symlink():
            raise RunnerError(f"refusing symlink in isolated queue path: {directory}")
    for name in ("jobs.sqlite3", "jobs.sqlite3-wal", "jobs.sqlite3-shm", "jobs.sqlite3-journal", "worker.lock"):
        path = queue / name
        if path.is_symlink() or (path.exists() and path.stat().st_nlink != 1):
            raise RunnerError(f"refusing linked file in isolated queue: {path}")


def available_port():
    # Use an unprivileged range away from deployed services, independently of
    # the OS ephemeral range. The API bind remains authoritative.
    for _ in range(32):
        port = 49152 + secrets.randbelow(16384)
        with socket.socket() as probe:
            try:
                probe.bind((HOST, port))
            except OSError as exc:
                if exc.errno != errno.EADDRINUSE:
                    raise
            else:
                return port
    raise RunnerError("no free high port found; pass an available --port")


@contextmanager
def managed_child(command, env, data_root):
    parent = os.getpid()

    def arm_parent_death():
        die_with_parent(parent)

    with TemporaryFile(dir=data_root) as log:
        child = subprocess.Popen(
            command, cwd=REPO, env=env, stdin=subprocess.DEVNULL, stdout=log,
            start_new_session=True, preexec_fn=arm_parent_death,
        )
        try:
            yield child, log
        finally:
            child.terminate()
            try:
                child.wait(timeout=5)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait()


def require_running(child, name):
    code = child.poll()
    if code is not None:
        raise RunnerError(f"{name} exited with status {code}")


def wait_for_startup(child, log, name, expected, check_interrupt):
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        check_interrupt()
        require_running(child, name)
        # pread leaves the shared stdout file position alone.
        first, newline, _ = os.pread(log.fileno(), 512, 0).partition(b"\n")
        if newline:
            if first.decode("utf-8", errors="replace") != expected:
                raise RunnerError(f"{name} did not acknowledge isolated startup")
            return
        time.sleep(0.05)
    raise RunnerError(f"{name} startup timed out")


def request_json(port, method, path, expected, *, payload=None, key=None):
    headers = {"Connection": "close"}
    if key is not None:
        headers.update({"Idempotency-Key": key, "Content-Type": "application/json"})
    connection = HTTPConnection(HOST, port, timeout=5)
    try:
        connection.request(method, path, body=None if payload is None else json.dumps(payload), headers=headers)
        response = connection.getresponse()
        result = json.loads(response.read())
        if response.status != expected:
            raise RunnerError(f"{method} {path} returned HTTP {response.status}: {json.dumps(result)}")
        return result
    finally:
        connection.close()


def run(args, check_interrupt):
    check_queue_paths(args.data_root)
    port = args.port if args.port is not None else available_port()
    # Child config is entirely supplied here. In particular, deployment roots,
    # proxy/database configuration and credentials are not inherited.
    env = {
        "FERTILE_DATA_ROOT": str(args.data_root),
        "FERTILE_RESEARCH_ROOT": str(args.research_root),
        "PYTHONPATH": str(REPO / "python"),
        "PYTHONNOUSERSITE": "1",
        "PYTHONDONTWRITEBYTECODE": "1",
    }
    api = [str(args.api_bin), "serve-isolated", str(args.data_root), str(args.research_root), str(port)]
    worker = [sys.executable, "-m", "fertile_finance.worker", "--isolated", "--timeout", str(args.job_timeout)]
    job_request = {
        "strategy": "SmaCrossover", "instrument": args.instrument,
        "start": f"{args.start.isoformat()}T00:00:00Z", "end": f"{args.end.isoformat()}T00:00:00Z",
        "params": {"fast_period": args.fast_period, "slow_period": args.slow_period, "shares": args.shares},
    }
    check_interrupt()
    with managed_child(api, env, args.data_root) as (server, api_log):
        wait_for_startup(server, api_log, "finance-api", f"{HOST}:{port}", check_interrupt)
        print(f"isolated finance-api listening on {HOST}:{port}", file=sys.stderr, flush=True)
        with managed_child(worker, env, args.data_root) as (worker_process, worker_log):
            wait_for_startup(worker_process, worker_log, "worker", "finance-backtest-worker ready", check_interrupt)
            check_interrupt()
            accepted = request_json(port, "POST", JOBS, 202, payload=job_request, key=uuid4().hex)
            deadline = time.monotonic() + args.job_timeout + 10
            path = f"{JOBS}/{accepted['id']}"
            while time.monotonic() < deadline:
                check_interrupt()
                require_running(server, "finance-api")
                require_running(worker_process, "worker")
                job = request_json(port, "GET", path, 200)
                if job["status"] in {"succeeded", "failed", "timed_out"}:
                    return job
                time.sleep(0.1)
            raise RunnerError(f"timed out waiting for job {accepted['id']}")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--api-bin", type=Path, required=True, help="absolute path to the built finance-api binary")
    parser.add_argument("--data-root", type=existing_directory, required=True, help="independent SQLite data root")
    parser.add_argument("--research-root", type=existing_directory, required=True, help="imported research snapshot root")
    parser.add_argument("--port", type=int, help="available IPv4 loopback port (default: select a free high port)")
    parser.add_argument("--instrument", required=True, help="imported instrument ID, for example IBM.XNYS")
    parser.add_argument("--start", type=date.fromisoformat, required=True, help="inclusive start (YYYY-MM-DD)")
    parser.add_argument("--end", type=date.fromisoformat, required=True, help="inclusive end (YYYY-MM-DD)")
    parser.add_argument("--fast-period", type=int, default=10)
    parser.add_argument("--slow-period", type=int, default=30)
    parser.add_argument("--shares", type=int, default=1)
    parser.add_argument("--job-timeout", type=int, default=600, help="worker timeout in seconds (default: 600)")
    args = parser.parse_args(argv)
    if not args.api_bin.is_absolute() or not args.api_bin.is_file():
        parser.error("--api-bin must be an absolute path to a built finance-api binary")
    if args.data_root.is_relative_to(args.research_root) or args.research_root.is_relative_to(args.data_root):
        parser.error("--data-root and --research-root must not overlap")
    if args.port is not None and not 1 <= args.port <= 65535:
        parser.error("--port must be in 1..65535")
    if not 1 <= args.job_timeout <= 3600:
        parser.error("--job-timeout must be in 1..3600 seconds")

    interrupted = None

    def stop(signum, _frame):
        nonlocal interrupted
        interrupted = signum

    def check_interrupt():
        if interrupted is not None:
            raise Interrupted(interrupted)

    previous = {sig: signal.signal(sig, stop) for sig in (signal.SIGINT, signal.SIGTERM)}
    try:
        job = run(args, check_interrupt)
        check_interrupt()
        print(json.dumps(job, ensure_ascii=False, indent=2))
        return 0 if job["status"] == "succeeded" else 1
    except Interrupted as exc:
        print(f"isolated historical job interrupted by {signal.Signals(exc.signum).name}", file=sys.stderr)
        return 128 + exc.signum
    except (RunnerError, OSError, subprocess.SubprocessError, HTTPException, ValueError, KeyError) as exc:
        print(f"isolated historical job: {exc}", file=sys.stderr)
        return 1
    finally:
        for sig, handler in previous.items():
            signal.signal(sig, handler)


if __name__ == "__main__":
    sys.exit(main())
