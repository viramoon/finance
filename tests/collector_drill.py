"""Run the real collector, terminate it twice, and inspect the resulting Nautilus catalog."""
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

from nautilus_trader.persistence.catalog.parquet import ParquetDataCatalog


def wait_for(check, timeout=30):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            value = check()
        except (ConnectionError, urllib.error.URLError, FileNotFoundError):
            value = False
        if value:
            return value
        time.sleep(.05)
    raise TimeoutError(f"condition not reached in {timeout}s: {check}")


def drill(root):
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    config = root / "collector.yaml"
    step_ms = 250
    config.write_text(f"""bind: 127.0.0.1:{port}
source: replay
instruments: [SPY.ARCA]
bar_spec: 1-SECOND-LAST
max_age_seconds: 3
flush_ms: 1500
shutdown_seconds: 15
history_chunk_seconds: 2
catalog: catalog
replay:
  fixture: config/collector/replay.csv
  interval_ms: {step_ms}
""")
    env = dict(os.environ, FERTILE_DATA_ROOT=str(root), FINANCE_COLLECTOR_CONFIG=str(config),
               FINANCE_PYTHON=sys.executable)
    state = root / "finance-state/collector"

    def http(path):
        req = f"http://127.0.0.1:{port}{path}"
        try:
            response = urllib.request.urlopen(req, timeout=1)
        except urllib.error.HTTPError as err:
            response = err
        with response:
            return response.status, json.load(response)

    def start():
        return subprocess.Popen([os.environ["FINANCE_COLLECTOR_BIN"]], env=env)

    def watermark():
        return json.loads((state / "watermarks.json").read_text())["SPY.ARCA"]

    missing = root / "missing-mount"
    bad_env = dict(env, FERTILE_DATA_ROOT=str(missing))
    attempt = subprocess.run([os.environ["FINANCE_COLLECTOR_BIN"]], env=bad_env, capture_output=True, timeout=10)
    assert attempt.returncode != 0 and not missing.exists(), "missing data mount must not be created"
    process = start()
    try:
        wait_for(lambda: http("/healthz/ready")[0] == 200)
        print("GET /healthz/ready => 200", flush=True)
        before = watermark()
        wait_for(lambda: http("/status")[1]["buffered"] >= 3)
        process.send_signal(signal.SIGTERM)
        assert process.wait(15) == 0
        after = watermark()
        assert after >= before + 2 * step_ms * 1_000_000, (before, after)
        print(f"SIGTERM exit=0, flushed buffered bars, watermark {before} -> {after}", flush=True)
        process = start()
        wait_for(lambda: http("/healthz/ready")[0] == 200)
        before = watermark()
        process.kill()
        assert process.wait(5) == -signal.SIGKILL
        print(f"SIGKILL exit=-9, durable watermark={before}", flush=True)
        origin = json.loads((state / "replay-origin.json").read_text())
        # Freeze the fixture shortly after the kill. Restart loads the remaining
        # bounded interval even if CI takes a long time to import Nautilus.
        count = (time.time_ns() - origin) // (step_ms * 1_000_000) + 5
        with config.open("a") as stream:
            stream.write(f"  limit: {count}\n")
        process = start()
        wait_for(lambda: watermark() == origin + (count - 1) * step_ms * 1_000_000)
        wait_for(lambda: http("/healthz/ready")[0] == 503)
        status, report = http("/healthz/ready")
        assert any(c == {"name": "fresh:SPY.ARCA", "ok": False} for c in report["checks"])
        assert http("/healthz/live")[0] == 200
        print(f"exhausted fixture => live=200 ready={status}, stale instrument check confirmed", flush=True)
        process.terminate()
        assert process.wait(15) == 0
        rows = ParquetDataCatalog(root / "catalog").bars()
        assert [r.ts_event for r in rows] == [origin + i * step_ms * 1_000_000 for i in range(count)]
        assert str(rows[0].close.as_decimal()).rstrip("0") == "591.131234567"
        print(f"Nautilus catalog returned {len(rows)} ordered unique bars, first close={rows[0].close}", flush=True)
    finally:
        if process.poll() is None:
            process.kill()
            process.wait()


if __name__ == "__main__":
    with tempfile.TemporaryDirectory() as root:
        drill(Path(root))
