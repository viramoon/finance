from datetime import date
from decimal import Decimal
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import patch

from fertile_finance.backtest_job import execute
from fertile_finance.historical_catalog import HistoricalCatalog
from fertile_finance.research_catalog import load_interval, sessions
from fertile_finance.yahoo_daily import load_yahoo_daily


REPO = Path(__file__).resolve().parents[1]
SYMBOL = "IBM.XNYS"
START = date(1975, 11, 25)
END = date(2025, 12, 31)
SPLIT_DAYS = (date(1979, 6, 1), date(1997, 5, 28), date(1999, 5, 27), date(2021, 11, 4))
EVIDENCE = "Constructed fixture with all quotes and volumes in the same share unit"


def write_split_archive(path, start, end, *, last_split_quote=None):
    """Construct Yahoo JSON using real calendar labels and invented quotes and ratios."""
    calendar = sessions(start, end)
    timestamp = {day: when // 10**9 for day, when in calendar.items()}
    events = {
        str(timestamp[day]): {"date": timestamp[day], "numerator": 2, "denominator": 1}
        for day in SPLIT_DAYS if day in timestamp
    }
    numbers = []
    for index, day in enumerate(calendar):
        cents = 4200 - abs(index % 80 - 40) * 5
        price = f"{cents // 100}.{cents % 100:02d}"
        if day < SPLIT_DAYS[-1]:
            price += "37500001000001"
        if day == SPLIT_DAYS[-1] and last_split_quote is not None:
            price = last_split_quote
        numbers.append(price)
    result = {
        "meta": {"currency": "USD", "symbol": "IBM", "exchangeName": "NYQ", "instrumentType": "EQUITY",
                 "exchangeTimezoneName": "America/New_York", "dataGranularity": "1d"},
        "timestamp": list(timestamp.values()),
        "indicators": {"quote": [{field: "fixture-prices" for field in ("open", "high", "low", "close")}
                                  | {"volume": [1000] * len(calendar)}]},
        "events": {"splits": events},
    }
    payload = json.dumps({"chart": {"result": [result], "error": None}})
    path.write_text(payload.replace('"fixture-prices"', f"[{','.join(numbers)}]"), encoding="utf-8")
    return calendar


class YahooSplitResearch(unittest.TestCase):
    def setUp(self):
        directory = TemporaryDirectory(dir="/tmp")
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.archive = self.root / "yahoo.json"
        self.history = self.root / "history"
        self.csv = self.root / "ibm.csv"
        self.lineage = self.root / "ibm.lineage.json"
        self.research = self.root / "research"
        self.env = {"PYTHONPATH": str(REPO / "python"), "PYTHONDONTWRITEBYTECODE": "1",
                    "FERTILE_RESEARCH_ROOT": str(self.research), "FERTILE_DATA_ROOT": str(self.root / "unused")}

    def cli(self, module, *args):
        return subprocess.run([sys.executable, "-m", f"fertile_finance.{module}", *map(str, args)],
                              capture_output=True, text=True, env=self.env, timeout=60, check=False)

    def convert(self, start, end, *, archive_snapshot=None):
        if archive_snapshot is None:
            source = ("--historical-data-root", self.history, "--series-id", SYMBOL)
        else:
            source = ("--yahoo-json", self.archive, "--adjustment-as-of", archive_snapshot)
        return self.cli("convert_research_csv", *source, "--start", start, "--end", end, "--output", self.csv,
                        "--yahoo-split-normalized-evidence", EVIDENCE)

    def import_csv(self, *review):
        return self.cli("research_catalog", self.csv, "--instrument", SYMBOL, "--source", "Constructed Yahoo fixture",
                        "--kind", "fixture", "--asset-type", "stock", *review)

    def test_ibm_long_split_normalized_catalog_runs_all_four_windows(self):
        calendar = write_split_archive(self.archive, START, END)
        HistoricalCatalog(self.history).write_yahoo(load_yahoo_daily(self.archive, adjustment_as_of=END))
        converted = self.convert(START, END)
        self.assertEqual(converted.returncode, 0, converted.stderr)
        self.lineage.write_text(converted.stdout, encoding="utf-8")
        lineage = json.loads(converted.stdout)
        self.assertEqual(lineage["source_kind"], "historical_catalog")
        self.assertEqual(lineage["price_basis"], "yahoo_chart_quote")
        self.assertEqual(lineage["output_price_basis"], "yahoo_chart_split_normalized")
        self.assertEqual(lineage["adjustment_as_of"], [str(END)])
        self.assertEqual(lineage["split_review_evidence"], EVIDENCE)
        self.assertEqual(lineage["split_events"], [
            {"session": str(day), "numerator": 2, "denominator": 1,
             "source_timestamp": calendar[day] // 10**9, "source_id": str(calendar[day] // 10**9)}
            for day in SPLIT_DAYS
        ])
        self.assertEqual(lineage["preserved_subcent_price_count"], 4 * sum(day < SPLIT_DAYS[-1] for day in calendar))
        self.assertEqual(lineage["normalized_price_count"], 0)
        self.assertEqual(lineage["sha256"], hashlib.sha256(self.csv.read_bytes()).hexdigest())
        early_price = "40.0037500001000001"
        self.assertEqual(self.csv.read_text().splitlines()[1], ",".join((str(START), *([early_price] * 4), "1000")))

        imported = self.import_csv("--yahoo-split-normalized-lineage", self.lineage)
        self.assertEqual(imported.returncode, 0, imported.stderr)
        self.assertEqual((self.research / SYMBOL / "yahoo-lineage.json").read_bytes(), self.lineage.read_bytes())
        manifest = json.loads(imported.stdout)
        self.assertEqual(manifest["price_basis"], lineage["output_price_basis"])
        self.assertEqual(manifest["corporate_actions"], lineage["corporate_actions"])
        self.assertEqual(manifest["source_sha256"], lineage["sha256"])
        _, _, bars, _ = load_interval(self.research, SYMBOL, START, START)
        self.assertEqual(bars[0].close.as_decimal(), Decimal(early_price))

        target = self.root / "result.json"
        with patch.dict(os.environ, self.env):
            execute({"strategy": "SmaCrossover", "instrument": SYMBOL,
                     "start": f"{START}T00:00:00Z", "end": f"{END}T00:00:00Z",
                     "params": {"fast_period": 10, "slow_period": 30, "shares": 1}}, target)
        summary = json.loads(target.read_text())["research"]
        self.assertEqual(summary["dataset"], manifest)
        self.assertGreater(summary["audit"]["fills"], 0)
        self.assertEqual(summary["observations"], len(calendar))
        windows = summary["windows"]
        self.assertEqual([row["window"] for row in windows], ["50y", "10y", "5y", "longest_sample"])
        for window in windows:
            self.assertEqual((window["coverage"], window["actual_end"], window["rf_source"]), ("full", str(END), "rf=0"))
            self.assertTrue(Decimal(window["metrics"]["sharpe"]).is_finite())
        self.assertFalse((self.root / "unused").exists())

    def test_later_split_is_retained_and_unattested_or_tampered_imports_fail(self):
        start, end = date(2021, 11, 2), date(2021, 11, 5)
        write_split_archive(self.archive, start, end)
        HistoricalCatalog(self.history).write_yahoo(load_yahoo_daily(self.archive, adjustment_as_of=end))
        converted = self.convert(start, date(2021, 11, 3))
        self.assertEqual(converted.returncode, 0, converted.stderr)
        self.lineage.write_text(converted.stdout, encoding="utf-8")
        lineage = json.loads(converted.stdout)
        self.assertEqual([event["session"] for event in lineage["split_events"]], ["2021-11-04"])
        self.assertEqual((lineage["observations"], lineage["preserved_subcent_price_count"]), (2, 8))

        for args, reason in [
            ((), "one of the arguments"),
            (("--raw-split-free-reviewed", "--yahoo-split-normalized-lineage", self.lineage), "not allowed"),
            (("--raw-split-free-reviewed",), "open on 2021-11-02"),
        ]:
            with self.subTest(args=args):
                rejected = self.import_csv(*args)
                self.assertNotEqual(rejected.returncode, 0)
                self.assertIn(reason, rejected.stderr)
                self.assertFalse((self.research / SYMBOL).exists())

        self.csv.write_bytes(self.csv.read_bytes().replace(b",1000", b",1001", 1))
        rejected = self.import_csv("--yahoo-split-normalized-lineage", self.lineage)
        self.assertNotEqual(rejected.returncode, 0)
        self.assertIn("Yahoo split lineage does not match the CSV", rejected.stderr)
        self.assertFalse((self.research / SYMBOL).exists())

    def test_split_normalized_source_rejects_off_tick_prices_from_last_split_day(self):
        start, end = date(2021, 11, 2), date(2021, 11, 5)
        write_split_archive(self.archive, start, end, last_split_quote="41.001")
        converted = self.convert(start, end, archive_snapshot=end)
        self.assertNotEqual(converted.returncode, 0)
        self.assertIn("Source quote open on 2021-11-04", converted.stderr)
        self.assertEqual(converted.stdout, "")
        self.assertFalse(self.csv.exists())
