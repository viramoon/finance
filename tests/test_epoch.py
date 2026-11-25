from pathlib import Path
from shutil import copyfile
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import Mock

import pandas as pd
import pyarrow.parquet as parquet
from nautilus_trader.backtest.engine import BacktestEngine
from nautilus_trader.config import BacktestEngineConfig, LoggingConfig
from nautilus_trader.model.currencies import USD
from nautilus_trader.model.enums import AccountType, OmsType
from nautilus_trader.model.objects import Money
from nautilus_trader.persistence.catalog.parquet import ParquetDataCatalog
from nautilus_trader.persistence.wranglers import BarDataWrangler

from fertile_finance.backtest_research import simulate
from fertile_finance.backtest_sample import run_sample
from fertile_finance.epoch import DroppedTimestamps, filter_pre_epoch, process_bar_frame
from fertile_finance.research_catalog import bar_type, cash_equity


DAY_NS = 86_400_000_000_000
WRAPPED = 18446657673709551616


class EpochGuardTests(unittest.TestCase):
    def setUp(self):
        self.tmp = TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.instrument = cash_equity("TEST.XNYS")
        self.wrangler = BarDataWrangler(bar_type("TEST.XNYS"), self.instrument)

    def frame(self):
        index = pd.DatetimeIndex(["1969-12-31", "1970-01-01", "1970-01-02"], tz="UTC")
        # The native sample wrangler requires a writable double buffer. Mixed
        # column dtypes, as in the upstream CSV example, produce that buffer.
        return pd.DataFrame(dict(open=[100.0]*3, high=[100.0]*3, low=[100.0]*3,
                                 close=[100.0]*3, volume=[1]*3), index=index)

    def engine(self):
        engine = BacktestEngine(BacktestEngineConfig(logging=LoggingConfig(bypass_logging=True)))
        self.addCleanup(engine.dispose)
        engine.add_venue(
            venue=self.instrument.id.venue, oms_type=OmsType.NETTING, account_type=AccountType.CASH,
            starting_balances=[Money.from_str("100.00 USD")], base_currency=USD,
        )
        engine.add_instrument(self.instrument)
        return engine

    def test_pinned_nautilus_wraps_before_catalog_and_engine_reject(self):
        bars = self.wrangler.process(self.frame())
        events = [bar.ts_event for bar in bars]
        self.assertEqual(events, [WRAPPED, 0, DAY_NS])
        catalog = ParquetDataCatalog(self.root)
        with self.assertRaisesRegex(ValueError, "monotonically increasing") as unsorted:
            catalog.write_data(bars)
        self.assertFalse(list(self.root.rglob("*.parquet")))

        sorted_bars = sorted(bars, key=lambda bar: bar.ts_init)
        catalog.write_data(sorted_bars)
        (filename,) = self.root.rglob("*.parquet")
        self.assertTrue(filename.name.endswith(f"_{WRAPPED}.parquet"))
        written = parquet.read_table(filename, columns=["ts_event"])["ts_event"].to_pylist()
        self.assertEqual(written, [0, DAY_NS, WRAPPED])
        with self.assertRaisesRegex(ValueError, "not enough values to unpack") as query:
            catalog.bars()

        engine = self.engine()
        engine.add_data(bars)
        ordered = [bar.ts_event for bar in engine.data]
        self.assertEqual(ordered, [0, DAY_NS, WRAPPED])
        with self.assertRaisesRegex(pd.errors.OutOfBoundsDatetime, str(WRAPPED)) as run:
            engine.run()
        print(f"Raw wrangler ts_event: {events}; catalog unsorted: {type(unsorted.exception).__name__}; "
              f"sorted Parquet ts_event: {written}; catalog query: {type(query.exception).__name__}; "
              f"engine add_data: {ordered}; engine run: {type(run.exception).__name__}")

    def test_finance_rejects_before_wrangling_and_runs_epoch_and_later(self):
        process = Mock(wraps=self.wrangler.process)
        with self.assertRaisesRegex(ValueError, "at or after the epoch"):
            process_bar_frame(process, self.frame())
        process.assert_not_called()
        bars, dropped = process_bar_frame(process, self.frame(), mode="drop")
        process.assert_called_once()
        self.assertEqual(dropped, DroppedTimestamps(1, -DAY_NS, -DAY_NS))
        self.assertEqual([bar.ts_event for bar in bars], [0, DAY_NS])
        catalog = ParquetDataCatalog(self.root)
        catalog.write_data(bars)
        self.assertEqual([bar.ts_event for bar in catalog.bars()], [0, DAY_NS])
        engine = self.engine()
        engine.add_data(catalog.bars())
        engine.run()
        result = engine.get_result()
        self.assertEqual((result.backtest_start, result.backtest_end, result.iterations), (0, DAY_NS, 2))
        print(f"Guarded epoch backtest: {result.backtest_start} to {result.backtest_end}, "
              f"iterations={result.iterations}, dropped={dropped}")

    def test_drop_counts_and_timezone_boundary(self):
        times = [0, -1, DAY_NS, -DAY_NS, -1]
        kept, dropped = filter_pre_epoch(times, timestamp=int, mode="drop")
        self.assertEqual(kept, [0, DAY_NS])
        self.assertEqual(dropped, DroppedTimestamps(3, -DAY_NS, -1))
        frame = self.frame().tz_convert("America/New_York")
        bars, dropped = process_bar_frame(self.wrangler.process, frame, mode="drop")
        self.assertEqual([bar.ts_event for bar in bars], [0, DAY_NS])
        self.assertEqual(dropped, DroppedTimestamps(1, -DAY_NS, -DAY_NS))
        bars, dropped = process_bar_frame(self.wrangler.process, frame.iloc[:1], mode="drop")
        self.assertEqual(bars, [])
        self.assertEqual(dropped.count, 1)

    def test_invalid_times_cannot_be_dropped_or_forwarded(self):
        frame = self.frame().iloc[1:].copy()
        frame.index = pd.DatetimeIndex([None, "1970-01-02"], tz="UTC")
        process = Mock()
        with self.assertRaisesRegex(ValueError, "without NaT"):
            process_bar_frame(process, frame, mode="drop")
        with self.assertRaisesRegex(ValueError, "within int64"):
            process_bar_frame(process, self.frame().iloc[1:], ts_init_delta=2**63 - 1)
        process.assert_not_called()
        with self.assertRaisesRegex(ValueError, "within int64"):
            filter_pre_epoch([WRAPPED], timestamp=int, mode="drop")
        with self.assertRaisesRegex(ValueError, "within int64"):
            simulate(self.instrument, self.wrangler.process(self.frame()), {}, 2, 3, 1)

    def test_official_sample_wrapper_rejects_pre_epoch_csv(self):
        example = Path("vendor/nautilus_trader/examples/backtest/example_01_load_bars_from_custom_csv")
        source = Path(__file__).resolve().parents[1] / example
        fixture = self.root / example
        fixture.mkdir(parents=True)
        for name in ("run_example.py", "strategy.py"):
            copyfile(source / name, fixture / name)
        (fixture / "6EH4.XCME_1min_bars.csv").write_text(
            "timestamp_utc;open;high;low;close;volume;pricetype\n"
            "1969-12-31 23:59:00;1.1076;1.10785;1.1076;1.10775;205;Last\n"
            "1970-01-01 00:00:00;1.10775;1.1078;1.1077;1.10775;86;Last\n"
        )
        with self.assertRaisesRegex(ValueError, "at or after the epoch"):
            run_sample(self.root)
