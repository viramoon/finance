"""Point-in-time risk-free rates and Sharpe calculations for daily research."""

from collections.abc import Iterable, Mapping, Sequence
from dataclasses import dataclass
from datetime import date
from decimal import Decimal, localcontext
from itertools import pairwise
from statistics import mean, stdev
from typing import Literal

from .research_decimal import RESEARCH_CONTEXT
from .research_factors import FrenchDaily, FrenchRiskFree


TRADING_DAYS = Decimal(252)
_EPOCH = date(1970, 1, 1)
_NS_PER_DAY = 86_400_000_000_000

type EquityClose = tuple[date, int, Decimal | None]


def _utc_day_index(ts: int) -> int:
    if type(ts) is not int or not 0 <= ts <= 2**63 - 1:
        raise ValueError("Expected nonnegative UTC Unix nanoseconds within int64")
    return ts // _NS_PER_DAY


@dataclass(frozen=True)
class RiskFreeRate:
    """A decimal rate for an observation date, known from available_ts (UTC ns)."""

    observation_date: date
    available_ts: int
    rate: Decimal

    def __post_init__(self) -> None:
        if type(self.observation_date) is not date:
            raise ValueError("RF observation_date must be a calendar date")
        if _utc_day_index(self.available_ts) < (self.observation_date - _EPOCH).days:
            raise ValueError("RF availability cannot precede its observation date")
        if not isinstance(self.rate, Decimal) or not self.rate.is_finite() or self.rate <= -1:
            raise ValueError("RF rate must be a finite Decimal greater than -1")


@dataclass(frozen=True)
class RiskFreeSeries:
    """A named set of daily or effective annual rates expressed as fractions."""

    source: str
    basis: Literal["daily", "annual"]
    rates: Sequence[RiskFreeRate]

    def __post_init__(self) -> None:
        if not isinstance(self.source, str) or not self.source.strip():
            raise ValueError("RF source must be nonempty")
        if self.basis not in ("daily", "annual"):
            raise ValueError("RF basis must be daily or annual")
        rates = tuple(self.rates)
        if any(not isinstance(rate, RiskFreeRate) for rate in rates):
            raise ValueError("RF rates must contain RiskFreeRate entries")
        keys = {(rate.available_ts, rate.observation_date) for rate in rates}
        if len(rates) != len(keys):
            raise ValueError("Duplicate RF observation and availability time")
        object.__setattr__(self, "rates", tuple(sorted(rates, key=lambda rate: rate.available_ts)))

    @classmethod
    def from_french(
        cls,
        batch: FrenchDaily | FrenchRiskFree,
        *,
        available_ts: Mapping[date, int],
        source: str = "Ken French",
    ) -> "RiskFreeSeries":
        """Use daily French fractions with verified UTC release times for this archive vintage."""
        if not isinstance(batch, (FrenchDaily, FrenchRiskFree)):
            raise ValueError("Expected a FrenchDaily or FrenchRiskFree batch")
        rates = []
        for row in batch.rows:
            if row.rf is None:
                continue
            if row.session not in available_ts:
                raise ValueError(f"Missing verified Ken French RF availability timestamp for {row.session}")
            rates.append(RiskFreeRate(row.session, available_ts[row.session], row.rf))
        return cls(source, "daily", rates)


def _validate_close(day: date, close_ts: int) -> None:
    if type(day) is not date or _utc_day_index(close_ts) != (day - _EPOCH).days:
        raise ValueError("Equity close timestamp must fall on its UTC session date")


def _daily_risk_free(
    closes: Iterable[tuple[date, int]], rf: RiskFreeSeries,
) -> dict[date, Decimal]:
    releases = iter(rf.rates)
    upcoming = next(releases, None)
    observation_day = None
    daily_rate = None
    aligned = {}
    with localcontext(RESEARCH_CONTEXT):
        for (_, start_ts), (end_day, _) in pairwise(closes):
            while upcoming is not None and upcoming.available_ts <= start_ts:
                if observation_day is None or upcoming.observation_date >= observation_day:
                    observation_day = upcoming.observation_date
                    daily_rate = ((1 + upcoming.rate) ** (1 / TRADING_DAYS) - 1
                                  if rf.basis == "annual" else upcoming.rate)
                upcoming = next(releases, None)
            if daily_rate is not None:
                aligned[end_day] = daily_rate
    return aligned


def daily_risk_free(closes: Mapping[date, int], rf: RiskFreeSeries) -> dict[date, Decimal]:
    """Align published rates to the ending dates of consecutive close-to-close returns.

    Supply each exchange session date and its close in UTC nanoseconds, including
    sessions with no valuation. The first date and any interval whose starting
    close precedes the first known release are omitted from the result.
    """
    for day, close_ts in closes.items():
        _validate_close(day, close_ts)
    return _daily_risk_free(sorted(closes.items()), rf)


def excess_returns(
    equity: Sequence[EquityClose],
    rf: RiskFreeSeries | None = None,
) -> tuple[Decimal | None, ...]:
    """Subtract the last published rate known at the start of each close-to-close interval.

    Pass one row per consecutive session, using None for an unavailable valuation.
    An interval missing either valuation or an explicitly supplied RF is unavailable.
    """
    closes = {}
    previous_day = None
    for day, close_ts, value in equity:
        _validate_close(day, close_ts)
        if previous_day is not None and day <= previous_day:
            raise ValueError("Equity close dates must be strictly increasing")
        if value is not None and (not isinstance(value, Decimal) or not value.is_finite() or value <= 0):
            raise ValueError("Equity close must be a positive finite Decimal or None")
        previous_day = day
        closes[day] = close_ts

    daily_rates = _daily_risk_free(closes.items(), rf) if rf is not None else None
    result = []
    with localcontext(RESEARCH_CONTEXT):
        for (_, _, start_value), (end_day, _, end_value) in pairwise(equity):
            daily_rate = daily_rates.get(end_day) if daily_rates is not None else Decimal(0)
            if daily_rate is None or start_value is None or end_value is None:
                result.append(None)
            else:
                result.append(end_value / start_value - 1 - daily_rate)
    return tuple(result)


def sharpe_ratio(returns: Sequence[Decimal | None]) -> Decimal | None:
    """Annualize daily excess returns using sample standard deviation and 252 sessions."""
    available = [value for value in returns if value is not None]
    if len(available) < 2 or len(available) != len(returns):
        return None
    with localcontext(RESEARCH_CONTEXT):
        volatility = stdev(available)
        return mean(available) / volatility * TRADING_DAYS.sqrt() if volatility else None
