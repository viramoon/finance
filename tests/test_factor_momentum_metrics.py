from dataclasses import dataclass
from datetime import date
from decimal import Decimal
from importlib import import_module
from statistics import mean, stdev
import unittest

import numpy as np

from fertile_finance.research_windows import month_end, monthly_coverage_windows


metrics = import_module("strategies.2026-11-25-factor-momentum.metrics")
D = Decimal
BASELINE = date(2019, 12, 31)


@dataclass(frozen=True)
class Observation:
    nav_open: Decimal
    nav_close: Decimal
    net_return: Decimal
    rf: Decimal
    turnover: Decimal
    weights: dict[str, Decimal]
    gross_exposure: Decimal
    net_exposure: Decimal


def fixture():
    nav = D(100)
    observations = {}
    parameters = ((".10", ".01", "1", "1"), ("-.20", ".005", "2", "-1"), (".25", ".002", "2", "1"))
    for offset, values in enumerate(parameters, 1):
        month = month_end(BASELINE, offset)
        net_return, rf, turnover, weight = map(D, values)
        close = nav * (1 + net_return)
        observations[month] = Observation(nav, close, net_return, rf, turnover, {"Mom": weight}, abs(weight), weight)
        nav = close
    return observations


class MonthlyMetricsTests(unittest.TestCase):
    def test_window_coverage_returns_null_on_short_and_gapped_samples(self):
        observations = fixture()
        end = max(observations)
        rows = metrics.window_rows(observations, end, ("Mom",))
        for name, count in (("50y", 600), ("10y", 120), ("5y", 60)):
            self.assertEqual(rows[name]["expected_months"], count)
            self.assertEqual(rows[name]["missing_months"], count - 3)
            self.assertEqual(rows[name]["coverage"], "insufficient")
            self.assertIsNone(rows[name]["metrics"])
        self.assertEqual(rows["longest_sample"]["coverage"], "full")

        del observations[month_end(BASELINE, 2)]
        row = metrics.window_row(observations, "oos", month_end(BASELINE, 1), end, ("Mom",))
        self.assertEqual((row["coverage"], row["observations"], row["missing_months"]), ("insufficient", 2, 1))
        self.assertIsNone(row["metrics"])
        empty = metrics.window_rows({}, end, ("Mom",))["longest_sample"]
        self.assertEqual((empty["coverage"], empty["observations"]), ("insufficient", 0))
        self.assertIsNone(empty["metrics"])
        future = metrics.window_row(observations, "future", month_end(end, 1), end, ("Mom",))
        self.assertEqual(future["coverage_reason"], "cutoff_before_requested_start")
        self.assertIsNone(future["metrics"])

    def test_fixed_window_counts_returns_after_calendar_baseline(self):
        end = date(2024, 2, 29)
        days = [month_end(end, offset) for offset in range(-59, 1)]
        row = monthly_coverage_windows(days, end)[2]
        self.assertEqual(row["requested_baseline_date"], "2019-02-28")
        self.assertEqual((row["actual_start"], row["actual_end"]), ("2019-03-31", "2024-02-29"))
        self.assertEqual((row["coverage"], row["observations"]), ("full", 60))

    def test_nav_baseline_rf_drawdown_exposure_and_turnover(self):
        observations = fixture()
        row = metrics.window_row(observations, "oos", min(observations), max(observations), ("Mom",))
        detail = row["metrics"]
        self.assertEqual(row["nav_baseline_date"], BASELINE.isoformat())
        self.assertEqual(detail["return_pct"], "10.0000")
        self.assertEqual(detail["cagr_pct"], "46.4100")
        self.assertEqual(detail["max_drawdown_pct"], "20.0000")
        self.assertEqual(detail["drawdown_peak"], "2020-01-31")
        self.assertEqual(detail["drawdown_trough"], "2020-02-29")
        self.assertEqual(detail["drawdown_recovery"], "2020-03-31")
        factors = detail["factor_weights"]["Mom"]
        self.assertEqual(factors["average_weight_pct"], "33.3333")
        self.assertEqual((factors["long_month_pct"], factors["short_month_pct"]), ("66.6667", "33.3333"))
        self.assertEqual(detail["average_gross_exposure_pct"], "100.0000")
        self.assertEqual(detail["average_net_exposure_pct"], "33.3333")
        self.assertEqual(detail["average_monthly_turnover_pct"], "166.6667")
        self.assertEqual(detail["total_turnover_pct"], "500.0000")
        excess = [D(".09"), D("-.205"), D(".248")]
        expected = mean(excess) / stdev(excess) * D(12).sqrt()
        self.assertEqual(detail["sharpe"], format(expected, ".4f"))
        self.assertAlmostEqual(D(detail["sharpe_precise"]), expected, places=26)

    def test_pairwise_correlations_preserve_their_own_overlap(self):
        january, february, march = fixture()
        returns = {january: D(".01"), february: D(".02"), march: D(".03")}
        factors = {
            "partial": {january: D(".10"), february: None, march: D(".30")},
            "constant": dict.fromkeys(returns, D(".01")),
        }
        result = metrics.correlations(returns, factors)
        self.assertEqual(result["partial"], {
            "n": 2, "start": january.isoformat(), "end": march.isoformat(), "pearson_r": 1.0,
        })
        self.assertIsNone(result["constant"]["pearson_r"])
        self.assertEqual(result["constant"]["n"], 3)

    def test_regression_matches_bartlett_hac_with_small_sample_correction(self):
        rng = np.random.default_rng(42)
        count = 42
        factor_matrix = rng.normal(0, 0.04, size=(count, 6))
        design = np.column_stack((np.ones(count), factor_matrix))
        noise = rng.normal(0, 0.02, size=count + 1)
        outcome = design @ np.array([.003, .1, .2, .3, .4, .5, .6]) + noise[1:] + noise[:-1] / 2
        dates = [month_end(BASELINE, offset) for offset in range(1, count + 1)]
        rf = {day: D(index + 10) / 10_000 for index, day in enumerate(dates)}
        portfolio = {day: D(str(value)) + rf[day] for day, value in zip(dates, outcome, strict=True)}
        factor_returns = {
            name: dict(zip(dates, (D(str(value)) for value in column), strict=True))
            for name, column in zip(metrics.REGRESSION_FACTORS, factor_matrix.T, strict=True)
        }
        result = metrics.factor_regression(portfolio, rf, factor_returns)
        coefficient = np.linalg.lstsq(design, outcome, rcond=None)[0]
        scores = design * (outcome - design @ coefficient)[:, None]
        meat = scores.T @ scores
        for lag in range(1, 7):
            cross = scores[lag:].T @ scores[:-lag]
            meat += (1 - lag / 7) * (cross + cross.T)
        bread = np.linalg.inv(design.T @ design)
        covariance = bread @ meat @ bread * count / (count - design.shape[1])
        self.assertEqual((result["n"], result["newey_west_lags"], result["status"]), (count, 6, "ok"))
        self.assertAlmostEqual(result["alpha_monthly"], coefficient[0], places=13)
        self.assertAlmostEqual(result["alpha_newey_west_t"], coefficient[0] / np.sqrt(covariance[0, 0]), places=13)

    def test_flip_events_label_signal_and_holding_performance(self):
        observations = fixture()
        january, february, march = observations
        factor_returns = {"Mom": {january: D(".02"), february: D(".10"), march: D(".15")}}
        events = metrics.flip_rows(observations, factor_returns, ("Mom",))
        self.assertEqual(len(events), 2)
        self.assertEqual(
            (events[0]["signal_month"], events[0]["holding_month"], events[0]["direction"]),
            (january.isoformat(), february.isoformat(), "long_to_short"),
        )
        self.assertEqual(events[0]["signal_month_factor_contribution"], D(".02"))
        self.assertEqual(events[0]["holding_month_factor_contribution"], D("-.10"))
        self.assertEqual(events[1]["holding_month_strategy_return"], D(".25"))
        summary = metrics.flip_summary(events, ("Mom",))
        self.assertEqual(summary["Mom"]["long_to_short"]["average_holding_month_strategy_return_pct"], "-20.0000")
        del observations[february]
        self.assertEqual(metrics.flip_rows(observations, factor_returns, ("Mom",)), [])


if __name__ == "__main__":
    unittest.main()
