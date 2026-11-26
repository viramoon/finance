from dataclasses import replace
from datetime import datetime, timedelta, timezone
from decimal import Decimal
import unittest

from fertile_finance.factor_evaluation import EvaluationSession, EvaluationWindow, evaluate_factor
from fertile_finance.research_catalog import FactorResearchCatalog, ResearchError, ResearchState
from fertile_finance.research_factors import FactorDefinition, FactorObservation, PointInTimeFactor, UniverseMembership


def definition():
    return FactorDefinition(
        factor_id="fixture.review", version="1.0.0", definition="Synthetic ordinal score",
        parameters={}, unit="score", asset_scope=("cash_equity",), data_version="formula-v1",
        availability_rule="Fixed release before first session", missing_policy="exclude",
        warmup_sessions=0, license="Synthetic test fixture",
    )


def report_for(factor, *, breadth=10, flat=False):
    """Seed-free synthetic scores and closes without source data or a data directory."""
    start = datetime(2026, 1, 5, 21, tzinfo=timezone.utc)
    assets = [f"SYN{i:02}.XNYS" for i in range(breadth)]
    published = start - timedelta(days=1)
    pit = PointInTimeFactor(
        factor,
        [FactorObservation(asset, published, published, Decimal(i)) for i, asset in enumerate(assets)],
        [UniverseMembership(asset, "cash_equity", published, published, True) for asset in assets],
    )
    sessions = []
    for offset in (0, 1, 2, 3, 4, 7):
        time = start + timedelta(days=offset)
        closes = {asset: Decimal(100 + (0 if flat else i * offset)) for i, asset in enumerate(assets)}
        sessions.append(EvaluationSession(time.date(), time, closes))
    windows = [
        EvaluationWindow(name, phase, sessions[index].session, sessions[index + 1].session)
        for index, name, phase in [(0, "fit", "train"), (2, "validate", "validation"), (4, "roll-1", "out_of_sample")]
    ]
    return evaluate_factor(pit, sessions, windows, cost_bps=Decimal(25), quantiles=2)


class FactorResearchCatalogTests(unittest.TestCase):
    def setUp(self):
        self.catalog = FactorResearchCatalog()
        self.factor = definition()
        self.key = self.factor.factor_id, self.factor.version
        self.catalog.register(self.factor)

    def move(self, state, **options):
        return self.catalog.transition(*self.key, state, reason="Reviewed fixture", **options)

    def begin_validation(self):
        return self.move(ResearchState.OUT_OF_SAMPLE_VALIDATION, evidence_ref="review/protocol/v1")

    def assert_research_error(self, code, function, *args, **kwargs):
        with self.assertRaises(ResearchError) as error:
            function(*args, **kwargs)
        self.assertEqual(error.exception.code, code)

    def test_version_binding_is_idempotent_and_new_versions_start_in_research(self):
        validation = self.begin_validation()
        self.assertIs(self.catalog.register(definition()), validation)
        changed = replace(self.factor, data_version="formula-v2")
        self.assert_research_error("factor_version_conflict", self.catalog.register, changed)
        newer = self.catalog.register(replace(changed, version="1.1.0"))
        self.assertEqual(newer.state, ResearchState.RESEARCH)
        self.assertEqual(newer.history, ())
        self.assertIs(self.catalog.get(*self.key), validation)

    def test_forward_states_require_review_and_backtest_selection_is_revocable(self):
        self.assert_research_error("invalid_research_transition", self.move, ResearchState.BACKTEST_SELECTABLE)
        self.assert_research_error("invalid_research_evidence", self.move, ResearchState.OUT_OF_SAMPLE_VALIDATION)
        evaluation = report_for(self.factor)
        self.begin_validation()
        self.move(ResearchState.OBSERVE_ONLY, evidence_ref="review/holdout/v1", report=evaluation)
        self.assert_research_error("factor_not_selectable", self.catalog.select_for_backtest, *self.key)
        approved = self.move(ResearchState.BACKTEST_SELECTABLE, evidence_ref="review/approval/v1", report=evaluation)
        self.assertEqual(self.catalog.select_for_backtest(*self.key), self.factor)
        disabled = self.move(ResearchState.OBSERVE_ONLY)
        self.assert_research_error("factor_not_selectable", self.catalog.select_for_backtest, *self.key)
        self.assertEqual(disabled.history[:-1], approved.history)
        self.assertEqual(disabled.history[1].evidence_ref, "review/holdout/v1")
        self.assertIs(self.catalog.get(*self.key).definition, evaluation.definition)

    def test_only_usable_oos_reports_for_the_same_definition_are_evidence(self):
        self.begin_validation()
        different = report_for(replace(self.factor, data_version="formula-v2"))
        flat = report_for(self.factor, flat=True)
        result = report_for(self.factor)
        in_sample_only = replace(result, windows=result.windows[:2])
        for invalid_report in (None, different, flat, in_sample_only):
            with self.subTest(report=type(invalid_report)):
                self.assert_research_error(
                    "invalid_research_evidence", self.move, ResearchState.OBSERVE_ONLY,
                    evidence_ref="review/holdout/v1", report=invalid_report,
                )
        self.assertEqual(self.catalog.get(*self.key).state, ResearchState.OUT_OF_SAMPLE_VALIDATION)

    def test_nine_assets_can_be_observed_but_cannot_approve_backtest_selection(self):
        self.begin_validation()
        nine = report_for(self.factor, breadth=9)
        self.move(ResearchState.OBSERVE_ONLY, evidence_ref="review/holdout/v1", report=nine)
        self.assert_research_error(
            "insufficient_research_breadth", self.move, ResearchState.BACKTEST_SELECTABLE,
            evidence_ref="review/approval/v1", report=nine,
        )
        self.assert_research_error("factor_not_selectable", self.catalog.select_for_backtest, *self.key)

    def test_validation_can_return_to_research_without_losing_review_history(self):
        self.begin_validation()
        current = self.move(ResearchState.RESEARCH)
        self.assertEqual(current.history[-1].previous, ResearchState.OUT_OF_SAMPLE_VALIDATION)
        self.assertEqual(current.history[0].evidence_ref, "review/protocol/v1")
        self.assert_research_error("unknown_factor_version", self.catalog.select_for_backtest, self.key[0], "2.0.0")
