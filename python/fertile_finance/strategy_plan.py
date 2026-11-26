"""Check the identity of a locally supplied F2 compiled plan."""

from hashlib import sha256
import json
import re


BUNDLE_PLAN_VERSION = "fertile.finance/strategy-plan/v1"
SHA256 = re.compile(r"[0-9a-f]{64}")


def canonical_sha256(value):
    encoded = json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"), allow_nan=False)
    return sha256(encoded.encode("utf-8")).hexdigest()


def checked_bundle_plan(plan: object) -> dict:
    """Verify the version and content digest before using trusted compiler output."""
    try:
        if (isinstance(plan, dict) and plan["plan_version"] == BUNDLE_PLAN_VERSION
                and isinstance(plan["bundle"], dict)
                and SHA256.fullmatch(plan["source_sha256"])
                and plan["content_sha256"] == canonical_sha256(plan["bundle"])):
            return plan
    except (ValueError, TypeError, KeyError, RecursionError):
        pass
    raise ValueError("Invalid compiled StrategyBundle plan")
