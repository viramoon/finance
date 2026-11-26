import csv
from contextlib import closing
from datetime import date
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
from tempfile import TemporaryDirectory
import time
import unittest

from fertile_finance.backtest_research import run_research
from fertile_finance.market_protocol import Event
from fertile_finance.research_catalog import import_catalog, sessions
from fertile_finance.strategy_evaluation import CpuEvaluationBackend, SmaCrossoverPlan


REPO = Path(__file__).resolve().parents[1]
INSTRUMENT = "TEST.XNYS"
FIRST_SEQUENCE = 2**63
LICENSE = "synthetic"


def frame(index, close_ts, price):
    """Generate synthetic F1 bars using caller-supplied whole-dollar OHLC values."""
    px = f"{price}.00"
    return {
        "schema_version": "1.0",
        "channel": "bars.1d",
        "source": "replay",
        "product": "test",
        "instrument_id": INSTRUMENT,
        "asset_kind": "stock",
        "venue": "XNYS",
        "exchange_ts": str(close_ts),
        "receive_ts": str(close_ts + 13),
        "first_available_ts": str(close_ts + 7),
        "epoch": "1",
        "seq": str(FIRST_SEQUENCE + index),
        "cursor": f"synthetic-{index}",
        "revision": "0",
        "license": LICENSE,
        "kind": "bar_closed",
        "payload": {
            "open_ts": str(close_ts - 1), "open": px, "high": px, "low": px,
            "close": px, "volume": "100",
        },
    }


def encode(value):
    return (Event.from_dict(value).to_json() + "\n").encode("utf-8")


class RecordingBackend:
    """Observe the real queued SMA strategy's CPU evaluation before simulated fills."""

    def __init__(self):
        self.targets = []

    def open(self, plan):
        return _RecordingEvaluator(CpuEvaluationBackend().open(plan), self.targets)


class _RecordingEvaluator:
    def __init__(self, cpu, targets):
        self.cpu = cpu
        self.targets = targets

    def evaluate(self, bar):
        decision = self.cpu.evaluate(bar)
        if decision is not None:
            self.targets.append((str(decision.ts_event), format(decision.target_shares, "f")))
        return decision


class DecisionTests(unittest.TestCase):
    def setUp(self):
        temp = TemporaryDirectory(dir="/tmp")
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        self.plan_path = self.root / "plan.json"
        self.plan_path.write_text(json.dumps(SmaCrossoverPlan(INSTRUMENT, 2, 3, 10).as_dict()))
        self.replay_path = self.root / "bars.ndjson"
        self.database = self.root / "finance-state/decisions/decisions.sqlite3"
        self.env = {
            "PATH": os.defpath,
            "FINANCE_PYTHON": sys.executable,
            "FERTILE_DATA_ROOT": str(self.root),
        }

    def arguments(self):
        return ["--plan", str(self.plan_path), "--replay", str(self.replay_path)]

    def command(self, *extras):
        return [str(REPO / "scripts/finance-decision"), *self.arguments(), *extras]

    def launch(self, *extras):
        return subprocess.run(self.command(*extras), env=self.env, capture_output=True, text=True, timeout=10)

    def read(self, sql):
        with closing(sqlite3.connect(self.database.as_uri() + "?mode=ro", uri=True)) as connection:
            return connection.execute(sql).fetchall()

    def wait_for_checkpoint(self, child, offset):
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            if child.poll() is not None:
                self.fail(child.communicate()[1])
            try:
                if self.read("SELECT replay_offset FROM decision_checkpoint") == [(offset,)]:
                    return
            except sqlite3.OperationalError as error:
                if self.database.exists() and "no such table" not in str(error):
                    raise
            time.sleep(0.025)
        self.fail(f"decision checkpoint did not reach byte {offset}")

    def follow(self):
        child = subprocess.Popen(
            self.command("--follow"), env=self.env, stdout=subprocess.PIPE,
            stderr=subprocess.PIPE, text=True,
        )

        def cleanup():
            if child.poll() is None:
                child.kill()
            child.communicate(timeout=10)

        self.addCleanup(cleanup)
        return child

    def test_follow_and_restart_match_production_backtest_signals(self):
        start, end = date(2024, 11, 25), date(2024, 12, 5)
        sample = list(zip(sessions(start, end).items(), (90, 100, 130, 130, 140, 150, 30, 60), strict=True))
        # Add an ordered heartbeat before the third bar. It must not warm up the SMA.
        heartbeat = frame(2, sample[1][0][1], sample[1][1])
        heartbeat.update(kind="heartbeat", exchange_ts=None, payload={})
        replay_frames = [encode(frame(i + (i >= 2), ts, px)) for i, ((_, ts), px) in enumerate(sample)]
        prefix = b"".join(replay_frames[:2]) + encode(heartbeat)
        third = replay_frames[2]
        self.replay_path.write_bytes(prefix + third[:-1])
        child = self.follow()
        self.wait_for_checkpoint(child, len(prefix))
        self.assertEqual(self.read("SELECT target_shares FROM decisions"), [])
        with self.replay_path.open("ab") as out:
            out.write(third[-1:])
        self.wait_for_checkpoint(child, len(prefix + third))
        self.assertEqual(self.read("SELECT target_shares FROM decisions"), [("10",)])
        child.kill()
        _, errors = child.communicate(timeout=10)
        self.assertEqual(child.returncode, -9, errors)

        complete = prefix + b"".join(replay_frames[2:])
        with self.replay_path.open("ab") as out:
            out.write(b"".join(replay_frames[3:]))
        restarted = self.follow()
        self.wait_for_checkpoint(restarted, len(complete))
        restarted.terminate()
        _, errors = restarted.communicate(timeout=10)
        self.assertEqual(restarted.returncode, 0, errors)
        once = self.launch()
        self.assertEqual(once.returncode, 0, once.stderr)
        self.assertEqual(self.replay_path.read_bytes(), complete)
        self.assertFalse((self.root / "finance-state/backtests").exists())

        csv_path = self.root / "synthetic.csv"
        with csv_path.open("w", newline="") as csv_file:
            out = csv.writer(csv_file)
            out.writerow(["date", "open", "high", "low", "close", "volume"])
            out.writerows((day, px, px, px, px, 100) for (day, _), px in sample)
        catalog = self.root / "research"
        import_catalog(catalog, csv_path, instrument=INSTRUMENT, source="Fixed synthetic SMA fixture",
                       kind="fixture", asset_type="stock")
        backend = RecordingBackend()
        run_research(catalog, {
            "strategy": "SmaCrossover", "instrument": INSTRUMENT, "start": f"{start}T00:00:00Z",
            "end": f"{end}T00:00:00Z", "params": {"fast_period": 2, "slow_period": 3, "shares": 10},
        }, backend=backend)
        rows = self.read("SELECT exchange_ts, target_shares FROM decisions ORDER BY replay_offset")
        expected = [(str(ts), target) for ((_, ts), _), target in
                    zip(sample[2:], ("10", "10", "10", "10", "0", "0"), strict=True)]
        self.assertEqual(rows, expected)
        self.assertEqual(rows, backend.targets)
        ts = sample[2][0][1]
        self.assertEqual(self.read(
            "SELECT instrument_id, seq, cursor, license, first_available_ts, decision_ts "
            "FROM decisions ORDER BY replay_offset LIMIT 1"
        ), [(INSTRUMENT, str(FIRST_SEQUENCE + 3), "synthetic-3", LICENSE, str(ts + 7), str(ts + 7))])

    def test_refuses_changed_or_shortened_checkpoint_prefix(self):
        frames = [encode(frame(i, 1_000_000 + i, px)) for i, px in enumerate((100, 110, 120))]
        self.replay_path.write_bytes(b"".join(frames))
        first = self.launch()
        self.assertEqual(first.returncode, 0, first.stderr)
        self.replay_path.write_bytes(encode(frame(0, 1_000_000, 200)) + b"".join(frames[1:]))
        altered = self.launch()
        self.assertEqual(altered.returncode, 1)
        self.assertIn("replay prefix differs", altered.stderr)
        self.replay_path.write_bytes(frames[0])
        shortened = self.launch()
        self.assertEqual(shortened.returncode, 1)
        self.assertIn("before the committed checkpoint", shortened.stderr)
        self.assertEqual(self.read("SELECT target_shares FROM decisions"), [("10",)])

    def test_discontinuities_preserve_last_complete_checkpoint(self):
        first = encode(frame(0, 1_000_000, 100))
        next_bar = frame(1, 1_000_001, 130)
        gap = frame(1, 1_000_001, 100)
        gap.update(kind="gap", exchange_ts=None,
                   payload={"from_ts": "1000001", "to_ts": "1000002", "reason": "synthetic loss"})
        reset = {**next_bar, "kind": "reset", "epoch": "2", "seq": "0", "exchange_ts": None,
                 "payload": {"reason": "synthetic reset"}}
        corrected = {**next_bar, "kind": "correction", "revision": "1", "payload": {
            "target_epoch": "1", "target_seq": str(FIRST_SEQUENCE),
            "replacement": {"kind": "bar_closed", "payload": next_bar["payload"]},
        }}
        for broken, diagnostic in ((gap, "gap"), (reset, "reset"), (corrected, "correction"),
                                   (frame(2, 1_000_002, 130), "/seq"),
                                   ({**next_bar, "asset_kind": "future"}, "stock or ETF")):
            with self.subTest(diagnostic=diagnostic):
                self.replay_path.write_bytes(first + encode(broken))
                interrupted = self.launch()
                self.assertEqual(interrupted.returncode, 1)
                self.assertIn(diagnostic, interrupted.stderr)
                self.assertEqual(self.read("SELECT replay_offset FROM decision_checkpoint"), [(len(first),)])
                self.assertEqual(self.read("SELECT target_shares FROM decisions"), [])

    def test_refuses_changed_plan_for_existing_store(self):
        sample = b"".join(encode(frame(i, 1_000_000 + i, px)) for i, px in enumerate((100, 110, 120)))
        self.replay_path.write_bytes(sample)
        first = self.launch()
        self.assertEqual(first.returncode, 0, first.stderr)
        self.plan_path.write_text(json.dumps(SmaCrossoverPlan(INSTRUMENT, 2, 3, 11).as_dict()))
        changed = self.launch()
        self.assertEqual(changed.returncode, 1)
        self.assertIn("different strategy plan", changed.stderr)
        self.assertEqual(self.read("SELECT target_shares FROM decisions"), [("10",)])

    def test_follow_refuses_replaced_replay_while_waiting_for_bar_availability(self):
        prefix = encode(frame(0, 1_000_000, 100)) + encode(frame(1, 1_000_001, 110))
        future = frame(2, 1_000_002, 120)
        future["first_available_ts"] = str(2**64 - 1)
        original = prefix + encode(future)
        self.replay_path.write_bytes(original)
        child = self.follow()
        self.wait_for_checkpoint(child, len(prefix))
        substitute = self.root / "replacement.ndjson"
        substitute.write_bytes(original)
        substitute.replace(self.replay_path)
        _, errors = child.communicate(timeout=10)
        self.assertEqual(child.returncode, 1, errors)
        self.assertIn("replay was replaced or truncated", errors)
        self.assertEqual(self.read("SELECT replay_offset FROM decision_checkpoint"), [(len(prefix),)])
        self.assertEqual(self.read("SELECT target_shares FROM decisions"), [])

    def test_process_is_socket_free_and_writes_only_decision_store(self):
        self.replay_path.write_bytes(
            b"".join(encode(frame(i, 1_000_000 + i, px)) for i, px in enumerate((100, 110, 120)))
        )
        self.replay_path.chmod(0o444)
        self.plan_path.chmod(0o444)
        guard = """
import os
from pathlib import Path
import sys

root = Path(os.environ["FERTILE_DATA_ROOT"])
database = root / "finance-state/decisions/decisions.sqlite3"
def local_decision(event, args):
    assert not event.startswith("socket."), event
    if event == "open":
        assert not args[2] & (os.O_WRONLY | os.O_RDWR | os.O_CREAT | os.O_TRUNC), args
    if event == "sqlite3.connect":
        assert Path(args[0]) == database, args
    if event == "os.mkdir":
        assert Path(args[0]) in (database.parent, database.parent.parent), args

sys.addaudithook(local_decision)
from fertile_finance.decision import main
code = main()
assert not any(name.startswith("nautilus_trader") for name in sys.modules)
raise SystemExit(code)
"""
        out = subprocess.run(
            [sys.executable, "-B", "-c", guard, *self.arguments()],
            env={**self.env, "PYTHONPATH": str(REPO / "python")}, text=True, capture_output=True, timeout=10,
        )
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertEqual(self.read("SELECT target_shares FROM decisions"), [("10",)])
        self.assertEqual(set((self.root / "finance-state").rglob("*")),
                         {self.database.parent, self.database})
        order = self.launch("--submit-order")
        self.assertEqual(order.returncode, 2)
        self.assertIn("unrecognized arguments", order.stderr)
