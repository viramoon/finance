//! Bounded, read-only historical SMART option trades and bid/ask evidence.

mod paper;
pub use paper::PaperOptionHistory;

use crate::market::{ConfirmedContract, ContractKind, option_market_date};
use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};
use finance_market_protocol::DecimalText;
use rust_decimal::Decimal;
use serde::Serialize;
use std::{collections::HashMap, hash::Hash};
use utoipa::ToSchema;

const PAGE_LIMIT: u16 = 1_000;
const MAX_PAGES_PER_KIND: u16 = 64;
const MAX_RESPONSE_ROWS: usize = 100_000;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum HistoricalKind {
    Trades,
    BidAsk,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum HistoricalIssueReason {
    ExpiredContract,
    NoHistory,
    PermissionDenied,
    Pacing,
    Timeout,
    Disconnected,
    Unsupported,
    ProviderError,
    IncompletePage,
    PageLimit,
    InconsistentOverlap,
    UnconfirmedContract,
    FutureWindow,
    PacingState,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, ToSchema)]
pub struct HistoricalIssue {
    pub kind: HistoricalKind,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub observed_at: DateTime<Utc>,
    pub reason: HistoricalIssueReason,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, ToSchema)]
pub struct HistoryTrade {
    pub occurred_at: DateTime<Utc>,
    pub price: DecimalText,
    pub quantity: DecimalText,
    pub exchange: String,
    pub special_conditions: String,
    pub past_limit: bool,
    pub unreported: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, ToSchema)]
pub struct HistoryQuote {
    pub occurred_at: DateTime<Utc>,
    pub bid: Option<DecimalText>,
    pub ask: Option<DecimalText>,
    pub bid_quantity: Option<DecimalText>,
    pub ask_quantity: Option<DecimalText>,
    pub bid_past_low: bool,
    pub ask_past_high: bool,
}

/// Request and receipt evidence. The broker request ID is scoped to its connection.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, ToSchema)]
pub struct HistoricalBatch {
    pub kind: HistoricalKind,
    pub requested_start: DateTime<Utc>,
    pub obtained_at: DateTime<Utc>,
    pub provider_request_id: Option<i32>,
    pub page: u16,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, ToSchema)]
pub struct HistoricalTick<R> {
    pub row: R,
    pub batch: HistoricalBatch,
    /// One-based distinct row position within this complete second and kind.
    /// IB does not supply an execution ID or establish order across streams.
    pub occurrence: u32,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct HistoricalOptionTicks {
    pub trades: Vec<HistoricalTick<HistoryTrade>>,
    pub quotes: Vec<HistoricalTick<HistoryQuote>>,
    pub issues: Vec<HistoricalIssue>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error(
    "option history requires a single Eastern session date, UTC second bounds and a 1–64 page budget per kind"
)]
pub struct InvalidHistoricalWindow;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HistoricalWindow {
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    quote_start: DateTime<Utc>,
    max_pages_per_kind: u16,
}

impl HistoricalWindow {
    /// Half-open, single-session RTH window. A bounded quote lookback on the
    /// same market date can supply a quote preceding the first requested trade.
    pub fn new(
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        quote_start: DateTime<Utc>,
        max_pages_per_kind: u16,
    ) -> Result<Self, InvalidHistoricalWindow> {
        if !(1..=MAX_PAGES_PER_KIND).contains(&max_pages_per_kind)
            || quote_start.timestamp() < 0
            || quote_start > start
            || start >= end
            || [quote_start, start, end]
                .iter()
                .any(|value| value.timestamp_subsec_nanos() != 0)
        {
            return Err(InvalidHistoricalWindow);
        }
        let date = |time| option_market_date(time).map_err(|_| InvalidHistoricalWindow);
        if date(quote_start)? != date(end - TimeDelta::seconds(1))? {
            return Err(InvalidHistoricalWindow);
        }
        Ok(Self {
            start,
            end,
            quote_start,
            max_pages_per_kind,
        })
    }

    pub fn start(self) -> DateTime<Utc> {
        self.start
    }

    pub fn end(self) -> DateTime<Utc> {
        self.end
    }

    pub fn quote_start(self) -> DateTime<Utc> {
        self.quote_start
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HistoricalPageRequest {
    pub kind: HistoricalKind,
    pub start: DateTime<Utc>,
    pub limit: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HistoricalRows {
    Trades(Vec<HistoryTrade>),
    BidAsk(Vec<HistoryQuote>),
}

/// A page returned only after the provider has delivered the terminal marker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoricalPage {
    pub rows: HistoricalRows,
    pub obtained_at: DateTime<Utc>,
    pub provider_request_id: Option<i32>,
}

#[async_trait]
pub trait HistoricalOptionSource: Send + Sync {
    async fn option_tick_page(
        &self,
        contract: &ConfirmedContract,
        request: HistoricalPageRequest,
    ) -> Result<HistoricalPage, HistoricalIssueReason>;

    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

pub async fn fetch_option_ticks(
    source: &(impl HistoricalOptionSource + ?Sized),
    contract: &ConfirmedContract,
    window: HistoricalWindow,
) -> HistoricalOptionTicks {
    let mut result = HistoricalOptionTicks::default();
    result.trades = fetch_pages(source, contract, window.start, window, &mut result.issues).await;
    result.quotes = fetch_pages(
        source,
        contract,
        window.quote_start,
        window,
        &mut result.issues,
    )
    .await;
    result
}

pub(super) fn contract_available(
    contract: &ConfirmedContract,
    now: DateTime<Utc>,
) -> Result<(), HistoricalIssueReason> {
    let ContractKind::Option(option) = &contract.kind else {
        return Err(HistoricalIssueReason::UnconfirmedContract);
    };
    if contract.con_id <= 0 || option.multiplier.value() <= Decimal::ZERO {
        return Err(HistoricalIssueReason::UnconfirmedContract);
    }
    let today = option_market_date(now).map_err(|_| HistoricalIssueReason::ProviderError)?;
    if option.expiry < today {
        return Err(HistoricalIssueReason::ExpiredContract);
    }
    Ok(())
}

trait TickRow: Sized {
    const KIND: HistoricalKind;
    type Key: Eq + Hash;
    fn occurred_at(&self) -> DateTime<Utc>;
    fn key(&self) -> Self::Key;
    fn take_rows(rows: HistoricalRows) -> Option<Vec<Self>>;
}

impl TickRow for HistoryTrade {
    const KIND: HistoricalKind = HistoricalKind::Trades;
    type Key = (Decimal, Decimal, String, String, bool, bool);

    fn occurred_at(&self) -> DateTime<Utc> {
        self.occurred_at
    }

    fn key(&self) -> Self::Key {
        (
            self.price.value(),
            self.quantity.value(),
            self.exchange.clone(),
            self.special_conditions.clone(),
            self.past_limit,
            self.unreported,
        )
    }

    fn take_rows(rows: HistoricalRows) -> Option<Vec<Self>> {
        match rows {
            HistoricalRows::Trades(trades) => Some(trades),
            HistoricalRows::BidAsk(_) => None,
        }
    }
}

impl TickRow for HistoryQuote {
    const KIND: HistoricalKind = HistoricalKind::BidAsk;
    type Key = (
        Option<Decimal>,
        Option<Decimal>,
        Option<Decimal>,
        Option<Decimal>,
        bool,
        bool,
    );

    fn occurred_at(&self) -> DateTime<Utc> {
        self.occurred_at
    }

    fn key(&self) -> Self::Key {
        (
            self.bid.as_ref().map(DecimalText::value),
            self.ask.as_ref().map(DecimalText::value),
            self.bid_quantity.as_ref().map(DecimalText::value),
            self.ask_quantity.as_ref().map(DecimalText::value),
            self.bid_past_low,
            self.ask_past_high,
        )
    }

    fn take_rows(rows: HistoricalRows) -> Option<Vec<Self>> {
        match rows {
            HistoricalRows::Trades(_) => None,
            HistoricalRows::BidAsk(quotes) => Some(quotes),
        }
    }
}

async fn fetch_pages<R: TickRow>(
    source: &(impl HistoricalOptionSource + ?Sized),
    contract: &ConfirmedContract,
    start: DateTime<Utc>,
    window: HistoricalWindow,
    issues: &mut Vec<HistoricalIssue>,
) -> Vec<HistoricalTick<R>> {
    let mut ticks = Vec::new();
    let mut cursor = start;
    for page in 1..=window.max_pages_per_kind {
        let now = source.now();
        let result = match contract_available(contract, now) {
            Err(reason) => Err(reason),
            Ok(()) if window.end > now => Err(HistoricalIssueReason::FutureWindow),
            Ok(()) => source
                .option_tick_page(
                    contract,
                    HistoricalPageRequest {
                        kind: R::KIND,
                        start: cursor,
                        limit: PAGE_LIMIT,
                    },
                )
                .await
                .and_then(|reply| append_page(&mut ticks, start, cursor, window.end, page, reply)),
        };
        match result {
            Ok(Some(next)) => cursor = next,
            Ok(None) => return ticks,
            Err(reason) => {
                let issue_start = if reason == HistoricalIssueReason::InconsistentOverlap {
                    // The overlapping part also lost its complete-second evidence.
                    start
                } else {
                    cursor
                };
                issues.push(HistoricalIssue {
                    kind: R::KIND,
                    start: issue_start,
                    end: window.end,
                    reason,
                    observed_at: source.now(),
                });
                return ticks;
            }
        }
    }
    issues.push(HistoricalIssue {
        kind: R::KIND,
        start: cursor,
        end: window.end,
        reason: HistoricalIssueReason::PageLimit,
        observed_at: source.now(),
    });
    ticks
}

fn append_page<R: TickRow>(
    ticks: &mut Vec<HistoricalTick<R>>,
    start: DateTime<Utc>,
    cursor: DateTime<Utc>,
    end: DateTime<Utc>,
    page: u16,
    reply: HistoricalPage,
) -> Result<Option<DateTime<Utc>>, HistoricalIssueReason> {
    let rows = R::take_rows(reply.rows).ok_or(HistoricalIssueReason::IncompletePage)?;
    if rows.len() > MAX_RESPONSE_ROWS
        || rows.iter().any(|row| {
            let time = row.occurred_at();
            time.timestamp_subsec_nanos() != 0 || time > reply.obtained_at
        })
        || rows
            .windows(2)
            .any(|rows| rows[0].occurred_at() > rows[1].occurred_at())
    {
        return Err(HistoricalIssueReason::IncompletePage);
    }
    let overlap_start = rows.partition_point(|row| row.occurred_at() < start);
    let new_start = rows.partition_point(|row| row.occurred_at() < cursor);
    for second in rows[overlap_start..new_start]
        .chunk_by(|left, right| left.occurred_at() == right.occurred_at())
    {
        let time = second[0].occurred_at();
        let previous_start = ticks.partition_point(|tick| tick.row.occurred_at() < time);
        let previous_end = ticks.partition_point(|tick| tick.row.occurred_at() <= time);
        let previous = counts(
            ticks[previous_start..previous_end]
                .iter()
                .map(|tick| tick.row.key()),
        );
        if previous != counts(second.iter().map(TickRow::key)) {
            return Err(HistoricalIssueReason::InconsistentOverlap);
        }
    }
    if !rows.is_empty() && new_start == rows.len() {
        return Err(HistoricalIssueReason::IncompletePage);
    }

    let batch = HistoricalBatch {
        kind: R::KIND,
        requested_start: cursor,
        obtained_at: reply.obtained_at,
        provider_request_id: reply.provider_request_id,
        page,
    };
    // IB completes the last returned second even when it exceeds the requested
    // size. A short page, or a row at/past our end, finishes the session window.
    let next = match rows.last().map(TickRow::occurred_at) {
        _ if rows.len() < usize::from(PAGE_LIMIT) => None,
        Some(last) if last >= end - TimeDelta::seconds(1) => None,
        Some(last) if last >= cursor => Some(last + TimeDelta::seconds(1)),
        _ => return Err(HistoricalIssueReason::IncompletePage),
    };
    let mut second = None;
    let mut occurrence = 0;
    for row in rows
        .into_iter()
        .skip(new_start)
        .take_while(|row| row.occurred_at() < end)
    {
        if second != Some(row.occurred_at()) {
            second = Some(row.occurred_at());
            occurrence = 0;
        }
        occurrence += 1;
        ticks.push(HistoricalTick {
            row,
            batch,
            occurrence,
        });
    }
    Ok(next)
}

fn counts<K: Eq + Hash>(keys: impl Iterator<Item = K>) -> HashMap<K, usize> {
    let mut counts = HashMap::new();
    for key in keys {
        *counts.entry(key).or_default() += 1;
    }
    counts
}

#[cfg(test)]
mod tests;
