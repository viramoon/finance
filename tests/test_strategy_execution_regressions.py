"""Regressions for allocation causes and exact settlement in the configured worker."""

from datetime import date
from decimal import Decimal
from fractions import Fraction
from types import SimpleNamespace
import unittest

from fertile_finance.portfolio_backtest import PortfolioConfig, Price
from fertile_finance.strategy_allocation import Limit, allocate
from fertile_finance.strategy_backtest import (
    _CashEquityCosts, _ConfiguredStrategy, _Intent, run_config_strategy_backtest,
)

from test_strategy_backtest import SYMBOL, _input, _plan


class StrategyExecutionRegressionTests(unittest.TestCase):
    def test_cash_cause_survives_later_shared_asset_exhaustion(self):
        buyer_a, buyer_b = (SYMBOL, "A"), (SYMBOL, "B")
        filled, reasons = allocate(
            {buyer_a: 4, buyer_b: 5}, {SYMBOL: Fraction(1)},
            {"A": 10, "B": 10}, {"A": Fraction(1, 2), "B": Fraction(1, 2)},
            [Limit("portfolio_asset_limit", Fraction(6), {buyer_a: Fraction(1), buyer_b: Fraction(1)}),
             Limit("sleeve_cash", Fraction(1), {buyer_a: Fraction(1)})],
        )
        self.assertEqual(filled, {buyer_a: 1, buyer_b: 5})
        self.assertEqual(reasons, {buyer_a: "sleeve_cash"})

    def test_full_cross_with_exact_cash_does_not_place_residual_sell(self):
        dates, assets, universe = _input(["10.00", "10.00", "8.00", "10.00", "10.00"], [100] * 5)
        plan = _plan(("Seller", "Buyer"), fee="100", slippage="1000")
        plan["bundle"]["spec"]["portfolio"]["initial_cash"]["amount"] = "200"
        first_signal = assets[0].bars[1].ts_event
        second_signal = assets[0].bars[3].ts_event

        def indicator(bars, _reference, params):
            expected = first_signal if params["lookback_sessions"] == 1 else second_signal
            return Decimal(int(bars[-1].ts_event == expected))

        result = run_config_strategy_backtest(
            plan=plan, assets=assets, universe=universe, start=dates[1], end=dates[-1],
            indicator_evaluator=indicator,
        )
        self.assertEqual(len(result.fills), 1)
        initial = result.fills[0]
        self.assertEqual((initial["side"], initial["quantity"], initial["commission"]),
                         ("buy", Decimal(10), Decimal("0.88")))
        transfer = {row["recipe"]: row for row in result.attribution if row["execution_date"] == dates[-1]}
        self.assertEqual(transfer.keys(), {"Seller", "Buyer"})
        self.assertEqual(transfer["Seller"]["cash_delta"], Decimal(100))
        self.assertEqual(transfer["Buyer"]["cash_delta"], Decimal(-100))
        self.assertEqual(transfer["Buyer"]["requested_quantity"], Decimal(10))
        for row in transfer.values():
            self.assertEqual(row["status"], "accepted")
            self.assertEqual(row["reason"], "internal_netting")
            self.assertEqual(row["netted_quantity"], Decimal(10))
            self.assertEqual(row["external_quantity"], Decimal(0))
            self.assertEqual(row["execution_price"], Decimal("10.00"))
            self.assertEqual(row["commission"], Decimal(0))
            self.assertEqual(row["slippage_cost"], Decimal(0))
            self.assertIsNone(row["order_id"])
        self.assertEqual(result.positions, {SYMBOL: Decimal(10)})
        self.assertEqual(result.cash, Decimal("111.12"))

    def test_external_fee_is_included_in_the_same_fill_weight_limit(self):
        dates, assets, universe = _input(["10.00"] * 3, [100] * 3)
        plan = _plan(("Sleeve",), fee="100")
        portfolio = plan["bundle"]["spec"]["portfolio"]
        portfolio["max_gross_weight"] = portfolio["max_weight_per_asset"] = "0.5"
        result = run_config_strategy_backtest(
            plan=plan, assets=assets, universe=universe, start=dates[1], end=dates[-1],
            indicator_evaluator=lambda *_: Decimal(1),
        )
        fill, = result.fills
        self.assertEqual(fill["quantity"], Decimal(49))
        self.assertEqual(fill["commission"], Decimal("4.90"))
        self.assertEqual(result.equity[dates[-1]], Decimal("995.10"))
        self.assertLessEqual(result.positions[SYMBOL] * Decimal(10), result.equity[dates[-1]] / 2)
        row, = [row for row in result.attribution if row["execution_date"] == dates[-1]]
        self.assertEqual((row["status"], row["reason"], row["requested_quantity"], row["accepted_quantity"]),
                         ("clipped", "portfolio_gross_limit", Decimal(100), Decimal(49)))

    def test_sale_fee_blocks_buy_when_untouched_asset_would_exceed_limit(self):
        session = date(2024, 1, 3)
        instrument_a, instrument_b, instrument_c = SYMBOL, "BBB.XNYS", "CCC.XNYS"
        sell, buy = (instrument_b, "Seller"), (instrument_c, "Buyer")
        plan = _plan(("Owner", "Seller", "Buyer"), fee="100")
        for sleeve in plan["sleeves"]:
            sleeve["budget_weight"] = "0.5" if sleeve["recipe"] == "Owner" else "0.25"
        spec = plan["bundle"]["spec"]
        spec["portfolio"]["max_weight_per_asset"] = "0.5"
        costs = _CashEquityCosts(spec["execution"])
        price = Price.from_str("10.00")
        bar = SimpleNamespace(close=price)
        inputs = SimpleNamespace(
            bars={session: dict.fromkeys((instrument_a, instrument_b, instrument_c), bar)},
            members={session: {instrument_a, instrument_b, instrument_c}}, closes={session: 1},
        )
        strategy = _ConfiguredStrategy(inputs, PortfolioConfig(initial_cash=Decimal(220)), plan, session, None, costs)
        strategy.cash = Decimal(90)
        strategy.shares.update({instrument_a: 11, instrument_b: 2})
        strategy.sleeves["Owner"].shares[instrument_a] = 11
        strategy.sleeves["Owner"].cash = Decimal(0)
        strategy.sleeves["Seller"].shares[instrument_b] = 2
        strategy.sleeves["Seller"].cash = Decimal(10)
        strategy.sleeves["Buyer"].cash = Decimal(80)
        quote = SimpleNamespace(bid_price=price, ask_price=price)
        strategy.quotes.update({instrument_b: quote, instrument_c: quote})

        fills, orders, reasons = strategy._choose_executions(
            session, {sell: _Intent(-2, session, 0, "recipe_exit"), buy: _Intent(2, session, 0, "recipe_entry")},
            dict.fromkeys((instrument_a, instrument_b, instrument_c), Decimal(10)),
        )
        self.assertEqual(fills.keys(), {sell})
        self.assertEqual(orders.keys(), {instrument_b})
        self.assertEqual(orders[instrument_b].commission, Decimal("0.20"))
        self.assertEqual(reasons, {buy: "portfolio_asset_limit"})

    def test_cash_clip_releases_external_budget_to_other_internal_matches(self):
        session = date(2024, 1, 3)
        aaa, bbb, ccc = SYMBOL, "BBB.XNYS", "CCC.XNYS"
        closes = dict.fromkeys((aaa, bbb, ccc), Decimal(10))
        plan = _plan(("Seller", "Buyer"), fee="10000", slippage="10000")
        price = Price.from_str("10.00")
        inputs = SimpleNamespace(
            bars={session: dict.fromkeys(closes, SimpleNamespace(close=price))},
            members={session: set(closes)}, closes={session: 1},
        )
        costs = _CashEquityCosts(plan["bundle"]["spec"]["execution"])
        strategy = _ConfiguredStrategy(inputs, PortfolioConfig(initial_cash=Decimal(1000)), plan, session, None, costs)
        strategy.cash = Decimal(800)
        strategy.shares.update({aaa: 10, ccc: 10})
        strategy.sleeves["Seller"].shares.update(strategy.shares)
        strategy.sleeves["Seller"].cash = Decimal(760)
        strategy.sleeves["Buyer"].cash = Decimal(40)
        bid, ask = costs.prices(price)
        strategy.quotes.update(dict.fromkeys(closes, SimpleNamespace(bid_price=bid, ask_price=ask)))
        requested = {(aaa, "Seller"): -10, (ccc, "Seller"): -10,
                     (aaa, "Buyer"): 21, (bbb, "Buyer"): 8, (ccc, "Buyer"): 21}
        fills, orders, reasons = strategy._choose_executions(
            session, {key: _Intent(qty, session, 0, "test") for key, qty in requested.items()}, closes,
        )

        # The $40 wallet can buy four $10 matched shares. A $20 external
        # share plus its $20 fee would consume the same cash on its own.
        self.assertEqual({key: fill.quantity for key, fill in fills.items() if key[1] == "Buyer"},
                         {(aaa, "Buyer"): 2, (ccc, "Buyer"): 2})
        self.assertEqual({key: order.quantity for key, order in orders.items()}, {aaa: -8, ccc: -8})
        self.assertEqual(sum(fill.cash_delta for key, fill in fills.items() if key[1] == "Buyer"), Decimal(-40))
        self.assertEqual(reasons[bbb, "Buyer"], "sleeve_cash")

    def test_post_cost_risk_preserves_cross_while_rejecting_outside_buy(self):
        session = date(2024, 1, 3)
        aaa, bbb, ccc = SYMBOL, "BBB.XNYS", "CCC.XNYS"
        closes = dict.fromkeys((aaa, bbb, ccc), Decimal(10))
        plan = _plan(("Seller", "Earlier", "Later"), slippage="1000")
        for sleeve, budget, priority in zip(plan["sleeves"], ("0.82", "0.08", "0.1"), (10, 10, 20), strict=True):
            sleeve.update(budget_weight=budget, priority=priority)
        spec = plan["bundle"]["spec"]
        spec["portfolio"]["max_weight_per_asset"] = "0.5"
        price = Price.from_str("10.00")
        inputs = SimpleNamespace(
            bars={session: dict.fromkeys(closes, SimpleNamespace(close=price))},
            members={session: set(closes)}, closes={session: 1},
        )
        costs = _CashEquityCosts(spec["execution"])
        strategy = _ConfiguredStrategy(inputs, PortfolioConfig(initial_cash=Decimal(400)), plan, session, None, costs)
        strategy.cash = Decimal(80)
        strategy.shares.update({aaa: 20, bbb: 12})
        strategy.sleeves["Seller"].shares.update(strategy.shares)
        for recipe, amount in (("Seller", 0), ("Earlier", 30), ("Later", 50)):
            strategy.sleeves[recipe].cash = Decimal(amount)
        bid, ask = costs.prices(price)
        strategy.quotes.update(dict.fromkeys(closes, SimpleNamespace(bid_price=bid, ask_price=ask)))
        matched_sell, matched_buy, outside_buy = (bbb, "Seller"), (bbb, "Earlier"), (ccc, "Later")
        candidates = {key: _Intent(qty, session, 0, "test") for key, qty in (
            (matched_sell, -3), (matched_buy, 3), (outside_buy, 1),
        )}

        fills, orders, reasons = strategy._choose_executions(session, candidates, closes)
        self.assertEqual(orders, {})
        self.assertEqual({key: fill.quantity for key, fill in fills.items()}, {matched_sell: -3, matched_buy: 3})
        self.assertEqual(reasons, {outside_buy: "portfolio_asset_limit"})

    def test_internal_shares_are_split_by_priority_and_same_tier_budget(self):
        session = date(2024, 1, 3)
        plan = _plan(("Seller", "BuyerA", "BuyerB"), slippage="1000")
        for sleeve, budget in zip(plan["sleeves"], ("0.3", "0.2", "0.5"), strict=True):
            sleeve["budget_weight"] = budget
        inputs = SimpleNamespace(closes={session: 1})
        costs = _CashEquityCosts(plan["bundle"]["spec"]["execution"])
        strategy = _ConfiguredStrategy(inputs, PortfolioConfig(initial_cash=Decimal(400)), plan, session, None, costs)
        price = Price.from_str("10.00")
        bid, ask = costs.prices(price)
        strategy.quotes[SYMBOL] = SimpleNamespace(bid_price=bid, ask_price=ask)
        sell, buyer_a, buyer_b = (SYMBOL, "Seller"), (SYMBOL, "BuyerA"), (SYMBOL, "BuyerB")
        quantities = {sell: -10, buyer_a: 10, buyer_b: 10}
        closes = {SYMBOL: Decimal(10)}

        fills, orders = strategy._project_fills(quantities, closes)
        self.assertEqual((fills[buyer_a].netted, fills[buyer_b].netted), (3, 7))
        self.assertEqual((fills[buyer_a].external, fills[buyer_b].external), (7, 3))
        self.assertEqual(orders[SYMBOL].quantity, 10)
        strategy.priorities["BuyerA"] = 9
        fills, _ = strategy._project_fills(quantities, closes)
        self.assertEqual((fills[buyer_a].netted, fills[buyer_b].netted), (10, 0))
