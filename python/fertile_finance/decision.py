"""Evaluate an ordered stream of closed bars and persist decisions locally."""

import argparse
from collections.abc import Callable
from contextlib import closing
from dataclasses import dataclass
from hashlib import sha256
from pathlib import Path
import os
import signal
import sqlite3
import stat
import sys
import time

from .market_protocol import AssetKind, BarClosed, Event, Kind, ProtocolError
from .strategy_evaluation import ClosedBar, Decision, SmaCrossoverPlan, open_evaluator


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


def _open_store(connection: sqlite3.Connection, plan: SmaCrossoverPlan) -> _Checkpoint:
    connection.executescript("""
        CREATE TABLE IF NOT EXISTS decision_checkpoint (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            plan_sha256 TEXT NOT NULL,
            replay_offset INTEGER NOT NULL CHECK (replay_offset >= 0),
            replay_sha256 TEXT NOT NULL
        );
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
        );
        """)
    with connection:
        connection.execute(
            "INSERT OR IGNORE INTO decision_checkpoint VALUES (1, ?, 0, ?)",
            (plan.content_sha256, sha256().hexdigest()),
        )
        plan_hash, offset, digest = connection.execute(
            "SELECT plan_sha256, replay_offset, replay_sha256 FROM decision_checkpoint WHERE id = 1"
        ).fetchone()
    if plan_hash != plan.content_sha256:
        raise DecisionError("decision store belongs to a different strategy plan")
    return _Checkpoint(offset, digest)


def _commit(
    connection: sqlite3.Connection,
    plan: SmaCrossoverPlan,
    prior: _Checkpoint,
    checkpoint: _Checkpoint,
    event: Event,
    decision: Decision | None,
    decision_ts: int,
) -> None:
    with connection:
        updated = connection.execute(
            """UPDATE decision_checkpoint SET replay_offset = ?, replay_sha256 = ?
               WHERE id = 1 AND replay_offset = ? AND replay_sha256 = ?""",
            (checkpoint.offset, checkpoint.prefix_sha256, prior.offset, prior.prefix_sha256),
        )
        if updated.rowcount != 1:
            raise DecisionError("decision checkpoint moved during evaluation; another process owns the stream")
        if decision is not None:
            connection.execute(
                """INSERT INTO decisions (
                    replay_offset, plan_sha256, source, product, channel, instrument_id,
                    epoch, seq, cursor, license, exchange_ts, first_available_ts, decision_ts, target_shares
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)""",
                (checkpoint.offset, plan.content_sha256, *event.stream_key, event.epoch, event.seq,
                 event.cursor, event.license, str(decision.ts_event), event.first_available_ts, str(decision_ts),
                 format(decision.target_shares, "f")),
            )


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


def _consume(
    plan: SmaCrossoverPlan,
    replay_path: Path,
    database: Path,
    *,
    follow: bool,
    stopped: Callable[[], bool],
) -> None:
    evaluator = open_evaluator(plan)
    # Read-only, nonblocking open also lets us reject non-regular inputs such as FIFOs.
    with os.fdopen(os.open(replay_path, os.O_RDONLY | os.O_NONBLOCK), "rb") as replay:
        opened = os.fstat(replay.fileno())
        if not stat.S_ISREG(opened.st_mode):
            raise DecisionError("replay must be a regular file")
        database.parent.parent.mkdir(exist_ok=True)
        database.parent.mkdir(exist_ok=True)
        with closing(sqlite3.connect(database)) as connection:
            checkpoint = _open_store(connection, plan)
            digest = sha256()
            previous = None
            decision_ts = 0
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
                event = _decode(raw, start, previous, plan)
                previous = event
                decision = None
                if isinstance(event.payload, BarClosed):
                    close_ts = event.exchange_ts.integer
                    decision_ts = max(decision_ts, close_ts, event.first_available_ts.integer)
                    if follow and consumed.offset > checkpoint.offset:
                        while not stopped() and decision_ts > time.time_ns():
                            _check_replay(replay_path, opened, consumed.offset)
                            time.sleep(_POLL_SECONDS)
                        if stopped():
                            return
                        _check_replay(replay_path, opened, consumed.offset)
                    decision = evaluator.evaluate(
                        ClosedBar(event.instrument_id, close_ts, event.payload.close.decimal)
                    )
                if consumed.offset > checkpoint.offset:
                    _commit(connection, plan, checkpoint, consumed, event, decision, decision_ts)
                    checkpoint = consumed


def main() -> int:
    parser = argparse.ArgumentParser(prog="finance-decision", description=__doc__)
    parser.add_argument("--plan", type=Path, required=True, help="compiled strategy plan JSON")
    parser.add_argument("--replay", type=Path, required=True, help="append-only F1 event NDJSON")
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
        plan = SmaCrossoverPlan.from_json(args.plan.read_bytes())
        _consume(plan, args.replay, database, follow=args.follow, stopped=lambda: stopping)
    except (ValueError, sqlite3.Error, OSError) as error:
        print(f"finance-decision: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
