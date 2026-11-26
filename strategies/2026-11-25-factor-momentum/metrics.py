"""Monthly performance and inference for the factor momentum study."""

from collections.abc import Mapping, Sequence
from datetime import date
from decimal import Decimal, localcontext
from statistics import mean, stdev
from typing import Protocol

import numpy as np
from scipy.stats import pearsonr

from fertile_finance.research_decimal import RESEARCH_CONTEXT
from fertile_finance.research_windows import month_end, monthly_coverage, monthly_coverage_windows, percent


REGRESSION_FACTORS = ("Mkt-RF", "SMB", "HML", "RMW", "CMA", "Mom")
NEWEY_WEST_LAGS = 6
MONTHS_PER_YEAR = Decimal(12)
ZERO = Decimal(0)

type ReturnSeries = Mapping[date, Decimal | None]


class MonthlyObservation(Protocol):
    nav_open: Decimal
    nav_close: Decimal
    net_return: Decimal
    rf: Decimal
    turnover: Decimal
    weights: Mapping[str, Decimal]
    gross_exposure: Decimal
    net_exposure: Decimal


def _drawdown(nav: Sequence[tuple[date, Decimal]]) -> dict:
    peak_date, peak_value = nav[0]
    worst = ZERO
    worst_peak_date = trough_date = recovery_date = None
    worst_peak_value = None
    for day, value in nav[1:]:
        if value > peak_value:
            peak_date, peak_value = day, value
        loss = 1 - value / peak_value
        if loss > worst:
            worst = loss
            worst_peak_date, worst_peak_value = peak_date, peak_value
            trough_date = day
            recovery_date = None
        elif trough_date is not None and recovery_date is None and value >= worst_peak_value:
            recovery_date = day
    return {
        "max_drawdown_pct": percent(worst),
        "drawdown_peak": worst_peak_date.isoformat() if worst_peak_date else None,
        "drawdown_trough": trough_date.isoformat() if trough_date else None,
        "drawdown_recovery": recovery_date.isoformat() if recovery_date else None,
    }


def _metrics(observations: Mapping[date, MonthlyObservation], dates: Sequence[date],
             factors: Sequence[str]) -> dict:
    records = [observations[day] for day in dates]
    opening, closing = records[0].nav_open, records[-1].nav_close
    nav = [(month_end(dates[0], -1), opening)]
    nav.extend((day, row.nav_close) for day, row in zip(dates, records, strict=True))
    returns = [row.net_return for row in records]
    excess = [row.net_return - row.rf for row in records]
    volatility = stdev(returns) if len(records) > 1 else None
    excess_vol = stdev(excess) if len(records) > 1 else None
    annual_scale = MONTHS_PER_YEAR.sqrt()
    sharpe = mean(excess) / excess_vol * annual_scale if excess_vol else None
    count = Decimal(len(records))
    weights = {}
    for name in factors:
        series = [row.weights.get(name, ZERO) for row in records]
        long_months = sum(value > 0 for value in series)
        short_months = sum(value < 0 for value in series)
        weights[name] = {
            "average_weight_pct": percent(mean(series)),
            "long_month_pct": percent(long_months / count),
            "short_month_pct": percent(short_months / count),
            "flat_month_pct": percent((count - long_months - short_months) / count),
        }
    return {
        "return_pct": percent(closing / opening - 1),
        "cagr_pct": percent((closing / opening) ** (MONTHS_PER_YEAR / count) - 1),
        "volatility_pct": percent(volatility * annual_scale) if volatility is not None else None,
        "sharpe": format(sharpe, ".4f") if sharpe is not None else None,
        "sharpe_precise": str(sharpe) if sharpe is not None else None,
        **_drawdown(nav),
        "average_gross_exposure_pct": percent(mean(row.gross_exposure for row in records)),
        "average_net_exposure_pct": percent(mean(row.net_exposure for row in records)),
        "average_monthly_turnover_pct": percent(mean(row.turnover for row in records)),
        "total_turnover_pct": percent(sum(row.turnover for row in records)),
        "factor_weights": weights,
    }


def _with_metrics(observations: Mapping[date, MonthlyObservation], coverage: dict,
                  factors: Sequence[str]) -> dict:
    metrics = None
    with localcontext(RESEARCH_CONTEXT):
        count = coverage["observations"]
        if coverage["coverage"] == "full":
            first = date.fromisoformat(coverage["actual_start"])
            last = date.fromisoformat(coverage["actual_end"])
            dates = sorted(day for day in observations if first <= day <= last)
            metrics = _metrics(observations, dates, factors)
        return {
            **coverage,
            "actual_years": format(count / MONTHS_PER_YEAR, ".4f") if count else None,
            "metrics": metrics,
        }


def window_rows(observations: Mapping[date, MonthlyObservation], end: date,
                factors: Sequence[str]) -> dict[str, dict]:
    """Apply the research window names to contiguous monthly return coverage."""
    return {
        row["window"]: _with_metrics(observations, row, factors)
        for row in monthly_coverage_windows(observations.keys(), end)
    }


def window_row(observations: Mapping[date, MonthlyObservation], name: str, start: date | None,
               end: date, factors: Sequence[str]) -> dict:
    """Report an explicit range such as an out-of-sample window or market segment."""
    return _with_metrics(observations, monthly_coverage(observations.keys(), name=name, start=start, end=end), factors)


def _sample(dates: Sequence[date]) -> dict:
    return {
        "n": len(dates),
        "start": dates[0].isoformat() if dates else None,
        "end": dates[-1].isoformat() if dates else None,
    }


def correlations(returns: ReturnSeries, references: Mapping[str, ReturnSeries]) -> dict[str, dict]:
    """Correlate matched months independently for each pair, without filling missing returns."""
    results = {}
    for name, reference in references.items():
        dates = sorted(day for day, value in returns.items() if value is not None and reference.get(day) is not None)
        left = [float(returns[day]) for day in dates]
        right = [float(reference[day]) for day in dates]
        coefficient = None
        if len(set(left)) > 1 and len(set(right)) > 1:
            coefficient = float(pearsonr(left, right).statistic)
        results[name] = {**_sample(dates), "pearson_r": coefficient}
    return results


def factor_regression(returns: ReturnSeries, rf: ReturnSeries, factors: Mapping[str, ReturnSeries]) -> dict:
    """Regress monthly excess returns on FF5 + Mom with Bartlett Newey-West errors."""
    import statsmodels.api as sm

    series = [factors[name] for name in REGRESSION_FACTORS]
    dates = sorted(
        day for day, value in returns.items()
        if value is not None and rf.get(day) is not None and all(column.get(day) is not None for column in series)
    )
    result = {
        **_sample(dates),
        "factors": list(REGRESSION_FACTORS),
        "newey_west_lags": NEWEY_WEST_LAGS,
        "alpha_monthly": None,
        "alpha_annualized_arithmetic": None,
        "alpha_newey_west_t": None,
        "coefficients": None,
        "r_squared": None,
    }
    if len(dates) <= len(series) + 1:
        return {**result, "status": "insufficient_observations"}
    with localcontext(RESEARCH_CONTEXT):
        outcome = np.array([float(returns[day] - rf[day]) for day in dates])
    if np.ptp(outcome) == 0:
        return {**result, "status": "constant_excess_returns"}
    design = sm.add_constant(np.array([[float(column[day]) for column in series] for day in dates]),
                             has_constant="add")
    if np.linalg.matrix_rank(design) < design.shape[1]:
        return {**result, "status": "rank_deficient"}
    fit = sm.OLS(outcome, design).fit(cov_type="HAC", cov_kwds={
        "maxlags": NEWEY_WEST_LAGS, "kernel": "bartlett", "use_correction": True,
    })
    alpha = float(fit.params[0])
    t_value = float(fit.tvalues[0])
    numerical_tolerance = np.finfo(float).eps * max(design.shape) * np.max(np.abs(outcome))
    exact_fit = np.max(np.abs(fit.resid)) <= numerical_tolerance
    return {
        **result,
        "status": "exact_fit" if exact_fit else "ok",
        "alpha_monthly": alpha,
        "alpha_annualized_arithmetic": alpha * int(MONTHS_PER_YEAR),
        "alpha_newey_west_t": t_value if not exact_fit and np.isfinite(t_value) else None,
        "coefficients": dict(zip(REGRESSION_FACTORS, map(float, fit.params[1:]), strict=True)),
        "r_squared": float(fit.rsquared),
    }


def flip_rows(observations: Mapping[date, MonthlyObservation], factor_returns: Mapping[str, ReturnSeries],
              factors: Sequence[str]) -> list[dict]:
    """Report signal-month and following holding-month outcomes for adjacent sign flips."""
    events = []
    with localcontext(RESEARCH_CONTEXT):
        for holding_month in sorted(observations):
            signal_month = month_end(holding_month, -1)
            previous = observations.get(signal_month)
            if previous is None:
                continue
            current = observations[holding_month]
            for name in factors:
                prior_weight = previous.weights.get(name, ZERO)
                next_weight = current.weights.get(name, ZERO)
                if prior_weight * next_weight >= 0:
                    continue
                event = {
                    "factor": name,
                    "direction": "short_to_long" if next_weight > 0 else "long_to_short",
                    "signal_month": signal_month.isoformat(),
                    "holding_month": holding_month.isoformat(),
                    "previous_weight": prior_weight,
                    "new_weight": next_weight,
                }
                for prefix, day, row, weight in (
                    ("signal", signal_month, previous, prior_weight),
                    ("holding", holding_month, current, next_weight),
                ):
                    value = factor_returns[name].get(day)
                    event[f"{prefix}_month_factor_return"] = value
                    event[f"{prefix}_month_factor_contribution"] = weight * value if value is not None else None
                    event[f"{prefix}_month_strategy_return"] = row.net_return
                events.append(event)
    return events


def flip_summary(events: Sequence[dict], factors: Sequence[str]) -> dict[str, dict]:
    """Summarize realized outcomes for each factor and direction."""
    fields = tuple(f"{period}_month_{outcome}" for period in ("signal", "holding")
                   for outcome in ("factor_return", "factor_contribution", "strategy_return"))
    summary = {}
    with localcontext(RESEARCH_CONTEXT):
        for name in factors:
            grouped = {}
            for direction in ("short_to_long", "long_to_short"):
                group = [event for event in events if event["factor"] == name and event["direction"] == direction]
                measures = {}
                for field in fields:
                    values = [event[field] for event in group if event[field] is not None]
                    measures[f"average_{field}_pct"] = percent(mean(values)) if values else None
                grouped[direction] = {"n": len(group), **measures}
            summary[name] = grouped
    return summary
