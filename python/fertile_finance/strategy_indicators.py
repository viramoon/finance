"""Use the same Rust indicator engine as the chart for closed strategy bars."""

from collections.abc import Mapping, Sequence
from contextlib import closing, suppress
from dataclasses import dataclass
from datetime import date
from decimal import Decimal
import json
import os
from pathlib import Path
import subprocess
from typing import Protocol, TYPE_CHECKING

from .epoch import EPOCH
from .research_catalog import ResearchError, nyse

if TYPE_CHECKING:
    from nautilus_trader.model.data import Bar


BUILTIN_EVALUATOR_ID = "fertile.finance/f6-strategy-indicators/v1"
_CHUNK_BARS = 256
_NS_PER_DAY = 86_400_000_000_000


class IndicatorEvaluator(Protocol):
    def __call__(
        self, bars: Sequence["Bar"], component: str, parameters: Mapping[str, int | str],
    ) -> Decimal | None:
        """Evaluate one id@version at the last closed bar, or return missing."""
        ...


@dataclass(slots=True)
class _Series:
    identifier: int
    count: int = 0
    value: Decimal | None = None


class RustIndicatorEvaluator:
    """One local subprocess, with one incremental series per member and factor."""

    def __init__(self):
        self.process = None
        self.series = {}
        self.next_id = 0

    def _request(self, request):
        if self.process is None:
            executable = os.environ.get("FINANCE_STRATEGY_BIN", "finance-strategy")
            if not executable or ("FINANCE_STRATEGY_BIN" in os.environ and not Path(executable).is_absolute()):
                raise ResearchError("indicator_backend_unavailable", "FINANCE_STRATEGY_BIN must be an absolute path")
            try:
                self.process = subprocess.Popen(
                    [executable, "evaluate-indicators"], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                    stderr=subprocess.STDOUT, text=True, encoding="utf-8",
                )
            except OSError:
                raise ResearchError("indicator_backend_unavailable", "Could not start the Rust indicator evaluator") from None
        try:
            self.process.stdin.write(json.dumps(request, allow_nan=False) + "\n")
            self.process.stdin.flush()
            reply = json.loads(self.process.stdout.readline(1024))
            if not isinstance(reply, dict) or reply.keys() != {"value"}:
                raise ValueError("Invalid response")
            value = reply["value"]
            if value is None:
                return None
            if isinstance(value, str) and (decimal := Decimal(value)).is_finite():
                return decimal
        except (OSError, ValueError, ArithmeticError):
            pass
        raise ResearchError("indicator_evaluation_failed", "Rust indicator evaluation failed") from None

    def value(self, asset, factor, history, component, parameters, output, session_times):
        by_factor = self.series.setdefault(asset, {})
        state = by_factor.get(factor)
        if state is None:
            # F2 named this same close-return algorithm before the F6 registry.
            if component == "factor.close_return@1.0.0":
                component = "indicator.close_return@1.0.0"
                parameters = {"period": parameters["lookback_sessions"]}
            state = _Series(self.next_id)
            self._request({
                "action": "start", "series": state.identifier,
                "id": component, "params": dict(parameters), "output": output,
            })
            self.next_id += 1
            by_factor[factor] = state
        for start in range(state.count, len(history), _CHUNK_BARS):
            bars = [_closed_bar(bar, session_times) for bar in history[start:start + _CHUNK_BARS]]
            state.value = self._request({"action": "push", "series": state.identifier, "bars": bars})
            state.count += len(bars)
        return state.value

    def discard(self, asset):
        for state in self.series.pop(asset, {}).values():
            self._request({"action": "drop", "series": state.identifier})

    def close(self):
        if self.process is not None:
            process, self.process = self.process, None
            with suppress(OSError):
                process.stdin.close()
            try:
                process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
            process.stdout.close()
        self.series.clear()


def _closed_bar(bar, session_times):
    day, open_ts = session_times[bar.ts_event]
    return {
        "open_ts": str(open_ts), "end_ts": str(bar.ts_event), "trading_day": str(day),
        "open": format(bar.open.as_decimal(), "f"),
        "high": format(bar.high.as_decimal(), "f"),
        "low": format(bar.low.as_decimal(), "f"),
        "close": format(bar.close.as_decimal(), "f"),
        "volume": format(bar.volume.as_decimal(), "f"),
    }


def evaluate_indicator(
    bars: Sequence["Bar"], component: str, parameters: Mapping[str, int | str], *, output: str = "value",
) -> Decimal | None:
    """Evaluate a supplied XNYS daily prefix through the Rust indicator engine."""
    if not bars:
        return None
    schedule = nyse().schedule
    session_times = {}
    for bar in bars:
        day = date.fromordinal(EPOCH.toordinal() + bar.ts_event // _NS_PER_DAY)
        session_times[bar.ts_event] = (day, schedule.at[str(day), "open"].value)
    with closing(RustIndicatorEvaluator()) as evaluator:
        return evaluator.value("", "", bars, component, parameters, output, session_times)
