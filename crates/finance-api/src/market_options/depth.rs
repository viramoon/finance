use super::{ReadSources, missing, parse, unavailable};
use crate::{ApiError, ErrorBody};
use axum::{
    Json,
    extract::{Extension, Query, rejection::QueryRejection},
};
use chrono::{DateTime, Utc};
use finance_collector::market_ingress::ProductPolicy;
use finance_ib::depth::{
    DepthError, DepthFrame, DepthLevel, DepthPermissions, DepthState, DepthSymbol,
    permission::PRODUCT,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};
use utoipa::{IntoParams, ToSchema};

/// Attach `publish` to the existing depth owner's callback. Each symbol only
/// retains its latest frame, including the reset or inactive frame.
#[derive(Clone, Default)]
pub struct DepthFrames(Arc<RwLock<HashMap<DepthSymbol, DepthFrame>>>);

impl DepthFrames {
    pub fn publish(&self, frame: DepthFrame) {
        self.0
            .write()
            .expect("depth frames")
            .insert(frame.symbol, frame);
    }

    fn latest(&self, symbol: DepthSymbol) -> Option<DepthFrame> {
        self.0.read().expect("depth frames").get(&symbol).cloned()
    }
}

#[derive(Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(crate) struct DepthBookQuery {
    #[param(value_type = DepthSymbol, inline)]
    symbol: String,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
enum DepthBookState {
    PermissionUnconfirmed,
    Awaiting,
    Receiving,
    Reset,
    Inactive,
    Failed,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct DepthBook {
    access: ProductPolicy,
    symbol: DepthSymbol,
    venue: &'static str,
    permissions: DepthPermissions,
    state: DepthBookState,
    failure: Option<DepthError>,
    requested_rows: u32,
    returned_rows: usize,
    received_at: Option<DateTime<Utc>>,
    /// Null when no request-specific data type is supplied by the upstream.
    market_data_type: Option<u8>,
    bids: Vec<DepthLevel>,
    asks: Vec<DepthLevel>,
}

#[utoipa::path(
    get, path = "/api/v1/market/book", params(DepthBookQuery),
    responses(
        (status = 200, body = DepthBook),
        (status = 400, body = ErrorBody),
        (status = 404, body = ErrorBody),
        (status = 503, body = ErrorBody)
    )
)]
pub(crate) async fn book(
    Extension(sources): ReadSources,
    query: Result<Query<DepthBookQuery>, QueryRejection>,
) -> Result<Json<DepthBook>, ApiError> {
    let symbol = parse(query)?.symbol.parse().map_err(|_| super::invalid())?;
    let depth = sources.depth.as_ref().ok_or_else(unavailable)?;
    let mut frame = depth.source.latest(symbol).ok_or_else(missing)?;
    if !frame.permissions.is_confirmed() {
        frame.state = DepthState::PermissionUnconfirmed;
    }
    if frame.state != DepthState::Receiving {
        frame.bids.clear();
        frame.asks.clear();
        frame.received_at = None;
    }
    let (state, failure) = match frame.state {
        DepthState::PermissionUnconfirmed => (DepthBookState::PermissionUnconfirmed, None),
        DepthState::Awaiting => (DepthBookState::Awaiting, None),
        DepthState::Receiving => (DepthBookState::Receiving, None),
        DepthState::Reset => (DepthBookState::Reset, None),
        DepthState::Inactive => (DepthBookState::Inactive, None),
        DepthState::Failed(error) => (DepthBookState::Failed, Some(error)),
    };
    Ok(Json(DepthBook {
        access: depth.access(PRODUCT),
        symbol,
        venue: frame.venue(),
        permissions: frame.permissions,
        state,
        failure,
        requested_rows: frame.requested_rows,
        returned_rows: frame.returned_rows(),
        received_at: frame.received_at,
        market_data_type: frame.market_data_type().map(|value| value.code()),
        bids: frame.bids,
        asks: frame.asks,
    }))
}
