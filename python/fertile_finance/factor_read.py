"""Publish reviewed F8 catalog entries and evaluation summaries for the local API."""

from collections.abc import Iterable, Mapping
from dataclasses import dataclass
from datetime import UTC, datetime
from decimal import Decimal
import json
import os
from pathlib import Path
import re
from tempfile import NamedTemporaryFile

from .factor_evaluation import FactorEvaluation
from .historical_research import load_factor_momentum_registration
from .research_catalog import RegisteredFactor, ResearchState
from .research_factors import FactorDefinition, _FACTOR_ID, _FACTOR_VERSION


SNAPSHOT_PATH = Path("finance-state/factor-research/read-v1.json")
MAX_BYTES = 4 * 1024 * 1024
_SOURCE_COMMIT = re.compile(r"[0-9a-f]{40}")


@dataclass(frozen=True)
class FactorReadPermission:
    """Reviewed permission for all inputs to this factor version."""

    license: str
    record_local: bool
    derive_non_display: bool
    redistribute: bool = False

    def _wire(self, expected_license: str | None):
        if (
            not isinstance(self.license, str)
            or not self.license.strip()
            or (expected_license is not None and self.license != expected_license)
            or self.record_local is not True
            or self.derive_non_display is not True
            or self.redistribute is not False
        ):
            raise ValueError("Factor read publication requires matching local recording and derivation permission")
        return {
            "license": self.license,
            "record_local": self.record_local,
            "derive_non_display": self.derive_non_display,
            "redistribute": self.redistribute,
        }


@dataclass(frozen=True)
class FactorReadEvaluation:
    evaluation_id: str
    report: FactorEvaluation


def _parameter_wire(value):
    if type(value) is bool:
        return {"kind": "boolean", "value": value}
    if type(value) is int:
        return {"kind": "integer", "value": str(value)}
    if isinstance(value, Decimal):
        return {"kind": "decimal", "value": _decimal_wire(value)}
    return {"kind": "text", "value": value}


def _decimal_wire(value: Decimal | None) -> str | None:
    if value is None:
        return None
    if not value.is_finite():
        raise ValueError("A published research metric must be finite")
    _, digits, exponent = value.as_tuple()
    # Check length before converting an accidentally enormous exponent to fixed-point text.
    if max(len(digits) + max(exponent, 0), -exponent + 1) + 2 > MAX_BYTES:
        raise ValueError("Research Decimal text exceeds the snapshot size limit")
    return format(value, "f")


def _permission(permissions, key, expected_license):
    permission = permissions.get(key)
    if not isinstance(permission, FactorReadPermission):
        raise ValueError("Provide reviewed read permission for each factor version and evaluation peer")
    return permission._wire(expected_license)


def _definition_key(definition: FactorDefinition) -> tuple[str, str]:
    return definition.factor_id, definition.version


def _factor_item(entry: RegisteredFactor, access):
    definition = entry.definition
    return {
        "factor_id": definition.factor_id,
        "version": definition.version,
        "state": entry.state.value,
        "access": access,
        "selectable_for_backtest": entry.state == ResearchState.BACKTEST_SELECTABLE,
        "tradable": False,
        "details": {
            "kind": "factor",
            "definition": definition.definition,
            "parameters": {name: _parameter_wire(value) for name, value in definition.parameters.items()},
            "unit": definition.unit,
            "asset_scope": list(definition.asset_scope),
            "data_version": definition.data_version,
            "availability_rule": definition.availability_rule,
            "missing_policy": definition.missing_policy,
            "warmup_sessions": str(definition.warmup_sessions),
            "history": [
                {"previous": change.previous.value, "current": change.current.value,
                 "reason": change.reason, "evidence_ref": change.evidence_ref}
                for change in entry.history
            ],
        },
    }


def _historical_item(repo_root: Path, permissions):
    root = repo_root.resolve()
    entry = load_factor_momentum_registration(root)
    if (not _FACTOR_ID.fullmatch(entry.id) or not _FACTOR_VERSION.fullmatch(entry.version)
            or not _SOURCE_COMMIT.fullmatch(entry.source_commit)):
        raise ValueError("Invalid historical research identity")
    decision = entry.decision
    return {
        "factor_id": entry.id,
        "version": entry.version,
        "state": entry.state,
        "access": _permission(permissions, (entry.id, entry.version), None),
        "selectable_for_backtest": entry.selectable_for_backtest,
        "tradable": entry.tradable,
        "details": {
            "kind": "historical_research",
            "source_commit": entry.source_commit,
            "summary_ref": entry.summary.relative_to(root).as_posix(),
            "decision": {
                "verdict": decision.verdict,
                "passed": decision.passed,
                "cost_bps": str(decision.cost_bps),
                "rule": decision.rule,
                "comparisons": {
                    name: {
                        "main_sharpe": _decimal_wire(row.main_sharpe),
                        "static_sharpe": _decimal_wire(row.static_sharpe),
                        "difference": _decimal_wire(row.difference),
                        "outperformance": row.outperformance,
                        "flip": row.flip,
                    }
                    for name, row in decision.comparisons.items()
                },
            },
        },
    }


def _evaluation_item(item: FactorReadEvaluation, registered, permissions):
    evaluation_id, report = item.evaluation_id, item.report
    if not isinstance(evaluation_id, str) or not _FACTOR_ID.fullmatch(evaluation_id):
        raise ValueError("Invalid evaluation_id")
    factor_id, version = key = _definition_key(report.definition)
    if key not in registered or registered[key].definition != report.definition:
        raise ValueError("Published evaluation must match a registered factor and data version")
    access = _permission(permissions, key, report.definition.license)
    peers = [
        {
            "factor_id": peer.factor_id,
            "version": peer.version,
            "data_version": peer.data_version,
            "access": _permission(permissions, _definition_key(peer), peer.license),
        }
        for peer in report.peer_definitions
    ]
    # Select aggregate fields before serializing so per-asset evidence never reaches the handoff.
    windows = [
        {
            "window": _window_bounds(row.window),
            "factor_coverage": _decimal_wire(row.factor_coverage),
            "scored_coverage": _decimal_wire(row.scored_coverage),
            "ic": {str(horizon): _distribution(result) for horizon, result in row.ic.items()},
            "redundancy": {name: _distribution(result) for name, result in row.redundancy.items()},
            "quantiles": [_portfolio_summary(result) for result in row.quantiles],
            "benchmark": _portfolio_summary(row.benchmark),
            "gross_information": _distribution(row.gross_information),
            "net_information": _distribution(row.net_information),
        }
        for row in report.windows
    ]
    return {
        "factor_id": factor_id,
        "version": version,
        "data_version": report.definition.data_version,
        "evaluation_id": evaluation_id,
        "access": access,
        "peer_factors": peers,
        "cost_bps": _decimal_wire(report.cost_bps),
        "quantile_count": report.quantile_count,
        "windows": windows,
    }


def _window_bounds(window):
    return {"name": window.name, "phase": window.phase,
            "start": window.start.isoformat(), "end": window.end.isoformat()}


def _distribution(result):
    return {"count": result.count, "mean": _decimal_wire(result.mean),
            "sample_std": _decimal_wire(result.sample_std), "ratio": _decimal_wire(result.ratio)}


def _portfolio_summary(result):
    return {
        "periods": result.periods,
        "gross_observations": result.gross_observations,
        "net_observations": result.net_observations,
        "turnover_observations": result.turnover_observations,
        "cumulative_gross_return": _decimal_wire(result.cumulative_gross_return),
        "cumulative_net_return": _decimal_wire(result.cumulative_net_return),
        "total_turnover": _decimal_wire(result.total_turnover),
        "total_fee_fraction": _decimal_wire(result.total_fee_fraction),
    }


def publish_factor_read_snapshot(
    data_root: Path,
    *,
    as_of: datetime,
    permissions: Mapping[tuple[str, str], FactorReadPermission],
    factors: Iterable[RegisteredFactor] = (),
    evaluations: Iterable[FactorReadEvaluation] = (),
    historical_repo_root: Path | None = None,
) -> Path:
    """Write one complete read snapshot from reviewed offline outputs under an explicit data root."""
    if not data_root.is_absolute() or not data_root.is_dir():
        raise ValueError("Use an existing absolute data root")
    if not isinstance(as_of, datetime) or as_of.utcoffset() is None:
        raise ValueError("as_of must be a timezone-aware publication timestamp")

    registered = {}
    factor_items = {}
    for entry in factors:
        key = _definition_key(entry.definition)
        if key in registered:
            raise ValueError("Duplicate factor version")
        registered[key] = entry
        factor_items[key] = _factor_item(entry, _permission(permissions, key, entry.definition.license))
    if historical_repo_root is not None:
        historical = _historical_item(historical_repo_root, permissions)
        key = historical["factor_id"], historical["version"]
        if key in factor_items:
            raise ValueError("Historical registration conflicts with a factor version")
        factor_items[key] = historical

    evaluation_items = {}
    for entry in evaluations:
        item = _evaluation_item(entry, registered, permissions)
        key = item["factor_id"], item["version"], item["evaluation_id"]
        if key in evaluation_items:
            raise ValueError("Duplicate evaluation ID for a factor version")
        evaluation_items[key] = item

    payload = (json.dumps({
        "schema_version": 1,
        "as_of": as_of.astimezone(UTC).isoformat(),
        "factors": [factor_items[key] for key in sorted(factor_items)],
        "evaluations": [evaluation_items[key] for key in sorted(evaluation_items)],
    }, ensure_ascii=False, allow_nan=False, sort_keys=True, separators=(",", ":")) + "\n").encode()
    if len(payload) > MAX_BYTES:
        raise ValueError("Factor read snapshot exceeds 4 MiB")

    path = data_root / SNAPSHOT_PATH
    path.parent.mkdir(parents=True, exist_ok=True)
    with NamedTemporaryFile(dir=path.parent, prefix=".read-v1-", delete_on_close=False) as pending:
        pending.write(payload)
        pending.flush()
        os.fsync(pending.fileno())
        pending.close()
        os.replace(pending.name, path)
    return path
