//! The read API serves an explicitly published F8 snapshot without running research code.

use crate::{ApiError, AppState, ErrorBody, error};
use axum::{
    Json, Router,
    extract::{
        Path, Query, State,
        rejection::{PathRejection, QueryRejection},
    },
    http::StatusCode,
    routing::get,
};
use chrono::{DateTime, NaiveDate, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{self, Read},
    path::{Component, Path as FsPath},
    sync::LazyLock,
};
use utoipa::{IntoParams, ToSchema};

const SNAPSHOT_PATH: &str = "finance-state/factor-research/read-v1.json";
const MAX_BYTES: u64 = 4 * 1024 * 1024;
const DEFAULT_LIMIT: u16 = 50;
const MAX_LIMIT: u16 = 100;

static FACTOR_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z0-9][a-z0-9_.-]{0,63}$").unwrap());
static VERSION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:0|[1-9][0-9]{0,9})(?:\.(?:0|[1-9][0-9]{0,9})){2}$").unwrap());
static DECIMAL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^-?(?:0|[1-9][0-9]*)(?:\.[0-9]+)?$").unwrap());

/// Exact fixed-point Decimal text at F8 precision.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(try_from = "String", into = "String")]
#[schema(value_type = String, pattern = r"^-?(?:0|[1-9][0-9]*)(?:\.[0-9]+)?$")]
struct FactorDecimal(String);

impl TryFrom<String> for FactorDecimal {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        DECIMAL
            .is_match(&value)
            .then_some(Self(value))
            .ok_or("expected finite research Decimal text")
    }
}

impl From<FactorDecimal> for String {
    fn from(value: FactorDecimal) -> Self {
        value.0
    }
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
enum Parameter {
    Text(String),
    Boolean(bool),
    Integer(FactorInteger),
    Decimal(FactorDecimal),
}

/// Signed integer in canonical decimal text.
#[derive(Debug, Deserialize, Serialize, ToSchema)]
#[serde(transparent)]
#[schema(value_type = String, pattern = r"^(?:0|-?[1-9][0-9]*)$")]
struct FactorInteger(#[serde(deserialize_with = "integer_text")] String);

fn integer_text<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let text = String::deserialize(deserializer)?;
    if text == "-0" || !unsigned_text(text.strip_prefix('-').unwrap_or(&text)) {
        return Err(serde::de::Error::custom("expected canonical integer text"));
    }
    Ok(text)
}

#[derive(Debug, Deserialize, Serialize, ToSchema, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum FactorState {
    Research,
    OutOfSampleValidation,
    ObserveOnly,
    BacktestSelectable,
    ResearchOnly,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
struct Transition {
    previous: FactorState,
    current: FactorState,
    reason: String,
    evidence_ref: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, ToSchema, PartialEq, Eq)]
struct FactorReadAccess {
    license: String,
    record_local: bool,
    derive_non_display: bool,
    redistribute: bool,
}

impl FactorReadAccess {
    fn allowed(&self) -> bool {
        !self.license.trim().is_empty()
            && self.record_local
            && self.derive_non_display
            && !self.redistribute
    }
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
struct SharpeComparison {
    main_sharpe: Option<FactorDecimal>,
    static_sharpe: Option<FactorDecimal>,
    difference: Option<FactorDecimal>,
    outperformance: Option<bool>,
    flip: Option<bool>,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
struct HistoricalDecision {
    verdict: String,
    passed: bool,
    cost_bps: FactorDecimal,
    rule: String,
    comparisons: BTreeMap<String, SharpeComparison>,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum FactorDetails {
    Factor {
        definition: String,
        parameters: BTreeMap<String, Parameter>,
        unit: String,
        asset_scope: Vec<String>,
        data_version: String,
        availability_rule: String,
        missing_policy: String,
        #[schema(pattern = r"^(?:0|[1-9][0-9]*)$")]
        warmup_sessions: String,
        history: Vec<Transition>,
    },
    HistoricalResearch {
        #[schema(pattern = "^[0-9a-f]{40}$")]
        source_commit: String,
        /// Repository-relative pointer to the original, hash-verified report.
        summary_ref: String,
        decision: HistoricalDecision,
    },
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
struct FactorReadItem {
    factor_id: String,
    version: String,
    state: FactorState,
    access: FactorReadAccess,
    selectable_for_backtest: bool,
    tradable: bool,
    details: FactorDetails,
}

impl FactorReadItem {
    fn valid(&self) -> bool {
        if !valid_key(&self.factor_id, &self.version) || !self.access.allowed() || self.tradable {
            return false;
        }
        match &self.details {
            FactorDetails::Factor {
                data_version,
                warmup_sessions,
                ..
            } => {
                self.state != FactorState::ResearchOnly
                    && self.selectable_for_backtest
                        == (self.state == FactorState::BacktestSelectable)
                    && !data_version.trim().is_empty()
                    && unsigned_text(warmup_sessions)
            }
            FactorDetails::HistoricalResearch {
                source_commit,
                summary_ref,
                ..
            } => {
                self.state == FactorState::ResearchOnly
                    && !self.selectable_for_backtest
                    && source_commit.len() == 40
                    && source_commit
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
                    && !summary_ref.is_empty()
                    && !summary_ref.contains('\\')
                    && FsPath::new(summary_ref)
                        .components()
                        .all(|part| matches!(part, Component::Normal(_)))
            }
        }
    }
}

fn unsigned_text(value: &str) -> bool {
    !value.is_empty()
        && (value.len() == 1 || !value.starts_with('0'))
        && value.bytes().all(|b| b.is_ascii_digit())
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
struct FactorPeer {
    factor_id: String,
    version: String,
    data_version: String,
    access: FactorReadAccess,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
struct FactorDistribution {
    count: u32,
    mean: Option<FactorDecimal>,
    sample_std: Option<FactorDecimal>,
    ratio: Option<FactorDecimal>,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
struct FactorPortfolioSummary {
    periods: u32,
    gross_observations: u32,
    net_observations: u32,
    turnover_observations: u32,
    cumulative_gross_return: Option<FactorDecimal>,
    cumulative_net_return: Option<FactorDecimal>,
    total_turnover: Option<FactorDecimal>,
    total_fee_fraction: Option<FactorDecimal>,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
enum EvaluationPhase {
    Train,
    Validation,
    OutOfSample,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
struct FactorWindowBounds {
    name: String,
    phase: EvaluationPhase,
    /// Inclusive first research session.
    start: NaiveDate,
    /// Inclusive last research session.
    end: NaiveDate,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
struct FactorWindowSummary {
    window: FactorWindowBounds,
    factor_coverage: Option<FactorDecimal>,
    scored_coverage: Option<FactorDecimal>,
    /// Information coefficient distributions keyed by horizon in sessions (`1`, `5`, `20`).
    ic: BTreeMap<String, FactorDistribution>,
    /// Cross-sectional rank correlation distributions keyed by peer `factor_id@version`.
    redundancy: BTreeMap<String, FactorDistribution>,
    quantiles: Vec<FactorPortfolioSummary>,
    benchmark: FactorPortfolioSummary,
    gross_information: FactorDistribution,
    net_information: FactorDistribution,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
struct FactorEvaluationItem {
    factor_id: String,
    version: String,
    data_version: String,
    evaluation_id: String,
    access: FactorReadAccess,
    peer_factors: Vec<FactorPeer>,
    cost_bps: FactorDecimal,
    quantile_count: u32,
    windows: Vec<FactorWindowSummary>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct FactorReadPage {
    #[schema(minimum = 1, maximum = 1)]
    schema_version: u8,
    /// Snapshot publication time, or null when no offline snapshot has been published.
    as_of: Option<DateTime<Utc>>,
    items: Vec<FactorReadItem>,
    next_cursor: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct FactorEvaluationPage {
    #[schema(minimum = 1, maximum = 1)]
    schema_version: u8,
    /// Snapshot publication time, or null when no offline snapshot has been published.
    as_of: Option<DateTime<Utc>>,
    items: Vec<FactorEvaluationItem>,
    next_cursor: Option<String>,
}

#[derive(Deserialize)]
struct Snapshot {
    schema_version: u8,
    as_of: DateTime<Utc>,
    factors: Vec<FactorReadItem>,
    evaluations: Vec<FactorEvaluationItem>,
}

impl Snapshot {
    fn valid(&self) -> bool {
        if self.schema_version != 1 {
            return false;
        }
        let mut factors = BTreeMap::new();
        for factor in &self.factors {
            if !factor.valid()
                || factors
                    .insert((factor.factor_id.as_str(), factor.version.as_str()), factor)
                    .is_some()
            {
                return false;
            }
        }
        let mut reports = BTreeSet::new();
        self.evaluations.iter().all(|report| {
            let key = (report.factor_id.as_str(), report.version.as_str());
            let Some(factor) = factors.get(&key) else {
                return false;
            };
            matches!(&factor.details, FactorDetails::Factor { data_version, .. } if *data_version == report.data_version)
                && report.access == factor.access
                && FACTOR_ID.is_match(&report.evaluation_id)
                && reports.insert((key, report.evaluation_id.as_str()))
                && report.peer_factors.iter().all(|peer| {
                    peer.access.allowed() && valid_key(&peer.factor_id, &peer.version)
                })
        })
    }
}

fn valid_key(id: &str, version: &str) -> bool {
    FACTOR_ID.is_match(id) && VERSION.is_match(version)
}

fn snapshot_invalid() -> ApiError {
    error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "factor_snapshot_invalid",
        "Invalid factor read snapshot",
    )
}

fn snapshot_unreadable() -> ApiError {
    error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "factor_snapshot_unreadable",
        "Cannot read factor snapshot",
    )
}

fn load_snapshot(data_root: &FsPath) -> Result<Option<Snapshot>, ApiError> {
    let path = data_root.join(SNAPSHOT_PATH);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => return Err(snapshot_invalid()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(snapshot_unreadable()),
    }
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|file| file.take(MAX_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|_| snapshot_unreadable())?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(snapshot_invalid());
    }
    let snapshot: Snapshot = serde_json::from_slice(&bytes).map_err(|_| snapshot_invalid())?;
    if !snapshot.valid() {
        return Err(snapshot_invalid());
    }
    Ok(Some(snapshot))
}

async fn read_snapshot(app: AppState) -> Result<Option<Snapshot>, ApiError> {
    tokio::task::spawn_blocking(move || load_snapshot(&app.data_root))
        .await
        .map_err(|_| snapshot_unreadable())?
}

#[derive(Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(crate) struct FactorQuery {
    factor_id: Option<String>,
    version: Option<String>,
    /// Exclusive cursor from `next_cursor`, ordered by `factor_id@version`.
    after: Option<String>,
    #[param(minimum = 1, maximum = 100)]
    limit: Option<u16>,
}

#[derive(Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(crate) struct EvaluationQuery {
    version: Option<String>,
    /// Exclusive cursor from `next_cursor`, ordered by `version@evaluation_id`.
    after: Option<String>,
    #[param(minimum = 1, maximum = 100)]
    limit: Option<u16>,
}

fn invalid_query() -> ApiError {
    error(
        StatusCode::BAD_REQUEST,
        "invalid_factor_query",
        "Invalid factor identifier, version or pagination",
    )
}

fn valid_cursor(cursor: &str, valid_pair: impl Fn(&str, &str) -> bool) -> bool {
    cursor
        .split_once('@')
        .is_some_and(|(first, second)| valid_pair(first, second))
}

fn limit(value: Option<u16>) -> Result<usize, ApiError> {
    let value = value.unwrap_or(DEFAULT_LIMIT);
    (1..=MAX_LIMIT)
        .contains(&value)
        .then_some(usize::from(value))
        .ok_or_else(invalid_query)
}

fn page<T>(
    items: impl Iterator<Item = T>,
    limit: usize,
    after: Option<&str>,
    cursor: impl Fn(&T) -> String,
) -> (Vec<T>, Option<String>) {
    let mut items: Vec<_> = items
        .map(|row| (cursor(&row), row))
        .filter(|(key, _)| after.is_none_or(|start| key.as_str() > start))
        .collect();
    items.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let next_cursor = (items.len() > limit).then(|| items[limit - 1].0.clone());
    items.truncate(limit);
    (items.into_iter().map(|(_, row)| row).collect(), next_cursor)
}

#[utoipa::path(get, path = "/api/v1/factors", operation_id = "factor_list", params(FactorQuery), responses(
    (status = 200, body = FactorReadPage),
    (status = 400, body = ErrorBody),
    (status = 500, body = ErrorBody)
))]
pub(crate) async fn list(
    State(app): State<AppState>,
    query: Result<Query<FactorQuery>, QueryRejection>,
) -> Result<Json<FactorReadPage>, ApiError> {
    let Query(query) = query.map_err(|_| invalid_query())?;
    let limit = limit(query.limit)?;
    if query
        .factor_id
        .as_deref()
        .is_some_and(|id| !FACTOR_ID.is_match(id))
        || query
            .version
            .as_deref()
            .is_some_and(|version| !VERSION.is_match(version))
        || query
            .after
            .as_deref()
            .is_some_and(|cursor| !valid_cursor(cursor, valid_key))
    {
        return Err(invalid_query());
    }
    let (as_of, factors) = read_snapshot(app)
        .await?
        .map_or((None, Vec::new()), |snapshot| {
            (Some(snapshot.as_of), snapshot.factors)
        });
    let (items, next_cursor) = page(
        factors.into_iter().filter(|item| {
            query
                .factor_id
                .as_ref()
                .is_none_or(|id| id == &item.factor_id)
                && query
                    .version
                    .as_ref()
                    .is_none_or(|version| version == &item.version)
        }),
        limit,
        query.after.as_deref(),
        |item| format!("{}@{}", item.factor_id, item.version),
    );
    Ok(Json(FactorReadPage {
        schema_version: 1,
        as_of,
        items,
        next_cursor,
    }))
}

#[utoipa::path(get, path = "/api/v1/factors/{id}/evaluations", operation_id = "factor_evaluations", params(("id" = String, Path), EvaluationQuery), responses(
    (status = 200, body = FactorEvaluationPage),
    (status = 400, body = ErrorBody),
    (status = 404, body = ErrorBody),
    (status = 500, body = ErrorBody)
))]
pub(crate) async fn evaluations(
    State(app): State<AppState>,
    path: Result<Path<String>, PathRejection>,
    query: Result<Query<EvaluationQuery>, QueryRejection>,
) -> Result<Json<FactorEvaluationPage>, ApiError> {
    let Path(id) = path.map_err(|_| invalid_query())?;
    let Query(query) = query.map_err(|_| invalid_query())?;
    let limit = limit(query.limit)?;
    if !FACTOR_ID.is_match(&id)
        || query
            .version
            .as_deref()
            .is_some_and(|version| !VERSION.is_match(version))
        || query
            .after
            .as_deref()
            .is_some_and(|cursor| !valid_cursor(cursor, |version, id| valid_key(id, version)))
    {
        return Err(invalid_query());
    }
    let snapshot = read_snapshot(app).await?;
    if snapshot.as_ref().is_some_and(|snapshot| {
        !snapshot.factors.iter().any(|item| {
            item.factor_id == id
                && query
                    .version
                    .as_ref()
                    .is_none_or(|version| version == &item.version)
        })
    }) {
        return Err(error(
            StatusCode::NOT_FOUND,
            "factor_not_found",
            "No registered factor for this query",
        ));
    }
    let (as_of, evaluations) = snapshot.map_or((None, Vec::new()), |snapshot| {
        (Some(snapshot.as_of), snapshot.evaluations)
    });
    let (items, next_cursor) = page(
        evaluations.into_iter().filter(|item| {
            item.factor_id == id
                && query
                    .version
                    .as_ref()
                    .is_none_or(|version| version == &item.version)
        }),
        limit,
        query.after.as_deref(),
        |item| format!("{}@{}", item.version, item.evaluation_id),
    );
    Ok(Json(FactorEvaluationPage {
        schema_version: 1,
        as_of,
        items,
        next_cursor,
    }))
}

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/factors", get(list))
        .route("/api/v1/factors/{id}/evaluations", get(evaluations))
}
