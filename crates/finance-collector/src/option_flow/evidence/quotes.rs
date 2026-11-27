use chrono::{TimeDelta, Utc};
use finance_ib::historical::option_ticks::{
    HistoricalIssue, HistoricalKind, HistoricalTick, HistoryQuote, HistoryTrade,
};
use rust_decimal::Decimal;
use serde::Serialize;
use utoipa::ToSchema;

use crate::option_flow::decimal::exact_difference;

const RULE_VERSION: &str = "historical-quote-test-v1";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum EstimatedSide {
    EstimatedBuy,
    EstimatedSell,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum InferenceConfidence {
    Estimate,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum QuoteReason {
    AtOrAboveAsk,
    AtOrBelowBid,
    CloserToAsk,
    CloserToBid,
    Midpoint,
    MissingQuote,
    QuoteCoverageUnknown,
    TradeOverlapConflict,
    SameSecondUnordered,
    PriorQuoteOrderUnknown,
    StaleQuote,
    InvalidQuote,
    LockedQuote,
    CrossedQuote,
    ProviderQuoteFlag,
    ProviderTradeFlag,
    ComplexLeg,
    ConditionUnrecognized,
    InactiveTrade,
    PrecisionUnavailable,
}

#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct QuoteEvidence {
    pub quote: HistoricalTick<HistoryQuote>,
    pub age_ms: i64,
}

#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct SideInference {
    pub direction: EstimatedSide,
    pub confidence: InferenceConfidence,
    pub reason: QuoteReason,
    pub rule_version: &'static str,
    pub quote: Option<QuoteEvidence>,
}

impl SideInference {
    fn new(direction: EstimatedSide, reason: QuoteReason, quote: Option<QuoteEvidence>) -> Self {
        Self {
            direction,
            confidence: if direction == EstimatedSide::Unknown {
                InferenceConfidence::Unknown
            } else {
                InferenceConfidence::Estimate
            },
            reason,
            rule_version: RULE_VERSION,
            quote,
        }
    }

    pub(super) fn unknown(reason: QuoteReason) -> Self {
        Self::new(EstimatedSide::Unknown, reason, None)
    }
}

pub(super) struct QuoteIndex<'a> {
    quotes: Vec<&'a HistoricalTick<HistoryQuote>>,
    issues: Vec<&'a HistoricalIssue>,
    max_age: TimeDelta,
}

impl<'a> QuoteIndex<'a> {
    pub(super) fn new(
        rows: &'a [HistoricalTick<HistoryQuote>],
        issues: &'a [HistoricalIssue],
        max_age: TimeDelta,
    ) -> Self {
        let mut quotes: Vec<_> = rows.iter().collect();
        quotes.sort_by_key(|quote| quote.row.occurred_at);
        Self {
            quotes,
            issues: issues
                .iter()
                .filter(|issue| issue.kind == HistoricalKind::BidAsk)
                .collect(),
            max_age,
        }
    }

    pub(super) fn infer(&self, trade: &HistoryTrade) -> SideInference {
        let second = trade.occurred_at.timestamp();
        let prior_end = self
            .quotes
            .partition_point(|tick| tick.row.occurred_at.timestamp() < second);
        if let Some(&same_second) = self
            .quotes
            .get(prior_end)
            .filter(|tick| tick.row.occurred_at.timestamp() == second)
        {
            let following_is_same_second = self
                .quotes
                .get(prior_end + 1)
                .is_some_and(|tick| tick.row.occurred_at.timestamp() == second);
            let quote = (!following_is_same_second).then(|| QuoteEvidence {
                quote: same_second.clone(),
                age_ms: (trade.occurred_at - same_second.row.occurred_at).num_milliseconds(),
            });
            return SideInference::new(
                EstimatedSide::Unknown,
                QuoteReason::SameSecondUnordered,
                quote,
            );
        }
        let Some(&quote) = self.quotes[..prior_end].last() else {
            let lower = trade
                .occurred_at
                .checked_sub_signed(self.max_age)
                .unwrap_or(chrono::DateTime::<Utc>::MIN_UTC);
            return SideInference::unknown(if self.has_gap(lower, trade.occurred_at) {
                QuoteReason::QuoteCoverageUnknown
            } else {
                QuoteReason::MissingQuote
            });
        };
        let age = trade.occurred_at - quote.row.occurred_at;
        let evidence = Some(QuoteEvidence {
            quote: quote.clone(),
            age_ms: age.num_milliseconds(),
        });
        if self.has_gap(quote.row.occurred_at, trade.occurred_at) {
            return SideInference::new(
                EstimatedSide::Unknown,
                QuoteReason::QuoteCoverageUnknown,
                evidence,
            );
        }
        if age > self.max_age {
            return SideInference::new(EstimatedSide::Unknown, QuoteReason::StaleQuote, evidence);
        }
        let quote_second = quote.row.occurred_at.timestamp();
        let prior_start = self.quotes[..prior_end]
            .partition_point(|tick| tick.row.occurred_at.timestamp() < quote_second);
        if self.quotes[prior_start..prior_end]
            .iter()
            .any(|tick| tick.row != quote.row)
        {
            return SideInference::unknown(QuoteReason::PriorQuoteOrderUnknown);
        }
        let (direction, reason) = estimate(trade, &quote.row);
        SideInference::new(direction, reason, evidence)
    }

    fn has_gap(&self, from: chrono::DateTime<Utc>, at: chrono::DateTime<Utc>) -> bool {
        self.issues
            .iter()
            .any(|issue| issue.start <= at && issue.end > from)
    }
}

fn estimate(trade: &HistoryTrade, quote: &HistoryQuote) -> (EstimatedSide, QuoteReason) {
    use EstimatedSide::{EstimatedBuy, EstimatedSell, Unknown};

    if quote.bid_past_low || quote.ask_past_high {
        return (Unknown, QuoteReason::ProviderQuoteFlag);
    }
    let (Some(bid), Some(ask), Some(bid_quantity), Some(ask_quantity)) = (
        &quote.bid,
        &quote.ask,
        &quote.bid_quantity,
        &quote.ask_quantity,
    ) else {
        return (Unknown, QuoteReason::InvalidQuote);
    };
    if [bid, ask, bid_quantity, ask_quantity]
        .iter()
        .any(|value| value.value() <= Decimal::ZERO)
    {
        return (Unknown, QuoteReason::InvalidQuote);
    }
    if bid.value() == ask.value() {
        return (Unknown, QuoteReason::LockedQuote);
    }
    if bid.value() > ask.value() {
        return (Unknown, QuoteReason::CrossedQuote);
    }
    if trade.price.value() >= ask.value() {
        return (EstimatedBuy, QuoteReason::AtOrAboveAsk);
    }
    if trade.price.value() <= bid.value() {
        return (EstimatedSell, QuoteReason::AtOrBelowBid);
    }
    let (Some(to_bid), Some(to_ask)) = (
        exact_difference(&trade.price, bid),
        exact_difference(ask, &trade.price),
    ) else {
        return (Unknown, QuoteReason::PrecisionUnavailable);
    };
    match to_bid.value().cmp(&to_ask.value()) {
        std::cmp::Ordering::Less => (EstimatedSell, QuoteReason::CloserToBid),
        std::cmp::Ordering::Greater => (EstimatedBuy, QuoteReason::CloserToAsk),
        std::cmp::Ordering::Equal => (Unknown, QuoteReason::Midpoint),
    }
}
