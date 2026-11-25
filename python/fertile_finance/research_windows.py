"""Calendar coverage and performance metrics for daily research equity curves."""

from collections.abc import Mapping
from datetime import date, timedelta
from decimal import Decimal
from itertools import pairwise
from statistics import mean, stdev

import exchange_calendars

from .epoch import EPOCH
from .research_catalog import LATEST, nyse


WINDOW_YEARS = (50, 10, 5)
DAYS_PER_YEAR = Decimal("365.2425")
SESSIONS_PER_YEAR = Decimal(252)


def percent(value):
    return format(value * 100, ".4f")


def metrics(equity):
    if len(equity) < 2:
        return None
    values = [equity[day] for day in sorted(equity)]
    high = values[0]
    drawdown = Decimal(0)
    for value in values:
        high = max(high, value)
        drawdown = max(drawdown, 1 - value / high)
    return {
        "return_pct": percent(values[-1] / values[0] - 1),
        "max_drawdown_pct": percent(drawdown),
    }


def years_before(day, years):
    try:
        return day.replace(year=day.year - years)
    except ValueError:
        return day.replace(year=day.year - years, day=28)


def _series_values(series, dates, name):
    values = []
    for day in dates:
        if day not in series:
            return None
        value = series[day]
        if not isinstance(value, Decimal) or not value.is_finite():
            raise ValueError(f"{name} must use finite Decimal values")
        if name == "turnover" and value < 0:
            raise ValueError("turnover must be nonnegative")
        values.append(value)
    return values


def _window_metrics(equity, elapsed, rf, turnover):
    summary = metrics(equity)
    values = list(equity.values())
    returns = [close / prior - 1 for prior, close in pairwise(values)]
    dates = list(equity)[1:]
    vol = stdev(returns) if len(returns) > 1 else None
    risk_free = _series_values(rf, dates, "rf") if rf is not None else None
    excess, excess_vol = returns, vol
    if rf is not None:
        excess = ([daily - rate for daily, rate in zip(returns, risk_free, strict=True)]
                  if risk_free is not None else [])
        excess_vol = stdev(excess) if len(excess) > 1 else None
    annual_scale = SESSIONS_PER_YEAR.sqrt()
    traded = _series_values(turnover, dates, "turnover") if turnover is not None else None
    return {
        **summary,
        "cagr_pct": percent((values[-1] / values[0]) ** (1 / elapsed) - 1),
        "sharpe": format(mean(excess) / excess_vol * annual_scale, ".4f") if excess_vol else None,
        "volatility_pct": percent(vol * annual_scale) if vol is not None else None,
        "turnover_pct": percent(sum(traded)) if traded is not None else None,
    }


def coverage_windows(
    equity: Mapping[date, Decimal],
    end: date | None = None,
    *,
    calendar: str = "XNYS",
    rf: Mapping[date, Decimal] | None = None,
    rf_source: str | None = None,
    turnover: Mapping[date, Decimal] | None = None,
) -> list[dict]:
    """Report 50y, 10y, 5y and longest_sample for post-epoch daily observations."""
    for day in equity:
        if type(day) is not date or not EPOCH <= day <= LATEST:
            raise ValueError("equity dates must be calendar dates from 1970-01-01 through 2100-12-31")
    if end is None:
        end = max(equity, default=None)
    if type(end) is not date or not EPOCH <= end <= LATEST:
        raise ValueError("end must be a calendar date from 1970-01-01 through 2100-12-31")
    if rf is None and rf_source is not None:
        raise ValueError("rf_source requires an RF series")
    source = "rf=0" if rf is None else (rf_source or "provided")

    observed = {day: value for day, value in equity.items() if day <= end}
    if any(not isinstance(value, Decimal) or not value.is_finite() or value <= 0
           for value in observed.values()):
        raise ValueError("equity must use positive finite Decimal values")
    longest = min(observed, default=None)
    requests = [(f"{years}y", years, years_before(end, years), end) for years in WINDOW_YEARS]
    requests.append(("longest_sample", None, longest, max(observed, default=None)))
    first_date = max(EPOCH, min(start for _, _, start, _ in requests if start is not None))
    schedule = nyse() if calendar == "XNYS" else exchange_calendars.get_calendar(
        calendar, start=str(first_date), end=str(max(end, first_date + timedelta(days=7))),
    )
    trading_days = [day.date() for day in schedule.schedule.loc[str(first_date):str(end)].index]
    if outside := observed.keys() - set(trading_days):
        raise ValueError(f"{min(outside)} is not a {calendar} session")

    output = []
    for name, years, start, window_end in requests:
        calendar_start = max(EPOCH, start) if start is not None else None
        expected = [day for day in trading_days if calendar_start is not None
                    and calendar_start <= day <= window_end]
        window = {day: observed[day] for day in expected if day in observed}
        first = next(iter(window), None)
        last = next(reversed(window), None)
        missing = [day for day in expected if day not in window]
        full = (start == calendar_start and len(window) >= 2 and len(missing) <= 5
                and all(day < first for day in missing))
        elapsed = Decimal((last - first).days) / DAYS_PER_YEAR if first is not None else None
        output.append({
            "window": name,
            "label": name if full or years is None else f"{name} (insufficient)",
            "requested_years": years, "requested_start": str(start) if start is not None else None,
            "actual_start": str(first) if first is not None else None,
            "actual_end": str(last) if last is not None else None,
            "actual_years": format(elapsed, ".4f") if elapsed is not None else None,
            "calendar_start": str(calendar_start) if calendar_start is not None else None,
            "observations": len(window), "expected_sessions": len(expected), "missing_sessions": len(missing),
            "missing_pct": percent(Decimal(len(missing)) / len(expected)) if expected else None,
            "coverage": "full" if full else "insufficient",
            "rf_source": source,
            "metrics": _window_metrics(window, elapsed, rf, turnover) if full else None,
        })
    return output
