from datetime import date
from decimal import Decimal
import json
from pathlib import Path
import tempfile
import unittest
from zipfile import ZipFile

from fertile_finance.research_factors import (
    FredObservation,
    FrenchFactor,
    FrenchRF,
    load_fred,
    load_ken_french,
    load_ken_french_rf,
)


FIXTURES = Path(__file__).parent / "fixtures" / "historical"
FRENCH_FACTORS = FIXTURES / "french_daily.csv"
FRENCH_MOMENTUM = FIXTURES / "french_momentum_daily.csv"
FRED_CSV = FIXTURES / "fred_dgs10_daily.csv"
FRED_JSON = FIXTURES / "fred_dgs10_observations.json"


class ResearchFactorsTest(unittest.TestCase):
    def test_french_daily_factors_and_momentum_keep_units_and_gaps(self):
        result = load_ken_french(FRENCH_FACTORS, momentum_path=FRENCH_MOMENTUM, pre_epoch="drop")
        self.assertEqual((result.dropped_pre_epoch.count, result.dropped_pre_epoch.first,
                          result.dropped_pre_epoch.last), (1, date(1969, 12, 31), date(1969, 12, 31)))
        self.assertEqual(len(result.rows), 4)
        self.assertEqual(result.rows[1], FrenchFactor(date(2024, 7, 3), Decimal("0.0123"), Decimal("-0.0023"),
                                                     Decimal("0.0000"), Decimal("0.0002"), Decimal("-0.0017")))
        self.assertIsNone(result.rows[2].smb)
        self.assertIsNone(result.rows[2].rf)
        self.assertIsNone(result.rows[2].mom)
        self.assertEqual(result.rows[3], FrenchFactor(date(2024, 7, 8), None, None, None, None, Decimal("0.0033")))

    def test_french_rf_has_no_invented_publication_date(self):
        result = load_ken_french_rf(FRENCH_FACTORS, pre_epoch="drop")
        self.assertEqual(result.rows, (
            FrenchRF(date(1970, 1, 2), Decimal("0.0003"), None),
            FrenchRF(date(2024, 7, 3), Decimal("0.0002"), None),
            FrenchRF(date(2024, 7, 5), None, None),
        ))
        self.assertEqual(result.dropped_pre_epoch.count, 1)
        with self.assertRaises(ValueError):
            load_ken_french_rf(FRENCH_FACTORS)

    def test_french_daily_zip_and_extracted_csv_read_identically(self):
        with tempfile.TemporaryDirectory(prefix="french-archive-") as directory:
            archive_path = Path(directory) / "F-F_Research_Data_Factors_daily_CSV.zip"
            with ZipFile(archive_path, "w") as archive:
                archive.write(FRENCH_FACTORS, "F-F_Research_Data_Factors_daily.CSV")
            self.assertEqual(load_ken_french(archive_path, pre_epoch="drop"),
                             load_ken_french(FRENCH_FACTORS, pre_epoch="drop"))

    def test_french_rejects_monthly_rows_with_daily_header(self):
        with tempfile.TemporaryDirectory(prefix="french-monthly-") as directory:
            path = Path(directory) / "monthly.csv"
            path.write_text(",Mkt-RF,SMB,HML,RF\n202407,1.23,-0.23,0.00,0.02\n", encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "daily data with YYYYMMDD dates"):
                load_ken_french(path)

    def test_fred_graph_csv_preserves_missing_and_reports_epoch_drop(self):
        with self.assertRaises(ValueError):
            load_fred(FRED_CSV, series_id="DGS10")
        result = load_fred(FRED_CSV, series_id="DGS10", pre_epoch="drop")
        self.assertEqual(result.series_id, "DGS10")
        self.assertEqual(result.rows, (
            FredObservation(date(1970, 1, 2), Decimal("7.23")),
            FredObservation(date(2024, 7, 3), Decimal("4.36")),
            FredObservation(date(2024, 7, 4), None),
        ))
        self.assertEqual((result.dropped_pre_epoch.count, result.dropped_pre_epoch.first,
                          result.dropped_pre_epoch.last), (1, date(1969, 12, 31), date(1969, 12, 31)))
        with self.assertRaisesRegex(ValueError, "CSV for SOFR"):
            load_fred(FRED_CSV, series_id="SOFR", pre_epoch="drop")

    def test_fred_api_preserves_exact_native_values_and_vintage_bounds(self):
        result = load_fred(FRED_JSON, series_id="DGS10")
        self.assertEqual(result.dropped_pre_epoch.count, 0)
        self.assertEqual(result.rows[0], FredObservation(date(2024, 7, 3), Decimal("4.12345678901234567890123456789"),
                                                        date(2024, 7, 10), date(2024, 7, 10)))
        self.assertIsNone(result.rows[1].value)
        self.assertEqual(result.rows[2].value.as_tuple(), Decimal("4.3900").as_tuple())

    def test_fred_api_rejects_multiple_vintages_for_the_same_observation(self):
        payload = json.loads(FRED_JSON.read_text(encoding="utf-8"))
        revised = dict(payload["observations"][0], realtime_start="2024-07-12", realtime_end="2024-07-12", value="4.17")
        payload["observations"].append(revised)
        with tempfile.TemporaryDirectory(prefix="fred-vintages-") as directory:
            path = Path(directory) / "multiple.json"
            path.write_text(json.dumps(payload), encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "one vintage per observation date"):
                load_fred(path, series_id="DGS10")


if __name__ == "__main__":
    unittest.main()
