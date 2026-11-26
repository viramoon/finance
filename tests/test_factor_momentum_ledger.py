import csv
import gzip
import importlib
import tempfile
import unittest
from datetime import date, datetime, timezone
from decimal import Decimal, ROUND_DOWN, localcontext
from pathlib import Path


ledger = importlib.import_module("strategies.2026-11-25-factor-momentum.ledger")
D = Decimal
JANUARY = date(2020, 1, 31)
FEBRUARY = date(2020, 2, 29)


def closing(day):
    return datetime.combine(day, datetime.min.time().replace(hour=21), tzinfo=timezone.utc)


def fixture():
    plans = (
        ledger.MonthPlan(JANUARY, closing(date(2019, 12, 31)), closing(date(2020, 1, 2)),
                         {"A": D("0.5"), "B": D("-0.5")}),
        ledger.MonthPlan(FEBRUARY, closing(JANUARY), closing(date(2020, 2, 3)),
                         {"A": D("-0.5"), "B": D("0.5")}),
    )
    returns = {
        JANUARY: {"A": D("0.10"), "B": D("-0.02"), "RF": D("0.01")},
        FEBRUARY: {"A": D("0.04"), "B": D("0.02"), "RF": D("0.01")},
    }
    return plans, returns


class FactorMomentumLedgerTests(unittest.TestCase):
    def test_signed_notional_turnover_charges_buys_sells_and_initial_entry(self):
        plans, returns = fixture()
        run = ledger.simulate(plans, returns, cost_bps=25, initial_nav=D(100))
        january, february = run.months
        self.assertEqual(run.orders[0], {"A": D(50), "B": D(-50)})
        self.assertEqual(january.cost, D("0.25"))
        self.assertEqual(january.nav_close, D("106.75"))
        self.assertEqual(run.orders[1], {"A": D("-103.375"), "B": D("103.375")})
        self.assertEqual(february.cost, D("0.516875"))
        self.assertEqual(february.nav_close, D("106.233125"))
        with localcontext(ledger.RESEARCH_CONTEXT):
            self.assertEqual(february.turnover, D("206.75") / D("106.75"))

    def test_rf_credits_opening_collateral_and_funds_excess_gross(self):
        plans, returns = fixture()
        leveraged = ledger.MonthPlan(JANUARY, plans[0].signal_ts, plans[0].fill_ts, {"A": D("1.5")})
        run = ledger.simulate((leveraged,), returns, cost_bps=25, initial_nav=D(100))
        observation = run.months[0]
        self.assertEqual(observation.rf, D("0.01"))
        self.assertEqual(observation.rf_interest, D(1))
        self.assertEqual(observation.financing_cost, D("0.50"))
        self.assertEqual(observation.gross_exposure, D("1.5"))
        self.assertEqual(observation.net_return, D("0.15125"))

    def test_rejects_nonchronological_fills_and_missing_realized_rates(self):
        plans, returns = fixture()
        reversed_times = ledger.MonthPlan(JANUARY, plans[0].fill_ts, plans[0].signal_ts, plans[0].weights)
        with self.assertRaisesRegex(ValueError, "signal must precede"):
            ledger.simulate((reversed_times,), returns, cost_bps=0)
        returns[JANUARY]["A"] = None
        with self.assertRaisesRegex(ValueError, "missing or invalid realized A"):
            ledger.simulate(plans, returns, cost_bps=0)

    def test_independent_csv_ledgers_rebuild_nav_and_detect_tampering(self):
        plans, returns = fixture()
        scenario = {("fixture", 25): ledger.simulate(plans, returns, cost_bps=25, initial_nav=D(100))}
        mutations = {
            "orders": {"quantity": "51", "notional": "51", "fee": "0.1275"},
            "positions": {"signed_quantity": "51", "signed_notional": "51"},
            "cash": {"factor_settlement": "7"},
        }
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)
            for changed, mutation in mutations.items():
                with self.subTest(ledger=changed):
                    result = ledger.write_audit(scenario, returns, path)
                    self.assertTrue(all(D(value) == 0 for value in result["max_relative_error"].values()))
                    changed_file = path / f"{changed}.csv.gz"
                    with gzip.open(changed_file, "rt", newline="", encoding="utf-8") as source:
                        reader = csv.DictReader(source)
                        records = list(reader)
                        fields = reader.fieldnames
                    records[0].update(mutation)
                    with gzip.open(changed_file, "wt", newline="", encoding="utf-8") as target:
                        writer = csv.DictWriter(target, fieldnames=fields)
                        writer.writeheader()
                        writer.writerows(records)
                    errors = ledger.verify_audit(scenario, returns, path)["max_relative_error"]
                    for name in mutations:
                        if name == changed:
                            self.assertGreater(D(errors[f"formula_vs_{name}"]), ledger.AUDIT_TOLERANCE)
                        else:
                            self.assertEqual(D(errors[f"formula_vs_{name}"]), D(0))

    def test_accounting_isolated_from_caller_decimal_context(self):
        plans, returns = fixture()
        with localcontext() as context:
            context.prec = 3
            context.rounding = ROUND_DOWN
            run = ledger.simulate(plans, returns, cost_bps=25, initial_nav=D(100))
            self.assertEqual(run.months[-1].nav_close, D("106.233125"))
            self.assertFalse(any(context.flags.values()))

    def test_inventory_reconciliation_detects_missing_fill_when_nav_is_unchanged(self):
        plans, _ = fixture()
        returns = {JANUARY: dict.fromkeys(("A", "B", "RF"), D(0))}
        scenario = {("fixture", 0): ledger.simulate(plans[:1], returns, cost_bps=0, initial_nav=D(100))}
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)
            ledger.write_audit(scenario, returns, path)
            order_path = path / "orders.csv.gz"
            with gzip.open(order_path, "rt", encoding="utf-8") as file:
                header, _, *orders = file.readlines()
            with gzip.open(order_path, "wt", encoding="utf-8") as file:
                file.writelines((header, *orders))
            errors = ledger.verify_audit(scenario, returns, path)["max_relative_error"]
            self.assertEqual(D(errors["formula_vs_orders"]), D(0))
            self.assertEqual(D(errors["orders_vs_positions_quantity_scaled_by_nav"]), D("0.5"))


if __name__ == "__main__":
    unittest.main()
