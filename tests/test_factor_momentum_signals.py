from datetime import date, datetime, timezone
from decimal import Decimal
from fractions import Fraction
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

from fertile_finance.research_windows import month_end


STRATEGY = Path(__file__).resolve().parents[1] / "strategies/2026-11-25-factor-momentum"
with patch.object(sys, "path", [str(STRATEGY), *sys.path]):
    from signals import MAIN_FACTORS, build_plans, target_weights


class FactorMomentumSignalsTest(unittest.TestCase):
    def setUp(self):
        self.signal = date(2024, 6, 30)
        self.holding = date(2024, 7, 31)
        self.rows = {month_end(self.signal, offset): dict.fromkeys(MAIN_FACTORS, Decimal(0))
                     for offset in range(-11, 1)}
        self.rows[self.signal]["SMB"] = Decimal("0.02")
        self.rows[self.signal]["HML"] = Decimal("-0.40")
        self.rows[month_end(self.signal, -1)]["HML"] = Decimal("0.50")
        self.rows[month_end(self.signal, -1)]["CMA"] = None
        self.rows[month_end(self.signal, -11)]["Mom"] = Decimal("0.03")

    def test_sign_compounding_zero_and_equal_notional_use_only_available_factors(self):
        weights = target_weights(self.rows, self.signal, "main")
        self.assertEqual(weights, {"SMB": Fraction(1, 4), "HML": Fraction(-1, 4),
                                   "RMW": Fraction(-1, 4), "Mom": Fraction(1, 4)})
        self.assertEqual(sum(map(abs, weights.values())), 1)
        self.assertEqual(target_weights(self.rows, month_end(self.signal, -1), "main"), {})

    def test_future_returns_do_not_affect_current_signal_and_fill_is_next_month(self):
        jun_first = datetime(2024, 6, 3, 20, tzinfo=timezone.utc)
        jun_last = datetime(2024, 6, 28, 20, tzinfo=timezone.utc)
        jul_first = datetime(2024, 7, 1, 20, tzinfo=timezone.utc)
        jul_last = datetime(2024, 7, 31, 20, tzinfo=timezone.utc)
        bounds = {self.signal: (jun_first, jun_last), self.holding: (jul_first, jul_last)}
        first = build_plans(self.rows, bounds, "main", start=self.holding, end=self.holding)
        self.rows[self.holding] = dict.fromkeys(MAIN_FACTORS, Decimal("-0.99"))
        changed = build_plans(self.rows, bounds, "main", start=self.holding, end=self.holding)
        self.assertEqual(first, changed)
        (plan,) = first
        self.assertEqual((plan.signal_ts, plan.fill_ts), (jun_last, jul_first))
        self.assertLess(plan.signal_ts, plan.fill_ts)

    def test_volatility_matching_requires_trailing_history_and_is_prefix_invariant(self):
        self.assertEqual(target_weights(self.rows, self.signal, "v2_inverse_vol"), {})
        self.assertEqual(target_weights(self.rows, self.signal, "vol_matched"), {})
        rows = {month_end(self.signal, offset): {
            name: Decimal((offset + index + 2) % (index + 2) + 1) * Decimal("0.01") * (-1) ** index
            for index, name in enumerate(MAIN_FACTORS)
        } for offset in range(-35, 1)}
        for variant in ("v2_inverse_vol", "vol_matched"):
            with self.subTest(variant=variant):
                prior = target_weights(rows, self.signal, variant)
                future = rows | {self.holding: dict.fromkeys(MAIN_FACTORS, Decimal(2))}
                self.assertTrue(prior)
                self.assertEqual(prior, target_weights(future, self.signal, variant))
                self.assertLessEqual(sum(map(abs, prior.values())), Decimal(2))
