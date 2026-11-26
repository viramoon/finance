"""Deterministic tests using the F1 synthetic examples and field overrides."""

from dataclasses import replace
import json
from pathlib import Path
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import patch

import pyarrow.parquet as pq

from fertile_finance import market_projector
from fertile_finance.market_projector import (
    ProjectionError,
    WalRecord,
    open_projection,
    projected_partitions,
    read_latest_event,
)
from fertile_finance.market_protocol import Event, U64Text


_EXAMPLES = Path(__file__).resolve().parents[1] / "protocol" / "v1" / "examples.json"


class MemoryWal:
    def __init__(self, *events):
        self.events = list(events)

    def read_after(self, _source, _product, after, limit):
        batch = self.events[after:after + limit]
        return [WalRecord(position, event) for position, event in enumerate(batch, after + 1)]


class MarketProjectionTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        examples = json.loads(_EXAMPLES.read_text(encoding="utf-8"))
        cls.first, cls.second, cls.correction, cls.gap = (
            Event.from_dict(example) for example in examples["events"][:4]
        )
        cls.book = [Event.from_dict(row) for row in examples["events"][-2:]]

    def setUp(self):
        directory = TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.product = (self.first.source, self.first.product)
        self.marker = self.root.joinpath("finance-state", "market-projection", *self.product, "watermark.json")

    def watermark(self):
        return json.loads(self.marker.read_text(encoding="utf-8"))["projected_through"]

    def latest(self, original):
        return read_latest_event(
            self.root, *original.stream_key, original.epoch, original.seq,
        )

    def project(self, wal, limit=20):
        with open_projection(self.root, *self.product) as projection:
            return projection.project_batch(wal, limit)

    def partitions(self):
        return projected_partitions(self.root, *self.product)

    def test_same_timestamp_duplicate_and_increasing_revisions(self):
        # Identical prices, quantities and timestamps still describe two
        # different original trades because F1 assigned distinct positions.
        second = replace(self.second, payload=self.first.payload)
        duplicate = replace(self.first, receive_ts=second.receive_ts)
        revised_again = Event.from_dict(self.correction.to_dict() | {
            "seq": self.gap.seq, "cursor": "synthetic-revision-two", "revision": "2",
            "payload": {
                **self.correction.to_dict()["payload"],
                "replacement": {
                    "kind": "trade",
                    "payload": {"price": "663.1300", "quantity": "3", "side": "buy"},
                },
            },
        })
        wal = MemoryWal(self.first, second, duplicate, self.correction, revised_again)
        self.assertEqual(self.project(wal), 5)
        self.assertEqual(self.project(wal, 2), 0)
        self.assertEqual(self.watermark(), "5")
        partition, = self.partitions()
        self.assertEqual((partition.first_position, partition.last_position, partition.row_count), (1, 5, 4))
        rows = pq.ParquetFile(partition.path).read().to_pylist()
        self.assertEqual([row["wal_position"] for row in rows], [1, 2, 4, 5])
        self.assertEqual([row["revision"] for row in rows], [0, 0, 1, 2])
        self.assertEqual(rows[-1]["target_seq"], self.first.seq.integer)
        self.assertEqual([Event.from_json(row["event_json"]) for row in rows], [
            self.first, second, self.correction, revised_again,
        ])
        self.assertEqual(json.loads(rows[0]["event_json"])["payload"]["price"], "663.1200")
        self.assertEqual(self.latest(self.first), revised_again)
        self.assertEqual(self.latest(second), second)

    def test_unsigned_event_positions_round_trip_in_parquet_and_index(self):
        first, second = self.book
        self.product = (first.source, first.product)
        self.assertEqual(self.project(MemoryWal(first, second)), 2)
        partition, = self.partitions()
        table = pq.ParquetFile(partition.path).read()
        self.assertEqual(table["seq"].to_pylist(), [2**64 - 2, 2**64 - 1])
        self.assertEqual(self.latest(second), second)

    def test_retry_after_parquet_publish_keeps_original_range_on_a_growing_wal(self):
        wal = MemoryWal(self.first, self.second)
        publish = market_projector._publish_parquet
        destinations = []

        def fail_after_publish(path, rows):
            publish(path, rows)
            destinations.append(path)
            raise OSError("synthetic loss after publish")

        with patch.object(market_projector, "_publish_parquet", side_effect=fail_after_publish):
            with self.assertRaisesRegex(OSError, "synthetic loss"):
                self.project(wal, 2)
        path, = destinations
        inode = path.stat().st_ino
        self.assertEqual(self.partitions(), [])
        self.assertEqual(self.watermark(), "0")
        self.assertIsNone(self.latest(self.first))

        wal.events.extend((self.correction, self.gap))
        self.assertEqual(self.project(wal, 20), 2)
        self.assertEqual(path.stat().st_ino, inode)
        self.assertEqual(self.watermark(), "2")
        self.assertEqual(self.project(wal, 1), 1)
        self.assertEqual(self.project(wal, 20), 1)
        self.assertEqual(self.watermark(), "4")
        self.assertEqual([(part.first_position, part.last_position) for part in self.partitions()], [
            (1, 2), (3, 3), (4, 4),
        ])
        self.assertEqual(self.latest(self.first), self.correction)

    def test_checkpoint_commit_before_watermark_repair(self):
        wal = MemoryWal(self.first)
        with open_projection(self.root, *self.product) as projection:
            with patch.object(market_projector, "atomic_json", side_effect=OSError("synthetic disk fault")):
                with self.assertRaisesRegex(OSError, "synthetic disk fault"):
                    projection.project_batch(wal, 1)
        self.assertEqual(self.watermark(), "0")
        self.assertEqual(self.latest(self.first), self.first)
        self.assertEqual(self.project(wal), 0)
        self.assertEqual(self.watermark(), "1")
        self.assertEqual(len(self.partitions()), 1)

    def test_reject_conflicting_duplicate_without_advancing(self):
        wal = MemoryWal(self.first)
        self.assertEqual(self.project(wal), 1)
        wal.events.append(replace(self.first, payload=self.second.payload))
        with self.assertRaisesRegex(ProjectionError, "conflicting duplicate"):
            self.project(wal)
        self.assertEqual(self.watermark(), "1")
        self.assertEqual(len(self.partitions()), 1)
        self.assertEqual(self.latest(self.first), self.first)

    def test_reject_stale_revision_without_advancing(self):
        wal = MemoryWal(self.first, self.second, self.correction)
        self.assertEqual(self.project(wal), 3)
        wal.events.append(replace(self.correction, seq=self.gap.seq, cursor=self.gap.cursor))
        with self.assertRaisesRegex(ProjectionError, "invalid correction kind or revision"):
            self.project(wal)
        self.assertEqual(self.watermark(), "3")
        self.assertEqual(self.latest(self.first), self.correction)

    def test_reject_correction_without_original(self):
        with self.assertRaisesRegex(ProjectionError, "missing original"):
            self.project(MemoryWal(self.correction))
        self.assertEqual(self.watermark(), "0")
        self.assertEqual(self.partitions(), [])

    def test_duplicate_only_batch_advances_without_creating_partition(self):
        wal = MemoryWal(self.first, self.first)
        self.assertEqual(self.project(wal, 1), 1)
        self.assertEqual(self.project(wal, 1), 1)
        self.assertEqual(self.watermark(), "2")
        partition, = self.partitions()
        self.assertEqual((partition.first_position, partition.last_position), (1, 1))

    def test_exclusive_writer_and_contiguous_wal_page_checked(self):
        wal = MemoryWal(self.first)
        with open_projection(self.root, *self.product) as projection:
            with self.assertRaises(BlockingIOError), open_projection(self.root, *self.product):
                pass
            with patch.object(wal, "read_after", return_value=[WalRecord(2, self.first)]):
                with self.assertRaisesRegex(ProjectionError, "missing position"):
                    projection.project_batch(wal, 1)
        self.assertEqual(self.watermark(), "0")
        self.assertEqual(self.partitions(), [])


if __name__ == "__main__":
    unittest.main()
