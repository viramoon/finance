"""Evaluate an ordered stream of closed bars and persist decisions locally."""

import argparse
from collections.abc import Callable
from contextlib import closing
from dataclasses import dataclass
from hashlib import sha256
import json
from pathlib import Path
import os
import signal
import sqlite3
import stat
import sys
import time

from .market_protocol import AssetKind, BarClosed, Event, Kind, ProtocolError
from .strategy_evaluation import ClosedBar, SmaCrossoverPlan, _unique_object, open_evaluator
from .strategy_plan import BUNDLE_PLAN_VERSION, checked_bundle_plan


STATE_DIRECTORY = Path("finance-state/decisions")
DATABASE_NAME = "decisions.sqlite3"
_MAX_FRAME_BYTES = 1024 * 1024
_POLL_SECONDS = 0.1
_INTERRUPTIONS = {Kind.GAP, Kind.RESET, Kind.CORRECTION}


class DecisionError(ValueError):
    """The decision stream could not be consumed safely."""


@dataclass(frozen=True, slots=True)
class _Checkpoint:
    offset: int
    prefix_sha256: str


def _open_store(connection: sqlite3.Connection, replay) -> _Checkpoint:
    connection.execute("""
        CREATE TABLE IF NOT EXISTS decision_checkpoint (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            plan_sha256 TEXT NOT NULL,
            replay_offset INTEGER NOT NULL CHECK (replay_offset >= 0),
            replay_sha256 TEXT NOT NULL
        );
        """)
    with connection:
        connection.execute(
            "INSERT OR IGNORE INTO decision_checkpoint VALUES (1, ?, 0, ?)",
            (replay.checkpoint_identity, sha256().hexdigest()),
        )
        plan_hash, offset, digest = connection.execute(
            "SELECT plan_sha256, replay_offset, replay_sha256 FROM decision_checkpoint WHERE id = 1"
        ).fetchone()
    if plan_hash != replay.checkpoint_identity:
        raise DecisionError("decision store belongs to a different strategy plan or indicator evaluator")
    replay.initialize(connection)
    return _Checkpoint(offset, digest)


def _commit(
    connection: sqlite3.Connection,
    replay,
    prior: _Checkpoint,
    checkpoint: _Checkpoint,
    frame,
    decision,
) -> None:
    with connection:
        updated = connection.execute(
            """UPDATE decision_checkpoint SET replay_offset = ?, replay_sha256 = ?
               WHERE id = 1 AND replay_offset = ? AND replay_sha256 = ?""",
            (checkpoint.offset, checkpoint.prefix_sha256, prior.offset, prior.prefix_sha256),
        )
        if updated.rowcount != 1:
            raise DecisionError("decision checkpoint moved during evaluation; another process owns the stream")
        replay.record(connection, checkpoint.offset, frame, decision)


def _replay_error_path(error: ProtocolError) -> str:
    if error.kind == "unknown_field":
        # Schema validation appends one escaped input key to a fixed object location.
        return f"{error.path.rpartition('/')[0]}/<field>"
    if error.kind == "duplicate_field":
        # Duplicate detection precedes schema validation, so any ancestor can be an input key.
        for prefix in ("/payload/replacement/payload", "/payload/replacement", "/payload"):
            if error.path.startswith(prefix + "/"):
                return f"{prefix}/<field>"
        return "/<field>"
    return error.path or "/"


def _decode(raw: bytes, offset: int, previous: Event | None, plan: SmaCrossoverPlan) -> Event:
    try:
        event = Event.from_json(raw)
        event.validate_successor(previous)
    except ProtocolError as error:
        raise DecisionError(
            f"invalid replay frame at byte {offset} ({error.kind}, {_replay_error_path(error)})"
        ) from None
    if event.instrument_id != plan.instrument_id:
        raise DecisionError(f"instrument differs from the strategy plan at byte {offset}")
    if event.asset_kind not in (AssetKind.STOCK, AssetKind.ETF):
        raise DecisionError(f"strategy requires a stock or ETF at byte {offset}")
    if event.kind in _INTERRUPTIONS:
        raise DecisionError(f"{event.kind} at byte {offset} requires a reviewed replay baseline")
    return event


def _check_replay(path: Path, opened: os.stat_result, offset: int) -> None:
    current = path.stat()
    if not os.path.samestat(opened, current) or current.st_size < offset:
        raise DecisionError("replay was replaced or truncated")


class _SmaReplay:
    def __init__(self, plan):
        self.plan = plan
        self.plan_sha256 = plan.content_sha256
        self.checkpoint_identity = self.plan_sha256
        self.evaluator = open_evaluator(plan)
        self.previous = None
        self.decision_ts = 0

    def close(self):
        """The in-process evaluator holds no external resources."""

    def initialize(self, connection):
        connection.execute("""
            CREATE TABLE IF NOT EXISTS decisions (
                replay_offset INTEGER PRIMARY KEY,
                plan_sha256 TEXT NOT NULL,
                source TEXT NOT NULL,
                product TEXT NOT NULL,
                channel TEXT NOT NULL,
                instrument_id TEXT NOT NULL,
                epoch TEXT NOT NULL,
                seq TEXT NOT NULL,
                cursor TEXT NOT NULL,
                license TEXT NOT NULL,
                exchange_ts TEXT NOT NULL,
                first_available_ts TEXT NOT NULL,
                decision_ts TEXT NOT NULL,
                target_shares TEXT NOT NULL,
                UNIQUE (epoch, seq)
            )
        """)

    def decode(self, raw, offset):
        event = _decode(raw, offset, self.previous, self.plan)
        self.previous = event
        if isinstance(event.payload, BarClosed):
            self.decision_ts = max(self.decision_ts, event.exchange_ts.integer, event.first_available_ts.integer)
            return event, self.decision_ts
        return event, None

    def evaluate(self, event, _offset):
        if isinstance(event.payload, BarClosed):
            return self.evaluator.evaluate(
                ClosedBar(event.instrument_id, event.exchange_ts.integer, event.payload.close.decimal)
            )
        return None

    def record(self, connection, offset, event, decision):
        if decision is not None:
            connection.execute(
                """INSERT INTO decisions (
                    replay_offset, plan_sha256, source, product, channel, instrument_id,
                    epoch, seq, cursor, license, exchange_ts, first_available_ts, decision_ts, target_shares
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)""",
                (offset, self.plan_sha256, *event.stream_key, event.epoch, event.seq,
                 event.cursor, event.license, str(decision.ts_event), event.first_available_ts,
                 str(self.decision_ts), format(decision.target_shares, "f")),
            )


def _load_plan(raw):
    try:
        plan = json.loads(raw, object_pairs_hook=_unique_object)
    except (UnicodeError, ValueError, RecursionError):
        raise DecisionError("Invalid compiled strategy plan JSON") from None
    if isinstance(plan, dict) and plan.get("plan_version") == BUNDLE_PLAN_VERSION:
        return plan
    return SmaCrossoverPlan.from_json(raw)


def _replay_adapter(plan, *, stopped=None):
    if isinstance(plan, SmaCrossoverPlan):
        return _SmaReplay(plan)
    plan = checked_bundle_plan(plan)
    try:
        from .decision_bundle import BundleDecisionReplay
    except ImportError:
        raise DecisionError("StrategyBundle replay requires the source-built Nautilus Python environment") from None
    return BundleDecisionReplay(plan, stopped=stopped)


def _consume(
    plan: SmaCrossoverPlan | dict,
    replay_path: Path,
    database: Path,
    *,
    follow: bool,
    stopped: Callable[[], bool],
) -> None:
    # Read-only, nonblocking open also lets us reject non-regular inputs such as FIFOs.
    with (
        closing(_replay_adapter(plan, stopped=stopped)) as adapter,
        os.fdopen(os.open(replay_path, os.O_RDONLY | os.O_NONBLOCK), "rb") as replay,
    ):
        opened = os.fstat(replay.fileno())
        if not stat.S_ISREG(opened.st_mode):
            raise DecisionError("replay must be a regular file")
        database.parent.parent.mkdir(exist_ok=True)
        database.parent.mkdir(exist_ok=True)
        with closing(sqlite3.connect(database)) as connection:
            checkpoint = _open_store(connection, adapter)
            digest = sha256()
            while not stopped():
                start = replay.tell()
                _check_replay(replay_path, opened, start)
                raw = replay.readline(_MAX_FRAME_BYTES + 1)
                if len(raw) > _MAX_FRAME_BYTES:
                    raise DecisionError(f"replay frame exceeds {_MAX_FRAME_BYTES} bytes at byte {start}")
                if not raw.endswith(b"\n"):
                    if start < checkpoint.offset:
                        raise DecisionError("replay ended before the committed checkpoint")
                    if not follow:
                        if raw:
                            raise DecisionError(f"unterminated replay frame at byte {start}")
                        return
                    replay.seek(start)
                    time.sleep(_POLL_SECONDS)
                    continue

                digest.update(raw)
                consumed = _Checkpoint(replay.tell(), digest.hexdigest())
                if start < checkpoint.offset < consumed.offset or (
                    consumed.offset == checkpoint.offset and consumed != checkpoint
                ):
                    raise DecisionError("replay prefix differs from the committed checkpoint")
                frame, available = adapter.decode(raw, start)
                if available is not None and follow and consumed.offset > checkpoint.offset:
                    while not stopped() and available > time.time_ns():
                        _check_replay(replay_path, opened, consumed.offset)
                        time.sleep(_POLL_SECONDS)
                    if stopped():
                        return
                    _check_replay(replay_path, opened, consumed.offset)
                try:
                    decision = adapter.evaluate(frame, start)
                except InterruptedError:
                    if stopped():
                        return
                    raise
                if stopped():
                    return
                if consumed.offset > checkpoint.offset:
                    _commit(connection, adapter, checkpoint, consumed, frame, decision)
                    checkpoint = consumed


def main() -> int:
    parser = argparse.ArgumentParser(prog="finance-decision", description=__doc__)
    parser.add_argument("--plan", type=Path, required=True, help="compiled strategy plan JSON")
    parser.add_argument("--replay", type=Path, required=True, help="append-only F1 events or StrategyBundle sessions")
    parser.add_argument("--follow", action="store_true", help="wait for complete appended frames")
    args = parser.parse_args()

    stopping = False

    def request_stop(_signum, _frame):
        nonlocal stopping
        stopping = True

    signal.signal(signal.SIGTERM, request_stop)
    signal.signal(signal.SIGINT, request_stop)

    try:
        configured_root = Path(os.environ.get("FERTILE_DATA_ROOT", ""))
        if not configured_root.is_absolute():
            raise DecisionError("FERTILE_DATA_ROOT must be an absolute path to an existing directory")
        root = configured_root.resolve(strict=True)
        if not root.is_dir():
            raise DecisionError("FERTILE_DATA_ROOT must be a directory")
        state_directory = root / STATE_DIRECTORY
        resolved_state_directory = state_directory.resolve()
        if not resolved_state_directory.is_relative_to(root):
            raise DecisionError("decision state directory must stay under FERTILE_DATA_ROOT")
        database = state_directory / DATABASE_NAME
        if database.is_symlink():
            raise DecisionError("decision database must not be a symlink")
        for input_path in (args.plan, args.replay):
            if input_path.resolve(strict=True).is_relative_to(resolved_state_directory) or (
                database.exists() and input_path.samefile(database)
            ):
                raise DecisionError("inputs must be outside the decision state directory")
        plan = _load_plan(args.plan.read_bytes())
        _consume(plan, args.replay, database, follow=args.follow, stopped=lambda: stopping)
    except (ValueError, sqlite3.Error, OSError) as error:
        print(f"finance-decision: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
