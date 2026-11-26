"""Deterministic XNYS replay fixtures built from constant synthetic USD bars."""

from datetime import date
import hashlib
import json
import unittest

from fertile_finance.research_catalog import ResearchError, nyse
from fertile_finance.strategy_replay import REPLAY_SCHEMA, load_strategy_replay


FIRST = date.fromisoformat("2024-01-02")
LAST = date.fromisoformat("2024-01-04")
END = date.fromisoformat("2024-01-05")
INSTRUMENT = "SYNTH.XNYS"
BARS = "example.reviewed-us-daily@1.0.0"
UNIVERSE = "example.us-cash-equities@1.0.0"
METADATA = {
    "schema": REPLAY_SCHEMA, "bars_ref": BARS, "universe_ref": UNIVERSE,
    "calendar": "XNYS", "currency": "USD", "timeframe": "1d", "price_basis": "raw_split_free_reviewed",
}
PLAN = {
    "bundle": {"spec": {
        "data": {key: METADATA[key] for key in ("currency", "timeframe", "price_basis")} | {"bars": BARS},
        "universe": {"ref": UNIVERSE}, "clock": {"calendar": "XNYS"},
    }},
    "inputs": {"bars": {"id": BARS}, "universe": {"id": UNIVERSE}},
}


def fixture():
    schedule = nyse().schedule.loc[str(FIRST):str(LAST)]
    bars = [
        {
            "instrument_id": INSTRUMENT, "date": str(label.date()), "first_available_ts": str(close.value),
            "open": "100.00", "high": "100.00", "low": "100.00", "close": "100.00", "volume": "1000",
        }
        for label, close in schedule["close"].items()
    ]
    return {
        **METADATA, "first_session": str(FIRST), "last_session": str(LAST), "instruments": [INSTRUMENT],
        "membership": [{
            "date": str(FIRST), "first_available_ts": str(schedule["open"].iloc[0].value),
            "members": [INSTRUMENT],
        }],
        "bars": bars,
    }


def load(document, *, start=FIRST, end=END, digest=None):
    raw = document if isinstance(document, bytes) else json.dumps(document).encode("utf-8")
    return load_strategy_replay(raw, digest or hashlib.sha256(raw).hexdigest(), PLAN, start, end)


class StrategyReplayTests(unittest.TestCase):
    def test_late_bar_is_pinned_and_withheld_from_the_close(self):
        document = fixture()
        late = document["bars"][1]
        late["first_available_ts"] = str(int(late["first_available_ts"]) + 1)
        replay = load(document)
        self.assertEqual(replay.availability["excluded_late_bars"], 1)
        self.assertEqual(replay.data["partitions"][0]["rows"], 3)
        self.assertEqual(len(replay.assets[0].bars), 2)
        self.assertEqual(replay.calendar["timezone"], "America/New_York")
        self.assertNotEqual(replay.data["snapshot_sha256"], load(fixture()).data["snapshot_sha256"])

    def test_missing_boundary_bar_uses_declared_coverage(self):
        document = fixture()
        document["bars"].pop()
        replay = load(document)
        self.assertEqual(replay.end, LAST)
        self.assertEqual(len(replay.assets[0].bars), 2)
        with self.assertRaises(ResearchError) as caught:
            load(document, end=date(2024, 1, 6))
        self.assertEqual(caught.exception.code, "strategy_snapshot_range")

    def test_refuses_wrong_digest_late_membership_and_inexact_price(self):
        document = fixture()
        late = document["membership"][0]
        late["first_available_ts"] = str(int(late["first_available_ts"]) + 1)
        inexact = fixture()
        inexact["bars"][0]["close"] = 100.0
        bad_digest = fixture()
        duplicate = json.dumps(fixture()).replace('"currency": "USD"', '"currency": "USD", "currency": "USD"')
        for document, digest in ((document, None), (inexact, None), (bad_digest, "0" * 64), (duplicate.encode(), None)):
            with self.subTest(digest=digest, document_type=type(document).__name__):
                with self.assertRaises(ResearchError) as caught:
                    load(document, digest=digest)
                self.assertEqual(caught.exception.code, "strategy_snapshot_invalid")


if __name__ == "__main__":
    unittest.main()
