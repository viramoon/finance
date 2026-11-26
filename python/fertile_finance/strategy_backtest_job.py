"""Execute the compiled strategy and its pinned replay in the worker child."""

from contextlib import closing
from dataclasses import asdict, is_dataclass
from datetime import date, datetime, timedelta, timezone
from decimal import Decimal, localcontext
from importlib.metadata import version
from pathlib import Path
import sqlite3
from uuid import uuid4

from .research_catalog import EPOCH, LATEST, ResearchError
from .research_decimal import EXACT_RESEARCH_CONTEXT
from .strategy_indicators import BUILTIN_EVALUATOR_ID
from .strategy_replay import MAX_REPLAY_BYTES, SHA256, canonical_sha256, load_strategy_replay


def _checked_plan(request):
    try:
        plan = request["plan"]
        if (plan["plan_version"] != "fertile.finance/strategy-plan/v1"
                or not SHA256.fullmatch(plan["source_sha256"])
                or plan["content_sha256"] != canonical_sha256(plan["bundle"])):
            raise ValueError("Invalid compiled plan")
        return plan
    except (ValueError, TypeError, KeyError, RecursionError):
        raise ResearchError("strategy_plan_invalid", "Could not verify the stored compiled plan") from None


def _request_interval(request):
    def midnight(value):
        stamp = datetime.fromisoformat(value)
        if stamp.utcoffset() != timedelta(0) or stamp.time() != datetime.min.time():
            raise ValueError("Expected midnight UTC")
        return stamp.date()

    try:
        start, end = midnight(request["start"]), midnight(request["end"])
        if EPOCH <= start < end <= LATEST + timedelta(days=1):
            return start, end
    except (ValueError, TypeError, KeyError):
        pass
    raise ResearchError("invalid_date", "Use an increasing UTC midnight interval within 1970–2100 sessions")


def _pinned_bytes(digest, snapshot, queue):
    if not isinstance(digest, str) or not SHA256.fullmatch(digest):
        raise ResearchError("strategy_snapshot_invalid", "Invalid stored replay digest")
    try:
        if isinstance(snapshot, bytes):
            return snapshot
        if snapshot is not None:
            with Path(snapshot).open("rb") as handle:
                return handle.read(MAX_REPLAY_BYTES + 1)
        if queue is not None:
            with closing(sqlite3.connect(f"{queue.as_uri()}?mode=ro", uri=True, timeout=5)) as db:
                row = db.execute(
                    "SELECT CASE WHEN length(content) <= ? THEN content END "
                    "FROM strategy_backtest_snapshots WHERE snapshot_sha256=?",
                    (MAX_REPLAY_BYTES, digest),
                ).fetchone()
            if row is not None:
                if not isinstance(row[0], bytes):
                    raise ResearchError("strategy_snapshot_invalid", "Invalid stored replay bytes")
                return row[0]
    except (OSError, sqlite3.Error):
        pass
    raise ResearchError("strategy_snapshot_missing", "Could not read the pinned local replay")


def _wire(value):
    if isinstance(value, Decimal):
        if not value.is_finite():
            raise ValueError("Backtest report requires finite Decimal values")
        return format(value, "f")
    if isinstance(value, date):
        return value.isoformat()
    if is_dataclass(value):
        return _wire(asdict(value))
    if isinstance(value, dict):
        return {_wire(key): _wire(item) for key, item in value.items()}
    if isinstance(value, (list, tuple)):
        return [_wire(item) for item in value]
    if value is None or type(value) in (bool, int, str):
        return value
    raise TypeError("Unsupported or inexact backtest report value")


def _nautilus_source_commit():
    toolchain = Path(__file__).resolve().parents[2] / "toolchain.env"
    return next(line.removeprefix("NAUTILUS_COMMIT=") for line in toolchain.read_text().splitlines()
                if line.startswith("NAUTILUS_COMMIT="))


def _manifest(plan, replay, request):
    bundle = plan["bundle"]
    spec = bundle["spec"]
    execution = spec["execution"]
    components = {row["component"] for row in (*plan["factors"], *plan["modules"])}
    components.add(plan["execution"])
    return {
        "schema": "fertile.finance/run-manifest/v1",
        "plan_version": plan["plan_version"],
        "content_sha256": plan["content_sha256"],
        "source_sha256": plan["source_sha256"],
        "bundle": bundle,
        "components": sorted(components),
        "indicator_evaluator": BUILTIN_EVALUATOR_ID,
        "engine": {
            "id": spec["engine"],
            "nautilus_version": version("nautilus_trader"),
            "nautilus_source_commit": _nautilus_source_commit(),
            "strategy_executor_version": "fertile.finance/config-strategy-backtest/v1",
        },
        "data": replay.data,
        "calendar": {
            **replay.calendar,
            "provider": "exchange_calendars",
            "provider_version": version("exchange-calendars"),
        },
        "interval": {
            "start_inclusive": request["start"],
            "end_exclusive": request["end"],
            "first_session": str(replay.start),
            "last_session": str(replay.end),
            "warmup_sessions": spec["clock"]["warmup_sessions"],
        },
        "availability": replay.availability,
        "costs": {
            "execution_model": plan["execution"],
            "commission_bps": execution["commission_bps"],
            "slippage_bps": execution["slippage_bps"],
            "currency": "USD",
            "commission_rounding": "half_up_to_cent_per_external_fill",
            "slippage_price_rounding": "half_up_to_cent_with_one_cent_sell_floor",
            "internal_netting_price": "raw_close_without_commission_or_slippage",
        },
        "conflict_rules": plan["conflict_rules"],
    }


def run_strategy_backtest_job(request, *, snapshot=None, queue=None):
    plan = _checked_plan(request)
    start, end = _request_interval(request)
    digest = request.get("snapshot_sha256")
    raw = _pinned_bytes(digest, snapshot, queue)
    replay = load_strategy_replay(raw, digest, plan, start, end)
    from .strategy_backtest import run_config_strategy_backtest

    result = run_config_strategy_backtest(
        plan=plan, assets=replay.assets, universe=replay.membership, start=replay.start, end=replay.end,
        history_start=replay.history_start,
    )
    equity = {day: value for day, value in result.equity.items() if replay.start <= day <= replay.end}
    last_mark = max((day for day, value in equity.items() if value is not None), default=None)
    if last_mark is None:
        raise ResearchError("no_data", "No complete portfolio valuation in the requested interval")
    with localcontext(EXACT_RESEARCH_CONTEXT):
        pnl = equity[last_mark] - result.initial_cash
    return _wire({
        "run_id": str(uuid4()),
        "strategy": "StrategyBundle",
        "instrument": "PORTFOLIO",
        "start": request["start"],
        "end": request["end"],
        "total_orders": result.total_orders,
        "total_positions": result.total_positions,
        "pnl": pnl,
        "currency": "USD",
        "generated_at": datetime.now(timezone.utc),
        "run_manifest": _manifest(plan, replay, request),
        "strategy_report": {
            "initial_cash": result.initial_cash,
            "cash": result.cash,
            "equity": equity,
            "pnl_as_of": last_mark,
            "positions": result.positions,
            "fills": result.fills,
            "attribution": result.attribution,
            "sleeves": result.sleeves,
            "gaps": result.gaps,
            "windows": result.windows,
        },
    })
