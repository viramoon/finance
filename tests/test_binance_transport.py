"""F4 transport coverage with hand-authored payloads and SDK error responses."""

import json
from pathlib import Path
import unittest
from unittest.mock import patch
from types import SimpleNamespace
from unittest.mock import AsyncMock

from fertile_finance import binance_transport as transport
from fertile_finance.market_protocol import Capability, ContextKind, Event, ProtocolError

ROOT = Path(__file__).resolve().parents[1]
FIXTURES = ROOT / "crates/finance-collector/fixtures/binance"


class BinanceTransportTests(unittest.IsolatedAsyncioTestCase):
    async def test_combined_stream_replays_diff_before_snapshot_and_keeps_exact_id(self):
        from nautilus_trader.core import nautilus_pyo3

        data = json.loads((FIXTURES / "depth.json").read_text())
        seen = []

        class Socket:
            active = True

            def is_active(self):
                return self.active

            async def disconnect(self):
                self.active = False

        socket = Socket()

        class Client:
            @staticmethod
            async def connect(_loop, config, handler):
                self.assertEqual(config.reconnect_max_attempts, 0)
                handler(json.dumps({"stream": "btcusdt@depth@100ms", "data": data["spot_bridge"]}).encode())
                return socket

        def emit(frame):
            seen.append(frame)
            if frame["type"] == "snapshot":
                socket.active = False

        with (
            patch.object(nautilus_pyo3, "WebSocketClient", Client),
            patch.object(transport, "rest", return_value=data["spot_snapshot"]) as rest,
        ):
            with self.assertRaises(ExceptionGroup):
                await transport.watch_stream(None, transport.SPOT, transport.SPOT.depth_stream, ("BTCUSDT",), emit)
        rest.assert_awaited_once_with(None, f"{transport.SPOT.http}/depth", {"symbol": "BTCUSDT", "limit": 1000})
        self.assertEqual([item["type"] for item in seen], ["stream", "snapshot"])
        self.assertEqual(seen[0]["data"]["u"], 9_007_199_254_741_001)
        self.assertEqual(seen[1]["data"]["bids"][0], ["10.01", "3.000"])
        self.assertFalse(socket.active)

    async def test_perpetual_uses_public_depth_and_market_trades_and_funding(self):
        urls = set()
        emitted = []

        async def watch(_client, _market, url, _symbols, _emit):
            urls.add(url)

        async def interest(_client, market, symbols, _emit):
            self.assertEqual(market, transport.PERPETUAL)
            self.assertEqual(symbols, ("BTCUSDT",))

        synthetic = {"symbols": [{"symbol": "BTCUSDT"}, {"symbol": "EXCLUDEDUSDT"}]}
        with (
            patch.object(transport, "rest", return_value=synthetic) as rest,
            patch.object(transport, "watch_stream", watch),
            patch.object(transport, "poll_interest", interest),
        ):
            await transport.run(False, ("BTCUSDT",), emitted.append)
        rest.assert_awaited_once()
        self.assertEqual(rest.await_args.args[1:], ("https://fapi.binance.com/fapi/v1/exchangeInfo", {}))
        self.assertEqual(urls, {
            "wss://fstream.binance.com/public/stream?streams=btcusdt@depth@100ms",
            "wss://fstream.binance.com/market/stream?streams=btcusdt@aggTrade/btcusdt@markPrice@1s",
        })
        self.assertEqual(emitted, [{
            "type": "metadata", "market": "perpetual", "data": {"symbols": [{"symbol": "BTCUSDT"}]},
        }])

    async def test_nautilus_upgrade_http_403_and_451_are_classified_without_retry(self):
        from nautilus_trader.core import nautilus_pyo3

        for code in (403, 451):
            with self.subTest(code=code):
                error = nautilus_pyo3.WebSocketClientError(f"handshake failed: HTTP status {code} Synthetic")
                connect = AsyncMock(side_effect=error)
                with patch.object(nautilus_pyo3, "WebSocketClient", SimpleNamespace(connect=connect)):
                    with self.assertRaises(transport.FetchFailure) as caught:
                        await transport.watch_stream(
                            None, transport.SPOT, transport.SPOT.depth_stream, ("BTCUSDT",), self.fail,
                        )
                self.assertEqual(caught.exception.status, code)
                connect.assert_awaited_once()
                config = connect.await_args.args[1]
                self.assertEqual(config.reconnect_max_attempts, 0)
                self.assertEqual(config.backend, nautilus_pyo3.TransportBackend.TUNGSTENITE)

    async def test_rest_refusal_forwards_status_and_retry_after(self):
        from nautilus_trader.core import nautilus_pyo3

        for code in (403, 451, 429):
            frames = []
            response = SimpleNamespace(status=code, headers={"retry-after": "120"})
            client = SimpleNamespace(get=AsyncMock(return_value=response))
            with patch.object(nautilus_pyo3, "HttpClient", return_value=client):
                await transport.run(True, (), frames.append)
            client.get.assert_awaited_once()
            self.assertEqual(frames, [{"type": "failure", "status": code, "retry_after": 120}])


class PerpetualContextProtocolTests(unittest.TestCase):
    def test_v2_context_round_trips_exact_values_and_v1_rejects_new_kind(self):
        event = {
            "schema_version": "2.0", "source": "binance", "product": "usdt-perp-context",
            "channel": "funding_rate", "instrument_id": "BTC-USDT-PERP.BINANCE",
            "asset_kind": "perpetual", "venue": "BINANCE", "exchange_ts": "1000000000",
            "receive_ts": "1000000001", "first_available_ts": "1000000001",
            "epoch": "1", "seq": "1", "revision": "0", "cursor": "1:1", "license": "fixture.synthetic",
            "kind": "funding_rate", "payload": {"rate": "-0.00000001", "next_funding_ts": "2000000000"},
        }
        funding = Event.from_dict(event)
        self.assertEqual(funding.to_dict(), event)
        self.assertEqual(funding.kind, ContextKind.FUNDING_RATE)
        interest = event | {
            "kind": "open_interest", "channel": "open_interest", "payload": {"quantity": "123.456789123456"},
        }
        self.assertEqual(Event.from_dict(interest).to_dict(), interest)
        with self.assertRaises(ProtocolError):
            Event.from_dict(event | {"schema_version": "1.0"})
        with self.assertRaises(ProtocolError):
            Event.from_dict(event | {"asset_kind": "spot"})
        with self.assertRaises(ProtocolError):
            Event.from_dict(interest | {"payload": {"quantity": "-1"}})
        capability = {
            "schema_version": "2.0", "source": "binance", "product": "usdt-perp-context",
            "message_kinds": ["funding_rate", "open_interest"], "max_depth": 0,
            "can_backfill": False, "delay_tier": "realtime",
        }
        self.assertEqual(Capability.from_dict(capability).to_dict(), capability)
        with self.assertRaises(ProtocolError):
            Capability.from_dict(capability | {"schema_version": "1.0"})


    def test_funding_requires_nullable_next_time_and_still_rejects_an_earlier_time(self):
        event = {
            "schema_version": "2.0", "source": "binance", "product": "usdt-perp-context",
            "channel": "funding_rate", "instrument_id": "BTC-USDT-PERP.BINANCE",
            "asset_kind": "perpetual", "venue": "BINANCE", "exchange_ts": "1000000000",
            "receive_ts": "1000000001", "first_available_ts": "1000000001",
            "epoch": "1", "seq": "1", "revision": "0", "cursor": "1:1", "license": "fixture.synthetic",
            "kind": "funding_rate", "payload": {"rate": "-0.00000001", "next_funding_ts": None},
        }
        parsed = Event.from_dict(event)
        self.assertIsNone(parsed.payload.next_funding_ts)
        self.assertEqual(Event.from_json(parsed.to_json()).to_dict(), event)
        for payload in ({"rate": "-0.00000001"}, {"rate": "-0.00000001", "next_funding_ts": "999999999"}):
            with self.subTest(payload=payload), self.assertRaises(ProtocolError) as error:
                Event.from_dict(event | {"payload": payload})
            self.assertEqual(error.exception.path, "/payload/next_funding_ts")
