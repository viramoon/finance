"""Exercise the F10a handoff with fixed synthetic F8 outputs."""

from dataclasses import replace
from datetime import UTC, datetime
from decimal import Decimal
from hashlib import sha256
import json
from pathlib import Path
from tempfile import TemporaryDirectory
import unittest

from fertile_finance.factor_evaluation import Distribution
from fertile_finance.factor_read import (
    SNAPSHOT_PATH,
    FactorReadEvaluation,
    FactorReadPermission,
    publish_factor_read_snapshot,
)
from fertile_finance.research_catalog import FactorResearchCatalog, ResearchState
from test_factor_research_catalog import definition, report_for


AS_OF = datetime(2026, 2, 14, 16, tzinfo=UTC)
PRECISE = Decimal("0.1234567890123456789012345678901234")


def synthetic_history(repo_root):
    """Create a temporary fixed registration and matching aggregate report hash."""
    relative = Path("strategies/synthetic-momentum/results/summary.json")
    summary = repo_root / relative
    summary.parent.mkdir(parents=True)
    summary.write_text(json.dumps({
        "code": {"commit": "0123456789abcdef0123456789abcdef01234567"},
        "decision": {
            "verdict": "synthetic gate failed", "passed": False,
            "cost_bps_each_traded_dollar_both_strategies": 25, "rule": "strictly positive spread",
            "comparisons": {"longest_sample": {
                "main_sharpe": str(PRECISE), "static_sharpe": "1.00000",
                "difference": "-0.8765432109876543210987654321098766", "outperformance": False, "flip": None,
            }},
        },
    }), encoding="utf-8")
    manifest = repo_root / "strategies/registry/factor-momentum-v1.json"
    manifest.parent.mkdir(parents=True)
    manifest.write_text(json.dumps({
        "id": "research.factor-momentum", "version": "1.0.0", "state": "research_only",
        "directory": "strategies/synthetic-momentum", "summary": "results/summary.json",
        "artifacts_sha256": {"results/summary.json": sha256(summary.read_bytes()).hexdigest()},
    }), encoding="utf-8")
    return summary, relative.as_posix()


class FactorReadSnapshotTest(unittest.TestCase):
    def setUp(self):
        self.tmp = TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.definition = replace(definition(), parameters={
            "decimal": PRECISE,
            "large": 18446744073709551615,
            "flag": False,
            "text": "0.123",
        })
        self.key = self.definition.factor_id, self.definition.version
        self.permissions = {self.key: FactorReadPermission(self.definition.license, True, True)}
        catalog = FactorResearchCatalog()
        catalog.register(self.definition)
        self.entry = catalog.transition(*self.key, ResearchState.OUT_OF_SAMPLE_VALIDATION,
                                        reason="Synthetic review", evidence_ref="test/review/v1")
        self.report = replace(report_for(self.definition), cost_bps=PRECISE)

    def publish(self, **options):
        return publish_factor_read_snapshot(self.root, **{
            "as_of": AS_OF, "permissions": self.permissions, "factors": [self.entry],
            "evaluations": [FactorReadEvaluation("fit-1", self.report)], **options,
        })

    def test_snapshot_preserves_f8_metadata_precision_and_aggregate_metrics(self):
        path = self.publish()
        self.assertEqual(path, self.root / SNAPSHOT_PATH)
        first = path.read_bytes()
        self.assertEqual(self.publish().read_bytes(), first)
        wire = json.loads(first)
        self.assertEqual(wire["schema_version"], 1)
        self.assertEqual(wire["as_of"], AS_OF.isoformat())
        item, = wire["factors"]
        self.assertEqual(item["state"], "out_of_sample_validation")
        self.assertFalse(item["selectable_for_backtest"])
        self.assertEqual(item["details"]["history"][0]["evidence_ref"], "test/review/v1")
        self.assertEqual(item["details"]["parameters"], {
            "decimal": {"kind": "decimal", "value": str(PRECISE)},
            "large": {"kind": "integer", "value": "18446744073709551615"},
            "flag": {"kind": "boolean", "value": False},
            "text": {"kind": "text", "value": "0.123"},
        })
        self.assertEqual(item["access"]["license"], self.definition.license)
        self.assertFalse(item["access"]["redistribute"])
        evaluation, = wire["evaluations"]
        self.assertEqual(evaluation["access"], item["access"])
        self.assertEqual(evaluation["cost_bps"], str(PRECISE))
        source_window = self.report.as_dict()["windows"][-1]
        window = evaluation["windows"][-1]
        for field in ("window", "ic", "scored_coverage", "quantiles", "benchmark", "net_information"):
            self.assertEqual(window[field], source_window[field])
        self.assertNotIn("periods", window)
        self.assertNotIn(b"SYN00", first)

    def test_disallowed_mismatched_and_duplicate_exports_leave_snapshot_intact(self):
        denied = [
            {},
            {self.key: FactorReadPermission(self.definition.license, False, False)},
            {self.key: FactorReadPermission(self.definition.license, True, False)},
            {self.key: FactorReadPermission(self.definition.license, True, True, redistribute=True)},
            {self.key: FactorReadPermission("wrong source", True, True)},
        ]
        for permissions in denied:
            with self.assertRaisesRegex(ValueError, "permission"):
                self.publish(permissions=permissions)
            self.assertEqual(list(self.root.iterdir()), [])
        path = self.publish()
        original = path.read_bytes()
        incompatible = replace(self.definition, data_version="different")
        peer = replace(self.definition, factor_id="synthetic.peer")
        for evaluations, message in [
            ([FactorReadEvaluation("fit-1", replace(self.report, definition=incompatible))], "registered"),
            ([FactorReadEvaluation("fit-1", replace(self.report, peer_definitions=(peer,)))], "permission"),
            ([FactorReadEvaluation("fit-1", self.report)] * 2, "Duplicate evaluation"),
            ([FactorReadEvaluation("../fit-1", self.report)], "evaluation_id"),
            ([FactorReadEvaluation("fit-1", replace(self.report, cost_bps=Decimal("1e-999999999")))], "size limit"),
        ]:
            with self.assertRaisesRegex(ValueError, message):
                self.publish(evaluations=evaluations)
            self.assertEqual(path.read_bytes(), original)
            self.assertEqual(list(path.parent.iterdir()), [path])

    def test_redundancy_requires_the_exact_declared_and_authorized_peer(self):
        peer = replace(self.definition, factor_id="display.only", data_version="synthetic-peer-data")
        peer_key = (peer.factor_id, peer.version)
        statistic_key = "@".join(peer_key)
        statistic = Distribution(count=1, mean=Decimal("-1.0000"), sample_std=None, ratio=None)
        last_window = replace(self.report.windows[-1], redundancy={statistic_key: statistic})
        report = replace(self.report, peer_definitions=(peer,), windows=(*self.report.windows[:-1], last_window))
        denied = {**self.permissions, peer_key: FactorReadPermission(peer.license, False, False)}
        allowed = {**self.permissions, peer_key: FactorReadPermission(peer.license, True, True)}
        other_version = replace(peer, version="2.0.0")
        other_key = (other_version.factor_id, other_version.version)
        cases = (
            ("display only", report, denied, "permission"),
            ("missing declaration", replace(report, peer_definitions=()), denied, "declared evaluation peer"),
            ("permission alone", replace(report, peer_definitions=()), allowed, "declared evaluation peer"),
            ("wrong declared version", replace(report, peer_definitions=(other_version,)),
             {**allowed, other_key: allowed[peer_key]}, "declared evaluation peer"),
            ("duplicate declaration", replace(report, peer_definitions=(peer, peer)), allowed, "Duplicate evaluation peer"),
        )
        path = self.root / SNAPSHOT_PATH
        for label, invalid, permissions, error in cases:
            with self.subTest(label=label, snapshot_present=False):
                with self.assertRaisesRegex(ValueError, error):
                    self.publish(permissions=permissions, evaluations=[FactorReadEvaluation("fit-1", invalid)])
                self.assertEqual(list(self.root.iterdir()), [])

        original = self.publish(permissions=allowed, evaluations=[FactorReadEvaluation("fit-1", report)]).read_bytes()
        evaluation, = json.loads(original)["evaluations"]
        published_peer, = evaluation["peer_factors"]
        self.assertEqual((published_peer["factor_id"], published_peer["version"]), peer_key)
        self.assertEqual(published_peer["data_version"], peer.data_version)
        self.assertEqual(published_peer["access"], {
            "license": peer.license, "record_local": True, "derive_non_display": True, "redistribute": False,
        })
        self.assertEqual(evaluation["windows"][-1]["redundancy"][statistic_key]["mean"], "-1.0000")

        for label, invalid, permissions, error in cases:
            with self.subTest(label=label, snapshot_present=True):
                with self.assertRaisesRegex(ValueError, error):
                    self.publish(permissions=permissions, evaluations=[FactorReadEvaluation("fit-1", invalid)])
                self.assertEqual(path.read_bytes(), original)
                self.assertEqual(list(path.parent.iterdir()), [path])

    def test_snapshot_publishes_historical_verdict_and_reference_without_artifacts(self):
        repo_root = self.root / "fixture-repo"
        summary, summary_ref = synthetic_history(repo_root)
        permissions = {**self.permissions, ("research.factor-momentum", "1.0.0"):
                       FactorReadPermission("Synthetic historical aggregates", True, True)}
        path = self.publish(historical_repo_root=repo_root, permissions=permissions)
        original = path.read_bytes()
        historical = json.loads(original)["factors"][-1]
        self.assertEqual(historical["state"], "research_only")
        self.assertFalse(historical["selectable_for_backtest"])
        self.assertFalse(historical["tradable"])
        self.assertEqual(historical["details"]["summary_ref"], summary_ref)
        self.assertEqual(historical["details"]["decision"]["verdict"], "synthetic gate failed")
        self.assertEqual(historical["details"]["decision"]["comparisons"]["longest_sample"]["main_sharpe"], str(PRECISE))
        self.assertNotIn(str(repo_root).encode(), original)
        summary.write_text("changed", encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "hash mismatch"):
            self.publish(historical_repo_root=repo_root, permissions=permissions)
        self.assertEqual(path.read_bytes(), original)
