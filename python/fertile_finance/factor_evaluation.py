"""Evaluate point-in-time scores on explicitly supplied daily research inputs."""

from collections import Counter
from collections.abc import Mapping, Sequence
from dataclasses import dataclass, fields, is_dataclass
from datetime import date, datetime, timezone
from decimal import Decimal, localcontext
from itertools import pairwise
from statistics import mean, stdev
from types import MappingProxyType
from typing import Literal

from .factor_analysis import rank_correlation
from .research_decimal import RESEARCH_CONTEXT
from .research_factors import FactorDefinition, FactorSnapshotRow, PointInTimeFactor
from .research_metrics import TRADING_DAYS


HORIZONS = (1, 5, 20)
ZERO = Decimal(0)
ONE = Decimal(1)

type Phase = Literal["train", "validation", "out_of_sample"]


@dataclass(frozen=True)
class EvaluationSession:
    session: date
    decision_ts: datetime
    closes: Mapping[str, Decimal]

    def __post_init__(self) -> None:
        if type(self.session) is not date:
            raise ValueError("Evaluation session must be a calendar date")
        if not isinstance(self.decision_ts, datetime) or self.decision_ts.utcoffset() is None:
            raise ValueError("Evaluation decision_ts must be timezone-aware")
        decision_ts = self.decision_ts.astimezone(timezone.utc)
        if decision_ts.date() != self.session:
            raise ValueError("Evaluation decision_ts must fall on its UTC session date")
        if not isinstance(self.closes, Mapping):
            raise ValueError("Evaluation closes must be a mapping keyed by asset ID")
        for asset, close in self.closes.items():
            if not isinstance(asset, str) or not asset.strip():
                raise ValueError("Evaluation closes require nonempty asset IDs")
            if not isinstance(close, Decimal) or not close.is_finite() or close <= 0:
                raise ValueError(f"Close for {asset} must be a positive finite Decimal")
        object.__setattr__(self, "decision_ts", decision_ts)
        object.__setattr__(self, "closes", MappingProxyType(dict(self.closes)))


@dataclass(frozen=True)
class EvaluationWindow:
    name: str
    phase: Phase
    start: date
    end: date

    def __post_init__(self) -> None:
        if not isinstance(self.name, str) or not self.name.strip():
            raise ValueError("Evaluation window name must be nonempty")
        if self.phase not in ("train", "validation", "out_of_sample"):
            raise ValueError(f"Unknown evaluation phase {self.phase!r}")
        if type(self.start) is not date or type(self.end) is not date or self.start >= self.end:
            raise ValueError("Evaluation window must have strictly increasing session date bounds")


@dataclass(frozen=True)
class Distribution:
    count: int
    mean: Decimal | None
    sample_std: Decimal | None
    ratio: Decimal | None


@dataclass(frozen=True)
class PortfolioPeriod:
    target_weights: dict[str, Decimal]
    prior_weights: dict[str, Decimal] | None
    asset_returns: dict[str, Decimal | None]
    gross_return: Decimal | None
    turnover: Decimal | None
    fee_fraction: Decimal | None
    net_return: Decimal | None
    industry_weights: dict[str, Decimal]
    industry_coverage: Decimal
    weighted_mean_size: Decimal | None
    size_coverage: Decimal


@dataclass(frozen=True)
class PortfolioSummary:
    periods: int
    gross_observations: int
    net_observations: int
    turnover_observations: int
    cumulative_gross_return: Decimal | None
    cumulative_net_return: Decimal | None
    total_turnover: Decimal | None
    total_fee_fraction: Decimal | None


@dataclass(frozen=True)
class SessionEvaluation:
    session: date
    decision_ts: datetime
    eligible_count: int
    available_count: int
    scored_count: int
    missing_reasons: dict[str, int]
    scores: Distribution
    ic: dict[int, Decimal | None]
    ic_pairs: dict[int, int]
    redundancy: dict[str, Decimal | None]
    quantiles: tuple[PortfolioPeriod, ...] | None
    benchmark: PortfolioPeriod | None


@dataclass(frozen=True)
class WindowEvaluation:
    window: EvaluationWindow
    periods: tuple[SessionEvaluation, ...]
    factor_coverage: Decimal | None
    scored_coverage: Decimal | None
    ic: dict[int, Distribution]
    redundancy: dict[str, Distribution]
    quantiles: tuple[PortfolioSummary, ...]
    benchmark: PortfolioSummary
    gross_information: Distribution
    net_information: Distribution


@dataclass(frozen=True)
class FactorEvaluation:
    definition: FactorDefinition
    peer_definitions: tuple[FactorDefinition, ...]
    cost_bps: Decimal
    quantile_count: int
    windows: tuple[WindowEvaluation, ...]

    def as_dict(self) -> dict:
        """Encode an audit report with ISO dates and lossless Decimal strings."""
        return _wire(self)


def _wire(value):
    if is_dataclass(value):
        return {field.name: _wire(getattr(value, field.name)) for field in fields(value) if field.init}
    if isinstance(value, Mapping):
        return {str(key): _wire(item) for key, item in value.items()}
    if isinstance(value, tuple):
        return [_wire(item) for item in value]
    if isinstance(value, (date, datetime)):
        return value.isoformat()
    if isinstance(value, Decimal):
        return str(value)
    return value


def evaluate_factor(
    factor: PointInTimeFactor,
    sessions: Sequence[EvaluationSession],
    windows: Sequence[EvaluationWindow],
    *,
    cost_bps: Decimal,
    quantiles: int = 5,
    peers: Sequence[PointInTimeFactor] = (),
) -> FactorEvaluation:
    """Evaluate fixed scores without fitting and retain the evidence by independent window.

    Supply each session of the research calendar, including sessions with missing
    marks. Closes are decision-time marks for a hypothetical research portfolio.
    They are used to measure later returns, without modeling order execution.
    """
    if not isinstance(cost_bps, Decimal) or not cost_bps.is_finite() or cost_bps < 0:
        raise ValueError("cost_bps must be a nonnegative finite Decimal")
    if type(quantiles) is not int or quantiles < 2:
        raise ValueError("quantiles must be an integer of at least two")
    if not isinstance(factor, PointInTimeFactor) or any(not isinstance(peer, PointInTimeFactor) for peer in peers):
        raise ValueError("Evaluation factor and peers must be point-in-time factors")
    peer_map = {f"{peer.definition.factor_id}@{peer.definition.version}": peer for peer in peers}
    if len(peer_map) != len(peers) or f"{factor.definition.factor_id}@{factor.definition.version}" in peer_map:
        raise ValueError("Primary factor and peers must have distinct factor IDs and versions")
    peer_map = dict(sorted(peer_map.items()))
    slices = _window_slices(sessions, windows)
    with localcontext(RESEARCH_CONTEXT):
        evaluated = tuple(
            _evaluate_window(factor, sessions[section], window, cost_bps, quantiles, peer_map)
            for section, window in zip(slices, windows, strict=True)
        )
    return FactorEvaluation(factor.definition, tuple(peer.definition for peer in peer_map.values()),
                            cost_bps, quantiles, evaluated)


def _window_slices(sessions: Sequence[EvaluationSession], windows: Sequence[EvaluationWindow]) -> list[slice]:
    if any(not isinstance(row, EvaluationSession) for row in sessions):
        raise ValueError("Evaluation sessions must contain EvaluationSession entries")
    if any(a.session >= b.session or a.decision_ts >= b.decision_ts for a, b in pairwise(sessions)):
        raise ValueError("Evaluation dates and decision timestamps must be strictly increasing")
    if any(not isinstance(window, EvaluationWindow) for window in windows):
        raise ValueError("Evaluation windows must contain EvaluationWindow entries")
    phases = [window.phase for window in windows]
    if len(phases) < 3 or phases[:2] != ["train", "validation"] or set(phases[2:]) != {"out_of_sample"}:
        raise ValueError("Supply a train, a validation, and at least one rolling out_of_sample window in order")
    if any(a.end >= b.start for a, b in pairwise(windows)):
        raise ValueError("Evaluation windows must be ordered and must not overlap")
    if len({window.name for window in windows}) != len(windows):
        raise ValueError("Evaluation window names must be unique")
    indices = {row.session: index for index, row in enumerate(sessions)}
    if any(window.start not in indices or window.end not in indices for window in windows):
        raise ValueError("Evaluation window bounds must be supplied session dates")
    return [slice(indices[window.start], indices[window.end] + 1) for window in windows]


def _evaluate_window(factor, sessions, window, cost_bps, quantiles, peers) -> WindowEvaluation:
    periods = []
    current_quantiles = [{} for _ in range(quantiles)]
    current_benchmark = {}
    for index, session in enumerate(sessions):
        universe = {row.asset_id: row for row in factor.snapshot(session.decision_ts).rows}
        available = {asset: row.value for asset, row in universe.items() if row.value is not None}
        marked = session.closes.keys() & universe.keys()
        scored = marked & available.keys()
        missing = Counter(row.missing_reason for row in universe.values() if row.missing_reason is not None)
        missing["missing_close"] = len(universe) - len(marked)
        returns = {horizon: _forward_returns(session, sessions[index + horizon], marked)
                   for horizon in HORIZONS if index + horizon < len(sessions)}
        ic = {}
        ic_pairs = {}
        for horizon in HORIZONS:
            forward = returns.get(horizon, {})
            paired = sorted(scored & forward.keys())
            ic[horizon] = rank_correlation([available[asset] for asset in paired], [forward[asset] for asset in paired])
            ic_pairs[horizon] = len(paired)

        redundancy = {name: _peer_correlation(available, peer, session.decision_ts) for name, peer in peers.items()}
        portfolio_rows = benchmark_row = None
        if 1 in returns:
            targets = _quantile_weights(available, scored, quantiles)
            portfolio_rows = tuple(_portfolio_period(target, prior, returns[1], cost_bps, universe)
                                   for target, prior in zip(targets, current_quantiles, strict=True))
            benchmark_row = _portfolio_period(_equal_weights(marked), current_benchmark, returns[1], cost_bps, universe)
            current_quantiles = [_drifted_weights(row) for row in portfolio_rows]
            current_benchmark = _drifted_weights(benchmark_row)
        periods.append(SessionEvaluation(
            session=session.session,
            decision_ts=session.decision_ts,
            eligible_count=len(universe),
            available_count=len(available),
            scored_count=len(scored),
            missing_reasons=dict(sorted(missing.items())),
            scores=_distribution(available.values()),
            ic=ic,
            ic_pairs=ic_pairs,
            redundancy=redundancy,
            quantiles=portfolio_rows,
            benchmark=benchmark_row,
        ))

    eligible = sum(row.eligible_count for row in periods)
    portfolio_periods = periods[:-1]
    top = [row.quantiles[-1] for row in portfolio_periods]
    benchmark = [row.benchmark for row in portfolio_periods]
    return WindowEvaluation(
        window=window,
        periods=tuple(periods),
        factor_coverage=Decimal(sum(row.available_count for row in periods)) / eligible if eligible else None,
        scored_coverage=Decimal(sum(row.scored_count for row in periods)) / eligible if eligible else None,
        ic={horizon: _distribution((row.ic[horizon] for row in periods), scale=ONE) for horizon in HORIZONS},
        redundancy={name: _distribution(row.redundancy[name] for row in periods) for name in peers},
        quantiles=tuple(_portfolio_summary([row.quantiles[index] for row in portfolio_periods])
                        for index in range(quantiles)),
        benchmark=_portfolio_summary(benchmark),
        gross_information=_information_ratio(top, benchmark, "gross_return"),
        net_information=_information_ratio(top, benchmark, "net_return"),
    )


def _forward_returns(start, end, marked):
    return {asset: end.closes[asset] / start.closes[asset] - ONE
            for asset in marked & end.closes.keys()}


def _peer_correlation(available, peer, decision_ts):
    other = {row.asset_id: row.value for row in peer.snapshot(decision_ts).rows if row.value is not None}
    paired = sorted(available.keys() & other.keys())
    return rank_correlation([available[asset] for asset in paired], [other[asset] for asset in paired])


def _distribution(values, *, scale: Decimal | None = None) -> Distribution:
    observed = [value for value in values if value is not None]
    average = mean(observed) if observed else None
    deviation = stdev(observed) if len(observed) >= 2 else None
    ratio = average / deviation * scale if scale is not None and deviation else None
    return Distribution(len(observed), average, deviation, ratio)


def _equal_weights(assets) -> dict[str, Decimal]:
    ordered = sorted(assets)
    if not ordered:
        return {}
    weight = ONE / len(ordered)
    weights = {asset: weight for asset in ordered}
    weights[ordered[-1]] += ONE - sum(weights.values())
    return weights


def _quantile_weights(available, scored, quantiles) -> list[dict[str, Decimal]]:
    if len(scored) < quantiles:
        return [{} for _ in range(quantiles)]
    ordered = sorted(scored, key=lambda asset: (available[asset], asset))
    group_size, remainder = divmod(len(ordered), quantiles)
    groups = []
    start = 0
    for index in range(quantiles):
        end = start + group_size + (index < remainder)
        groups.append(_equal_weights(ordered[start:end]))
        start = end
    return groups


def _portfolio_period(target, prior, forward, cost_bps, universe: Mapping[str, FactorSnapshotRow]) -> PortfolioPeriod:
    asset_returns = {asset: forward.get(asset) for asset in target}
    gross = None if None in asset_returns.values() else sum(
        (weight * asset_returns[asset] for asset, weight in target.items()), ZERO,
    )
    turnover = None
    fee = None
    if prior is not None:
        turnover = sum((abs(target.get(asset, ZERO) - prior.get(asset, ZERO))
                        for asset in sorted(target.keys() | prior.keys())), ZERO)
        fee = turnover * cost_bps / 10_000
        if fee >= ONE:
            raise ValueError("Modeled transaction cost exhausts portfolio equity")
    net = (ONE + gross) * (ONE - fee) - ONE if gross is not None and fee is not None else None
    industries = {}
    total_size = size_weight = ZERO
    for asset, weight in target.items():
        industry, size = universe[asset].industry, universe[asset].size
        if industry is not None:
            industries[industry] = industries.get(industry, ZERO) + weight
        if size is not None:
            size_weight += weight
            total_size += weight * size
    return PortfolioPeriod(
        target_weights=target,
        prior_weights=prior,
        asset_returns=asset_returns,
        gross_return=gross,
        turnover=turnover,
        fee_fraction=fee,
        net_return=net,
        industry_weights=dict(sorted(industries.items())),
        industry_coverage=sum(industries.values(), ZERO),
        weighted_mean_size=total_size / size_weight if size_weight else None,
        size_coverage=size_weight,
    )


def _drifted_weights(period: PortfolioPeriod) -> dict[str, Decimal] | None:
    if period.gross_return is None:
        return None
    gross_growth = ONE + period.gross_return
    if not gross_growth:
        return None
    return {asset: weight * (ONE + period.asset_returns[asset]) / gross_growth
            for asset, weight in period.target_weights.items()}


def _compound(values: Sequence[Decimal | None]) -> Decimal | None:
    if None in values:
        return None
    growth = ONE
    for value in values:
        growth *= ONE + value
    return growth - ONE


def _portfolio_summary(periods: Sequence[PortfolioPeriod]) -> PortfolioSummary:
    gross = [row.gross_return for row in periods]
    net = [row.net_return for row in periods]
    turnover = [row.turnover for row in periods]
    return PortfolioSummary(
        periods=len(periods),
        gross_observations=sum(value is not None for value in gross),
        net_observations=sum(value is not None for value in net),
        turnover_observations=sum(value is not None for value in turnover),
        cumulative_gross_return=_compound(gross),
        cumulative_net_return=_compound(net),
        total_turnover=sum(turnover, ZERO) if None not in turnover else None,
        total_fee_fraction=sum((row.fee_fraction for row in periods), ZERO) if None not in turnover else None,
    )


def _information_ratio(portfolio, benchmark, attribute):
    paired = ((getattr(row, attribute), getattr(reference, attribute))
              for row, reference in zip(portfolio, benchmark, strict=True))
    return _distribution((result - base for result, base in paired if result is not None and base is not None),
                         scale=TRADING_DAYS.sqrt())
