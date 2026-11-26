"""Indicator boundary for StrategyBundle backtests."""

from collections.abc import Mapping, Sequence
from decimal import Decimal, localcontext
from typing import Protocol

from .research_decimal import RESEARCH_CONTEXT
from .portfolio_inputs import Bar, ResearchError


BUILTIN_EVALUATOR_ID = "fertile.finance/builtin-strategy-indicators/v1"


class IndicatorEvaluator(Protocol):
    def __call__(
        self, bars: Sequence[Bar], component: str, parameters: Mapping[str, int | str],
    ) -> Decimal | None:
        """Evaluate one id@version at the last closed bar, or return missing."""
        ...


def evaluate_indicator(
    bars: Sequence[Bar], component: str, parameters: Mapping[str, int | str],
) -> Decimal | None:
    """Built-in close return and Wilder RSI until the shared F6 kernel is available."""
    with localcontext(RESEARCH_CONTEXT):
        if component == "factor.close_return@1.0.0":
            lookback = int(parameters["lookback_sessions"])
            if len(bars) <= lookback:
                return None
            return bars[-1].close.as_decimal() / bars[-lookback - 1].close.as_decimal() - 1
        if component == "indicator.rsi_wilder@1.0.0":
            period = int(parameters["period"])
            if len(bars) <= period:
                return None
            previous = bars[0].close.as_decimal()
            gain, loss = Decimal(0), Decimal(0)
            for index, bar in enumerate(bars):
                if index == 0:
                    continue
                close = bar.close.as_decimal()
                change, previous = close - previous, close
                up, down = max(Decimal(0), change), max(Decimal(0), -change)
                if index <= period:
                    gain, loss = gain + up, loss + down
                    if index == period:
                        gain, loss = gain / period, loss / period
                else:
                    gain = (gain * (period - 1) + up) / period
                    loss = (loss * (period - 1) + down) / period
            return None if not gain and not loss else Decimal(100) * gain / (gain + loss)
    raise ResearchError("unsupported_strategy_component", "The requested indicator requires an evaluator")
