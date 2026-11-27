//! Private REST views of the shared market owner's read-only data.

pub(crate) mod analytics;
pub(crate) mod depth;
pub(crate) mod lines;

use analytics::{exposure, history};
use depth::book;
use lines::{chain, coverage, subscriptions};

pub use analytics::{ExposureInput, ExposureSource, HistoryInput, HistorySource};
pub use depth::DepthFrames;
pub use lines::OptionLineSource;

use crate::{ApiError, AppState, error};
use axum::{
    Extension, Router,
    extract::{Query, rejection::QueryRejection},
    http::StatusCode,
    routing::get,
};
use finance_collector::market_ingress::ProductPolicy;
use finance_market_protocol::Token;
use serde::de::DeserializeOwned;
use std::{path::PathBuf, sync::Arc};

#[derive(Default)]
pub struct MarketReadSources {
    pub binance_config: Option<PathBuf>,
    pub option_lines: Option<Licensed<Arc<dyn OptionLineSource>>>,
    pub depth: Option<Licensed<DepthFrames>>,
    pub exposure: Option<Licensed<Arc<ExposureSource>>>,
    pub history: Option<Licensed<Arc<HistorySource>>>,
}

/// Attach a product already authorized by the owner for private display.
/// The resulting API policy permits display within the session only.
pub struct Licensed<T> {
    source: T,
    license: Token,
}

impl<T> Licensed<T> {
    pub fn new(source: T, license: Token) -> Self {
        Self { source, license }
    }

    fn access(&self, product: &str) -> ProductPolicy {
        ProductPolicy {
            source: "ibkr".parse().expect("static source"),
            product: product.parse().expect("static product"),
            license: self.license.clone(),
            display_live: true,
            record_local: false,
            derive_non_display: false,
            backup_personal_drive: false,
        }
    }
}

const OPRA: &str = "opra";

pub(crate) fn routes(sources: MarketReadSources) -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/market/binance/status",
            get(crate::binance_read::binance_status),
        )
        .route("/api/v1/market/book", get(book))
        .route("/api/v1/market/subscriptions", get(subscriptions))
        .route("/api/v1/market/subscriptions/coverage", get(coverage))
        .route("/api/v1/options/chain", get(chain))
        .route("/api/v1/options/exposure", get(exposure))
        .route("/api/v1/options/history", get(history))
        .layer(Extension(Arc::new(sources)))
}

type ReadSources = Extension<Arc<MarketReadSources>>;

fn parse<T: DeserializeOwned>(query: Result<Query<T>, QueryRejection>) -> Result<T, ApiError> {
    query.map(|Query(query)| query).map_err(|_| invalid())
}

fn invalid() -> ApiError {
    error(
        StatusCode::BAD_REQUEST,
        "market_read_query_invalid",
        "Invalid market read query",
    )
}

fn missing() -> ApiError {
    error(
        StatusCode::NOT_FOUND,
        "market_read_not_found",
        "Requested market data is unavailable",
    )
}

fn unavailable() -> ApiError {
    error(
        StatusCode::SERVICE_UNAVAILABLE,
        "market_read_source_unavailable",
        "Market read source is not connected",
    )
}

fn failed() -> ApiError {
    error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "market_read_failed",
        "Could not read market data",
    )
}
