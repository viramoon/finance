"""Types and strict codecs for the finance market stream protocol 1.0.

Use from_json or from_dict to decode and validate untrusted data. Dataclass
constructors accept values supplied by typed callers.
"""

from dataclasses import dataclass, fields, is_dataclass
from decimal import Decimal, InvalidOperation
from enum import StrEnum
import json
import re
from typing import Self


SCHEMA_VERSION = "1.0"
_MAX_U64 = 2**64 - 1
_MAX_U32 = 2**32 - 1
_U64 = re.compile(r"0|[1-9][0-9]{0,19}")
_DECIMAL = re.compile(r"-?(0|[1-9][0-9]*)(\.[0-9]+)?")
_VENUE_OR_CALENDAR = re.compile(r"[A-Z0-9][A-Z0-9_-]{0,31}")
_LOCAL_KEY = re.compile(r"[A-Z0-9][A-Z0-9_-]*(?:\.[A-Z0-9][A-Z0-9_-]*)*")
_CURRENCY = re.compile(r"[A-Z0-9]{1,16}")
_TOKEN = re.compile(r"[a-z0-9][a-z0-9_.-]{0,63}")
_SURROGATE = re.compile(r"[\ud800-\udfff]")


class ProtocolError(ValueError):
    """The given value or stream transition violates the protocol."""


def _require(condition: bool, path: str, reason: str) -> None:
    if not condition:
        raise ProtocolError(f"{path}: {reason}")


def _text(value: object, path: str) -> str:
    if not isinstance(value, str) or not value or _SURROGATE.search(value):
        raise ProtocolError(f"{path}: expected a nonempty Unicode string")
    return value


class DecimalText(str):
    """Exact decimal text, including the producer's scale and trailing zeros."""

    def __new__(cls, value: object, *, field: str = "decimal") -> Self:
        text = _text(value, field)
        digits = len(text) - text.startswith("-") - ("." in text)
        _require(
            digits <= 28 and _DECIMAL.fullmatch(text) is not None,
            field,
            "expected a plain decimal with at most 28 digits",
        )
        return super().__new__(cls, text)

    @property
    def decimal(self) -> Decimal:
        return Decimal(self)

    def is_multiple_of(self, step: "DecimalText") -> bool:
        numerator, denominator = self.decimal.as_integer_ratio()
        step_numerator, step_denominator = step.decimal.as_integer_ratio()
        return step_numerator != 0 and (numerator * step_denominator) % (denominator * step_numerator) == 0


class U64Text(str):
    """Canonical unsigned 64-bit integer transported as a JSON string."""

    def __new__(cls, value: object, *, field: str = "integer") -> Self:
        text = _text(value, field)
        _require(
            _U64.fullmatch(text) is not None and int(text) <= _MAX_U64,
            field,
            "expected a canonical unsigned 64-bit integer string",
        )
        return super().__new__(cls, text)

    @property
    def integer(self) -> int:
        return int(self)


class AssetKind(StrEnum):
    STOCK = "stock"
    ETF = "etf"
    INDEX = "index"
    SPOT = "spot"
    PERPETUAL = "perpetual"
    OPTION = "option"
    FUTURE = "future"


class SettlementKind(StrEnum):
    CASH = "cash"
    PHYSICAL = "physical"
    NONE = "none"


class DelayTier(StrEnum):
    REALTIME = "realtime"
    DELAYED = "delayed"
    REPLAY = "replay"


class Kind(StrEnum):
    BAR_CLOSED = "bar_closed"
    TRADE = "trade"
    SNAPSHOT = "snapshot"
    DELTA = "delta"
    CORRECTION = "correction"
    GAP = "gap"
    RESET = "reset"
    HEARTBEAT = "heartbeat"
    STATUS = "status"


_MARKET_KINDS = frozenset((Kind.BAR_CLOSED, Kind.TRADE, Kind.SNAPSHOT, Kind.DELTA))
_BOOK_KINDS = frozenset((Kind.SNAPSHOT, Kind.DELTA))


class TradeSide(StrEnum):
    BUY = "buy"
    SELL = "sell"
    UNKNOWN = "unknown"


class BookSide(StrEnum):
    BID = "bid"
    ASK = "ask"


class SourceState(StrEnum):
    CONNECTED = "connected"
    DEGRADED = "degraded"
    DISCONNECTED = "disconnected"


def _object(
    value: object,
    path: str,
    required: tuple[str, ...],
    optional: tuple[str, ...] = (),
) -> dict[str, object]:
    if not isinstance(value, dict) or any(not isinstance(key, str) for key in value):
        raise ProtocolError(f"{path}: expected a JSON object")
    for key in required:
        _require(key in value, f"{path}.{key}", "missing required field")
    for key in value:
        _require(key in required or key in optional, f"{path}.{key}", "unknown field")
    return value


def _array(value: object, path: str) -> list[object]:
    if not isinstance(value, list):
        raise ProtocolError(f"{path}: expected an array")
    return value


def _pattern(value: object, pattern: re.Pattern[str], path: str) -> str:
    text = _text(value, path)
    _require(pattern.fullmatch(text) is not None, path, f"expected {pattern.pattern}")
    return text


def _choice[T: StrEnum](enum: type[T], value: object, path: str) -> T:
    text = _text(value, path)
    try:
        return enum(text)
    except ValueError:
        choices = ", ".join(item.value for item in enum)
        raise ProtocolError(f"{path}: unknown value {text!r}, expected {choices}") from None


def _version(record: dict[str, object], path: str) -> str:
    field = f"{path}.schema_version"
    value = _text(record["schema_version"], field)
    _require(value == SCHEMA_VERSION, field, f"expected {SCHEMA_VERSION!r}")
    return SCHEMA_VERSION


def _instrument_id(value: object, venue: str, path: str) -> str:
    text = _text(value, path)
    local, _, suffix = text.rpartition(".")
    _require(
        len(local) <= 96 and _LOCAL_KEY.fullmatch(local) is not None and suffix == venue,
        path,
        "expected LOCAL.VENUE with a matching venue and a local key of up to 96 characters",
    )
    return text


def _unique_object(pairs: list[tuple[str, object]]) -> dict[str, object]:
    result = {}
    for key, value in pairs:
        _require(key not in result, key, "duplicate field")
        result[key] = value
    return result


def _reject_constant(text: str) -> None:
    raise ProtocolError(f"JSON: unexpected constant {text}")


def _json_integer(text: str) -> int | Decimal:
    # Keep -0 distinct from the accepted unsigned integer spelling 0.
    return Decimal(text) if text == "-0" else int(text)


def _wire(value: object) -> object:
    if isinstance(value, str):
        return str(value)
    if isinstance(value, tuple):
        return [_wire(item) for item in value]
    if is_dataclass(value):
        return _wire_record(value)
    return value


def _wire_record(record: object) -> dict[str, object]:
    return {field.name: _wire(getattr(record, field.name)) for field in fields(record)}


class _WireObject:
    __slots__ = ()

    @classmethod
    def from_dict(cls, value: object) -> Self:
        raise NotImplementedError

    @classmethod
    def from_json(cls, text: str | bytes) -> Self:
        try:
            if isinstance(text, bytes):
                text = text.decode("utf-8")
            value = json.loads(
                text,
                object_pairs_hook=_unique_object,
                parse_float=Decimal,
                parse_int=_json_integer,
                parse_constant=_reject_constant,
            )
        except ProtocolError:
            raise
        except (ValueError, InvalidOperation, RecursionError) as exc:
            raise ProtocolError(f"JSON: {exc}") from exc
        return cls.from_dict(value)

    def to_dict(self) -> dict[str, object]:
        return _wire_record(self)

    def to_json(self) -> str:
        return json.dumps(self.to_dict(), ensure_ascii=False, separators=(",", ":"))


@dataclass(frozen=True, slots=True)
class SupplierSymbol:
    source: str
    product: str
    symbol: str

    @classmethod
    def _parse(cls, value: object, path: str) -> Self:
        data = _object(value, path, ("source", "product", "symbol"))
        return cls(
            source=_pattern(data["source"], _TOKEN, f"{path}.source"),
            product=_pattern(data["product"], _TOKEN, f"{path}.product"),
            symbol=_text(data["symbol"], f"{path}.symbol"),
        )


@dataclass(frozen=True, slots=True)
class Instrument(_WireObject):
    schema_version: str
    instrument_id: str
    asset_kind: AssetKind
    venue: str
    quote_currency: str
    tick_size: DecimalText
    lot_size: DecimalText
    contract_multiplier: DecimalText
    settlement_kind: SettlementKind
    settlement_currency: str | None
    calendar_id: str
    supplier_symbols: tuple[SupplierSymbol, ...]

    @classmethod
    def from_dict(cls, value: object) -> Self:
        path = "instrument"
        data = _object(value, path, (
            "schema_version", "instrument_id", "asset_kind", "venue", "quote_currency",
            "tick_size", "lot_size", "contract_multiplier", "settlement_kind",
            "settlement_currency", "calendar_id", "supplier_symbols",
        ))
        venue = _pattern(data["venue"], _VENUE_OR_CALENDAR, f"{path}.venue")
        settlement_kind = _choice(SettlementKind, data["settlement_kind"], f"{path}.settlement_kind")
        currency = data["settlement_currency"]
        _require(
            (currency is None) == (settlement_kind == SettlementKind.NONE),
            f"{path}.settlement_currency",
            "must be null exactly when settlement_kind is none",
        )
        symbols = tuple(
            SupplierSymbol._parse(entry, f"{path}.supplier_symbols[{index}]")
            for index, entry in enumerate(_array(data["supplier_symbols"], f"{path}.supplier_symbols"))
        )
        _require(
            len({(item.source, item.product) for item in symbols}) == len(symbols),
            f"{path}.supplier_symbols",
            "duplicate source/product mapping",
        )
        return cls(
            schema_version=_version(data, path),
            instrument_id=_instrument_id(data["instrument_id"], venue, f"{path}.instrument_id"),
            asset_kind=_choice(AssetKind, data["asset_kind"], f"{path}.asset_kind"),
            venue=venue,
            quote_currency=_pattern(data["quote_currency"], _CURRENCY, f"{path}.quote_currency"),
            tick_size=_quantity(data["tick_size"], f"{path}.tick_size", positive=True),
            lot_size=_quantity(data["lot_size"], f"{path}.lot_size", positive=True),
            contract_multiplier=_quantity(data["contract_multiplier"], f"{path}.contract_multiplier", positive=True),
            settlement_kind=settlement_kind,
            settlement_currency=None if currency is None else _pattern(currency, _CURRENCY, f"{path}.settlement_currency"),
            calendar_id=_pattern(data["calendar_id"], _VENUE_OR_CALENDAR, f"{path}.calendar_id"),
            supplier_symbols=symbols,
        )


@dataclass(frozen=True, slots=True)
class Capability(_WireObject):
    schema_version: str
    source: str
    product: str
    message_kinds: tuple[Kind, ...]
    max_depth: int
    can_backfill: bool
    delay_tier: DelayTier

    @classmethod
    def from_dict(cls, value: object) -> Self:
        path = "capability"
        data = _object(value, path, (
            "schema_version", "source", "product", "message_kinds",
            "max_depth", "can_backfill", "delay_tier",
        ))
        kinds_path = f"{path}.message_kinds"
        kinds = tuple(
            _choice(Kind, item, f"{kinds_path}[{index}]")
            for index, item in enumerate(_array(data["message_kinds"], kinds_path))
        )
        _require(bool(kinds), kinds_path, "expected at least one market message kind")
        _require(len(set(kinds)) == len(kinds), kinds_path, "duplicate message kind")
        _require(_MARKET_KINDS.issuperset(kinds), kinds_path, "only market message kinds may be advertised")
        max_depth = data["max_depth"]
        if type(max_depth) is not int or not 0 <= max_depth <= _MAX_U32:
            raise ProtocolError(f"{path}.max_depth: expected an unsigned 32-bit integer")
        _require(
            (max_depth > 0) == bool(_BOOK_KINDS.intersection(kinds)),
            f"{path}.max_depth",
            "must be positive exactly when snapshot or delta is supported",
        )
        can_backfill = data["can_backfill"]
        if type(can_backfill) is not bool:
            raise ProtocolError(f"{path}.can_backfill: expected a boolean")
        return cls(
            schema_version=_version(data, path),
            source=_pattern(data["source"], _TOKEN, f"{path}.source"),
            product=_pattern(data["product"], _TOKEN, f"{path}.product"),
            message_kinds=kinds,
            max_depth=max_depth,
            can_backfill=can_backfill,
            delay_tier=_choice(DelayTier, data["delay_tier"], f"{path}.delay_tier"),
        )


@dataclass(frozen=True, slots=True)
class BarClosed:
    open_ts: U64Text
    open: DecimalText
    high: DecimalText
    low: DecimalText
    close: DecimalText
    volume: DecimalText


@dataclass(frozen=True, slots=True)
class Trade:
    price: DecimalText
    quantity: DecimalText
    side: TradeSide


@dataclass(frozen=True, slots=True)
class BookLevel:
    price: DecimalText
    quantity: DecimalText


@dataclass(frozen=True, slots=True)
class Snapshot:
    bids: tuple[BookLevel, ...]
    asks: tuple[BookLevel, ...]


@dataclass(frozen=True, slots=True)
class BookUpdate:
    side: BookSide
    price: DecimalText
    quantity: DecimalText


@dataclass(frozen=True, slots=True)
class Delta:
    updates: tuple[BookUpdate, ...]


type MarketPayload = BarClosed | Trade | Snapshot | Delta


@dataclass(frozen=True, slots=True)
class Replacement:
    kind: Kind
    payload: MarketPayload


@dataclass(frozen=True, slots=True)
class Correction:
    target_epoch: U64Text
    target_seq: U64Text
    replacement: Replacement


@dataclass(frozen=True, slots=True)
class Gap:
    from_ts: U64Text
    to_ts: U64Text
    reason: str


@dataclass(frozen=True, slots=True)
class Reset:
    reason: str


@dataclass(frozen=True, slots=True)
class Heartbeat:
    pass


@dataclass(frozen=True, slots=True)
class Status:
    state: SourceState


type Payload = MarketPayload | Correction | Gap | Reset | Heartbeat | Status


def _quantity(value: object, path: str, *, positive: bool = False) -> DecimalText:
    result = DecimalText(value, field=path)
    if positive:
        _require(result.decimal > 0, path, "must be positive")
    else:
        _require(result.decimal >= 0, path, "must be nonnegative")
    return result


def _level(value: object, path: str) -> BookLevel:
    data = _object(value, path, ("price", "quantity"))
    return BookLevel(
        DecimalText(data["price"], field=f"{path}.price"),
        _quantity(data["quantity"], f"{path}.quantity", positive=True),
    )


def _levels(value: object, path: str, *, ascending: bool) -> tuple[BookLevel, ...]:
    levels = tuple(
        _level(item, f"{path}[{index}]")
        for index, item in enumerate(_array(value, path))
    )
    for index, (previous, current) in enumerate(zip(levels, levels[1:]), start=1):
        ordered = (
            previous.price.decimal < current.price.decimal if ascending
            else previous.price.decimal > current.price.decimal
        )
        _require(ordered, f"{path}[{index}].price", "prices must be strictly ordered")
    return levels


def _book_update(value: object, path: str) -> BookUpdate:
    data = _object(value, path, ("side", "price", "quantity"))
    return BookUpdate(
        _choice(BookSide, data["side"], f"{path}.side"),
        DecimalText(data["price"], field=f"{path}.price"),
        _quantity(data["quantity"], f"{path}.quantity"),
    )


def _market_payload(
    kind: Kind, value: object, path: str, exchange_ts: U64Text,
) -> MarketPayload:
    match kind:
        case Kind.BAR_CLOSED:
            data = _object(value, path, ("open_ts", "open", "high", "low", "close", "volume"))
            bar = BarClosed(
                open_ts=U64Text(data["open_ts"], field=f"{path}.open_ts"),
                open=DecimalText(data["open"], field=f"{path}.open"),
                high=DecimalText(data["high"], field=f"{path}.high"),
                low=DecimalText(data["low"], field=f"{path}.low"),
                close=DecimalText(data["close"], field=f"{path}.close"),
                volume=_quantity(data["volume"], f"{path}.volume"),
            )
            _require(bar.open_ts.integer < exchange_ts.integer, f"{path}.open_ts", "must precede exchange_ts")
            _require(
                bar.low.decimal <= min(bar.open.decimal, bar.close.decimal)
                and max(bar.open.decimal, bar.close.decimal) <= bar.high.decimal,
                path,
                "OHLC prices must be bounded by low and high",
            )
            return bar
        case Kind.TRADE:
            data = _object(value, path, ("price", "quantity", "side"))
            return Trade(
                price=DecimalText(data["price"], field=f"{path}.price"),
                quantity=_quantity(data["quantity"], f"{path}.quantity", positive=True),
                side=_choice(TradeSide, data["side"], f"{path}.side"),
            )
        case Kind.SNAPSHOT:
            data = _object(value, path, ("bids", "asks"))
            return Snapshot(
                bids=_levels(data["bids"], f"{path}.bids", ascending=False),
                asks=_levels(data["asks"], f"{path}.asks", ascending=True),
            )
        case Kind.DELTA:
            data = _object(value, path, ("updates",))
            updates = tuple(
                _book_update(item, f"{path}.updates[{index}]")
                for index, item in enumerate(_array(data["updates"], f"{path}.updates"))
            )
            _require(bool(updates), f"{path}.updates", "expected at least one update")
            return Delta(updates)
        case _:
            raise ProtocolError(f"{path}: expected a market payload")


def _payload(kind: Kind, value: object, exchange_ts: U64Text | None) -> Payload:
    path = "event.payload"
    if kind in _MARKET_KINDS or kind == Kind.CORRECTION:
        if exchange_ts is None:
            raise ProtocolError("event.exchange_ts: required for market data and corrections")
        if kind in _MARKET_KINDS:
            return _market_payload(kind, value, path, exchange_ts)
        data = _object(value, path, ("target_epoch", "target_seq", "replacement"))
        replacement_path = f"{path}.replacement"
        replacement = _object(data["replacement"], replacement_path, ("kind", "payload"))
        replacement_kind = _choice(Kind, replacement["kind"], f"{replacement_path}.kind")
        _require(replacement_kind in _MARKET_KINDS, f"{replacement_path}.kind", "expected a market message kind")
        return Correction(
            target_epoch=U64Text(data["target_epoch"], field=f"{path}.target_epoch"),
            target_seq=U64Text(data["target_seq"], field=f"{path}.target_seq"),
            replacement=Replacement(
                replacement_kind,
                _market_payload(replacement_kind, replacement["payload"], f"{replacement_path}.payload", exchange_ts),
            ),
        )

    match kind:
        case Kind.GAP:
            data = _object(value, path, ("from_ts", "to_ts", "reason"))
            start = U64Text(data["from_ts"], field=f"{path}.from_ts")
            end = U64Text(data["to_ts"], field=f"{path}.to_ts")
            _require(start.integer <= end.integer, f"{path}.to_ts", "must be at or after from_ts")
            return Gap(start, end, _text(data["reason"], f"{path}.reason"))
        case Kind.RESET:
            data = _object(value, path, ("reason",))
            return Reset(_text(data["reason"], f"{path}.reason"))
        case Kind.HEARTBEAT:
            _object(value, path, ())
            return Heartbeat()
        case Kind.STATUS:
            data = _object(value, path, ("state",))
            return Status(_choice(SourceState, data["state"], f"{path}.state"))
        case _:
            raise ProtocolError(f"{path}: unknown message kind")


def _aligned(value: DecimalText, step: DecimalText, path: str, step_name: str) -> None:
    _require(value.is_multiple_of(step), path, f"must be a multiple of {step_name} ({step})")


def _check_level(level: BookLevel | BookUpdate | Trade, instrument: Instrument, path: str) -> None:
    _aligned(level.price, instrument.tick_size, f"{path}.price", "tick_size")
    _aligned(level.quantity, instrument.lot_size, f"{path}.quantity", "lot_size")


@dataclass(frozen=True, slots=True)
class Event(_WireObject):
    schema_version: str
    channel: str
    source: str
    product: str
    instrument_id: str
    asset_kind: AssetKind
    venue: str
    exchange_ts: U64Text | None
    receive_ts: U64Text
    first_available_ts: U64Text
    epoch: U64Text
    seq: U64Text
    cursor: str
    revision: U64Text
    license: str
    kind: Kind
    payload: Payload
    native_id: str | None = None

    @classmethod
    def from_dict(cls, value: object) -> Self:
        path = "event"
        data = _object(value, path, (
            "schema_version", "channel", "source", "product", "instrument_id", "asset_kind",
            "venue", "exchange_ts", "receive_ts", "first_available_ts", "epoch", "seq",
            "cursor", "revision", "license", "kind", "payload",
        ), ("native_id",))
        venue = _pattern(data["venue"], _VENUE_OR_CALENDAR, f"{path}.venue")
        kind = _choice(Kind, data["kind"], f"{path}.kind")
        exchange_ts = (
            None if data["exchange_ts"] is None
            else U64Text(data["exchange_ts"], field=f"{path}.exchange_ts")
        )
        epoch = U64Text(data["epoch"], field=f"{path}.epoch")
        seq = U64Text(data["seq"], field=f"{path}.seq")
        revision = U64Text(data["revision"], field=f"{path}.revision")
        _require(epoch.integer > 0, f"{path}.epoch", "must be positive")
        _require(
            (revision.integer > 0) == (kind == Kind.CORRECTION),
            f"{path}.revision",
            "must be positive for correction and zero for other kinds",
        )
        _require(kind != Kind.RESET or seq.integer == 0, f"{path}.seq", "reset must have sequence zero")
        payload = _payload(kind, data["payload"], exchange_ts)
        if isinstance(payload, Correction):
            _require(payload.target_epoch.integer > 0, f"{path}.payload.target_epoch", "must be positive")
            _require(
                (payload.target_epoch.integer, payload.target_seq.integer) < (epoch.integer, seq.integer),
                f"{path}.payload.target_seq",
                "correction target must precede the new position",
            )
        return cls(
            schema_version=_version(data, path),
            channel=_pattern(data["channel"], _TOKEN, f"{path}.channel"),
            source=_pattern(data["source"], _TOKEN, f"{path}.source"),
            product=_pattern(data["product"], _TOKEN, f"{path}.product"),
            instrument_id=_instrument_id(data["instrument_id"], venue, f"{path}.instrument_id"),
            asset_kind=_choice(AssetKind, data["asset_kind"], f"{path}.asset_kind"),
            venue=venue,
            exchange_ts=exchange_ts,
            receive_ts=U64Text(data["receive_ts"], field=f"{path}.receive_ts"),
            first_available_ts=U64Text(data["first_available_ts"], field=f"{path}.first_available_ts"),
            epoch=epoch,
            seq=seq,
            cursor=_text(data["cursor"], f"{path}.cursor"),
            revision=revision,
            license=_pattern(data["license"], _TOKEN, f"{path}.license"),
            kind=kind,
            payload=payload,
            native_id=_text(data["native_id"], f"{path}.native_id") if "native_id" in data else None,
        )

    def to_dict(self) -> dict[str, object]:
        record = _wire_record(self)
        if self.native_id is None:
            del record["native_id"]
        return record

    @property
    def stream_key(self) -> tuple[str, str, str, str]:
        return self.source, self.product, self.channel, self.instrument_id

    def validate_successor(self, previous: Self | None) -> None:
        """Check protocol continuity from a reader's previous event, if any."""
        if previous is None:
            return
        _require(self.stream_key == previous.stream_key, "event.stream_key", "stream key changed")
        current_epoch = self.epoch.integer
        previous_epoch = previous.epoch.integer
        _require(current_epoch >= previous_epoch, "event.epoch", "epoch regressed")
        if current_epoch > previous_epoch:
            _require(self.kind == Kind.RESET, "event.epoch", "epoch change requires reset")
            return
        _require(self.kind != Kind.RESET, "event.epoch", "reset requires a newer epoch")
        _require(self.seq.integer > previous.seq.integer, "event.seq", "sequence regressed or repeated")
        _require(
            self.seq.integer == previous.seq.integer + 1,
            "event.seq",
            f"sequence gap, expected {previous.seq.integer + 1}",
        )

    def validate_against(self, instrument: Instrument, capability: Capability) -> None:
        """Apply reference data, source capabilities, and exact tick/lot rules."""
        for field in ("instrument_id", "venue", "asset_kind"):
            _require(
                getattr(self, field) == getattr(instrument, field),
                f"event.{field}",
                "does not match instrument metadata",
            )
        for field in ("source", "product"):
            _require(
                getattr(self, field) == getattr(capability, field),
                f"event.{field}",
                "does not match source capability",
            )

        path = "event.payload"
        kind_path = "event.kind"
        kind, payload = self.kind, self.payload
        if isinstance(payload, Correction):
            path += ".replacement.payload"
            kind_path = "event.payload.replacement.kind"
            kind, payload = payload.replacement.kind, payload.replacement.payload
        if kind not in _MARKET_KINDS:
            return
        _require(kind in capability.message_kinds, kind_path, f"{kind} is not supported by the source product")

        if isinstance(payload, BarClosed):
            for field in ("open", "high", "low", "close"):
                _aligned(getattr(payload, field), instrument.tick_size, f"{path}.{field}", "tick_size")
            _aligned(payload.volume, instrument.lot_size, f"{path}.volume", "lot_size")
        elif isinstance(payload, Trade):
            _check_level(payload, instrument, path)
        elif isinstance(payload, Snapshot):
            for name, levels in (("bids", payload.bids), ("asks", payload.asks)):
                _require(len(levels) <= capability.max_depth, f"{path}.{name}", "exceeds max_depth")
                for index, level in enumerate(levels):
                    _check_level(level, instrument, f"{path}.{name}[{index}]")
        elif isinstance(payload, Delta):
            for index, update in enumerate(payload.updates):
                _check_level(update, instrument, f"{path}.updates[{index}]")
