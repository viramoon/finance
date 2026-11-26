"""Synthetic regressions for the settlement of rounded shared fees."""

from decimal import Decimal
from fractions import Fraction
from math import floor
from random import Random
import unittest

from fertile_finance.strategy_fee_search import largest_cash_feasible_fee_total, largest_fee_ratio_quantity

from test_strategy_backtest import SYMBOL, _plan
from test_strategy_priority_regressions import SECOND, choose, make_strategy


class StrategyFeeRegressionTests(unittest.TestCase):
    def test_fixed_fee_search_matches_exhaustive_apportionment(self):
        samples = Random(36)
        for case in range(160):
            rate = Fraction(samples.randrange(20), samples.randrange(1, 20))
            prefix = samples.randrange(20)
            peer_shares = [samples.randrange(1, 15) for _ in range(samples.randrange(4))]
            peers = tuple((shares, samples.randrange(20)) for shares in peer_shares)
            focal_index = samples.randrange(len(peers) + 1)
            wallet, ask = samples.randrange(50), samples.randrange(1, 10)
            fixed = sum(peer_shares)
            low, high = fixed + 1, fixed + samples.randrange(1, 30)
            prefix_fee = floor(rate * prefix + Fraction(1, 2))
            expected = None
            for total in range(low, high + 1):
                shares = peer_shares.copy()
                shares.insert(focal_index, total - fixed)
                fee = floor(rate * (total + prefix) + Fraction(1, 2)) - prefix_fee
                charges = [fee * quantity // total for quantity in shares]
                cents_left = fee - sum(charges)
                for index in range(cents_left):
                    charges[index] += 1
                own_charge = charges.pop(focal_index)
                if (total - fixed) * ask + own_charge <= wallet and all(
                    charge <= cap for charge, (_, cap) in zip(charges, peers, strict=True)
                ):
                    expected = total
            with self.subTest(case=case):
                self.assertEqual(largest_cash_feasible_fee_total(
                    low, high, rate, prefix, peers, focal_index, focal=(wallet, ask),
                ), expected)

    def test_sparse_fee_ratio_in_a_billion_share_interval(self):
        quantity = 500_000_000
        ratio = Fraction(1, quantity)
        self.assertEqual(largest_fee_ratio_quantity(
            1, 1_000_000_000, Fraction(1, 1_000_000_000), 0, ratio, ratio, include_upper=True,
        ), quantity)

    def test_refill_keeps_larger_feasible_buy_after_an_infeasible_quantity(self):
        third = "CCC.XNYS"
        plan = _plan(("A", "B", "C"), fee="1500", slippage="2500")
        for sleeve, weight in zip(plan["sleeves"], ("0.77", "0.20", "0.03"), strict=True):
            sleeve["budget_weight"] = weight
        strategy, closes = make_strategy(
            plan, {SYMBOL: "3.00", SECOND: "0.02", third: "0.01"},
            {"A": "5.25", "B": "0.26", "C": "1.14"}, {},
        )
        shared_a, shared_b, shared_c = (SECOND, "A"), (SECOND, "B"), (SECOND, "C")
        separate_b, separate_c = (third, "B"), (SYMBOL, "C")
        peers = {shared_a: 2, shared_b: 7, separate_b: 2}
        requested = peers | {shared_c: 4, separate_c: 1}
        prices = {instrument: Fraction(close) for instrument, close in closes.items()}
        equity = Fraction("6.65")

        # B's seven BBB shares and its two CCC shares leave exactly three
        # cents for fees. The apportionment charges it one extra cent for
        # each even-sized C trial, while the portfolio risk checks all pass.
        for quantity in range(1, 5):
            with self.subTest(quantity=quantity):
                trial = strategy._settle(peers | {shared_c: quantity}, closes)
                affordable = quantity % 2 != 0
                expected_fee = Decimal("0.03") if affordable else Decimal("0.04")
                expected_cash = Fraction() if affordable else Fraction("-0.01")
                self.assertEqual(trial.fills[shared_b].commission, expected_fee)
                self.assertEqual(trial.balances["B"], expected_cash)
                self.assertIsNone(strategy._execution_risk_breach({}, prices, equity, trial.fills))
                self.assertEqual(strategy._execution_rejection({}, prices, equity, trial),
                                 None if affordable else "sleeve_cash")

        fills, orders, reasons = choose(strategy, closes, requested)
        accepted = {key: fill.quantity for key, fill in fills.items()}
        self.assertEqual(accepted, peers | {shared_c: 3})
        self.assertEqual(reasons, {separate_c: "sleeve_cash", shared_c: "sleeve_cash"})
        self.assertEqual(set(orders), {SECOND, third})
        self.assertEqual(orders[SECOND].quantity, 12)
        self.assertEqual(orders[SECOND].price.as_decimal(), Decimal("0.03"))
        self.assertEqual(orders[SECOND].commission, Decimal("0.05"))
        self.assertEqual(orders[third].quantity, 2)
        self.assertEqual(orders[third].price.as_decimal(), Decimal("0.01"))
        self.assertEqual(orders[third].commission, Decimal())
        balances = {recipe: sleeve.cash for recipe, sleeve in strategy.sleeves.items()}
        for (_, recipe), fill in fills.items():
            balances[recipe] += fill.cash_delta
        self.assertEqual(balances, {"A": Decimal("5.18"), "B": Decimal(), "C": Decimal("1.04")})
