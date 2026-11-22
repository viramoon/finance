//! Durable, single-host submission. SQLite's write transaction serializes the
//! idempotency lookup, capacity check and insertion across API processes.
use crate::{ApiError, AppState, BacktestSummary, ErrorBody, error};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
    routing::get,
};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

const CAPACITY: i64 = 128;
const SAMPLE_FIRST: i64 = 1704150060;
const SAMPLE_LAST: i64 = SAMPLE_FIRST + 8 * 60;

#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct StrategyParams {}

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
    fn validate(&self) -> bool {
        self.strategy == "DemoStrategy"
            && self.instrument == "6EH4.XCME"
            && self.start < self.end
            && [self.start, self.end].iter().all(|t| {
                (SAMPLE_FIRST..=SAMPLE_LAST).contains(&t.timestamp())
                    && t.timestamp() % 60 == 0
                    && t.timestamp_subsec_nanos() == 0
            })
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
    let key = headers
        .get("Idempotency-Key")
        .and_then(|v| v.to_str().ok())
        .filter(|v| (1..=128).contains(&v.len()) && v.bytes().all(|c| c.is_ascii_graphic()))
        .ok_or_else(|| invalid("Idempotency-Key required (1..128 visible ASCII characters)"))?
        .to_owned();
    let req = request
        .map_err(|_| invalid("Invalid job request or parameters"))?
        .0;
    if !req.validate() {
        return Err(invalid("Unsupported strategy, instrument or data interval"));
    }
    let request = serde_json::to_string(&req).map_err(storage_error)?;
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
