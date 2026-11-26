"""Calendar coverage for daily equity curves and monthly return observations."""

from calendar import monthrange
from collections.abc import Collection, Mapping
from datetime import date, timedelta
from decimal import Decimal, localcontext
from itertools import pairwise
from statistics import mean, stdev

import exchange_calendars
from exchange_calendars.errors import NoSessionsError

from .epoch import EPOCH
from .research_catalog import LATEST, nyse
from .research_decimal import RESEARCH_CONTEXT


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


def month_end(day: date, offset: int = 0) -> date:
    """Return the calendar month-end offset from the month containing day."""
    year, month_index = divmod(day.year * 12 + day.month - 1 + offset, 12)
    month = month_index + 1
    return date(year, month, monthrange(year, month)[1])


def monthly_coverage(
    observed: Collection[date], *, name: str, start: date | None, end: date,
) -> dict:
    """Check every monthly return label from start through end (both inclusive)."""
    if any(type(day) is not date or day != month_end(day) for day in (*observed, end)):
        raise ValueError("Monthly observations and the cutoff must use calendar month-end dates")
    if len(set(observed)) != len(observed):
        raise ValueError("Monthly observation dates must be unique")
    if start is not None and (type(start) is not date or start != month_end(start)):
        raise ValueError("Monthly start must be a calendar month-end")

    actual = sorted(day for day in observed if day <= end and (start is None or day >= start))
    first = actual[0] if actual else None
    last = actual[-1] if actual else None
    first_expected = start or first
    expected = (max(0, (end.year - first_expected.year) * 12 + end.month - first_expected.month + 1)
                if first_expected is not None else 0)
    missing = expected - len(actual)
    full = bool(actual) and not missing
    with localcontext(RESEARCH_CONTEXT):
        missing_pct = percent(Decimal(missing) / expected) if expected else None
    row = {
        "window": name,
        "label": name if full else f"{name} (insufficient)",
        "requested_start": first_expected.isoformat() if first_expected is not None else None,
        "requested_end": end.isoformat(),
        "requested_baseline_date": (month_end(first_expected, -1).isoformat()
                                    if first_expected is not None else None),
        "actual_start": first.isoformat() if first is not None else None,
        "actual_end": last.isoformat() if last is not None else None,
        "nav_baseline_date": month_end(first, -1).isoformat() if first is not None else None,
        "observations": len(actual),
        "expected_months": expected,
        "missing_months": missing,
        "missing_pct": missing_pct,
        "coverage": "full" if full else "insufficient",
    }
    if start is not None and start > end:
        row["coverage_reason"] = "cutoff_before_requested_start"
    elif not actual:
        row["coverage_reason"] = "no_monthly_observations"
    elif missing:
        row["coverage_reason"] = "missing_monthly_observations"
    return row


def monthly_coverage_windows(observed: Collection[date], end: date) -> list[dict]:
    """Use the standard four window names with strictly complete monthly coverage.

    Fixed windows include monthly returns after the N-year NAV baseline through
    end; longest_sample begins with the first observed return and ends at end.
    Pre-epoch labels are valid for offline monthly analysis only.
    """
    rows = []
    for years in WINDOW_YEARS:
        start = month_end(years_before(end, years), 1)
        row = monthly_coverage(observed, name=f"{years}y", start=start, end=end)
        rows.append({**row, "requested_years": years})
    longest = monthly_coverage(observed, name="longest_sample", start=None, end=end)
    rows.append({**longest, "requested_years": None})
    return rows


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


def _calendar_limits(name):
    if name == "XNYS":
        return EPOCH, LATEST

    # A default schedule is only a way to discover declared bounds. Some
    # calendars can build an explicit window even when their default fails.
    try:
        calendar = exchange_calendars.get_calendar(name)
        lower, upper = calendar.bound_min(), calendar.bound_max()
    except Exception:
        return EPOCH, LATEST
    first_supported = max(EPOCH, lower.date()) if lower is not None else EPOCH
    last_supported = min(LATEST, upper.date()) if upper is not None else LATEST
    return first_supported, last_supported


def _calendar_sessions(name, first, last, first_supported, last_supported):
    if first > last:
        return ()
    if name == "XNYS":
        return nyse().schedule.loc[str(first):str(last)].index.date

    # The constructor requires a span of at least two calendar dates.
    build_start = max(first_supported, first - timedelta(days=1)) if first == last else first
    build_end = min(last_supported, last + timedelta(days=1)) if first == last else last
    try:
        schedule = exchange_calendars.get_calendar(name, start=str(build_start), end=str(build_end))
    except NoSessionsError:
        return ()
    return schedule.schedule.loc[str(first):str(last)].index.date


def coverage_windows(
    equity: Mapping[date, Decimal],
    end: date | None = None,
    *,
    calendar: str = "XNYS",
    rf: Mapping[date, Decimal] | None = None,
    rf_source: str | None = None,
    turnover: Mapping[date, Decimal] | None = None,
    longest_through_end: bool = False,
) -> list[dict]:
    """Report 50y, 10y, 5y and longest_sample for post-epoch daily observations.

    With longest_through_end, count trailing missing sessions through end in
    longest_sample when a first observation exists.
    """
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

    observed = {day: equity[day] for day in sorted(equity) if day <= end}
    if any(not isinstance(value, Decimal) or not value.is_finite() or value <= 0
           for value in observed.values()):
        raise ValueError("equity must use positive finite Decimal values")
    longest = min(observed, default=None)
    longest_end = end if longest_through_end and longest is not None else max(observed, default=None)
    requests = [(f"{years}y", years, years_before(end, years), end) for years in WINDOW_YEARS]
    requests.append(("longest_sample", None, longest, longest_end))
    calendar = exchange_calendars.resolve_alias(calendar)
    supported_start, supported_end = _calendar_limits(calendar)

    output = []
    for name, years, start, window_end in requests:
        calendar_start = max(supported_start, start) if start is not None else None
        calendar_end = min(supported_end, window_end) if window_end is not None else None
        calendar_out_of_range = start is not None and (
            max(EPOCH, start) < supported_start or window_end > supported_end
        )
        window = {day: value for day, value in observed.items() if start is not None
                  and start <= day <= window_end}
        diagnostic = None
        expected = ()
        if calendar_start is not None:
            try:
                expected = _calendar_sessions(calendar, calendar_start, calendar_end, supported_start, supported_end)
            except Exception as exc:
                # Only isolate failures while constructing/reading the dependency's calendar.
                detail = str(exc)
                if len(detail) > 200:
                    detail = f"{detail[:160]} … {detail[-35:]}"
                diagnostic = f"{calendar} {calendar_start} to {calendar_end}: {type(exc).__name__}: {detail}"
        if diagnostic is None:
            trading_set = set(expected)
            for day in window:
                if calendar_start <= day <= calendar_end and day not in trading_set:
                    raise ValueError(f"{day} is not a {calendar} session")

        first = next(iter(window), None)
        last = next(reversed(window), None)
        missing = [day for day in expected if day not in window]
        full = (diagnostic is None and not calendar_out_of_range and start == calendar_start
                and len(window) >= 2 and len(missing) <= 5 and all(day < first for day in missing))
        elapsed = Decimal((last - first).days) / DAYS_PER_YEAR if first is not None else None
        row = {
            "window": name,
            "label": name if full or years is None else f"{name} (insufficient)",
            "requested_years": years, "requested_start": str(start) if start is not None else None,
            "actual_start": str(first) if first is not None else None,
            "actual_end": str(last) if last is not None else None,
            "actual_years": format(elapsed, ".4f") if elapsed is not None else None,
            "calendar_start": str(calendar_start) if diagnostic is None and calendar_start is not None else None,
            "calendar_end": str(calendar_end) if diagnostic is None and calendar_end is not None else None,
            "observations": len(window),
            "expected_sessions": len(expected) if diagnostic is None else None,
            "missing_sessions": len(missing) if diagnostic is None else None,
            "missing_pct": percent(Decimal(len(missing)) / len(expected)) if len(expected) else None,
            "coverage": "full" if full else "insufficient",
            "rf_source": source,
            "metrics": _window_metrics(window, elapsed, rf, turnover) if full else None,
        }
        if diagnostic is not None:
            row["coverage_reason"] = "calendar_unavailable"
            row["calendar_diagnostic"] = diagnostic
        elif calendar_out_of_range:
            row["coverage_reason"] = "calendar_out_of_range"
        output.append(row)
    return output
