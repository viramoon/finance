"""Python checks for the same market protocol examples exported by Rust."""

from copy import deepcopy
from decimal import Decimal, localcontext
import json
from pathlib import Path
import unittest

from fertile_finance.market_protocol import (
    AssetKind,
    Capability,
    Correction,
    DecimalText,
    DelayTier,
    Event,
    Instrument,
    Kind,
    ProtocolError,
    Trade,
    U64Text,
)


_EXAMPLES = Path(__file__).resolve().parents[1] / "protocol" / "v1" / "examples.json"


class MarketProtocolTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.examples = json.loads(_EXAMPLES.read_text(encoding="utf-8"))

    def event(self, kind=Kind.TRADE):
        return deepcopy(next(row for row in self.examples["events"] if row["kind"] == kind))

    def instrument(self, instrument_id):
        return Instrument.from_dict(next(
            row for row in self.examples["instruments"] if row["instrument_id"] == instrument_id
        ))

    def capability(self, source, product):
        return Capability.from_dict(next(
            row for row in self.examples["capabilities"]
            if (row["source"], row["product"]) == (source, product)
        ))

    def test_rust_exported_examples_roundtrip(self):
        groups = (("instruments", Instrument), ("capabilities", Capability), ("events", Event))
        for name, model in groups:
            for index, raw in enumerate(self.examples[name]):
                with self.subTest(group=name, index=index):
                    typed = model.from_json(json.dumps(raw).encode())
                    self.assertEqual(typed.to_dict(), raw)
                    self.assertEqual(json.loads(typed.to_json()), raw)
                    self.assertEqual(model.from_json(typed.to_json()), typed)

        positions = {}
        for raw in self.examples["events"]:
            event = Event.from_dict(raw)
            event.validate_against(
                self.instrument(event.instrument_id),
                self.capability(event.source, event.product),
            )
            event.validate_successor(positions.get(event.stream_key))
            positions[event.stream_key] = event
        self.assertSetEqual({event["kind"] for event in self.examples["events"]}, set(Kind))

    def test_instrument_identity_calendar_and_supplier_mapping(self):
        arca = self.instrument("SPY.ARCA")
        bats = self.instrument("SPY.BATS")
        self.assertEqual(arca.asset_kind, AssetKind.ETF)
        self.assertEqual(arca.calendar_id, "XNYS")
        self.assertNotEqual(arca.instrument_id, bats.instrument_id)
        self.assertNotEqual(
            arca.instrument_id, self.instrument("SPY-SPLIT-NORMALIZED.RESEARCH").instrument_id,
        )
        self.assertEqual(self.instrument("SPX.CBOE").asset_kind, AssetKind.INDEX)
        self.assertIsNone(self.instrument("SPX.CBOE").settlement_currency)
        perp = self.instrument("BTC-USDT-PERP.BINANCE")
        self.assertEqual((perp.asset_kind, perp.quote_currency, perp.calendar_id), (
            AssetKind.PERPETUAL, "USDT", "24X7",
        ))

        for changes, path in (
            ({"venue": "BATS"}, "instrument_id"),
            ({"instrument_id": "SPY..ARCA"}, "instrument_id"),
            ({"instrument_id": f"{'A' * 97}.ARCA"}, "instrument_id"),
            ({"calendar_id": "xnys"}, "calendar_id"),
            ({"settlement_currency": None}, "settlement_currency"),
            ({"tick_size": "0"}, "tick_size"),
        ):
            with self.subTest(changes=changes), self.assertRaisesRegex(ProtocolError, path):
                Instrument.from_dict(arca.to_dict() | changes)

        mapping = arca.to_dict()["supplier_symbols"][0]
        with self.assertRaisesRegex(ProtocolError, "supplier_symbols.*duplicate"):
            Instrument.from_dict(arca.to_dict() | {
                "supplier_symbols": [mapping, mapping | {"symbol": "ALIAS"}],
            })

    def test_same_time_trades_delayed_availability_and_correction(self):
        trades = [
            Event.from_dict(raw)
            for raw in self.examples["events"]
            if raw["kind"] == Kind.TRADE and raw["venue"] == "ARCA"
        ]
        first, second = trades
        self.assertEqual(first.exchange_ts.integer, 1_760_000_000_123_456_789)
        self.assertEqual(first.exchange_ts, second.exchange_ts)
        self.assertNotEqual(first.seq, second.seq)
        self.assertNotEqual(first.native_id, second.native_id)
        self.assertGreater(first.seq.integer, 2**53 - 1)
        self.assertIsInstance(first.payload, Trade)
        self.assertEqual(first.payload.price, "663.1200")
        self.assertEqual(first.payload.price.decimal.as_tuple(), Decimal("663.1200").as_tuple())
        capability = self.capability(first.source, first.product)
        self.assertEqual(capability.delay_tier, DelayTier.DELAYED)
        self.assertEqual(first.first_available_ts.integer - first.exchange_ts.integer, 20 * 60 * 10**9)

        correction = Event.from_dict(self.event(Kind.CORRECTION))
        self.assertIsInstance(correction.payload, Correction)
        target = (correction.payload.target_epoch, correction.payload.target_seq)
        self.assertIn(target, [(trade.epoch, trade.seq) for trade in trades])
        self.assertEqual(correction.revision.integer, 1)
        second.validate_successor(first)
        correction.validate_successor(second)
        self.assertNotEqual(correction.cursor, second.cursor)

    def test_invalid_decimal_timestamps_and_reserved_kind(self):
        event = self.event()
        for value in ("1e2", "1E2", "01.25", "+1", "1.", ".1", "1\n", "９", 1.5, "9" * 29):
            invalid = deepcopy(event)
            invalid["payload"]["price"] = value
            with self.subTest(price=value), self.assertRaisesRegex(ProtocolError, "payload.price"):
                Event.from_json(json.dumps(invalid))
        for value in (True, 1_760_000_000_123_456_789, "18446744073709551616", "00", "-1", "12\n", None):
            with self.subTest(receive_ts=value), self.assertRaisesRegex(ProtocolError, "receive_ts"):
                Event.from_dict(event | {"receive_ts": value})
        for kind in ("option_chain", "classified_trade"):
            with self.subTest(kind=kind), self.assertRaisesRegex(ProtocolError, "kind"):
                Event.from_dict(event | {"kind": kind})
        self.assertEqual(U64Text("18446744073709551615").integer, 2**64 - 1)
        self.assertEqual(DecimalText("-0.00"), "-0.00")

    def test_strict_fields_and_duplicates(self):
        event = self.event()
        for key in ("schema_version", "exchange_ts", "receive_ts", "first_available_ts", "license", "kind", "payload"):
            with self.subTest(missing=key), self.assertRaisesRegex(ProtocolError, f"{key}.*missing"):
                Event.from_dict({name: value for name, value in event.items() if name != key})
        with self.assertRaisesRegex(ProtocolError, "extra.*unknown"):
            Event.from_dict(event | {"extra": 0})
        with self.assertRaisesRegex(ProtocolError, "payload.extra.*unknown"):
            Event.from_dict(event | {"payload": event["payload"] | {"extra": 0}})
        with self.assertRaisesRegex(ProtocolError, "native_id"):
            Event.from_dict(event | {"native_id": None})
        with self.assertRaisesRegex(ProtocolError, "schema_version"):
            Event.from_dict(event | {"schema_version": "1.1"})

        wire = json.dumps(event)
        with self.assertRaisesRegex(ProtocolError, "source.*duplicate"):
            Event.from_json('{"source":"fixture",' + wire[1:])
        with self.assertRaisesRegex(ProtocolError, "price.*duplicate"):
            Event.from_json(wire.replace('"price":', '"price":"100","price":', 1))
        with self.assertRaisesRegex(ProtocolError, "JSON.*NaN"):
            Event.from_json('{"source": NaN}')
        with self.assertRaisesRegex(ProtocolError, "cursor"):
            Event.from_json(json.dumps(event | {"cursor": "\ud800"}))

    def test_tick_and_lot_validation_use_exact_arithmetic(self):
        raw = self.event()
        event = Event.from_dict(raw)
        instrument = self.instrument(event.instrument_id)
        capability = self.capability(event.source, event.product)
        for field, value, step in (("price", "663.1201", "tick_size"), ("quantity", "1.001", "lot_size")):
            with self.subTest(field=field), self.assertRaisesRegex(ProtocolError, f"{field}.*{step}"):
                altered = Event.from_dict(raw | {"payload": raw["payload"] | {field: value}})
                altered.validate_against(instrument, capability)

        # Global Decimal arithmetic context must not round grid calculations,
        # including prices whose scales differ greatly from the tick.
        tiny = "0." + "0" * 26 + "1"
        huge = "9" * 28
        self.assertEqual(len(tiny.replace(".", "")), 28)
        with localcontext() as context:
            context.prec = 2
            small_tick = Instrument.from_dict(instrument.to_dict() | {"tick_size": tiny})
            large_trade = Event.from_dict(raw | {"payload": raw["payload"] | {"price": huge}})
            large_trade.validate_against(small_tick, capability)
            almost_tick = Instrument.from_dict(instrument.to_dict() | {"tick_size": tiny[:-1] + "7"})
            with self.assertRaisesRegex(ProtocolError, "price.*tick_size"):
                large_trade.validate_against(almost_tick, capability)

    def test_ticks_and_lots_apply_to_replacement_and_book(self):
        raw = self.event(Kind.CORRECTION)
        replacement = raw["payload"]["replacement"]
        replacement["payload"]["price"] = "663.1201"
        correction = Event.from_dict(raw)
        with self.assertRaisesRegex(ProtocolError, "replacement.payload.price.*tick_size"):
            correction.validate_against(
                self.instrument(correction.instrument_id),
                self.capability(correction.source, correction.product),
            )

        raw = self.event(Kind.SNAPSHOT)
        snapshot = Event.from_dict(raw)
        instrument = self.instrument(snapshot.instrument_id)
        raw["payload"]["bids"] = [
            {"price": "100.00", "quantity": "1"},
            {"price": "99.00", "quantity": "1"},
        ]
        shallow = Capability.from_dict(
            self.capability(snapshot.source, snapshot.product).to_dict() | {"max_depth": 1}
        )
        with self.assertRaisesRegex(ProtocolError, "bids.*max_depth"):
            Event.from_dict(raw).validate_against(instrument, shallow)
        raw["payload"]["bids"].reverse()
        with self.assertRaisesRegex(ProtocolError, "bids.*strictly ordered"):
            Event.from_dict(raw)

        raw = self.event(Kind.DELTA)
        for quantity in ("-0.01", "0.00"):
            raw["payload"]["updates"][0]["quantity"] = quantity
            with self.subTest(quantity=quantity):
                if quantity.startswith("-"):
                    with self.assertRaisesRegex(ProtocolError, "quantity.*nonnegative"):
                        Event.from_dict(raw)
                else:
                    delta = Event.from_dict(raw)
                    delta.validate_against(instrument, shallow)

    def test_capability_mismatches(self):
        event = Event.from_dict(self.event())
        instrument = self.instrument(event.instrument_id)
        capability = self.capability(event.source, event.product)
        with self.assertRaisesRegex(ProtocolError, "instrument_id.*match"):
            event.validate_against(self.instrument("SPY.BATS"), capability)
        with self.assertRaisesRegex(ProtocolError, "product.*match"):
            event.validate_against(instrument, Capability.from_dict(capability.to_dict() | {"product": "other"}))
        bars_only = Capability.from_dict(capability.to_dict() | {"message_kinds": ["bar_closed"]})
        with self.assertRaisesRegex(ProtocolError, "kind.*supported"):
            event.validate_against(instrument, bars_only)
        with self.assertRaisesRegex(ProtocolError, "replacement.kind.*supported"):
            Event.from_dict(self.event(Kind.CORRECTION)).validate_against(instrument, bars_only)
        for changes, path in (
            ({"message_kinds": []}, "message_kinds"),
            ({"message_kinds": ["trade", "trade"]}, "message_kinds"),
            ({"message_kinds": ["gap"]}, "message_kinds"),
            ({"message_kinds": ["option_chain"]}, "message_kinds"),
            ({"max_depth": 1}, "max_depth"),
            ({"max_depth": True}, "max_depth"),
            ({"can_backfill": 1}, "can_backfill"),
        ):
            with self.subTest(changes=changes), self.assertRaisesRegex(ProtocolError, path):
                Capability.from_dict(capability.to_dict() | changes)
        for spelling in ("-0", "0.0", "0e0"):
            wire = capability.to_json().replace('"max_depth":0', f'"max_depth":{spelling}', 1)
            with self.subTest(max_depth=spelling), self.assertRaisesRegex(ProtocolError, "max_depth"):
                Capability.from_json(wire)

    def test_seq_epoch_gaps_and_resets(self):
        raw = self.event()
        previous = Event.from_dict(raw)
        for changes, reason in (
            ({"seq": str(previous.seq.integer - 1)}, "seq.*regressed"),
            ({"seq": str(previous.seq.integer)}, "seq.*repeated"),
            ({"seq": str(previous.seq.integer + 2)}, "seq.*gap"),
            ({"epoch": str(previous.epoch.integer + 1)}, "epoch.*requires reset"),
            ({"epoch": str(previous.epoch.integer - 1)}, "epoch.*regressed"),
            ({"channel": "other"}, "stream key changed"),
        ):
            candidate = Event.from_dict(raw | changes)
            with self.subTest(changes=changes), self.assertRaisesRegex(ProtocolError, reason):
                candidate.validate_successor(previous)
        reset = self.event(Kind.RESET)
        Event.from_dict(reset).validate_successor(previous)
        with self.assertRaisesRegex(ProtocolError, "epoch.*newer"):
            Event.from_dict(reset | {"epoch": str(previous.epoch)}).validate_successor(previous)
        with self.assertRaisesRegex(ProtocolError, "seq.*zero"):
            Event.from_dict(reset | {"seq": "1"})

    def test_correction_target_control_and_clock_constraints(self):
        correction = self.event(Kind.CORRECTION)
        with self.assertRaisesRegex(ProtocolError, "revision.*positive"):
            Event.from_dict(correction | {"revision": "0"})
        correction["payload"]["target_seq"] = correction["seq"]
        with self.assertRaisesRegex(ProtocolError, "target_seq.*precede"):
            Event.from_dict(correction)

        heartbeat = self.event(Kind.HEARTBEAT)
        self.assertIsNone(Event.from_dict(heartbeat | {"exchange_ts": None}).exchange_ts)
        with self.assertRaisesRegex(ProtocolError, "payload.extra.*unknown"):
            Event.from_dict(heartbeat | {"payload": {"extra": 1}})
        trade = self.event()
        with self.assertRaisesRegex(ProtocolError, "exchange_ts.*required"):
            Event.from_dict(trade | {"exchange_ts": None})

        independently_clocked = Event.from_dict(trade | {"receive_ts": "0"})
        self.assertEqual(independently_clocked.receive_ts.integer, 0)
        self.assertGreater(independently_clocked.exchange_ts.integer, 0)

        bar = self.event(Kind.BAR_CLOSED)
        bar["payload"]["open_ts"] = bar["exchange_ts"]
        with self.assertRaisesRegex(ProtocolError, "open_ts.*precede"):
            Event.from_dict(bar)

        gap = self.event(Kind.GAP)
        gap["payload"]["from_ts"] = str(int(gap["payload"]["to_ts"]) + 1)
        with self.assertRaisesRegex(ProtocolError, "to_ts.*after"):
            Event.from_dict(gap)


if __name__ == "__main__":
    unittest.main()
