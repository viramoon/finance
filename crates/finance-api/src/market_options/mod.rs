//! Private REST views and subscription demand for the shared market owner.

pub(crate) mod analytics;
pub(crate) mod demand;
pub(crate) mod depth;
pub(crate) mod flow;
pub(crate) mod lines;
mod owner;

use analytics::{exposure, history};
use depth::book;
use lines::{chain, coverage, subscriptions};

pub use analytics::{ExposureInput, ExposureSource, HistoryInput, HistorySource};
pub use depth::DepthFrames;
pub use flow::{
    FlowObservationSnapshot, FlowObservationSource, FlowVerificationSource, OptionFlowSource,
    OptionFlowWindow,
};
pub use lines::OptionLineSource;
pub use owner::PaperSubscriptionOwner;

use crate::{ApiError, AppState, error, option_flow_stream::OptionFlowStream};
use axum::{
    Extension, Router,
    extract::{Query, rejection::QueryRejection},
    http::StatusCode,
    routing::get,
};
use finance_collector::{
    line_budget::{SubscriptionDemandHandle, SubscriptionInstrumentRegistry},
    market_ingress::ProductPolicy,
};
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
    pub flow_observations: Option<OptionFlowSource<FlowObservationSource>>,
    pub flow_verifications: Option<OptionFlowSource<FlowVerificationSource>>,
    pub flow_stream: Option<Arc<OptionFlowStream>>,
    pub subscription_demand: Option<SubscriptionDemandHandle>,
    pub subscription_registry: SubscriptionInstrumentRegistry,
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
        .route(
            "/api/v1/market/subscriptions/demand",
            get(demand::get_subscription_demand).put(demand::put_subscription_demand),
        )
        .route("/api/v1/options/chain", get(chain))
        .route("/api/v1/options/exposure", get(exposure))
        .route("/api/v1/options/history", get(history))
        .route("/api/v1/options/flow/observations", get(flow::observations))
        .route("/api/v1/options/flow/verification", get(flow::verification))
        .merge(crate::option_flow_stream::routes())
        .layer(Extension(Arc::new(sources)))
}

pub(crate) type ReadSources = Extension<Arc<MarketReadSources>>;

pub(crate) fn parse<T: DeserializeOwned>(
    query: Result<Query<T>, QueryRejection>,
) -> Result<T, ApiError> {
    query.map(|Query(query)| query).map_err(|_| invalid())
}

pub(crate) fn invalid() -> ApiError {
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

pub(crate) fn unavailable() -> ApiError {
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
