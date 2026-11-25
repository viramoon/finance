"""Align supplied daily returns with a French factor table."""

from dataclasses import dataclass
from datetime import date, datetime

import numpy as np
import pandas as pd
from scipy.stats import pearsonr

from .research_factors import FrenchDaily


REQUIRED_FACTORS = ("Mkt-RF", "SMB", "HML")


@dataclass(frozen=True)
class FactorRegression:
    intercept: float
    coefficients: dict[str, float]
    r_squared: float


@dataclass(frozen=True)
class FactorAnalysis:
    sample_start: date | None
    sample_end: date | None
    observations: int
    dropped_observations: int
    as_of: date | None
    correlations: dict[str, float | None]
    regression: FactorRegression | None
    regression_status: str | None


def analyze_factors(
    returns: pd.Series,
    factors: pd.DataFrame | FrenchDaily,
    *,
    as_of: date | str | None = None,
    regression: bool = False,
) -> FactorAnalysis:
    """Calculate Pearson correlations and optionally OLS on one complete-case sample."""
    cutoff = _session_date(as_of) if as_of is not None else None
    french_input = isinstance(factors, FrenchDaily)
    if french_input:
        factors = _french_frame(factors, cutoff)
    if not factors.columns.is_unique:
        raise ValueError("Factor columns must be unique")
    missing = [name for name in REQUIRED_FACTORS if name not in factors.columns]
    if missing:
        raise ValueError(f"Missing factor columns: {', '.join(missing)}")
    returns = _date_indexed(returns, cutoff)
    factors = _date_indexed(factors, cutoff)
    if returns.empty:
        factors = factors.iloc[:0]
    else:
        factors = factors.loc[returns.index[0]:returns.index[-1]]

    names = [*REQUIRED_FACTORS]
    if "Mom" in factors.columns and (not french_input or factors["Mom"].notna().any()):
        names.append("Mom")
    aligned = pd.concat([returns.rename("returns"), factors[names]], axis=1, join="outer").sort_index()
    aligned = aligned.apply(pd.to_numeric)
    if np.iscomplexobj(aligned.to_numpy()):
        raise ValueError("Daily returns must be real numbers")
    aligned = aligned.astype(float)
    if np.isinf(aligned.to_numpy()).any():
        raise ValueError("Daily returns must be finite or missing")
    sample = aligned.dropna()
    outcome = sample["returns"]
    outcome_varies = outcome.nunique() > 1
    correlations = {
        name: float(pearsonr(outcome, sample[name]).statistic)
        if outcome_varies and sample[name].nunique() > 1
        else None
        for name in names
    }
    fitted, status = _ols(outcome, sample[names]) if regression else (None, None)
    return FactorAnalysis(
        sample_start=sample.index[0] if len(sample) else None,
        sample_end=sample.index[-1] if len(sample) else None,
        observations=len(sample),
        dropped_observations=len(aligned) - len(sample),
        as_of=cutoff,
        correlations=correlations,
        regression=fitted,
        regression_status=status,
    )


def _french_frame(batch: FrenchDaily, cutoff: date | None) -> pd.DataFrame:
    rows = [
        (row.session, row.mkt_rf, row.smb, row.hml, row.mom)
        for row in batch.rows
        if cutoff is None or row.available_on is None or row.available_on <= cutoff
    ]
    return pd.DataFrame.from_records(rows, columns=["session", *REQUIRED_FACTORS, "Mom"]).set_index("session")


def _session_date(value: date | str) -> date:
    if isinstance(value, str):
        parsed = date.fromisoformat(value)
        if parsed.isoformat() == value:
            return parsed
    elif isinstance(value, datetime):
        if not pd.isna(value) and value.tzinfo is None and value == pd.Timestamp(value).normalize():
            return value.date()
    elif isinstance(value, date):
        return value
    raise ValueError(f"Expected an ISO date or a timezone-naive midnight label, got {value!r}")


def _date_indexed(frame, cutoff: date | None):
    frame = frame.set_axis(pd.Index([_session_date(label) for label in frame.index], dtype=object))
    if not frame.index.is_unique:
        raise ValueError("Daily return dates must be unique")
    frame = frame.sort_index()
    return frame if cutoff is None else frame.loc[:cutoff]


def _ols(outcome: pd.Series, factors: pd.DataFrame) -> tuple[FactorRegression | None, str]:
    if len(outcome) <= len(factors.columns) + 1:
        return None, "insufficient_observations"
    if outcome.nunique() <= 1:
        return None, "constant_returns"

    import statsmodels.api as sm

    design = sm.add_constant(factors, has_constant="add")
    if np.linalg.matrix_rank(design.to_numpy()) < len(design.columns):
        return None, "rank_deficient"
    fit = sm.OLS(outcome, design).fit()
    return FactorRegression(
        intercept=float(fit.params["const"]),
        coefficients={name: float(fit.params[name]) for name in factors.columns},
        r_squared=float(fit.rsquared),
    ), "ok"
