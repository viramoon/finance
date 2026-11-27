"""Compare F7 job targets with the decision process on fixed synthetic sessions."""

from contextlib import closing
from decimal import Decimal
from hashlib import sha256
import json
import os
from pathlib import Path
import shutil
import sqlite3
import subprocess
import sys
from tempfile import TemporaryDirectory
import time
import unittest

from config_strategy_replay_fixture import BARS_REF, UNIVERSE_REF, build
from fertile_finance.backtest_job import execute
from fertile_finance.research_catalog import nyse
from fertile_finance.strategy_backtest_job import run_strategy_backtest_job
from fertile_finance.strategy_replay import canonical_sha256


REPO = Path(__file__).resolve().parents[1]
SESSION_SCHEMA = "fertile.finance/decision-session/v1"
EXECUTION = "execution.next_close_cash_equity@1.0.0"
BOLLINGER = "indicator.bollinger@1.0.0"


def _plan(*, ranking):
    """Build a normalized synthetic plan consumed by the F7 job entry point."""
    modules = {
        "volume": {"kind": "filter", "use": "filter.complete_session_bar@1.0.0",
                   "params": {"require_positive_volume": True}},
        "cash": {"kind": "risk", "use": "risk.long_only_cash@1.0.0",
                 "params": {"max_gross_weight": "1", "allow_borrow": False}},
    }
    recipe = {"filters": ["volume"], "risk": ["cash"]}
    if ranking:
        name = "topTwo"
        modules["rank"] = {"kind": "allocator", "use": "allocator.top_n@1.0.0",
                           "params": {"factor": "momentum", "top_n": 2}}
        recipe.update(type="target_weights", allocator="rank")
    else:
        name = "A1_B2"
        modules.update(
            A1={"kind": "entry", "use": "entry.above_when_flat@1.0.0",
                "params": {"factor": "momentum", "threshold": "0.02"}},
            B2={"kind": "exit", "use": "exit.holding_sessions@1.0.0",
                "params": {"sessions": 10, "start_at": "first_fill"}},
            size={"kind": "sizing", "use": "sizing.equal_weight_capped@1.0.0",
                  "params": {"max_weight_per_asset": "0.25"}},
        )
        recipe.update(type="entry_exit", entry="A1", exit="B2", sizing="size")
    conflict_rules = {
        "on_entry_exit_collision": "exit_first", "on_missing_execution_price": "defer_and_report",
        "same_session_reentry": False, "tie_break": "stable_instrument_id",
    }
    sleeves = [{"recipe": name, "priority": 10, "budget_weight": "1"}]
    universe = {"ref": UNIVERSE_REF, "membership": "point_in_time"}
    bars = {"bars": BARS_REF, "timeframe": "1d", "price_basis": "raw_split_free_reviewed", "currency": "USD"}
    bundle = {
        "apiVersion": "fertile.finance/strategy/v1", "kind": "StrategyBundle",
        "metadata": {"id": "test.ranking" if ranking else "test.entry-exit", "version": "1.0.0",
                     "title": "Fixed synthetic decision replay"},
        "spec": {
            "engine": "nautilus_cash_equity", "mode": "backtest", "universe": universe, "data": bars,
            "clock": {"calendar": "XNYS", "warmup_sessions": 30, "decide_on": "session_close",
                      "earliest_fill": "next_session_close", "missing_input": "block_new_risk"},
            "policy": conflict_rules,
            "portfolio": {"initial_cash": {"amount": "10000" if ranking else "100000", "currency": "USD"},
                          "max_gross_weight": "1", "max_weight_per_asset": "1" if ranking else "0.25",
                          "sleeves": sleeves, "allocation": "fixed_budgets", "residual": "cash", "netting": "by_instrument"},
            "execution": {"model": EXECUTION, "lot_size_policy": "instrument_master",
                          "commission_bps": "0" if ranking else "2", "slippage_bps": "0" if ranking else "5"},
            "factors": {"momentum": {"use": "factor.close_return@1.0.0", "params": {"lookback_sessions": 20}}},
            "modules": modules, "recipes": {name: recipe},
        },
    }
    digest = canonical_sha256(bundle)
    return {
        "plan_version": "fertile.finance/strategy-plan/v1", "content_sha256": digest, "source_sha256": digest,
        "bundle": bundle, "execution": EXECUTION, "sleeves": sleeves, "conflict_rules": conflict_rules,
        "inputs": {"bars": {"id": BARS_REF}, "universe": {"id": UNIVERSE_REF}},
        "factors": [{"component": "factor.close_return@1.0.0"}],
        "modules": [{"component": module["use"]} for module in modules.values()],
    }


def _encode(frame):
    return (json.dumps(frame, sort_keys=True) + "\n").encode("utf-8")


def _bollinger_bundle():
    bundle = _plan(ranking=False)["bundle"]
    spec = bundle["spec"]
    spec["factors"] = {
        name: {"use": BOLLINGER, "params": {"period": 2}, "output": name}
        for name in ("upper", "lower")
    }
    entry = spec["modules"]["A1"]
    entry["params"] = {"factor": "upper", "threshold": "100"}
    spec["modules"]["lowerEntry"] = {**entry, "params": {"factor": "lower", "threshold": "100"}}
    spec["recipes"]["Lower"] = {**spec["recipes"]["A1_B2"], "entry": "lowerEntry"}
    sleeves = spec["portfolio"]["sleeves"]
    sleeves[0]["budget_weight"] = "0.5"
    sleeves.append({"recipe": "Lower", "priority": 20, "budget_weight": "0.5"})
    return bundle


def _session_frame(session):
    def number(value):
        return format(value.as_decimal(), "f")

    return {
        "schema": SESSION_SCHEMA, "session": session.day.isoformat(), "warmup": not session.emit,
        "members": sorted(session.members), "cash": format(session.cash, "f"),
        "bars": {
            instrument: {
                **{field: number(getattr(bar, field)) for field in ("open", "high", "low", "close", "volume")},
                "first_available_ts": str(session.ts_event),
            }
            for instrument, bar in session.bars.items()
        },
        "sleeves": {
            recipe: {"shares": {key: str(shares) for key, shares in sleeve.shares.items()},
                     "sessions_held": {key: str(count) for key, count in sleeve.sessions_held.items()},
                     "exited": sorted(sleeve.exited)}
            for recipe, sleeve in session.sleeves.items()
        },
        "pending_reductions": [
            {"instrument_id": instrument, "recipe": recipe, "quantity": str(quantity)}
            for (instrument, recipe), quantity in sorted(session.pending_reductions.items())
        ],
    }


class BundleDecisionTests(unittest.TestCase):
    def setUp(self):
        temp = TemporaryDirectory(dir="/tmp")
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        self.plan_path = self.root / "plan.json"
        self.replay_path = self.root / "sessions.ndjson"
        self.database = self.root / "finance-state/decisions/decisions.sqlite3"
        self.env = {"PATH": os.defpath, "FINANCE_PYTHON": sys.executable, "FERTILE_DATA_ROOT": str(self.root)}
        if "FINANCE_STRATEGY_BIN" in os.environ:
            self.env["FINANCE_STRATEGY_BIN"] = os.environ["FINANCE_STRATEGY_BIN"]
        self.arguments = ["--plan", str(self.plan_path), "--replay", str(self.replay_path)]
        self.command = [str(REPO / "scripts/finance-decision"), *self.arguments]

    def record_backtest(self, *, ranking, plan=None, raw=None):
        if plan is None:
            plan = _plan(ranking=ranking)
        plan_digest = canonical_sha256(plan)
        self.plan_path.write_text(json.dumps(plan), encoding="utf-8")
        if raw is None:
            # The synthetic replay lives outside the decision state directory.
            fixture = self.root / "fixture"
            build(fixture, target_weights=ranking)
            replay = fixture / "finance-state/backtests/strategy-inputs" / BARS_REF / f"{UNIVERSE_REF}.json"
            raw = replay.read_bytes()
        frames, rows = [], []
        offset = 0

        def record(session, result):
            nonlocal offset
            frame = _session_frame(session)
            offset += len(_encode(frame))
            frames.append(frame)
            if session.emit:
                for instrument, recipe in sorted(result.targets, key=lambda key: (key[1], key[0])):
                    key = (instrument, recipe)
                    rows.append((offset, plan_digest, frame["session"], str(session.ts_event),
                                 recipe, instrument, str(result.targets[key]), result.reasons.get(key)))

        report = run_strategy_backtest_job({
            "plan": plan, "snapshot_sha256": sha256(raw).hexdigest(), "start": "2024-03-01T00:00:00Z",
            "end": "2024-03-08T00:00:00Z" if ranking else "2024-04-15T00:00:00Z",
        }, snapshot=raw, signal_observer=record)
        self.assertGreater(report["total_orders"], 0)
        return frames, rows

    def _read(self, sql):
        with closing(sqlite3.connect(self.database.as_uri() + "?mode=ro", uri=True)) as connection:
            return connection.execute(sql).fetchall()

    def _rows(self):
        return self._read(
            "SELECT replay_offset, plan_sha256, session, signal_ts, recipe, instrument_id, target_shares, reason "
            "FROM bundle_decisions ORDER BY replay_offset, recipe, instrument_id"
        )

    def _launch(self):
        return subprocess.run(self.command, env=self.env, capture_output=True, text=True, timeout=20)

    def _follow(self):
        child = subprocess.Popen([*self.command, "--follow"], env=self.env, stderr=subprocess.PIPE, text=True)

        def cleanup():
            if child.poll() is None:
                child.kill()
            child.communicate(timeout=10)

        self.addCleanup(cleanup)
        return child

    def _wait_for_checkpoint(self, child, offset):
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            if child.poll() is not None:
                self.fail(child.communicate()[1])
            try:
                if self._read("SELECT replay_offset FROM decision_checkpoint") == [(offset,)]:
                    return
            except sqlite3.OperationalError as error:
                if self.database.exists() and "no such table" not in str(error):
                    raise
            time.sleep(0.025)
        self.fail(f"decision checkpoint did not reach byte {offset}")

    def test_entry_exit_targets_match_backtest_after_process_restart(self):
        frames, expected = self.record_backtest(ranking=False)
        active = next(i for i, frame in enumerate(frames) if not frame["warmup"])
        self.assertGreaterEqual(active, 30)
        lines = [_encode(frame) for frame in frames]
        prefix = b"".join(lines[:active])
        first = lines[active]
        self.replay_path.write_bytes(prefix + first[:-1])
        child = self._follow()
        self._wait_for_checkpoint(child, len(prefix))
        self.assertEqual(self._rows(), [])
        with self.replay_path.open("ab") as stream:
            stream.write(first[-1:])
        stop_offset = len(prefix + first)
        self._wait_for_checkpoint(child, stop_offset)
        self.assertEqual(self._rows(), [row for row in expected if row[0] <= stop_offset])
        child.kill()
        _, stderr = child.communicate(timeout=10)
        self.assertEqual(child.returncode, -9, stderr)

        with self.replay_path.open("ab") as stream:
            stream.writelines(lines[active + 1:])
        restarted = self._follow()
        self._wait_for_checkpoint(restarted, sum(map(len, lines)))
        restarted.terminate()
        _, stderr = restarted.communicate(timeout=10)
        self.assertEqual(restarted.returncode, 0, stderr)
        self.assertEqual(self._rows(), expected)
        once = self._launch()
        self.assertEqual(once.returncode, 0, once.stderr)
        self.assertEqual(self._rows(), expected)
        self.assertEqual(self.replay_path.read_bytes(), b"".join(lines))
        for point in (("2024-03-01", "AAA.XNYS", "177", "recipe_entry"),
                      ("2024-03-01", "BBB.XNYS", "155", "recipe_entry"),
                      ("2024-03-01", "CCC.XNYS", "138", "recipe_entry"),
                      ("2024-03-18", "BBB.XNYS", "0", "recipe_exit")):
            self.assertIn(point, {(row[2], *row[5:]) for row in self._rows()})

    def test_target_weights_targets_match_backtest_without_execution_access(self):
        frames, expected = self.record_backtest(ranking=True)
        self.replay_path.write_bytes(b"".join(map(_encode, frames)))
        guard = """
import os
from pathlib import Path
import stat
import sys

root = Path(os.environ['FERTILE_DATA_ROOT'])
database = root / 'finance-state/decisions/decisions.sqlite3'
indicator = os.environ.get('FINANCE_STRATEGY_BIN', 'finance-strategy')
indicator_launches = []
def local_decision(event, args):
    assert not event.startswith('socket.'), event
    if event == 'open' and args[2] & (os.O_WRONLY | os.O_RDWR | os.O_CREAT | os.O_TRUNC):
        # Popen wraps the existing stdin pipe descriptor in a writable stream.
        assert type(args[0]) is int and stat.S_ISFIFO(os.fstat(args[0]).st_mode), args
    if event == 'subprocess.Popen':
        assert args[0] == indicator and args[1] == [indicator, 'evaluate-indicators'], args
        indicator_launches.append(args[0])
    if event == 'sqlite3.connect':
        assert Path(args[0]) == database, args
    if event == 'os.mkdir':
        assert Path(args[0]) in (database.parent, database.parent.parent), args

sys.addaudithook(local_decision)
from fertile_finance.decision import main
result = main()
assert indicator_launches == [indicator]
assert 'fertile_finance.strategy_backtest' not in sys.modules
assert 'nautilus_trader.backtest.engine' not in sys.modules
assert 'nautilus_trader.trading.strategy' not in sys.modules
raise SystemExit(result)
"""
        run = subprocess.run(
            [sys.executable, "-B", "-c", guard, *self.arguments],
            env={**self.env, "PYTHONPATH": str(REPO / "python")}, capture_output=True, text=True, timeout=20,
        )
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(self._rows(), expected)
        self.assertEqual(set((self.root / "finance-state").rglob("*")), {self.database.parent, self.database})
        once = self._launch()
        self.assertEqual(once.returncode, 0, once.stderr)
        self.assertEqual(self._rows(), expected)
        points = {(row[2], *row[5:]) for row in self._rows()}
        for point in (("2024-03-01", "BBB.XNYS", "50", "target_weight"),
                      ("2024-03-01", "CCC.XNYS", "100", "target_weight"),
                      ("2024-03-05", "BBB.XNYS", "0", "target_weight"),
                      ("2024-03-05", "AAA.XNYS", "20", "target_weight")):
            self.assertIn(point, points)

    def test_f6_bollinger_chart_worker_and_restarted_decision_agree(self):
        compiler = os.environ.get("FINANCE_STRATEGY_BIN") or shutil.which("finance-strategy")
        self.assertIsNotNone(compiler, "Set FINANCE_STRATEGY_BIN to the built finance-strategy executable")
        self.env["FINANCE_STRATEGY_BIN"] = compiler
        compiled = subprocess.run(
            [compiler, "compile", "--format", "json", "-"], input=json.dumps(_bollinger_bundle()),
            capture_output=True, text=True, timeout=20,
        )
        self.assertEqual(compiled.returncode, 0, compiled.stderr)
        plan = json.loads(compiled.stdout)
        self.assertEqual({row.get("output") for row in plan["factors"]}, {"upper", "lower"})
        replay_path = os.environ.get("FINANCE_F6_REPLAY_JSON")
        chart_path = os.environ.get("FINANCE_F6_CHART_JSON")
        self.assertEqual(bool(replay_path), bool(chart_path), "Chart integration requires both fixture paths")
        replay = Path(replay_path).read_bytes() if replay_path else None
        frames, expected = self.record_backtest(ranking=True, plan=plan, raw=replay)
        if chart_path:
            chart = json.loads(Path(chart_path).read_text(encoding="utf-8"))
            self._assert_bollinger_matches_chart(compiler, chart, frames)

        # BBB rises from 80 to 100 at the first decision close. Only its upper
        # band exceeds 100. AAA's lower band crosses 100 after two closes at 250.
        points = {(row[2], *row[4:]) for row in expected}
        self.assertEqual([row[4:] for row in expected if row[2] == "2024-03-01"],
                         [("A1_B2", "BBB.XNYS", "250", "recipe_entry")])
        self.assertTrue(any(day == "2024-03-06" and recipe == "Lower" and instrument == "AAA.XNYS"
                            and int(target) > 0 and reason == "recipe_entry"
                            for day, recipe, instrument, target, reason in points))

        lines = list(map(_encode, frames))
        first_active = next(i for i, frame in enumerate(frames) if not frame["warmup"])
        prefix = b"".join(lines[:first_active + 1])
        self.replay_path.write_bytes(prefix)
        first = self._follow()
        self._wait_for_checkpoint(first, len(prefix))
        self.assertEqual(self._rows(), [row for row in expected if row[0] <= len(prefix)])
        first.kill()
        _, stderr = first.communicate(timeout=10)
        self.assertEqual(first.returncode, -9, stderr)

        with self.replay_path.open("ab") as stream:
            stream.writelines(lines[first_active + 1:])
        restarted = self._follow()
        self._wait_for_checkpoint(restarted, sum(map(len, lines)))
        restarted.terminate()
        _, stderr = restarted.communicate(timeout=10)
        self.assertEqual(restarted.returncode, 0, stderr)
        self.assertEqual(self._rows(), expected)
        once = self._launch()
        self.assertEqual(once.returncode, 0, once.stderr)
        self.assertEqual(self._rows(), expected)

        # Before the F6 bridge, checkpoints used the compiled plan hash alone.
        # The same plan must not append to an existing pre-F6 decision history.
        old_checkpoint = canonical_sha256(plan)
        with closing(sqlite3.connect(self.database)) as connection, connection:
            self.assertNotEqual(connection.execute("SELECT plan_sha256 FROM decision_checkpoint").fetchone(),
                                (old_checkpoint,))
            connection.execute("UPDATE decision_checkpoint SET plan_sha256=?", (old_checkpoint,))
        previous_backend = self._launch()
        self.assertEqual(previous_backend.returncode, 1, previous_backend.stderr)
        self.assertIn("decision store belongs to a different strategy plan", previous_backend.stderr)
        self.assertEqual(self._rows(), expected)

    def _assert_bollinger_matches_chart(self, compiler, chart, frames):
        schedule = nyse().schedule
        self.assertEqual(set(chart), set(frames[0]["bars"]))
        for instrument, rows in sorted(chart.items()):
            by_day = {row["bar"]["trading_day"]: row for row in rows}
            for selected in ("upper", "lower"):
                commands = [{"action": "start", "series": 1, "id": BOLLINGER,
                             "params": {"period": 2}, "output": selected}]
                values = [{"value": None}]
                for frame in frames:
                    day = frame["session"]
                    row = by_day[day]
                    bar = frame["bars"][instrument]
                    fields = {name: bar[name] for name in ("open", "high", "low", "close", "volume")}
                    self.assertEqual({name: Decimal(value) for name, value in fields.items()},
                                     {name: Decimal(row["bar"][name]) for name in fields})
                    opening, closing = schedule.at[day, "open"], schedule.at[day, "close"]
                    self.assertEqual(row["bar"]["open_ts"], opening.isoformat().replace("+00:00", "Z"))
                    self.assertEqual(row["bar"]["end_ts"], closing.isoformat().replace("+00:00", "Z"))
                    self.assertEqual(str(closing.value), bar["first_available_ts"])
                    self.assertFalse(row["provisional"])
                    commands.append({"action": "push", "series": 1, "bars": [{
                        "trading_day": day, "open_ts": str(opening.value),
                        "end_ts": bar["first_available_ts"], **fields,
                    }]})
                    values.append({"value": row["indicator_values"][0]["values"].get(selected)})
                points = subprocess.run(
                    [compiler, "evaluate-indicators"], input=b"".join(map(_encode, commands)),
                    capture_output=True, timeout=20,
                )
                self.assertEqual(points.returncode, 0, points.stderr.decode(errors="replace"))
                actual = [json.loads(line) for line in points.stdout.splitlines()]
                self.assertEqual(actual, values, (instrument, selected))
                self.assertIsNone(actual[1]["value"])
                self.assertIsNotNone(actual[-1]["value"])

    def test_bundle_invalid_session_does_not_advance_checkpoint(self):
        frames, _ = self.record_backtest(ranking=True)
        active = next(i for i, frame in enumerate(frames) if not frame["warmup"])
        prefix = b"".join(map(_encode, frames[:active]))
        frame = frames[active]
        key = "marker/~\nforged diagnostic"
        future = {**frame["bars"]["AAA.XNYS"], "first_available_ts": str(2**63 - 1)}
        for case, bad in (
            ("untrusted field name", {**frame, key: True}),
            ("future bar", {**frame, "bars": {**frame["bars"], "AAA.XNYS": future}}),
            ("uncovered reduction", {**frame, "pending_reductions": [
                {"recipe": "topTwo", "instrument_id": "AAA.XNYS", "quantity": "-1"},
            ]}),
        ):
            with self.subTest(case=case):
                self.replay_path.write_bytes(prefix + _encode(bad))
                run = self._launch()
                self.assertEqual(run.returncode, 1, run.stderr)
                self.assertIn(f"at byte {len(prefix)}", run.stderr)
                self.assertEqual(len(run.stderr.splitlines()), 1, run.stderr)
                self.assertNotIn("marker", run.stderr)
                self.assertNotIn("forged diagnostic", run.stderr)
                self.assertEqual(self._read("SELECT replay_offset FROM decision_checkpoint"), [(len(prefix),)])
                self.assertEqual(self._rows(), [])

    def test_compiled_sleeve_budget_is_exact_and_bound_to_bundle(self):
        plan = _plan(ranking=True)
        spec = plan["bundle"]["spec"]
        sleeve = spec["portfolio"]["sleeves"][0]
        sleeve["budget_weight"] = "0.3"
        plan["sleeves"] = [sleeve.copy()]
        spec["portfolio"]["initial_cash"]["amount"] = "100"
        spec["clock"]["warmup_sessions"] = 1
        spec["factors"]["momentum"]["params"]["lookback_sessions"] = 1
        plan["content_sha256"] = canonical_sha256(plan["bundle"])

        instrument = "AAA.XNYS"
        sessions = (("2024-03-01", True), ("2024-03-04", False))
        frames = []
        for day, warmup in sessions:
            bar = dict.fromkeys(("open", "high", "low", "close"), "0.10")
            bar.update(volume="100", first_available_ts=str(nyse().schedule.at[day, "close"].value))
            frames.append({
                "schema": SESSION_SCHEMA, "session": day, "warmup": warmup, "members": [instrument],
                "cash": "100", "bars": {instrument: bar}, "pending_reductions": [],
                "sleeves": {sleeve["recipe"]: {"shares": {}, "sessions_held": {}, "exited": []}},
            })
        self.replay_path.write_bytes(b"".join(map(_encode, frames)))
        self.plan_path.write_text(json.dumps(plan), encoding="utf-8")
        valid = self._launch()
        self.assertEqual(valid.returncode, 0, valid.stderr)
        self.assertEqual(self._read("SELECT target_shares FROM bundle_decisions"), [("300",)])

        rehashed = json.loads(json.dumps(plan))
        rehashed["bundle"]["spec"]["portfolio"]["sleeves"][0]["budget_weight"] = 0.3
        rehashed["sleeves"][0]["budget_weight"] = 0.3
        rehashed["content_sha256"] = canonical_sha256(rehashed["bundle"])
        for index, (case, changes) in enumerate((
            ("fractional JSON number", {"sleeves": [{**sleeve, "budget_weight": 0.3}]}),
            ("changed decimal string", {"sleeves": [{**sleeve, "budget_weight": "0.4"}]}),
            ("non-integer priority", {"sleeves": [{**sleeve, "priority": 10.0}]}),
            ("changed priority", {"sleeves": [{**sleeve, "priority": 11}]}),
            ("changed execution model", {"execution": "execution.modified@1.0.0"}),
            ("changed data reference", {"inputs": {**plan["inputs"], "bars": {"id": "test.other@1.0.0"}}}),
            ("rehashed floating budget", rehashed),
        )):
            with self.subTest(case=case):
                tampered = {**plan, **changes}
                self.plan_path.write_text(json.dumps(tampered), encoding="utf-8")
                # A new data root prevents the checkpoint's whole-plan digest from masking
                # acceptance of a tampered plan by the loader.
                empty_root = self.root / f"invalid-{index}"
                empty_root.mkdir()
                rejected = subprocess.run(
                    self.command, env={**self.env, "FERTILE_DATA_ROOT": str(empty_root)},
                    capture_output=True, text=True, timeout=20,
                )
                self.assertEqual(rejected.returncode, 1, rejected.stderr)
                self.assertEqual(rejected.stderr, "finance-decision: Invalid compiled StrategyBundle plan\n")
                self.assertEqual(list(empty_root.iterdir()), [])

                result = self.root / "worker-result.json"
                execute({"strategy": "StrategyBundle", "plan": tampered}, result)
                self.assertEqual(json.loads(result.read_text()), {"strategy_error": "strategy_plan_invalid"})


if __name__ == "__main__":
    unittest.main()
