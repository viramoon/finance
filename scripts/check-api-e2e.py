"""对真实批任务生成物执行 HTTP 验证；不伪造回测结果。"""
import json
from datetime import datetime
import os
from pathlib import Path
import socket
import subprocess
import time
from urllib.request import urlopen

root = Path(os.environ["FERTILE_DATA_ROOT"])
expected = json.loads((root / "finance-state/backtests/sample/latest.json").read_text())
with socket.socket() as sock:
    sock.bind(("127.0.0.1", 0))
    port = sock.getsockname()[1]
env = dict(os.environ, FINANCE_API_BIND_HOST="127.0.0.1", FINANCE_API_PORT=str(port))
process = subprocess.Popen([os.environ["FINANCE_API_BIN"]], env=env)
try:
    deadline = time.monotonic() + 15
    while True:
        try:
            with urlopen(f"http://127.0.0.1:{port}/healthz/live", timeout=1) as response:
                assert response.status == 200
            break
        except OSError:
            if process.poll() is not None or time.monotonic() > deadline:
                raise RuntimeError("API failed to become live")
            time.sleep(0.1)
    for route in ["/healthz/ready", "/api/v1/backtests/sample"]:
        with urlopen(f"http://127.0.0.1:{port}{route}", timeout=3) as response:
            assert response.status == 200
            body = json.load(response)
            if route.endswith("sample"):
                for field in ("start", "end", "generated_at"):
                    assert datetime.fromisoformat(body.pop(field)) == datetime.fromisoformat(expected[field])
                assert body == {k: v for k, v in expected.items() if k not in ("start", "end", "generated_at")}, (body, expected)
                assert isinstance(body["pnl"], str)
    print("PASS real backtest JSON -> HTTP summary, exact string PnL, live and ready")
finally:
    process.terminate()
    assert process.wait(timeout=5) == 0, "API did not drain cleanly after SIGTERM"
