use axum::{Extension, Json, extract::rejection::JsonRejection, http::StatusCode};
use finance_collector::{
    line_budget::{SubscriptionDemand, SubscriptionDemandHandle},
    market_ingress::ProductPolicy,
};
use finance_ib::depth::{MAX_DEPTH_SUBSCRIPTIONS, PINNED_DEPTH_SLOTS, permission};
use finance_market_protocol::InstrumentId;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use utoipa::ToSchema;

use crate::{ApiError, ErrorBody, error};

use super::{
    MarketReadSources, ReadSources,
    lines::{SubscriptionBudget, subscription_budget},
};

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SubscriptionDemandRequest {
    /// F1 instrument IDs. An omitted or empty array clears additional watchlist demand.
    #[serde(default)]
    watchlist: Vec<InstrumentId>,
    /// F1 ID eligible for Nasdaq TotalView. Omission or null clears the page focus.
    depth_focus: Option<InstrumentId>,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
enum SubscriptionDemandState {
    Disabled,
    Enabled,
}

#[derive(Serialize, ToSchema)]
struct DepthSubscriptionBudget {
    pinned_slots: usize,
    page_focus_slots: usize,
    /// Private display policy from the depth owner, when attached.
    #[schema(required = true)]
    access: Option<ProductPolicy>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct SubscriptionDemandStatus {
    state: SubscriptionDemandState,
    /// Latest demand accepted by the owner, or an empty demand when disabled.
    demand: SubscriptionDemand,
    /// Current counts and OPRA display policy from the existing line budget.
    #[schema(required = true)]
    budget: Option<SubscriptionBudget>,
    depth_budget: DepthSubscriptionBudget,
}

impl SubscriptionDemandStatus {
    fn new(sources: &MarketReadSources, demand: Option<SubscriptionDemand>) -> Self {
        Self {
            state: if demand.is_some() {
                SubscriptionDemandState::Enabled
            } else {
                SubscriptionDemandState::Disabled
            },
            demand: demand.unwrap_or_default(),
            budget: sources.option_lines.as_ref().map(subscription_budget),
            depth_budget: DepthSubscriptionBudget {
                pinned_slots: PINNED_DEPTH_SLOTS,
                page_focus_slots: MAX_DEPTH_SUBSCRIPTIONS - PINNED_DEPTH_SLOTS,
                access: sources
                    .depth
                    .as_ref()
                    .map(|depth| depth.access(permission::PRODUCT)),
            },
        }
    }
}

#[utoipa::path(
    get, path = "/api/v1/market/subscriptions/demand",
    responses((status = 200, body = SubscriptionDemandStatus))
)]
pub(crate) async fn get_subscription_demand(
    Extension(sources): ReadSources,
) -> Json<SubscriptionDemandStatus> {
    let demand = sources
        .subscription_demand
        .as_ref()
        .and_then(SubscriptionDemandHandle::current);
    Json(SubscriptionDemandStatus::new(&sources, demand))
}

#[utoipa::path(
    put, path = "/api/v1/market/subscriptions/demand",
    request_body = SubscriptionDemandRequest,
    responses(
        (status = 200, body = SubscriptionDemandStatus),
        (status = 400, body = ErrorBody)
    )
)]
pub(crate) async fn put_subscription_demand(
    Extension(sources): ReadSources,
    request: Result<Json<Box<RawValue>>, JsonRejection>,
) -> Result<Json<SubscriptionDemandStatus>, ApiError> {
    let Json(raw) = request.map_err(|_| invalid())?;
    // Decode only an object. Keep the original keys so Serde can reject duplicates.
    let body = raw.get();
    if !body.starts_with('{') {
        return Err(invalid());
    }
    let request: SubscriptionDemandRequest = serde_json::from_str(body).map_err(|_| invalid())?;
    let demand = sources
        .subscription_registry
        .validate(request.watchlist, request.depth_focus)
        .ok_or_else(invalid)?;
    let accepted = sources
        .subscription_demand
        .as_ref()
        .and_then(|owner| owner.replace(demand));
    Ok(Json(SubscriptionDemandStatus::new(&sources, accepted)))
}

fn invalid() -> ApiError {
    error(
        StatusCode::BAD_REQUEST,
        "subscription_demand_invalid",
        "Invalid subscription demand",
    )
}
