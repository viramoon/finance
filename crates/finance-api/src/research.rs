use crate::{ApiError, AppState, ErrorBody, error};
use axum::{Json, extract::State, http::StatusCode};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use std::{fs, io, path::Path};
use utoipa::ToSchema;

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct Dataset {
    pub instrument: String,
    pub asset_type: String,
    pub source: String,
    pub source_sha256: String,
    pub data_kind: String,
    pub calendar: String,
    pub first_session: NaiveDate,
    pub last_session: NaiveDate,
    pub observations: u32,
    pub price_basis: String,
    pub corporate_actions: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DatasetList {
    pub items: Vec<Dataset>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct Metrics {
    /// Percent, exact decimal text.
    pub return_pct: String,
    /// Positive peak-to-trough percent, exact decimal text.
    pub max_drawdown_pct: String,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Coverage {
    Full,
    Insufficient,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct Window {
    pub requested_years: u8,
    pub requested_start: NaiveDate,
    pub actual_start: Option<NaiveDate>,
    pub actual_end: Option<NaiveDate>,
    pub actual_years: Option<String>,
    /// First supported date counted in the calendar totals.
    pub calendar_start: NaiveDate,
    pub expected_sessions: u32,
    pub missing_sessions: u32,
    pub missing_pct: Option<String>,
    pub coverage: Coverage,
    pub metrics: Option<Metrics>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct Fill {
    pub order_id: String,
    pub signal_session: NaiveDate,
    pub execution_session: NaiveDate,
    pub executed_at: chrono::DateTime<chrono::Utc>,
    pub side: String,
    pub shares: u32,
    pub price: String,
    pub commission: String,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct BlockedOrder {
    pub signal_session: NaiveDate,
    pub execution_session: NaiveDate,
    pub bar_volume: String,
    pub requested_shares: u32,
    pub reason: String,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ExecutionAudit {
    pub fills: u32,
    pub zero_volume_bars: u32,
    pub blocked_zero_volume: u32,
    pub blocked_insufficient_volume: u32,
    pub blocked_insufficient_cash: u32,
    pub recent_fills: Vec<Fill>,
    pub recent_blocked: Vec<BlockedOrder>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ResearchReport {
    pub dataset: Dataset,
    pub requested_start: NaiveDate,
    pub requested_end: NaiveDate,
    pub actual_start: NaiveDate,
    pub actual_end: NaiveDate,
    pub observations: u32,
    pub warmup_sessions: u32,
    pub performance_start: NaiveDate,
    pub initial_equity: String,
    pub ending_equity: String,
    pub commission_per_order: String,
    pub slippage_bps: u32,
    pub risk_free_rate: String,
    pub execution: String,
    pub metrics: Option<Metrics>,
    pub windows: Vec<Window>,
    pub audit: ExecutionAudit,
}

pub fn instrument_valid(id: &str) -> bool {
    let Some(symbol) = id.strip_suffix(".XNYS") else {
        return false;
    };
    let mut bytes = symbol.bytes();
    (1..=16).contains(&symbol.len())
        && bytes.next().is_some_and(|b| b.is_ascii_uppercase())
        && bytes.all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'-')
}

fn catalog_error(err: impl std::fmt::Display) -> ApiError {
    eprintln!("research catalog: {err}");
    error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "catalog_error",
        "Cannot read research catalog",
    )
}

pub fn dataset(root: &Path, id: &str) -> Result<Option<Dataset>, ApiError> {
    debug_assert!(instrument_valid(id));
    let dir = root.join(id);
    if dir.is_symlink() {
        return Err(catalog_error("dataset paths must be concrete directories"));
    }
    let path = dir.join("dataset.json");
    let value = match fs::read(path) {
        Ok(value) => value,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(catalog_error(err)),
    };
    let metadata: Dataset = serde_json::from_slice(&value).map_err(catalog_error)?;
    if metadata.instrument != id
        || !matches!(metadata.asset_type.as_str(), "stock" | "etf")
        || !matches!(metadata.data_kind.as_str(), "historical" | "fixture")
        || metadata.calendar != "XNYS"
        || metadata.first_session > metadata.last_session
        || metadata.price_basis != "raw_unadjusted"
        || metadata.corporate_actions != "cash_dividends_excluded_split_free_reviewed"
        || metadata.source_sha256.len() != 64
        || !metadata
            .source_sha256
            .bytes()
            .all(|b| b.is_ascii_hexdigit())
        || metadata.observations == 0
    {
        return Err(catalog_error("dataset metadata is invalid"));
    }
    Ok(Some(metadata))
}

#[utoipa::path(get, path = "/api/v1/backtests/datasets",
    responses((status=200, body=DatasetList),(status=500,body=ErrorBody)))]
pub async fn datasets(State(state): State<AppState>) -> Result<Json<DatasetList>, ApiError> {
    tokio::task::spawn_blocking(move || {
        let dirs = match fs::read_dir(&state.research_root) {
            Ok(dirs) => dirs,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Ok(Json(DatasetList { items: vec![] }));
            }
            Err(err) => return Err(catalog_error(err)),
        };
        let mut result = Vec::new();
        for entry in dirs {
            let entry = entry.map_err(catalog_error)?;
            let name = entry.file_name();
            if let Some(name) = name.to_str().filter(|name| instrument_valid(name))
                && let Some(item) = dataset(&state.research_root, name)?
            {
                result.push(item);
            }
        }
        result.sort_by(|a, b| a.instrument.cmp(&b.instrument));
        Ok(Json(DatasetList { items: result }))
    })
    .await
    .map_err(catalog_error)?
}
