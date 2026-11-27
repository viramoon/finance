use super::{
    ReadSources, failed, invalid, lines::SubscriptionCoverage, missing, parse, unavailable,
};
use crate::{ApiError, ErrorBody};
use axum::{
    Json,
    extract::{Extension, Query, rejection::QueryRejection},
};
use chrono::{DateTime, Utc};
use finance_collector::{
    line_budget::{BudgetStatus, CoverageReport},
    market_ingress::ProductPolicy,
    option_flow::{
        HistoricalFlow, IBKR, OPRA, OPRA_HISTORY, ObservedFlow,
        evidence::{HistoricalCounts, VerifiedTrade},
        observation::FlowObservation,
    },
};
use finance_ib::{
    historical::option_ticks::{HistoricalIssue, HistoricalWindow},
    market::Product,
};
use finance_market_protocol::{DecimalText, U64Text};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use utoipa::{IntoParams, ToSchema};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OptionFlowWindow {
    pub con_id: i32,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

pub struct FlowObservationSnapshot {
    pub as_of: DateTime<Utc>,
    /// Matching observations in the owner's retained order, including equal receipt times.
    pub records: Vec<ObservedFlow>,
}

/// Read a retained, in-memory snapshot for one contract and receipt window.
pub type FlowObservationSource =
    dyn Fn(OptionFlowWindow) -> Option<Arc<FlowObservationSnapshot>> + Send + Sync;

/// Look up an already completed F9e report for the exact trade window.
pub type FlowVerificationSource =
    dyn Fn(OptionFlowWindow) -> Option<Arc<HistoricalFlow>> + Send + Sync;

/// The owner's current policy applies to all results from a read-only lookup.
/// Lookups must be local and must not initiate subscriptions or history requests.
pub struct OptionFlowSource<T: ?Sized> {
    access: ProductPolicy,
    read: Arc<T>,
}

impl<T: ?Sized> OptionFlowSource<T> {
    pub fn new(access: ProductPolicy, read: Arc<T>) -> Self {
        Self { access, read }
    }

    fn display_access(&self, product: &str) -> Option<&ProductPolicy> {
        let access = &self.access;
        (access.source.as_str() == IBKR
            && access.product.as_str() == product
            && access.display_live
            && (!access.backup_personal_drive || access.record_local))
            .then_some(access)
    }
}

#[derive(Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(crate) struct OptionFlowQuery {
    #[param(minimum = 1)]
    con_id: i32,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    #[param(default = 0, minimum = 0)]
    offset: Option<usize>,
    #[param(default = 20, minimum = 1, maximum = 100)]
    limit: Option<usize>,
}

impl OptionFlowQuery {
    fn window(&self) -> Result<OptionFlowWindow, ApiError> {
        if self.con_id <= 0 || self.start >= self.end || !(1..=100).contains(&self.limit()) {
            return Err(invalid());
        }
        Ok(OptionFlowWindow {
            con_id: self.con_id,
            start: self.start,
            end: self.end,
        })
    }

    fn limit(&self) -> usize {
        self.limit.unwrap_or(20)
    }

    fn page<'a, T>(&self, items: &'a [T]) -> (&'a [T], Option<usize>) {
        let start = self.offset.unwrap_or(0).min(items.len());
        let end = start.saturating_add(self.limit()).min(items.len());
        (&items[start..end], (end < items.len()).then_some(end))
    }
}

#[derive(Serialize, ToSchema)]
struct OptionFlowObservation {
    clue: FlowObservation,
    budget: BudgetStatus,
    #[schema(required = true)]
    coverage: Option<SubscriptionCoverage>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct OptionFlowObservationPage {
    access: ProductPolicy,
    as_of: DateTime<Utc>,
    con_id: i32,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    items: Vec<OptionFlowObservation>,
    #[schema(required = true)]
    next_offset: Option<usize>,
}

/// Retained OPRA L1 clues by receipt time in [start, end).
#[utoipa::path(
    get, path = "/api/v1/options/flow/observations", params(OptionFlowQuery),
    responses(
        (status = 200, body = OptionFlowObservationPage),
        (status = 400, body = ErrorBody),
        (status = 404, body = ErrorBody),
        (status = 500, body = ErrorBody),
        (status = 503, body = ErrorBody)
    )
)]
pub(crate) async fn observations(
    Extension(sources): ReadSources,
    query: Result<Query<OptionFlowQuery>, QueryRejection>,
) -> Result<Json<OptionFlowObservationPage>, ApiError> {
    let query = parse(query)?;
    let window = query.window()?;
    let source = sources.flow_observations.as_ref().ok_or_else(unavailable)?;
    let access = source.display_access(OPRA).ok_or_else(missing)?;
    let snapshot = (source.read)(window).ok_or_else(missing)?;
    let (records, next_offset) = query.page(&snapshot.records);
    let items = records
        .iter()
        .map(|record| {
            let clue = &record.clue;
            if &record.access != access
                || clue.con_id != window.con_id
                || !(window.start..window.end).contains(&clue.observed_at)
                || clue.observed_at > snapshot.as_of
            {
                return Err(failed());
            }
            Ok(OptionFlowObservation {
                clue: clue.clone(),
                budget: record.budget.clone(),
                coverage: coverage(record.coverage.as_ref(), access, window.con_id)?,
            })
        })
        .collect::<Result<_, ApiError>>()?;
    Ok(Json(OptionFlowObservationPage {
        access: access.clone(),
        as_of: snapshot.as_of,
        con_id: window.con_id,
        start: window.start,
        end: window.end,
        items,
        next_offset,
    }))
}

#[derive(Serialize, ToSchema)]
pub(crate) struct OptionFlowVerificationPage {
    access: ProductPolicy,
    con_id: i32,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    quote_start: DateTime<Utc>,
    multiplier: DecimalText,
    verified_at: DateTime<Utc>,
    max_quote_age_ms: i64,
    counts: HistoricalCounts,
    issues: Vec<HistoricalIssue>,
    #[schema(required = true)]
    observation_coverage: Option<SubscriptionCoverage>,
    /// F3 local WAL watermark for ibkr/opra-history, when local recording is licensed.
    #[schema(required = true, inline)]
    durable_through: Option<U64Text>,
    trades: Vec<VerifiedTrade>,
    #[schema(required = true)]
    next_offset: Option<usize>,
}

/// Existing historical verification for one exact, whole-second trade window.
#[utoipa::path(
    get, path = "/api/v1/options/flow/verification", params(OptionFlowQuery),
    responses(
        (status = 200, body = OptionFlowVerificationPage),
        (status = 400, body = ErrorBody),
        (status = 404, body = ErrorBody),
        (status = 500, body = ErrorBody),
        (status = 503, body = ErrorBody)
    )
)]
pub(crate) async fn verification(
    Extension(sources): ReadSources,
    query: Result<Query<OptionFlowQuery>, QueryRejection>,
) -> Result<Json<OptionFlowVerificationPage>, ApiError> {
    let query = parse(query)?;
    let window = query.window()?;
    // The lookup key is the trade window. The owner supplies the original quote lookback.
    HistoricalWindow::new(window.start, window.end, window.start, 1).map_err(|_| invalid())?;
    let source = sources
        .flow_verifications
        .as_ref()
        .ok_or_else(unavailable)?;
    let access = source.display_access(OPRA_HISTORY).ok_or_else(missing)?;
    let result = (source.read)(window).ok_or_else(missing)?;
    let evidence = &result.evidence;
    if &result.access != access
        || evidence.con_id != window.con_id
        || result.window.start() != window.start
        || result.window.end() != window.end
        || (!access.record_local && result.durable_through.is_some())
    {
        return Err(failed());
    }
    let observation_coverage = match sources
        .flow_observations
        .as_ref()
        .and_then(|source| source.display_access(OPRA))
    {
        Some(opra_access) => coverage(
            result.observation_coverage.as_ref(),
            opra_access,
            window.con_id,
        )?,
        None => None,
    };
    let (trades, next_offset) = query.page(&evidence.trades);
    Ok(Json(OptionFlowVerificationPage {
        access: access.clone(),
        con_id: evidence.con_id,
        start: result.window.start(),
        end: result.window.end(),
        quote_start: result.window.quote_start(),
        multiplier: evidence.multiplier.clone(),
        verified_at: evidence.verified_at,
        max_quote_age_ms: evidence.max_quote_age_ms,
        counts: evidence.counts,
        issues: evidence.issues.clone(),
        observation_coverage,
        durable_through: result.durable_through,
        trades: trades.to_vec(),
        next_offset,
    }))
}

fn coverage(
    report: Option<&CoverageReport>,
    access: &ProductPolicy,
    con_id: i32,
) -> Result<Option<SubscriptionCoverage>, ApiError> {
    report
        .map(|report| {
            if report.key.product != Product::Opra || report.key.con_id != con_id {
                return Err(failed());
            }
            Ok(SubscriptionCoverage::from_report(access.clone(), report))
        })
        .transpose()
}
