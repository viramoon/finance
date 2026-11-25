from datetime import date
from decimal import Decimal, localcontext
import unittest

from fertile_finance.research_metrics import RiskFreeRate, RiskFreeSeries, excess_returns, sharpe_ratio


def close_ts(day):
    return ((day - date(1970, 1, 1)).days * 24 + 21) * 3_600_000_000_000


class ResearchMetrics(unittest.TestCase):
    def setUp(self):
        days = [date(2024, 1, day) for day in (2, 3, 4, 5)]
        self.equity = [(day, close_ts(day), Decimal(value))
                       for day, value in zip(days, (100, 110, 99, 99), strict=True)]

    def test_sharpe_with_daily_rf_and_default_zero_are_reproducible(self):
        first, ts, _ = self.equity[0]
        rf = RiskFreeSeries("Synthetic daily RF", "daily", (RiskFreeRate(first, ts, Decimal("0.01")),))
        self.assertEqual(excess_returns(self.equity), tuple(map(Decimal, ("0.1", "-0.1", "0"))))
        self.assertEqual(excess_returns(self.equity, rf), tuple(map(Decimal, ("0.09", "-0.11", "-0.01"))))
        self.assertEqual(sharpe_ratio(excess_returns(self.equity)), Decimal(0))
        expected = sharpe_ratio(excess_returns(self.equity, rf))
        self.assertEqual(round(expected, 12), Decimal("-1.587450786639"))
        with localcontext() as context:
            context.prec = 10
            self.assertEqual(sharpe_ratio(excess_returns(self.equity, rf)), expected)

    def test_rf_revisions_and_new_observations_are_used_only_after_release(self):
        day1, ts1, _ = self.equity[0]
        day2, ts2, _ = self.equity[1]
        day3, ts3, _ = self.equity[2]
        rf = RiskFreeSeries("Synthetic as-of RF", "daily", (
            RiskFreeRate(day1, ts3, Decimal("0.99")),  # Older observation, revised after day2 was published.
            RiskFreeRate(day2, ts3 - 1, Decimal("0.04")),
            RiskFreeRate(day1, ts1, Decimal("0.01")),
            RiskFreeRate(day1, ts1 + 1, Decimal("0.02")),
            RiskFreeRate(day3, ts3 + 1, Decimal("0.05")),
        ))
        self.assertEqual(excess_returns(self.equity, rf), tuple(map(Decimal, ("0.09", "-0.12", "-0.04"))))
        delayed = RiskFreeSeries("Late first release", "daily", (RiskFreeRate(day2, ts2, Decimal("0.01")),))
        self.assertEqual(excess_returns(self.equity, delayed), (None, Decimal("-0.11"), Decimal("-0.01")))
        self.assertIsNone(sharpe_ratio(excess_returns(self.equity, delayed)))

    def test_annual_basis_converts_to_effective_daily_rate(self):
        first, ts, _ = self.equity[0]
        with localcontext() as context:
            context.prec = 800
            annual = Decimal("1.001") ** 252 - 1
        rf = RiskFreeSeries("Synthetic annual RF", "annual", (RiskFreeRate(first, ts, annual),))
        self.assertEqual(excess_returns(self.equity, rf), tuple(map(Decimal, ("0.099", "-0.101", "-0.001"))))

    def test_valuation_gaps_and_degenerate_sharpe_remain_unavailable(self):
        day, ts, _ = self.equity[1]
        self.equity[1] = day, ts, None
        self.assertEqual(excess_returns(self.equity), (None, None, Decimal(0)))
        self.assertIsNone(sharpe_ratio(excess_returns(self.equity)))
        self.assertIsNone(sharpe_ratio((Decimal("0.01"),)))
        self.assertIsNone(sharpe_ratio((Decimal("0.01"), Decimal("0.01"))))

    def test_rejects_invalid_rf_dates_and_duplicate_releases(self):
        first, ts, _ = self.equity[0]
        next_day, _, _ = self.equity[1]
        with self.assertRaisesRegex(ValueError, "cannot precede its observation date"):
            RiskFreeRate(next_day, ts, Decimal("0.01"))
        with self.assertRaisesRegex(ValueError, "Unix nanoseconds"):
            RiskFreeRate(first, -1, Decimal("0.01"))
        with self.assertRaisesRegex(ValueError, "finite Decimal"):
            RiskFreeRate(first, ts, Decimal("NaN"))
        rate = RiskFreeRate(first, ts, Decimal("0.01"))
        with self.assertRaisesRegex(ValueError, "Duplicate RF observation"):
            RiskFreeSeries("Fixture", "daily", (rate, rate))
        with self.assertRaisesRegex(ValueError, "daily or annual"):
            RiskFreeSeries("Fixture", "weekly", (rate,))
        with self.assertRaisesRegex(ValueError, "strictly increasing"):
            excess_returns(self.equity[::-1])
        with self.assertRaisesRegex(ValueError, "UTC session date"):
            excess_returns([(next_day, ts, Decimal(100))])
