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
pub struct WindowMetrics {
    pub return_pct: String,
    pub max_drawdown_pct: String,
    pub cagr_pct: Option<String>,
    pub sharpe: Option<String>,
    pub volatility_pct: Option<String>,
    pub turnover_pct: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Coverage {
    Full,
    Insufficient,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct Window {
    /// Window identifier (50y, 10y, 5y, longest_sample).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub requested_years: Option<u8>,
    pub requested_start: Option<NaiveDate>,
    pub actual_start: Option<NaiveDate>,
    pub actual_end: Option<NaiveDate>,
    pub actual_years: Option<String>,
    /// First supported date counted in the calendar totals.
    pub calendar_start: Option<NaiveDate>,
    /// Upper date bound used for the calendar totals.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub calendar_end: Option<NaiveDate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observations: Option<u32>,
    pub expected_sessions: u32,
    pub missing_sessions: u32,
    pub missing_pct: Option<String>,
    pub coverage: Coverage,
    /// Reason reported when requested dates extend beyond the exchange calendar bounds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rf_source: Option<String>,
    pub metrics: Option<WindowMetrics>,
}

#[cfg(test)]
mod tests {
    use super::Window;
    use serde_json::{Value, from_value, json, to_value};

    #[test]
    fn window_json_supports_existing_jobs_and_new_decimal_metrics() {
        let previous = json!({
            "requested_years": 50,
            "requested_start": "1976-11-20",
            "actual_start": "2026-11-18",
            "actual_end": "2026-11-20",
            "actual_years": "0.0055",
            "calendar_start": "1976-11-20",
            "expected_sessions": 12604,
            "missing_sessions": 12601,
            "missing_pct": "99.9762",
            "coverage": "insufficient",
            "metrics": null
        });
        let legacy: Window = from_value(previous.clone()).unwrap();
        assert_eq!(to_value(legacy).unwrap(), previous);

        let mut longest = previous;
        longest["window"] = json!("longest_sample");
        longest["label"] = json!("longest_sample");
        longest["requested_years"] = Value::Null;
        longest["requested_start"] = json!("2026-11-18");
        longest["calendar_start"] = json!("2026-11-18");
        longest["observations"] = json!(3);
        longest["expected_sessions"] = json!(3);
        longest["missing_sessions"] = json!(0);
        longest["missing_pct"] = json!("0.0000");
        longest["coverage"] = json!("full");
        longest["rf_source"] = json!("rf=0");
        longest["metrics"] = json!({
            "return_pct": "1.2345",
            "max_drawdown_pct": "0.0000",
            "cagr_pct": "12.3456",
            "sharpe": "1.0000",
            "volatility_pct": "4.4321",
            "turnover_pct": null
        });
        let current: Window = from_value(longest.clone()).unwrap();
        assert_eq!(to_value(current).unwrap(), longest);
    }

    #[test]
    fn window_json_preserves_partial_calendar_bounds() {
        let report = json!({
            "requested_start": "1974-06-05",
            "calendar_start": "1990-12-03",
            "calendar_end": "2024-06-05",
            "expected_sessions": 8200,
            "missing_sessions": 5700,
            "coverage": "insufficient",
            "coverage_reason": "calendar_out_of_range",
            "metrics": null
        });
        let data: Window = from_value(report).unwrap();
        let serialized = to_value(data).unwrap();
        assert_eq!(serialized["calendar_start"], "1990-12-03");
        assert_eq!(serialized["calendar_end"], "2024-06-05");
        assert_eq!(serialized["coverage_reason"], "calendar_out_of_range");
        assert_eq!(serialized["metrics"], Value::Null);
    }
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
