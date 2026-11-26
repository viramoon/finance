from datetime import date
from decimal import Decimal
from hashlib import sha256
from pathlib import Path
import runpy
from tempfile import TemporaryDirectory
import unittest
from zipfile import ZipFile

from fertile_finance.research_catalog import sessions


_INPUTS = runpy.run_path(str(
    Path(__file__).resolve().parents[1] / "strategies" / "2026-11-25-factor-momentum" / "inputs.py"
))


class FactorMomentumInputsTest(unittest.TestCase):
    def test_manifests_monthly_and_daily_are_distinct_and_missing_is_not_filled(self):
        with TemporaryDirectory() as directory:
            raw = Path(directory)
            manifest = ["downloaded_at_utc: 2026-11-26T03:02:31Z"]
            for frequency, suffix, dates in (
                ("monthly", "_CSV.zip", ("196912", "197001")),
                ("daily", "_daily_CSV.zip", ("19691231", "19700102")),
            ):
                for stem, columns, _ in _INPUTS["_ARCHIVES"]:
                    archive = raw / (stem + suffix)
                    member = stem + ".csv"
                    first = ",".join(["1.25"] * len(columns))
                    marker = "-99.99" if frequency == "monthly" else "-999"
                    second = ",".join([marker] * len(columns))
                    content = "\n".join((
                        "This file was created using the 202609 CRSP database.",
                        "," + ",".join(columns),
                        dates[0] + "," + first,
                        dates[1] + "," + second,
                        "Annual Factors:" if frequency == "monthly" else "Copyright 2026",
                        "1969,2.0" if frequency == "monthly" else "",
                    ))
                    with ZipFile(archive, "w") as output:
                        output.writestr(member, content)
                    payload = archive.read_bytes()
                    manifest.append(f"{sha256(payload).hexdigest()}  {len(payload)}  "
                                    f"{archive.name}  https://example.org/{archive.name}")
            (raw / "MANIFEST.txt").write_text("\n".join(manifest), encoding="utf-8")

            monthly, daily, provenance = _INPUTS["load_french_snapshot"](raw)
            self.assertEqual(len(provenance["files"]), 10)
            self.assertEqual(provenance["crsp_vintage"], "202609")
            self.assertEqual(monthly[date(1969, 12, 31)]["RMW"], Decimal("0.0125"))
            self.assertEqual(daily[date(1969, 12, 31)]["RF"], Decimal("0.0125"))
            self.assertIsNone(monthly[date(1970, 1, 31)]["ST_Rev"])
            self.assertIsNone(daily[date(1970, 1, 2)]["LT_Rev"])
            summary = provenance["files"]["F-F_Research_Data_Factors_daily_CSV.zip"]["series"]["RF"]
            self.assertEqual(summary, {"first": "1969-12-31", "last": "1969-12-31",
                                       "observations": 1, "missing": 1})

            tampered = bytearray(archive.read_bytes())
            tampered[-1] ^= 1
            archive.write_bytes(tampered)
            with self.assertRaisesRegex(ValueError, "differs from manifest"):
                _INPUTS["load_french_snapshot"](raw)

    def test_comparison_compounds_only_complete_months_and_validates_snapshot(self):
        with TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "sources"
            snapshot = root / "snapshot"
            all_days = list(sessions(date(2024, 1, 1), date(2024, 2, 29)))
            rows = [",strategy_ret,benchmark_ret"]
            for day in all_days:
                if day == date(2024, 1, 3):
                    continue
                change = {
                    all_days[0]: "",
                    date(2024, 2, 1): "0.10",
                    date(2024, 2, 2): "-0.05",
                }.get(day, "0")
                rows.append(f"{day} 21:00:00+00:00,{change},0.03")
            payload = ("\n".join(rows) + "\n").encode()
            for slug in _INPUTS["_COMPARISONS"].values():
                daily = source / slug / "results" / "performance_returns.csv"
                daily.parent.mkdir(parents=True)
                daily.write_bytes(payload)

            monthly, provenance = _INPUTS["load_comparisons"](snapshot, source_dir=source)
            self.assertEqual(monthly, {
                date(2024, 2, 29): dict.fromkeys(_INPUTS["_COMPARISONS"], Decimal("0.045")),
            })
            for info in provenance["sources"].values():
                self.assertEqual(info["sha256"], sha256(payload).hexdigest())
                self.assertEqual(info["missing_session_labels"], ["2024-01-03"])
                self.assertEqual(info["missing_strategy_returns"], [all_days[0].isoformat()])
                self.assertEqual(info["excluded_months"], ["2024-01-31"])
            self.assertEqual(_INPUTS["load_comparisons"](snapshot), (monthly, provenance))

            with (snapshot / "monthly_returns.csv").open("a", encoding="utf-8") as handle:
                handle.write("\n")
            with self.assertRaisesRegex(ValueError, "differs from its provenance"):
                _INPUTS["load_comparisons"](snapshot)
