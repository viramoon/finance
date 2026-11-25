"""Point-in-time inputs for the XNYS portfolio research API."""

from bisect import bisect_left, bisect_right
from collections.abc import Collection, Mapping, Sequence
from dataclasses import dataclass
from datetime import date, timedelta
from decimal import Context, Decimal, localcontext
from fractions import Fraction

from nautilus_trader.model.currencies import USD
from nautilus_trader.model.data import Bar
from nautilus_trader.model.instruments import Equity
from nautilus_trader.model.objects import Price, Quantity

from .collector_store import NS_PER_DAY, unix_nanos
from .historical_catalog import YahooHistory
from .historical_types import DailyBar
from .research_catalog import CENT, EPOCH, ID, LATEST, ResearchError, bar_type, cash_equity, nyse


@dataclass(frozen=True)
class PortfolioAsset:
    """A cash equity and its reviewed, original daily OHLCV bars."""

    instrument: Equity
    bars: Sequence[Bar]
    raw_split_free_reviewed: bool

    @classmethod
    def from_yahoo(cls, history: YahooHistory, *, raw_split_free_reviewed: bool) -> "PortfolioAsset":
        """Convert archived source bars after verifying prices are raw and split-free.

        The caller must review supplier normalization as well as the selected span.
        A bounded Yahoo read can omit later splits that affected stored prices.
        """
        if not isinstance(history, YahooHistory) or not ID.fullmatch(history.series_id) or (
            history.asset_type not in {"stock", "etf"} or history.currency != "USD"
        ):
            raise ResearchError("invalid_instrument", "Expected a Yahoo XNYS USD stock or ETF history")
        if raw_split_free_reviewed is not True or history.splits:
            raise ResearchError("invalid_source", "Yahoo prices require raw price review and a split-free history")
        kind = bar_type(history.series_id)
        schedule = nyse().schedule
        bars = []
        for row in history.bars:
            if not isinstance(row, DailyBar):
                raise ResearchError("invalid_source", "Expected Yahoo source bars with original OHLCV")
            when = _session_time(schedule, row.session, "close")
            if type(row.ts_event) is not int or row.ts_event != when or row.ts_init != when:
                raise ResearchError("invalid_bar", f"{history.series_id} {row.session}: expected archived XNYS close")
            if not row.is_missing_quote:
                bars.append(_yahoo_quote_bar(kind, row, when))
        return cls(cash_equity(history.series_id), tuple(bars), raw_split_free_reviewed=True)


def _yahoo_quote_bar(kind, row, when):
    prices = (row.open, row.high, row.low, row.close)
    volume = row.volume
    if any(not isinstance(value, Decimal) or not value.is_finite() for value in (*prices, volume)):
        raise ResearchError("invalid_bar", f"{kind} {row.session}: expected a complete finite Yahoo quote")
    try:
        with localcontext(Context(prec=34)):
            cents = tuple(value.quantize(CENT) for value in prices)
            shares = volume.quantize(Decimal(1))
            if (cents != prices or any(value <= 0 for value in cents)
                    or shares != volume or shares < 0):
                raise ValueError("inexact or invalid quote")
            native_prices = tuple(Price.from_decimal(value) for value in cents)
            native_volume = Quantity.from_decimal(shares)
            return Bar(kind, *native_prices, native_volume, when, when)
    except (ArithmeticError, ValueError) as exc:
        raise ResearchError("invalid_bar", f"{kind} {row.session}: expected representable USD cents and whole shares") from exc


@dataclass(frozen=True)
class UniverseSnapshot:
    """Complete membership effective from the named session open."""

    date: date
    available_ts: int
    members: Collection[str]


@dataclass(frozen=True)
class TargetWeights:
    """Long-only weights effective from the named session close."""

    date: date
    available_ts: int
    weights: Mapping[str, Decimal]


@dataclass(frozen=True)
class FactorValue:
    """A score usable at the named session close if already available."""

    date: date
    available_ts: int
    instrument_id: str
    value: Decimal


@dataclass(frozen=True)
class DataGap:
    date: date
    instrument_id: str
    kind: str
    filled_from: date | None = None


@dataclass
class _PortfolioInputs:
    assets: dict[str, Equity]
    bars: dict[date, dict[str, Bar]]
    opens: dict[date, int]
    closes: dict[date, int]
    members: dict[date, frozenset[str]]
    _entered: dict[date, dict[str, date]]
    _target_dates: list[date]
    _weights: dict[date, dict[str, Decimal]]
    _factors: dict[str, dict[date, Decimal]]
    _factor_dates: dict[str, list[date]]
    _price_dates: dict[str, list[date]]
    _top_n: int | None

    def _prior_date(self, day, instrument_id, dates):
        entered = self._entered[day].get(instrument_id)
        previous = bisect_left(dates, day) - 1
        if entered is not None and previous >= 0 and entered <= dates[previous]:
            return dates[previous]
        return None

    def signal_bar(self, day, instrument_id, *, fill_missing=False):
        """Return the current real print or an explicitly carried earlier print."""
        if instrument_id not in self.members[day]:
            return None, None
        bar = self.bars[day].get(instrument_id)
        if bar is not None and bar.volume.as_decimal() > 0:
            return bar, day
        if fill_missing:
            previous = self._prior_date(day, instrument_id, self._price_dates[instrument_id])
            if previous is not None:
                return self.bars[previous][instrument_id], previous
        return None, None

    def resolve_targets(self, day, *, fill_missing=False):
        """Select weights from eligible members and return the observed input gaps."""
        gaps = []
        eligible = []
        for instrument_id in sorted(self.members[day]):
            raw = self.bars[day].get(instrument_id)
            if raw is not None and raw.volume.as_decimal() == 0:
                gaps.append(DataGap(day, instrument_id, "zero_volume"))
                continue
            bar, price_date = self.signal_bar(day, instrument_id, fill_missing=fill_missing)
            if price_date != day:
                gaps.append(DataGap(day, instrument_id, "missing_price", price_date))

            factor_date = day
            if self._top_n is not None and day not in self._factors[instrument_id]:
                factor_date = None
                if fill_missing:
                    factor_date = self._prior_date(day, instrument_id, self._factor_dates[instrument_id])
                gaps.append(DataGap(day, instrument_id, "missing_factor", factor_date))
            if bar is not None and factor_date is not None:
                eligible.append((instrument_id, factor_date))

        if self._top_n is not None:
            ranked = sorted(eligible, key=lambda item: self._factors[item[0]][item[1]], reverse=True)
            selected = ranked[:self._top_n]
            if not selected:
                return {}, gaps
            weight = Fraction(1, len(selected))
            return {key: weight for key, _ in selected}, gaps

        index = bisect_right(self._target_dates, day) - 1
        if index < 0:
            return {}, gaps
        target_day = self._target_dates[index]
        weights = self._weights[target_day]
        return ({key: weights[key] for key, _ in eligible
                 if key in weights and self._entered[day][key] <= target_day}, gaps)


def _valid_date(day):
    if type(day) is not date or not EPOCH <= day <= LATEST:
        raise ResearchError("invalid_date", "Use a date from 1970-01-01 through 2100-12-31")
    return day


def _session_time(schedule, day, field):
    _valid_date(day)
    try:
        return unix_nanos(schedule.at[day.isoformat(), field].value)
    except KeyError:
        raise ResearchError("invalid_session", f"{day} is not an XNYS session") from None


def _require_known(available_ts, deadline, description):
    try:
        unix_nanos(available_ts)
    except ValueError as exc:
        raise ResearchError("invalid_timestamp", f"{description}: {exc}") from None
    if available_ts > deadline:
        raise ResearchError("lookahead", f"{description} is only available after its required session time")


def _prepare_assets(assets, schedule, closes):
    instruments = {}
    bars = {day: {} for day in closes}
    for asset in assets:
        if not isinstance(asset, PortfolioAsset):
            raise ResearchError("invalid_instrument", "Supply each equity as a PortfolioAsset")
        instrument = asset.instrument
        if not isinstance(instrument, Equity) or not ID.fullmatch(str(instrument.id)) or (
            instrument.quote_currency != USD or instrument.price_precision != 2
            or instrument.price_increment.as_decimal() != CENT or instrument.size_precision != 0
            or instrument.lot_size.as_decimal() != 1
        ):
            raise ResearchError("invalid_instrument", "Expected cent-priced XNYS equities in USD with one-share lots")
        if asset.raw_split_free_reviewed is not True:
            raise ResearchError("invalid_source", "Review supplied prices as raw, unadjusted and split-free")
        key = str(instrument.id)
        if key in instruments:
            raise ResearchError("invalid_instrument", f"Duplicate instrument {key}")
        instruments[key] = instrument
        expected_type = bar_type(key)
        observed = set()
        for bar in asset.bars:
            if not isinstance(bar, Bar) or bar.bar_type != expected_type or bar.is_revision:
                raise ResearchError("invalid_bar", f"Expected an original external daily LAST bar for {key}")
            try:
                when = unix_nanos(bar.ts_event)
            except ValueError as exc:
                raise ResearchError("invalid_bar", f"{key}: {exc}") from None
            day = EPOCH + timedelta(days=when // NS_PER_DAY)
            if when != _session_time(schedule, day, "close") or day in observed:
                raise ResearchError("invalid_bar", f"Expected one bar at the official close for {key} on {day}")
            if bar.ts_init != when:
                raise ResearchError("invalid_bar", f"Bar availability must equal the official close for {key} on {day}")
            prices = (bar.open, bar.high, bar.low, bar.close)
            opening, high, low, close = (value.as_decimal() for value in prices)
            if (any(value.precision != instrument.price_precision for value in prices)
                    or not 0 < low <= min(opening, close) <= max(opening, close) <= high
                    or bar.volume.precision != instrument.size_precision):
                raise ResearchError("invalid_bar", f"Invalid cent USD OHLC or whole-share volume for {key} on {day}")
            observed.add(day)
            if day in bars:
                bars[day][key] = bar
    if len(instruments) < 2:
        raise ResearchError("invalid_instrument", "Supply at least two distinct XNYS equities")
    if not any(bars.values()):
        raise ResearchError("no_data", "No portfolio bars in the requested range")
    return instruments, bars


def _prepare_universe(universe, instruments, schedule, opens):
    snapshots = {}
    for row in universe:
        if not isinstance(row, UniverseSnapshot) or not isinstance(row.members, Collection) or (
            isinstance(row.members, (str, bytes)) or any(not isinstance(key, str) for key in row.members)
        ):
            raise ResearchError("invalid_universe", "Supply UniverseSnapshot rows with a collection of instrument IDs")
        deadline = _session_time(schedule, row.date, "open")
        _require_known(row.available_ts, deadline, f"Universe snapshot for {row.date}")
        members = frozenset(row.members)
        if row.date in snapshots or len(members) != len(row.members) or not members <= instruments.keys():
            raise ResearchError("invalid_universe", "Use one snapshot per date with unique, supplied instrument IDs")
        snapshots[row.date] = members
    if not snapshots or min(snapshots) > min(opens):
        raise ResearchError("invalid_universe", "A point-in-time snapshot is required by the first session open")

    changes = iter(sorted(snapshots.items()))
    next_change = next(changes)
    members_by_date = {}
    entered_by_date = {}
    current = frozenset()
    entered = {}
    for day in opens:
        while next_change is not None and next_change[0] <= day:
            effective, current = next_change
            entered = {key: entered.get(key, effective) for key in current}
            next_change = next(changes, None)
        members_by_date[day] = current
        entered_by_date[day] = entered
    return members_by_date, entered_by_date


def _prepare_weights(rows, instruments, schedule):
    weights = {}
    for row in rows:
        if not isinstance(row, TargetWeights) or not isinstance(row.weights, Mapping):
            raise ResearchError("invalid_weights", "Supply TargetWeights rows with an instrument-to-weight mapping")
        deadline = _session_time(schedule, row.date, "close")
        _require_known(row.available_ts, deadline, f"Target weights for {row.date}")
        if row.date in weights or not row.weights.keys() <= instruments.keys():
            raise ResearchError("invalid_weights", "Use at most one target per date and supplied instrument IDs")
        values = row.weights.values()
        if any(not isinstance(value, Decimal) or not value.is_finite() or not 0 <= value <= 1 for value in values):
            raise ResearchError("invalid_weights", "Each target weight must be a finite Decimal between zero and one")
        if sum(map(Fraction, values)) > 1:
            raise ResearchError("invalid_weights", "Total target weight cannot exceed one")
        weights[row.date] = dict(row.weights)
    if not weights:
        raise ResearchError("invalid_weights", "Provide at least one dated target weight row")
    return weights


def _prepare_factors(rows, instruments, schedule):
    factors = {key: {} for key in instruments}
    for row in rows:
        if not isinstance(row, FactorValue) or not isinstance(row.instrument_id, str):
            raise ResearchError("invalid_factor", "Supply FactorValue rows with an instrument ID")
        deadline = _session_time(schedule, row.date, "close")
        _require_known(row.available_ts, deadline, f"Factor for {row.instrument_id} on {row.date}")
        values = factors.get(row.instrument_id)
        if values is None or row.date in values or not isinstance(row.value, Decimal) or not row.value.is_finite():
            raise ResearchError("invalid_factor", "Use one finite Decimal factor per supplied instrument and date")
        values[row.date] = row.value
    return factors


def prepare_portfolio(assets, *, start, end, universe, target_weights=None, factors=None, top_n=None):
    """Validate chronological input contracts before running Nautilus."""
    _valid_date(start)
    _valid_date(end)
    if start > end:
        raise ResearchError("invalid_date", "Portfolio start must be on or before end")
    if (target_weights is None) == (factors is None):
        raise ResearchError("invalid_parameters", "Choose dated target weights or cross-sectional factors")
    if factors is not None and (type(top_n) is not int or top_n < 1):
        raise ResearchError("invalid_parameters", "Factor ranking requires a positive integer top_n")
    if target_weights is not None and top_n is not None:
        raise ResearchError("invalid_parameters", "top_n applies to factor ranking")

    schedule = nyse().schedule
    days = schedule.loc[start.isoformat():end.isoformat()]
    opens = {label.date(): unix_nanos(value.value) for label, value in days["open"].items()}
    closes = {label.date(): unix_nanos(value.value) for label, value in days["close"].items()}
    if len(opens) < 2:
        raise ResearchError("no_data", "Require at least two XNYS sessions")
    instruments, bars = _prepare_assets(assets, schedule, closes)
    members, entered = _prepare_universe(universe, instruments, schedule, opens)
    weights = _prepare_weights(target_weights, instruments, schedule) if target_weights is not None else {}
    scores = _prepare_factors(factors, instruments, schedule) if factors is not None else {}
    factor_dates = {key: sorted(values) for key, values in scores.items()}
    price_dates = {key: [] for key in instruments}
    for day, by_instrument in bars.items():
        for key, bar in by_instrument.items():
            if bar.volume.as_decimal() > 0:
                price_dates[key].append(day)
    return _PortfolioInputs(instruments, bars, opens, closes, members, entered,
                            sorted(weights), weights, scores, factor_dates, price_dates, top_n)
