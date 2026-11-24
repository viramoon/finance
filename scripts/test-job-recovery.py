"""Run real HTTP submissions and a worker SIGKILL drill on disposable state."""

import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import tempfile
import time

REPO = Path(__file__).resolve().parents[1]
BODY = dict(
    strategy="DemoStrategy",
    instrument="6EH4.XCME",
    start="2024-01-01T23:02:00Z",
    end="2024-01-01T23:06:00Z",
    params={},
)


def drill(directory):
    port = int(os.environ.get("FINANCE_DRILL_PORT", "0"))
    if not port:
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
    env = dict(
        os.environ,
        FERTILE_DATA_ROOT=str(directory),
        FINANCE_API_BIND_HOST="127.0.0.1",
        FINANCE_API_PORT=str(port),
        FINANCE_CONFIG_DIR=str(REPO / "config"),
    )
    children = []
    base = f"http://127.0.0.1:{port}"

    def spawn(command):
        p = subprocess.Popen(command, cwd=REPO, env=env)
        children.append(p)
        return p

    def curl(path, *args):
        result = subprocess.check_output(
            [
                "curl",
                "--noproxy",
                "*",
                "-sS",
                "--max-time",
                "3",
                "-w",
                "\n%{http_code}",
                *args,
                base + path,
            ],
            text=True,
        )
        payload, code = result.rsplit("\n", 1)
        return int(code), json.loads(payload)

    def wait_for(call, done, timeout=120):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            value = call()
            if done(value):
                return value
            time.sleep(0.05)
        raise AssertionError("deadline elapsed")

    try:
        server = spawn([os.environ["FINANCE_API_BIN"]])

        def live():
            assert server.poll() is None
            try:
                return curl("/healthz/live")[0]
            except subprocess.CalledProcessError:
                return None

        wait_for(live, lambda status: status == 200, 10)
        worker_cmd = [str(REPO / "scripts/finance-backtest-worker")]
        worker = spawn(worker_cmd)
        path = "/api/v1/backtests"

        def post(key):
            args = (
                "-X",
                "POST",
                "-H",
                "Content-Type: application/json",
                "-H",
                "Idempotency-Key: " + key,
                "-d",
                json.dumps(BODY),
            )
            status, data = curl(path, *args)
            print("curl", *args, base + path, "->", status, data, flush=True)
            assert status == 202
            return data["id"]

        first = post("k1")
        assert first == post("k1")

        def get(id):
            return curl(f"{path}/{id}")[1]

        result = wait_for(
            lambda: get(first), lambda job: job["status"] not in ("running", "queued")
        )
        assert result["status"] == "succeeded" and isinstance(
            result["summary"]["pnl"], str
        ), result
        print("GET job", first, json.dumps(result), flush=True)
        second = post("kill-drill")
        wait_for(lambda: get(second), lambda job: job["status"] == "running")

        # Verify the running claim has actually started its engine subprocess.
        def live_tree(pid):
            # /proc/PID/task/TID/children is optional in the Linux kernel.
            # ppid in /proc/PID/stat is available on ordinary Linux systems.
            tree = {}
            for stat in Path("/proc").glob("[0-9]*/stat"):
                try:
                    ppid = int(stat.read_text().rsplit(")", 1)[1].split()[1])
                    tree.setdefault(ppid, []).append(int(stat.parent.name))
                except (FileNotFoundError, ProcessLookupError, PermissionError):
                    continue
            children = tree.get(pid, []).copy()
            for child in children:
                children.extend(tree.get(child, []))
            return children

        def engine_launched(tree):
            for pid in tree:
                try:
                    args = Path(f"/proc/{pid}/cmdline").read_bytes().split(b"\x00")
                except FileNotFoundError:
                    continue
                if b"python" in args[0] and b"fertile_finance.backtest_job" in args:
                    return True
            return False

        descendants = wait_for(lambda: live_tree(worker.pid), engine_launched)
        start = time.monotonic()
        os.kill(worker.pid, signal.SIGKILL)
        worker.wait()

        def dead(pid):
            try:
                return (
                    Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()[0]
                    == "Z"
                )
            except FileNotFoundError:
                return True

        wait_for(lambda: all(dead(pid) for pid in descendants), bool, 5)
        print(
            "orphan check: all execution descendants stopped", descendants, flush=True
        )
        print(
            "kill -9 worker pid", worker.pid, "during running, restarting", flush=True
        )
        worker = spawn(worker_cmd)
        wait_for(
            lambda: get(second),
            lambda job: job["status"] == "running" and job["attempt"] == 2,
        )
        worker.terminate()
        third = post("timeout-drill")
        result = wait_for(
            lambda: get(second), lambda job: job["status"] not in ("running", "queued")
        )
        assert result["status"] == "succeeded" and result["attempt"] == 2, result
        print(
            "recovered in",
            round(time.monotonic() - start, 3),
            "seconds",
            json.dumps(result),
            flush=True,
        )
        assert worker.wait(timeout=10) == 0
        assert get(third)["status"] == "queued"
        print("SIGTERM drained active job and left pending job queued", flush=True)
        # A very short cap must enforce a real subprocess deadline.
        worker = spawn([*worker_cmd, "--timeout", "3"])
        descendants = wait_for(lambda: live_tree(worker.pid), engine_launched)
        # Pause the execution group explicitly so even a fast runner tests the deadline.
        os.killpg(descendants[0], signal.SIGSTOP)
        result = wait_for(
            lambda: get(third), lambda job: job["status"] not in ("running", "queued")
        )
        assert result["status"] == "timed_out" and result["error"] == "job_timeout", (
            result
        )
        print("deadline enforced", json.dumps(result), flush=True)
    finally:
        for child in reversed(children):
            if child.poll() is None:
                child.terminate()
                try:
                    child.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait()


if __name__ == "__main__":
    with tempfile.TemporaryDirectory() as tmp:
        drill(Path(tmp))
