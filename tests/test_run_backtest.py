from concurrent.futures import ThreadPoolExecutor
from datetime import date
from decimal import (
    Context, Decimal, FloatOperation, Inexact, InvalidOperation, Overflow, ROUND_FLOOR,
    ROUND_HALF_EVEN, ROUND_UP, Rounded, Underflow, getcontext, localcontext,
)
from pathlib import Path
from tempfile import TemporaryDirectory
from threading import Barrier
import unittest

import pandas as pd
from nautilus_trader.model.events import OrderFilled

from fertile_finance import run_backtest
from fertile_finance.historical_catalog import HistoricalCatalog
from fertile_finance.historical_types import DailyBar, EpochDiscard
from fertile_finance.portfolio_backtest import TargetWeights, UniverseSnapshot
from fertile_finance.research_backtest import BacktestResult
from fertile_finance.research_catalog import ResearchError, nyse, sessions
from fertile_finance.research_decimal import RESEARCH_CONTEXT
from fertile_finance.research_factors import FrenchDaily, FrenchFactor
from fertile_finance.yahoo_daily import YahooDaily


ALPHA = "ALPHA.XNYS"
BETA = "BETA.XNYS"
INDEX = "MKT.INDEX"


class RunBacktestTests(unittest.TestCase):
    def setUp(self):
        directory = TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.catalog = HistoricalCatalog(Path(directory.name))
        self.start, self.end = date(2024, 11, 27), date(2024, 12, 4)
        self.closes = sessions(self.start, self.end)
        self.days = tuple(self.closes)
        schedule = nyse().schedule
        self.opens = {day: int(schedule.at[day.isoformat(), "open"].value) for day in self.days}
        self.params = dict(
            start=self.start, end=self.end, catalog=self.catalog, raw_split_free_reviewed=True,
        )

    def archive(self, instrument, quotes, *, mode="append"):
        rows = []
        for day, quote in zip(self.days, quotes, strict=True):
            if quote is None:
                rows.append(DailyBar(day, None, None, None, None, None, None, None, self.end))
            else:
                opening, closing, volume = map(Decimal, quote)
                rows.append(DailyBar(day, opening, max(opening, closing), min(opening, closing),
                                     closing, volume, None, None, self.end))
        symbol, venue = instrument.split(".")
        asset_type = "index" if venue == "INDEX" else "stock"
        self.catalog.write_yahoo(YahooDaily(
            symbol, asset_type, "USD", None if venue == "INDEX" else venue,
            self.end, tuple(rows), (), (), EpochDiscard(),
        ), mode=mode)

    def universe(self, *instruments):
        return (UniverseSnapshot(self.start, self.opens[self.start], instruments),)

    def targets(self, **weights):
        return (TargetWeights(self.start, self.closes[self.start], weights),)

    def test_callable_and_signals_match_with_realized_weights_and_honest_windows(self):
        self.archive(ALPHA, [
            ("100", "100", "100"), ("101", "103", "100"), ("102", "104", "100"),
            ("103", "105", "100"), ("105", "107", "100"),
        ])
        self.archive(BETA, [("200", "200", "100")] * len(self.days))
        instruments = [ALPHA, BETA]
        received = []
        weights = {ALPHA: Decimal("0.25"), BETA: Decimal("0.20")}

        def weights_at_close(context):
            received.append(context)
            getcontext().prec = 1
            getcontext().traps[Inexact] = True
            return weights

        params = dict(self.params, universe=self.universe(*instruments), rf=None)
        callable_result = run_backtest(weights_at_close, instruments, **params)
        signal_result = run_backtest(self.targets(**weights), instruments, **params)
        self.assertIsInstance(callable_result, BacktestResult)
        self.assertEqual(callable_result.nav, signal_result.nav)
        self.assertEqual(callable_result.targets, signal_result.targets)
        self.assertEqual(callable_result.weights, signal_result.weights)
        self.assertEqual(callable_result.windows, signal_result.windows)
        self.assertEqual([item.date for item in received], list(self.days))
        self.assertEqual(received[0].closes, {ALPHA: Decimal(100), BETA: Decimal(200)})
        self.assertEqual(received[1].closes[ALPHA], Decimal(103))
        self.assertEqual(received[0].equity, Decimal("100000.00"))
        self.assertEqual(received[0].members, frozenset(instruments))
        self.assertEqual(callable_result.weights[self.start], {})
        self.assertGreater(callable_result.weights[self.days[1]][ALPHA], Decimal(0))
        self.assertEqual(callable_result.metadata.fill_timing, "next_open")
        self.assertEqual(callable_result.metadata.costs, Decimal(0))
        self.assertEqual(callable_result.metadata.risk_free, {"source": "rf=0", "basis": "daily"})
        first = callable_result.fill_audit[0]
        self.assertEqual((first.signal_ts, first.fill_ts), (self.closes[self.start], self.opens[self.days[1]]))
        self.assertEqual(first.fill.average_price, Decimal(101))
        self.assertTrue(all(fill.signal_ts < fill.fill_ts for fill in callable_result.fill_audit))
        self.assertEqual(len(callable_result.fill_audit), callable_result.total_orders)
        self.assertEqual([row["window"] for row in callable_result.windows], ["50y", "10y", "5y", "longest_sample"])
        for row in callable_result.windows[:3]:
            self.assertEqual(row["coverage"], "insufficient")
            self.assertIsNone(row["metrics"])
            self.assertEqual((row["actual_start"], row["actual_end"], row["observations"]),
                             (self.start.isoformat(), self.end.isoformat(), len(self.days)))
        self.assertIsNotNone(callable_result.windows[-1]["metrics"])

        close_result = run_backtest(self.targets(**weights), instruments, fill_timing="next_close", costs=0, **params)
        first_close = close_result.fill_audit[0]
        self.assertEqual(close_result.metadata.fill_timing, "next_close")
        self.assertEqual(first_close.fill_ts, self.closes[self.days[1]])
        self.assertEqual(first_close.fill.average_price, Decimal(103))

    def test_benchmark_uses_its_own_coverage_and_index_is_reference_only(self):
        self.archive(ALPHA, [("100", "100", "100")] * len(self.days))
        self.archive(INDEX, [("100", "101", "0"), ("101", "102", "0"), None, None, None])
        params = dict(self.params, universe=self.universe(ALPHA), rf=None)
        result = run_backtest(self.targets(**{ALPHA: Decimal("0.25")}), [ALPHA], benchmark=INDEX, **params)
        self.assertEqual(result.metadata.benchmark, INDEX)
        self.assertEqual(result.benchmark_nav[self.start], Decimal(101))
        self.assertIsNone(result.benchmark_nav[self.end])
        self.assertEqual(result.windows[-1]["coverage"], "full")
        self.assertEqual(result.benchmark_windows[-1]["coverage"], "insufficient")
        self.assertEqual(result.benchmark_windows[-1]["observations"], 2)
        self.assertEqual(result.benchmark_windows[-1]["actual_end"], self.days[1].isoformat())
        self.assertIsNone(result.benchmark_windows[-1]["metrics"])
        equity = run_backtest(self.targets(), [ALPHA], benchmark=ALPHA, **params)
        self.assertEqual(equity.benchmark_nav[self.end], Decimal(100))
        self.assertEqual(equity.benchmark_windows[-1]["metrics"]["turnover_pct"], "0.0000")
        for enabled in (False, True):
            with self.subTest(fixture_zero_volume=enabled), self.assertRaisesRegex(ResearchError, "XNYS USD stock or ETF"):
                run_backtest(self.targets(), [INDEX], fixture_zero_volume=enabled,
                             **dict(params, universe=self.universe(INDEX)))

    def test_nonzero_costs_epoch_and_unreviewed_prices_are_rejected(self):
        params = dict(self.params, universe=self.universe(ALPHA), rf=None)
        for cost in (Decimal("0.01"), -1):
            with self.subTest(cost=cost), self.assertRaisesRegex(ResearchError, "only zero costs"):
                run_backtest(self.targets(), [ALPHA], costs=cost, **params)
        with self.assertRaisesRegex(ResearchError, "1970-01-01"):
            run_backtest(self.targets(), [ALPHA], **dict(params, start="1969-12-31"))
        with self.assertRaisesRegex(ResearchError, "split-free"):
            run_backtest(self.targets(), [ALPHA], **dict(params, raw_split_free_reviewed=False))

    def test_late_universe_and_target_rejected_and_callback_sees_current_members(self):
        self.archive(ALPHA, [("100", "100", "100")] * len(self.days))
        self.archive(BETA, [("200", "200", "100")] * len(self.days))
        first, second, *_ = self.days
        params = dict(self.params, rf=None)
        bad_universe = (UniverseSnapshot(first, self.opens[first] + 1, (ALPHA, BETA)),)
        with self.assertRaisesRegex(ResearchError, "only available after"):
            run_backtest(self.targets(), [ALPHA, BETA], universe=bad_universe, **params)
        late_target = TargetWeights(first, self.closes[first] + 1, {ALPHA: Decimal("0.5")})
        with self.assertRaisesRegex(ResearchError, "only available after"):
            run_backtest((late_target,), [ALPHA, BETA], universe=self.universe(ALPHA, BETA), **params)

        seen = []
        universe = (*self.universe(ALPHA), UniverseSnapshot(second, self.opens[second], (ALPHA, BETA)))

        def choose(context):
            seen.append(tuple(context.closes))
            return {BETA: Decimal("0.5")} if BETA in context.closes else {}

        result = run_backtest(choose, [ALPHA, BETA], universe=universe, **params)
        self.assertEqual(seen[:2], [(ALPHA,), (ALPHA, BETA)])
        self.assertEqual(result.fill_audit[0].signal_date, second)
        self.assertEqual(result.fill_audit[0].fill_date, self.days[2])

    def test_default_french_rf_requires_verified_availability_and_uses_known_rate(self):
        self.archive(ALPHA, [
            ("100", "100", "100"), ("100", "104", "100"), ("104", "103", "100"),
            ("103", "106", "100"), ("106", "108", "100"),
        ])
        rf_value = Decimal("0.0001")
        self.catalog.write_french(FrenchDaily(
            (FrenchFactor(self.start, None, None, None, rf_value),), EpochDiscard(),
        ))
        params = dict(self.params, universe=self.universe(ALPHA))
        targets = self.targets(**{ALPHA: Decimal("0.25")})
        with self.assertRaisesRegex(ResearchError, "requires rf_available_ts"):
            run_backtest(targets, [ALPHA], **params)
        known = run_backtest(targets, [ALPHA], rf_available_ts={self.start: self.closes[self.start]}, **params)
        late = run_backtest(targets, [ALPHA], rf_available_ts={self.start: self.closes[self.start] + 1}, **params)
        self.assertEqual(known.metadata.risk_free, {"source": "Ken French", "basis": "daily"})
        self.assertEqual(known.windows[-1]["rf_source"], "Ken French")
        self.assertIsNotNone(known.windows[-1]["metrics"]["sharpe"])
        self.assertIsNone(late.windows[-1]["metrics"]["sharpe"])

    def test_zero_volume_blocks_execution_and_only_actual_order_counts_for_turnover(self):
        self.archive(ALPHA, [
            ("100", "100", "100"), ("100", "100", "0"), ("100", "100", "100"),
            ("100", "100", "100"), ("100", "100", "100"),
        ])
        self.archive(BETA, [("200", "200", "100")] * len(self.days))
        result = run_backtest(
            self.targets(**{ALPHA: Decimal("0.25")}), [ALPHA, BETA],
            universe=self.universe(ALPHA, BETA), rf=None, **self.params,
        )
        self.assertIn((self.days[1], "execution_zero_volume"), [(gap.date, gap.kind) for gap in result.gaps])
        self.assertEqual(result.total_orders, 1)
        (audit,) = result.fill_audit
        self.assertEqual(audit.fill_date, self.days[3])
        self.assertEqual(audit.notional, Decimal("25000"))
        self.assertEqual(result.windows[-1]["metrics"]["turnover_pct"], "25.0000")

    def test_zero_volume_fixture_returns_one_logical_trade_with_native_cash_and_turnover(self):
        cases = [
            (0, [("1", "68.27"), ("6", "68.28")], "477.95", "68.27857142857142857142857143",
             "99522.05", "99999.94", "0.4780"),
            (100, [("7", "68.27")], "477.89", "68.27", "99522.11", "100000.00", "0.4779"),
        ]
        targets = self.targets(**{ALPHA: Decimal("0.0048")})
        params = dict(self.params, universe=self.universe(ALPHA), rf=None, end=self.days[1])
        for volume, parts, notional, price, cash, nav, turnover in cases:
            self.archive(ALPHA, [("68.27", "68.27", str(volume))] * len(self.days),
                         mode="replace" if volume else "append")
            if volume == 0:
                default = run_backtest(targets, [ALPHA], **params)
                self.assertFalse(default.metadata.fixture_zero_volume)
                self.assertEqual((default.total_orders, default.fill_audit, default.positions), (0, (), {}))
                self.assertTrue(all(value is None for value in default.nav.values()))
            for timing, expected_ts in (("next_open", self.opens[self.days[1]]),
                                        ("next_close", self.closes[self.days[1]])):
                with self.subTest(volume=volume, timing=timing):
                    result = run_backtest(targets, [ALPHA], **params, fixture_zero_volume=True, fill_timing=timing)
                    self.assertTrue(result.metadata.fixture_zero_volume)
                    self.assertEqual(result.total_orders, 1)
                    (trade,) = result.fill_audit
                    fill = trade.fill
                    self.assertEqual(trade.fill_ts, expected_ts)
                    self.assertLess(trade.signal_ts, trade.fill_ts)
                    self.assertEqual(len(fill.raw_fills), len(parts))
                    self.assertEqual([(part.quantity, part.price) for part in fill.parts],
                                     [tuple(map(Decimal, part)) for part in parts])
                    self.assertTrue(all(part.fill_ts == expected_ts for part in fill.parts))
                    self.assertEqual(fill.quantity, Decimal(7))
                    self.assertEqual(fill.average_price, Decimal(price))
                    self.assertEqual(fill.notional, Decimal(notional))
                    self.assertEqual(fill.cash_delta, Decimal(notional).copy_negate())
                    self.assertEqual(result.cash, Decimal(cash))
                    self.assertEqual(result.nav[self.days[1]], Decimal(nav))
                    self.assertEqual(result.positions, {ALPHA: Decimal(7)})
                    self.assertEqual(result.windows[-1]["metrics"]["turnover_pct"], turnover)
                    print(f"fixture volume={volume} timing={timing} raw={len(fill.raw_fills)} "
                          f"logical={len(result.fill_audit)} qty={fill.quantity} notional={fill.notional} "
                          f"cash={result.cash} positions={result.positions} turnover_pct={turnover}")

    def test_fixture_continuation_preflights_cash_and_rejects_zero_sell_price(self):
        self.archive(ALPHA, [("1000", "1000", "0")] * len(self.days))
        params = dict(self.params, universe=self.universe(ALPHA), rf=None, fixture_zero_volume=True, end=self.days[1])
        funded = run_backtest(self.targets(**{ALPHA: Decimal(1)}), [ALPHA], **params)
        self.assertEqual(funded.positions, {ALPHA: Decimal(99)})
        self.assertEqual(funded.cash, Decimal("999.02"))
        self.assertIn("execution_cash", [gap.kind for gap in funded.gaps])

        second, third = self.days[1:3]
        flat = TargetWeights(second, self.closes[second], {})
        targets = (*self.targets(**{ALPHA: Decimal("0.07")}), flat)
        sold = run_backtest(targets, [ALPHA], **dict(params, end=third))
        self.assertEqual(sold.positions, {})
        self.assertEqual(sold.cash, Decimal("99999.88"))
        self.assertEqual([(part.quantity, part.price) for part in sold.fill_audit[-1].fill.parts],
                         [(Decimal(1), Decimal("1000")), (Decimal(6), Decimal("999.99"))])

        self.archive(ALPHA, [("1000", "1000", "0"), ("1000", "1000", "0"),
                             ("0.01", "0.01", "0"), ("1000", "1000", "0"), ("1000", "1000", "0")],
                     mode="replace")
        with self.assertRaisesRegex(ResearchError, "fixture continuation price must be positive"):
            run_backtest(targets, [ALPHA], **dict(params, end=third))

    def test_benchmark_and_native_split_results_are_independent_of_decimal_context(self):
        self.archive(ALPHA, [("68.27", "68.27", "0")] * len(self.days))
        self.archive(INDEX, [("100", "100", "0"), None, ("101", "101", "0"), None, None])
        end = self.days[2]
        params = dict(self.params, universe=self.universe(ALPHA), rf=None, benchmark=INDEX,
                      end=end, fixture_zero_volume=True)
        targets = self.targets(**{ALPHA: Decimal("0.0048")})
        factors = pd.DataFrame(
            {key: [Decimal("0.01"), Decimal("0.02")] for key in ("Mkt-RF", "SMB", "HML")},
            index=self.days[1:3],
        )

        def run_in_context(caller, barrier=None):
            with localcontext(caller) as context:
                context.flags[Rounded] = True
                before = str(context)
                if barrier is not None:
                    barrier.wait(timeout=10)
                result = run_backtest(targets, [ALPHA], **params)
                (trade,) = result.fill_audit
                comparisons = (result, repr(result), trade.fill.average_price, trade.fill.cash_delta,
                               result.analyze_factors(factors))
                self.assertIs(getcontext(), context)
                self.assertEqual(str(context), before)
                # The native event UUID is random. Compare the rest of its payload,
                # including trade ID, timestamps, quantities, price and commission.
                with localcontext(RESEARCH_CONTEXT):
                    raw = tuple(OrderFilled.to_dict(event) for event in trade.fill.raw_fills)
                for event in raw:
                    del event["event_id"]
                return (*comparisons, raw)

        standard = run_in_context(Context(prec=28, rounding=ROUND_HALF_EVEN))
        window = standard[0].benchmark_windows[-1]
        self.assertEqual((window["actual_years"], window["missing_pct"]), ("0.0137", "33.3333"))
        self.assertEqual(standard[2], Decimal("68.27857142857142857142857143"))
        callers = (
            Context(prec=2, rounding=ROUND_UP, Emin=-2, Emax=2, traps=[]),
            Context(prec=3, rounding=ROUND_FLOOR, Emin=-3, Emax=3, clamp=1,
                    traps=[Inexact, Rounded, FloatOperation, InvalidOperation, Overflow, Underflow]),
        )
        for caller in callers:
            with self.subTest(prec=caller.prec, rounding=caller.rounding):
                self.assertEqual(run_in_context(caller), standard)
        barrier = Barrier(2)
        with ThreadPoolExecutor(max_workers=2) as pool:
            jobs = [pool.submit(run_in_context, caller, barrier) for caller in callers]
            for job in jobs:
                self.assertEqual(job.result(timeout=30), standard)

    def test_result_factor_analysis_does_not_bridge_missing_nav(self):
        self.archive(ALPHA, [
            ("100", "100", "100"), ("100", "105", "100"), None,
            ("105", "108", "100"), ("108", "109", "100"),
        ])
        result = run_backtest(
            self.targets(**{ALPHA: Decimal("0.5")}), [ALPHA], universe=self.universe(ALPHA), rf=None, **self.params,
        )
        factors = pd.DataFrame({key: range(len(self.days)) for key in ("Mkt-RF", "SMB", "HML")}, index=self.days)
        self.assertIsNone(result.nav[self.days[2]])
        self.assertIsNone(result.weights[self.days[2]])
        analysis = result.analyze_factors(factors)
        self.assertEqual((analysis.sample_start, analysis.sample_end), (self.days[1], self.end))
        self.assertEqual((analysis.observations, analysis.dropped_observations), (2, 2))
        early = result.analyze_factors(factors, as_of=self.days[3])
        self.assertEqual((early.sample_end, early.observations), (self.days[1], 1))


if __name__ == "__main__":
    unittest.main()
