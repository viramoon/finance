from datetime import date
from decimal import Decimal, localcontext
from pathlib import Path
import tempfile
import unittest
from urllib.parse import parse_qs, urlsplit

from fertile_finance.yahoo_daily import load_yahoo_daily, yahoo_chart_url


FIXTURES = Path(__file__).parent / "fixtures" / "historical"


class YahooDailyTest(unittest.TestCase):
    def test_chart_url_uses_exact_inclusive_day_periods(self):
        url = urlsplit(yahoo_chart_url("^GSPC", date(2024, 3, 10), date(2024, 3, 10)))
        self.assertEqual(url.path, "/v8/finance/chart/%5EGSPC")
        self.assertEqual(parse_qs(url.query), {
            "period1": ["1710046800"], "period2": ["1710129600"],
            "interval": ["1d"], "events": ["div,split"], "includeAdjustedClose": ["true"],
        })

    def test_quotes_and_company_actions_preserve_exact_source_values(self):
        with localcontext() as ctx:
            ctx.prec = 4
            history = load_yahoo_daily(FIXTURES / "yahoo_ibm.json", adjustment_as_of=date(2024, 12, 3))
        self.assertEqual((history.series_id, history.asset_type, history.currency), ("IBM.XNYS", "stock", "USD"))
        self.assertEqual([row.session for row in history.bars], [
            date(2024, 1, 3), date(2024, 7, 2), date(2024, 7, 3), date(2024, 11, 29),
        ])
        first, second = history.bars[:2]
        self.assertEqual(first.close, Decimal("150.1234567890123456"))
        self.assertEqual(first.volume, Decimal("12345678"))
        self.assertEqual(first.adjusted_close, Decimal("75.0617283945061728"))
        self.assertEqual(first.adjustment_factor, Decimal("0.5"))
        self.assertEqual(first.adjustment_as_of, date(2024, 12, 3))
        self.assertEqual(second.adjustment_factor, Decimal("0.6666666666666666666666666666666667"))
        self.assertEqual([event.amount for event in history.dividends], [Decimal("0.212345678901234567"), Decimal("0.375")])
        self.assertEqual(history.dividends[-1].session, date(2024, 12, 2))
        (split,) = history.splits
        self.assertEqual((split.session, split.source_timestamp, split.numerator, split.denominator, split.source_id),
                         (date(2024, 7, 3), 1720013400, 2, 1, "1720013400"))

    def test_cutoff_cannot_precede_outlying_event(self):
        with self.assertRaisesRegex(ValueError, "adjustment_as_of precedes"):
            load_yahoo_daily(FIXTURES / "yahoo_ibm.json", adjustment_as_of=date(2024, 11, 29))

    def test_index_epoch_guard_precedes_quote_parsing(self):
        archive = FIXTURES / "yahoo_gspc.json"
        with self.assertRaisesRegex(ValueError, "1969-12-31"):
            load_yahoo_daily(archive, adjustment_as_of=date(1970, 1, 5))
        history = load_yahoo_daily(archive, adjustment_as_of=date(1970, 1, 5), pre_epoch="drop")
        self.assertEqual((history.series_id, history.asset_type, history.listing_venue), ("GSPC.INDEX", "index", None))
        self.assertEqual(history.dropped_pre_epoch.count, 1)
        self.assertEqual(history.dropped_pre_epoch.first, date(1969, 12, 31))
        self.assertEqual(history.dropped_pre_epoch.last, date(1969, 12, 31))
        first, second = history.bars
        self.assertEqual((first.session, first.open, first.high, first.close, first.volume, first.adjustment_factor),
                         (date(1970, 1, 2), Decimal(0), None, Decimal("95.10"), None, None))
        self.assertEqual(second.adjustment_factor, Decimal(1))
        self.assertEqual(history.dividends, ())
        self.assertEqual(history.splits, ())

    def test_prefixed_index_cannot_be_labelled_equity(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "mislabelled.json"
            raw = (FIXTURES / "yahoo_gspc.json").read_text()
            path.write_text(raw.replace('"instrumentType": "INDEX"', '"instrumentType": "EQUITY"'))
            with self.assertRaisesRegex(ValueError, "index symbol cannot identify an equity"):
                load_yahoo_daily(path, adjustment_as_of=date(1970, 1, 5), pre_epoch="drop")
