from dataclasses import replace
from datetime import date, datetime, timezone
from decimal import Decimal
from pathlib import Path
from tempfile import TemporaryDirectory
import unittest

from fertile_finance.historical_catalog import HistoricalCatalog
from fertile_finance.historical_types import DailyBar, Dividend, EpochDiscard
from fertile_finance.portfolio_backtest import (
    PortfolioAsset, PortfolioConfig, TargetWeights, UniverseSnapshot, run_portfolio_backtest,
)
from fertile_finance.research_catalog import ResearchError, nyse, sessions
from fertile_finance.yahoo_daily import YahooDaily, load_yahoo_daily


FIXTURES = Path(__file__).parent / "fixtures" / "historical"


class PortfolioYahooTests(unittest.TestCase):
    def setUp(self):
        directory = TemporaryDirectory(dir="/tmp")
        self.addCleanup(directory.cleanup)
        self.catalog = HistoricalCatalog(Path(directory.name))
        self.start = date(2024, 11, 27)
        self.end = date(2024, 12, 4)
        self.closes = sessions(self.start, self.end)

    def archive(self, symbol, quotes, *, dividends=()):
        bars = []
        for day, quote in zip(self.closes, quotes, strict=True):
            if quote is None:
                row = DailyBar(day, None, None, None, None, None, None, Decimal("50"), self.end)
            else:
                opening, closing, volume = map(Decimal, quote)
                row = DailyBar(day, opening, max(opening, closing), min(opening, closing), closing,
                               volume, Decimal("0.5"), closing / 2, self.end)
            bars.append(row)
        batch = YahooDaily(symbol, "stock", "USD", "XNYS", self.end, tuple(bars), dividends, (), EpochDiscard())
        self.catalog.write_yahoo(batch)
        return self.catalog.read_yahoo(batch.series_id, start=self.start, end=self.end)

    def test_catalog_source_bars_run_with_raw_prices_and_quote_gaps(self):
        alpha = self.archive(
            "ALPHA",
            (("100.00000", "100.00", "200"), ("101.37", "103.25", "200"), None,
             ("106.10", "106.80", "0"), ("108.00", "108.00", "200")),
            dividends=(Dividend(self.end, self.closes[self.end] // 10**9, Decimal("1.00")),),
        )
        beta = self.archive("BETA", [("200.00", "200.00", "200")] * len(self.closes))
        assets = tuple(PortfolioAsset.from_yahoo(history, raw_split_free_reviewed=True)
                       for history in (alpha, beta))
        first, half_day, missing, zero, last = self.closes
        self.assertTrue(alpha.bars[2].is_missing_quote)
        self.assertEqual(alpha.bars[1].adjusted().close, Decimal("51.625"))
        self.assertEqual([bar.ts_event for bar in assets[0].bars],
                         [self.closes[day] for day in (first, half_day, zero, last)])
        self.assertEqual(assets[0].bars[2].volume.as_decimal(), Decimal(0))
        self.assertEqual(datetime.fromtimestamp(assets[0].bars[1].ts_event // 10**9, timezone.utc),
                         datetime(2024, 11, 29, 18, tzinfo=timezone.utc))

        opening = int(nyse().schedule.loc[str(first), "open"].value)
        members = (alpha.series_id, beta.series_id)
        result = run_portfolio_backtest(
            assets=assets, universe=(UniverseSnapshot(first, opening, members),),
            start=self.start, end=self.end,
            targets=(TargetWeights(first, self.closes[first], dict.fromkeys(members, Decimal("0.20"))),),
            config=PortfolioConfig(fill_timing="next_close", initial_cash=Decimal("1000.00")),
        )
        self.assertEqual([(fill.instrument_id, fill.price, fill.fill_ts) for fill in result.fills],
                         [(alpha.series_id, Decimal("103.25"), self.closes[half_day]),
                          (beta.series_id, Decimal("200.00"), self.closes[half_day])])
        self.assertIsNone(result.equity[missing])
        self.assertIsNone(result.equity[zero])
        self.assertEqual(result.equity[last], Decimal("1009.50"))
        self.assertIn((missing, "missing_price"), [(gap.date, gap.kind) for gap in result.gaps])
        self.assertIn((zero, "zero_volume"), [(gap.date, gap.kind) for gap in result.gaps])
        self.assertEqual(result.longest_sample["coverage"], "insufficient")
        self.assertEqual(result.longest_sample["missing_sessions"], 2)

    def test_archived_split_and_reference_index_are_rejected(self):
        for file, series, error in (("yahoo_ibm.json", "IBM.XNYS", "invalid_source"),
                                    ("yahoo_gspc.json", "GSPC.INDEX", "invalid_instrument")):
            with self.subTest(series=series):
                source = load_yahoo_daily(FIXTURES / file, adjustment_as_of=self.end, pre_epoch="drop")
                self.catalog.write_yahoo(source)
                with self.assertRaises(ResearchError) as raised:
                    PortfolioAsset.from_yahoo(self.catalog.read_yahoo(series), raw_split_free_reviewed=True)
                self.assertEqual(raised.exception.code, error)

    def test_unreviewed_adjusted_late_and_lossy_quotes_are_rejected(self):
        history = self.archive("TEST", [("100.00", "100.00", "200")] * len(self.closes))
        with self.assertRaisesRegex(ResearchError, "raw price review"):
            PortfolioAsset.from_yahoo(history, raw_split_free_reviewed=False)
        bar = history.bars[0]
        variants = (
            (bar.adjusted(), "invalid_source"),
            (replace(bar, ts_event=bar.ts_event + 1), "invalid_bar"),
            (replace(bar, close=Decimal("100.001")), "invalid_bar"),
            (replace(bar, volume=Decimal("200.5")), "invalid_bar"),
            (replace(bar, volume=Decimal("1e20")), "invalid_bar"),
            (replace(bar, open=Decimal("1e20"), high=Decimal("1e20")), "invalid_bar"),
        )
        for variant, error in variants:
            with self.subTest(variant=variant):
                with self.assertRaises(ResearchError) as raised:
                    PortfolioAsset.from_yahoo(replace(history, bars=(variant,)), raw_split_free_reviewed=True)
                self.assertEqual(raised.exception.code, error)
