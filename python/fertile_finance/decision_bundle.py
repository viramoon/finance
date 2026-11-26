"""Adapt ordered XNYS close observations to the shared StrategyBundle evaluator."""

import json
import re

from nautilus_trader.model.data import Bar
from nautilus_trader.model.objects import Quantity

from .epoch import unix_nanos
from .market_protocol import DecimalText, U64Text
from .research_catalog import ID, bar_type, iso_date, nyse, price
from .strategy_evaluation import _unique_object
from .strategy_plan import canonical_sha256
from .strategy_signals import BundleSession, BundleSignalEvaluator, SleeveState


SCHEMA = "fertile.finance/decision-session/v1"
_FIELDS = {
    "schema", "session", "warmup", "members", "bars", "cash", "sleeves", "pending_reductions",
}
_PRICES = ("open", "high", "low", "close")
_VOLUME = re.compile(r"[0-9]{1,12}")
_INVALID_INPUT = (ValueError, TypeError, KeyError, ArithmeticError, AttributeError, RecursionError)


def _object(value, fields):
    if not isinstance(value, dict) or value.keys() != fields:
        raise ValueError("Invalid StrategyBundle replay fields")
    return value


def _instrument(value):
    if not isinstance(value, str) or not ID.fullmatch(value):
        raise ValueError("Invalid XNYS instrument")
    return value


def _instruments(values):
    if not isinstance(values, list):
        raise ValueError("Expected an instrument array")
    keys = frozenset(_instrument(value) for value in values)
    if len(keys) != len(values):
        raise ValueError("Duplicate instrument")
    return keys


def _session_bar(instrument, row, close_ts):
    _object(row, {*_PRICES, "volume", "first_available_ts"})
    if U64Text(row["first_available_ts"]).integer != close_ts or not _VOLUME.fullmatch(row["volume"]):
        raise ValueError("Expected an on-time closed bar and whole-share volume")
    prices = tuple(price(row[field]) for field in _PRICES)
    opening, high, low, close = (item.raw for item in prices)
    if not low <= min(opening, close) <= max(opening, close) <= high:
        raise ValueError("Invalid OHLC bounds")
    return Bar(bar_type(instrument), *prices, Quantity.from_int(int(row["volume"])), close_ts, close_ts)


def _sleeve(row):
    _object(row, {"shares", "sessions_held", "exited"})
    raw_shares, raw_ages = row["shares"], row["sessions_held"]
    if not isinstance(raw_shares, dict) or not isinstance(raw_ages, dict) or raw_shares.keys() != raw_ages.keys():
        raise ValueError("Holdings and fill ages must identify the same instruments")
    shares = {_instrument(key): U64Text(value).integer for key, value in raw_shares.items()}
    if any(quantity == 0 for quantity in shares.values()):
        raise ValueError("Sleeve holdings must be positive")
    ages = {key: U64Text(value).integer for key, value in raw_ages.items()}
    return SleeveState(shares=shares, sessions_held=ages, exited=_instruments(row["exited"]))


def _pending_reductions(rows, sleeves):
    if not isinstance(rows, list):
        raise ValueError("Expected an outstanding reductions array")
    result = {}
    for row in rows:
        _object(row, {"instrument_id", "recipe", "quantity"})
        instrument, recipe = _instrument(row["instrument_id"]), row["recipe"]
        key = (instrument, recipe)
        quantity = row["quantity"]
        if key in result or not isinstance(quantity, str) or not quantity.startswith("-"):
            raise ValueError("Expected a unique negative outstanding reduction")
        reduction = U64Text(quantity[1:]).integer
        if not 0 < reduction <= sleeves[recipe].shares.get(instrument, 0):
            raise ValueError("Outstanding reduction exceeds sleeve holdings")
        result[key] = -reduction
    return result


class BundleDecisionReplay:
    def __init__(self, plan):
        try:
            self.plan_sha256 = canonical_sha256(plan)
            spec = plan["bundle"]["spec"]
            clock = spec["clock"]
            self.warmup_sessions = clock["warmup_sessions"]
            if clock["calendar"] != "XNYS" or type(self.warmup_sessions) is not int or self.warmup_sessions < 0:
                raise ValueError("Invalid StrategyBundle clock")
            self.recipes = {row["recipe"] for row in plan["sleeves"]}
            self.evaluator = BundleSignalEvaluator(plan)
        except _INVALID_INPUT:
            raise ValueError("Invalid compiled StrategyBundle plan") from None
        self.schedule = nyse().schedule
        self.previous_index = None
        self.warmed = 0
        self.active = False

    def initialize(self, connection):
        connection.execute("""
            CREATE TABLE IF NOT EXISTS bundle_decisions (
                replay_offset INTEGER NOT NULL,
                plan_sha256 TEXT NOT NULL,
                session TEXT NOT NULL,
                signal_ts TEXT NOT NULL,
                recipe TEXT NOT NULL,
                instrument_id TEXT NOT NULL,
                target_shares TEXT NOT NULL,
                reason TEXT,
                PRIMARY KEY (replay_offset, recipe, instrument_id)
            )
        """)

    def decode(self, raw, offset):
        try:
            row = _object(json.loads(raw.decode("utf-8"), object_pairs_hook=_unique_object), _FIELDS)
            warmup = row["warmup"]
            if row["schema"] != SCHEMA or type(warmup) is not bool:
                raise ValueError("Invalid StrategyBundle replay version or warmup flag")
            day = iso_date(row["session"])
            index = self.schedule.index.get_loc(str(day))
            if self.previous_index is not None and index != self.previous_index + 1:
                raise ValueError("Expected consecutive XNYS sessions")
            if (warmup and self.active) or (not warmup and self.warmed < self.warmup_sessions):
                raise ValueError("Invalid StrategyBundle warmup prefix")
            close_ts = unix_nanos(self.schedule.at[str(day), "close"].value)
            cash = DecimalText(row["cash"]).decimal
            if cash < 0 or not isinstance(row["bars"], dict):
                raise ValueError("Expected nonnegative cash and a bar mapping")
            sleeves = {key: _sleeve(value) for key, value in _object(row["sleeves"], self.recipes).items()}
            frame = BundleSession(
                day=day, ts_event=close_ts, members=_instruments(row["members"]), cash=cash,
                bars={_instrument(key): _session_bar(key, value, close_ts) for key, value in row["bars"].items()},
                sleeves=sleeves, pending_reductions=_pending_reductions(row["pending_reductions"], sleeves),
                emit=not warmup,
            )
        except _INVALID_INPUT:
            raise ValueError(f"invalid StrategyBundle replay frame at byte {offset}") from None
        self.previous_index = index
        self.active = not warmup
        self.warmed += warmup
        return frame, close_ts

    def evaluate(self, frame, offset):
        try:
            return self.evaluator.evaluate(frame)
        except _INVALID_INPUT:
            raise ValueError(f"could not evaluate StrategyBundle replay frame at byte {offset}") from None

    def record(self, connection, offset, frame, signal):
        connection.executemany("""
            INSERT INTO bundle_decisions (
                replay_offset, plan_sha256, session, signal_ts, recipe, instrument_id, target_shares, reason
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
        """, (
            (offset, self.plan_sha256, str(frame.day), str(frame.ts_event), recipe, instrument, str(target),
             signal.reasons.get((instrument, recipe)))
            for (instrument, recipe), target in sorted(signal.targets.items())
        ))
