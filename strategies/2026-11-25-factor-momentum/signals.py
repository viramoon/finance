"""Month-end factor signals and first-close labels for the offline research."""

from collections.abc import Mapping, Sequence
from datetime import date, datetime
from decimal import Decimal, localcontext
from fractions import Fraction
from statistics import stdev

import exchange_calendars

from fertile_finance.research_decimal import RESEARCH_CONTEXT
from fertile_finance.research_windows import month_end

from ledger import MonthPlan

MAIN_FACTORS = ("SMB", "HML", "RMW", "CMA", "Mom")
EXTENDED_FACTORS = (*MAIN_FACTORS, "ST_Rev", "LT_Rev")
# Order is used for reports only. The main specification alone decides the verdict.
CORE = {
    "main": MAIN_FACTORS,
    "static": MAIN_FACTORS,
    "vol_matched": MAIN_FACTORS,
    "umd": ("Mom",),
}
VARIANTS = {
    "v3_without_mom": MAIN_FACTORS[:-1],
    "v2_inverse_vol": MAIN_FACTORS,
    "v1_cross_sectional": MAIN_FACTORS,
    "extended_7": EXTENDED_FACTORS,
    "lookback_1m": MAIN_FACTORS,
}
MAX_GROSS = Decimal(2)
VOL_LOOKBACK = 36

type FactorRows = Mapping[date, Mapping[str, Decimal | None]]
type Weight = Decimal | Fraction


def history(rows: FactorRows, names: Sequence[str], signal: date, lookback: int) -> dict[str, tuple[Decimal, ...]]:
    dates = [month_end(signal, offset) for offset in range(1 - lookback, 1)]
    series = {}
    for name in names:
        values = tuple(rows.get(day, {}).get(name) for day in dates)
        observed = tuple(value for value in values if value is not None)
        if len(observed) == lookback:
            series[name] = observed
    return series


def _score(returns: Sequence[Decimal]) -> Fraction:
    compounded = Fraction(1)
    for value in returns:
        compounded *= 1 + Fraction(value)
    return compounded - 1


def _sign(value: Fraction) -> int:
    return 1 if value > 0 else -1


def _signed_weights(scores):
    return {name: Fraction(_sign(score), len(scores)) for name, score in scores.items()}


def _decimal(weight: Weight) -> Decimal:
    if isinstance(weight, Decimal):
        return weight
    return Decimal(weight.numerator) / Decimal(weight.denominator)


def _portfolio_vol(weights, trailing):
    if not weights or not weights.keys() <= trailing.keys():
        return None
    weighted_returns = zip(*(
        tuple(_decimal(weight) * value for value in trailing[name])
        for name, weight in weights.items()
    ), strict=True)
    return stdev([sum(month) for month in weighted_returns])


def target_weights(rows: FactorRows, signal: date, variant: str) -> dict[str, Weight]:
    """All input access for a target is confined to consecutive months ending at signal."""
    names = (CORE | VARIANTS)[variant]
    if variant in {"static", "umd"}:
        return dict.fromkeys(names, Fraction(1, len(names)))
    lookback = 1 if variant == "lookback_1m" else 12
    scores = {name: _score(values) for name, values in history(rows, names, signal, lookback).items()}
    if variant == "v1_cross_sectional":
        ranked = sorted(scores, key=lambda name: (scores[name], name))
        half = len(ranked) // 2
        if half == 0:
            return {}
        leg = Fraction(1, 2 * half)
        return dict.fromkeys(ranked[:half], -leg) | dict.fromkeys(ranked[-half:], leg)
    if variant not in {"vol_matched", "v2_inverse_vol"}:
        return _signed_weights(scores)

    with localcontext(RESEARCH_CONTEXT):
        trailing = history(rows, names, signal, VOL_LOOKBACK)
        main_vol = _portfolio_vol(_signed_weights(scores), trailing)
        if not main_vol:
            return {}
        if variant == "vol_matched":
            candidate = dict.fromkeys(names, Fraction(1, len(names)))
        else:
            volatility = {name: stdev(trailing[name]) for name in scores if name in trailing}
            inverse = {name: _sign(scores[name]) / sigma for name, sigma in volatility.items() if sigma}
            gross = sum(map(abs, inverse.values()))
            candidate = {name: weight / gross for name, weight in inverse.items()}
        candidate_vol = _portfolio_vol(candidate, trailing)
        if not candidate_vol:
            return {}
        scale = min(MAX_GROSS, main_vol / candidate_vol)
        return {name: _decimal(weight) * scale for name, weight in candidate.items()}


def close_bounds(daily: FactorRows, start: date, end: date) -> dict[date, tuple[datetime, datetime]]:
    """Use actual FF5 observation dates and the XNYS close clock."""
    calendar = exchange_calendars.get_calendar("XNYS", start=str(start.replace(day=1)), end=str(end))
    closes = {label.date(): stamp.to_pydatetime() for label, stamp in calendar.schedule["close"].items()}
    used = {}
    for day, row in sorted(daily.items()):
        if not start <= day <= end or row.get("RMW") is None or row.get("CMA") is None:
            continue
        label = month_end(day)
        used.setdefault(label, []).append(day)
    bounds = {}
    for label, days in used.items():
        first, last = days[0], days[-1]
        if first not in closes or last not in closes:
            raise ValueError(f"{label}: FF5 boundary is absent from the XNYS close clock")
        bounds[label] = closes[first], closes[last]
    return bounds


def build_plans(rows: FactorRows, bounds, variant: str, *, start: date, end: date) -> tuple[MonthPlan, ...]:
    plans = []
    for month in sorted(label for label in bounds if start <= label <= end):
        signal_month = month_end(month, -1)
        weights = target_weights(rows, signal_month, variant)
        if not weights:
            continue
        fill_ts, _ = bounds[month]
        _, signal_ts = bounds[signal_month]
        if not signal_ts < fill_ts:
            raise ValueError(f"{month}: invalid signal and execution ordering")
        plans.append(MonthPlan(month, signal_ts, fill_ts, weights))
    return tuple(plans)
