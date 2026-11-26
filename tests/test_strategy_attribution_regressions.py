"""Regressions for clipping reasons derived from final synthetic allocations."""

from decimal import Decimal
from fractions import Fraction
import unittest

from test_strategy_backtest import SYMBOL, _plan
from test_strategy_priority_regressions import SECOND, choose, make_strategy


class StrategyAttributionRegressionTests(unittest.TestCase):
    def test_clipping_reasons_use_final_peer_fills(self):
        plan = _plan(("A", "B"), fee="10000", slippage="100")
        plan["bundle"]["spec"]["portfolio"].update(max_gross_weight="0.4", max_weight_per_asset="0.5")
        strategy, closes = make_strategy(
            plan, {SYMBOL: "0.18", SECOND: "0.62"}, {"A": "0.97", "B": "0.84"}, {("A", SYMBOL): 3},
        )
        sell_a, buy_a, buy_b = (SYMBOL, "A"), (SECOND, "A"), (SYMBOL, "B")
        increments = {sell_a: -3, buy_a: 2, buy_b: 4}
        fills, orders, reasons = choose(strategy, closes, increments)

        accepted = {key: fill.quantity for key, fill in fills.items()}
        self.assertEqual(accepted, {sell_a: -3, buy_b: 3})
        self.assertEqual(orders, {})
        self.assertEqual((fills[buy_b].netted, fills[buy_b].external), (3, 0))
        self.assertEqual(fills[buy_b].cash_delta, Decimal("-0.54"))

        holdings = {sell_a: 3}
        prices = {key: Fraction(price) for key, price in closes.items()}
        equity = Fraction("2.35")
        next_b = strategy._settle(accepted | {buy_b: 4}, closes)
        self.assertEqual((next_b.fills[buy_b].netted, next_b.fills[buy_b].external), (3, 1))
        self.assertEqual(next_b.fills[buy_b].cash_delta, Decimal("-0.90"))
        self.assertEqual(next_b.balances["B"], Fraction("-0.06"))
        self.assertLess(Decimal(4) * closes[SYMBOL], Decimal("2.17") * Decimal("0.4"))
        self.assertIsNone(strategy._execution_risk_breach(holdings, prices, equity, next_b.fills))

        # With B's final fill held fixed, A could pay for one share of BBB,
        # but that extra share breaches the post-cost portfolio gross limit.
        next_a = strategy._settle(accepted | {buy_a: 1}, closes)
        self.assertEqual(next_a.balances["A"], Fraction("0.25"))
        self.assertGreater(Decimal(3) * closes[SYMBOL] + closes[SECOND], Decimal("1.71") * Decimal("0.4"))
        self.assertEqual(strategy._execution_risk_breach(holdings, prices, equity, next_a.fills),
                         "portfolio_gross_limit")

        self.assertEqual(reasons, {buy_a: "portfolio_gross_limit", buy_b: "sleeve_cash"})

    def test_later_cross_changes_prior_clip_reason_after_quantities_are_fixed(self):
        plan = _plan(("Earlier", "Later"), slippage="1000")
        for sleeve, priority in zip(plan["sleeves"], (1, 2), strict=True):
            sleeve["priority"] = priority
        strategy, closes = make_strategy(
            plan, {SYMBOL: "10.00", SECOND: "10.00"},
            {"Earlier": "0.00", "Later": "100.00"}, {("Earlier", SYMBOL): 10},
        )
        sell, buy_earlier, buy_later = (SYMBOL, "Earlier"), (SECOND, "Earlier"), (SYMBOL, "Later")
        earlier_batch = {sell: -10, buy_earlier: 10}

        earlier_fills, _, earlier_reasons = choose(strategy, closes, earlier_batch)
        self.assertEqual(earlier_fills[buy_earlier].quantity, 8)
        self.assertEqual(earlier_reasons, {buy_earlier: "sleeve_cash"})

        fills, orders, reasons = choose(strategy, closes, earlier_batch | {buy_later: 10})
        accepted = {key: fill.quantity for key, fill in fills.items()}
        self.assertEqual(accepted, {sell: -10, buy_earlier: 8, buy_later: 9})
        self.assertEqual({key: order.quantity for key, order in orders.items()}, {SYMBOL: -1, SECOND: 8})
        self.assertEqual(fills[sell].cash_delta, Decimal("99.00"))

        # The later buy increases the earlier sale proceeds via internal crossing.
        # Nine shares at the $11 ask now cost exactly the earlier sleeve's $99.
        next_earlier = strategy._settle(accepted | {buy_earlier: 9}, closes)
        self.assertEqual(next_earlier.fills[buy_earlier].cash_delta, Decimal("-99.00"))
        self.assertEqual(next_earlier.balances["Earlier"], Fraction(0))
        self.assertIsNone(strategy._execution_risk_breach(
            {sell: 10}, {key: Fraction(price) for key, price in closes.items()}, Fraction(200), next_earlier.fills,
        ))
        self.assertEqual(reasons, {buy_earlier: "allocation_order", buy_later: "sleeve_gross_limit"})
