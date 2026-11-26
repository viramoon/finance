"""Synthetic point-in-time records. All releases and scores are hand-constructed."""

from dataclasses import replace
from datetime import UTC, datetime, timedelta, timezone
from decimal import Decimal
import unittest

from fertile_finance.research_factors import (
    FactorDefinition,
    FactorLookaheadError,
    FactorObservation,
    PointInTimeFactor,
    UniverseMembership,
    validate_factor_snapshot,
)


def instant(day, hour=12):
    return datetime(2024, 1, day, hour, tzinfo=UTC)


def definition(**changes):
    return replace(FactorDefinition(
        factor_id="synthetic.disclosure",
        version="1.0.0",
        definition="Synthetic value published in an invented disclosure",
        parameters={"window": 2},
        unit="ratio",
        asset_scope=("stock",),
        data_version="synthetic-2024-v1",
        availability_rule="Recorded first disclosure or revision timestamp",
        missing_policy="exclude",
        warmup_sessions=0,
        license="Synthetic internal test input",
    ), **changes)


def member(asset_id, *, effective=1, available=1, is_member=True, industry=None, size=None):
    return UniverseMembership(asset_id, "stock", instant(effective), instant(available),
                              is_member, industry, size)


class FactorPitTest(unittest.TestCase):
    def test_explicit_snapshot_rejects_future_disclosure_and_accepts_exact_boundary(self):
        disclosure = FactorObservation("A", instant(1), instant(3), Decimal("0.1234567890123456789"))
        members = [member("A")]
        with self.assertRaisesRegex(FactorLookaheadError, "first_available_ts follows decision_ts"):
            validate_factor_snapshot(definition(), instant(3, 11), [disclosure], members)

        same_instant = instant(3).astimezone(timezone(timedelta(hours=-5)))
        snapshot = validate_factor_snapshot(definition(), same_instant, [disclosure], members)
        self.assertEqual(snapshot.decision_ts, instant(3))
        self.assertEqual(snapshot.rows[0].value, Decimal("0.1234567890123456789"))
        self.assertEqual(snapshot.rows[0].first_available_ts, instant(3))

    def test_replay_does_not_backfill_disclosures_or_revisions(self):
        initial = FactorObservation("A", instant(1), instant(3), Decimal("1.1"))
        revision = replace(initial, first_available_ts=instant(5), value=Decimal("8.8"))
        next_period = FactorObservation("A", instant(2), instant(4), Decimal("2.2"))
        factor = PointInTimeFactor(definition(), [revision, next_period, initial], [member("A")])
        before_publication = factor.snapshot(instant(2)).rows[0]
        self.assertIsNone(before_publication.value)
        self.assertEqual(before_publication.missing_reason, "missing_observation")
        self.assertIsNone(before_publication.observation_ts)
        self.assertEqual(factor.snapshot(instant(3)).rows[0].value, Decimal("1.1"))
        self.assertEqual(factor.snapshot(instant(4)).rows[0].value, Decimal("2.2"))
        self.assertEqual(factor.snapshot(instant(5)).rows[0].value, Decimal("2.2"))

        same_period = PointInTimeFactor(definition(), [revision, initial], [member("A")])
        self.assertEqual(same_period.snapshot(instant(4)).rows[0].value, Decimal("1.1"))
        self.assertEqual(same_period.snapshot(instant(5)).rows[0].value, Decimal("8.8"))
        with self.assertRaises(FactorLookaheadError):
            validate_factor_snapshot(definition(), instant(4), [initial, revision], [member("A")])

    def test_historical_membership_and_exposures_require_publication_and_effectiveness(self):
        first = member("A", industry="synthetic-tech", size=Decimal("1.23"))
        revise = replace(first, first_available_ts=instant(2), industry="synthetic-energy", size=Decimal("9.87"))
        remove = member("A", effective=3, available=4, is_member=False)
        advance_notice = member("B", effective=3, available=2)
        archive = PointInTimeFactor(definition(), [], [remove, advance_notice, revise, first])

        row, = archive.snapshot(instant(1)).rows
        self.assertEqual((row.asset_id, row.industry, row.size), ("A", "synthetic-tech", Decimal("1.23")))
        row, = archive.snapshot(instant(2)).rows
        self.assertEqual((row.asset_id, row.industry, row.size), ("A", "synthetic-energy", Decimal("9.87")))
        self.assertEqual([row.asset_id for row in archive.snapshot(instant(3)).rows], ["A", "B"])
        self.assertEqual([row.asset_id for row in archive.snapshot(instant(4)).rows], ["B"])
        for event, cutoff in ((remove, instant(3)), (advance_notice, instant(2)), (revise, instant(1))):
            with self.subTest(event=event), self.assertRaisesRegex(FactorLookaheadError, "membership"):
                validate_factor_snapshot(definition(), cutoff, [], [event])

    def test_exclusion_keeps_missing_and_unwarmed_members_in_coverage_denominator(self):
        observations = [
            FactorObservation("A", instant(1), instant(1), Decimal("4"), history_sessions=2,
                              valid_until_ts=instant(3)),
            FactorObservation("B", instant(1), instant(1), Decimal("1"), history_sessions=1),
            FactorObservation("C", instant(1), instant(1), Decimal("7"), history_sessions=2),
            FactorObservation("C", instant(2), instant(2), None, history_sessions=2),
        ]
        factor = PointInTimeFactor(definition(warmup_sessions=2), observations,
                                   [member(asset) for asset in "DCBA"])
        self.assertEqual([row.value for row in factor.snapshot(instant(1)).rows],
                         [Decimal("4"), None, Decimal("7"), None])
        self.assertEqual([(row.asset_id, row.value, row.missing_reason) for row in factor.snapshot(instant(3)).rows], [
            ("A", None, "expired_observation"),
            ("B", None, "insufficient_warmup"),
            ("C", None, "missing_value"),
            ("D", None, "missing_observation"),
        ])

    def test_version_metadata_is_copied_and_rejects_invalid_declarations(self):
        parameters = {"window": 2, "scale": Decimal("1.250")}
        published = definition(parameters=parameters)
        parameters["window"] = 5
        self.assertEqual(published.parameters, {"window": 2, "scale": Decimal("1.250")})
        self.assertEqual(published.parameters["scale"].as_tuple(), Decimal("1.250").as_tuple())
        with self.assertRaises(TypeError):
            published.parameters["window"] = 10
        self.assertNotEqual(definition(parameters={"window": True}), definition(parameters={"window": 1}))
        self.assertNotEqual(definition(parameters={"window": 1}), definition(parameters={"window": Decimal(1)}))
        for changes in ({"version": "01.0.0"}, {"license": ""}, {"warmup_sessions": True},
                        {"asset_scope": ()}, {"availability_rule": " "}, {"missing_policy": "impute"},
                        {"parameters": {"scale": 1.25}}, {"parameters": {"scale": Decimal("NaN")}}):
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                definition(**changes)

    def test_rejects_invalid_timestamps_inexact_values_scope_and_duplicate_vintages(self):
        with self.assertRaisesRegex(ValueError, "timezone-aware"):
            FactorObservation("A", instant(1), datetime(2024, 1, 1), Decimal("1"))
        with self.assertRaisesRegex(ValueError, "predates observation_ts"):
            FactorObservation("A", instant(2), instant(1), Decimal("1"))
        with self.assertRaisesRegex(ValueError, "finite Decimal"):
            FactorObservation("A", instant(1), instant(1), 1.1)
        with self.assertRaisesRegex(ValueError, "size must be positive"):
            member("A", size=Decimal("0"))
        with self.assertRaisesRegex(ValueError, "outside the factor asset_scope"):
            PointInTimeFactor(definition(), [], [replace(member("A"), asset_type="etf")])

        original = FactorObservation("A", instant(1), instant(1), Decimal("1"))
        same_availability = instant(1).astimezone(timezone(timedelta(hours=-5)))
        conflict = replace(original, first_available_ts=same_availability, value=Decimal("2"))
        with self.assertRaisesRegex(ValueError, "Duplicate FactorObservation vintage"):
            PointInTimeFactor(definition(), [original, conflict], [])
        original_member = member("A")
        with self.assertRaisesRegex(ValueError, "Duplicate UniverseMembership vintage"):
            PointInTimeFactor(definition(), [], [original_member, replace(original_member, is_member=False)])
