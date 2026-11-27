"""Evaluate compiled strategy plans on closed bars without execution dependencies."""

from collections import deque
from dataclasses import dataclass, field
from decimal import Decimal
import hashlib
import json
import re
from typing import Protocol, Self


SMA_STRATEGY = "SmaCrossover"
_SMA_PLAN_VERSION = "fertile.finance/sma-crossover-plan/v1"
_XNYS_ID = re.compile(r"[A-Z][A-Z0-9-]{0,15}\.XNYS")
_PRICE_DECIMAL_PLACES = 27  # F1 prices have at most 28 digits, including the integer part.
_PRICE_SCALE = 10**_PRICE_DECIMAL_PLACES


def _unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("Duplicate compiled plan field")
        result[key] = value
    return result


@dataclass(frozen=True, slots=True)
class SmaCrossoverPlan:
    instrument_id: str
    fast_period: int
    slow_period: int
    shares: int
    content_sha256: str = field(init=False)

    def __post_init__(self):
        if not isinstance(self.instrument_id, str) or not _XNYS_ID.fullmatch(self.instrument_id):
            raise ValueError("Choose a catalog XNYS stock or ETF")
        if any(type(value) is not int for value in (self.fast_period, self.slow_period, self.shares)) or not (
            2 <= self.fast_period < self.slow_period <= 250 and 1 <= self.shares <= 1000
        ):
            raise ValueError("Require 2 <= fast < slow <= 250 and 1..1000 shares")
        canonical = json.dumps(self._payload(), sort_keys=True, separators=(",", ":"))
        object.__setattr__(self, "content_sha256", hashlib.sha256(canonical.encode("utf-8")).hexdigest())

    def _payload(self) -> dict:
        return {
            "plan_version": _SMA_PLAN_VERSION,
            "strategy": SMA_STRATEGY,
            "instrument_id": self.instrument_id,
            "parameters": {
                "fast_period": self.fast_period,
                "slow_period": self.slow_period,
                "shares": self.shares,
            },
        }

    def as_dict(self) -> dict:
        return {**self._payload(), "content_sha256": self.content_sha256}

    @classmethod
    def from_json(cls, raw: str | bytes) -> Self:
        try:
            plan = json.loads(raw, object_pairs_hook=_unique_object)
        except (UnicodeError, ValueError, RecursionError):
            raise ValueError("Invalid compiled SMA plan JSON") from None
        if not isinstance(plan, dict) or plan.keys() != {
            "plan_version", "strategy", "instrument_id", "parameters", "content_sha256",
        } or plan["plan_version"] != _SMA_PLAN_VERSION or plan["strategy"] != SMA_STRATEGY:
            raise ValueError("Unsupported compiled strategy plan")
        params = plan["parameters"]
        if not isinstance(params, dict) or params.keys() != {"fast_period", "slow_period", "shares"}:
            raise ValueError("Invalid compiled SMA parameters")
        result = cls(plan["instrument_id"], **params)
        if plan["content_sha256"] != result.content_sha256:
            raise ValueError("Compiled SMA plan content hash mismatch")
        return result


@dataclass(frozen=True, slots=True)
class ClosedBar:
    instrument_id: str
    ts_event: int
    close: Decimal


@dataclass(frozen=True, slots=True)
class Decision:
    instrument_id: str
    ts_event: int
    target_shares: Decimal


class Evaluator(Protocol):
    def evaluate(self, bar: ClosedBar) -> Decision | None:
        """Consume one ordered closed bar. Return None during warm-up."""
        ...


class EvaluationBackend(Protocol):
    def open(self, plan: SmaCrossoverPlan) -> Evaluator:
        """Start independent evaluation state for a compiled plan."""
        ...


class CpuEvaluationBackend:
    def open(self, plan: SmaCrossoverPlan) -> Evaluator:
        return _CpuSmaEvaluator(plan)


def open_evaluator(plan: SmaCrossoverPlan, *, backend: EvaluationBackend | None = None) -> Evaluator:
    return (CpuEvaluationBackend() if backend is None else backend).open(plan)


class _CpuSmaEvaluator:
    def __init__(self, plan: SmaCrossoverPlan):
        self.plan = plan
        self.fast = deque(maxlen=plan.fast_period)
        self.slow = deque(maxlen=plan.slow_period)
        self.fast_total = 0
        self.slow_total = 0
        self.last_ts = -1

    def evaluate(self, bar: ClosedBar) -> Decision | None:
        if bar.instrument_id != self.plan.instrument_id:
            raise ValueError("Closed bar instrument differs from the compiled plan")
        if type(bar.ts_event) is not int or not self.last_ts < bar.ts_event < 2**64:
            raise ValueError("Closed bars must have strictly increasing Unix nanoseconds")
        if not isinstance(bar.close, Decimal) or not bar.close.is_finite() or bar.close <= 0:
            raise ValueError("Closed bar price must be a positive finite Decimal")
        if (bar.close.adjusted() > _PRICE_DECIMAL_PLACES
                or bar.close.as_tuple().exponent < -_PRICE_DECIMAL_PLACES):
            raise ValueError("Closed bar price exceeds the supported precision")

        # The denominator divides the fixed-point scale, so scaling is exact.
        numerator, denominator = bar.close.as_integer_ratio()
        close = numerator * _PRICE_SCALE // denominator
        self.last_ts = bar.ts_event
        self.fast_total += close - (self.fast[0] if len(self.fast) == self.fast.maxlen else 0)
        self.slow_total += close - (self.slow[0] if len(self.slow) == self.slow.maxlen else 0)
        self.fast.append(close)
        self.slow.append(close)
        if len(self.slow) < self.plan.slow_period:
            return None
        above = self.fast_total * self.plan.slow_period > self.slow_total * self.plan.fast_period
        return Decision(bar.instrument_id, bar.ts_event, Decimal(self.plan.shares if above else 0))
