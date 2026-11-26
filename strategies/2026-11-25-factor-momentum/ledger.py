"""Cash-settled factor exposures for this monthly research backtest."""

import csv
from calendar import monthrange
from collections import defaultdict
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from datetime import date, datetime, timedelta
from decimal import Decimal, localcontext
from fractions import Fraction
from gzip import GzipFile, open as gzip_open
from io import TextIOWrapper
from itertools import combinations
from pathlib import Path

from fertile_finance.research_decimal import RESEARCH_CONTEXT


ZERO = Decimal(0)
UNIT_NOTIONAL = Decimal(1)
INITIAL_NAV = Decimal("100000.00")
AUDIT_TOLERANCE = Decimal("1e-25")


@dataclass(frozen=True)
class MonthPlan:
    month: date
    signal_ts: datetime
    fill_ts: datetime
    weights: Mapping[str, Decimal | Fraction]


@dataclass(frozen=True)
class MonthResult:
    month: date
    signal_ts: datetime
    fill_ts: datetime
    weights: Mapping[str, Decimal]
    nav_open: Decimal
    nav_close: Decimal
    net_return: Decimal
    rf: Decimal
    turnover: Decimal
    gross_exposure: Decimal
    net_exposure: Decimal
    factor_pnl: Decimal
    rf_interest: Decimal
    financing_cost: Decimal
    cost: Decimal


@dataclass(frozen=True)
class Simulation:
    initial_nav: Decimal
    cost_bps: int
    months: tuple[MonthResult, ...]
    quantities: tuple[Mapping[str, Decimal], ...]
    orders: tuple[Mapping[str, Decimal], ...]


def _month_key(day):
    return day.year, day.month


def _validate_plan(plan, previous_month):
    month = plan.month
    previous_day = month.replace(day=1) - timedelta(days=1)
    if month.day != monthrange(month.year, month.month)[1] or (previous_month and month <= previous_month):
        raise ValueError("Plans must have strictly increasing calendar month-end keys")
    if any(when.utcoffset() != timedelta(0) for when in (plan.signal_ts, plan.fill_ts)):
        raise ValueError("Signal and fill timestamps must be UTC")
    if (plan.signal_ts >= plan.fill_ts or _month_key(plan.signal_ts) != _month_key(previous_day)
            or _month_key(plan.fill_ts) != _month_key(month)):
        raise ValueError(f"{month}: signal must precede its next-month fill")


def _decimal_weight(weight):
    if isinstance(weight, Fraction):
        return Decimal(weight.numerator) / Decimal(weight.denominator)
    if not isinstance(weight, Decimal) or not weight.is_finite():
        raise ValueError("Expected finite Decimal or Fraction weights")
    return weight


def _rate(source, factor, month):
    value = source.get(factor)
    if not isinstance(value, Decimal) or not value.is_finite():
        raise ValueError(f"{month}: missing or invalid realized {factor} return")
    return value


def _add_quantities(left, right):
    # Preserve every emitted quantity digit through the subtraction and replay.
    if not left or not right:
        return left if left else right
    with localcontext(RESEARCH_CONTEXT) as context:
        context.prec = max(left.adjusted(), right.adjusted()) - min(
            left.as_tuple().exponent, right.as_tuple().exponent,
        ) + 2
        return left + right


def _notional_changes(positions, previous):
    changes = {}
    for factor in sorted(positions.keys() | previous.keys()):
        change = _add_quantities(positions.get(factor, ZERO), previous.get(factor, ZERO).copy_negate())
        if change:
            changes[factor] = change
    return changes


def _gross_exposure(quantities, collateral):
    exposure = sum(map(abs, quantities), ZERO) / collateral
    # Recurring weights and long NAV products can round at 34 significant digits.
    return UNIT_NOTIONAL if abs(exposure - UNIT_NOTIONAL) <= Decimal("1e-30") else exposure


def simulate(
    plans: Sequence[MonthPlan], monthly_factors: Mapping[date, Mapping[str, Decimal | None]],
    *, cost_bps: int, initial_nav: Decimal = INITIAL_NAV,
) -> Simulation:
    """Book the published full-month payoff and charge for all signed notional changes."""
    if type(cost_bps) is not int or cost_bps < 0:
        raise ValueError("Expected nonnegative integer cost bps")
    if not isinstance(initial_nav, Decimal) or not initial_nav.is_finite() or initial_nav <= 0:
        raise ValueError("Expected positive, finite Decimal initial NAV")

    with localcontext(RESEARCH_CONTEXT):
        months = []
        quantities = []
        orders = []
        previous = {}
        previous_month = None
        collateral = initial_nav
        fee_rate = Decimal(cost_bps) / 10_000
        for plan in plans:
            month = plan.month
            _validate_plan(plan, previous_month)
            source = monthly_factors.get(month, {})
            rf = _rate(source, "RF", month)
            weights = {factor: _decimal_weight(weight) for factor, weight in sorted(plan.weights.items())}
            positions = {factor: collateral * weight for factor, weight in weights.items() if weight}
            changes = _notional_changes(positions, previous)
            traded = sum(map(abs, changes.values()), ZERO)
            cost = traded * fee_rate
            gross = _gross_exposure(positions.values(), collateral)
            factor_pnl = sum((quantity * _rate(source, factor, month) for factor, quantity in positions.items()), ZERO)
            rf_interest = collateral * rf
            financing = collateral * max(ZERO, gross - UNIT_NOTIONAL) * rf
            pnl = factor_pnl + rf_interest - financing - cost
            nav_close = collateral + pnl
            if nav_close <= 0:
                raise ValueError(f"{month}: nonpositive research NAV")
            months.append(MonthResult(
                month, plan.signal_ts, plan.fill_ts, weights, collateral, nav_close, pnl / collateral,
                rf, traded / collateral, gross, sum(positions.values(), ZERO) / collateral,
                factor_pnl, rf_interest, financing, cost,
            ))
            quantities.append(positions)
            orders.append(changes)
            previous = positions
            previous_month = month
            collateral = nav_close
        return Simulation(initial_nav, cost_bps, tuple(months), tuple(quantities), tuple(orders))


AUDIT_FIELDS = {
    "orders": ("order_id", "factor", "side", "quantity", "notional_per_unit", "notional", "fee", "signal_ts", "fill_ts"),
    "positions": ("factor", "signed_quantity", "notional_per_unit", "signed_notional"),
    "cash": ("opening_funding", "factor_settlement", "rf_interest", "financing_cash_flow", "cost_cash_flow"),
}


def write_audit(simulations, monthly_factors, directory: Path):
    """Write reproducible CSV.gz ledgers and reconcile their serialized contents."""
    directory.mkdir(parents=True, exist_ok=True)
    with localcontext(RESEARCH_CONTEXT):
        for name, fields in AUDIT_FIELDS.items():
            with (directory / f"{name}.csv.gz").open("wb") as raw, GzipFile(
                filename="", mode="wb", fileobj=raw, mtime=0,
            ) as compressed, TextIOWrapper(compressed, encoding="utf-8", newline="") as file:
                writer = csv.writer(file, lineterminator="\n")
                writer.writerow(("strategy", "cost_bps", "month", *fields))
                writer.writerows(_audit_rows(simulations, name))

    report = verify_audit(simulations, monthly_factors, directory)
    if any(Decimal(error) > AUDIT_TOLERANCE for error in report["max_relative_error"].values()):
        raise ValueError(f"Serialized audit failed: {report['max_relative_error']}")
    return report


def _audit_rows(simulations, name):
    for (strategy, cost_bps), result in simulations.items():
        if cost_bps != result.cost_bps:
            raise ValueError("Simulation key and audited fees differ")
        fee_rate = Decimal(cost_bps) / 10_000
        for index, month in enumerate(result.months):
            key = (strategy, cost_bps, month.month)
            if name == "orders":
                for factor, change in result.orders[index].items():
                    side = "BUY" if change > 0 else "SELL"
                    quantity = change.copy_abs()
                    order_id = f"{strategy}/{cost_bps}/{month.month}/{factor}"
                    yield (*key, order_id, factor, side, quantity, UNIT_NOTIONAL, quantity,
                           quantity * fee_rate, month.signal_ts.isoformat(), month.fill_ts.isoformat())
            elif name == "positions":
                for factor, quantity in result.quantities[index].items():
                    yield (*key, factor, quantity, UNIT_NOTIONAL, quantity)
            else:
                yield (*key, result.initial_nav if index == 0 else ZERO, month.factor_pnl,
                       month.rf_interest, -month.financing_cost, -month.cost)


def _read_ledger(directory, name, simulations):
    rows = defaultdict(list)
    with gzip_open(directory / f"{name}.csv.gz", "rt", encoding="utf-8", newline="") as file:
        reader = csv.DictReader(file)
        if reader.fieldnames != ["strategy", "cost_bps", "month", *AUDIT_FIELDS[name]]:
            raise ValueError(f"Unexpected {name} ledger columns")
        for row in reader:
            strategy = (row["strategy"], int(row["cost_bps"]))
            if strategy not in simulations:
                raise ValueError(f"Unexpected {name} ledger simulation: {strategy}")
            rows[strategy, date.fromisoformat(row["month"])].append(row)
    return rows


def _position_payoff(positions, source, month):
    return sum((quantity * _rate(source, factor, month) for factor, quantity in positions.items() if quantity), ZERO)


def _replay_orders(rows, results, monthly_factors, seed, fee_rate):
    positions = defaultdict(Decimal)
    nav = seed
    values = []
    quantity_snapshots = []
    seen_ids = set()
    for month in results:
        fees = ZERO
        for row in rows.pop(month.month, ()):
            signal = datetime.fromisoformat(row["signal_ts"])
            fill = datetime.fromisoformat(row["fill_ts"])
            if row["order_id"] in seen_ids or not signal < fill or (signal, fill) != (month.signal_ts, month.fill_ts):
                raise ValueError(f"{month.month}: invalid order identity or signal/fill timestamps")
            seen_ids.add(row["order_id"])
            notional = Decimal(row["quantity"])
            if (row["side"] not in {"BUY", "SELL"} or notional <= 0
                    or Decimal(row["notional_per_unit"]) != UNIT_NOTIONAL
                    or notional != Decimal(row["notional"]) or notional * fee_rate != Decimal(row["fee"])):
                raise ValueError(f"{month.month}: invalid order amount or fee")
            signed = notional if row["side"] == "BUY" else notional.copy_negate()
            factor = row["factor"]
            positions[factor] = _add_quantities(positions[factor], signed)
            fees += notional * fee_rate
        source = monthly_factors[month.month]
        rf = _rate(source, "RF", month.month)
        borrowed = max(ZERO, _gross_exposure(positions.values(), nav) - 1) * nav
        nav += _position_payoff(positions, source, month.month) + (nav - borrowed) * rf - fees
        values.append(nav)
        quantity_snapshots.append(dict(positions))
    return values, quantity_snapshots


def _replay_positions(rows, results, monthly_factors, seed, fee_rate):
    previous = {}
    nav = seed
    values = []
    quantity_snapshots = []
    for month in results:
        positions = {}
        for row in rows.pop(month.month, ()):
            signed = Decimal(row["signed_quantity"])
            factor = row["factor"]
            if (factor in positions or Decimal(row["notional_per_unit"]) != UNIT_NOTIONAL
                    or signed != Decimal(row["signed_notional"])):
                raise ValueError(f"{month.month}: invalid signed position amount or duplicate factor")
            positions[factor] = signed
        traded = sum((abs(positions.get(factor, ZERO) - previous.get(factor, ZERO))
                      for factor in sorted(positions.keys() | previous.keys())), ZERO)
        source = monthly_factors[month.month]
        rf = _rate(source, "RF", month.month)
        cash_earning_rf = nav * (1 - max(ZERO, _gross_exposure(positions.values(), nav) - 1))
        nav = nav + cash_earning_rf * rf + _position_payoff(positions, source, month.month) - traded * fee_rate
        values.append(nav)
        quantity_snapshots.append(positions)
        previous = positions
    return values, quantity_snapshots


def _replay_cash(rows, results):
    cash = ZERO
    values = []
    for month in results:
        records = rows.pop(month.month, ())
        if len(records) != 1:
            raise ValueError(f"{month.month}: expected one cash ledger record")
        cash += sum((Decimal(records[0][field]) for field in AUDIT_FIELDS["cash"]), ZERO)
        values.append(cash)
    return values


def verify_audit(simulations, monthly_factors, directory: Path):
    """Rebuild NAV separately from fills, position snapshots and cash movements."""
    with localcontext(RESEARCH_CONTEXT):
        readback = {name: _read_ledger(directory, name, simulations) for name in AUDIT_FIELDS}
        all_errors = {}
        by_simulation = {}
        for scenario, result in simulations.items():
            strategy, cost_bps = scenario
            fee_rate = Decimal(cost_bps) / 10_000
            observed_months = {month.month for month in result.months}
            rows = {}
            for name, ledger in readback.items():
                rows[name] = {month: ledger.pop((scenario, month)) for month in observed_months
                              if (scenario, month) in ledger}
            order_nav, order_quantities = _replay_orders(
                rows["orders"], result.months, monthly_factors, result.initial_nav, fee_rate,
            )
            position_nav, position_quantities = _replay_positions(
                rows["positions"], result.months, monthly_factors, result.initial_nav, fee_rate,
            )
            curves = {
                "formula": [month.nav_close for month in result.months],
                "orders": order_nav,
                "positions": position_nav,
                "cash": _replay_cash(rows["cash"], result.months),
            }
            errors = {}
            for (name_a, values_a), (name_b, values_b) in combinations(curves.items(), 2):
                comparison = f"{name_a}_vs_{name_b}"
                error = max((abs(a - b) / reference for a, b, reference in zip(
                    values_a, values_b, curves["formula"], strict=True)), default=ZERO)
                errors[comparison] = str(error)
                all_errors[comparison] = max(error, all_errors.get(comparison, ZERO))
            inventory_check = "orders_vs_positions_quantity_scaled_by_nav"
            inventory_error = max((
                abs(orders.get(name, ZERO) - positions.get(name, ZERO)) / reference
                for orders, positions, reference in zip(order_quantities, position_quantities, curves["formula"], strict=True)
                for name in orders.keys() | positions.keys()
            ), default=ZERO)
            errors[inventory_check] = str(inventory_error)
            all_errors[inventory_check] = max(inventory_error, all_errors.get(inventory_check, ZERO))
            by_simulation[f"{strategy}/{cost_bps}bp"] = {
                "observations": len(result.months),
                "orders": sum(map(len, result.orders)),
                "position_rows": sum(map(len, result.quantities)),
                "cash_rows": len(result.months),
                "max_relative_error": errors,
            }

        if any(readback.values()):
            raise ValueError("Audit includes observations outside the simulation sample")
        return {
            "relative_error_denominator": "absolute formula NAV for each month",
            "tolerance": str(AUDIT_TOLERANCE),
            "max_relative_error": {name: str(value) for name, value in all_errors.items()},
            "simulations": by_simulation,
        }
