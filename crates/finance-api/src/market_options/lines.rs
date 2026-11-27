use super::{OPRA, ReadSources, failed, invalid, parse, unavailable};
use crate::{ApiError, ErrorBody};
use axum::{
    Json,
    extract::{Extension, Query, rejection::QueryRejection},
};
use chrono::{DateTime, NaiveDate, Utc};
use finance_collector::{
    line_budget::{BudgetStatus, CoverageInterval, CoverageReport, LineHandle, SchedulerPhase},
    market_ingress::ProductPolicy,
};
use finance_ib::market::{
    ConfirmedContract, ContractKind, OptionRight, Product, option_market_date,
};
use finance_market_protocol::DecimalText;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

/// Read the existing scheduler without opening, cancelling or rotating a line.
pub trait OptionLineSource: Send + Sync {
    /// Confirmed contracts and the clock for this roster. HTTP filters expiry
    /// against the Eastern date of this clock before serving the page.
    fn option_chain(&self) -> (DateTime<Utc>, bool, Vec<ConfirmedContract>);
    fn status(&self) -> BudgetStatus;
    /// Coverage for the requested conId on the shared OPRA line with OI enabled.
    fn option_coverage(
        &self,
        con_id: i32,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Option<CoverageReport>;
}

impl OptionLineSource for LineHandle {
    fn option_chain(&self) -> (DateTime<Utc>, bool, Vec<ConfirmedContract>) {
        self.option_chain()
    }

    fn status(&self) -> BudgetStatus {
        self.status()
    }

    fn option_coverage(
        &self,
        con_id: i32,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Option<CoverageReport> {
        self.option_coverage(con_id, start, end)
    }
}

#[derive(Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(crate) struct OptionChainQuery {
    #[param(minimum = 1)]
    underlying_con_id: i32,
    expiry: Option<NaiveDate>,
    #[param(minimum = 1)]
    after_con_id: Option<i32>,
    #[param(default = 100, minimum = 1, maximum = 500)]
    limit: Option<usize>,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
enum OptionChainRight {
    Call,
    Put,
}

#[derive(Serialize, ToSchema)]
struct OptionChainContract {
    con_id: i32,
    symbol: String,
    primary_exchange: Option<String>,
    expiry: NaiveDate,
    strike: DecimalText,
    right: OptionChainRight,
    multiplier: DecimalText,
}

impl OptionChainContract {
    fn for_underlying(
        contract: ConfirmedContract,
        query: &OptionChainQuery,
        market_date: NaiveDate,
    ) -> Option<Self> {
        let ContractKind::Option(option) = contract.kind else {
            return None;
        };
        if option.expiry < market_date
            || option.underlying_con_id != query.underlying_con_id
            || query.expiry.is_some_and(|date| date != option.expiry)
            || query
                .after_con_id
                .is_some_and(|after| contract.con_id <= after)
        {
            return None;
        }
        Some(Self {
            con_id: contract.con_id,
            symbol: contract.symbol,
            primary_exchange: contract.primary_exchange,
            expiry: option.expiry,
            strike: option.strike,
            right: match option.right {
                OptionRight::Call => OptionChainRight::Call,
                OptionRight::Put => OptionChainRight::Put,
            },
            multiplier: option.multiplier,
        })
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct OptionChainPage {
    access: ProductPolicy,
    as_of: DateTime<Utc>,
    underlying_con_id: i32,
    /// Discovery status for the scheduler's watchlist.
    discovery_complete: bool,
    items: Vec<OptionChainContract>,
    next_after_con_id: Option<i32>,
}

#[utoipa::path(
    get, path = "/api/v1/options/chain", params(OptionChainQuery),
    responses(
        (status = 200, body = OptionChainPage),
        (status = 400, body = ErrorBody),
        (status = 500, body = ErrorBody),
        (status = 503, body = ErrorBody)
    )
)]
pub(crate) async fn chain(
    Extension(sources): ReadSources,
    query: Result<Query<OptionChainQuery>, QueryRejection>,
) -> Result<Json<OptionChainPage>, ApiError> {
    let query = parse(query)?;
    let limit = query.limit.unwrap_or(100);
    if query.underlying_con_id <= 0
        || query.after_con_id.is_some_and(|id| id <= 0)
        || !(1..=500).contains(&limit)
    {
        return Err(invalid());
    }
    let lines = sources.option_lines.as_ref().ok_or_else(unavailable)?;
    let (as_of, discovery_complete, mut contracts) = lines.source.option_chain();
    let market_date = option_market_date(as_of).map_err(|_| failed())?;
    contracts.sort_unstable_by_key(|contract| contract.con_id);
    let mut matching = contracts
        .into_iter()
        .filter_map(|contract| OptionChainContract::for_underlying(contract, &query, market_date));
    let items: Vec<_> = matching.by_ref().take(limit).collect();
    let next_after_con_id = if matching.next().is_some() {
        items.last().map(|item| item.con_id)
    } else {
        None
    };
    Ok(Json(OptionChainPage {
        access: lines.access(OPRA),
        as_of,
        underlying_con_id: query.underlying_con_id,
        discovery_complete,
        items,
        next_after_con_id,
    }))
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
enum LineSchedulerPhase {
    Starting,
    Online,
    Reconnecting,
    Stopped,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct SubscriptionBudget {
    access: ProductPolicy,
    phase: LineSchedulerPhase,
    as_of: DateTime<Utc>,
    account_lines: usize,
    reserve_lines: usize,
    stock_lines: usize,
    hot_lines: usize,
    rotation_lines: usize,
    confirmed_options: usize,
    observed_options: usize,
    discovery_complete: bool,
}

#[utoipa::path(
    get, path = "/api/v1/market/subscriptions",
    responses((status = 200, body = SubscriptionBudget), (status = 503, body = ErrorBody))
)]
pub(crate) async fn subscriptions(
    Extension(sources): ReadSources,
) -> Result<Json<SubscriptionBudget>, ApiError> {
    let lines = sources.option_lines.as_ref().ok_or_else(unavailable)?;
    let BudgetStatus {
        phase,
        as_of,
        account_lines,
        reserve_lines,
        stock_lines,
        hot_lines,
        rotation_lines,
        confirmed_options,
        observed_options,
        discovery_complete,
    } = lines.source.status();
    Ok(Json(SubscriptionBudget {
        access: lines.access(OPRA),
        phase: match phase {
            SchedulerPhase::Starting => LineSchedulerPhase::Starting,
            SchedulerPhase::Online => LineSchedulerPhase::Online,
            SchedulerPhase::Reconnecting => LineSchedulerPhase::Reconnecting,
            SchedulerPhase::Stopped => LineSchedulerPhase::Stopped,
        },
        as_of,
        account_lines,
        reserve_lines,
        stock_lines,
        hot_lines,
        rotation_lines,
        confirmed_options,
        observed_options,
        discovery_complete,
    }))
}

#[derive(Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(crate) struct SubscriptionCoverageQuery {
    #[param(minimum = 1)]
    con_id: i32,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
}

#[derive(Serialize, ToSchema)]
struct SubscriptionCoverageInterval {
    start: DateTime<Utc>,
    end: DateTime<Utc>,
}

impl From<CoverageInterval> for SubscriptionCoverageInterval {
    fn from(interval: CoverageInterval) -> Self {
        Self {
            start: interval.start,
            end: interval.end,
        }
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct SubscriptionCoverage {
    access: ProductPolicy,
    con_id: i32,
    last_observed_at: Option<DateTime<Utc>>,
    covered: Vec<SubscriptionCoverageInterval>,
    uncovered: Vec<SubscriptionCoverageInterval>,
    unknown: Vec<SubscriptionCoverageInterval>,
}

#[utoipa::path(
    get, path = "/api/v1/market/subscriptions/coverage", params(SubscriptionCoverageQuery),
    responses(
        (status = 200, body = SubscriptionCoverage),
        (status = 400, body = ErrorBody),
        (status = 500, body = ErrorBody),
        (status = 503, body = ErrorBody)
    )
)]
pub(crate) async fn coverage(
    Extension(sources): ReadSources,
    query: Result<Query<SubscriptionCoverageQuery>, QueryRejection>,
) -> Result<Json<SubscriptionCoverage>, ApiError> {
    let query = parse(query)?;
    if query.con_id <= 0 || query.start >= query.end {
        return Err(invalid());
    }
    let lines = sources.option_lines.as_ref().ok_or_else(unavailable)?;
    let report = lines
        .source
        .option_coverage(query.con_id, query.start, query.end)
        .ok_or_else(invalid)?;
    if report.key.con_id != query.con_id
        || report.key.product != Product::Opra
        || !report.key.parameters.option_open_interest
    {
        return Err(failed());
    }
    Ok(Json(SubscriptionCoverage {
        access: lines.access(OPRA),
        con_id: query.con_id,
        last_observed_at: report.last_observed_at,
        covered: report.covered.into_iter().map(Into::into).collect(),
        uncovered: report.uncovered.into_iter().map(Into::into).collect(),
        unknown: report.unknown.into_iter().map(Into::into).collect(),
    }))
}
