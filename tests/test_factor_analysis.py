from dataclasses import replace
from datetime import date
from decimal import Decimal
from pathlib import Path
from tempfile import TemporaryDirectory
import unittest

import pandas as pd

from fertile_finance.factor_analysis import analyze_factors
from fertile_finance.historical_catalog import HistoricalCatalog
from fertile_finance.research_factors import load_ken_french


FIXTURES = Path(__file__).parent / "fixtures"


class FactorAnalysisTests(unittest.TestCase):
    def setUp(self):
        self.factors = pd.read_csv(FIXTURES / "factor_returns.csv", index_col="date")

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


class FrenchCatalogAnalysisTests(unittest.TestCase):
    def test_catalog_factors_and_loader_match_with_gaps_and_cutoff(self):
        fixtures = FIXTURES / "historical"
        batch = load_ken_french(
            fixtures / "french_daily.csv", momentum_path=fixtures / "french_momentum_daily.csv", pre_epoch="drop",
        )
        returns = pd.Series({
            date(1970, 1, 2): 0.01,
            date(2024, 7, 1): 0.02,
            date(2024, 7, 3): 0.03,
            date(2024, 7, 5): 0.04,
            date(2024, 7, 8): 0.05,
        })
        with TemporaryDirectory() as directory:
            catalog = HistoricalCatalog(Path(directory))
            catalog.write_french(batch)
            result = analyze_factors(returns, catalog.read_french(), as_of="2024-07-05")

        self.assertEqual(result, analyze_factors(returns, batch, as_of="2024-07-05"))
        self.assertEqual((result.sample_start, result.sample_end), (date(1970, 1, 2), date(2024, 7, 3)))
        self.assertEqual(result.observations, 2)
        self.assertEqual(result.dropped_observations, 2)
        self.assertEqual(list(result.correlations), ["Mkt-RF", "SMB", "HML", "Mom"])
        self.assertAlmostEqual(result.correlations["Mkt-RF"], 1)

    def test_french_historical_sample_excludes_late_publication_and_future_momentum(self):
        batch = load_ken_french(FIXTURES / "historical" / "french_daily.csv", pre_epoch="drop")
        first, second, incomplete = batch.rows
        late = replace(second, available_on=date(2024, 7, 9))
        future = replace(second, session=date(2024, 7, 8), mom=Decimal("0.0033"))
        batch = replace(batch, rows=(first, late, incomplete, future))
        returns = pd.Series({row.session: i / 100 for i, row in enumerate(batch.rows)})
        result = analyze_factors(returns, batch, as_of="2024-07-05")

        self.assertEqual((result.sample_start, result.sample_end), (first.session, first.session))
        self.assertEqual(result.observations, 1)
        self.assertEqual(result.dropped_observations, 2)
        self.assertEqual(result.correlations, {"Mkt-RF": None, "SMB": None, "HML": None})


if __name__ == "__main__":
    unittest.main()
