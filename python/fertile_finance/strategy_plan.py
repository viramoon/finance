"""Check compiler plan identity, exact decimals and runtime projections."""

from decimal import Decimal
from hashlib import sha256
import json
import re


BUNDLE_PLAN_VERSION = "fertile.finance/strategy-plan/v1"
SHA256 = re.compile(r"[0-9a-f]{64}")
_DECIMAL = re.compile(r"-?(?:0|[1-9][0-9]{0,9})(?:\.[0-9]{1,18})?")
_SLEEVE_FIELDS = {"recipe", "priority", "budget_weight"}
_MODULE_DECIMAL_FIELDS = {"threshold", "max_gross_weight", "max_weight_per_asset"}


def plan_decimal(value: object) -> Decimal:
    if not isinstance(value, str) or not _DECIMAL.fullmatch(value):
        raise ValueError("Expected a compiled strategy decimal string")
    return Decimal(value)


def canonical_sha256(value):
    encoded = json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"), allow_nan=False)
    return sha256(encoded.encode("utf-8")).hexdigest()


def _reject_floats(value):
    # The compiler emits decimal strings and normalizes integral parameters to JSON integers.
    if isinstance(value, float):
        raise ValueError("Unexpected floating-point plan value")
    if isinstance(value, dict):
        for item in value.values():
            _reject_floats(item)
    elif isinstance(value, (list, tuple)):
        for item in value:
            _reject_floats(item)


def _check_sleeves(declared, compiled, recipes):
    if not isinstance(declared, list) or not declared or not isinstance(compiled, list):
        raise ValueError("Invalid compiled sleeves")
    for row in (*declared, *compiled):
        if (not isinstance(row, dict) or row.keys() != _SLEEVE_FIELDS
                or not isinstance(row["recipe"], str) or row["recipe"] not in recipes
                or type(row["priority"]) is not int or row["priority"] < 0
                or not 0 <= plan_decimal(row["budget_weight"]) <= 1):
            raise ValueError("Invalid compiled sleeve")
    if (len({row["recipe"] for row in declared}) != len(declared)
            or compiled != sorted(declared, key=lambda row: (row["priority"], row["recipe"]))):
        raise ValueError("Compiled sleeves differ from the bundle")


def _check_runtime_fields(plan, spec):
    portfolio, execution = spec["portfolio"], spec["execution"]
    _check_sleeves(portfolio["sleeves"], plan["sleeves"], spec["recipes"])
    reentry = spec["policy"]["same_session_reentry"]
    if (plan["execution"] != execution["model"]
            or plan["inputs"]["bars"]["id"] != spec["data"]["bars"]
            or plan["inputs"]["universe"]["id"] != spec["universe"]["ref"]
            or type(reentry) is not bool or plan["conflict_rules"]["same_session_reentry"] is not reentry):
        raise ValueError("Compiled runtime fields differ from the bundle")
    for value in (portfolio["initial_cash"]["amount"], portfolio["max_gross_weight"],
                  portfolio["max_weight_per_asset"], execution["commission_bps"], execution["slippage_bps"]):
        plan_decimal(value)
    for module in spec["modules"].values():
        params = module["params"]
        for name in params.keys() & _MODULE_DECIMAL_FIELDS:
            plan_decimal(params[name])


def checked_bundle_plan(plan: object) -> dict:
    """Verify the content digest and runtime fields before evaluating a compiled plan."""
    try:
        if (isinstance(plan, dict) and plan["plan_version"] == BUNDLE_PLAN_VERSION
                and isinstance(plan["bundle"], dict)
                and SHA256.fullmatch(plan["source_sha256"])
                and plan["content_sha256"] == canonical_sha256(plan["bundle"])):
            _reject_floats(plan)
            _check_runtime_fields(plan, plan["bundle"]["spec"])
            return plan
    except (ValueError, TypeError, KeyError, AttributeError, RecursionError):
        pass
    raise ValueError("Invalid compiled StrategyBundle plan") from None
