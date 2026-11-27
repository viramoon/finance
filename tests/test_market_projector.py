"""Deterministic tests using the F1 synthetic examples and field overrides."""

from dataclasses import replace
import json
import os
from pathlib import Path
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import patch

import pyarrow as pa
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

    def assert_readers_reject(self, message):
        with self.assertRaisesRegex(ProjectionError, message):
            self.partitions()
        with self.assertRaisesRegex(ProjectionError, message):
            self.latest(self.first)

    def test_projection_writes_and_reads_a_committed_batch(self):
        self.assertEqual(self.project(MemoryWal(self.first)), 1)
        self.assertEqual(self.watermark(), "1")
        partition, = self.partitions()
        self.assertTrue(partition.path.is_file())
        self.assertEqual(self.latest(self.first), self.first)

    def test_latest_event_requires_a_unique_wal_position_in_the_partition(self):
        self.project(MemoryWal(self.first, self.second))
        partition, = self.partitions()
        with pq.ParquetFile(partition.path) as reader:
            table = reader.read()
        self.assertEqual(self.latest(self.second), self.second)

        for indices in ([0], [0, 1, 1]):
            with self.subTest(indices=indices):
                pq.write_table(table.take(indices), partition.path)
                with self.assertRaisesRegex(ProjectionError, "no unique WAL position 0*2$"):
                    self.latest(self.second)

    def test_latest_event_distinguishes_adjacent_wal_positions_at_uint64_max(self):
        first_position, last_position = 2**64 - 2, 2**64 - 1
        records = [WalRecord(first_position, self.first), WalRecord(last_position, self.second)]
        wal = MemoryWal()

        with open_projection(self.root, *self.product) as projection:
            # Seed the checkpoint so a two-record WAL page reaches the uint64 boundary.
            with projection.db:
                projection.db.execute(
                    "UPDATE checkpoint SET projected_through=? WHERE singleton=1",
                    (str(first_position - 1),),
                )
            with patch.object(wal, "read_after", return_value=records):
                self.assertEqual(projection.project_batch(wal, 2), 2)

        partition, = self.partitions()
        self.assertEqual((partition.first_position, partition.last_position), (first_position, last_position))
        self.assertEqual(self.latest(self.first), self.first)
        self.assertEqual(self.latest(self.second), self.second)

    def test_projection_readers_reject_directory_and_file_symlinks(self):
        self.project(MemoryWal(self.first))
        partition, = self.partitions()
        index = self.marker.with_name("projection.sqlite3")
        with TemporaryDirectory() as outside:
            outside = Path(outside)
            for path in (index.parent.parent, index, partition.path.parent, partition.path):
                for target_dir in (outside, outside / "worktree"):
                    with self.subTest(path=path, target_dir=target_dir):
                        target_dir.mkdir(exist_ok=True)
                        if target_dir != outside:
                            (target_dir / ".git").mkdir(exist_ok=True)
                            (target_dir / ".git" / "HEAD").write_text("ref: refs/heads/fixture\n", encoding="utf-8")
                        target = target_dir / path.name
                        is_directory = path.is_dir()
                        path.rename(target)
                        path.symlink_to(target, target_is_directory=is_directory)
                        self.assert_readers_reject("symbolic links are forbidden")
                        path.unlink()
                        target.rename(path)

    def test_projection_readers_reject_dangling_index_and_sqlite_sidecar_links(self):
        self.project(MemoryWal(self.first))
        self.marker.with_name("projection.sqlite3-journal").symlink_to("missing-journal")
        self.assert_readers_reject("symbolic links are forbidden")
        self.marker.with_name("projection.sqlite3-journal").unlink()
        index = self.marker.with_name("projection.sqlite3")
        index.unlink()
        index.symlink_to("missing-index")
        self.assert_readers_reject("symbolic links are forbidden")

    def test_projection_readers_reject_git_worktree_ancestors_and_nested_checkouts(self):
        self.project(MemoryWal(self.first))
        partition, = self.partitions()
        marker = self.root / ".git"
        marker.write_text("gitdir: synthetic-linked-worktree\n", encoding="utf-8")
        self.assert_readers_reject("outside Git working trees")
        marker.unlink()
        marker = partition.path.parent.parent / ".git"
        marker.mkdir()
        (marker / "HEAD").write_text("ref: refs/heads/fixture\n", encoding="utf-8")
        self.assert_readers_reject("outside Git working trees")

    def test_projection_readers_accept_regular_atomic_replacements(self):
        wal = MemoryWal(self.first)
        self.project(wal)
        partition, = self.partitions()
        self.assertEqual(self.latest(self.first), self.first)

        wal.events.extend((self.second, self.correction))
        with TemporaryDirectory(dir=self.root) as staged:
            staged = Path(staged)
            with open_projection(staged, *self.product) as projection:
                projection.project_batch(wal, 1)
                projection.project_batch(wal, 20)
            for part in projected_partitions(staged, *self.product):
                os.replace(part.path, partition.path.with_name(part.path.name))
            staged_index = staged / self.marker.relative_to(self.root).with_name("projection.sqlite3")
            os.replace(staged_index, self.marker.with_name("projection.sqlite3"))

        self.assertEqual(self.latest(self.first), self.correction)
        self.assertEqual(len(self.partitions()), 2)

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

    def test_option_history_raw_quotes_and_trades_keep_evidence_and_index(self):
        path = _EXAMPLES.parents[1] / "v3" / "examples.json"
        examples = json.loads(path.read_text(encoding="utf-8"))
        quote, trade = [Event.from_dict(event) for event in examples["events"]]
        self.product = (trade.source, trade.product)
        self.assertEqual(self.project(MemoryWal(quote, trade)), 2)
        partition, = self.partitions()
        rows = pq.ParquetFile(partition.path).read().to_pylist()
        self.assertEqual([json.loads(row["event_json"]) for row in rows], examples["events"])
        self.assertEqual(self.latest(trade), trade)
        self.assertEqual(self.latest(quote), quote)

    def option_history_wal(self):
        path = _EXAMPLES.parents[1] / "v3" / "examples.json"
        examples = json.loads(path.read_text(encoding="utf-8"))
        wal = MemoryWal(*(Event.from_dict(event) for event in examples["events"]))
        trade = wal.events[-1]
        self.product = (trade.source, trade.product)
        self.marker = self.root.joinpath("finance-state", "market-projection", *self.product, "watermark.json")
        return wal

    def synthetic_worktree(self, path, *, linked=False):
        path.mkdir(parents=True, exist_ok=True)
        if linked:
            (path / ".git").write_text("gitdir: synthetic-checkout\n", encoding="utf-8")
        else:
            (path / ".git").mkdir()
            (path / ".git" / "HEAD").write_text("ref: refs/heads/synthetic\n", encoding="utf-8")

    def test_option_history_projection_rejects_link_to_worktree_at_source_directory(self):
        wal = self.option_history_wal()
        source, _ = self.product
        for components in (("catalog", "market-events"), ("finance-state", "market-projection")):
            with self.subTest(components=components):
                data_root = self.root / components[-1]
                checkout = self.root.joinpath("checkout", components[-1])
                self.synthetic_worktree(checkout)
                parent = data_root.joinpath(*components)
                parent.mkdir(parents=True)
                link = parent / source
                link.symlink_to(checkout, target_is_directory=True)
                with self.assertRaises(OSError), open_projection(data_root, *self.product) as projection:
                    projection.project_batch(wal, 20)
                self.assertEqual(list(checkout.iterdir()), [checkout / ".git"])
                self.assertFalse((data_root / self.marker.relative_to(self.root)).exists())

    def test_option_history_projection_rejects_nested_worktree_directories(self):
        wal = self.option_history_wal()
        for components, linked in (
            (("catalog", "market-events"), False),
            (("finance-state", "market-projection"), True),
        ):
            with self.subTest(components=components):
                data_root = self.root / components[-1]
                checkout = data_root.joinpath(*components, *self.product)
                self.synthetic_worktree(checkout, linked=linked)
                with (
                    self.assertRaisesRegex(ProjectionError, "outside Git worktrees"),
                    open_projection(data_root, *self.product) as projection,
                ):
                    projection.project_batch(wal, 20)
                self.assertEqual(list(checkout.iterdir()), [checkout / ".git"])

    def test_option_history_projection_rejects_linked_index_and_sidecars(self):
        wal = self.option_history_wal()
        checkout = self.root / "checkout"
        self.synthetic_worktree(checkout)
        target = checkout / "protected"
        target.write_bytes(b"fixed synthetic content")
        state = self.marker.parent
        state.mkdir(parents=True)
        for name in ("projection.sqlite3", "projection.sqlite3-journal", "projection.sqlite3-wal", "projection.sqlite3-shm"):
            with self.subTest(name=name):
                link = state / name
                link.symlink_to(target)
                with patch.object(market_projector.sqlite3, "connect") as connect:
                    with self.assertRaises((OSError, ProjectionError)):
                        self.project(wal)
                    connect.assert_not_called()
                self.assertEqual(target.read_bytes(), b"fixed synthetic content")
                self.assertTrue(link.is_symlink())
                link.unlink()

    def test_option_history_projection_rejects_state_replacement_before_transaction(self):
        wal = self.option_history_wal()
        state = self.marker.parent
        held = self.root / "held-state"
        checkout = self.root / "checkout"
        self.synthetic_worktree(checkout)
        read = wal.read_after

        def replace_state(*args):
            state.rename(held)
            state.symlink_to(checkout, target_is_directory=True)
            return read(*args)

        with open_projection(self.root, *self.product) as projection, patch.object(wal, "read_after", replace_state):
            with self.assertRaises((OSError, ProjectionError)):
                projection.project_batch(wal, 20)
        self.assertEqual(list(checkout.iterdir()), [checkout / ".git"])
        self.assertEqual(json.loads((held / "watermark.json").read_text(encoding="utf-8")), {
            "projected_through": "0",
        })
        state.unlink()
        held.rename(state)
        self.assertEqual(self.project(wal), 2)
        self.assertEqual(self.watermark(), "2")

    def test_option_history_projection_rechecks_sidecar_links_before_transaction(self):
        wal = self.option_history_wal()
        checkout = self.root / "checkout"
        self.synthetic_worktree(checkout)
        target = checkout / "protected"
        target.write_bytes(b"fixed synthetic content")
        journal = self.marker.parent / "projection.sqlite3-journal"
        read = wal.read_after

        def add_journal_link(*args):
            journal.symlink_to(target)
            return read(*args)

        with open_projection(self.root, *self.product) as projection, patch.object(wal, "read_after", add_journal_link):
            with self.assertRaisesRegex(ProjectionError, "invalid projection file"):
                projection.project_batch(wal, 20)
        self.assertEqual(target.read_bytes(), b"fixed synthetic content")
        self.assertTrue(journal.is_symlink())
        journal.unlink()
        self.assertEqual(self.watermark(), "0")
        self.assertEqual(self.partitions(), [])

    def test_option_history_projection_rechecks_catalog_before_parquet_publication(self):
        wal = self.option_history_wal()
        catalog = self.root.joinpath("catalog", "market-events", *self.product)
        held = self.root / "held-catalog"
        checkout = self.root / "checkout"
        self.synthetic_worktree(checkout)
        with open_projection(self.root, *self.product) as projection:
            catalog.rename(held)
            catalog.symlink_to(checkout, target_is_directory=True)
            with self.assertRaises((OSError, ProjectionError)):
                projection.project_batch(wal, 20)
        self.assertEqual(list(checkout.iterdir()), [checkout / ".git"])
        self.assertEqual(list(held.iterdir()), [])
        self.assertEqual(self.watermark(), "0")
        catalog.unlink()
        held.rename(catalog)
        self.assertEqual(self.project(wal, limit=1), 2)
        self.assertEqual(self.watermark(), "2")
        self.assertEqual(self.latest(wal.events[-1]), wal.events[-1])

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

    def test_retry_rejects_mismatched_partition_without_advancing_checkpoint(self):
        wal = MemoryWal(self.first, self.second, self.correction, self.gap)
        self.assertEqual(self.project(wal, 3), 3)
        committed = self.partitions()
        publish = market_projector._publish_parquet

        def fail_after_publish(path, rows):
            publish(path, rows)
            raise OSError("synthetic loss after publish")

        with patch.object(market_projector, "_publish_parquet", side_effect=fail_after_publish) as interrupted:
            with self.assertRaisesRegex(OSError, "synthetic loss after publish"):
                self.project(wal)
        path, _ = interrupted.call_args.args

        with pq.ParquetFile(path) as reader:
            published = reader.read()
        event_json = published.schema.field("event_json")
        different_event = pa.array([self.second.to_json()], type=event_json.type)
        mismatches = {
            "row content": published.set_column(
                published.schema.get_field_index("event_json"), event_json, different_event,
            ),
            "schema metadata": published.replace_schema_metadata({
                **published.schema.metadata, b"finance.market_projection.version": b"2",
            }),
        }

        for mismatch, table in mismatches.items():
            with self.subTest(mismatch=mismatch):
                pq.write_table(table, path)
                with self.assertRaisesRegex(ProjectionError, "existing partition differs from replay at WAL 4$"):
                    self.project(wal)
                self.assertEqual(self.watermark(), "3")
                self.assertEqual(self.partitions(), committed)
                with open_projection(self.root, *self.product) as projection:
                    checkpoint = projection.db.execute(
                        "SELECT projected_through, pending_through FROM checkpoint WHERE singleton=1",
                    ).fetchone()
                self.assertEqual(checkpoint, ("3", "4"))

        pq.write_table(published, path)
        self.assertEqual(self.project(wal), 1)
        self.assertEqual(self.watermark(), "4")
        self.assertEqual([partition.path for partition in self.partitions()], [committed[0].path, path])

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
