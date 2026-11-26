"""Project committed F1 WAL records into immutable, indexed Parquet ranges."""

from __future__ import annotations

import argparse
from contextlib import closing, contextmanager
from dataclasses import dataclass
import fcntl
from hashlib import sha256
import json
import math
import os
from pathlib import Path
import re
import signal
import sqlite3
import subprocess
import sys
from threading import Event as StopEvent
from typing import Protocol

import pyarrow as pa
import pyarrow.parquet as pq

from fertile_finance.collector_store import atomic_json, atomic_publish, ensure_dir, fsync_dir
from fertile_finance.market_protocol import Correction, Event, Kind, U64Text


_TOKEN = re.compile(r"[a-z0-9][a-z0-9._-]{0,63}")
_MAX_BATCH = 4096
_INDEX_NAME = "projection.sqlite3"
_MARKET_KINDS = frozenset((Kind.BAR_CLOSED, Kind.TRADE, Kind.SNAPSHOT, Kind.DELTA))

_PARQUET_SCHEMA = pa.schema([
    ("wal_position", pa.uint64()),
    ("source", pa.string()),
    ("product", pa.string()),
    ("channel", pa.string()),
    ("instrument_id", pa.string()),
    ("kind", pa.string()),
    ("epoch", pa.uint64()),
    ("seq", pa.uint64()),
    ("revision", pa.uint64()),
    ("exchange_ts", pa.uint64()),
    ("receive_ts", pa.uint64()),
    ("first_available_ts", pa.uint64()),
    ("target_epoch", pa.uint64()),
    ("target_seq", pa.uint64()),
    ("event_json", pa.large_string()),
], metadata={b"finance.market_projection.version": b"1"})

# WAL positions use zero-padded decimal text so ordering remains exact above
# SQLite's signed integer limit. F1 epochs, sequences and revisions retain text.
_INITIALIZE = """
BEGIN;
CREATE TABLE checkpoint (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    projected_through TEXT NOT NULL,
    pending_through TEXT
);
INSERT INTO checkpoint VALUES (1, '0', NULL);
CREATE TABLE seen (
    channel TEXT NOT NULL,
    instrument_id TEXT NOT NULL,
    epoch TEXT NOT NULL,
    seq TEXT NOT NULL,
    fingerprint BLOB NOT NULL,
    PRIMARY KEY (channel, instrument_id, epoch, seq)
) WITHOUT ROWID;
CREATE TABLE latest (
    channel TEXT NOT NULL,
    instrument_id TEXT NOT NULL,
    epoch TEXT NOT NULL,
    seq TEXT NOT NULL,
    kind TEXT NOT NULL,
    revision TEXT NOT NULL,
    position TEXT NOT NULL,
    PRIMARY KEY (channel, instrument_id, epoch, seq)
) WITHOUT ROWID;
CREATE TABLE partitions (
    first_position TEXT PRIMARY KEY NOT NULL,
    last_position TEXT NOT NULL,
    row_count INTEGER NOT NULL
) WITHOUT ROWID;
PRAGMA user_version = 1;
COMMIT;
"""


class ProjectionError(ValueError):
    """A committed input or projection artifact failed a consistency check."""


@dataclass(frozen=True, slots=True)
class WalRecord:
    position: int
    event: Event


class WalPageReader(Protocol):
    def read_after(self, source: str, product: str, after: int, limit: int) -> list[WalRecord]: ...


@dataclass(frozen=True, slots=True)
class ProjectedPartition:
    path: Path
    first_position: int
    last_position: int
    row_count: int


def _product_paths(data_root: Path, source: str, product: str) -> tuple[Path, Path]:
    if not all(isinstance(token, str) and _TOKEN.fullmatch(token) for token in (source, product)):
        raise ProjectionError("invalid source or product")
    return (
        data_root / "finance-state" / "market-projection" / source / product,
        data_root / "catalog" / "market-events" / source / product,
    )


def _partition_path(catalog: Path, first: int, last: int) -> Path:
    return catalog / f"positions-{first:020d}-{last:020d}.parquet"


class WalCommand:
    """Read the committed product prefix through the Rust WAL reader."""

    def __init__(self, data_root: Path, executable: str):
        self.command = [executable, "--data-root", str(data_root)]

    def _call(self, command: str, *arguments: str) -> str:
        result = subprocess.run(
            [*self.command, command, *arguments], check=True, encoding="utf-8",
            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        )
        return result.stdout

    def products(self) -> list[tuple[str, str]]:
        return [(item["source"], item["product"]) for item in json.loads(self._call("products"))]

    def read_after(self, source: str, product: str, after: int, limit: int) -> list[WalRecord]:
        page = self._call(
            "read", "--source", source, "--product", product,
            "--after", str(after), "--limit", str(limit),
        )
        records = []
        # JSON strings may contain Unicode line separators; only ASCII LF
        # delimits frames in the helper's stdout.
        for line in page.split("\n"):
            if line:
                row = json.loads(line)
                records.append(WalRecord(U64Text(row["position"]).integer, Event.from_dict(row["event"])))
        return records


def _fingerprint(event: Event) -> bytes:
    wire = event.to_dict()
    # A transport retry can update the receipt clock. Preserve the first
    # receipt in Parquet and require the other fields to agree.
    del wire["receive_ts"]
    canonical = json.dumps(wire, ensure_ascii=False, separators=(",", ":"), sort_keys=True)
    return sha256(canonical.encode("utf-8")).digest()


def _parquet_row(record: WalRecord) -> dict:
    event = record.event
    correction = event.payload if isinstance(event.payload, Correction) else None
    return {
        "wal_position": record.position,
        "source": event.source,
        "product": event.product,
        "channel": event.channel,
        "instrument_id": event.instrument_id,
        "kind": str(event.kind),
        "epoch": event.epoch.integer,
        "seq": event.seq.integer,
        "revision": event.revision.integer,
        "exchange_ts": event.exchange_ts.integer if event.exchange_ts is not None else None,
        "receive_ts": event.receive_ts.integer,
        "first_available_ts": event.first_available_ts.integer,
        "target_epoch": correction.target_epoch.integer if correction else None,
        "target_seq": correction.target_seq.integer if correction else None,
        "event_json": event.to_json(),
    }


def _publish_parquet(dest: Path, rows: list[dict]) -> None:
    table = pa.Table.from_pylist(rows, schema=_PARQUET_SCHEMA)
    if dest.exists():
        # Recovery after rename and before SQLite commit. The retry checks
        # the exact intended rows and never overwrites a published partition.
        if not pq.ParquetFile(dest).read().equals(table, check_metadata=True):
            raise ProjectionError(f"existing partition differs from replay at WAL {rows[0]['wal_position']}")
        with dest.open("rb") as existing:
            os.fsync(existing.fileno())
        fsync_dir(dest.parent)
        return
    temp = dest.with_name(f".{dest.name}.tmp")
    try:
        pq.write_table(table, temp, compression="zstd")
        atomic_publish(temp, dest)
    finally:
        temp.unlink(missing_ok=True)


class ProductProjection:
    def __init__(self, db: sqlite3.Connection, state: Path, catalog: Path, source: str, product: str):
        self.db = db
        self.state = state
        self.catalog = catalog
        self.product_key = (source, product)

    def _positions(self) -> tuple[int, int | None]:
        through, pending = self.db.execute(
            "SELECT projected_through, pending_through FROM checkpoint WHERE singleton=1"
        ).fetchone()
        return U64Text(through).integer, U64Text(pending).integer if pending is not None else None

    def publish_watermark(self) -> None:
        through, _ = self._positions()
        watermarks = {"projected_through": str(through)}
        path = self.state / "watermark.json"
        try:
            if json.loads(path.read_text(encoding="utf-8")) == watermarks:
                return
        except (FileNotFoundError, ValueError):
            pass
        atomic_json(path, watermarks)

    def _index_record(self, record: WalRecord) -> bool:
        event = record.event
        stream = (event.channel, event.instrument_id)
        key = (*stream, str(event.epoch), str(event.seq))
        digest = _fingerprint(event)
        previous = self.db.execute(
            "SELECT fingerprint FROM seen WHERE channel=? AND instrument_id=? AND epoch=? AND seq=?", key,
        ).fetchone()
        if previous is not None:
            if previous[0] != digest:
                raise ProjectionError(f"conflicting duplicate at WAL {record.position}")
            return False
        self.db.execute("INSERT INTO seen VALUES (?, ?, ?, ?, ?)", (*key, digest))
        if event.kind in _MARKET_KINDS:
            self.db.execute(
                "INSERT INTO latest VALUES (?, ?, ?, ?, ?, ?, ?)",
                (*key, str(event.kind), "0", f"{record.position:020d}"),
            )
        if isinstance(event.payload, Correction):
            self._index_correction(record, stream)
        return True

    def _index_correction(self, record: WalRecord, stream: tuple[str, str]) -> None:
        event = record.event
        correction = event.payload
        assert isinstance(correction, Correction)
        target = (*stream, str(correction.target_epoch), str(correction.target_seq))
        original = self.db.execute(
            "SELECT kind, revision FROM latest WHERE channel=? AND instrument_id=? AND epoch=? AND seq=?",
            target,
        ).fetchone()
        if original is None:
            raise ProjectionError(f"missing original for correction at WAL {record.position}")
        kind, revision = original
        if correction.replacement.kind != kind or event.revision.integer <= int(revision):
            raise ProjectionError(f"invalid correction kind or revision at WAL {record.position}")
        self.db.execute(
            "UPDATE latest SET revision=?, position=? WHERE channel=? AND instrument_id=? AND epoch=? AND seq=?",
            (str(event.revision), f"{record.position:020d}", *target),
        )

    def project_batch(self, reader: WalPageReader, limit: int) -> int:
        if not 1 <= limit <= _MAX_BATCH:
            raise ProjectionError(f"batch size must be in 1..{_MAX_BATCH}")
        through, pending = self._positions()
        count = limit if pending is None else pending - through
        if not 1 <= count <= _MAX_BATCH:
            raise ProjectionError("invalid pending projection range")
        records = reader.read_after(*self.product_key, through, count)
        if len(records) > count or any(
            record.position != position or (record.event.source, record.event.product) != self.product_key
            for position, record in enumerate(records, through + 1)
        ):
            raise ProjectionError("WAL page has a missing position or incorrect product")
        last = through + len(records)
        if pending is not None and last != pending:
            raise ProjectionError("WAL prefix ends inside a pending projection range")
        if not records:
            self.publish_watermark()
            return 0

        if pending is None:
            # Pin this exact range before publishing so a growing WAL or a
            # changed page size cannot create overlapping files after restart.
            with self.db:
                self.db.execute("UPDATE checkpoint SET pending_through=? WHERE singleton=1", (str(last),))
        first = through + 1
        with self.db:
            rows = [_parquet_row(record) for record in records if self._index_record(record)]
            if rows:
                _publish_parquet(_partition_path(self.catalog, first, last), rows)
                self.db.execute(
                    "INSERT INTO partitions VALUES (?, ?, ?)", (f"{first:020d}", f"{last:020d}", len(rows)),
                )
            self.db.execute(
                "UPDATE checkpoint SET projected_through=?, pending_through=NULL WHERE singleton=1",
                (str(last),),
            )
        self.publish_watermark()
        return len(records)


@contextmanager
def open_projection(data_root: Path, source: str, product: str):
    state, catalog = _product_paths(data_root, source, product)
    ensure_dir(state)
    ensure_dir(catalog)
    with (state / "writer.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        with closing(sqlite3.connect(state / _INDEX_NAME)) as db:
            db.execute("PRAGMA synchronous=EXTRA")
            version = db.execute("PRAGMA user_version").fetchone()[0]
            if version == 0:
                if next(catalog.glob("*.parquet"), None) is not None:
                    raise ProjectionError("Parquet exists without its projection index; rebuild both together")
                db.executescript(_INITIALIZE)
                fsync_dir(state)
            elif version != 1:
                raise ProjectionError("unsupported projection index version")
            projection = ProductProjection(db, state, catalog, source, product)
            projection.publish_watermark()
            yield projection


@contextmanager
def _read_index(path: Path):
    with closing(sqlite3.connect(path.absolute().as_uri() + "?mode=ro", uri=True)) as db:
        if db.execute("PRAGMA user_version").fetchone()[0] != 1:
            raise ProjectionError("unsupported projection index version")
        yield db


def projected_partitions(data_root: Path, source: str, product: str) -> list[ProjectedPartition]:
    """List the committed Parquet files in ascending local WAL order."""
    state, catalog = _product_paths(data_root, source, product)
    if not (state / _INDEX_NAME).exists():
        return []
    with _read_index(state / _INDEX_NAME) as db:
        partitions = []
        for first, last, count in db.execute(
            "SELECT first_position, last_position, row_count FROM partitions ORDER BY first_position"
        ):
            path = _partition_path(catalog, int(first), int(last))
            if not path.is_file():
                raise ProjectionError(f"committed projection file missing for WAL range {first}..{last}")
            partitions.append(ProjectedPartition(path, int(first), int(last), count))
        return partitions


def read_latest_event(
    data_root: Path, source: str, product: str, channel: str, instrument_id: str, epoch: str, seq: str,
) -> Event | None:
    """Read the original event or latest correction for a projected F1 position."""
    state, catalog = _product_paths(data_root, source, product)
    if not (state / _INDEX_NAME).exists():
        return None
    with _read_index(state / _INDEX_NAME) as db:
        row = db.execute(
            "SELECT position FROM latest WHERE channel=? AND instrument_id=? AND epoch=? AND seq=?",
            (channel, instrument_id, str(U64Text(epoch)), str(U64Text(seq))),
        ).fetchone()
        if row is None:
            return None
        (position,) = row
        bounds = db.execute(
            """SELECT first_position, last_position FROM partitions
               WHERE first_position<=? ORDER BY first_position DESC LIMIT 1""",
            (position,),
        ).fetchone()
        if bounds is None or bounds[1] < position:
            raise ProjectionError(f"projection index has no file for WAL position {position}")
        parquet = _partition_path(catalog, int(bounds[0]), int(bounds[1]))
        table = pq.read_table(
            parquet, columns=["event_json"],
            filters=[("wal_position", "=", pa.scalar(int(position), pa.uint64()))],
        )
        if table.num_rows != 1:
            raise ProjectionError(f"projection file has no unique WAL position {position}")
        return Event.from_json(table["event_json"][0].as_py())


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--data-root", required=True, type=Path)
    parser.add_argument("--wal-bin", default="finance-market-wal")
    parser.add_argument("--batch-size", default=512, type=int)
    parser.add_argument("--poll-seconds", default=1.0, type=float)
    parser.add_argument("--once", action="store_true", help="exit after catching up to the WAL")
    args = parser.parse_args()
    if not 1 <= args.batch_size <= _MAX_BATCH:
        parser.error(f"--batch-size must be in 1..{_MAX_BATCH}")
    if not math.isfinite(args.poll_seconds) or args.poll_seconds <= 0:
        parser.error("--poll-seconds must be positive")

    stop = StopEvent()

    def request_stop(_signum, _frame):
        stop.set()

    for signum in (signal.SIGINT, signal.SIGTERM):
        signal.signal(signum, request_stop)
    data_root = args.data_root.resolve()
    wal = WalCommand(data_root, args.wal_bin)
    try:
        while not stop.is_set():
            processed = 0
            for source, product in wal.products():
                if stop.is_set():
                    break
                with open_projection(data_root, source, product) as projection:
                    processed += projection.project_batch(wal, args.batch_size)
            if not processed:
                if args.once:
                    break
                stop.wait(args.poll_seconds)
    except subprocess.CalledProcessError as exc:
        print(f"finance-projector: WAL reader exited with status {exc.returncode}", file=sys.stderr)
        return 1
    except (OSError, ValueError, sqlite3.Error) as exc:
        print(f"finance-projector: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
