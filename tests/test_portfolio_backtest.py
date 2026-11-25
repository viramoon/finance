from dataclasses import replace
from datetime import date, datetime, timezone
from decimal import Decimal
import unittest

from nautilus_trader.model.data import Bar
from nautilus_trader.model.objects import Price, Quantity

from fertile_finance.portfolio_backtest import (
    FactorValue,
    PortfolioAsset,
    PortfolioConfig,
    TargetWeights,
    UniverseSnapshot,
    run_portfolio_backtest,
)
from fertile_finance.research_catalog import ResearchError, bar_type, cash_equity, nyse, sessions
from fertile_finance.research_metrics import RiskFreeRate, RiskFreeSeries, excess_returns, sharpe_ratio


A = "ALPHA.XNYS"
B = "BETA.XNYS"
C = "GAMMA.XNYS"


def utc_ns(value):
    elapsed = datetime.fromisoformat(value) - datetime(1970, 1, 1, tzinfo=timezone.utc)
    return (elapsed.days * 86_400 + elapsed.seconds) * 10**9


class PortfolioBacktestTests(unittest.TestCase):
    def setUp(self):
        self.start = date(2024, 11, 27)
        self.end = date(2024, 12, 6)
        self.closes = sessions(self.start, self.end)
        self.days = tuple(self.closes)
        self.opens = {day: int(nyse().schedule.loc[str(day), "open"].value) for day in self.days}
        self.assets = {symbol: self.asset(symbol, offset) for offset, symbol in enumerate((A, B, C))}
        self.universe = (self.snapshot(self.start, A, B),)
        self.targets = tuple(
            TargetWeights(day, self.closes[day], {A: Decimal("0.45"), B: Decimal("0.45")})
            for day in self.days
        )

    def asset(self, symbol, offset, closes=None):
        instrument = cash_equity(symbol)
        kind = bar_type(symbol)
        bars = []
        for index, ts in enumerate((self.closes if closes is None else closes).values()):
            open_price = 100 + 10 * offset + index * 3
            close_price = open_price + (2 if index % 2 == 0 else -1)
            prices = [open_price, max(open_price, close_price) + 1,
                      min(open_price, close_price) - 1, close_price]
            bars.append(Bar(kind, *(Price.from_str(f"{price}.00") for price in prices),
                            Quantity.from_int(100_000), ts, ts))
        return PortfolioAsset(instrument, tuple(bars), raw_split_free_reviewed=True)

    def constant_price_asset(self, symbol, value):
        price = Price.from_str(value)
        volume = Quantity.from_int(100_000)
        kind = bar_type(symbol)
        bars = tuple(Bar(kind, price, price, price, price, volume, ts, ts)
                     for ts in self.closes.values())
        return PortfolioAsset(cash_equity(symbol), bars, raw_split_free_reviewed=True)

    def snapshot(self, day, *symbols):
        return UniverseSnapshot(day, self.opens[day], symbols)

    def factors(self, scores):
        return tuple(
            FactorValue(day, self.closes[day], symbol, Decimal(score))
            for day, row in zip(self.days, scores, strict=True)
            for symbol, score in row.items()
        )

    def run_targets(self, **overrides):
        inputs = dict(assets=(self.assets[A], self.assets[B]), universe=self.universe,
                      targets=self.targets, start=self.start, end=self.end)
        inputs.update(overrides)
        return run_portfolio_backtest(**inputs)

    def run_factors(self, scores, **overrides):
        inputs = dict(assets=tuple(self.assets.values()), universe=self.universe,
                      factors=self.factors(scores), top_n=1, start=self.start, end=self.end)
        inputs.update(overrides)
        return run_portfolio_backtest(**inputs)

    def test_two_equities_in_one_engine_with_honest_windows(self):
        result = self.run_targets()
        self.assertEqual(result.fill_timing, "next_open")
        self.assertEqual({fill.instrument_id for fill in result.fills}, {A, B})
        self.assertEqual(result.total_orders, len(result.fills))
        self.assertEqual(len({fill.order_id for fill in result.fills}), len(result.fills))
        self.assertEqual(result.total_positions, 2)
        self.assertEqual(set(result.positions), {A, B})
        self.assertTrue(all(isinstance(quantity, Decimal) and quantity > 0 for quantity in result.positions.values()))
        self.assertEqual(tuple(result.equity), self.days)
        self.assertEqual(result.equity[self.start], PortfolioConfig().initial_cash)
        self.assertTrue(all(isinstance(value, Decimal) for value in result.equity.values()))
        self.assertNotEqual(result.equity[self.end], result.equity[self.start])
        self.assertEqual([row["requested_years"] for row in result.windows], [50, 10, 5])
        for row in result.windows:
            self.assertEqual(row["coverage"], "insufficient")
            self.assertIsNone(row["metrics"])
            self.assertEqual(row["actual_start"], self.start.isoformat())
            self.assertEqual(row["actual_end"], self.end.isoformat())
            self.assertEqual(row["expected_sessions"] - row["missing_sessions"], len(self.days))
        self.assertIsNotNone(result.longest_sample["metrics"])

    def test_sparse_final_bars_cannot_fill_fifty_year_window(self):
        start, first_bar, end = date(1975, 9, 15), date(2025, 9, 12), date(2025, 9, 15)
        closes = sessions(first_bar, end)
        first_open = int(nyse().schedule.loc[str(start), "open"].value)
        result = run_portfolio_backtest(
            assets=(self.asset(A, 0, closes), self.asset(B, 1, closes)),
            universe=(UniverseSnapshot(start, first_open, (A, B)),),
            targets=(TargetWeights(first_bar, closes[first_bar], {A: Decimal("0.50")}),),
            start=start, end=end,
        )

        self.assertIsNone(result.equity[start])
        self.assertEqual(tuple(day for day, value in result.equity.items() if value is not None),
                         (first_bar, end))
        self.assertEqual({fill.instrument_id for fill in result.fills}, {A})
        self.assertEqual([row["requested_years"] for row in result.windows], [50, 10, 5])
        for row in result.windows:
            self.assertEqual(row["coverage"], "insufficient")
            self.assertEqual(row["observations"], 2)
            self.assertEqual(row["missing_sessions"], row["expected_sessions"] - 2)
            self.assertEqual(row["actual_start"], str(first_bar))
            self.assertEqual(row["actual_end"], str(end))
            self.assertIsNone(row["metrics"])
        self.assertEqual(result.longest_sample["actual_start"], str(first_bar))
        self.assertEqual(result.longest_sample["observations"], 2)

    def test_cash_only_session_needs_a_real_bar_from_an_active_member(self):
        index = 3
        missing_day = self.days[index]
        alpha, beta = self.assets[A], self.assets[B]
        bar = beta.bars[index]
        zero = Bar(bar.bar_type, bar.open, bar.high, bar.low, bar.close,
                   Quantity.from_int(0), bar.ts_event, bar.ts_init)
        assets = (
            replace(alpha, bars=(*alpha.bars[:index], *alpha.bars[index + 1:])),
            replace(beta, bars=(*beta.bars[:index], zero, *beta.bars[index + 1:])),
            self.assets[C],  # This instrument has a print, but is outside the universe.
        )
        for fill_missing in (False, True):
            with self.subTest(fill_missing=fill_missing):
                config = PortfolioConfig(fill_missing=fill_missing)
                result = self.run_targets(
                    assets=assets, config=config,
                    targets=(TargetWeights(self.start, self.closes[self.start], {}),),
                )
                self.assertEqual(result.total_orders, 0)
                self.assertEqual(result.equity[self.start], config.initial_cash)
                self.assertIsNone(result.equity[missing_day])
                self.assertEqual(result.equity[self.end], config.initial_cash)
                report = result.longest_sample
                self.assertEqual(report["observations"], len(self.days) - 1)
                self.assertEqual(report["missing_sessions"], 1)
                self.assertEqual(report["coverage"], "insufficient")
                self.assertIsNone(report["metrics"])

    def test_rf_is_used_for_full_window_and_full_sample_sharpe(self):
        start, end = date(2019, 11, 27), date(2024, 11, 29)
        closes = sessions(start, end)
        starts_at = int(nyse().schedule.loc[str(start), "open"].value)
        portfolio = dict(
            assets=(self.asset(A, 0, closes), self.asset(B, 1, closes)),
            universe=(UniverseSnapshot(start, starts_at, (A, B)),),
            targets=(TargetWeights(start, closes[start], {A: Decimal("0.40"), B: Decimal("0.40")}),),
            start=start, end=end, config=PortfolioConfig(rebalance_every=21),
        )
        rf = RiskFreeSeries("Synthetic daily RF", "daily", (RiskFreeRate(start, closes[start], Decimal("0.001")),))
        default = run_portfolio_backtest(**portfolio)
        supplied = run_portfolio_backtest(**portfolio, rf=rf)
        self.assertEqual(default.equity, supplied.equity)
        self.assertEqual(default.risk_free, {"source": "rf=0", "basis": "daily"})
        self.assertEqual(supplied.risk_free, {"source": rf.source, "basis": rf.basis})
        self.assertEqual([row["coverage"] for row in supplied.windows], ["insufficient", "insufficient", "full"])
        self.assertEqual([row["metrics"] for row in supplied.windows[:2]], [None, None])
        for base, with_rate in ((default.longest_sample, supplied.longest_sample),
                                (default.windows[2], supplied.windows[2])):
            with self.subTest(window=with_rate["requested_start"]):
                window_start = date.fromisoformat(with_rate["actual_start"])
                rows = [(day, ts, supplied.equity[day]) for day, ts in closes.items() if day >= window_start]
                self.assertEqual(with_rate["observations"], len(rows))
                self.assertEqual(with_rate["risk_free"], supplied.risk_free)
                self.assertEqual(base["risk_free"], default.risk_free)
                self.assertEqual(base["metrics"]["sharpe"], sharpe_ratio(excess_returns(rows)))
                self.assertEqual(with_rate["metrics"]["sharpe"], sharpe_ratio(excess_returns(rows, rf)))
                self.assertNotEqual(base["metrics"]["sharpe"], with_rate["metrics"]["sharpe"])
                self.assertIsInstance(with_rate["metrics"]["turnover"], Decimal)
                self.assertGreater(with_rate["metrics"]["turnover"], 0)

    def test_fills_use_next_session_open_or_close_after_the_signal(self):
        # Thanksgiving intervenes and Friday closes early.
        friday = date(2024, 11, 29)
        self.assertEqual(self.days[1], friday)
        for timing, expected_ns in (
            ("next_open", utc_ns("2024-11-29T14:30:00+00:00")),
            ("next_close", utc_ns("2024-11-29T18:00:00+00:00")),
        ):
            with self.subTest(timing=timing):
                result = self.run_targets(config=PortfolioConfig(fill_timing=timing))
                self.assertEqual(result.fill_timing, timing)
                first_fills = [fill for fill in result.fills if fill.signal_date == self.start]
                self.assertEqual({fill.instrument_id for fill in first_fills}, {A, B})
                self.assertTrue(all(fill.fill_ts == expected_ns for fill in first_fills))
                for fill in result.fills:
                    index = self.days.index(fill.signal_date) + 1
                    self.assertEqual(fill.fill_date, self.days[index])
                    self.assertEqual(fill.signal_ts, self.closes[fill.signal_date])
                    self.assertLess(fill.signal_ts, fill.fill_ts)
                    bar = self.assets[fill.instrument_id].bars[index]
                    if timing == "next_open":
                        self.assertEqual(fill.fill_ts, self.opens[fill.fill_date])
                        self.assertEqual(fill.price, bar.open.as_decimal())
                    else:
                        self.assertEqual(fill.fill_ts, self.closes[fill.fill_date])
                        self.assertEqual(fill.price, bar.close.as_decimal())

    def test_next_open_ignores_later_prices_and_positive_volume_magnitude(self):
        original = self.assets[A]
        bar = original.bars[1]
        changed = Bar(bar.bar_type, bar.open, Price.from_str("210.00"), Price.from_str("75.00"),
                      Price.from_str("190.00"), Quantity.from_int(1), bar.ts_event, bar.ts_init)
        variant = replace(original, bars=(original.bars[0], changed, *original.bars[2:]))
        baseline = self.run_targets()
        result = self.run_targets(assets=(variant, self.assets[B]))

        def first_opening_fills(report):
            return sorted((fill.instrument_id, fill.side, fill.quantity, fill.price, fill.fill_ts)
                          for fill in report.fills if fill.signal_date == self.start)

        self.assertEqual(len(first_opening_fills(result)), 2)
        self.assertEqual(first_opening_fills(baseline), first_opening_fills(result))
        self.assertNotEqual(baseline.equity[self.days[1]], result.equity[self.days[1]])

    def test_native_notional_cent_mismatch_is_rejected_before_submission(self):
        assets = (self.constant_price_asset(A, "129160.17"), self.constant_price_asset(B, "24.00"))
        targets = (TargetWeights(self.start, self.closes[self.start], {A: Decimal(1)}),)
        for timing in ("next_open", "next_close"):
            with self.subTest(timing=timing):
                unsafe = PortfolioConfig(fill_timing=timing, rebalance_every=100,
                                         initial_cash=Decimal("16541339673797.42"))
                with self.assertRaisesRegex(
                    ResearchError, r"128068426 shares.*exact notional 16541339673792\.42 USD, native 16541339673792\.43 USD",
                ) as caught:
                    self.run_targets(assets=assets, targets=targets, config=unsafe)
                self.assertEqual(caught.exception.code, "native_cash_precision")

                # A nearby, larger notional reconciles exactly through the native path.
                safe = replace(unsafe, initial_cash=Decimal("16541339932117.76"))
                result = self.run_targets(assets=assets, targets=targets, config=safe)
                self.assertEqual(result.total_orders, 1)
                self.assertEqual(result.positions, {A: Decimal(128_068_428)})
                fill, = result.fills
                self.assertEqual(fill.quantity * fill.price, Decimal("16541339932112.76"))
                self.assertEqual(result.equity[self.end] - fill.quantity * fill.price, Decimal("5.00"))

    def test_sale_checks_projected_native_cash_range_before_submission(self):
        alpha = self.constant_price_asset(A, "10.00")
        old = alpha.bars[2]
        price = Price.from_str("17014118346045.00")
        expensive = Bar(old.bar_type, price, price, price, price, old.volume, old.ts_event, old.ts_init)
        alpha = replace(alpha, bars=(*alpha.bars[:2], expensive, *alpha.bars[3:]))
        targets = (
            TargetWeights(self.start, self.closes[self.start], {A: Decimal("0.10")}),
            TargetWeights(self.days[1], self.closes[self.days[1]], {}),
        )
        with self.assertRaisesRegex(ResearchError, r"projected cash 17014118346135\.00 USD.*native balance range") as caught:
            self.run_targets(assets=(alpha, self.assets[B]), targets=targets,
                             config=PortfolioConfig(initial_cash=Decimal("100.00")))
        self.assertEqual(caught.exception.code, "native_cash_precision")

    def test_rebalance_frequency_counts_sessions_and_drift_can_block_orders(self):
        result = self.run_targets(config=PortfolioConfig(rebalance_every=2))
        self.assertEqual([event.date for event in result.rebalances], list(self.days[::2]))
        self.assertTrue(result.fills)
        self.assertTrue(all(fill.signal_date in self.days[::2] for fill in result.fills))

        blocked = self.run_targets(config=PortfolioConfig(weight_drift_threshold=Decimal("0.50")))
        self.assertEqual(blocked.total_orders, 0)
        self.assertFalse(blocked.fills)
        self.assertEqual(set(blocked.equity.values()), {PortfolioConfig().initial_cash})

    def test_equal_weight_thirds_buy_one_share_per_instrument(self):
        symbols = (A, B, C)
        result = self.run_factors(
            [{A: 3, B: 2, C: 1}] * len(self.days),
            assets=tuple(self.constant_price_asset(symbol, "100.00") for symbol in symbols),
            universe=(self.snapshot(self.start, *symbols),),
            top_n=3,
            config=PortfolioConfig(initial_cash=Decimal("300.00")),
        )
        expected = dict.fromkeys(symbols, Decimal(1))
        self.assertEqual(result.positions, expected)
        self.assertEqual(result.total_orders, 3)
        self.assertEqual({fill.instrument_id: fill.quantity for fill in result.fills}, expected)
        self.assertTrue(all(fill.side == "buy" and fill.price == Decimal("100.00") for fill in result.fills))

    def test_whole_share_floor_preserves_precise_target_weight(self):
        assets = tuple(self.constant_price_asset(symbol, "10.00") for symbol in (A, B))
        for weight, shares in ((Decimal("0.099999999999999999999999999999999"), 0),
                               (Decimal("0.10"), 1)):
            with self.subTest(weight=weight):
                result = self.run_targets(
                    assets=assets,
                    targets=(TargetWeights(self.start, self.closes[self.start], {A: weight}),),
                    config=PortfolioConfig(initial_cash=Decimal("100.00")),
                )
                self.assertEqual(result.total_orders, shares)
                self.assertEqual(result.positions, {A: Decimal(shares)} if shares else {})

    def test_cross_section_rotates_only_through_point_in_time_members(self):
        exit_day = self.days[3]
        history = (*self.universe, self.snapshot(exit_day, B, C))
        scores = [
            {A: 9, B: 1, C: 99},
            {A: 1, B: 9, C: 99},
            {A: 1, B: 9, C: 99},
            *({A: 99, B: 1, C: 9} for _ in self.days[3:]),
        ]
        result = self.run_factors(scores, universe=history)
        selected = [{symbol for symbol, weight in rebalance.targets.items() if weight > 0}
                    for rebalance in result.rebalances]
        self.assertEqual(selected, [{A}, {B}, {B}, *({C} for _ in self.days[3:])])
        self.assertEqual({fill.instrument_id for fill in result.fills}, {A, B, C})
        for fill in result.fills:
            if fill.instrument_id == A:
                self.assertLess(fill.fill_date, exit_day)
            elif fill.instrument_id == C:
                self.assertGreaterEqual(fill.signal_date, exit_day)
        self.assertEqual(set(result.positions), {C})

    def test_pending_order_cannot_fill_after_member_exit(self):
        exit_day = self.days[1]
        history = (*self.universe, self.snapshot(exit_day, B, C))
        result = self.run_factors([{A: 99, B: 1, C: 9} for _ in self.days], universe=history)
        self.assertEqual(result.rebalances[0].targets[A], Decimal(1))
        self.assertTrue(result.fills)
        self.assertTrue(all(fill.instrument_id != A for fill in result.fills))
        self.assertTrue(any(gap.date == exit_day and gap.instrument_id == A and gap.kind == "execution_inactive"
                            for gap in result.gaps))

    def test_held_member_exit_leaves_an_unvalued_position(self):
        exit_day = self.days[2]
        history = (*self.universe, self.snapshot(exit_day, B, C))
        result = self.run_factors([{A: 99, B: 1, C: 9} for _ in self.days], universe=history)
        self.assertGreater(result.positions[A], 0)
        self.assertTrue(any(fill.instrument_id == A and fill.fill_date < exit_day for fill in result.fills))
        self.assertFalse(any(fill.instrument_id == A and fill.fill_date >= exit_day for fill in result.fills))
        self.assertIsNone(result.equity[exit_day])
        self.assertTrue(any(gap.date == exit_day and gap.instrument_id == A and gap.kind == "valuation_inactive"
                            for gap in result.gaps))
        self.assertIsNone(result.longest_sample["metrics"])

    def test_end_only_universe_and_late_signal_inputs_fail(self):
        with self.assertRaises(ValueError):
            self.run_targets(universe=(self.snapshot(self.end, A, B),))
        with self.assertRaises(ValueError):
            self.run_targets(universe=(replace(self.universe[0], available_ts=self.opens[self.start] + 1),))
        with self.assertRaises(ValueError):
            self.run_targets(targets=(replace(self.targets[0], available_ts=self.closes[self.start] + 1),
                                      *self.targets[1:]))

        scores = [{A: 9, B: 1} for _ in self.days]
        factors = self.factors(scores)
        with self.assertRaises(ValueError):
            self.run_factors(scores, factors=(replace(factors[0], available_ts=self.closes[self.start] + 1),
                                              *factors[1:]))

        alpha = self.assets[A]
        bar = alpha.bars[0]
        late = Bar(bar.bar_type, bar.open, bar.high, bar.low, bar.close, bar.volume, bar.ts_event, bar.ts_init + 1)
        with self.assertRaises(ValueError):
            self.run_targets(assets=(replace(alpha, bars=(late, *alpha.bars[1:])), self.assets[B]))
        with self.assertRaises(ValueError):
            self.run_targets(assets=(replace(alpha, raw_split_free_reviewed=False), self.assets[B]))

    def test_missing_held_close_has_no_default_synthetic_valuation_or_signal(self):
        missing_day = self.days[2]
        alpha = self.assets[A]
        sparse = replace(alpha, bars=(*alpha.bars[:2], *alpha.bars[3:]))
        result = self.run_targets(assets=(sparse, self.assets[B]))
        self.assertFalse(result.fill_missing)
        self.assertTrue(any(fill.instrument_id == A and fill.fill_date < missing_day for fill in result.fills))
        self.assertTrue(any(gap.date == missing_day and gap.instrument_id == A and gap.kind == "missing_price"
                            and gap.filled_from is None for gap in result.gaps))
        self.assertIsNone(result.equity[missing_day])
        self.assertFalse(any(fill.instrument_id == A and missing_day in (fill.fill_date, fill.signal_date)
                             for fill in result.fills))
        self.assertTrue(all(rebalance.targets.get(A, 0) == 0 for rebalance in result.rebalances
                            if rebalance.date == missing_day))
        self.assertEqual(result.longest_sample["observations"], len(self.days) - 1)
        self.assertIsNone(result.longest_sample["metrics"])

        filled = self.run_targets(assets=(sparse, self.assets[B]), config=PortfolioConfig(fill_missing=True))
        self.assertTrue(filled.fill_missing)
        self.assertIsNotNone(filled.equity[missing_day])
        self.assertFalse(any(fill.instrument_id == A and fill.fill_date == missing_day for fill in filled.fills))
        self.assertTrue(any(gap.date == missing_day and gap.instrument_id == A and gap.kind == "valuation_price"
                            and gap.filled_from == self.days[1] for gap in filled.gaps))

    def test_padded_valuation_keeps_five_year_window_incomplete(self):
        start, end = date(2019, 7, 12), date(2024, 7, 12)
        missing_day = date(2024, 7, 9)
        closes = sessions(start, end)
        starts_at = int(nyse().schedule.loc[str(start), "open"].value)
        alpha = self.asset(A, 0, closes)
        sparse = replace(alpha, bars=tuple(bar for bar in alpha.bars if bar.ts_event != closes[missing_day]))
        portfolio = dict(
            assets=(sparse, self.asset(B, 1, closes)),
            universe=(UniverseSnapshot(start, starts_at, (A, B)),),
            targets=(TargetWeights(start, closes[start], {A: Decimal("0.40"), B: Decimal("0.40")}),),
            start=start, end=end,
        )
        config = PortfolioConfig(rebalance_every=len(closes))
        default = run_portfolio_backtest(**portfolio, config=config)
        padded = run_portfolio_backtest(**portfolio, config=replace(config, fill_missing=True))

        self.assertIn(A, padded.positions)
        self.assertIsNone(default.equity[missing_day])
        self.assertIsNotNone(padded.equity[missing_day])
        self.assertIn((missing_day, A, "valuation_price", date(2024, 7, 8)),
                      [(gap.date, gap.instrument_id, gap.kind, gap.filled_from) for gap in padded.gaps])
        for result in (default, padded):
            window = result.windows[2]
            self.assertEqual(window["requested_years"], 5)
            for row in (window, result.longest_sample):
                self.assertEqual(row["coverage"], "insufficient")
                self.assertEqual(row["missing_sessions"], 1)
                self.assertEqual(row["observations"], len(closes) - 1)
                self.assertIsNone(row["metrics"])

    def test_missing_factor_is_excluded_unless_explicitly_filled_from_the_past(self):
        missing_day = self.days[1]
        scores = [{A: 9, B: 1}, {B: 1}, *({A: 9, B: 1} for _ in self.days[2:])]
        default = self.run_factors(scores)
        filled = self.run_factors(scores, config=PortfolioConfig(fill_missing=True))
        self.assertFalse(default.fill_missing)
        self.assertTrue(filled.fill_missing)

        def target_at(result):
            return next(rebalance.targets for rebalance in result.rebalances if rebalance.date == missing_day)

        self.assertEqual(target_at(default).get(A, 0), 0)
        self.assertEqual(target_at(default)[B], Decimal(1))
        self.assertEqual(target_at(filled)[A], Decimal(1))
        for result, filled_from in ((default, None), (filled, self.start)):
            self.assertTrue(any(gap.date == missing_day and gap.instrument_id == A and gap.kind == "missing_factor"
                                and gap.filled_from == filled_from for gap in result.gaps))

    def test_next_execution_requires_a_raw_print_even_with_padding(self):
        alpha = self.assets[A]
        bar = alpha.bars[1]
        zero = Bar(bar.bar_type, bar.open, bar.high, bar.low, bar.close,
                   Quantity.from_int(0), bar.ts_event, bar.ts_init)
        for second_bar, reason in (((), "execution_price"), ((zero,), "execution_zero_volume")):
            with self.subTest(reason=reason):
                variant = replace(alpha, bars=(alpha.bars[0], *second_bar, *alpha.bars[2:]))
                result = self.run_targets(
                    assets=(variant, self.assets[B]),
                    config=PortfolioConfig(fill_missing=True, rebalance_every=3),
                )
                self.assertTrue(any(fill.instrument_id == B and fill.signal_date == self.start
                                    for fill in result.fills))
                self.assertFalse(any(fill.instrument_id == A and fill.signal_date == self.start
                                     for fill in result.fills))
                self.assertTrue(any(gap.date == self.days[1] and gap.instrument_id == A
                                    and gap.kind == reason and gap.filled_from is None for gap in result.gaps))


if __name__ == "__main__":
    unittest.main()
