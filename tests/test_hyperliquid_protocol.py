"""Check the generated perpetual-context contract through the F3 projector."""

import json
from pathlib import Path
from tempfile import TemporaryDirectory
import unittest

import pyarrow.parquet as pq

from fertile_finance.market_projector import (
    WalRecord,
    open_projection,
    projected_partitions,
    read_latest_event,
)
from fertile_finance.market_protocol import Capability, Event, Instrument, ProtocolError


class HyperliquidProtocolTests(unittest.TestCase):
    def test_generated_context_projects_and_indexes_exact_values(self):
        path = Path(__file__).resolve().parents[1] / "protocol" / "v2" / "examples.json"
        examples = json.loads(path.read_text())
        event, = map(Event.from_dict, examples["events"])
        instrument, = map(Instrument.from_dict, examples["instruments"])
        capability, = map(Capability.from_dict, examples["capabilities"])
        event.validate_against(instrument, capability)
        original, = examples["events"]
        self.assertEqual(event.to_dict(), original)
        with self.assertRaises(ProtocolError):
            Event.from_dict(original | {"schema_version": "1.0"})
        with self.assertRaises(ProtocolError):
            Event.from_dict(original | {"asset_kind": "spot"})
        with TemporaryDirectory() as directory:
            root = Path(directory)
            product = (event.source, event.product)

            class SyntheticWal:
                def read_after(self, source, name, after, limit):
                    assert (source, name, after, limit) == (*product, 0, 20)
                    return [WalRecord(1, event)]

            with open_projection(root, *product) as projection:
                self.assertEqual(projection.project_batch(SyntheticWal(), 20), 1)
            partition, = projected_partitions(root, *product)
            row, = pq.ParquetFile(partition.path).read().to_pylist()
            self.assertEqual(row["kind"], "perpetual_context")
            self.assertIsNone(row["exchange_ts"])
            self.assertEqual(Event.from_json(row["event_json"]).to_dict(), original)
            self.assertEqual(read_latest_event(root, *event.stream_key, event.epoch, event.seq), event)
