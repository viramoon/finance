from datetime import date
from decimal import Decimal, Inexact, ROUND_DOWN, ROUND_UP, Rounded, getcontext, localcontext
from pathlib import Path
from tempfile import TemporaryDirectory
import unittest

from fertile_finance.historical_catalog import HistoricalCatalog
from fertile_finance.research_catalog import sessions
from fertile_finance.research_factors import load_fred, load_ken_french, load_ken_french_rf
from fertile_finance.research_metrics import RiskFreeRate, RiskFreeSeries, daily_risk_free, excess_returns, sharpe_ratio


HISTORICAL_FIXTURES = Path(__file__).parent / "fixtures" / "historical"


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

    def test_daily_risk_free_aligns_to_return_end_across_holiday(self):
        wed, fri, mon, tue = (date(2024, 11, 27), date(2024, 11, 29), date(2024, 12, 2), date(2024, 12, 3))
        early_close = close_ts(fri) - 3 * 3_600_000_000_000
        closes = {tue: close_ts(tue), mon: close_ts(mon), fri: early_close, wed: close_ts(wed)}
        rf = RiskFreeSeries("Synthetic daily RF", "daily", (
            RiskFreeRate(wed, early_close + 1, Decimal("0.02")),
            RiskFreeRate(wed, close_ts(wed) + 1, Decimal("0.01")),
        ))
        self.assertEqual(daily_risk_free(closes, rf), {mon: Decimal("0.01"), tue: Decimal("0.02")})

    def test_french_archive_and_catalog_use_verified_release_times(self):
        source = HISTORICAL_FIXTURES / "french_daily.csv"
        raw = load_ken_french_rf(source, pre_epoch="drop")
        with TemporaryDirectory(dir="/tmp") as work:
            catalog = HistoricalCatalog(Path(work))
            catalog.write_french(load_ken_french(source, pre_epoch="drop"))
            stored = catalog.read_french()

        epoch_session, wed, fri, mon = (date(1970, 1, 2), date(2024, 7, 3), date(2024, 7, 5), date(2024, 7, 8))
        closes = sessions(wed, mon)
        issued = {epoch_session: close_ts(epoch_session), wed: closes[wed] + 1}
        expected = (
            RiskFreeRate(epoch_session, issued[epoch_session], Decimal("0.0003")),
            RiskFreeRate(wed, issued[wed], Decimal("0.0002")),
        )
        for batch in (raw, stored):
            with self.subTest(batch=type(batch).__name__):
                self.assertTrue(all(row.available_on is None for row in batch.rows))
                series = RiskFreeSeries.from_french(batch, available_ts=issued, source="French fixture")
                self.assertEqual((series.source, series.basis, series.rates), ("French fixture", "daily", expected))
                self.assertEqual(daily_risk_free(closes, series), {fri: Decimal("0.0003"), mon: Decimal("0.0002")})
                with self.assertRaisesRegex(
                    ValueError, "Missing verified Ken French RF availability timestamp for 2024-07-03",
                ):
                    RiskFreeSeries.from_french(batch, available_ts={epoch_session: issued[epoch_session]})

        fred = load_fred(HISTORICAL_FIXTURES / "fred_dgs10_observations.json", series_id="DGS10")
        with self.assertRaisesRegex(ValueError, "Expected a FrenchDaily or FrenchRiskFree batch"):
            RiskFreeSeries.from_french(fred, available_ts={})

    def test_rf_adapter_and_metrics_are_independent_of_caller_context(self):
        source = HISTORICAL_FIXTURES / "french_daily.csv"
        first, last = date(2024, 7, 3), date(2024, 7, 9)
        closes = sessions(first, last)
        values = map(Decimal, ("99999.99", "100123.45", "100995.43", "99987.65"))
        equity = [(day, ts, value) for (day, ts), value in zip(closes.items(), values, strict=True)]
        epoch_session = date(1970, 1, 2)
        releases = {epoch_session: close_ts(epoch_session), first: closes[first] + 1}
        annual = RiskFreeSeries("Annual fixture", "annual", (
            RiskFreeRate(first, closes[first], Decimal("0.05321456789123456789")),
        ))

        def evaluate():
            french = RiskFreeSeries.from_french(load_ken_french_rf(source, pre_epoch="drop"), available_ts=releases)
            excess = excess_returns(equity, french)
            annual_excess = excess_returns(equity, annual)
            return (french, daily_risk_free(closes, french), daily_risk_free(closes, annual),
                    excess, annual_excess, sharpe_ratio(excess), sharpe_ratio(annual_excess))

        expected = evaluate()
        for precision, rounding in ((6, ROUND_UP), (3, ROUND_DOWN)):
            with self.subTest(precision=precision, rounding=rounding), localcontext() as caller:
                caller.prec = precision
                caller.rounding = rounding
                caller.Emin = -4
                caller.Emax = 4
                caller.clamp = 1
                caller.flags[Inexact] = True
                caller.traps[Inexact] = True
                caller.traps[Rounded] = True
                before = repr(caller)
                self.assertEqual(evaluate(), expected)
                self.assertIs(getcontext(), caller)
                self.assertEqual(repr(getcontext()), before)

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
