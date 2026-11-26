"""Python checks for the same market protocol examples exported by Rust."""

from contextlib import contextmanager
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

    @contextmanager
    def assert_protocol_error(self, path, kind, reason=None):
        with self.assertRaises(ProtocolError) as raised:
            yield
        self.assertEqual(raised.exception.path, path)
        self.assertEqual(raised.exception.kind, kind)
        if reason:
            self.assertIn(reason, str(raised.exception))

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
        index = self.instrument("SPX.CBOE")
        self.assertEqual(index.asset_kind, AssetKind.INDEX)
        self.assertIsNone(index.settlement_currency)
        perp = self.instrument("BTC-USDT-PERP.BINANCE")
        self.assertEqual((perp.asset_kind, perp.quote_currency, perp.calendar_id), (
            AssetKind.PERPETUAL, "USDT", "24X7",
        ))

        for changes, path, kind in (
            ({"venue": "BATS"}, "/venue", "constraint_violation"),
            ({"instrument_id": "SPY..ARCA"}, "/instrument_id", "invalid_value"),
            ({"instrument_id": f"{'A' * 97}.ARCA"}, "/instrument_id", "invalid_value"),
            ({"calendar_id": "xnys"}, "/calendar_id", "invalid_value"),
            ({"settlement_currency": None}, "/settlement_currency", "constraint_violation"),
            ({"tick_size": "0"}, "/tick_size", "constraint_violation"),
        ):
            with self.subTest(changes=changes), self.assert_protocol_error(path, kind):
                Instrument.from_dict(arca.to_dict() | changes)
        for currency, kind in ((0, "invalid_type"), ("usd", "invalid_value"), ("USD", "constraint_violation")):
            with self.subTest(currency=currency), self.assert_protocol_error("/settlement_currency", kind):
                Instrument.from_json(json.dumps(index.to_dict() | {"settlement_currency": currency}))

        mapping = arca.to_dict()["supplier_symbols"][0]
        with self.assert_protocol_error("/supplier_symbols/1", "constraint_violation", "duplicate"):
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
        for value in ("6.6312e2", "1e2", "1E2", "01.25", "+1", "1.", ".1", "1\n", "９", 1.5, "9" * 29):
            invalid = deepcopy(event)
            invalid["payload"]["price"] = value
            kind = "invalid_decimal" if isinstance(value, str) else "invalid_type"
            with self.subTest(price=value), self.assert_protocol_error("/payload/price", kind):
                Event.from_json(json.dumps(invalid))
        for value, kind in (
            (True, "invalid_type"),
            (1_760_000_000_123_456_789, "invalid_type"),
            ("18446744073709551616", "out_of_range"),
            ("00", "invalid_integer"),
            ("-1", "invalid_integer"),
            ("12\n", "invalid_integer"),
            (None, "invalid_type"),
        ):
            with self.subTest(receive_ts=value), self.assert_protocol_error("/receive_ts", kind):
                Event.from_dict(event | {"receive_ts": value})
        for value, kind in ((1, "invalid_type"), ("18446744073709551616", "out_of_range"), ("01", "invalid_integer")):
            with self.subTest(seq=value), self.assert_protocol_error("/seq", kind):
                Event.from_json(json.dumps(event | {"seq": value}))
        for kind in ("option_chain", "classified_trade"):
            with self.subTest(kind=kind), self.assert_protocol_error("/kind", "invalid_value"):
                Event.from_dict(event | {"kind": kind})
        self.assertEqual(U64Text("18446744073709551615").integer, 2**64 - 1)
        self.assertEqual(DecimalText("-0.00"), "-0.00")

    def test_decode_errors_in_nested_and_indexed_fields(self):
        correction = self.event(Kind.CORRECTION)
        correction["payload"]["replacement"]["payload"]["price"] = "6.6312e2"
        with self.assert_protocol_error("/payload/replacement/payload/price", "invalid_decimal"):
            Event.from_json(json.dumps(correction))
        snapshot = self.event(Kind.SNAPSHOT)
        snapshot["payload"]["asks"][0]["quantity"] = 1
        with self.assert_protocol_error("/payload/asks/0/quantity", "invalid_type"):
            Event.from_json(json.dumps(snapshot))

    def test_raw_json_numbers_keep_field_diagnostics(self):
        trade = self.event()
        trade["payload"]["quantity"] = "RAW_NUMBER"
        trade_wire = json.dumps(trade)
        long_integer = "9" * 4301
        huge_exponent = "1e1000000000000000000"
        for name, number in (
            ("large exponent", huge_exponent),
            ("small exponent", "1e-1000000000000000000"),
            ("long integer", long_integer),
        ):
            with self.subTest(quantity=name), self.assert_protocol_error("/payload/quantity", "invalid_type"):
                Event.from_json(trade_wire.replace('"RAW_NUMBER"', number, 1))

        for constant in ("NaN", "Infinity", "-Infinity"):
            with self.subTest(constant=constant), self.assert_protocol_error("", "invalid_json", constant):
                Event.from_json(trade_wire.replace('"RAW_NUMBER"', constant, 1))

        book = next(row for row in self.examples["capabilities"] if row["max_depth"] > 0)
        depth_wire = json.dumps(book | {"max_depth": "RAW_NUMBER"})
        for name, number, kind in (
            ("large exponent", huge_exponent, "invalid_type"),
            ("long positive integer", long_integer, "out_of_range"),
            ("long negative integer", "-" + long_integer, "out_of_range"),
            ("quoted integer", '"1"', "invalid_type"),
        ):
            with self.subTest(max_depth=name), self.assert_protocol_error("/max_depth", kind):
                Capability.from_json(depth_wire.replace('"RAW_NUMBER"', number, 1))

        depth_limit = 2**32 - 1
        capability = Capability.from_json(depth_wire.replace('"RAW_NUMBER"', str(depth_limit), 1))
        self.assertIs(type(capability.max_depth), int)
        self.assertEqual(capability.max_depth, depth_limit)

    def test_strict_fields_and_duplicates(self):
        event = self.event()
        for key in ("schema_version", "exchange_ts", "receive_ts", "first_available_ts", "license", "kind", "payload"):
            with self.subTest(missing=key), self.assert_protocol_error(f"/{key}", "missing_field"):
                Event.from_dict({name: value for name, value in event.items() if name != key})
        with self.assert_protocol_error("/extra", "unknown_field"):
            Event.from_dict(event | {"extra": 0})
        with self.assert_protocol_error("/payload/extra", "unknown_field"):
            Event.from_dict(event | {"payload": event["payload"] | {"extra": 0}})
        with self.assert_protocol_error("/payload/a~1b~0c", "unknown_field"):
            Event.from_dict(event | {"payload": event["payload"] | {"a/b~c": 0}})
        with self.assert_protocol_error("/payload/quantity", "missing_field"):
            Event.from_json(json.dumps(event | {"payload": {"price": "1", "side": "buy"}}))
        with self.assert_protocol_error("/native_id", "invalid_type"):
            Event.from_dict(event | {"native_id": None})
        with self.assert_protocol_error("/schema_version", "invalid_value"):
            Event.from_dict(event | {"schema_version": "1.1"})

        wire = json.dumps(event)
        with self.assert_protocol_error("/source", "duplicate_field"):
            Event.from_json('{"source":"fixture",' + wire[1:])
        with self.assert_protocol_error("/payload/price", "duplicate_field"):
            Event.from_json(wire.replace('"price":', '"price":"100","price":', 1))
        snapshot = json.dumps(self.event(Kind.SNAPSHOT))
        with self.assert_protocol_error("/payload/asks/0/quantity", "duplicate_field"):
            Event.from_json(snapshot.replace('"quantity":', '"quantity":"1","quantity":', 1))
        with self.assert_protocol_error("", "invalid_json", "NaN"):
            Event.from_json('{"source": NaN}')
        with self.assert_protocol_error("/cursor", "invalid_value"):
            Event.from_json(json.dumps(event | {"cursor": "\ud800"}))

    def test_tick_and_lot_validation_use_exact_arithmetic(self):
        raw = self.event()
        event = Event.from_dict(raw)
        instrument = self.instrument(event.instrument_id)
        capability = self.capability(event.source, event.product)
        for field, value, step in (("price", "663.1201", "tick_size"), ("quantity", "1.001", "lot_size")):
            with self.subTest(field=field), self.assert_protocol_error(f"/payload/{field}", "constraint_violation", step):
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
            with self.assert_protocol_error("/payload/price", "constraint_violation", "tick_size"):
                large_trade.validate_against(almost_tick, capability)

    def test_ticks_and_lots_apply_to_replacement_and_book(self):
        raw = self.event(Kind.CORRECTION)
        replacement = raw["payload"]["replacement"]
        replacement["payload"]["price"] = "663.1201"
        correction = Event.from_dict(raw)
        with self.assert_protocol_error("/payload/replacement/payload/price", "constraint_violation", "tick_size"):
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
        with self.assert_protocol_error("/payload/bids", "constraint_violation", "max_depth"):
            Event.from_dict(raw).validate_against(instrument, shallow)
        raw["payload"]["bids"].reverse()
        with self.assert_protocol_error("/payload/bids/1/price", "constraint_violation", "strictly ordered"):
            Event.from_dict(raw)

        raw = self.event(Kind.DELTA)
        for quantity in ("-0.01", "0.00"):
            raw["payload"]["updates"][0]["quantity"] = quantity
            with self.subTest(quantity=quantity):
                if quantity.startswith("-"):
                    with self.assert_protocol_error("/payload/updates/0/quantity", "constraint_violation", "nonnegative"):
                        Event.from_dict(raw)
                else:
                    delta = Event.from_dict(raw)
                    delta.validate_against(instrument, shallow)

    def test_capability_mismatches(self):
        event = Event.from_dict(self.event())
        instrument = self.instrument(event.instrument_id)
        capability = self.capability(event.source, event.product)
        with self.assert_protocol_error("/instrument_id", "constraint_violation", "match"):
            event.validate_against(self.instrument("SPY.BATS"), capability)
        with self.assert_protocol_error("/product", "constraint_violation", "match"):
            event.validate_against(instrument, Capability.from_dict(capability.to_dict() | {"product": "other"}))
        bars_only = Capability.from_dict(capability.to_dict() | {"message_kinds": ["bar_closed"]})
        with self.assert_protocol_error("/kind", "constraint_violation", "supported"):
            event.validate_against(instrument, bars_only)
        with self.assert_protocol_error("/payload/replacement/kind", "constraint_violation", "supported"):
            Event.from_dict(self.event(Kind.CORRECTION)).validate_against(instrument, bars_only)
        for changes, path, kind in (
            ({"message_kinds": []}, "/message_kinds", "constraint_violation"),
            ({"message_kinds": ["trade", "trade"]}, "/message_kinds", "constraint_violation"),
            ({"message_kinds": ["gap"]}, "/message_kinds/0", "invalid_value"),
            ({"message_kinds": ["option_chain"]}, "/message_kinds/0", "invalid_value"),
            ({"max_depth": 1}, "/max_depth", "constraint_violation"),
            ({"max_depth": True}, "/max_depth", "invalid_type"),
            ({"can_backfill": 1}, "/can_backfill", "invalid_type"),
        ):
            with self.subTest(changes=changes), self.assert_protocol_error(path, kind):
                Capability.from_dict(capability.to_dict() | changes)
        for spelling, kind in (
            ("-0", "invalid_type"),
            ("0.0", "invalid_type"),
            ("0e0", "invalid_type"),
            ("1e1000", "invalid_type"),
            ("4294967296", "out_of_range"),
            ("18446744073709551616", "out_of_range"),
            ("-9223372036854775809", "out_of_range"),
        ):
            wire = capability.to_json().replace('"max_depth":0', f'"max_depth":{spelling}', 1)
            with self.subTest(max_depth=spelling), self.assert_protocol_error("/max_depth", kind):
                Capability.from_json(wire)

    def test_seq_epoch_gaps_and_resets(self):
        raw = self.event()
        previous = Event.from_dict(raw)
        for changes, path, reason in (
            ({"seq": str(previous.seq.integer - 1)}, "/seq", "regressed"),
            ({"seq": str(previous.seq.integer)}, "/seq", "repeated"),
            ({"seq": str(previous.seq.integer + 2)}, "/seq", "gap"),
            ({"epoch": str(previous.epoch.integer + 1)}, "/epoch", "requires reset"),
            ({"epoch": str(previous.epoch.integer - 1)}, "/epoch", "regressed"),
            ({"channel": "other"}, "", "stream key changed"),
        ):
            candidate = Event.from_dict(raw | changes)
            with self.subTest(changes=changes), self.assert_protocol_error(path, "constraint_violation", reason):
                candidate.validate_successor(previous)
        reset = self.event(Kind.RESET)
        Event.from_dict(reset).validate_successor(previous)
        with self.assert_protocol_error("/epoch", "constraint_violation", "newer"):
            Event.from_dict(reset | {"epoch": str(previous.epoch)}).validate_successor(previous)
        with self.assert_protocol_error("/seq", "constraint_violation", "zero"):
            Event.from_dict(reset | {"seq": "1"})

    def test_correction_target_control_and_clock_constraints(self):
        correction = self.event(Kind.CORRECTION)
        with self.assert_protocol_error("/revision", "constraint_violation", "positive"):
            Event.from_dict(correction | {"revision": "0"})
        correction["payload"]["target_seq"] = correction["seq"]
        with self.assert_protocol_error("/payload/target_seq", "constraint_violation", "precede"):
            Event.from_dict(correction)

        heartbeat = self.event(Kind.HEARTBEAT)
        self.assertIsNone(Event.from_dict(heartbeat | {"exchange_ts": None}).exchange_ts)
        with self.assert_protocol_error("/payload/extra", "unknown_field"):
            Event.from_dict(heartbeat | {"payload": {"extra": 1}})
        trade = self.event()
        with self.assert_protocol_error("/exchange_ts", "constraint_violation", "required"):
            Event.from_dict(trade | {"exchange_ts": None})

        independently_clocked = Event.from_dict(trade | {"receive_ts": "0"})
        self.assertEqual(independently_clocked.receive_ts.integer, 0)
        self.assertGreater(independently_clocked.exchange_ts.integer, 0)

        bar = self.event(Kind.BAR_CLOSED)
        bar["payload"]["open_ts"] = bar["exchange_ts"]
        with self.assert_protocol_error("/payload/open_ts", "constraint_violation", "precede"):
            Event.from_dict(bar)

        gap = self.event(Kind.GAP)
        gap["payload"]["from_ts"] = str(int(gap["payload"]["to_ts"]) + 1)
        with self.assert_protocol_error("/payload/to_ts", "constraint_violation", "after"):
            Event.from_dict(gap)


if __name__ == "__main__":
    unittest.main()
