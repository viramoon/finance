"""Single-writer bridge to the pinned, source-built Nautilus ParquetDataCatalog."""

import fcntl
import json
import os
from pathlib import Path
import re
import shutil
import sys
import tempfile
from uuid import uuid4
from collections import defaultdict, deque
from contextlib import contextmanager
from datetime import date
from decimal import Decimal

from nautilus_trader.model.data import Bar, BarType
from nautilus_trader.model.objects import FIXED_PRECISION, Price, Quantity
from nautilus_trader.persistence.catalog.parquet import ParquetDataCatalog

if not __package__:
    # The Rust collector launches this file by path from the repository root.
    sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from fertile_finance.epoch import EPOCH, unix_nanos
from fertile_finance.history_paths import history_path


NS_PER_DAY = 86_400_000_000_000
DECIMAL = re.compile(r"-?[0-9]+(?:\.[0-9]+)?")
INSTRUMENT = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]*\.[A-Za-z0-9_]+")


def fsync_dir(path):
    fd = os.open(path, os.O_DIRECTORY | os.O_RDONLY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def ensure_dir(path):
    if not path.is_dir():
        ensure_dir(path.parent)
        path.mkdir(exist_ok=True)
        fsync_dir(path.parent)


def atomic_json(target, value):
    state = target.parent
    with tempfile.NamedTemporaryFile("w", dir=state, prefix=".watermarks-", delete=False) as output:
        temp = Path(output.name)
        try:
            json.dump(value, output, sort_keys=True)
            output.write("\n")
            output.flush()
            os.fsync(output.fileno())
            os.replace(temp, target)
            fsync_dir(state)
        finally:
            temp.unlink(missing_ok=True)


def checkpoint(state, marks):
    atomic_json(state / "watermarks.json", marks)


def check_provenance(catalog, state, source, spec, source_metadata=None):
    marker = catalog / ".finance-collector/source.json"
    binding = state / "catalog.json"
    if marker.exists():
        metadata = json.loads(marker.read_text())
        if (metadata["source"], metadata["spec"]) != (source, spec):
            raise ValueError("catalog belongs to another source or bar spec; select a separate data root")
        if metadata.get("source_metadata") != source_metadata:
            raise ValueError("catalog uses different source metadata; select a separate data root")
    else:
        if binding.exists() or any((catalog / "data/bar").glob("*/*.parquet")):
            raise ValueError("existing catalog or state without matching provenance")
        metadata = {"source": source, "spec": spec, "id": str(uuid4())}
        if source_metadata is not None:
            metadata["source_metadata"] = source_metadata
        atomic_json(marker, metadata)
    if binding.exists():
        if json.loads(binding.read_text()) != metadata:
            raise ValueError("collector state belongs to another catalog")
    else:
        atomic_json(binding, metadata)


def fixed(value, cls):
    if not isinstance(value, str) or not DECIMAL.fullmatch(value):
        raise ValueError(f"{cls.__name__} must be decimal text")
    parsed = cls.from_str(value)
    if parsed.as_decimal() != Decimal(value):
        raise ValueError(f"{cls.__name__} precision exceeds source-built Nautilus precision")
    # Nautilus encodes a shared display precision in Parquet schema metadata.
    # Use the build's precision even across files with different input scales.
    return cls.from_raw(parsed.raw, FIXED_PRECISION)


class Store:
    def __init__(self, catalog, state, staging, spec, *, historical):
        self.root = catalog
        self.state = state
        self.staging = staging
        self.writer = ParquetDataCatalog(staging)
        self.spec = spec
        BarType.from_str(f"SPY.XNYS-{spec}-EXTERNAL")
        self.marks = {}
        self.reader = ParquetDataCatalog(catalog)
        layout = catalog / "data/bar"
        suffix = f"-{spec}-EXTERNAL"
        for directory in layout.glob(f"*{suffix}"):
            if historical:
                history_path(layout, directory.name)
            if directory.is_dir():
                last = self.reader.query_last_timestamp(Bar, identifier=directory.name)
                if last is not None:
                    self.marks[directory.name.removesuffix(suffix)] = last.value
        checkpoint(state, self.marks)

    def _groups(self, batch, *, append_only):
        if not isinstance(batch, list):
            raise ValueError("expected a list of bars")
        groups = defaultdict(dict)
        for row in batch:
            instrument = row["instrument"]
            if not INSTRUMENT.fullmatch(instrument):
                raise ValueError("expected a path-safe Nautilus instrument ID such as SPY.XNYS")
            ts = unix_nanos(row["ts_ns"])
            if append_only and ts <= self.marks.get(instrument, -1):
                continue  # producer supplies ordered closed bars after replay/backfill
            kind = BarType.from_str(f"{instrument}-{self.spec}-EXTERNAL")
            bar = Bar(kind, *(fixed(row[k], Price) for k in ("open", "high", "low", "close")),
                      fixed(row["volume"], Quantity), ts, ts)
            group = groups[instrument, ts // NS_PER_DAY]
            if ts in group and group[ts] != bar:
                raise ValueError("conflicting closed bars for the same instrument and time")
            group[ts] = bar
        return groups

    def _publish(self, instrument, bars, *, replace=None, historical=False):
        self.writer.write_data(bars)
        # The staging catalog contains exactly the upstream-generated file.
        (src,) = self.staging.glob("data/bar/*/*.parquet")
        relative = src.relative_to(self.staging)
        dest = history_path(self.root, *relative.parts) if historical else self.root / relative
        if historical:
            if (replace is not None and dest != replace) or (replace is None and dest.exists()):
                raise ValueError("unexpected historical Parquet interval while publishing")
        ensure_dir(dest.parent)
        with src.open("rb") as file:
            os.fsync(file.fileno())
        os.replace(src, dest)
        fsync_dir(dest.parent)
        self.marks[instrument] = max(bars[-1].ts_event, self.marks.get(instrument, -1))

    def append(self, batch):
        groups = self._groups(batch, append_only=True)
        for (instrument, _day), by_ts in sorted(groups.items()):
            self._publish(instrument, [by_ts[ts] for ts in sorted(by_ts)])
        if groups:
            checkpoint(self.state, self.marks)

    def _day_files(self, instrument, day):
        if not INSTRUMENT.fullmatch(instrument):
            raise ValueError("expected a path-safe Nautilus instrument ID")
        name = f"{instrument}-{self.spec}-EXTERNAL"
        directory = history_path(self.root, "data", "bar", name)
        prefix = date.fromordinal(EPOCH.toordinal() + day).isoformat()
        return [history_path(directory, file.name) for file in sorted(directory.glob(f"{prefix}T*.parquet"))]

    def history_timestamps(self, instrument, start, end):
        """Inspect actual rows. File bounds and maximum timestamps do not prove coverage."""
        files = [str(file) for day in range(start // NS_PER_DAY, end // NS_PER_DAY + 1)
                 for file in self._day_files(instrument, day)]
        if not files:
            return set()
        return {row.ts_event for row in self.reader.query(Bar, start=start, end=end, files=files)}

    def merge_history(self, batch):
        """Fill gaps in an isolated history catalog, retaining every committed row."""
        groups = self._groups(batch, append_only=False)
        for (instrument, day), by_ts in sorted(groups.items()):
            remaining = deque(sorted(by_ts))
            for file in self._day_files(instrument, day):
                if not remaining:
                    break
                old = self.reader.query(Bar, files=[str(file)])
                before = []
                while remaining and remaining[0] < old[0].ts_event:
                    before.append(by_ts[remaining.popleft()])
                if before:
                    self._publish(instrument, before, historical=True)
                merged = {bar.ts_event: bar for bar in old}
                missing = []
                while remaining and remaining[0] <= old[-1].ts_event:
                    ts = remaining.popleft()
                    if ts not in merged:
                        missing.append(ts)
                if missing:
                    merged.update((ts, by_ts[ts]) for ts in missing)
                    # Bounds stay the same. Readers see either complete version of this file.
                    self._publish(instrument, [merged[ts] for ts in sorted(merged)], replace=file, historical=True)
            if remaining:
                self._publish(instrument, [by_ts[ts] for ts in remaining], historical=True)
        if groups:
            checkpoint(self.state, self.marks)


@contextmanager
def open_store(catalog, state, spec, source="replay", *, historical=False, source_metadata=None):
    if not catalog.is_absolute() or not state.is_absolute():
        raise ValueError("catalog and state paths must be absolute")
    private = catalog / ".finance-collector"
    if historical:
        for file in ("writer.lock", "source.json", "active"):
            history_path(catalog, ".finance-collector", file)
        for file in ("writer.lock", "catalog.json", "watermarks.json"):
            history_path(state, file)
        history_path(catalog, "data", "bar")
    ensure_dir(private)
    ensure_dir(state)
    with (private / "writer.lock").open("a") as lock, (state / "writer.lock").open("a") as state_lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        fcntl.flock(state_lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        check_provenance(catalog, state, source, spec, source_metadata)
        # Only this writer owns active/. Ignore uncommitted files after a hard kill.
        staging = private / "active"
        if staging.exists():
            shutil.rmtree(staging)
        staging.mkdir()
        yield Store(catalog, state, staging, spec, historical=historical)


def main():
    catalog, state, spec, source = sys.argv[1:]
    with open_store(Path(catalog), Path(state), spec, source) as store:
        print(json.dumps(store.marks), flush=True)
        for line in sys.stdin:
            store.append(json.loads(line))
            print(json.dumps(store.marks), flush=True)


if __name__ == "__main__":
    main()
