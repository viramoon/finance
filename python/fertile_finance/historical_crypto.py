"""Read public closed crypto candles and import sealed Binance Spot trade days."""

import csv
from collections.abc import Callable
from contextlib import contextmanager
from dataclasses import dataclass
from datetime import date
from decimal import Decimal
import fcntl
import hashlib
from http.client import HTTPException, HTTPSConnection
import io
import json
import os
from pathlib import Path
import re
import shutil
from urllib.parse import urlencode, urlsplit
from zipfile import BadZipFile, ZipFile

import pyarrow as pa
import pyarrow.parquet as pq

from .collector_store import ensure_dir, fsync_dir
from .epoch import EPOCH, unix_nanos
from .history_paths import history_path, validate_history_root
from .market_protocol import DecimalText, U64Text


CRYPTO_SYMBOLS = ("BTC", "ETH", "SOL")
CRYPTO_INTERVAL_MS = {"1m": 60_000, "1d": 86_400_000}
_CANDLE_LIMIT = {"hyperliquid": 5000, "binance": 1000}
_HYPERLIQUID_INFO = "https://api.hyperliquid.xyz/info"
_BINANCE_KLINES = "https://data-api.binance.vision/api/v3/klines"
_BINANCE_ARCHIVE = "https://data.binance.vision/data/spot/daily/trades"
_BINANCE_MICROSECONDS_FROM = date(2025, 1, 1)
_NS_PER_MS = 1_000_000
_NS_PER_DAY = CRYPTO_INTERVAL_MS["1d"] * _NS_PER_MS
_IO_BLOCK_BYTES = 1024 * 1024
_MAX_CANDLE_BYTES = 8 * _IO_BLOCK_BYTES
_TRADE_BATCH_ROWS = 64 * 1024
_TRADE_HEADER = ("id", "price", "qty", "quoteqty", "time", "isbuyermaker", "isbestmatch")
_TRADE_SCHEMA = pa.schema([
    pa.field("trade_id", pa.string(), nullable=False),
    pa.field("ts_ns", pa.int64(), nullable=False),
    pa.field("price", pa.string(), nullable=False),
    pa.field("quantity", pa.string(), nullable=False),
    pa.field("quote_quantity", pa.string(), nullable=False),
    pa.field("is_buyer_maker", pa.bool_(), nullable=False),
    pa.field("is_best_match", pa.bool_(), nullable=False),
])


class BinanceDisabled(ValueError):
    status = "disabled_by_profile"

    def __init__(self):
        super().__init__("Binance disabled_by_profile")


class CryptoHTTPError(ValueError):
    def __init__(self, venue: str, status_code: int):
        self.status_code = status_code
        self.status = "region_restricted" if venue == "binance" and status_code == 451 else "http_error"
        super().__init__(f"{venue} HTTP {status_code} ({self.status})")


def _require_binance(enabled: bool) -> None:
    if enabled is not True:
        raise BinanceDisabled()


@contextmanager
def _public_http(method: str, url: str, body: bytes | None = None):
    endpoint = urlsplit(url)
    connection = HTTPSConnection(endpoint.hostname, timeout=30)
    target = endpoint.path
    if endpoint.query:
        target += "?" + endpoint.query
    headers = {"User-Agent": "fertile-finance/0.0.0 (public historical data)"}
    if body is not None:
        headers["Content-Type"] = "application/json"
    try:
        connection.request(method, target, body=body, headers=headers)
        yield connection.getresponse()
    except HTTPException as exc:
        raise ValueError(f"historical HTTP transport failure ({type(exc).__name__})") from None
    finally:
        connection.close()


def _require_ok(venue: str, response) -> None:
    if response.status != 200:
        raise CryptoHTTPError(venue, response.status)


def _reject_constant(_value: str) -> None:
    raise ValueError("invalid non-finite JSON number in candle response")


def _require_symbol(symbol: str) -> None:
    if symbol not in CRYPTO_SYMBOLS:
        raise ValueError("expected crypto symbol BTC, ETH or SOL")


def _binance_pair(symbol: str) -> str:
    return f"{symbol}USDT"


def _check_ohlcv(values) -> tuple[str, ...]:
    fields = ("open", "high", "low", "close", "volume")
    texts = tuple(DecimalText(value, field=field) for field, value in zip(fields, values, strict=True))
    open_, high, low, close, volume = (text.decimal for text in texts)
    if not (0 < low <= min(open_, close) <= max(open_, close) <= high and volume >= 0):
        raise ValueError("invalid crypto candle OHLCV")
    return tuple(map(str, texts))


class CryptoBars:
    """Public candle backfill. Query bounds are aligned candle opens in UTC milliseconds."""

    def __init__(self, venue: str, *, binance_enabled: bool = False, http: Callable = _public_http):
        if venue not in _CANDLE_LIMIT:
            raise ValueError("expected hyperliquid or binance")
        self.venue = venue
        self.page_limit = _CANDLE_LIMIT[venue]
        self._binance_enabled = binance_enabled
        self._http = http

    def instrument(self, symbol: str) -> str:
        _require_symbol(symbol)
        if self.venue == "hyperliquid":
            return f"{symbol}-USD-PERP.HYPERLIQUID"
        return f"{_binance_pair(symbol)}.BINANCE"

    def _request(self, symbol: str, interval: str, start: int, end: int, step: int) -> list:
        if self.venue == "hyperliquid":
            body = json.dumps({
                "type": "candleSnapshot",
                "req": {"coin": symbol, "interval": interval, "startTime": start, "endTime": end - 1},
            }).encode("ascii")
            request = self._http("POST", _HYPERLIQUID_INFO, body)
        else:
            query = urlencode({
                "symbol": _binance_pair(symbol), "interval": interval,
                "startTime": start, "endTime": end - 1, "limit": (end - start) // step,
            })
            request = self._http("GET", f"{_BINANCE_KLINES}?{query}")
        with request as response:
            _require_ok(self.venue, response)
            payload = response.read(_MAX_CANDLE_BYTES + 1)
        if len(payload) > _MAX_CANDLE_BYTES:
            raise ValueError(f"{self.venue} candle response exceeds 8 MiB")
        rows = json.loads(payload, parse_float=Decimal, parse_constant=_reject_constant)
        if not isinstance(rows, list) or len(rows) > self.page_limit:
            raise ValueError(f"{self.venue} candle response is not a bounded array")
        return rows

    def _unpack(self, row, symbol: str, interval: str) -> tuple:
        if self.venue == "hyperliquid":
            if isinstance(row, dict) and row.get("s") == symbol and row.get("i") == interval:
                try:
                    return row["t"], row["T"], tuple(row[key] for key in ("o", "h", "l", "c", "v"))
                except KeyError:
                    pass
        elif isinstance(row, list) and len(row) == 12:
            return row[0], row[6], row[1:6]
        raise ValueError(f"malformed {self.venue} candle")

    def fetch(
        self, symbol: str, interval: str, start_ms: int, end_ms: int, *, now_ms: int,
    ) -> list[dict]:
        """Return sorted collector rows for closed candles in the half-open open range."""
        if self.venue == "binance":
            _require_binance(self._binance_enabled)
        instrument = self.instrument(symbol)
        if interval not in CRYPTO_INTERVAL_MS:
            raise ValueError("expected crypto interval 1m or 1d")
        step = CRYPTO_INTERVAL_MS[interval]
        for value in (start_ms, end_ms, now_ms):
            unix_nanos(value * _NS_PER_MS if type(value) is int else None)
        if start_ms >= end_ms or start_ms % step or end_ms % step:
            raise ValueError("crypto candle bounds must be increasing aligned UTC open times")
        end_ms = min(end_ms, now_ms - now_ms % step)
        by_open = {}
        for first in range(start_ms, end_ms, step * self.page_limit):
            last = min(end_ms, first + step * self.page_limit)
            for row in self._request(symbol, interval, first, last, step):
                opened, vendor_close, values = self._unpack(row, symbol, interval)
                if type(opened) is not int or opened < 0 or opened % step:
                    raise ValueError(f"invalid {self.venue} candle open time")
                if not first <= opened < last:
                    continue
                closed = opened + step
                valid_closes = (closed - 1, closed) if self.venue == "hyperliquid" else (closed - 1,)
                if type(vendor_close) is not int or vendor_close not in valid_closes:
                    raise ValueError(f"invalid {self.venue} candle close time")
                open_, high, low, close, volume = _check_ohlcv(values)
                bar = {
                    "instrument": instrument, "ts_ns": unix_nanos(closed * _NS_PER_MS),
                    "open": open_, "high": high, "low": low, "close": close, "volume": volume,
                }
                if by_open.setdefault(opened, bar) != bar:
                    raise ValueError(f"conflicting {self.venue} candles at the same open time")
        return [by_open[opened] for opened in sorted(by_open)]


@dataclass(frozen=True)
class TradeImportReport:
    imported_days: tuple[str, ...]
    existing_days: tuple[str, ...]
    missing_days: tuple[str, ...]
    trades_written: int


def _get_archive_checksum(url: str, filename: str, http: Callable) -> str:
    with http("GET", url + ".CHECKSUM") as response:
        _require_ok("binance", response)
        raw = response.read(513)
    parts = raw.split()
    encoded = filename.encode("ascii")
    if (
        len(raw) > 512 or len(parts) != 2 or not re.fullmatch(b"[a-fA-F0-9]{64}", parts[0])
        or parts[1] not in (encoded, b"*" + encoded)
    ):
        raise ValueError("invalid Binance archive checksum file")
    return parts[0].decode("ascii").lower()


def _download_archive(url: str, target: Path, checksum: str, http: Callable) -> None:
    digest = hashlib.sha256()
    with http("GET", url) as response:
        _require_ok("binance", response)
        with target.open("wb") as output:
            while chunk := response.read(_IO_BLOCK_BYTES):
                digest.update(chunk)
                output.write(chunk)
    if digest.hexdigest() != checksum:
        raise ValueError("Binance archive SHA256 mismatch")


def _archive_bool(value: str) -> bool:
    normalized = value.lower()
    if normalized not in ("false", "true"):
        raise ValueError("invalid boolean")
    return normalized == "true"


def _trade_row(fields: list[str], start_ns: int, ns_per_unit: int) -> dict:
    trade_id, price, size, quote, timestamp, buyer_maker, best_match = fields
    ts_ns = unix_nanos(U64Text(timestamp).integer * ns_per_unit)
    if not start_ns <= ts_ns < start_ns + _NS_PER_DAY:
        raise ValueError("trade outside archive day")
    price = DecimalText(price)
    size = DecimalText(size)
    quote = DecimalText(quote)
    if price.decimal <= 0 or size.decimal <= 0 or quote.decimal < 0:
        raise ValueError("invalid trade price or quantity")
    return {
        "trade_id": U64Text(trade_id),
        "ts_ns": ts_ns,
        "price": price,
        "quantity": size,
        "quote_quantity": quote,
        "is_buyer_maker": _archive_bool(buyer_maker),
        "is_best_match": _archive_bool(best_match),
    }


def _trade_batches(source, day: date):
    start_ns = (day - EPOCH).days * _NS_PER_DAY
    ns_per_unit = 1000 if day >= _BINANCE_MICROSECONDS_FROM else _NS_PER_MS
    previous_id = -1
    batch = []
    reader = csv.reader(source, strict=True)
    for line, fields in enumerate(reader, start=1):
        if line == 1 and tuple(field.strip().lower().replace("_", "") for field in fields) == _TRADE_HEADER:
            continue
        try:
            row = _trade_row(fields, start_ns, ns_per_unit)
            trade_id = row["trade_id"].integer
            if trade_id <= previous_id:
                raise ValueError("trade IDs must strictly increase")
        except ValueError as exc:
            raise ValueError(f"invalid Binance archive trade on CSV line {line}: {exc}") from None
        previous_id = trade_id
        batch.append(row)
        if len(batch) == _TRADE_BATCH_ROWS:
            yield batch
            batch = []
    if batch:
        yield batch


def _write_trade_day(
    archive: Path, staged_parquet: Path, day: date, schema: pa.Schema, csv_name: str,
) -> int:
    count = 0
    try:
        with ZipFile(archive) as zipped:
            if zipped.namelist() != [csv_name]:
                raise ValueError("Binance ZIP must contain the expected trade CSV")
            with (
                zipped.open(csv_name) as binary,
                io.TextIOWrapper(binary, encoding="utf-8", newline="") as source,
                pq.ParquetWriter(staged_parquet, schema, compression="zstd") as writer,
            ):
                for batch in _trade_batches(source, day):
                    writer.write_table(pa.Table.from_pylist(batch, schema=schema))
                    count += len(batch)
    except (BadZipFile, csv.Error, UnicodeError) as exc:
        raise ValueError(f"unreadable Binance trade archive ({type(exc).__name__})") from None
    if not count:
        raise ValueError("empty Binance trade archive")
    with staged_parquet.open("rb") as committed:
        os.fsync(committed.fileno())
    return count


def _check_existing_trade_day(path: Path, metadata: dict[bytes, bytes]) -> None:
    with pq.ParquetFile(path) as existing:
        schema = existing.schema_arrow
        fields = schema.metadata or {}
        if (
            schema.remove_metadata() != _TRADE_SCHEMA or existing.metadata.num_rows == 0
            or any(fields.get(key) != value for key, value in metadata.items())
            or re.fullmatch(b"[a-f0-9]{64}", fields.get(b"archive_sha256", b"")) is None
        ):
            raise ValueError("existing Binance trade partition has incompatible metadata")


def import_binance_trades(
    data_root: Path, symbol: str, start_date: date, end_date: date, *,
    binance_enabled: bool = False, http: Callable = _public_http,
) -> TradeImportReport:
    """Import inclusive UTC archive dates. Each published partition is a resume marker."""
    _require_binance(binance_enabled)
    _require_symbol(symbol)
    if type(start_date) is not date or type(end_date) is not date or not EPOCH <= start_date <= end_date:
        raise ValueError("expected an increasing inclusive range of UTC archive dates")
    unix_nanos((end_date - EPOCH).days * _NS_PER_DAY + _NS_PER_DAY - 1)

    root = validate_history_root(data_root)
    pair = _binance_pair(symbol)
    instrument = pair + ".BINANCE"
    components = ("catalog", "historical", "binance-spot-trades", instrument)
    directory = history_path(root, *components)
    ensure_dir(directory)
    imported, existing, missing = [], [], []
    count = 0
    lock_path = history_path(root, *components, ".writer.lock")
    with lock_path.open("ab") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        stage = history_path(root, *components, ".staging")
        if stage.exists():
            shutil.rmtree(stage)
        stage.mkdir(mode=0o700)
        try:
            for ordinal in range(start_date.toordinal(), end_date.toordinal() + 1):
                day = date.fromordinal(ordinal)
                day_text = day.isoformat()
                stem = f"{pair}-trades-{day_text}"
                filename = stem + ".zip"
                url = f"{_BINANCE_ARCHIVE}/{pair}/{filename}"
                output = history_path(root, *components, day_text + ".parquet")
                metadata = {
                    b"finance_schema": b"binance-spot-trades-v1",
                    b"instrument_id": instrument.encode("ascii"),
                    b"utc_day": day_text.encode("ascii"),
                    b"archive_url": url.encode("ascii"),
                }
                if output.exists():
                    _check_existing_trade_day(output, metadata)
                    existing.append(day_text)
                    continue
                archive = stage / filename
                staged_parquet = stage / "trades.parquet"
                try:
                    checksum = _get_archive_checksum(url, filename, http)
                    _download_archive(url, archive, checksum, http)
                except CryptoHTTPError as exc:
                    if exc.status_code != 404:
                        raise
                    missing.append(day_text)
                    continue
                try:
                    schema = _TRADE_SCHEMA.with_metadata({**metadata, b"archive_sha256": checksum.encode("ascii")})
                    count += _write_trade_day(archive, staged_parquet, day, schema, stem + ".csv")
                finally:
                    archive.unlink(missing_ok=True)
                os.replace(staged_parquet, output)
                fsync_dir(directory)
                imported.append(day_text)
        finally:
            shutil.rmtree(stage)
    return TradeImportReport(tuple(imported), tuple(existing), tuple(missing), count)
