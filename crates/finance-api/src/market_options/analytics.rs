use super::{OPRA, ReadSources, failed, invalid, missing, parse, unavailable};
use crate::{ApiError, ErrorBody};
use axum::{
    Json,
    extract::{Extension, Query, rejection::QueryRejection},
};
use chrono::{DateTime, Utc};
use finance_collector::{
    market_ingress::ProductPolicy,
    option_exposure::{
        self, DealerScenario, ExposureReport, GammaGrid, OptionChainSnapshot, TimingPolicy,
    },
    option_history::{BarWindow, ContractHistory, OptionBars, TradeBar},
};
use finance_market_protocol::{InstrumentId, U64Text};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use utoipa::{IntoParams, ToSchema};

/// A read-only lookup of the owner's in-memory valuation input. With no time,
/// return the latest snapshot. With a time, return that exact valuation if retained.
pub type ExposureSource =
    dyn Fn(&InstrumentId, Option<DateTime<Utc>>) -> Option<Arc<ExposureInput>> + Send + Sync;

pub struct ExposureInput {
    pub chain: OptionChainSnapshot,
    pub timing: TimingPolicy,
    pub gamma_grid: Option<GammaGrid>,
}

#[derive(Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(crate) struct OptionExposureQuery {
    #[param(inline)]
    underlying_id: InstrumentId,
    as_of: Option<DateTime<Utc>>,
    #[param(inline, default = "absolute")]
    scenario: Option<DealerScenario>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct OptionExposureResponse {
    access: ProductPolicy,
    report: ExposureReport,
}

#[utoipa::path(
    get, path = "/api/v1/options/exposure", params(OptionExposureQuery),
    responses(
        (status = 200, body = OptionExposureResponse),
        (status = 400, body = ErrorBody),
        (status = 404, body = ErrorBody),
        (status = 500, body = ErrorBody),
        (status = 503, body = ErrorBody)
    )
)]
pub(crate) async fn exposure(
    Extension(sources): ReadSources,
    query: Result<Query<OptionExposureQuery>, QueryRejection>,
) -> Result<Json<OptionExposureResponse>, ApiError> {
    let query = parse(query)?;
    let source = sources.exposure.as_ref().ok_or_else(unavailable)?;
    let input = (source.source)(&query.underlying_id, query.as_of).ok_or_else(missing)?;
    if input.chain.underlying_id != query.underlying_id
        || query.as_of.is_some_and(|time| input.chain.as_of != time)
    {
        return Err(failed());
    }
    let scenario = query.scenario.unwrap_or(DealerScenario::Absolute);
    let report = tokio::task::spawn_blocking(move || {
        option_exposure::evaluate(
            &input.chain,
            scenario,
            input.timing,
            input.gamma_grid.as_ref(),
        )
    })
    .await
    .map_err(|_| failed())?
    .map_err(|_| failed())?;
    Ok(Json(OptionExposureResponse {
        access: source.access(OPRA),
        report,
    }))
}

/// A private lookup of an already obtained F12 chain window. GET requests
/// do not acquire a vendor session, consume its pacing budget or initiate reads.
pub type HistorySource = dyn Fn(i32, BarWindow) -> Option<Arc<HistoryInput>> + Send + Sync;

pub struct HistoryInput {
    pub underlying_con_id: i32,
    pub window: BarWindow,
    pub as_of: DateTime<Utc>,
    pub contracts: Vec<ContractHistory>,
}

#[derive(Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(crate) struct OptionHistoryQuery {
    #[param(minimum = 1)]
    underlying_con_id: i32,
    #[param(inline)]
    start: U64Text,
    #[param(inline)]
    end: U64Text,
    #[param(default = 0)]
    offset: Option<usize>,
    #[param(default = 20, minimum = 1, maximum = 50)]
    limit: Option<usize>,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "snake_case", tag = "status")]
enum HistoricalOptionBars {
    Available { bars: Vec<TradeBar> },
    NoBars,
    Expired,
    RequestFailed,
}

#[derive(Serialize, ToSchema)]
struct HistoricalOptionResult {
    instrument_id: InstrumentId,
    result: HistoricalOptionBars,
}

impl From<&ContractHistory> for HistoricalOptionResult {
    fn from(contract: &ContractHistory) -> Self {
        Self {
            instrument_id: contract.instrument_id.clone(),
            result: match &contract.bars {
                Ok(OptionBars::Available(bars)) => {
                    HistoricalOptionBars::Available { bars: bars.clone() }
                }
                Ok(OptionBars::NoBars) => HistoricalOptionBars::NoBars,
                Ok(OptionBars::Expired) => HistoricalOptionBars::Expired,
                Err(_) => HistoricalOptionBars::RequestFailed,
            },
        }
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct OptionHistoryPage {
    access: ProductPolicy,
    as_of: DateTime<Utc>,
    underlying_con_id: i32,
    start: U64Text,
    end: U64Text,
    items: Vec<HistoricalOptionResult>,
    next_offset: Option<usize>,
}

#[utoipa::path(
    get, path = "/api/v1/options/history", params(OptionHistoryQuery),
    responses(
        (status = 200, body = OptionHistoryPage),
        (status = 400, body = ErrorBody),
        (status = 404, body = ErrorBody),
        (status = 500, body = ErrorBody),
        (status = 503, body = ErrorBody)
    )
)]
pub(crate) async fn history(
    Extension(sources): ReadSources,
    query: Result<Query<OptionHistoryQuery>, QueryRejection>,
) -> Result<Json<OptionHistoryPage>, ApiError> {
    let query = parse(query)?;
    let limit = query.limit.unwrap_or(20);
    if query.underlying_con_id <= 0 || query.start >= query.end || !(1..=50).contains(&limit) {
        return Err(invalid());
    }
    let source = sources.history.as_ref().ok_or_else(unavailable)?;
    let window = BarWindow {
        start: query.start,
        end: query.end,
    };
    let input = (source.source)(query.underlying_con_id, window).ok_or_else(missing)?;
    if input.underlying_con_id != query.underlying_con_id || input.window != window {
        return Err(failed());
    }
    let offset = query.offset.unwrap_or(0);
    let items: Vec<_> = input
        .contracts
        .iter()
        .skip(offset)
        .take(limit)
        .map(Into::into)
        .collect();
    let end_offset = offset + items.len();
    Ok(Json(OptionHistoryPage {
        access: source.access(OPRA),
        as_of: input.as_of,
        underlying_con_id: input.underlying_con_id,
        start: window.start,
        end: window.end,
        items,
        next_offset: (end_offset < input.contracts.len()).then_some(end_offset),
    }))
}
