"""Public HTTPS metadata and WebSocket transport for finance-hyperliquid."""

import argparse
import asyncio
from decimal import Decimal, DecimalException
from http.client import HTTPException, HTTPSConnection
import json
import sys
import time

_HOST = "api.hyperliquid.xyz"
_COINS = ("BTC", "ETH", "SOL")
_CHANNELS = ("l2Book", "trades", "activeAssetCtx")
_MAX_MESSAGE = 1024 * 1024
_TIMEOUT = 10


class SessionLost(Exception):
    """A public data session must be replaced and its subscriptions rebuilt."""


def _request_metadata(connection_factory=HTTPSConnection):
    conn = connection_factory(_HOST, timeout=_TIMEOUT)
    try:
        conn.request("POST", "/info", b'{"type":"meta"}', {"Content-Type": "application/json"})
        with conn.getresponse() as response:
            if response.status != 200:
                raise SessionLost(f"metadata HTTP {response.status}")
            body = response.read(_MAX_MESSAGE + 1)
        if len(body) > _MAX_MESSAGE:
            raise SessionLost("metadata response exceeded limit")
        return body.decode("utf-8")
    except (HTTPException, OSError, UnicodeError) as exc:
        raise SessionLost(type(exc).__name__) from None
    finally:
        conn.close()


class _NautilusSession:
    def __init__(self, transport_error):
        self._messages = asyncio.Queue(maxsize=512)
        self._overflow = False
        self._client = None
        self._transport_error = transport_error

    def _on_message(self, data):
        if self._messages.full() or len(data) > _MAX_MESSAGE:
            self._overflow = True
        elif not self._overflow:
            self._messages.put_nowait((time.time_ns(), data))

    async def send(self, payload):
        try:
            await asyncio.wait_for(self._client.send_text(payload), timeout=_TIMEOUT)
        except (self._transport_error, RuntimeError, OSError, TimeoutError):
            raise SessionLost("subscription write failed") from None

    async def receive(self):
        while True:
            if self._overflow:
                raise SessionLost("WebSocket receive queue exceeded limit")
            if self._client.is_closed() or self._client.is_reconnecting():
                raise SessionLost("WebSocket closed")
            try:
                return await asyncio.wait_for(self._messages.get(), timeout=0.25)
            except TimeoutError:
                continue

    async def close(self):
        await self._client.disconnect()


async def _connect():
    from nautilus_trader.core.nautilus_pyo3 import WebSocketClient, WebSocketClientError, WebSocketConfig

    session = _NautilusSession(WebSocketClientError)
    config = WebSocketConfig(
        f"wss://{_HOST}/ws",
        headers=[],
        heartbeat=20,
        heartbeat_msg='{"method":"ping"}',
        idle_timeout_ms=45_000,
        # Each new socket belongs to a new collector session.
        reconnect_max_attempts=0,
    )
    try:
        session._client = await asyncio.wait_for(
            WebSocketClient.connect(asyncio.get_running_loop(), config, session._on_message),
            timeout=_TIMEOUT,
        )
    except (WebSocketClientError, RuntimeError, OSError, TimeoutError):
        raise SessionLost("WebSocket handshake failed") from None
    return session


def _emit(event, timestamp, **fields):
    print(json.dumps({"event": event, "receive_ts": str(timestamp), **fields}), flush=True)


async def _stream_session(session, coins, emit):
    pending = {(coin, channel) for coin in coins for channel in _CHANNELS}
    for coin in coins:
        for channel in _CHANNELS:
            request = {"method": "subscribe", "subscription": {"type": channel, "coin": coin}}
            await session.send(json.dumps(request).encode("utf-8"))
    deadline = asyncio.get_running_loop().time() + _TIMEOUT
    while True:
        try:
            if pending:
                remaining = deadline - asyncio.get_running_loop().time()
                timestamp, payload = await asyncio.wait_for(session.receive(), timeout=remaining)
            else:
                timestamp, payload = await session.receive()
        except TimeoutError:
            raise SessionLost("WebSocket subscription acknowledgement timed out") from None
        try:
            frame = payload.decode("utf-8")
            envelope = json.loads(frame, parse_float=Decimal)
        except (ValueError, DecimalException, RecursionError):
            raise SessionLost("invalid WebSocket JSON") from None
        if not isinstance(envelope, dict):
            raise SessionLost("invalid WebSocket envelope")
        channel = envelope.get("channel")
        if channel == "error":
            raise SessionLost("public subscription rejected")
        if channel == "subscriptionResponse":
            data = envelope.get("data")
            if isinstance(data, dict) and data.get("method") == "subscribe":
                subscription = data.get("subscription")
                if isinstance(subscription, dict):
                    coin, kind = subscription.get("coin"), subscription.get("type")
                    if isinstance(coin, str) and isinstance(kind, str):
                        pending.discard((coin, kind))
        elif channel in _CHANNELS:
            emit("frame", timestamp, data=frame)


async def run(coins, *, connect=_connect, load_metadata=_request_metadata, emit=_emit, sleep=asyncio.sleep):
    delay = 1
    while True:
        try:
            # Metadata is loaded before opening the WebSocket or starting any receive tasks.
            metadata = load_metadata()
        except SessionLost as exc:
            print(f"Hyperliquid public metadata retry: {exc}", file=sys.stderr)
            await sleep(delay)
            delay = min(30, delay * 2)
        else:
            emit("metadata", time.time_ns(), data=metadata)
            break
    delay = 1
    while True:
        session = None
        loop = asyncio.get_running_loop()
        started = loop.time()
        try:
            session = await connect()
            emit("connected", time.time_ns())
            await _stream_session(session, coins, emit)
        except SessionLost as exc:
            if session is not None:
                emit("disconnected", time.time_ns())
            print(f"Hyperliquid public WebSocket retry: {exc}", file=sys.stderr)
        finally:
            if session is not None:
                await session.close()
        if loop.time() - started >= 30:
            delay = 1
        await sleep(delay)
        delay = min(30, delay * 2)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("coins", nargs="+", choices=_COINS)
    args = parser.parse_args()
    asyncio.run(run(tuple(dict.fromkeys(args.coins))))


if __name__ == "__main__":
    main()
