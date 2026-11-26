"""Offline transport checks with hand-authored public HTTP and WS frames."""

import asyncio
from contextlib import redirect_stderr
from http.client import HTTPSConnection, HTTPResponse
import io
import json
from pathlib import Path
import unittest
from unittest.mock import AsyncMock, MagicMock, Mock, patch

from fertile_finance import hyperliquid_transport as transport


_FIXTURES = Path(__file__).parent / "fixtures" / "hyperliquid"
_COINS = ("BTC", "ETH", "SOL")
_EXPECTED = {(coin, channel) for coin in _COINS for channel in ("l2Book", "trades", "activeAssetCtx")}


class ReplaySession:
    def __init__(self, path):
        self.frames = path.read_bytes().splitlines()
        self.sent = []
        self.pending = []
        self.closed = False

    async def send(self, payload):
        request = json.loads(payload)
        self.sent.append(request)
        self.pending.append(request)

    async def receive(self):
        if self.pending:
            subscription = self.pending.pop(0)
            assert subscription["method"] == "subscribe"
            data = json.dumps({"channel": "subscriptionResponse", "data": subscription})
            return 100, data.encode()
        if self.frames:
            return 101, self.frames.pop(0)
        raise transport.SessionLost("synthetic EOF")

    async def close(self):
        self.closed = True


class HyperliquidTransportTests(unittest.IsolatedAsyncioTestCase):
    def test_rest_meta_request_has_only_public_payload_and_headers(self):
        conn = Mock(spec=HTTPSConnection)
        factory = Mock(return_value=conn)
        response = conn.getresponse.return_value = MagicMock(spec=HTTPResponse)
        response.__enter__.return_value = response
        response.status = 200
        response.read.return_value = (_FIXTURES / "meta.json").read_bytes()
        self.assertEqual(transport._request_metadata(factory), response.read.return_value.decode())
        factory.assert_called_once_with("api.hyperliquid.xyz", timeout=10)
        conn.request.assert_called_once_with(
            "POST", "/info", b'{"type":"meta"}', {"Content-Type": "application/json"},
        )
        conn.close.assert_called_once()

    async def test_reconnect_resubscribes_all_three_public_channels_for_each_coin(self):
        sessions = [ReplaySession(_FIXTURES / f"ws-{suffix}.jsonl") for suffix in ("initial", "reconnected")]
        connect = AsyncMock(side_effect=sessions)
        load_meta = Mock(return_value=(_FIXTURES / "meta.json").read_text())
        messages = []

        def emit(kind, timestamp, **fields):
            self.assertIsInstance(timestamp, int)
            messages.append((kind, fields))

        sleep = AsyncMock(side_effect=[None, asyncio.CancelledError])
        with redirect_stderr(io.StringIO()), self.assertRaises(asyncio.CancelledError):
            await transport.run(_COINS, connect=connect, load_metadata=load_meta, emit=emit, sleep=sleep)

        self.assertEqual(connect.await_count, 2)
        load_meta.assert_called_once()
        self.assertEqual([call.args for call in sleep.await_args_list], [(1,), (2,)])
        for session in sessions:
            sent = session.sent
            self.assertEqual(len(sent), 9)
            self.assertEqual({(item["subscription"]["coin"], item["subscription"]["type"]) for item in sent}, _EXPECTED)
            for item in sent:
                self.assertEqual(set(item), {"method", "subscription"})
                self.assertEqual(set(item["subscription"]), {"type", "coin"})
                self.assertEqual(item["method"], "subscribe")
        self.assertTrue(all(session.closed for session in sessions))
        self.assertEqual([kind for kind, _ in messages if kind != "frame"], [
            "metadata", "connected", "disconnected", "connected", "disconnected",
        ])
        self.assertEqual(
            [fields["data"] for kind, fields in messages if kind == "frame"],
            [line for suffix in ("initial", "reconnected")
             for line in (_FIXTURES / f"ws-{suffix}.jsonl").read_text().splitlines()],
        )

    async def test_native_transport_options_and_retryable_handshake_error(self):
        from nautilus_trader.core import nautilus_pyo3 as native

        connect = AsyncMock(side_effect=native.WebSocketClientError("synthetic failure"))
        config = Mock(wraps=native.WebSocketConfig)
        with (
            patch.object(native, "WebSocketClient", Mock(connect=connect)),
            patch.object(native, "WebSocketConfig", config),
            self.assertRaisesRegex(transport.SessionLost, "WebSocket handshake failed"),
        ):
            await transport._connect()
        _, settings, receive = connect.await_args.args
        self.assertEqual(settings.url, "wss://api.hyperliquid.xyz/ws")
        self.assertEqual(config.call_args.kwargs["headers"], [])
        self.assertEqual(settings.heartbeat, 20)
        self.assertEqual(json.loads(settings.heartbeat_msg), {"method": "ping"})
        self.assertEqual(settings.reconnect_max_attempts, 0)
        self.assertTrue(callable(receive))

    async def test_subscription_timeout_and_queue_overflow_close_coverage(self):
        session = ReplaySession(_FIXTURES / "ws-initial.jsonl")
        with patch.object(transport, "_TIMEOUT", 0):
            with self.assertRaisesRegex(transport.SessionLost, "acknowledgement timed out"):
                await transport._stream_session(session, ("BTC",), Mock())

        live = transport._NautilusSession(RuntimeError)
        for _ in range(513):
            live._on_message(b'{"channel":"pong"}')
        with self.assertRaisesRegex(transport.SessionLost, "receive queue exceeded"):
            await live.receive()

    async def test_invalid_numeric_token_reconnects_without_logging_payload(self):
        first = ReplaySession(_FIXTURES / "ws-initial.jsonl")
        good = first.frames[0]
        huge_exponent = b"1e" + b"9" * 32
        bad = good.removesuffix(b"}") + b',"unused":' + huge_exponent + b"}"
        first.frames = [good, bad, good]
        second = ReplaySession(_FIXTURES / "ws-reconnected.jsonl")
        expected_fresh = second.frames[1].decode()
        events = []

        def emit(kind, _, **fields):
            events.append((kind, fields))

        sleep = AsyncMock(side_effect=[None, asyncio.CancelledError])
        errors = io.StringIO()
        with redirect_stderr(errors), self.assertRaises(asyncio.CancelledError):
            await transport.run(
                _COINS, connect=AsyncMock(side_effect=[first, second]),
                load_metadata=(_FIXTURES / "meta.json").read_text, emit=emit, sleep=sleep,
            )
        self.assertEqual([kind for kind, _ in events if kind != "frame"], [
            "metadata", "connected", "disconnected", "connected", "disconnected",
        ])
        self.assertEqual(events[2], ("frame", {"data": good.decode()}))
        self.assertEqual(events[3][0], "disconnected")
        self.assertIn(("frame", {"data": expected_fresh}), events[5:])
        self.assertEqual([len(session.sent) for session in (first, second)], [9, 9])
        self.assertTrue(first.closed and second.closed)
        self.assertEqual([call.args for call in sleep.await_args_list], [(1,), (2,)])
        self.assertIn("invalid WebSocket JSON", errors.getvalue())
        self.assertNotIn(huge_exponent.decode(), errors.getvalue())
