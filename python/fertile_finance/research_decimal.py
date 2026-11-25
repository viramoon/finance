"""Decimal contexts for portfolio research and its input conversions."""

from decimal import Context, DivisionByZero, Inexact, InvalidOperation, Overflow, ROUND_HALF_EVEN


# Context() fills unspecified fields from the mutable decimal.DefaultContext.
RESEARCH_CONTEXT = Context(
    prec=34, rounding=ROUND_HALF_EVEN, Emin=-999_999, Emax=999_999, capitals=1, clamp=0,
    flags=[], traps=[InvalidOperation, DivisionByZero, Overflow],
)
EXACT_RESEARCH_CONTEXT = RESEARCH_CONTEXT.copy()
EXACT_RESEARCH_CONTEXT.traps[Inexact] = True
