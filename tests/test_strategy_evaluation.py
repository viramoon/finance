import csv
from dataclasses import replace
from decimal import Decimal, localcontext
import json
from pathlib import Path
from tempfile import TemporaryDirectory
import unittest

from fertile_finance.backtest_research import run_research
from fertile_finance.research_catalog import import_catalog
from fertile_finance.strategy_evaluation import ClosedBar, Decision, SmaCrossoverPlan, open_evaluator


INSTRUMENT = "TEST.XNYS"
PLAN = SmaCrossoverPlan(INSTRUMENT, 2, 3, 10)


class FlatBackend:
    def __init__(self):
        self.plan = None
        self.bars = []

    def open(self, plan):
        self.plan = plan
        return self

    def evaluate(self, bar):
        self.bars.append(bar)
        if len(self.bars) >= self.plan.slow_period:
            return Decision(bar.instrument_id, bar.ts_event, Decimal(0))
        return None


class StrategyEvaluationTest(unittest.TestCase):
    def test_cpu_sma_exact_targets_and_independent_state(self):
        bars = [ClosedBar(INSTRUMENT, ts, Decimal(close))
                for ts, close in enumerate((100, 100, 110, 105, 95, 110, 100, 100))]
        continuous = open_evaluator(PLAN)
        expected = [None, None, 10, 10, 0, 0, 10, 0]
        actual = [continuous.evaluate(bar) for bar in bars]
        self.assertEqual(actual, [None if shares is None else Decision(INSTRUMENT, ts, Decimal(shares))
                                  for ts, shares in enumerate(expected)])
        replay = open_evaluator(PLAN)
        self.assertEqual([replay.evaluate(bar) for bar in bars], actual)

        with localcontext() as context:
            context.prec = 4
            below_cent = open_evaluator(PLAN)
            for bar in bars[:2]:
                self.assertIsNone(below_cent.evaluate(bar))
            self.assertEqual(below_cent.evaluate(replace(bars[2], close=Decimal("100.000000000000000000001"))),
                             Decision(INSTRUMENT, 2, Decimal(10)))

    def test_invalid_closed_bars_leave_evaluation_state_unchanged(self):
        evaluator = open_evaluator(PLAN)
        self.assertIsNone(evaluator.evaluate(ClosedBar(INSTRUMENT, 1, Decimal(100))))
        for bar in (ClosedBar("OTHER.XNYS", 2, Decimal(1)), ClosedBar(INSTRUMENT, 1, Decimal(1)),
                    ClosedBar(INSTRUMENT, 2, Decimal("NaN")), ClosedBar(INSTRUMENT, 2, 1.0)):
            with self.subTest(bar=bar), self.assertRaises(ValueError):
                evaluator.evaluate(bar)
        self.assertIsNone(evaluator.evaluate(ClosedBar(INSTRUMENT, 2, Decimal(100))))
        self.assertEqual(evaluator.evaluate(ClosedBar(INSTRUMENT, 3, Decimal(100))),
                         Decision(INSTRUMENT, 3, Decimal(0)))

    def test_compiled_plan_requires_version_hash_and_exact_parameters(self):
        plan = PLAN.as_dict()
        self.assertEqual(SmaCrossoverPlan.from_json(json.dumps(plan, indent=2)), PLAN)
        with self.assertRaisesRegex(ValueError, "hash mismatch"):
            SmaCrossoverPlan.from_json(json.dumps({**plan, "instrument_id": "OTHER.XNYS"}))
        with self.assertRaisesRegex(ValueError, "Unsupported"):
            SmaCrossoverPlan.from_json(json.dumps({**plan, "plan_version": "v2"}))
        with self.assertRaisesRegex(ValueError, "Invalid compiled SMA plan JSON"):
            SmaCrossoverPlan.from_json(json.dumps(plan).replace('"shares": 10', '"shares": 10, "shares": 10'))
        params = {**plan["parameters"], "shares": True}
        with self.assertRaisesRegex(ValueError, "1..1000 shares"):
            SmaCrossoverPlan.from_json(json.dumps({**plan, "parameters": params}))

    def test_research_backtest_uses_injected_backend(self):
        # Fixed artificial close sequence over five consecutive sessions, with no random seed.
        rows = [
            ("2024-11-25", 100), ("2024-11-26", 100), ("2024-11-27", 110),
            ("2024-11-29", 120), ("2024-12-02", 120),
        ]
        with TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "synthetic.csv"
            with source.open("w", encoding="utf-8", newline="") as csv_file:
                writer = csv.writer(csv_file, lineterminator="\n")
                writer.writerow(("date", "open", "high", "low", "close", "volume"))
                writer.writerows((day, close, close, close, close, 100) for day, close in rows)
            catalog = root / "catalog"
            import_catalog(catalog, source, instrument=INSTRUMENT, source="Deterministic fixture",
                           kind="fixture", asset_type="stock")
            request = {
                "strategy": "SmaCrossover", "instrument": INSTRUMENT,
                "start": rows[0][0] + "T00:00:00Z", "end": rows[-1][0] + "T00:00:00Z",
                "params": PLAN.as_dict()["parameters"],
            }
            backend = FlatBackend()
            cpu = run_research(catalog, request)
            flat = run_research(catalog, request, backend=backend)
        self.assertEqual(backend.plan, PLAN)
        self.assertEqual([bar.close for bar in backend.bars], [Decimal(close) for _, close in rows])
        self.assertEqual(cpu["total_orders"], 1)
        self.assertEqual(flat["total_orders"], 0)
