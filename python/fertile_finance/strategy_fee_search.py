"""Exact integer search for a rounded fee ratio between two bounds."""

from fractions import Fraction
from itertools import pairwise
from math import ceil, floor, lcm


def _sum_floors(low: int, high: int, slope: Fraction, offset: Fraction) -> int:
    """Sum floor(slope*x + offset), using Euclidean reduction of the lattice."""
    count = high - low + 1
    if count <= 0:
        return 0
    offset += slope * low
    divisor = lcm(slope.denominator, offset.denominator)
    step = slope.numerator * (divisor // slope.denominator)
    start = offset.numerator * (divisor // offset.denominator)
    total = 0
    while True:
        linear, step = divmod(step, divisor)
        constant, start = divmod(start, divisor)
        total += linear * count * (count - 1) // 2 + constant * count
        top = count * step + start
        if top < divisor:
            return total
        count, start = divmod(top, divisor)
        divisor, step = step, divisor


def _at_least(low: int, high: int, slope: Fraction, offset: Fraction, bound: int) -> tuple[int, int]:
    """Intersect an integer interval with slope*x + offset >= bound."""
    if slope > 0:
        low = max(low, ceil((bound - offset) / slope))
    elif slope < 0:
        high = min(high, floor((bound - offset) / slope))
    elif offset < bound:
        return 1, 0
    return low, high


def _count_above_ratio(
    low: int, high: int, rate: Fraction, phase: Fraction, ratio: Fraction, inclusive: bool,
) -> int:
    """Count where the rounded fee divided by quantity exceeds the ratio."""
    # For >=, compare the fee to ceil(ratio*x) - 1. For >, use floor(ratio*x).
    offset = -Fraction(1, ratio.denominator) if inclusive else Fraction(0)
    slope, intercept = rate - ratio, phase - offset
    low, high = _at_least(low, high, slope, intercept, 0)
    if low > high:
        return 0
    definite_low, definite_high = _at_least(low, high, slope, intercept, 1)
    definite_count = max(0, definite_high - definite_low + 1)
    if definite_count:
        if slope >= 0:
            high = definite_low - 1
        else:
            low = definite_high + 1
    # Where the real arguments differ by less than one, the floor difference
    # is either zero or one. Summing that difference counts all passing values.
    return definite_count + _sum_floors(low, high, rate, phase) - _sum_floors(low, high, ratio, offset)


def largest_fee_ratio_quantity(
    low: int, high: int, rate: Fraction, prefix_quantity: int, lower: Fraction, upper: Fraction,
    *, include_lower: bool = True, include_upper: bool = False,
) -> int | None:
    """Find the largest positive quantity whose marginal fee ratio is in the given band.

    `rate` is in cents per share. The fee in cents is the difference of the
    nonnegative half-up rounded order fees with and without the new shares.
    By default the ratio band is [lower, upper). An endpoint can be included
    independently, including a band consisting of a single value.
    """
    low = max(low, 1)
    if low > high or lower > upper or (lower == upper and not (include_lower and include_upper)):
        return None
    prefix = rate * prefix_quantity + Fraction(1, 2)
    phase = prefix - floor(prefix)
    top_ratio = Fraction(floor(rate * high + phase), high)
    if (top_ratio > lower or (include_lower and top_ratio == lower)) and (
        top_ratio < upper or (include_upper and top_ratio == upper)
    ):
        return high

    def exists(start: int, end: int) -> bool:
        at_lower = _count_above_ratio(start, end, rate, phase, lower, include_lower)
        above_upper = _count_above_ratio(start, end, rate, phase, upper, not include_upper)
        return at_lower > above_upper

    if not exists(low, high):
        return None
    # The existence of a passing quantity in a suffix is monotone even when
    # the ratio itself alternates on successive share counts.
    while low < high:
        midpoint = (low + high + 1) // 2
        if exists(midpoint, high):
            low = midpoint
        else:
            high = midpoint - 1
    return low


def largest_cash_feasible_fee_total(
    low: int, high: int, rate: Fraction, prefix_quantity: int,
    peers: tuple[tuple[int, int], ...], focal_index: int, *, focal: tuple[int, int],
) -> int | None:
    """Find the greatest external share total satisfying fixed-tier cash limits.

    Peers are (positive fixed external shares, available fee cents) in the
    order used for cent remainders, omitting the variable buyer. `focal_index`
    is its position in that order. The `focal` pair is its available
    purchase-cost cents and ask cents per external share, after internal fills.
    Earlier-tier shares contribute to the rounded fee prefix only.
    """
    fixed = sum(quantity for quantity, _ in peers)
    low = max(low, fixed + 1)
    if high < low or any(cash < 0 for _, cash in peers):
        return None
    prefix = rate * prefix_quantity + Fraction(1, 2)
    phase = prefix - floor(prefix)
    wallet, ask = focal

    def fee_total(quantity):
        return floor(rate * quantity + phase)

    def fees_in_band(ratio):
        floors = [floor(ratio * quantity) for quantity, _ in peers]
        # The variable buyer's floor is F - ceil(F*fixed/Q), where F is
        # the tier fee and Q its total quantity. The remainder depends
        # only on F/Q and the fixed peer quantities.
        remainder = ceil(ratio * fixed) - sum(floors)
        fees = (
            cents + (index + (index >= focal_index) < remainder)
            for index, cents in enumerate(floors)
        )
        total = 0
        for cents, (_, cash) in zip(fees, peers):
            if cents > cash:
                return None
            total += cents
        return total

    def focal_cost(quantity, peer_fees):
        return (quantity - fixed) * ask + fee_total(quantity) - peer_fees

    top_peer_fees = fees_in_band(Fraction(fee_total(high), high))
    if top_peer_fees is not None and focal_cost(high, top_peer_fees) <= wallet:
        return high
    high -= 1
    if high < low:
        return None

    # The rounding error in F/Q is bounded by 1/low. Each fixed peer holds
    # fewer than low shares, so its floor has at most two breakpoints here.
    lower = max(Fraction(0), rate + (phase - 1) / low)
    upper = rate + phase / low
    boundaries = {lower, upper}
    for shares in {fixed, *(quantity for quantity, _ in peers)} - {0}:
        for cents in range(ceil(lower * shares), floor(upper * shares) + 1):
            boundaries.add(Fraction(cents, shares))
    points = sorted(boundaries)
    singletons = [(ratio, ratio, True) for ratio in points]
    open_bands = [(left, right, False) for left, right in pairwise(points)]
    best = low - 1
    for left, right, includes_ends in singletons + open_bands:
        fee = fees_in_band((left + right) / 2)
        if fee is None:
            continue
        cap = high
        if focal_cost(cap, fee) > wallet:
            # The peers' commissions are constant in this band, so the
            # focal cost here is increasing apart from whole-cent plateaus.
            failing = cap
            cap = best
            while failing - cap > 1:
                midpoint = (cap + failing) // 2
                if focal_cost(midpoint, fee) <= wallet:
                    cap = midpoint
                else:
                    failing = midpoint
        quantity = largest_fee_ratio_quantity(
            best + 1, cap, rate, prefix_quantity, left, right,
            include_lower=includes_ends, include_upper=includes_ends,
        )
        if quantity is not None:
            best = quantity
            if best == high:
                return best
    return best if best >= low else None
