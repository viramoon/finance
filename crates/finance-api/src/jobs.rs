//! Durable, single-host submission. SQLite's write transaction serializes the
//! idempotency lookup, capacity check and insertion across API processes.
use crate::{ApiError, AppState, BacktestSummary, ErrorBody, error, research};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
    routing::get,
};
use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

const CAPACITY: i64 = 128;
const SAMPLE_FIRST: i64 = 1704150060;
const SAMPLE_LAST: i64 = SAMPLE_FIRST + 8 * 60;
const FIRST_RESEARCH_DAY: NaiveDate = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
const LAST_RESEARCH_DAY: NaiveDate = NaiveDate::from_ymd_opt(2100, 12, 31).unwrap();

#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct StrategyParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fast_period: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slow_period: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shares: Option<u16>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct JobRequest {
    pub strategy: String,
    pub instrument: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub params: StrategyParams,
}

impl JobRequest {
    fn validate(&self) -> Result<(), ApiError> {
        if self.start >= self.end {
            return Err(invalid("Start must precede end"));
        }
        let params = (
            self.params.fast_period,
            self.params.slow_period,
            self.params.shares,
        );
        match self.strategy.as_str() {
            "DemoStrategy"
                if self.instrument == "6EH4.XCME"
                    && params == (None, None, None)
                    && [self.start, self.end].iter().all(|t| {
                        (SAMPLE_FIRST..=SAMPLE_LAST).contains(&t.timestamp())
                            && t.timestamp() % 60 == 0
                            && t.timestamp_subsec_nanos() == 0
                    }) =>
            {
                Ok(())
            }
            "DemoStrategy" => Err(invalid(
                "Invalid DemoStrategy instrument, minutes or parameters",
            )),
            "SmaCrossover" => {
                if !research::instrument_valid(&self.instrument) {
                    return Err(invalid("Choose a catalog XNYS stock or ETF"));
                }
                if ![self.start, self.end].iter().all(|t| {
                    (FIRST_RESEARCH_DAY..=LAST_RESEARCH_DAY).contains(&t.date_naive())
                        && t.time() == NaiveTime::MIN
                }) {
                    return Err(invalid(
                        "Use dates from 1970-01-01 through 2100-12-31 at UTC midnight",
                    ));
                }
                if !matches!(params, (Some(fast), Some(slow), Some(shares))
                    if 2 <= fast && fast < slow && slow <= 250 && (1..=1000).contains(&shares))
                {
                    return Err(invalid(
                        "Require 2 <= fast_period < slow_period <= 250 and 1..1000 shares",
                    ));
                }
                Ok(())
            }
            _ => Err(invalid("Unsupported strategy")),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    TimedOut,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct Job {
    pub id: i64,
    pub status: JobStatus,
    pub request: JobRequest,
    pub attempt: u8,
    pub created_at: DateTime<Utc>,
    pub summary: Option<BacktestSummary>,
    pub error: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct Accepted {
    pub id: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pagination {
    limit: Option<u32>,
    cursor: Option<i64>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct Page {
    pub items: Vec<Job>,
    pub next_cursor: Option<i64>,
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/backtests", get(list).post(submit))
        .route("/api/v1/backtests/{id}", get(detail))
        .route("/api/v1/backtests/by-key", get(by_key))
        .layer(DefaultBodyLimit::max(8192))
}
fn invalid(message: &str) -> ApiError {
    error(StatusCode::UNPROCESSABLE_ENTITY, "invalid_request", message)
}
fn storage_error(err: impl std::fmt::Display) -> ApiError {
    eprintln!("backtest queue: {err}");
    error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "queue_error",
        "Cannot access job queue",
    )
}
fn submission_key(headers: &HeaderMap) -> Result<String, ApiError> {
    let mut keys = headers.get_all("Idempotency-Key").iter();
    keys.next()
        .and_then(|v| v.to_str().ok())
        .filter(|v| {
            (1..=128).contains(&v.len())
                && v.bytes().all(|c| c.is_ascii_graphic())
                && keys.next().is_none()
        })
        .map(str::to_owned)
        .ok_or_else(|| {
            invalid("One Idempotency-Key with 1..128 visible ASCII characters is required")
        })
}
async fn db<T: Send + 'static>(
    state: AppState,
    op: impl FnOnce(&mut Connection) -> Result<T, ApiError> + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(move || {
        // An absent data root may indicate a missing persistent mount.
        // Create children one level at a time and never recreate that root.
        let dir = state.data_root.join("finance-state/backtests");
        for path in [state.data_root.join("finance-state"), dir.clone()] {
            match std::fs::create_dir(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && path.is_dir() => {}
                Err(e) => return Err(storage_error(e)),
            }
        }
        let mut conn = Connection::open(dir.join("jobs.sqlite3")).map_err(storage_error)?;
        conn.busy_timeout(std::time::Duration::from_secs(3))
            .map_err(storage_error)?;
        conn.execute_batch(include_str!("../../../sql/backtest-queue.sql"))
            .map_err(storage_error)?;
        op(&mut conn)
    })
    .await
    .map_err(storage_error)?
}
#[utoipa::path(post, path = "/api/v1/backtests", params(("Idempotency-Key" = String, Header, description="Stable submission key, 1–128 visible ASCII characters")), request_body = JobRequest, responses(
    (status=202,body=Accepted),(status=409,body=ErrorBody),(status=422,body=ErrorBody),(status=429,body=ErrorBody),(status=500,body=ErrorBody)))]
pub async fn submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    request: Result<Json<JobRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<Accepted>), ApiError> {
    let key = submission_key(&headers)?;
    let req = request
        .map_err(|_| invalid("Invalid job request or parameters"))?
        .0;
    req.validate()?;
    let request = serde_json::to_string(&req).map_err(storage_error)?;
    let research_root = state.research_root.clone();
    let id = db(state, move |conn| {
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        let existing: Option<(i64, String)> = tx
            .query_row("SELECT id,request FROM jobs WHERE idem=?1", [&key], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .optional()
            .map_err(storage_error)?;
        if let Some((id, stored)) = existing {
            if stored != request {
                return Err(error(
                    StatusCode::CONFLICT,
                    "key_conflict",
                    "Key already used for another request",
                ));
            }
            return Ok(id);
        }
        if req.strategy == "SmaCrossover" {
            let dataset = research::dataset(&research_root, &req.instrument)?;
            if !dataset.is_some_and(|d| {
                req.start.date_naive() <= d.last_session && req.end.date_naive() >= d.first_session
            }) {
                return Err(error(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "no_data",
                    "No configured dataset overlaps the requested dates",
                ));
            }
        }
        let count: i64 = tx
            .query_row(
                "SELECT count(*) FROM jobs WHERE status IN ('queued','running')",
                [],
                |row| row.get(0),
            )
            .map_err(storage_error)?;
        if count >= CAPACITY {
            return Err(error(
                StatusCode::TOO_MANY_REQUESTS,
                "queue_full",
                "Backtest queue full",
            ));
        }
        tx.execute(
            "INSERT INTO jobs(idem,request) VALUES (?1,?2)",
            params![key, request],
        )
        .map_err(storage_error)?;
        let id = tx.last_insert_rowid();
        tx.commit().map_err(storage_error)?;
        Ok(id)
    })
    .await?;
    Ok((StatusCode::ACCEPTED, Json(Accepted { id })))
}

#[utoipa::path(get, path = "/api/v1/backtests/by-key",
    params(("Idempotency-Key" = String, Header, description="Exact key from the attempted submission")),
    responses((status=200,body=Accepted),(status=404,body=ErrorBody),(status=422,body=ErrorBody),(status=500,body=ErrorBody)))]
pub async fn by_key(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Accepted>, ApiError> {
    let key = submission_key(&headers)?;
    let id = db(state, move |conn| {
        conn.query_row("SELECT id FROM jobs WHERE idem=?1", [key], |row| row.get(0))
            .optional()
            .map_err(storage_error)?
            .ok_or_else(|| {
                error(
                    StatusCode::NOT_FOUND,
                    "job_not_found",
                    "No job for this key",
                )
            })
    })
    .await?;
    Ok(Json(Accepted { id }))
}

const COLUMNS: &str = "id,status,request,attempt,created_at,summary,error";
fn parse<T: serde::de::DeserializeOwned>(value: &str, idx: usize) -> rusqlite::Result<T> {
    serde_json::from_str(value).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(idx, rusqlite::types::Type::Text, Box::new(e))
    })
}
fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Job> {
    let status: String = r.get(1)?;
    let status = match status.as_str() {
        "queued" => JobStatus::Queued,
        "running" => JobStatus::Running,
        "succeeded" => JobStatus::Succeeded,
        "failed" => JobStatus::Failed,
        "timed_out" => JobStatus::TimedOut,
        _ => return Err(rusqlite::Error::InvalidQuery),
    };
    let created_at: String = r.get(4)?;
    Ok(Job {
        id: r.get(0)?,
        status,
        request: parse(&r.get::<_, String>(2)?, 2)?,
        attempt: r.get(3)?,
        created_at: created_at.parse().map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(e))
        })?,
        summary: r
            .get::<_, Option<String>>(5)?
            .as_deref()
            .map(|s| parse(s, 5))
            .transpose()?,
        error: r.get(6)?,
    })
}

#[utoipa::path(get, path = "/api/v1/backtests/{id}", params(("id" = i64, Path)), responses((status=200,body=Job),(status=404,body=ErrorBody),(status=422,body=ErrorBody),(status=500,body=ErrorBody)))]
pub async fn detail(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Job>, ApiError> {
    let id = id
        .parse::<i64>()
        .ok()
        .filter(|i| *i > 0)
        .ok_or_else(|| invalid("Invalid job id"))?;
    db(state, move |conn| {
        conn.query_row(
            &format!("SELECT {COLUMNS} FROM jobs WHERE id=?1"),
            [id],
            row,
        )
        .optional()
        .map_err(storage_error)?
        .map(Json)
        .ok_or_else(|| error(StatusCode::NOT_FOUND, "job_not_found", "No such job"))
    })
    .await
}

#[utoipa::path(get, path = "/api/v1/backtests", params(("limit"=Option<u32>, Query, description="1..100, default 20"),("cursor"=Option<i64>,Query,description="Exclusive last id in ascending order")),responses((status=200,body=Page),(status=422,body=ErrorBody),(status=500,body=ErrorBody)))]
pub async fn list(
    State(state): State<AppState>,
    query: Result<Query<Pagination>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Page>, ApiError> {
    let pagination = query.map_err(|_| invalid("Invalid pagination"))?.0;
    let limit = pagination.limit.unwrap_or(20);
    let cursor = pagination.cursor.unwrap_or(0);
    if !(1..=100).contains(&limit) || cursor < 0 {
        return Err(invalid("Invalid pagination"));
    }
    db(state, move |conn| {
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {COLUMNS} FROM jobs WHERE id>?1 ORDER BY id LIMIT ?2"
            ))
            .map_err(storage_error)?;
        let mut items = stmt
            .query_map(params![cursor, limit + 1], row)
            .map_err(storage_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(storage_error)?;
        let next_cursor = if items.len() > limit as usize {
            items.pop();
            items.last().map(|job| job.id)
        } else {
            None
        };
        Ok(Json(Page { items, next_cursor }))
    })
    .await
}
