"""Pinned t2 settlement cases using synthetic cent prices and virtual sleeve balances."""

from datetime import date
from decimal import Decimal
from types import SimpleNamespace
import unittest

from fertile_finance.portfolio_backtest import PortfolioConfig, Price
from fertile_finance.strategy_backtest import _CashEquityCosts, _ConfiguredStrategy, _Intent

from test_strategy_backtest import SYMBOL, _plan


SECOND = "BBB.XNYS"
SESSION = date(2024, 1, 3)


def make_strategy(plan, close_text, cash, holdings):
    closes = {instrument: Decimal(text) for instrument, text in close_text.items()}
    bars = {instrument: SimpleNamespace(close=Price.from_str(text))
            for instrument, text in close_text.items()}
    inputs = SimpleNamespace(bars={SESSION: bars}, members={SESSION: set(bars)}, closes={SESSION: 1})
    cash_total = sum((Decimal(amount) for amount in cash.values()), Decimal(0))
    marked_holdings = sum(closes[instrument] * shares for (_, instrument), shares in holdings.items())
    costs = _CashEquityCosts(plan["bundle"]["spec"]["execution"])
    strategy = _ConfiguredStrategy(
        inputs, PortfolioConfig(initial_cash=cash_total + marked_holdings), plan, SESSION, None, costs,
    )
    strategy.cash = cash_total
    for recipe, amount in cash.items():
        strategy.sleeves[recipe].cash = Decimal(amount)
    for (recipe, instrument), shares in holdings.items():
        strategy.sleeves[recipe].shares[instrument] = shares
        strategy.shares[instrument] += shares
    for instrument, bar in bars.items():
        bid, ask = costs.prices(bar.close)
        strategy.quotes[instrument] = SimpleNamespace(bid_price=bid, ask_price=ask)
    return strategy, closes


def choose(strategy, closes, requested):
    candidates = {key: _Intent(quantity, SESSION, 0, "test") for key, quantity in requested.items()}
    return strategy._choose_executions(SESSION, candidates, closes)


class StrategyPriorityRegressionTests(unittest.TestCase):
    def test_lower_priority_slippage_cannot_cut_admitted_higher_priority_shares(self):
        plan = _plan(("Earlier", "Later"), slippage="1000")
        plan["sleeves"][0]["priority"] = 1
        plan["sleeves"][1]["priority"] = 2
        plan["bundle"]["spec"]["portfolio"]["max_weight_per_asset"] = "0.5"
        strategy, closes = make_strategy(plan, {SYMBOL: "0.01", SECOND: "0.10"},
                                         {"Earlier": "0.50", "Later": "0.50"}, {})
        earlier, later = (SYMBOL, "Earlier"), (SECOND, "Later")

        fills, orders, reasons = choose(strategy, closes, {later: 1, earlier: 50})
        self.assertEqual({key: fill.quantity for key, fill in fills.items()}, {earlier: 50})
        self.assertEqual(reasons, {later: "portfolio_asset_limit"})
        order, = orders.values()
        self.assertEqual(set(orders), {SYMBOL})
        self.assertEqual((order.quantity, order.price.as_decimal(), order.commission),
                         (50, Decimal("0.01"), Decimal("0.00")))
        self.assertEqual(fills[earlier].cash_delta, Decimal("-0.50"))

        # At four cents the same slippage rounds to zero. The lower tier can
        # use the residual capacity without moving the earlier admission.
        strategy, closes = make_strategy(plan, {SYMBOL: "0.01", SECOND: "0.04"},
                                         {"Earlier": "0.50", "Later": "0.50"}, {})
        fills, orders, reasons = choose(strategy, closes, {earlier: 50, later: 1})
        self.assertEqual({key: fill.quantity for key, fill in fills.items()}, {earlier: 50, later: 1})
        self.assertEqual({key: order.price.as_decimal() for key, order in orders.items()}, closes)
        self.assertEqual(reasons, {})

    def test_same_tier_cash_clips_are_repriced_after_internal_liquidity_is_released(self):
        plan = _plan(("A", "B", "Seller"), slippage="8000")
        for sleeve, budget in zip(plan["sleeves"], ("0.45", "0.45", "0.10"), strict=True):
            sleeve["budget_weight"] = budget
        strategy, closes = make_strategy(plan, {SYMBOL: "10.00", SECOND: "10.00"},
                                         {"A": "100.00", "B": "0.00", "Seller": "0.00"},
                                         {("Seller", SYMBOL): 10, ("B", SECOND): 8})
        buy_a, buy_b = (SYMBOL, "A"), (SYMBOL, "B")
        sell_a, sell_b = (SYMBOL, "Seller"), (SECOND, "B")

        fills, orders, reasons = choose(strategy, closes, {buy_a: 10, buy_b: 8, sell_a: -10, sell_b: -8})
        self.assertEqual({key: fill.quantity for key, fill in fills.items()},
                         {buy_a: 9, buy_b: 1, sell_a: -10, sell_b: -8})
        self.assertEqual(reasons, {buy_a: "sleeve_cash", buy_b: "sleeve_cash"})
        self.assertEqual({key: (fill.netted, fill.external) for key, fill in fills.items()},
                         {buy_a: (9, 0), buy_b: (1, 0), sell_a: (10, 0), sell_b: (0, 8)})
        self.assertEqual(set(orders), {SECOND})
        order = orders[SECOND]
        self.assertEqual((order.quantity, order.price.as_decimal(), order.commission),
                         (-8, Decimal("2.00"), Decimal("0.00")))
        balances = {recipe: sleeve.cash for recipe, sleeve in strategy.sleeves.items()}
        for (_, recipe), fill in fills.items():
            balances[recipe] += fill.cash_delta
        self.assertEqual(balances, {"A": Decimal("10.00"), "B": Decimal("6.00"), "Seller": Decimal("100.00")})
        # At the $216 post-cost mark, the A and B gross caps are $97.20 each.
        post_cost_equity = sum(balances.values()) + 10 * closes[SYMBOL]
        self.assertEqual(post_cost_equity, Decimal("216.00"))
        for buyer in (buy_a, buy_b):
            self.assertLessEqual(fills[buyer].quantity * closes[SYMBOL], post_cost_equity * Decimal("0.45"))

    def test_cash_affordability_recomputes_fee_for_each_candidate_order_size(self):
        plan = _plan(("Sleeve",), fee="40", slippage="10000")
        buy = (SYMBOL, "Sleeve")
        # Two dollars covers both asks, with a one-cent shortfall from the fee alone.
        for cash in ("1.00", "2.00"):
            with self.subTest(cash=cash):
                strategy, closes = make_strategy(plan, {SYMBOL: "0.50"}, {"Sleeve": cash}, {})
                fills, orders, reasons = choose(strategy, closes, {buy: 2})
                self.assertEqual({key: fill.quantity for key, fill in fills.items()}, {buy: 1})
                self.assertEqual(reasons, {buy: "sleeve_cash"})
                self.assertEqual(set(orders), {SYMBOL})
                order = orders[SYMBOL]
                self.assertEqual((order.quantity, order.price.as_decimal(), order.commission),
                                 (1, Decimal("1.00"), Decimal("0.00")))
                self.assertEqual(fills[buy].cash_delta, Decimal("-1.00"))

    def test_incremental_net_order_fee_is_paid_by_the_tier_that_adds_it(self):
        plan = _plan(("Earlier", "Later"), fee="40")
        plan["sleeves"][0]["priority"] = 1
        plan["sleeves"][1]["priority"] = 2
        strategy, closes = make_strategy(plan, {SYMBOL: "1.00"},
                                         {"Earlier": "1.00", "Later": "1.01"}, {})
        earlier, later = (SYMBOL, "Earlier"), (SYMBOL, "Later")

        fills, orders, reasons = choose(strategy, closes, {later: 1, earlier: 1})
        self.assertEqual({key: fill.quantity for key, fill in fills.items()}, {earlier: 1, later: 1})
        self.assertEqual(reasons, {})
        self.assertEqual((fills[earlier].commission, fills[later].commission), (Decimal("0.00"), Decimal("0.01")))
        self.assertEqual((fills[earlier].cash_delta, fills[later].cash_delta), (Decimal("-1.00"), Decimal("-1.01")))
        self.assertEqual(set(orders), {SYMBOL})
        order = orders[SYMBOL]
        self.assertEqual((order.quantity, order.price.as_decimal(), order.commission),
                         (2, Decimal("1.00"), Decimal("0.01")))

    def test_internal_refill_finds_quantity_between_untouched_asset_and_sleeve_caps(self):
        plan = _plan(("Owner", "Buyer", "Seller"), slippage="8000")
        for sleeve, budget in zip(plan["sleeves"], ("0.525", "0.225", "0.250"), strict=True):
            sleeve["budget_weight"] = budget
        plan["bundle"]["spec"]["portfolio"]["max_weight_per_asset"] = "0.5"
        strategy, closes = make_strategy(plan, {SYMBOL: "10.00", SECOND: "192.00"},
                                         {"Owner": "0.00", "Buyer": "108.00", "Seller": "0.00"},
                                         {("Owner", SECOND): 1, ("Seller", SYMBOL): 10})
        buy, sell = (SYMBOL, "Buyer"), (SYMBOL, "Seller")

        fills, orders, reasons = choose(strategy, closes, {buy: 10, sell: -10})
        self.assertEqual({key: fill.quantity for key, fill in fills.items()}, {buy: 8, sell: -10})
        self.assertEqual(reasons, {buy: "sleeve_gross_limit"})
        self.assertEqual((fills[buy].netted, fills[buy].external, fills[buy].cash_delta), (8, 0, Decimal("-80.00")))
        self.assertEqual((fills[sell].netted, fills[sell].external, fills[sell].cash_delta), (8, 2, Decimal("84.00")))
        self.assertEqual(set(orders), {SYMBOL})
        order = orders[SYMBOL]
        self.assertEqual((order.quantity, order.price.as_decimal(), order.commission),
                         (-2, Decimal("2.00"), Decimal("0.00")))

        # Net equity is $320 + $8 per internal share. Seven matches leave
        # BBB above 50%, while nine matches exceed the buyer's 22.5% gross cap.
        self.assertGreater(closes[SECOND], Decimal("376.00") * Decimal("0.5"))
        self.assertEqual(closes[SECOND], Decimal("384.00") * Decimal("0.5"))
        self.assertLessEqual(8 * closes[SYMBOL], Decimal("384.00") * Decimal("0.225"))
        self.assertGreater(9 * closes[SYMBOL], Decimal("392.00") * Decimal("0.225"))
