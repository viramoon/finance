"""Deterministic whole-share allocation under shared cash and exposure constraints."""

from collections import defaultdict
from dataclasses import dataclass
from fractions import Fraction


type Claim = tuple[str, str]  # instrument ID, recipe ID


@dataclass
class Limit:
    reason: str
    available: Fraction
    unit_costs: dict[Claim, Fraction]


def allocate(
    claims: dict[Claim, int], prices: dict[str, Fraction], priorities: dict[str, int],
    budgets: dict[str, Fraction], limits: list[Limit],
) -> tuple[dict[Claim, int], dict[Claim, str]]:
    """Fill claims by priority and budget weight, then round in stable ID order."""
    by_priority = defaultdict(list)
    limits_by_claim = defaultdict(list)
    for key in sorted(claims):
        by_priority[priorities[key[1]]].append(key)
    for limit in limits:
        for key, unit_cost in limit.unit_costs.items():
            limits_by_claim[key].append((limit, unit_cost))

    admitted = {}
    reasons = {}
    for priority in sorted(by_priority):
        keys = by_priority[priority]
        requested_value = defaultdict(Fraction)
        for instrument, recipe in keys:
            requested_value[recipe] += claims[instrument, recipe] * prices[instrument]
        # Allocate each sleeve a fraction of spend proportional to its fixed
        # budget. Within a sleeve, preserve each claim's share of desired cash.
        rates = {key: budgets[key[1]] * claims[key] / requested_value[key[1]] for key in keys}
        shares = {key: Fraction() for key in keys}
        remaining = {key: Fraction(claims[key]) for key in keys}
        available = [limit.available for limit in limits]
        blocked_by = {}
        active = set(keys)
        while active:
            usage = [
                sum(rates[key] * cost for key, cost in limit.unit_costs.items() if key in active)
                for limit in limits
            ]
            step = min(
                [remaining[key] / rates[key] for key in active]
                + [capacity / cost for capacity, cost in zip(available, usage) if cost]
            )
            for key in active:
                increment = rates[key] * step
                shares[key] += increment
                remaining[key] -= increment
            available = [capacity - step * cost for capacity, cost in zip(available, usage)]
            for limit, capacity in zip(limits, available):
                if capacity == 0:
                    for key in limit.unit_costs.keys() & active:
                        blocked_by.setdefault(key, limit.reason)
            active.difference_update(blocked_by)
            active = {key for key in active if remaining[key]}

        def consume(key, quantity):
            for limit, cost in limits_by_claim[key]:
                limit.available -= quantity * cost
            admitted[key] = admitted.get(key, 0) + quantity

        for key in keys:
            consume(key, int(shares[key]))
        for key in keys:
            if shares[key] > admitted[key] and all(limit.available >= cost for limit, cost in limits_by_claim[key]):
                consume(key, 1)
        for key in keys:
            if admitted[key] < claims[key]:
                reasons[key] = blocked_by.get(key, "whole_share_rounding")
    return admitted, reasons
