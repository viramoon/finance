"""Synthetic strategy decisions, execution and attribution with no external inputs."""

from datetime import date
from decimal import Decimal
from fractions import Fraction
import unittest

from fertile_finance.portfolio_backtest import PortfolioAsset, Price, Quantity, ResearchError, UniverseSnapshot
from fertile_finance.portfolio_inputs import Bar
from fertile_finance.research_catalog import bar_type, cash_equity, nyse
from fertile_finance.strategy_allocation import Limit, allocate
from fertile_finance.strategy_backtest import run_config_strategy_backtest
from fertile_finance.strategy_indicators import evaluate_indicator


SYMBOL = "AAA.XNYS"


def _input(closes, volumes):
    # The caller generates literal cent prices and volumes, using only XNYS
    # session timestamps from the in-process calendar. None denotes no bar.
    days = nyse().schedule.loc["2024-01-02":"2024-01-18"].iloc[:len(closes)]
    kind = bar_type(SYMBOL)
    bars = []
    for value, volume, when in zip(closes, volumes, days["close"], strict=True):
        if value is not None:
            price = Price.from_str(value)
            bars.append(Bar(kind, price, price, price, price, Quantity.from_int(volume), when.value, when.value))
    dates = tuple(label.date() for label in days.index)
    universe = (UniverseSnapshot(dates[0], int(days.iloc[0]["open"].value), [SYMBOL]),)
    return dates, (PortfolioAsset(cash_equity(SYMBOL), tuple(bars), True),), universe


def _plan(names, *, fee="0", slippage="0"):
    def component(reference, **params):
        return {"use": f"{reference}@1.0.0", "params": params}

    allocation = "1" if len(names) == 1 else "0.5"
    return {
        "plan_version": "fertile.finance/strategy-plan/v1",
        "execution": "execution.next_close_cash_equity@1.0.0",
        "sleeves": [{"recipe": key, "priority": 10, "budget_weight": allocation} for key in names],
        "bundle": {"spec": {
            "clock": {"warmup_sessions": 1},
            "policy": {"same_session_reentry": False},
            "portfolio": {"initial_cash": {"amount": "1000"}, "max_gross_weight": "1", "max_weight_per_asset": "1"},
            "execution": {"commission_bps": fee, "slippage_bps": slippage},
            "modules": {
                "sellLater": component("exit.holding_sessions", sessions=1, start_at="first_fill"),
                "volume": component("filter.complete_session_bar", require_positive_volume=False),
                "size": component("sizing.equal_weight_capped", max_weight_per_asset="1"),
                **{key: component("entry.above_when_flat", factor=key, threshold="0.1") for key in names},
            },
            "factors": {key: component("factor.close_return", lookback_sessions=i)
                        for i, key in enumerate(names, 1)},
            "recipes": {key: {"type": "entry_exit", "entry": key, "exit": "sellLater",
                              "filters": ["volume"], "risk": [], "sizing": "size"} for key in names},
        }},
    }


class StrategyBacktestTests(unittest.TestCase):
    def test_priority_budget_ratio_and_stable_whole_share_remainder(self):
        first, second, third = ("AAA.XNYS", "First"), ("AAA.XNYS", "Second"), ("BBB.XNYS", "First")
        prices = {"AAA.XNYS": Fraction(1), "BBB.XNYS": Fraction(1)}
        claims = {second: 100, first: 100}
        budgets = {"First": Fraction(1, 3), "Second": Fraction(2, 3)}
        costs = dict.fromkeys(claims, Fraction(1))
        equal, _ = allocate(claims, prices, {"First": 10, "Second": 10}, budgets,
                            [Limit("cash", Fraction(30), costs)])
        priority, _ = allocate(claims, prices, {"First": 20, "Second": 10}, budgets,
                               [Limit("cash", Fraction(30), costs)])
        self.assertEqual(equal, {first: 10, second: 20})
        self.assertEqual(priority, {first: 0, second: 30})
        claims = {third: 1, second: 1, first: 1}
        remainder, _ = allocate(claims, prices, {"First": 10, "Second": 10},
                                {"First": Fraction(1, 2), "Second": Fraction(1, 2)},
                                [Limit("cash", Fraction(1), dict.fromkeys(claims, Fraction(1)))])
        self.assertEqual(remainder, {first: 1, second: 0, third: 0})

    def test_missing_execution_defers_and_holding_clock_starts_on_fill(self):
        dates, assets, universe = _input(
            ["10.00"] * 5 + [None, "10.00"], [10, 10, 0, 10, 10, 0, 10],
        )
        def indicator(bars, reference, params):
            self.assertEqual(reference, "factor.close_return@1.0.0")
            self.assertLessEqual(bars[-1].ts_event, nyse().schedule.loc[str(dates[-1]), "close"].value)
            return Decimal(1)

        result = run_config_strategy_backtest(
            plan=_plan(("Sleeve",)), assets=assets, universe=universe,
            start=dates[1], end=dates[-1], indicator_evaluator=indicator,
        )
        first, last = result.fills
        self.assertEqual((first["side"], first["signal_date"], first["fill_date"]), ("buy", dates[1], dates[3]))
        self.assertEqual((last["side"], last["signal_date"], last["fill_date"]), ("sell", dates[4], dates[6]))
        self.assertEqual([row["execution_date"] for row in result.attribution if row["status"] == "deferred"],
                         [dates[2], dates[5]])
        self.assertIsNone(result.equity[dates[5]])
        self.assertEqual(result.cash, Decimal(1000))
        self.assertEqual(result.positions, {})
        self.assertEqual(result.sleeves[0]["positions"], {})

    def test_opposite_sleeve_intents_cross_at_close_and_native_costs_reconcile(self):
        dates, assets, universe = _input([f"{value}.00" for value in range(10, 15)], [5] * 5)

        def indicator(bars, reference, params):
            threshold_price = 11 if params["lookback_sessions"] == 1 else 13
            return Decimal(int(bars[-1].close.as_decimal() == threshold_price))

        result = run_config_strategy_backtest(
            plan=_plan(("Seller", "Buyer"), fee="100", slippage="100"), assets=assets, universe=universe,
            start=dates[1], end=dates[-1], indicator_evaluator=indicator,
        )
        self.assertEqual(result.total_orders, 2)
        crossing = {row["recipe"]: row for row in result.attribution if row.get("netted_quantity")}
        seller, buyer = crossing["Seller"], crossing["Buyer"]
        self.assertEqual(seller["netted_quantity"], buyer["netted_quantity"])
        self.assertEqual(buyer["cash_delta"], -buyer["netted_quantity"] * Decimal(14))
        self.assertEqual(buyer["commission"], 0)
        self.assertIsNone(buyer["order_id"])
        fees = sum(row.get("commission", Decimal(0)) for row in result.attribution)
        self.assertGreater(fees, 0)
        self.assertEqual(fees, sum(row["commission"] for row in result.fills))
        self.assertEqual(result.cash, sum(row["cash"] for row in result.sleeves))
        self.assertEqual(result.positions[SYMBOL], buyer["accepted_quantity"])

    def test_unsupported_prehistory_is_rejected_before_replay(self):
        dates, assets, universe = _input(["10.00", "11.00", "12.00"], [10] * 3)
        with self.assertRaises(ResearchError) as error:
            run_config_strategy_backtest(
                plan=_plan(("Sleeve",)), assets=assets, universe=universe,
                start=dates[0], end=date(2024, 1, 4),
            )
        self.assertEqual(error.exception.code, "insufficient_history")

    def test_builtin_indicator_uses_only_supplied_closed_bars(self):
        _, assets, _ = _input(["10.00", "12.00", "11.00", "14.00"], [10] * 4)
        bars = assets[0].bars
        self.assertEqual(evaluate_indicator(bars, "factor.close_return@1.0.0", {"lookback_sessions": 3}), Decimal("0.4"))
        rsi = evaluate_indicator(bars, "indicator.rsi_wilder@1.0.0", {"period": 2, "field": "close"})
        self.assertEqual(rsi.quantize(Decimal("0.01")), Decimal("88.89"))
        self.assertIsNone(evaluate_indicator(bars[:2], "indicator.rsi_wilder@1.0.0", {"period": 2, "field": "close"}))

    def test_scalar_evaluator_receives_identical_inputs_for_distinct_band_outputs(self):
        dates, assets, universe = _input(["10.00", "12.00", "11.00"], [10] * 3)
        plan = _plan(("Upper", "Lower"))
        reference = "indicator.bollinger@1.0.0"
        parameters = {"period": 2}
        plan["bundle"]["spec"]["factors"] = {
            "Upper": {"use": reference, "params": dict(parameters), "output": "upper"},
            "Lower": {"use": reference, "params": dict(parameters), "output": "lower"},
        }
        for sleeve in ("Upper", "Lower"):
            plan["bundle"]["spec"]["modules"][sleeve]["params"]["threshold"] = "12"
        calls = []

        def indicator(bars, component, params):
            calls.append((tuple(bars), component, dict(params)))
            return None

        run_config_strategy_backtest(
            plan=plan, assets=assets, universe=universe,
            start=dates[1], end=dates[-1], indicator_evaluator=indicator,
        )
        first_close = assets[0].bars[:2]
        self.assertEqual([call for call in calls if call[0] == first_close], [
            (first_close, reference, parameters),
            (first_close, reference, parameters),
        ])

        observed = {}
        result = run_config_strategy_backtest(
            plan=plan, assets=assets, universe=universe, start=dates[1], end=dates[-1],
            signal_observer=lambda session, signal: observed.update({session.day: signal.targets}),
        )
        self.assertEqual(observed[dates[1]], {(SYMBOL, "Upper"): 41})
        self.assertEqual(result.total_orders, 1)

    def test_transitive_factor_dependency_blocks_entry_until_available(self):
        dates, assets, universe = _input(["10.00"] * 3 + ["11.00"] * 3, [10] * 6)
        plan = _plan(("Sleeve",))
        spec = plan["bundle"]["spec"]
        spec["clock"]["warmup_sessions"] = 2
        spec["modules"]["Sleeve"]["params"]["threshold"] = "-0.01"
        close_return = "factor.close_return@1.0.0"
        rsi = "indicator.rsi_wilder@1.0.0"
        spec["factors"]["Sleeve"]["depends_on"] = ["intermediate"]
        spec["factors"]["intermediate"] = {
            "use": close_return, "params": {"lookback_sessions": 2}, "depends_on": ["rsi"],
        }
        spec["factors"]["rsi"] = {"use": rsi, "params": {"period": 2, "field": "close"}}
        calls = []

        def indicator(bars, reference, params):
            window = params.get("period", params.get("lookback_sessions"))
            calls.append((len(bars), reference, window))
            return evaluate_indicator(bars, reference, params)

        result = run_config_strategy_backtest(
            plan=plan, assets=assets, universe=universe,
            start=dates[2], end=dates[-1], indicator_evaluator=indicator,
        )
        self.assertEqual([(row["side"], row["signal_date"], row["fill_date"]) for row in result.fills], [
            ("buy", dates[3], dates[4]),
        ])
        self.assertEqual([call for call in calls if call[0] <= 4], [
            (3, rsi, 2), (4, rsi, 2), (4, close_return, 2), (4, close_return, 1),
        ])
        missing_factors = {(gap.date, gap.instrument_id) for gap in result.gaps if gap.kind == "missing_factor"}
        self.assertIn((dates[2], SYMBOL), missing_factors)
        self.assertNotIn((dates[3], SYMBOL), missing_factors)
        self.assertIn(rsi, result.attribution[0]["components"])
