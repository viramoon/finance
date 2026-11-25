"""Catalog-backed Python research entry point for the cash portfolio engine."""

from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from datetime import date
from decimal import Decimal, localcontext
from itertools import pairwise
from typing import Literal

from .portfolio_backtest import FillTiming, PortfolioConfig, RebalanceAudit, _run_portfolio_backtest
from .fill_audit import AuditedFill
from .historical_catalog import HistoricalCatalog, YahooHistory
from .portfolio_inputs import DataGap, PortfolioAsset, TargetFunction, TargetWeights, UniverseSnapshot
from .research_catalog import ResearchError, iso_date, sessions
from .research_decimal import RESEARCH_CONTEXT
from .research_metrics import RiskFreeSeries, daily_risk_free
from .research_windows import coverage_windows


_DEFAULT_CONFIG = PortfolioConfig()


@dataclass(frozen=True)
class BacktestMetadata:
    instruments: tuple[str, ...]
    start: date
    end: date
    fill_timing: FillTiming
    initial_cash: Decimal
    costs: Decimal
    risk_free: dict[str, str]
    benchmark: str | None
    fixture_zero_volume: bool


@dataclass(frozen=True)
class BacktestResult:
    metadata: BacktestMetadata
    nav: dict[date, Decimal | None]
    weights: dict[date, dict[str, Decimal] | None]
    targets: tuple[RebalanceAudit, ...]
    positions: dict[str, Decimal]
    cash: Decimal
    fill_audit: tuple[AuditedFill, ...]
    gaps: tuple[DataGap, ...]
    total_orders: int
    windows: list[dict]
    benchmark_nav: dict[date, Decimal | None] | None
    benchmark_windows: list[dict] | None

    def analyze_factors(self, factors, *, as_of: date | str | None = None, regression: bool = False):
        """Analyze simple close-to-close returns, leaving unvalued intervals missing."""
        import pandas as pd

        from .factor_analysis import analyze_factors

        with localcontext(RESEARCH_CONTEXT):
            returns = {
                day: value / prior - 1 if value is not None and prior is not None else None
                for (_, prior), (day, value) in pairwise(self.nav.items())
            }
        series = pd.Series(returns, dtype=object)
        return analyze_factors(series, factors, as_of=self.metadata.end if as_of is None else as_of,
                               regression=regression)


def run_backtest(
    strategy: TargetFunction | Sequence[TargetWeights],
    instruments: Sequence[str],
    start: date | str,
    end: date | str,
    *,
    benchmark: str | None = None,
    costs: Decimal | int | None = None,
    rf: Literal["kenfrench"] | RiskFreeSeries | None = "kenfrench",
    fill_timing: FillTiming = _DEFAULT_CONFIG.fill_timing,
    catalog: HistoricalCatalog | None = None,
    universe: Sequence[UniverseSnapshot],
    raw_split_free_reviewed: bool = False,
    rf_available_ts: Mapping[date, int] | None = None,
    fixture_zero_volume: bool = False,
) -> BacktestResult:
    """Run reviewed catalog data with point-in-time targets and execution.

    Enable fixture_zero_volume only for synthetic equity data in a research fixture.
    """
    with localcontext(RESEARCH_CONTEXT):
        if costs is not None and not (
            (type(costs) is int and costs == 0) or (isinstance(costs, Decimal) and costs.is_zero())
        ):
            raise ResearchError("unsupported_costs", "Research backtests support only zero costs")
        if type(fixture_zero_volume) is not bool:
            raise ResearchError("invalid_parameters", "fixture_zero_volume must be a boolean")
        config = PortfolioConfig(fill_timing=fill_timing)
        start = iso_date(start.isoformat() if type(start) is date else start)
        end = iso_date(end.isoformat() if type(end) is date else end)
        if start > end:
            raise ResearchError("invalid_date", "Portfolio start must be on or before end")
        if not isinstance(instruments, Sequence) or isinstance(instruments, (str, bytes)) or (
            not instruments or any(not isinstance(instrument, str) for instrument in instruments)
            or len(set(instruments)) != len(instruments)
        ):
            raise ResearchError("invalid_instrument", "Supply a sequence of distinct Yahoo catalog instrument IDs")
        instruments = tuple(instruments)
        if raw_split_free_reviewed is not True:
            raise ResearchError("invalid_source", "Review supplied prices as raw, unadjusted and split-free")
        if benchmark is not None and not isinstance(benchmark, str):
            raise ResearchError("invalid_instrument", "Supply a Yahoo catalog ID for the benchmark")
        target_fn = strategy if callable(strategy) else None
        if target_fn is None and (
            not isinstance(strategy, Sequence) or isinstance(strategy, (str, bytes))
        ):
            raise ResearchError("invalid_weights", "Supply a target function or a sequence of TargetWeights")

        catalog = HistoricalCatalog() if catalog is None else catalog
        risk_free = _risk_free(catalog, start, end, rf, rf_available_ts)
        assets = tuple(
            PortfolioAsset.from_yahoo(catalog.read_yahoo(key, start=start, end=end), raw_split_free_reviewed=True)
            for key in instruments
        )
        reference = catalog.read_yahoo(benchmark, start=start, end=end) if benchmark is not None else None
        portfolio, weights, fills, cash = _run_portfolio_backtest(
            assets=assets, universe=universe, start=start, end=end, config=config,
            targets=strategy if target_fn is None else None, target_fn=target_fn,
            rf=risk_free, research_details=True, fixture_zero_volume=fixture_zero_volume,
        )
        benchmark_nav, benchmark_windows = (
            _benchmark(reference, sessions(start, end), risk_free) if reference is not None else (None, None)
        )
        return BacktestResult(
            metadata=BacktestMetadata(
                instruments, start, end, config.fill_timing, config.initial_cash, Decimal(0),
                portfolio.risk_free, benchmark, fixture_zero_volume,
            ),
            nav=portfolio.equity,
            weights=weights,
            targets=portfolio.rebalances,
            positions=portfolio.positions,
            cash=cash,
            fill_audit=fills,
            gaps=portfolio.gaps,
            total_orders=portfolio.total_orders,
            windows=portfolio.windows,
            benchmark_nav=benchmark_nav,
            benchmark_windows=benchmark_windows,
        )


def _risk_free(catalog, start, end, rf, available_ts):
    if rf is None or isinstance(rf, RiskFreeSeries):
        if available_ts is not None:
            raise ResearchError("invalid_risk_free", "rf_available_ts applies only to rf='kenfrench'")
        return rf
    if rf != "kenfrench":
        raise ResearchError("invalid_risk_free", "Use rf='kenfrench', a RiskFreeSeries or None")
    if not isinstance(available_ts, Mapping) or not available_ts or any(type(day) is not date for day in available_ts):
        raise ResearchError(
            "missing_rf_availability",
            "Ken French RF requires rf_available_ts keyed by observation date; use rf=None for explicit rf=0",
        )
    try:
        batch = catalog.read_french(start=min(start, min(available_ts)), end=end)
    except FileNotFoundError as exc:
        raise ResearchError("missing_risk_free", "Ken French daily factors are missing from the catalog") from exc
    series = RiskFreeSeries.from_french(batch, available_ts=available_ts)
    if not series.rates:
        raise ResearchError("missing_risk_free", "No Ken French RF observations in the selected range")
    return series


def _benchmark(history: YahooHistory, closes: Mapping[date, int], rf: RiskFreeSeries | None):
    if history.is_reference:
        if history.currency != "USD":
            raise ResearchError("invalid_instrument", "Expected a USD reference index for the benchmark")
        observed = {row.session: row.close for row in history.bars if row.close is not None}
    else:
        asset = PortfolioAsset.from_yahoo(history, raw_split_free_reviewed=True)
        prices = {bar.ts_event: bar.close.as_decimal() for bar in asset.bars if bar.volume.raw > 0}
        observed = {day: prices[when] for day, when in closes.items() if when in prices}
    nav = {day: observed.get(day) for day in closes}
    aligned_rf = daily_risk_free(closes, rf) if rf is not None else None
    windows = coverage_windows(
        observed, next(reversed(closes)), rf=aligned_rf, rf_source=rf.source if rf is not None else None,
        turnover=dict.fromkeys(closes, Decimal(0)), longest_through_end=True,
    )
    return nav, windows
