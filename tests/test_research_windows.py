from datetime import date
from decimal import Decimal
import unittest

import exchange_calendars

from fertile_finance.research_catalog import sessions
from fertile_finance.research_windows import coverage_windows


END = date(2026, 11, 20)


def equity_fixture(start):
    return {day: Decimal(100 + n) for n, day in enumerate(sessions(start, END))}


class ResearchWindowTests(unittest.TestCase):
    def test_spy_like_sample_reports_four_honest_windows(self):
        sample = equity_fixture(date(1993, 1, 29))
        rows = coverage_windows(sample)
        self.assertEqual([row["window"] for row in rows], ["50y", "10y", "5y", "longest_sample"])
        self.assertEqual([row["coverage"] for row in rows], ["insufficient", "full", "full", "full"])
        fifty, ten, five, longest = rows
        self.assertEqual([row["requested_start"] for row in rows[:3]],
                         ["1976-11-20", "2016-11-20", "2021-11-20"])
        self.assertEqual(fifty["label"], "50y (insufficient)")
        self.assertIsNone(fifty["metrics"])
        self.assertEqual((fifty["actual_start"], fifty["actual_end"]), ("1993-01-29", "2026-11-20"))
        self.assertEqual(fifty["actual_years"], "33.8077")
        self.assertEqual(fifty["observations"], len(sample))
        self.assertEqual(fifty["observations"] + fifty["missing_sessions"], fifty["expected_sessions"])
        self.assertGreater(Decimal(fifty["missing_pct"]), 0)
        self.assertEqual([row["rf_source"] for row in rows], ["rf=0"] * 4)
        self.assertEqual([ten["actual_start"], five["actual_start"]], ["2016-11-21", "2021-11-22"])
        self.assertIsNone(longest["requested_years"])
        self.assertEqual(longest["requested_start"], "1993-01-29")
        self.assertIsNotNone(longest["metrics"]["cagr_pct"])
        print("SPY-like fixture:", [(row["label"], row["actual_years"], row["observations"],
                                     row["missing_sessions"], row["expected_sessions"], row["missing_pct"])
                                    for row in rows])

    def test_long_history_has_full_windows_and_starts_after_epoch(self):
        sample = equity_fixture(date(1970, 1, 1))
        rows = coverage_windows(sample)
        self.assertEqual([row["coverage"] for row in rows], ["full"] * 4)
        self.assertEqual(rows[0]["requested_start"], "1976-11-20")
        self.assertEqual(rows[0]["actual_start"], "1976-11-22")
        self.assertEqual(rows[0]["actual_end"], "2026-11-20")
        self.assertEqual(rows[0]["missing_sessions"], 0)
        self.assertEqual(rows[-1]["actual_start"], "1970-01-02")
        sample[date(1969, 12, 31)] = Decimal(99)
        with self.assertRaisesRegex(ValueError, "from 1970-01-01"):
            coverage_windows(sample)
        print("Long fixture:", [(row["label"], row["actual_start"], row["actual_end"], row["observations"])
                                for row in rows])

    def test_exact_boundary_internal_and_trailing_gaps(self):
        sample = equity_fixture(date(1976, 11, 20))
        days = list(sample)
        for day in days[:5]:
            del sample[day]
        full = coverage_windows(sample)[0]
        self.assertEqual((full["coverage"], full["missing_sessions"]), ("full", 5))
        del sample[days[5]]
        short = coverage_windows(sample)[0]
        self.assertEqual((short["coverage"], short["missing_sessions"]), ("insufficient", 6))
        self.assertIsNone(short["metrics"])
        del sample[date(2024, 7, 5)]
        self.assertTrue(all(row["coverage"] == "insufficient" and row["metrics"] is None
                            for row in coverage_windows(sample)))

        sample = equity_fixture(date(2021, 11, 20))
        del sample[END]
        five, longest = coverage_windows(sample, END)[-2:]
        self.assertEqual((five["coverage"], five["missing_sessions"]), ("insufficient", 1))
        self.assertEqual((longest["coverage"], longest["missing_sessions"]), ("full", 0))
        self.assertEqual(longest["actual_end"], "2026-11-19")

    def test_pre_epoch_request_retains_original_date_and_counting_boundary(self):
        sample = {day: Decimal(100) for day in sessions(date(1970, 1, 1), date(2010, 1, 4))}
        row = coverage_windows(sample)[0]
        self.assertEqual(row["requested_start"], "1960-01-04")
        self.assertEqual(row["calendar_start"], "1970-01-01")
        self.assertEqual(row["coverage"], "insufficient")
        self.assertEqual(row["missing_sessions"], 0)
        self.assertIsNone(row["metrics"])

    def test_decimal_metrics_rf_and_turnover_use_return_dates(self):
        dates = [date(2026, 11, day) for day in (18, 19, 20)]
        sample = dict(zip(dates, map(Decimal, ("100", "110", "99")), strict=True))
        zeros = coverage_windows(sample)[-1]
        rf = {dates[1]: Decimal(".01"), dates[2]: Decimal("-.10")}
        turnover = dict(zip(dates, map(Decimal, ("9", ".10", ".20")), strict=True))
        row = coverage_windows(sample, rf=rf, rf_source="fixture RF", turnover=turnover)[-1]
        metrics = row["metrics"]
        self.assertEqual(zeros["rf_source"], "rf=0")
        self.assertEqual(zeros["metrics"]["sharpe"], "0.0000")
        self.assertIsNone(zeros["metrics"]["turnover_pct"])
        self.assertEqual(row["rf_source"], "fixture RF")
        self.assertEqual(metrics["sharpe"], format(Decimal(126).sqrt(), ".4f"))
        self.assertEqual(metrics["return_pct"], "-1.0000")
        self.assertEqual(metrics["max_drawdown_pct"], "10.0000")
        self.assertEqual(metrics["volatility_pct"], format(Decimal("5.04").sqrt() * 100, ".4f"))
        self.assertEqual(metrics["turnover_pct"], "30.0000")
        self.assertEqual(metrics["cagr_pct"], format((Decimal(".99") ** Decimal("182.62125") - 1) * 100,
                                                     ".4f"))
        del rf[dates[2]]
        del turnover[dates[2]]
        incomplete = coverage_windows(sample, rf=rf, turnover=turnover)[-1]
        self.assertEqual(incomplete["rf_source"], "provided")
        self.assertEqual(incomplete["coverage"], "full")
        self.assertIsNone(incomplete["metrics"]["sharpe"])
        self.assertIsNone(incomplete["metrics"]["turnover_pct"])
        self.assertEqual(incomplete["metrics"]["volatility_pct"], metrics["volatility_pct"])

    def test_calendar_selection_and_empty_observations(self):
        sample = {date(2026, 11, 25): Decimal(100), date(2026, 11, 26): Decimal(101)}
        with self.assertRaisesRegex(ValueError, "not a XNYS session"):
            coverage_windows(sample)
        longest = coverage_windows(sample, calendar="XLON")[-1]
        self.assertEqual((longest["coverage"], longest["observations"]), ("full", 2))
        self.assertIsNone(longest["metrics"]["sharpe"])
        self.assertIsNone(longest["metrics"]["volatility_pct"])
        empty = coverage_windows({}, END)[-1]
        self.assertEqual((empty["coverage"], empty["observations"]), ("insufficient", 0))
        self.assertIsNone(empty["metrics"])

    def test_bounded_calendars_return_all_rows_with_only_unsupported_windows_insufficient(self):
        for name, available_start in (("XSHG", "1990-12-03"), ("XBOM", "1997-01-01"),
                                      ("XTKS", "1997-01-01")):
            with self.subTest(calendar=name):
                calendar = exchange_calendars.get_calendar(name, start="2014-06-01", end="2024-06-05")
                sample = {day.date(): Decimal(100 + i) for i, day in enumerate(calendar.sessions)}
                fifty, ten, five, longest = coverage_windows(sample, calendar=name)
                self.assertEqual(fifty["requested_start"], "1974-06-05")
                self.assertEqual(fifty["calendar_start"], available_start)
                self.assertEqual(fifty["calendar_end"], "2024-06-05")
                self.assertEqual(fifty["coverage"], "insufficient")
                self.assertEqual(fifty["coverage_reason"], "calendar_out_of_range")
                self.assertEqual(fifty["actual_start"], str(min(sample)))
                self.assertEqual(fifty["actual_end"], "2024-06-05")
                self.assertEqual(fifty["observations"], len(sample))
                self.assertEqual(fifty["expected_sessions"] - len(sample), fifty["missing_sessions"])
                self.assertIsNone(fifty["metrics"])
                for row in (ten, five, longest):
                    self.assertEqual(row["coverage"], "full")
                    self.assertEqual(row["missing_sessions"], 0)
                    self.assertIsNotNone(row["metrics"]["cagr_pct"])
                print(f"{name} calendar starts {available_start}: "
                      f"50y={fifty['coverage']}, 10y={ten['coverage']}, "
                      f"5y={five['coverage']}, longest_sample={longest['coverage']}")

    def test_calendar_boundary_gap_is_never_counted_as_a_trading_day_tolerance(self):
        end = date(1995, 12, 2)
        calendar = exchange_calendars.get_calendar("XSHG", start="1990-12-03", end=str(end))
        sample = {day.date(): Decimal(100) for day in calendar.sessions}
        _, _, five, longest = coverage_windows(sample, end, calendar="XSHG")
        self.assertEqual((five["requested_start"], five["calendar_start"]), ("1990-12-02", "1990-12-03"))
        self.assertEqual(five["missing_sessions"], 0)
        self.assertEqual(five["coverage_reason"], "calendar_out_of_range")
        self.assertEqual(five["coverage"], "insufficient")
        self.assertIsNone(five["metrics"])
        self.assertEqual(longest["coverage"], "full")

        sample[date(1990, 12, 2)] = Decimal(99)
        longest = coverage_windows(sample, end, calendar="XSHG")[-1]
        self.assertEqual(longest["actual_start"], "1990-12-02")
        self.assertEqual(longest["observations"], len(sample))
        self.assertEqual(longest["coverage"], "insufficient")
        self.assertIsNone(longest["metrics"])

    def test_calendar_can_end_before_cutoff_or_start_after_entire_request(self):
        calendar = exchange_calendars.get_calendar("XSHG")
        end = calendar.bound_max().date()
        sample = {day.date(): Decimal(100) for day in calendar.sessions[-3:]}
        rows = coverage_windows(sample, date(end.year + 1, 1, 1), calendar="XSHG")
        for row in rows[:3]:
            self.assertEqual(row["calendar_end"], str(end))
            self.assertEqual(row["coverage_reason"], "calendar_out_of_range")
            self.assertEqual(row["coverage"], "insufficient")
            self.assertIsNone(row["metrics"])
        self.assertEqual(rows[-1]["coverage"], "full")

        rows = coverage_windows({}, date(1985, 1, 1), calendar="XSHG")
        self.assertEqual(len(rows), 4)
        for row in rows[:3]:
            self.assertEqual(row["calendar_start"], "1990-12-03")
            self.assertEqual(row["coverage_reason"], "calendar_out_of_range")
            self.assertEqual(row["expected_sessions"], 0)
            self.assertIsNone(row["missing_pct"])
            self.assertIsNone(row["metrics"])
