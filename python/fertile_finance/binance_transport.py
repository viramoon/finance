"""Public Binance WS/REST transport for the finance-binance producer.

Rust owns the order books, sequence validation, product policy, and retries.
This process only frames live public messages for its parent over stdout.
"""

from __future__ import annotations

import argparse
import asyncio
from collections.abc import Callable, Iterator
from contextlib import suppress
from dataclasses import dataclass
from datetime import UTC, datetime
from email.utils import parsedate_to_datetime
import json
import re
import sys
import time
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from nautilus_trader.core.nautilus_pyo3 import HttpClient

SYMBOLS = ("BTCUSDT", "ETHUSDT", "SOLUSDT")
DEPTH_LIMIT = 1000
OI_INTERVAL = 60
MAX_BODY = 3 * 1024 * 1024
MAX_WS_FRAME = 256 * 1024


@dataclass(frozen=True)
class Market:
    name: str
    http: str
    depth_stream: str
    trade_stream: str
    trade_topic: str


SPOT = Market(
    "spot",
    "https://api.binance.com/api/v3",
    "wss://stream.binance.com:9443/stream",
    "wss://stream.binance.com:9443/stream",
    "trade",
)
PERPETUAL = Market(
    "perpetual",
    "https://fapi.binance.com/fapi/v1",
    "wss://fstream.binance.com/public/stream",
    "wss://fstream.binance.com/market/stream",
    "aggTrade",
)


class FetchFailure(Exception):
    def __init__(self, status: int | None = None, retry_after: int | None = None):
        super().__init__("Binance public feed unavailable")
        self.status = status
        self.retry_after = retry_after


def retry_delay(value: str) -> int | None:
    if re.fullmatch(r"[0-9]{1,9}", value):
        return int(value)
    try:
        until = parsedate_to_datetime(value)
        if until.tzinfo is None:
            return None
        return max(0, int((until - datetime.now(UTC)).total_seconds()) + 1)
    except (ValueError, OverflowError):
        return None


async def rest(client: HttpClient, url: str, params: dict[str, str | int]) -> dict:
    response = await client.get(url, params=params)
    if response.status != 200:
        delay = retry_delay(response.headers.get("retry-after", ""))
        raise FetchFailure(response.status, delay)
    body = response.body
    if len(body) > MAX_BODY:
        raise FetchFailure()
    data = json.loads(body)
    if not isinstance(data, dict):
        raise FetchFailure()
    return data


def emit(frame: dict) -> None:
    # JSON integers survive this pipe exactly, including u64 exchange IDs.
    sys.stdout.write(json.dumps(frame, separators=(",", ":"), allow_nan=False) + "\n")
    sys.stdout.flush()


async def watch_stream(
    client: HttpClient, market: Market, url: str, symbols: tuple[str, ...], emit_frame: Callable[[dict], None],
) -> None:
    from nautilus_trader.core.nautilus_pyo3 import (
        TransportBackend,
        WebSocketClient,
        WebSocketClientError,
        WebSocketConfig,
    )

    queue = asyncio.Queue(maxsize=256)
    overflow = False

    def enqueue(raw: bytes) -> None:
        nonlocal overflow
        if len(raw) > MAX_WS_FRAME or queue.full():
            overflow = True
        elif not overflow:
            queue.put_nowait((time.time_ns(), raw))

    config = WebSocketConfig(
        url=url,
        headers=[],
        reconnect_max_attempts=0,
        idle_timeout_ms=20_000,
        backend=TransportBackend.TUNGSTENITE,
    )
    try:
        async with asyncio.timeout(10):
            socket = await WebSocketClient.connect(asyncio.get_running_loop(), config, enqueue)
    except WebSocketClientError as error:
        # Nautilus v1.231 exposes upgrade failures as a string from TransportError.
        match = re.match(r"^handshake failed: HTTP status ([0-9]{3})(?: |$)", str(error))
        raise FetchFailure(int(match[1]) if match else None) from None

    async def fetch_snapshot(symbol: str) -> None:
        data = await rest(client, f"{market.http}/depth", {"symbol": symbol, "limit": DEPTH_LIMIT})
        emit_frame({
            "type": "snapshot", "market": market.name, "symbol": symbol,
            "received": time.time_ns(), "data": data,
        })

    requested = set()
    try:
        async with asyncio.TaskGroup() as snapshots:
            while socket.is_active() and not overflow:
                try:
                    received, raw = await asyncio.wait_for(queue.get(), timeout=0.25)
                except TimeoutError:
                    continue
                if not socket.is_active() or overflow:
                    break
                data = json.loads(raw)["data"]
                symbol = data["s"]
                if symbol not in symbols:
                    raise FetchFailure()
                emit_frame({"type": "stream", "market": market.name, "received": received, "data": data})
                if data["e"] == "depthUpdate" and symbol not in requested:
                    requested.add(symbol)
                    snapshots.create_task(fetch_snapshot(symbol))
            raise FetchFailure()
    finally:
        with suppress(Exception):
            async with asyncio.timeout(2):
                await socket.disconnect()


async def poll_interest(client: HttpClient, market: Market, symbols: tuple[str, ...], emit_frame: Callable[[dict], None]) -> None:
    while True:
        for symbol in symbols:
            data = await rest(client, f"{market.http}/openInterest", {"symbol": symbol})
            if data.get("symbol") != symbol:
                raise FetchFailure()
            emit_frame({"type": "interest", "received": time.time_ns(), "data": data})
        await asyncio.sleep(OI_INTERVAL)


async def collect_market(client: HttpClient, market: Market, symbols: tuple[str, ...], emit_frame: Callable[[dict], None]) -> None:
    params = {"symbols": json.dumps(symbols, separators=(",", ":"))} if market == SPOT else {}
    data = await rest(client, f"{market.http}/exchangeInfo", params)
    selected = [row for row in data["symbols"] if row["symbol"] in symbols]
    emit_frame({"type": "metadata", "market": market.name, "data": {"symbols": selected}})

    lower = [symbol.lower() for symbol in symbols]
    depth = [f"{symbol}@depth@100ms" for symbol in lower]
    trades = [f"{symbol}@{market.trade_topic}" for symbol in lower]

    def url(base: str, topics: list[str]) -> str:
        return f"{base}?streams={'/'.join(topics)}"

    if market == SPOT:
        await watch_stream(client, market, url(market.depth_stream, depth + trades), symbols, emit_frame)
    else:
        funding = [f"{symbol}@markPrice@1s" for symbol in lower]
        async with asyncio.TaskGroup() as group:
            group.create_task(watch_stream(client, market, url(market.depth_stream, depth), symbols, emit_frame))
            group.create_task(watch_stream(client, market, url(market.trade_stream, trades + funding), symbols, emit_frame))
            group.create_task(poll_interest(client, market, symbols, emit_frame))


def failures(error: BaseException) -> Iterator[FetchFailure]:
    if isinstance(error, BaseExceptionGroup):
        for child in error.exceptions:
            yield from failures(child)
    elif isinstance(error, FetchFailure):
        yield error


async def run(spot: bool, perpetual: tuple[str, ...], emit_frame: Callable[[dict], None]) -> None:
    from nautilus_trader.core.nautilus_pyo3 import HttpClient

    try:
        client = HttpClient(header_keys=["retry-after"], timeout_secs=8)
        async with asyncio.TaskGroup() as group:
            if spot:
                group.create_task(collect_market(client, SPOT, SYMBOLS, emit_frame))
            if perpetual:
                group.create_task(collect_market(client, PERPETUAL, perpetual, emit_frame))
    except Exception as error:
        # If multiple tasks fail in the same tick, a regional refusal wins.
        failure = max(
            failures(error),
            key=lambda item: (item.status in (403, 451), item.status in (418, 429), item.status is not None),
            default=FetchFailure(),
        )
        emit_frame({"type": "failure", "status": failure.status, "retry_after": failure.retry_after})


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--spot", action="store_true")
    parser.add_argument("--perpetual", choices=SYMBOLS, nargs="+", default=[])
    args = parser.parse_args()
    asyncio.run(run(args.spot, tuple(args.perpetual), emit))


if __name__ == "__main__":
    main()
