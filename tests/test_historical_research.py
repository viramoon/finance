from decimal import Decimal, localcontext
import json
from pathlib import Path
from tempfile import TemporaryDirectory
import unittest

from fertile_finance.historical_research import load_factor_momentum_registration
from fertile_finance.research_decimal import RESEARCH_CONTEXT


ROOT = Path(__file__).resolve().parents[1]


class HistoricalResearchTest(unittest.TestCase):
    def test_monthly_factor_momentum_replays_archived_gate_as_research_only(self):
        entry = load_factor_momentum_registration(ROOT)
        self.assertEqual((entry.id, entry.version, entry.state),
                         ("research.factor-momentum", "1.0.0", "research_only"))
        self.assertFalse(entry.selectable_for_backtest)
        self.assertFalse(entry.tradable)
        self.assertEqual(entry.source_commit, "5537ce087d8705dca97d852cb64afc4fd26448f2")
        self.assertEqual(entry.directory, ROOT / "strategies/2026-11-25-factor-momentum")
        self.assertEqual(entry.summary, entry.directory / "results/summary.json")

        decision = entry.decision
        self.assertFalse(decision.passed)
        self.assertEqual(decision.verdict, "不通过")
        self.assertEqual(decision.cost_bps, 25)
        self.assertEqual(decision.rule,
                         "strictly positive Sharpe spread in longest_sample and both OOS; OOS spread <= 0 is a flip")
        self.assertEqual(set(decision.comparisons), {"longest_sample", "oos_jf_2022", "oos_nber_2019"})
        longest = decision.comparisons["longest_sample"]
        self.assertEqual(longest.main_sharpe, Decimal("0.4023902204799621891543994378727295"))
        self.assertEqual(longest.static_sharpe, Decimal("0.7679979689675887478675155252325802"))
        self.assertEqual((format(longest.main_sharpe, ".4f"), format(longest.static_sharpe, ".4f")),
                         ("0.4024", "0.7680"))
        self.assertFalse(longest.outperformance)
        self.assertIsNone(longest.flip)
        for name, comparison in decision.comparisons.items():
            with localcontext(RESEARCH_CONTEXT):
                spread = comparison.main_sharpe - comparison.static_sharpe
            self.assertEqual(comparison.difference, spread)
            self.assertEqual(comparison.outperformance, spread > 0)
            if name != "longest_sample":
                self.assertGreater(spread, 0)
                self.assertFalse(comparison.flip)

    def test_edited_summary_fails_its_replay_hash(self):
        registration_path = Path("strategies/registry/factor-momentum-v1.json")
        registration = json.loads((ROOT / registration_path).read_bytes())
        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            summary = root / registration["directory"] / registration["summary"]
            summary.parent.mkdir(parents=True)
            summary.write_text('{"decision":{"passed":true}}', encoding="utf-8")
            registration["artifacts_sha256"] = {
                registration["summary"]: registration["artifacts_sha256"][registration["summary"]],
            }
            manifest = root / registration_path
            manifest.parent.mkdir(parents=True)
            manifest.write_text(json.dumps(registration), encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "Historical evidence hash mismatch"):
                load_factor_momentum_registration(root)
