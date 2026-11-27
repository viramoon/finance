use crate::{ApiError, ErrorBody, error, market_options::MarketReadSources};
use axum::{Extension, Json, http::StatusCode};
use finance_collector::{binance::Config, market_ingress::ProductPolicy};
use serde::Serialize;
use std::sync::Arc;
use utoipa::ToSchema;

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
enum BinanceProfileState {
    DisabledByProfile,
    EnabledByProfile,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct BinanceReadStatus {
    state: BinanceProfileState,
    products: Vec<ProductPolicy>,
}

#[utoipa::path(
    get, path = "/api/v1/market/binance/status",
    responses(
        (status = 200, description = "Binance profile state and configured permission flags", body = BinanceReadStatus),
        (status = 500, description = "Binance profile could not be read or is invalid", body = ErrorBody)
    )
)]
pub(crate) async fn binance_status(
    Extension(sources): Extension<Arc<MarketReadSources>>,
) -> Result<Json<BinanceReadStatus>, ApiError> {
    let products = if let Some(path) = sources.binance_config.clone() {
        tokio::task::spawn_blocking(move || Config::load(&path)?.configured_products())
            .await
            .ok()
            .and_then(Result::ok)
            .ok_or_else(|| {
                error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "binance_profile_invalid",
                    "Cannot read Binance profile",
                )
            })?
    } else {
        None
    };
    Ok(Json(match products {
        Some(products) => BinanceReadStatus {
            state: BinanceProfileState::EnabledByProfile,
            products,
        },
        None => BinanceReadStatus {
            state: BinanceProfileState::DisabledByProfile,
            products: Vec::new(),
        },
    }))
}
