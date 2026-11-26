"""Pull closed historical bars into the collector's Nautilus catalog format."""

import argparse
from collections.abc import Iterable
from dataclasses import asdict, dataclass
from datetime import date, timedelta
import json
import os
from pathlib import Path
import subprocess
import sys
import time
from typing import Protocol

from fertile_finance.collector_store import NS_PER_DAY, atomic_json, open_store
from fertile_finance.epoch import EPOCH, unix_nanos
from fertile_finance.historical_crypto import CRYPTO_INTERVAL_MS, CRYPTO_SYMBOLS, CryptoBars, import_binance_trades
from fertile_finance.historical_equity import EQUITY_INSTRUMENTS, YahooDailySource, equity_daily_closes, equity_sessions
from fertile_finance.history_paths import history_path, validate_history_root


NS_PER_SECOND = 1_000_000_000
NS_PER_MINUTE = 60 * NS_PER_SECOND
NS_PER_MILLISECOND = 1_000_000


class HistoricalBarSource(Protocol):
    """Finite counterpart to BarSource.backfill with the same closed-bar rows."""

    name: str
    spec: str
    window_ns: int

    def instrument(self, symbol: str) -> str: ...

    def expected(self, start: int, end: int) -> Iterable[int]: ...

    def backfill(self, symbol: str, start: int, end: int) -> list[dict]: ...


class EquityDaily:
    name = "yahoo-daily"
    spec = "1-DAY-LAST"
    window_ns = 31 * NS_PER_DAY

    def __init__(self, source):
        self.source = source

    def instrument(self, symbol):
        return EQUITY_INSTRUMENTS[symbol]

    def expected(self, start, end):
        return equity_daily_closes(start, end).values()

    def backfill(self, symbol, start, end):
        return self.source.fetch(symbol, start, end)


class IbkrMinute:
    name = "ibkr-minute"
    spec = "1-MINUTE-LAST"
    window_ns = NS_PER_DAY

    def __init__(self, binary, root, *, run=subprocess.run):
        self.binary = binary
        self.root = root
        self.run = run

    def instrument(self, symbol):
        return EQUITY_INSTRUMENTS[symbol]

    def expected(self, start, end):
        first = EPOCH + timedelta(days=start // NS_PER_DAY)
        last = EPOCH + timedelta(days=end // NS_PER_DAY)
        for _session, opens, closes in equity_sessions(first, last):
            first_close = max(opens + NS_PER_MINUTE, ceil_slot(start, NS_PER_MINUTE))
            yield from range(first_close, min(end, closes) + 1, NS_PER_MINUTE)

    def backfill(self, symbol, start, end):
        command = [self.binary, "historical-minute", symbol,
                   str((start - NS_PER_MINUTE) // NS_PER_SECOND), str(end // NS_PER_SECOND)]
        env = {**os.environ, "FERTILE_DATA_ROOT": str(self.root)}
        result = self.run(command, env=env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, check=True)
        return json.loads(result.stdout)


def ceil_slot(ts, step):
    return ((ts + step - 1) // step) * step


class CryptoHistory:
    def __init__(self, name, interval, source, now_ns):
        self.name = name
        self.interval = interval
        self.source = source
        self.now_ms = now_ns // NS_PER_MILLISECOND
        self.spec = {"1m": "1-MINUTE-LAST", "1d": "1-DAY-LAST"}[interval]
        self.step = CRYPTO_INTERVAL_MS[interval] * NS_PER_MILLISECOND
        self.window_ns = self.step * source.page_limit

    def instrument(self, symbol):
        return self.source.instrument(symbol)

    def expected(self, start, end):
        return range(max(self.step, ceil_slot(start, self.step)), end + 1, self.step)

    def backfill(self, symbol, start, end):
        # Providers take [opening time, end) in milliseconds. Storage keys on close.
        return self.source.fetch(symbol, self.interval, (start - self.step) // NS_PER_MILLISECOND,
                                 end // NS_PER_MILLISECOND, now_ms=self.now_ms)


@dataclass(frozen=True)
class Gap:
    start_ns: int
    end_ns: int
    missing: int


def gap_runs(expected, present):
    first = last = None
    count = 0
    for ts in expected:
        if ts in present:
            if count:
                yield Gap(first, last, count)
                count = 0
            continue
        if not count:
            first = ts
        last = ts
        count += 1
    if count:
        yield Gap(first, last, count)


def _read_cursor(cursor, start, end):
    through, pending = start - 1, []
    if cursor.exists():
        saved = json.loads(cursor.read_text())
        if not isinstance(saved, dict):
            raise ValueError("invalid historical scan cursor")
        through, pending = saved.get("scanned_through_ns"), saved.get("windows_with_gaps")
        previous_end = start - 1
        if type(through) is not int or not previous_end <= through <= end or not isinstance(pending, list):
            raise ValueError("invalid historical scan cursor")
        for window in pending:
            if (not isinstance(window, list) or len(window) != 2 or any(type(ts) is not int for ts in window)
                    or not previous_end < window[0] <= window[1] <= through):
                raise ValueError("invalid window in historical scan cursor")
            previous_end = window[1]
    return through, pending


def _scan_window(store, source, symbol, instrument, start, end):
    expected = tuple(source.expected(start, end))
    if not expected:
        return []
    present = store.history_timestamps(instrument, expected[0], expected[-1])
    missing = [ts for ts in expected if ts not in present]
    if not missing:
        return []
    rows = source.backfill(symbol, missing[0], missing[-1])
    allowed = set(expected)
    if not isinstance(rows, list) or any(row["instrument"] != instrument
                                           or unix_nanos(row["ts_ns"]) not in allowed
                                           or not missing[0] <= row["ts_ns"] <= missing[-1]
                                           for row in rows):
        raise ValueError("historical BarSource returned a bar outside its requested intervals")
    new_rows = [row for row in rows if row["ts_ns"] not in present]
    store.merge_history(new_rows)
    present.update(row["ts_ns"] for row in new_rows)
    return list(gap_runs(expected, present))


def pull_bars(data_root, source: HistoricalBarSource, symbols, start, end, *, now_ns, rescan=False):
    """Resume per instrument and request range. Commit the scan cursor after Parquet."""
    root = validate_history_root(data_root)
    if unix_nanos(end) < unix_nanos(start):
        raise ValueError("historical start must be before or equal to end")
    limit = min(end, unix_nanos(now_ns))
    if limit < start:
        return {"source": source.name, "status": "no_closed_window", "instruments": {}}

    catalog = history_path(root, "catalog", "historical", source.name, source.spec)
    state = history_path(root, "finance-state", "historical", source.name, source.spec)
    instruments = {}
    with open_store(catalog, state, source.spec, source.name, historical=True) as store:
        for symbol in symbols:
            instrument = source.instrument(symbol)
            cursor = history_path(state, f"{instrument}-{start}-{end}.json")
            through, pending = (start - 1, []) if rescan else _read_cursor(cursor, start, end)
            if through > limit:
                raise ValueError("historical cutoff precedes the saved scan cursor")
            windows_with_gaps = dict(pending)
            gaps = []

            def scan(lower, upper):
                missing = _scan_window(store, source, symbol, instrument, lower, upper)
                gaps.extend(missing)
                if missing:
                    windows_with_gaps[lower] = upper
                else:
                    windows_with_gaps.pop(lower, None)

            def checkpoint():
                atomic_json(cursor, {"scanned_through_ns": through,
                                     "windows_with_gaps": sorted(windows_with_gaps.items())})

            for lower, upper in pending:
                scan(lower, upper)
                checkpoint()
            while through < limit:
                upper = min(through + source.window_ns, limit)
                scan(through + 1, upper)
                through = upper
                checkpoint()

            instruments[instrument] = {"scanned_through_ns": through,
                                       "watermark_ns": store.marks.get(instrument),
                                       "gaps": [asdict(gap) for gap in gaps]}
    status = "gaps_remaining" if any(report["gaps"] for report in instruments.values()) else "complete"
    return {"source": source.name, "status": status, "instruments": instruments}


def _parser():
    parser = argparse.ArgumentParser(description="Import permitted closed bars or Binance Spot trade archives")
    parser.add_argument("source", choices=("yahoo-daily", "ibkr-minute", "hyperliquid", "binance", "binance-trades"))
    parser.add_argument("--start", required=True, type=date.fromisoformat, help="first UTC date, inclusive")
    parser.add_argument("--end", required=True, type=date.fromisoformat, help="last UTC date, inclusive")
    parser.add_argument("--symbol", action="append", help="select a symbol (repeatable); default: all for this source")
    parser.add_argument("--interval", choices=("1m", "1d"), help="crypto bar interval (default: 1m)")
    parser.add_argument("--data-root", type=Path, help="absolute output root, or set FERTILE_DATA_ROOT")
    parser.add_argument("--ib-binary", default="finance-ib", help="path to the finance-ib executable")
    parser.add_argument("--allow-download", action="store_true", help="confirm deployment and local recording authorization")
    parser.add_argument("--enable-binance", action="store_true", help="explicitly enable Binance in this deployment")
    parser.add_argument("--rescan", action="store_true", help="recheck bar coverage against Parquet from the requested start")
    return parser


def main(argv=None):
    parser = _parser()
    args = parser.parse_args(argv)
    source = args.source
    if source in {"binance", "binance-trades"} and not args.enable_binance:
        print(json.dumps({"source": source, "status": "disabled_by_profile"}))
        return 0
    if not args.allow_download:
        parser.error("--allow-download is required on an authorized deployment")
    available = EQUITY_INSTRUMENTS if source in {"ibkr-minute", "yahoo-daily"} else CRYPTO_SYMBOLS
    symbols = list(dict.fromkeys(args.symbol or available))
    if any(symbol not in available for symbol in symbols):
        parser.error(f"supported symbols for {source}: {', '.join(available)}")
    if args.end < args.start:
        parser.error("--start must be before or equal to --end")
    if args.interval and source not in {"hyperliquid", "binance"}:
        parser.error("--interval applies to crypto bars")
    if args.rescan and source == "binance-trades":
        parser.error("--rescan applies to bars")
    root = args.data_root or os.environ.get("FERTILE_DATA_ROOT")
    if not root:
        parser.error("set --data-root or FERTILE_DATA_ROOT")

    try:
        root = validate_history_root(root)
        now = time.time_ns()
        if source == "binance-trades":
            reports = {symbol: asdict(import_binance_trades(root, symbol, args.start, args.end,
                                                           binance_enabled=True)) for symbol in symbols}
            status = "gaps_remaining" if any(report["missing_days"] for report in reports.values()) else "complete"
            report = {"source": source, "status": status, "instruments": reports}
        else:
            if source == "yahoo-daily":
                provider = EquityDaily(YahooDailySource())
            elif source == "ibkr-minute":
                provider = IbkrMinute(args.ib_binary, root)
            else:
                provider = CryptoHistory(source, args.interval or "1m", CryptoBars(source, binance_enabled=args.enable_binance), now)
            first = (args.start - EPOCH).days * NS_PER_DAY
            last = (args.end - EPOCH).days * NS_PER_DAY + NS_PER_DAY - 1
            report = pull_bars(root, provider, symbols, first, last, now_ns=now, rescan=args.rescan)
    except (OSError, ValueError, RuntimeError, subprocess.CalledProcessError) as exc:
        print(f"historical pull: {exc}", file=sys.stderr)
        return 1
    print(json.dumps(report, sort_keys=True))
    return 2 if report["status"] == "gaps_remaining" else 0


if __name__ == "__main__":
    sys.exit(main())
