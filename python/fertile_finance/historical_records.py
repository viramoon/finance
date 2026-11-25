"""Arrow records registered with the pinned Nautilus custom-data writer."""

from dataclasses import fields
from datetime import date
from decimal import Decimal

from nautilus_trader.core.data import Data
from nautilus_trader.model.custom import customdataclass

from .historical_types import DailyBar, Dividend, Split
from .research_factors import FredObservation, FrenchFactor


# The vendor's schema inference accepts str and int, and makes columns nullable.
# Decimal text preserves source scale and avoids a per-partition fixed point limit.
@customdataclass
class YahooSourceBar(Data):
    session: str
    open: str
    high: str
    low: str
    close: str
    volume: str
    adjustment_factor: str
    adjusted_close: str
    adjustment_as_of: str


@customdataclass
class YahooIndexBar(Data):
    session: str
    open: str
    high: str
    low: str
    close: str
    volume: str
    adjustment_factor: str
    adjusted_close: str
    adjustment_as_of: str


@customdataclass
class YahooDividend(Data):
    session: str
    source_timestamp: int
    amount: str
    source_id: str


@customdataclass
class YahooSplit(Data):
    session: str
    source_timestamp: int
    numerator: int
    denominator: int
    source_id: str


@customdataclass
class KenFrenchDaily(Data):
    session: str
    mkt_rf: str
    smb: str
    hml: str
    rf: str
    mom: str
    available_on: str


@customdataclass
class FredDailyObservation(Data):
    observation_date: str
    value: str
    realtime_start: str
    realtime_end: str


def encode_record(record_type, row, when):
    values = {}
    for field in fields(record_type):
        value = getattr(row, field.name)
        values[field.name] = str(value) if isinstance(value, (date, Decimal)) else value
    return record_type(ts_event=when, ts_init=when, **values)


def _decimal(value):
    return None if value is None else Decimal(value)


def _date(value):
    return None if value is None else date.fromisoformat(value)


def daily_bar(row):
    return DailyBar(
        date.fromisoformat(row.session), _decimal(row.open), _decimal(row.high), _decimal(row.low),
        _decimal(row.close), _decimal(row.volume), _decimal(row.adjustment_factor),
        _decimal(row.adjusted_close), date.fromisoformat(row.adjustment_as_of), row.ts_event,
    )


def dividend(row):
    return Dividend(date.fromisoformat(row.session), row.source_timestamp, Decimal(row.amount), row.source_id)


def split(row):
    return Split(date.fromisoformat(row.session), row.source_timestamp, row.numerator, row.denominator, row.source_id)


def french_factor(row):
    return FrenchFactor(
        date.fromisoformat(row.session), _decimal(row.mkt_rf), _decimal(row.smb), _decimal(row.hml),
        _decimal(row.rf), _decimal(row.mom), _date(row.available_on),
    )


def fred_observation(row):
    return FredObservation(
        date.fromisoformat(row.observation_date), _decimal(row.value), _date(row.realtime_start), _date(row.realtime_end),
    )
