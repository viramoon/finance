from datetime import date
from pathlib import Path
import unittest

import pandas as pd

from fertile_finance.factor_analysis import analyze_factors


class FactorAnalysisTests(unittest.TestCase):
    def setUp(self):
        fixture = Path(__file__).parent / "fixtures" / "french_daily.csv"
        self.factors = pd.read_csv(fixture, index_col="date")

    def test_complete_cases_use_common_sample_without_forward_fill(self):
        market = self.factors["Mkt-RF"]
        returns = (market * 2).drop(index="2024-01-05").iloc[::-1]
        returns.loc["2024-01-03"] = -0.8
        factors = self.factors.drop(index="2024-01-03")
        factors.loc["2024-01-12", "Mom"] = None
        result = analyze_factors(returns, factors)

        self.assertEqual(result.sample_start, date(2024, 1, 2))
        self.assertEqual(result.sample_end, date(2024, 1, 11))
        self.assertEqual(result.observations, 6)
        self.assertEqual(result.dropped_observations, 3)
        self.assertEqual(list(result.correlations), ["Mkt-RF", "SMB", "HML", "Mom"])
        self.assertAlmostEqual(result.correlations["Mkt-RF"], 1.0)
        self.assertIsNone(result.regression_status)

    def test_historical_cutoff_ignores_later_observations(self):
        cutoff = "2024-01-10"
        returns = self.factors["Mkt-RF"].copy()
        market = self.factors.copy()
        returns.loc["2024-01-11":] = 0.99
        market.loc["2024-01-11":, "Mkt-RF"] = -0.99

        result = analyze_factors(returns, market, as_of=cutoff, regression=True)
        prefix = analyze_factors(
            returns.loc[:cutoff], self.factors.loc[:cutoff], as_of=cutoff, regression=True,
        )
        self.assertEqual(result, prefix)
        self.assertEqual(result.as_of, date(2024, 1, 10))
        self.assertEqual(result.sample_end, date(2024, 1, 10))
        self.assertEqual(result.observations, 7)
        self.assertEqual(result.regression_status, "ok")
        self.assertAlmostEqual(result.correlations["Mkt-RF"], 1.0)

    def test_optional_ols_reports_intercept_loadings_and_r_squared(self):
        factors = self.factors
        returns = 0.001 + 1.5 * factors["Mkt-RF"] - 2 * factors["SMB"] + 0.5 * factors["Mom"]
        returns.index = pd.to_datetime(returns.index)
        result = analyze_factors(returns, factors, regression=True)

        self.assertEqual(result.regression_status, "ok")
        self.assertEqual(result.observations, 9)
        self.assertEqual(result.dropped_observations, 0)
        self.assertAlmostEqual(result.regression.intercept, 0.001)
        for name, expected in {"Mkt-RF": 1.5, "SMB": -2, "HML": 0, "Mom": 0.5}.items():
            self.assertAlmostEqual(result.regression.coefficients[name], expected)
        self.assertAlmostEqual(result.regression.r_squared, 1)

    def test_undefined_statistics_and_optional_momentum(self):
        factors = self.factors.drop(columns=["Mom"])
        returns = factors["Mkt-RF"]
        empty = analyze_factors(returns, factors, as_of="2024-01-01", regression=True)
        self.assertEqual(empty.observations, 0)
        self.assertIsNone(empty.sample_start)
        self.assertIsNone(empty.sample_end)
        self.assertEqual(empty.correlations, {"Mkt-RF": None, "SMB": None, "HML": None})
        self.assertEqual(empty.regression_status, "insufficient_observations")

        constant = analyze_factors(returns * 0, factors, regression=True)
        self.assertTrue(all(value is None for value in constant.correlations.values()))
        self.assertEqual(constant.regression_status, "constant_returns")
        deficient = analyze_factors(returns, factors.assign(HML=factors["SMB"]), regression=True)
        self.assertIsNone(deficient.regression)
        self.assertEqual(deficient.regression_status, "rank_deficient")

    def test_rejects_ambiguous_dates_missing_columns_and_infinite_values(self):
        market = self.factors["Mkt-RF"]
        timestamped = market.set_axis(pd.to_datetime(market.index, utc=True))
        with self.assertRaisesRegex(ValueError, "timezone-naive"):
            analyze_factors(timestamped, self.factors)
        with self.assertRaisesRegex(ValueError, "dates must be unique"):
            analyze_factors(pd.concat([market, market.iloc[:1]]), self.factors)
        with self.assertRaisesRegex(ValueError, "Missing factor columns: HML"):
            analyze_factors(market, self.factors.drop(columns=["HML"]))
        with self.assertRaisesRegex(ValueError, "finite or missing"):
            analyze_factors(market, self.factors.assign(SMB=float("inf")))


if __name__ == "__main__":
    unittest.main()
