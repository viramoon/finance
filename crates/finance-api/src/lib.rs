pub mod bars;
mod bundle_jobs;
pub mod config;
mod decision_read;
mod factor_read;
pub mod indicators;
pub mod jobs;
pub mod market;
mod market_read;
mod read_store;
pub mod read_stream;
pub mod research;
pub mod strategy_bundle;
use axum::{
    Json, Router,
    extract::{Request, State},
    http::{HeaderValue, StatusCode, header::CACHE_CONTROL},
    middleware::{self, Next},
    response::Response,
    routing::get,
};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, str::FromStr, sync::Arc};
use utoipa::{OpenApi, ToSchema};

#[derive(Clone)]
pub struct AppState {
    pub data_root: PathBuf,
    pub research_root: PathBuf,
}
#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BacktestSummary {
    pub run_id: String,
    pub strategy: String,
    pub instrument: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub total_orders: u64,
    pub total_positions: u64,
    /// 精确十进制字符串，禁止 JSON number。
    #[schema(pattern = r"^-?[0-9]+(\.[0-9]+)?$")]
    pub pnl: String,
    pub currency: String,
    pub generated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub research: Option<research::ResearchReport>,
    /// Immutable inputs, versions, cutoff, and economics for a StrategyBundle run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub run_manifest: Option<serde_json::Map<String, serde_json::Value>>,
    /// Exact positions, cash, fills, and per-recipe attribution for a StrategyBundle run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub strategy_report: Option<serde_json::Map<String, serde_json::Value>>,
}
impl BacktestSummary {
    fn valid(&self) -> bool {
        let digits = self.pnl.strip_prefix('-').unwrap_or(&self.pnl);
        let parts: Vec<_> = digits.split('.').collect();
        parts.len() <= 2
            && parts
                .iter()
                .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
            && Decimal::from_str(&self.pnl).is_ok()
            && self.start <= self.end
            && !self.run_id.is_empty()
            && !self.strategy.is_empty()
            && !self.instrument.is_empty()
            && !self.currency.is_empty()
    }
}
#[derive(Serialize, Deserialize, ToSchema)]
pub struct ErrorBody {
    pub error: ErrorDetail,
}
#[derive(Serialize, Deserialize, ToSchema)]
pub struct ErrorDetail {
    pub code: String,
    pub message: String,
    /// JSON Pointer-shaped path from a StrategyBundle compiler diagnostic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<strategy_bundle::Location>,
}
#[derive(Serialize, Deserialize, ToSchema)]
pub struct Check {
    pub name: String,
    pub ok: bool,
}
#[derive(Serialize, Deserialize, ToSchema)]
pub struct Health {
    pub status: String,
    pub checks: Vec<Check>,
}
type ApiError = (StatusCode, Json<ErrorBody>);
fn error(status: StatusCode, code: &str, message: &str) -> ApiError {
    (
        status,
        Json(ErrorBody {
            error: ErrorDetail {
                code: code.into(),
                message: message.into(),
                path: None,
                location: None,
            },
        }),
    )
}
#[utoipa::path(get, path = "/healthz/live", responses((status = 200, body = Health)))]
async fn live() -> Json<Health> {
    Json(Health {
        status: "ok".into(),
        checks: vec![Check {
            name: "process".into(),
            ok: true,
        }],
    })
}
#[utoipa::path(get, path = "/healthz/ready", responses((status = 200, body = Health), (status = 503, body = Health)))]
async fn ready(State(state): State<AppState>) -> (StatusCode, Json<Health>) {
    // read_dir 验证实际可读，而不只检查路径存在；不创建目录掩盖部署错误。
    let ok = tokio::fs::read_dir(&state.data_root).await.is_ok();
    (
        if ok {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        },
        Json(Health {
            status: if ok { "ready" } else { "not_ready" }.into(),
            checks: vec![Check {
                name: "data_root_readable".into(),
                ok,
            }],
        }),
    )
}
#[utoipa::path(get, path = "/api/v1/backtests/sample", responses((status = 200, body = BacktestSummary), (status = 404, body = ErrorBody), (status = 500, body = ErrorBody)))]
async fn sample(State(state): State<AppState>) -> Result<Json<BacktestSummary>, ApiError> {
    let path = state
        .data_root
        .join("finance-state/backtests/sample/latest.json");
    let data = tokio::fs::read(path).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            error(
                StatusCode::NOT_FOUND,
                "sample_not_found",
                "No sample backtest result",
            )
        } else {
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "sample_unreadable",
                "Cannot read sample result",
            )
        }
    })?;
    let summary: BacktestSummary = serde_json::from_slice(&data).map_err(|_| {
        error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "sample_invalid",
            "Invalid sample result",
        )
    })?;
    if !summary.valid() {
        return Err(error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "sample_invalid",
            "Invalid sample result",
        ));
    }
    Ok(Json(summary))
}

async fn private_read(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    response
}

pub fn router(state: AppState) -> Router {
    let source = Arc::new(bars::LocalBarsSource::new(state.data_root.clone()));
    router_with_bars_source(state, source)
}

pub fn router_with_bars_source(state: AppState, source: Arc<dyn bars::BarsSource>) -> Router {
    let restricted_reads = market_read::routes()
        .merge(factor_read::routes())
        .merge(bundle_jobs::routes())
        .merge(decision_read::routes())
        .merge(jobs::contract_routes())
        .layer(middleware::from_fn(private_read));
    Router::new()
        .route("/healthz/live", get(live))
        .route("/healthz/ready", get(ready))
        .route("/api/v1/backtests/sample", get(sample))
        .route("/api/v1/market/status", get(market::status))
        .route("/api/v1/backtests/datasets", get(research::datasets))
        .merge(bars::routes(source))
        .merge(jobs::routes())
        .merge(restricted_reads)
        .with_state(state)
}
#[derive(OpenApi)]
#[openapi(
    paths(
        live,
        ready,
        sample,
        market::status,
        bars::chart,
        bars::catalog,
        bars::values,
        jobs::submit,
        jobs::detail,
        jobs::list,
        jobs::by_key,
        jobs::cancel,
        jobs::worker_status,
        bundle_jobs::register,
        bundle_jobs::validate,
        bundle_jobs::versions,
        bundle_jobs::version,
        research::datasets,
        market_read::products,
        market_read::wal,
        market_read::watermarks,
        market_read::projected,
        market_read::gaps,
        factor_read::list,
        factor_read::evaluations,
        factor_read::evaluation,
        decision_read::signals
    ),
    components(schemas(
        BacktestSummary,
        ErrorBody,
        ErrorDetail,
        Health,
        Check,
        market::MarketStatus,
        market::InstrumentStatus,
        jobs::JobRequest,
        jobs::SubmitRequest,
        jobs::JobRequestView,
        bundle_jobs::BundleSubmission,
        bundle_jobs::BundleRequestView,
        bundle_jobs::StrategyDocument,
        bundle_jobs::RegisteredStrategy,
        strategy_bundle::Location,
        jobs::JobStatus,
        jobs::Job,
        jobs::Page,
        jobs::Accepted,
        jobs::StrategyParams,
        research::Dataset,
        research::DatasetList,
        research::ResearchReport,
        research::Metrics,
        research::Window,
        research::WindowMetrics,
        research::Coverage,
        research::ExecutionAudit,
        research::Fill,
        research::BlockedOrder
    ))
)]
pub struct ApiDoc;
