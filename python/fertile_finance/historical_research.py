"""Read the fixed monthly factor-momentum registration and its original verdict."""

from dataclasses import dataclass
from decimal import Decimal
from hashlib import file_digest, sha256
import json
from pathlib import Path
from types import MappingProxyType
from typing import Literal, Mapping


_REGISTRATION = "strategies/registry/factor-momentum-v1.json"


@dataclass(frozen=True)
class SharpeComparison:
    main_sharpe: Decimal | None
    static_sharpe: Decimal | None
    difference: Decimal | None
    outperformance: bool | None
    flip: bool | None


@dataclass(frozen=True)
class HistoricalDecision:
    verdict: str
    passed: bool
    cost_bps: int
    rule: str
    comparisons: Mapping[str, SharpeComparison]


@dataclass(frozen=True)
class HistoricalResearch:
    id: str
    version: str
    state: Literal["research_only"]
    directory: Path
    summary: Path
    source_commit: str
    decision: HistoricalDecision

    @property
    def selectable_for_backtest(self) -> Literal[False]:
        return False

    @property
    def tradable(self) -> Literal[False]:
        return False


def _relative_path(root: Path, name: str) -> Path:
    relative = Path(name)
    path = (root / relative).resolve()
    if relative.is_absolute() or not path.is_relative_to(root) or path == root:
        raise ValueError("Historical evidence paths must stay inside their registered directory")
    return path


def _optional_decimal(value: str | None) -> Decimal | None:
    return None if value is None else Decimal(value)


def load_factor_momentum_registration(repo_root: Path) -> HistoricalResearch:
    """Verify the repository's replay artifacts and return the decision at source precision."""
    root = repo_root.resolve()
    registration = json.loads(_relative_path(root, _REGISTRATION).read_bytes())
    if registration["state"] != "research_only":
        raise ValueError("The historical factor-momentum registration must be research_only")

    directory = _relative_path(root, registration["directory"])
    summary_name = registration["summary"]
    summary_path = _relative_path(directory, summary_name)
    artifacts = registration["artifacts_sha256"]
    if summary_name not in artifacts:
        raise ValueError("The original historical summary must have a registered hash")
    summary_content = summary_path.read_bytes()
    for name, expected in artifacts.items():
        if name == summary_name:
            actual = sha256(summary_content).hexdigest()
        else:
            with _relative_path(directory, name).open("rb") as stream:
                actual = file_digest(stream, "sha256").hexdigest()
        if actual != expected:
            raise ValueError(f"Historical evidence hash mismatch: {name}")

    summary = json.loads(summary_content)
    decision = summary["decision"]
    comparisons = {
        window: SharpeComparison(
            main_sharpe=_optional_decimal(comparison["main_sharpe"]),
            static_sharpe=_optional_decimal(comparison["static_sharpe"]),
            difference=_optional_decimal(comparison["difference"]),
            outperformance=comparison["outperformance"],
            flip=comparison["flip"],
        )
        for window, comparison in decision["comparisons"].items()
    }
    return HistoricalResearch(
        id=registration["id"],
        version=registration["version"],
        state="research_only",
        directory=directory,
        summary=summary_path,
        source_commit=summary["code"]["commit"],
        decision=HistoricalDecision(
            verdict=decision["verdict"],
            passed=decision["passed"],
            cost_bps=decision["cost_bps_each_traded_dollar_both_strategies"],
            rule=decision["rule"],
            comparisons=MappingProxyType(comparisons),
        ),
    )
