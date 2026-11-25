from concurrent.futures import ThreadPoolExecutor
from dataclasses import replace
from datetime import date, datetime, timezone
from decimal import Decimal
from pathlib import Path
from tempfile import TemporaryDirectory
from threading import Barrier
import unittest
from unittest.mock import patch

from nautilus_trader.persistence.catalog.parquet import ParquetDataCatalog

from fertile_finance.backtest_research import run_research
from fertile_finance.historical_catalog import HistoricalCatalog
from fertile_finance.historical_records import FredDailyObservation, YahooIndexBar, YahooSourceBar
from fertile_finance.historical_types import EpochDiscard
from fertile_finance.research_catalog import ResearchError
from fertile_finance.research_factors import load_fred, load_ken_french
from fertile_finance.yahoo_daily import load_yahoo_daily


FIXTURES = Path(__file__).parent / "fixtures" / "historical"
ARCHIVE_DAY = date(2024, 12, 3)


class HistoricalCatalogTests(unittest.TestCase):
    def setUp(self):
        self.tmp = TemporaryDirectory(dir="/tmp")
        self.addCleanup(self.tmp.cleanup)
        self.data_root = Path(self.tmp.name)
        with patch.dict("os.environ", {"FERTILE_DATA_ROOT": str(self.data_root)}):
            self.catalog = HistoricalCatalog()

    def yahoo(self, name="ibm", **kwargs):
        return load_yahoo_daily(FIXTURES / f"yahoo_{name}.json", adjustment_as_of=ARCHIVE_DAY, **kwargs)

    def snapshot(self, source, series_id):
        return self.data_root / "catalog" / "historical" / source / series_id / "current"

    def test_yahoo_quote_factor_and_corporate_actions_round_trip(self):
        batch = self.yahoo()
        written = self.catalog.write_yahoo(batch)
        history = self.catalog.read_yahoo("IBM.XNYS")
        self.assertEqual((written.first, written.last), (date(2024, 1, 3), date(2024, 11, 29)))
        self.assertEqual((written.observations, written.dividends, written.splits), (4, 2, 1))
        self.assertEqual(tuple(replace(bar, ts_event=None) for bar in history.bars), batch.bars)
        self.assertEqual(history.dividends, batch.dividends)
        self.assertEqual(history.splits, batch.splits)
        self.assertEqual(history.dividends[0].amount.as_tuple(), Decimal("0.212345678901234567").as_tuple())
        self.assertEqual((history.splits[0].numerator, history.splits[0].denominator), (2, 1))
        self.assertEqual(history.dividends[-1].session, date(2024, 12, 2))
        source = history.bars[0]
        self.assertEqual(source.close.as_tuple(), Decimal("150.1234567890123456").as_tuple())
        self.assertEqual(history.bars[2].close.as_tuple(), Decimal("77.50").as_tuple())
        self.assertEqual(source.price_basis, "yahoo_chart_quote")
        adjusted = source.adjusted()
        self.assertEqual(adjusted.price_basis, "yahoo_chart_adjusted_close_ratio")
        self.assertEqual(adjusted.close, Decimal("75.0617283945061728"))
        self.assertEqual(adjusted.adjustment_as_of, ARCHIVE_DAY)
        self.assertEqual(adjusted.volume, source.volume)
        self.assertEqual(adjusted.ts_event, source.ts_event)
        native = ParquetDataCatalog(self.snapshot("yahoo", "IBM.XNYS"))
        source_row = native.custom_data(YahooSourceBar)[0].data
        self.assertEqual(source_row.close, "150.1234567890123456")
        self.assertEqual(source_row.adjustment_factor, "0.5")

    def test_yahoo_close_uses_xnys_standard_dst_and_early_sessions(self):
        self.catalog.write_yahoo(self.yahoo())
        bars = self.catalog.read_yahoo("IBM.XNYS").bars
        closes = [datetime.fromtimestamp(bar.ts_event // 10**9, timezone.utc).isoformat() for bar in bars]
        self.assertEqual(closes, [
            "2024-01-03T21:00:00+00:00", "2024-07-02T20:00:00+00:00",
            "2024-07-03T17:00:00+00:00", "2024-11-29T18:00:00+00:00",
        ])
        self.assertTrue(all(bar.ts_init == bar.ts_event for bar in bars))
        holiday = replace(self.yahoo().bars[0], session=date(2024, 7, 4))
        with self.assertRaisesRegex(ValueError, "not an XNYS session"):
            self.catalog.write_yahoo(replace(self.yahoo(), bars=(holiday,)), mode="replace")

    def test_index_is_reference_and_official_job_cannot_register_it(self):
        self.catalog.write_yahoo(self.yahoo("gspc", pre_epoch="drop"))
        history = self.catalog.read_yahoo("GSPC.INDEX")
        self.assertEqual(history.series_id, "GSPC.INDEX")
        self.assertEqual(history.symbol, "^GSPC")
        self.assertTrue(history.is_reference)
        self.assertEqual(history.bars[0].open, Decimal(0))
        self.assertIsNone(history.bars[0].volume)
        self.assertIsNone(history.bars[0].adjusted())
        native = ParquetDataCatalog(self.snapshot("yahoo", "GSPC.INDEX"))
        self.assertEqual(native.instruments(), [])
        self.assertEqual(native.bars(), [])
        self.assertEqual(native.custom_data(YahooSourceBar), [])
        self.assertEqual(len(native.custom_data(YahooIndexBar)), 2)
        with self.assertRaises(ResearchError) as rejected:
            run_research(self.catalog.root, dict(
                strategy="SmaCrossover", instrument=history.series_id,
                start="2024-07-01T00:00:00Z", end="2024-07-05T00:00:00Z",
                params=dict(fast_period=2, slow_period=3, shares=1),
            ))
        self.assertEqual(rejected.exception.code, "invalid_instrument")

    def test_epoch_guard_reports_only_rows_really_written(self):
        with self.assertRaisesRegex(ValueError, "Pre-epoch observation 1969-12-31"):
            self.yahoo("gspc")
        batch = self.yahoo("gspc", pre_epoch="drop")
        discarded = date(1969, 12, 31)
        report = self.catalog.write_yahoo(batch, start=discarded, end=date(1970, 1, 8))
        days = [bar.session for bar in self.catalog.read_yahoo(batch.series_id).bars]
        self.assertEqual(report.dropped_pre_epoch, EpochDiscard(1, discarded, discarded))
        self.assertEqual(days, [date(1970, 1, 2), date(1970, 1, 5)])
        self.assertEqual((report.first, report.last, report.observations), (days[0], days[-1], len(days)))
        direct = replace(batch, bars=(replace(batch.bars[0], session=discarded),))
        with self.assertRaisesRegex(ValueError, "Pre-epoch observation"):
            self.catalog.write_yahoo(direct, mode="replace")
        self.assertEqual(days, [bar.session for bar in self.catalog.read_yahoo(batch.series_id).bars])

    def test_replace_same_range_publishes_new_rows_and_only_replaces_scoped_events(self):
        batch = self.yahoo()
        initial = self.catalog.write_yahoo(batch)
        snapshot = self.snapshot("yahoo", batch.series_id)
        before = snapshot.readlink()
        new_cutoff = date(2024, 12, 4)
        changed = replace(batch.bars[0], open=Decimal("149.9900"), adjustment_as_of=new_cutoff)
        correction = replace(batch, bars=(changed,), dividends=(), splits=(), adjustment_as_of=new_cutoff)
        day = changed.session
        report = self.catalog.write_yahoo(correction, mode="replace", start=day, end=day)
        self.assertNotEqual(snapshot.readlink(), before)
        self.assertEqual((report.first, report.last, report.observations), (day, day, 1))
        intermediate = self.catalog.read_yahoo(batch.series_id)
        self.assertEqual(intermediate.bars[0].open.as_tuple(), changed.open.as_tuple())
        self.assertEqual(intermediate.bars[0].adjustment_as_of, new_cutoff)
        self.assertEqual(intermediate.bars[1].adjustment_as_of, ARCHIVE_DAY)
        self.assertEqual(intermediate.bars[-1].session, initial.last)
        self.assertEqual(intermediate.dividends, batch.dividends[1:])
        self.assertEqual(intermediate.splits, batch.splits)
        second_value = Decimal("150.00500")
        second = replace(correction, bars=(replace(changed, open=second_value),))
        self.catalog.write_yahoo(second, mode="replace", start=day, end=day)
        self.assertEqual(self.catalog.read_yahoo(batch.series_id, start=day, end=day).bars[0].open.as_tuple(),
                         second_value.as_tuple())

    def test_append_requires_disjoint_ranges_including_observation_gaps(self):
        batch = self.yahoo()
        first = replace(batch, bars=batch.bars[:2], dividends=batch.dividends[:1], splits=())
        later = replace(batch, bars=batch.bars[2:], dividends=batch.dividends[1:])
        self.catalog.write_yahoo(first)
        with self.assertRaisesRegex(ValueError, "overlaps the stored range"):
            self.catalog.write_yahoo(first)
        self.catalog.write_yahoo(later)
        day_without_observation = date(2024, 7, 5)
        addition = replace(batch, bars=(replace(batch.bars[2], session=day_without_observation),),
                           dividends=(), splits=())
        with self.assertRaisesRegex(ValueError, "overlaps the stored range"):
            self.catalog.write_yahoo(addition)
        self.assertEqual(len(self.catalog.read_yahoo(batch.series_id).bars), 4)

    def test_failed_writer_verification_preserves_previous_snapshot(self):
        batch = self.yahoo()
        self.catalog.write_yahoo(batch)
        previous = self.catalog.read_yahoo(batch.series_id)
        snapshot = self.snapshot("yahoo", batch.series_id)
        published = snapshot.readlink()
        with patch.object(ParquetDataCatalog, "write_data", return_value=None):
            with self.assertRaisesRegex(RuntimeError, "Nautilus write verification failed"):
                self.catalog.write_yahoo(batch, mode="replace")
        self.assertEqual(snapshot.readlink(), published)
        self.assertEqual(self.catalog.read_yahoo(batch.series_id), previous)

    def test_concurrent_append_checks_overlap_against_committed_data(self):
        batch = self.yahoo()
        start = Barrier(2)

        def append():
            start.wait(timeout=5)
            try:
                return self.catalog.write_yahoo(batch)
            except ValueError as exc:
                return exc

        with ThreadPoolExecutor(max_workers=2) as executor:
            tasks = [executor.submit(append) for _ in range(2)]
            results = [task.result(timeout=10) for task in tasks]
        errors = [error for error in results if isinstance(error, ValueError)]
        self.assertEqual(len(errors), 1)
        self.assertRegex(str(errors[0]), "overlaps the stored range")
        self.assertEqual(len(self.catalog.read_yahoo(batch.series_id).bars), 4)

    def test_french_factors_and_availability_round_trip(self):
        batch = load_ken_french(
            FIXTURES / "french_daily.csv", momentum_path=FIXTURES / "french_momentum_daily.csv", pre_epoch="drop",
        )
        report = self.catalog.write_french(batch)
        history = self.catalog.read_french()
        self.assertEqual(history.rows, batch.rows)
        self.assertEqual(report.observations, 4)
        self.assertEqual(report.first, date(1970, 1, 2))
        self.assertEqual(report.dropped_pre_epoch.count, 1)
        sample, = self.catalog.read_french(start=date(2024, 7, 3), end=date(2024, 7, 3)).rows
        self.assertEqual((sample.rf, sample.mom), (Decimal("0.0002"), Decimal("-0.0017")))
        self.assertIsNone(sample.available_on)
        self.assertIsNone(history.rows[-1].rf)

    def test_fred_native_units_missing_vintage_and_epoch_boundary_round_trip(self):
        batch = load_fred(FIXTURES / "fred_dgs10_observations.json", series_id="DGS10")
        first_day = date(1970, 1, 1)
        epoch_row = replace(batch.rows[0], observation_date=first_day)
        report = self.catalog.write_fred(replace(batch, rows=(epoch_row, *batch.rows)))
        self.assertEqual((report.first, report.last, report.observations), (first_day, date(2024, 7, 5), 4))
        read = self.catalog.read_fred("DGS10", start=date(2024, 7, 3))
        self.assertEqual(read.rows, batch.rows)
        self.assertEqual(read.rows[0].value.as_tuple(), Decimal("4.12345678901234567890123456789").as_tuple())
        self.assertEqual(read.rows[0].realtime_start, date(2024, 7, 10))
        self.assertIsNone(read.rows[1].value)
        self.assertEqual(read.rows[2].value.as_tuple(), Decimal("4.3900").as_tuple())
        native = ParquetDataCatalog(self.snapshot("fred", "DGS10"))
        row = native.custom_data(FredDailyObservation)[0].data
        self.assertEqual((row.ts_event, row.ts_init), (0, 0))


if __name__ == "__main__":
    unittest.main()
