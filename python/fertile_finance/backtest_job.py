"""Run one validated, finite sample interval in an isolated Linux child."""

import argparse
import ctypes
import json
import os
from pathlib import Path
import signal

from .backtest_sample import run_sample, atomic_write


def die_with_parent(expected: int) -> None:
    libc = ctypes.CDLL(None, use_errno=True)
    # PR_SET_PDEATHSIG. Recheck parent after registering to close the race.
    if libc.prctl(1, signal.SIGKILL, 0, 0, 0):
        raise OSError(ctypes.get_errno(), "prctl(PR_SET_PDEATHSIG)")
    if os.getppid() != expected:
        raise SystemExit("worker already exited")


def execute(request: dict, target: Path) -> None:
    if request["strategy"] == "SmaCrossover":
        from .backtest_research import run_research
        from .research_catalog import ResearchError, research_root

        try:
            summary = run_research(research_root(), request)
        except ResearchError as exc:
            print(f"{exc.code}: {exc}", flush=True)
            summary = {"research_error": exc.code}
        atomic_write(target, summary)
        return
    if (request["strategy"], request["instrument"], request["params"]) != (
        "DemoStrategy",
        "6EH4.XCME",
        {},
    ):
        raise ValueError("unsupported job request")
    repo = Path(__file__).resolve().parents[2]
    summary = run_sample(repo, start=request["start"], end=request["end"])
    atomic_write(target, summary)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("request", type=Path)
    parser.add_argument("result", type=Path)
    args = parser.parse_args()
    die_with_parent(int(os.environ["FINANCE_WORKER_PID"]))
    execute(json.loads(args.request.read_text()), args.result)
