"""Load local factor archives and resolve versioned point-in-time research inputs."""

from bisect import bisect_right
from collections.abc import Callable, Collection, Iterable, Mapping
import csv
from dataclasses import dataclass, field
from datetime import UTC, date, datetime
from decimal import Decimal, InvalidOperation
import io
import json
from operator import attrgetter
from pathlib import Path
import re
from types import MappingProxyType
from zipfile import ZipFile, is_zipfile

from .historical_types import EpochDiscard, EpochPolicy, guard_epoch


FRED_SERIES_ID = re.compile(r"[A-Z0-9][A-Z0-9_]*")
_FRENCH_COLUMNS = ("Mkt-RF", "SMB", "HML", "RF")
_FRENCH_MOMENTUM = ("Mom",)
_FRENCH_MISSING = (Decimal("-99.99"), Decimal("-999"))
_FACTOR_ID = re.compile(r"[a-z0-9][a-z0-9_.-]{0,63}")
_FACTOR_VERSION = re.compile(r"(?:0|[1-9][0-9]{0,9})(?:\.(?:0|[1-9][0-9]{0,9})){2}")

type FactorParameter = str | bool | int | Decimal


def _required_text(value: str, name: str) -> None:
    if not isinstance(value, str) or not value.strip():
        raise ValueError(f"{name} must be nonempty text")


def _utc(value: datetime, name: str) -> datetime:
    if not isinstance(value, datetime) or value.utcoffset() is None:
        raise ValueError(f"{name} must be a timezone-aware datetime")
    return value.astimezone(UTC)


def _require_session_count(value: int, name: str) -> None:
    if type(value) is not int or value < 0:
        raise ValueError(f"{name} must be a nonnegative integer")


def _require_decimal(value: Decimal, name: str) -> None:
    if not isinstance(value, Decimal) or not value.is_finite():
        raise ValueError(f"{name} must be a finite Decimal")


@dataclass(frozen=True)
class FactorDefinition:
    """A versioned research factor and its declared input and usage policy."""

    factor_id: str
    version: str
    definition: str
    parameters: Mapping[str, FactorParameter]
    unit: str
    asset_scope: tuple[str, ...]
    data_version: str
    availability_rule: str
    missing_policy: str
    warmup_sessions: int
    license: str
    _parameter_types: tuple[tuple[str, type], ...] = field(init=False, repr=False)

    def __post_init__(self):
        if not isinstance(self.factor_id, str) or not _FACTOR_ID.fullmatch(self.factor_id):
            raise ValueError("factor_id must be a lowercase local identifier")
        if not isinstance(self.version, str) or not _FACTOR_VERSION.fullmatch(self.version):
            raise ValueError("version must contain three integer segments without leading zeros")
        for name in ("definition", "unit", "data_version", "availability_rule", "license"):
            _required_text(getattr(self, name), name)
        if self.missing_policy != "exclude":
            raise ValueError("missing_policy must be 'exclude'")
        _require_session_count(self.warmup_sessions, "warmup_sessions")

        if not isinstance(self.asset_scope, Collection) or isinstance(self.asset_scope, (str, bytes)):
            raise ValueError("asset_scope must be a collection of asset types")
        scope = tuple(self.asset_scope)
        for asset_type in scope:
            _required_text(asset_type, "asset_scope item")
        if not scope or len(set(scope)) != len(scope):
            raise ValueError("asset_scope must contain unique asset types")
        object.__setattr__(self, "asset_scope", tuple(sorted(scope)))

        if not isinstance(self.parameters, Mapping):
            raise ValueError("parameters must be a mapping")
        parameters = dict(self.parameters)
        for name, value in parameters.items():
            _required_text(name, "parameter name")
            if type(value) not in (str, bool, int):
                _require_decimal(value, f"parameter {name}")
        object.__setattr__(self, "parameters", MappingProxyType(parameters))
        # Python considers True, 1 and Decimal(1) equal, but their parameter types differ.
        parameter_types = tuple((name, type(value)) for name, value in sorted(parameters.items()))
        object.__setattr__(self, "_parameter_types", parameter_types)


@dataclass(frozen=True)
class FactorObservation:
    """One published vintage of a score and its completed input session count."""

    asset_id: str
    observation_ts: datetime
    first_available_ts: datetime
    value: Decimal | None
    history_sessions: int = 0
    valid_until_ts: datetime | None = None

    def __post_init__(self):
        _required_text(self.asset_id, "asset_id")
        observed = _utc(self.observation_ts, "observation_ts")
        available = _utc(self.first_available_ts, "first_available_ts")
        if observed > available:
            raise ValueError("first_available_ts predates observation_ts")
        object.__setattr__(self, "observation_ts", observed)
        object.__setattr__(self, "first_available_ts", available)
        if self.valid_until_ts is not None:
            end = _utc(self.valid_until_ts, "valid_until_ts")
            if end <= observed:
                raise ValueError("valid_until_ts must follow observation_ts")
            object.__setattr__(self, "valid_until_ts", end)
        if self.value is not None:
            _require_decimal(self.value, "value")
        _require_session_count(self.history_sessions, "history_sessions")


@dataclass(frozen=True)
class UniverseMembership:
    """An effective membership change or revision with published exposure metadata."""

    asset_id: str
    asset_type: str
    effective_ts: datetime
    first_available_ts: datetime
    is_member: bool
    industry: str | None = None
    size: Decimal | None = None

    def __post_init__(self):
        _required_text(self.asset_id, "asset_id")
        _required_text(self.asset_type, "asset_type")
        object.__setattr__(self, "effective_ts", _utc(self.effective_ts, "effective_ts"))
        object.__setattr__(self, "first_available_ts", _utc(self.first_available_ts, "first_available_ts"))
        if type(self.is_member) is not bool:
            raise ValueError("is_member must be a boolean")
        if self.industry is not None:
            _required_text(self.industry, "industry")
        if self.size is not None:
            _require_decimal(self.size, "size")
            if self.size <= 0:
                raise ValueError("size must be positive")


@dataclass(frozen=True)
class FactorSnapshotRow:
    asset_id: str
    value: Decimal | None
    industry: str | None
    size: Decimal | None
    missing_reason: str | None
    observation_ts: datetime | None
    first_available_ts: datetime | None
    membership_effective_ts: datetime
    membership_available_ts: datetime


@dataclass(frozen=True)
class FactorSnapshot:
    definition: FactorDefinition
    decision_ts: datetime
    rows: tuple[FactorSnapshotRow, ...]


class FactorLookaheadError(ValueError):
    """A submitted snapshot includes data beyond its decision time."""


def _index_history[Row: (FactorObservation, UniverseMembership)](
    rows: Iterable[Row], row_type: type[Row], effective_at: Callable[[Row], datetime],
) -> dict[str, tuple[Row, ...]]:
    indexed: dict[str, list[Row]] = {}
    seen = set()
    for row in rows:
        if not isinstance(row, row_type):
            raise ValueError(f"Expected {row_type.__name__} records")
        key = (row.asset_id, effective_at(row), row.first_available_ts)
        if key in seen:
            raise ValueError(f"Duplicate {row_type.__name__} vintage for {row.asset_id}")
        seen.add(key)
        indexed.setdefault(row.asset_id, []).append(row)
    return {
        asset_id: tuple(sorted(history, key=lambda row: (effective_at(row), row.first_available_ts)))
        for asset_id, history in sorted(indexed.items())
    }


def _visible_record[Row: (FactorObservation, UniverseMembership)](
    history: tuple[Row, ...], decision_ts: datetime, effective_at: Callable[[Row], datetime],
) -> Row | None:
    stop = bisect_right(history, decision_ts, key=effective_at)
    for index in range(stop - 1, -1, -1):
        row = history[index]
        if row.first_available_ts <= decision_ts:
            return row
    return None


_OBSERVED_AT = attrgetter("observation_ts")
_EFFECTIVE_AT = attrgetter("effective_ts")


@dataclass(frozen=True, init=False, eq=False)
class PointInTimeFactor:
    """Replay a frozen factor series and historical universe without future vintages."""

    definition: FactorDefinition
    _observations: Mapping[str, tuple[FactorObservation, ...]] = field(repr=False)
    _membership: Mapping[str, tuple[UniverseMembership, ...]] = field(repr=False)

    def __init__(
        self,
        definition: FactorDefinition,
        observations: Iterable[FactorObservation],
        membership: Iterable[UniverseMembership],
    ):
        if not isinstance(definition, FactorDefinition):
            raise ValueError("Expected a FactorDefinition")
        observed = _index_history(observations, FactorObservation, _OBSERVED_AT)
        members = _index_history(membership, UniverseMembership, _EFFECTIVE_AT)
        for history in members.values():
            if any(row.asset_type not in definition.asset_scope for row in history):
                raise ValueError("UniverseMembership is outside the factor asset_scope")
        object.__setattr__(self, "definition", definition)
        object.__setattr__(self, "_observations", MappingProxyType(observed))
        object.__setattr__(self, "_membership", MappingProxyType(members))

    def snapshot(self, decision_ts: datetime) -> FactorSnapshot:
        """Select members and their most recent available score as of decision_ts."""
        cutoff = _utc(decision_ts, "decision_ts")
        rows = []
        for asset_id, history in self._membership.items():
            member = _visible_record(history, cutoff, _EFFECTIVE_AT)
            if member is None or not member.is_member:
                continue
            score = _visible_record(self._observations.get(asset_id, ()), cutoff, _OBSERVED_AT)
            reason = self._missing_reason(score, cutoff)
            rows.append(FactorSnapshotRow(
                asset_id=asset_id,
                value=score.value if score is not None and reason is None else None,
                industry=member.industry,
                size=member.size,
                missing_reason=reason,
                observation_ts=score.observation_ts if score is not None else None,
                first_available_ts=score.first_available_ts if score is not None else None,
                membership_effective_ts=member.effective_ts,
                membership_available_ts=member.first_available_ts,
            ))
        return FactorSnapshot(self.definition, cutoff, tuple(rows))

    def _missing_reason(self, score: FactorObservation | None, cutoff: datetime) -> str | None:
        if score is None:
            return "missing_observation"
        if score.valid_until_ts is not None and cutoff >= score.valid_until_ts:
            return "expired_observation"
        if score.history_sessions < self.definition.warmup_sessions:
            return "insufficient_warmup"
        if score.value is None:
            return "missing_value"
        return None


def validate_factor_snapshot(
    definition: FactorDefinition,
    decision_ts: datetime,
    observations: Iterable[FactorObservation],
    membership: Iterable[UniverseMembership],
) -> FactorSnapshot:
    """Validate a submitted decision snapshot, rejecting future disclosures explicitly."""
    cutoff = _utc(decision_ts, "decision_ts")
    factor = PointInTimeFactor(definition, observations, membership)
    for history in factor._observations.values():
        for score in history:
            if score.first_available_ts > cutoff:
                raise FactorLookaheadError(f"{score.asset_id}: factor first_available_ts follows decision_ts")
    for history in factor._membership.values():
        for member in history:
            if max(member.effective_ts, member.first_available_ts) > cutoff:
                raise FactorLookaheadError(f"{member.asset_id}: membership is effective or published after decision_ts")
    return factor.snapshot(cutoff)


@dataclass(frozen=True)
class FrenchFactor:
    session: date
    mkt_rf: Decimal | None
    smb: Decimal | None
    hml: Decimal | None
    rf: Decimal | None
    mom: Decimal | None = None
    available_on: date | None = None


@dataclass(frozen=True)
class FrenchDaily:
    rows: tuple[FrenchFactor, ...]
    dropped_pre_epoch: EpochDiscard


@dataclass(frozen=True)
class FrenchRF:
    session: date
    rf: Decimal | None
    available_on: date | None


@dataclass(frozen=True)
class FrenchRiskFree:
    rows: tuple[FrenchRF, ...]
    dropped_pre_epoch: EpochDiscard


@dataclass(frozen=True)
class FredObservation:
    observation_date: date
    value: Decimal | None
    realtime_start: date | None = None
    realtime_end: date | None = None


@dataclass(frozen=True)
class FredDaily:
    series_id: str
    rows: tuple[FredObservation, ...]
    dropped_pre_epoch: EpochDiscard


def _french_text(path: str | Path) -> str:
    path = Path(path)
    if not is_zipfile(path):
        return path.read_text(encoding="utf-8-sig")
    with ZipFile(path) as archive:
        tables = [item for item in archive.infolist()
                  if not item.is_dir() and Path(item.filename).suffix.lower() in {".csv", ".txt"}]
        if len(tables) != 1:
            raise ValueError("Ken French ZIP must contain exactly one CSV or TXT table")
        return archive.read(tables[0]).decode("utf-8-sig")


def _decimal(token: str | Decimal, source: str) -> Decimal:
    if not isinstance(token, (str, Decimal)):
        raise ValueError(f"Invalid {source} decimal: {token!r}")
    try:
        value = Decimal(token)
    except InvalidOperation:
        raise ValueError(f"Invalid {source} decimal: {token!r}") from None
    if not value.is_finite():
        raise ValueError(f"Invalid {source} decimal: {token!r}")
    return value


def _french_return(token: str) -> Decimal | None:
    if not token:
        return None
    percent = _decimal(token, "Ken French")
    if percent in _FRENCH_MISSING:
        return None
    sign, digits, exponent = percent.as_tuple()
    return Decimal((sign, digits, exponent - 2))


def _daily_french_table(path: str | Path, accepted_headers: tuple[tuple[str, ...], ...]):
    lines = iter(_french_text(path).splitlines())
    for line in lines:
        csv_format = "," in line
        fields = [field.strip() for field in next(csv.reader([line]))] if csv_format else line.split()
        if fields and fields[0].lower() in {"", "date"}:
            fields = fields[1:]
        columns = tuple(fields)
        if columns in accepted_headers:
            break
    else:
        raise ValueError("Ken French archive lacks the expected daily factor header")

    table = {}
    for line in lines:
        if not line.strip():
            continue
        if line.strip().lower().startswith("copyright"):
            break
        fields = [field.strip() for field in next(csv.reader([line]))] if csv_format else line.split()
        if len(fields) != len(columns) + 1 or not re.fullmatch(r"[0-9]{8}", fields[0]):
            raise ValueError("Expected Ken French daily data with YYYYMMDD dates")
        session = date.fromisoformat(fields[0])
        if session in table:
            raise ValueError(f"Duplicate Ken French session: {session}")
        table[session] = tuple(_french_return(field) for field in fields[1:])
    if not table:
        raise ValueError("Ken French daily table has no observations")
    return columns, table


def load_ken_french(
    factors_path: str | Path,
    *,
    momentum_path: str | Path | None = None,
    pre_epoch: EpochPolicy = "reject",
) -> FrenchDaily:
    """Load daily factor fractions by session, with unknown publication dates."""
    combined = _FRENCH_COLUMNS + _FRENCH_MOMENTUM
    columns, factors = _daily_french_table(factors_path, (_FRENCH_COLUMNS, combined))
    if columns == combined and momentum_path is not None:
        raise ValueError("Momentum was supplied in two Ken French tables")
    momentum = {}
    if momentum_path is not None:
        _, momentum = _daily_french_table(momentum_path, (_FRENCH_MOMENTUM,))
    rows = []
    for session in sorted(factors.keys() | momentum.keys()):
        mkt_rf, smb, hml, rf, *embedded_mom = factors.get(session, (None, None, None, None))
        (mom,) = momentum.get(session, embedded_mom or (None,))
        rows.append(FrenchFactor(session, mkt_rf, smb, hml, rf, mom))
    kept, dropped = guard_epoch(rows, date_of=lambda row: row.session, policy=pre_epoch)
    return FrenchDaily(kept, dropped)


def load_ken_french_rf(factors_path: str | Path, *, pre_epoch: EpochPolicy = "reject") -> FrenchRiskFree:
    """Load the daily fractional risk-free return and its availability field."""
    result = load_ken_french(factors_path, pre_epoch=pre_epoch)
    return FrenchRiskFree(tuple(FrenchRF(row.session, row.rf, row.available_on) for row in result.rows),
                          result.dropped_pre_epoch)


def _iso_date(token: str) -> date:
    if not isinstance(token, str) or not re.fullmatch(r"[0-9]{4}-[0-9]{2}-[0-9]{2}", token):
        raise ValueError(f"Expected a FRED date in YYYY-MM-DD format: {token!r}")
    return date.fromisoformat(token)


def _fred_value(token: str | Decimal) -> Decimal | None:
    return None if token in ("", ".") else _decimal(token, "FRED")


def _fred_csv(content: str, series_id: str) -> list[FredObservation]:
    reader = csv.reader(io.StringIO(content, newline=""), strict=True)
    header = next(reader, [])
    if len(header) != 2 or header[0] not in {"observation_date", "DATE"} or header[1] != series_id:
        raise ValueError(f"Expected FRED graph CSV for {series_id}")
    rows = []
    for fields in reader:
        if not fields:
            continue
        if len(fields) != 2:
            raise ValueError("Expected a date and value in each FRED CSV row")
        day, value = fields
        rows.append(FredObservation(_iso_date(day), _fred_value(value.strip())))
    return rows


def _fred_json(content: str) -> list[FredObservation]:
    payload = json.loads(content, parse_float=Decimal, parse_int=Decimal, parse_constant=Decimal)
    if not isinstance(payload, dict) or not isinstance(payload.get("observations"), list):
        raise ValueError("Expected a FRED series observations JSON response")
    if payload.get("units", "lin") != "lin" or payload.get("output_type", 1) != 1:
        raise ValueError("Expected FRED observations by realtime period in native units")
    rows = []
    for item in payload["observations"]:
        if not isinstance(item, dict) or not {"date", "value", "realtime_start", "realtime_end"} <= item.keys():
            raise ValueError("Expected a date, value and realtime period in each FRED JSON observation")
        day = _iso_date(item["date"])
        start = _iso_date(item["realtime_start"])
        end = _iso_date(item["realtime_end"])
        if start > end:
            raise ValueError(f"Invalid FRED realtime period for {day}")
        rows.append(FredObservation(day, _fred_value(item["value"]), start, end))
    return rows


def load_fred(path: str | Path, *, series_id: str, pre_epoch: EpochPolicy = "reject") -> FredDaily:
    """Load a daily FRED archive in its source units, with one vintage per date."""
    if not isinstance(series_id, str) or not FRED_SERIES_ID.fullmatch(series_id):
        raise ValueError("Use a FRED series ID containing uppercase letters, digits or underscores")
    content = Path(path).read_text(encoding="utf-8-sig")
    try:
        rows = _fred_json(content) if content.lstrip().startswith("{") else _fred_csv(content, series_id)
    except csv.Error as exc:
        raise ValueError("Malformed FRED graph CSV") from exc
    if not rows:
        raise ValueError(f"FRED series {series_id} has no observations")
    days = {row.observation_date for row in rows}
    if len(days) != len(rows):
        raise ValueError("FRED archive must contain at most one vintage per observation date")
    rows.sort(key=lambda row: row.observation_date)
    kept, dropped = guard_epoch(rows, date_of=lambda row: row.observation_date, policy=pre_epoch)
    return FredDaily(series_id, kept, dropped)
