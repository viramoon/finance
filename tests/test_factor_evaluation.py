"""F8 fixtures are generated from fixed arithmetic and contain no market data."""

from dataclasses import replace
from datetime import date, datetime, time, timedelta, timezone
from decimal import Decimal, localcontext
import json
import unittest
from unittest.mock import patch

from fertile_finance.factor_analysis import rank_correlation
from fertile_finance.factor_evaluation import EvaluationSession, EvaluationWindow, evaluate_factor
from fertile_finance.research_decimal import RESEARCH_CONTEXT
from fertile_finance.research_factors import FactorDefinition, FactorObservation, PointInTimeFactor, UniverseMembership


D = Decimal
ASSETS = ("SYN-A", "SYN-B", "SYN-C", "SYN-D")


def synthetic_sessions(count):
    """Use weekdays from 2025-01-06 with close 100 + (asset index + 1) × session index."""
    rows = []
    day = date(2025, 1, 6)
    while len(rows) < count:
        if day.weekday() < 5:
            ts = datetime.combine(day, time(20), timezone.utc)
            marks = {asset: D(100 + ordinal * len(rows)) for ordinal, asset in enumerate(ASSETS, 1)}
            rows.append(EvaluationSession(day, ts, marks))
        day += timedelta(days=1)
    return rows


def window(name, phase, rows):
    return EvaluationWindow(name, phase, rows[0].session, rows[-1].session)


def windows(rows):
    return (
        window("fit", "train", rows[:3]),
        window("review", "validation", rows[3:6]),
        window("forward-1", "out_of_sample", rows[6:]),
    )


def inputs(rows, *, factor_id="synthetic", invert=False):
    definition = FactorDefinition(
        factor_id, "1.0.0", "Fixed synthetic instrument ordinal", {"scale": D("1.001"), "signed": True},
        "score", ("stock",),
        "synthetic-1", "Use the explicit publication timestamp", "exclude", 0, "Synthetic test data",
    )
    ts = rows[0].decision_ts
    observations = [FactorObservation(asset, ts, ts, D(-rank if invert else rank))
                    for rank, asset in enumerate(ASSETS, 1)]
    membership = [UniverseMembership(asset, "stock", ts, ts, True,
                                     "sector-a" if rank <= 2 else "sector-b", D(rank * 10))
                  for rank, asset in enumerate(ASSETS, 1)]
    return definition, observations, membership


class FactorEvaluationTest(unittest.TestCase):
    def test_gross_cost_net_and_benchmark_report_recompute_from_synthetic_closes(self):
        rows = synthetic_sessions(9)
        rows[2] = replace(rows[2], closes=dict(zip(ASSETS, map(D, ("103.02", "107.10", "104.03", "104")), strict=True)))
        definition, observations, membership = inputs(rows)
        at = rows[1].decision_ts
        observations.extend(FactorObservation(asset, at, at, D(-rank))
                            for rank, asset in enumerate(ASSETS, 1))
        factor = PointInTimeFactor(definition, observations, membership)
        result = evaluate_factor(factor, rows, windows(rows), cost_bps=D(25), quantiles=2)
        free = evaluate_factor(factor, rows, windows(rows), cost_bps=D(0), quantiles=2)
        train = result.windows[0]
        top1, top2 = (row.quantiles[-1] for row in train.periods[:2])
        base1, base2 = (row.benchmark for row in train.periods[:2])

        self.assertEqual(top1.target_weights, {"SYN-C": D("0.5"), "SYN-D": D("0.5")})
        self.assertEqual(top2.target_weights, {"SYN-A": D("0.5"), "SYN-B": D("0.5")})
        self.assertEqual(top1.industry_weights, {"sector-b": D(1)})
        self.assertEqual(top1.weighted_mean_size, D(35))
        self.assertEqual(top1.size_coverage, D(1))
        self.assertEqual(top1.prior_weights, {})
        self.assertEqual((top1.gross_return, top2.gross_return), (D("0.035"), D("0.035")))
        self.assertEqual((top1.turnover, top2.turnover), (D(1), D(2)))
        self.assertEqual((top1.fee_fraction, top2.fee_fraction), (D("0.0025"), D("0.005")))
        self.assertEqual((top1.net_return, top2.net_return), (D("0.0324125"), D("0.029825")))
        self.assertEqual((base1.gross_return, base2.gross_return), (D("0.025"), D("0.020")))
        with localcontext(RESEARCH_CONTEXT):
            expected_top_gross = D("1.035") ** 2 - 1
            expected_top_net = D("1.0324125") * D("1.029825") - 1
            expected_base_shift = sum(abs(D("0.25") - close / D(410)) for close in rows[1].closes.values())
            expected_base_net2 = D("1.020") * (1 - expected_base_shift * D("0.0025")) - 1
            self.assertEqual(base2.turnover, expected_base_shift)
            self.assertEqual(base2.net_return, expected_base_net2)
            self.assertEqual(base2.prior_weights, {asset: close / D(410) for asset, close in rows[1].closes.items()})
            self.assertEqual(train.benchmark.cumulative_net_return, D("1.0224375") * (1 + expected_base_net2) - 1)
            self.assertEqual(train.gross_information.mean, D("0.0125"))
            std = D("0.0000125").sqrt()
            self.assertEqual(train.gross_information.sample_std, std)
            self.assertEqual(train.gross_information.ratio, D("0.0125") / std * D(252).sqrt())
            active1 = D("0.0324125") - D("0.0224375")
            active2 = D("0.029825") - expected_base_net2
            expected_net_ir = (active1 + active2) / 2 / ((active1 - active2)**2 / 2).sqrt() * D(252).sqrt()
            self.assertLess(abs(train.net_information.ratio - expected_net_ir), D("1e-31"))
        self.assertEqual(train.quantiles[-1].cumulative_gross_return, expected_top_gross)
        self.assertEqual(train.quantiles[-1].cumulative_net_return, expected_top_net)
        self.assertEqual(train.quantiles[-1].total_turnover, D(3))
        self.assertEqual(train.quantiles[-1].total_fee_fraction, D("0.0075"))
        self.assertEqual(free.windows[0].quantiles[-1].cumulative_net_return, expected_top_gross)
        self.assertEqual(result.windows[1].periods[0].quantiles[-1].prior_weights, {})
        self.assertIsNone(train.periods[-1].quantiles)

        wire = json.loads(json.dumps(result.as_dict(), sort_keys=True))
        self.assertEqual(wire["definition"]["factor_id"], "synthetic")
        self.assertEqual(wire["definition"]["parameters"], {"scale": "1.001", "signed": True})
        self.assertNotIn("_parameter_types", wire["definition"])
        self.assertEqual(wire["windows"][0]["periods"][0]["quantiles"][-1]["net_return"], "0.0324125")
        self.assertEqual(wire["windows"][0]["periods"][0]["ic_pairs"], {"1": 4, "5": 0, "20": 0})

    def test_icir_uses_sample_dispersion_and_reports_each_period(self):
        rows = synthetic_sessions(10)
        with localcontext(RESEARCH_CONTEXT):
            for index, rates in enumerate(((1, 2, 3, 4), (4, 3, 2, 1), (1, 2, 3, 4)), 1):
                closes = {asset: rows[index - 1].closes[asset] * (1 + D(bp) / 100)
                          for asset, bp in zip(ASSETS, rates, strict=True)}
                rows[index] = replace(rows[index], closes=closes)
        plan = (
            window("fit", "train", rows[:4]),
            window("review", "validation", rows[4:7]),
            window("forward-1", "out_of_sample", rows[7:]),
        )
        result = evaluate_factor(PointInTimeFactor(*inputs(rows)), rows, plan, cost_bps=D(0), quantiles=2)
        train = result.windows[0]
        self.assertEqual([period.ic[1] for period in train.periods], [D(1), D(-1), D(1), None])
        summary = train.ic[1]
        with localcontext(RESEARCH_CONTEXT):
            self.assertEqual(summary.mean, D(1) / 3)
            self.assertEqual(summary.sample_std, (D(4) / 3).sqrt())
            self.assertEqual(summary.ratio, (D(1) / 3) / (D(4) / 3).sqrt())

    def test_decay_does_not_take_outcomes_across_training_validation_or_rolling_windows(self):
        rows = synthetic_sessions(46)
        plan = (
            window("fit", "train", rows[:22]),
            window("review", "validation", rows[22:28]),
            window("forward-1", "out_of_sample", rows[28:37]),
            window("forward-2", "out_of_sample", rows[37:]),
        )
        factor = PointInTimeFactor(*inputs(rows))
        peer = PointInTimeFactor(*inputs(rows, factor_id="inverse", invert=True))
        result = evaluate_factor(factor, rows, plan, cost_bps=D(0), quantiles=2, peers=[peer])
        for length, observed in zip((22, 6, 9, 9), result.windows, strict=True):
            self.assertEqual({h: summary.count for h, summary in observed.ic.items()},
                             {h: max(0, length - h) for h in (1, 5, 20)})
            self.assertEqual(observed.ic[1].mean, D(1))
            self.assertEqual(observed.ic[1].sample_std, D(0))
            self.assertIsNone(observed.ic[1].ratio)
            self.assertEqual(observed.redundancy["inverse@1.0.0"].mean, D(-1))
        self.assertEqual(result.windows[0].periods[1].ic_pairs[20], 4)
        self.assertEqual(result.windows[0].periods[2].ic_pairs[20], 0)

        for preceding, index in enumerate((22, 28, 37), 1):
            revised = rows.copy()
            revised[index] = replace(rows[index], closes={asset: D(1000 - i) for i, asset in enumerate(ASSETS)})
            revised_result = evaluate_factor(factor, revised, plan, cost_bps=D(0), quantiles=2, peers=[peer])
            self.assertEqual(revised_result.windows[:preceding], result.windows[:preceding])

    def test_unpublished_revision_and_missing_forward_mark_do_not_change_selection(self):
        rows = synthetic_sessions(9)
        definition, observations, membership = inputs(rows)
        revision = FactorObservation("SYN-A", rows[0].decision_ts, rows[1].decision_ts, D(10))
        observations.append(revision)
        observations = [row for row in observations if row.asset_id != "SYN-B"]
        rows[1] = replace(rows[1], closes={asset: close for asset, close in rows[1].closes.items() if asset != "SYN-D"})
        factor = PointInTimeFactor(definition, observations, membership)
        with patch("socket.socket", side_effect=AssertionError("research cannot access sockets")), \
                patch("pathlib.Path.open", side_effect=AssertionError("evaluation only consumes supplied records")):
            report = evaluate_factor(factor, rows, windows(rows), cost_bps=D(25), quantiles=2)
        first, second, third = report.windows[0].periods
        self.assertEqual((first.eligible_count, first.available_count, first.scored_count), (4, 3, 3))
        self.assertEqual(first.scores.mean, RESEARCH_CONTEXT.divide(D(8), D(3)))
        self.assertEqual(first.ic_pairs[1], 2)
        self.assertEqual(first.quantiles[-1].target_weights, {"SYN-D": D(1)})
        self.assertEqual(first.quantiles[-1].asset_returns, {"SYN-D": None})
        self.assertIsNone(first.quantiles[-1].net_return)
        self.assertEqual(second.quantiles[-1].target_weights, {"SYN-A": D(1)})
        self.assertEqual(second.missing_reasons["missing_close"], 1)
        self.assertIsNone(second.quantiles[-1].turnover)
        self.assertIsNone(third.quantiles)
        self.assertIsNone(report.windows[0].quantiles[-1].cumulative_net_return)
        self.assertEqual(report.windows[0].factor_coverage, D("0.75"))
        self.assertEqual(report.windows[0].scored_coverage, RESEARCH_CONTEXT.divide(D(8), D(12)))

    def test_coverage_and_quantiles_keep_unknown_exposure_and_sparse_scores_visible(self):
        rows = synthetic_sessions(9)
        definition, observations, membership = inputs(rows)
        membership[0] = replace(membership[0], industry=None, size=None)
        report = evaluate_factor(PointInTimeFactor(definition, observations, membership), rows, windows(rows),
                                 cost_bps=D(25))
        first = report.windows[0].periods[0]
        self.assertTrue(all(row.target_weights == {} for row in first.quantiles))
        self.assertTrue(all(row.gross_return == 0 and row.turnover == 0 for row in first.quantiles))
        self.assertEqual(first.benchmark.industry_coverage, D("0.75"))
        self.assertEqual(first.benchmark.size_coverage, D("0.75"))
        self.assertEqual(first.benchmark.weighted_mean_size, D(30))

    def test_rank_correlation_averages_ties_and_omits_constant_cross_sections(self):
        with localcontext(RESEARCH_CONTEXT):
            expected = D(3).sqrt() / 2
        tied = rank_correlation([D(1), D(1), D(2)], [D(0), D(1), D(2)])
        self.assertLess(abs(tied - expected), D("1e-32"))
        self.assertIsNone(rank_correlation([D(1), D(1)], [D(1), D(2)]))
        self.assertIsNone(rank_correlation([D(1)], [D(2)]))
        with self.assertRaisesRegex(ValueError, "paired"):
            rank_correlation([D(1)], [])

    def test_invalid_windows_unordered_sessions_and_inexact_inputs_are_rejected(self):
        rows = synthetic_sessions(9)
        plan = windows(rows)
        factor = PointInTimeFactor(*inputs(rows))
        overlap = replace(plan[1], start=plan[0].end)
        with self.assertRaisesRegex(ValueError, "overlap"):
            evaluate_factor(factor, rows, (plan[0], overlap, plan[2]), cost_bps=D(0))
        outside = replace(plan[2], end=date(2030, 1, 1))
        with self.assertRaisesRegex(ValueError, "supplied session dates"):
            evaluate_factor(factor, rows, (plan[0], plan[1], outside), cost_bps=D(0))
        with self.assertRaisesRegex(ValueError, "strictly increasing"):
            evaluate_factor(factor, [rows[1], *rows], plan, cost_bps=D(0))
        with self.assertRaisesRegex(ValueError, "cost_bps"):
            evaluate_factor(factor, rows, plan, cost_bps=25.0)
        with self.assertRaisesRegex(ValueError, "positive finite Decimal"):
            replace(rows[0], closes={"SYN-A": 100.0})
        with self.assertRaisesRegex(ValueError, "timezone-aware"):
            replace(rows[0], decision_ts=rows[0].decision_ts.replace(tzinfo=None))
        with self.assertRaisesRegex(ValueError, "UTC session date"):
            replace(rows[0], decision_ts=rows[0].decision_ts + timedelta(days=1))

    def test_precision_and_output_order_are_independent_of_caller_decimal_context(self):
        rows = synthetic_sessions(9)
        factor = PointInTimeFactor(*inputs(rows))
        plan = windows(rows)
        expected = evaluate_factor(factor, rows, plan, cost_bps=D("12.3456"), quantiles=2)
        with localcontext() as ctx:
            ctx.prec = 4
            transformed = [replace(row, closes=dict(reversed(tuple(row.closes.items())))) for row in rows]
            actual = evaluate_factor(factor, transformed, plan, cost_bps=D("12.3456"), quantiles=2)
        self.assertEqual(actual.as_dict(), expected.as_dict())


if __name__ == "__main__":
    unittest.main()
