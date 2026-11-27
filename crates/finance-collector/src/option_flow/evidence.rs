//! Evidence and conservative direction estimates for historical option prints.

mod quotes;
mod revisions;

use std::collections::BTreeMap;

use anyhow::{Result, ensure};
use chrono::{DateTime, TimeDelta, Utc};
use finance_ib::{
    historical::option_ticks::{
        HistoricalIssue, HistoricalIssueReason, HistoricalKind, HistoricalOptionTicks,
        HistoricalTick, HistoryTrade,
    },
    market::{ConfirmedContract, ContractKind},
};
use finance_market_protocol::{DecimalText, Token};
use rust_decimal::Decimal;
use serde::Serialize;

use super::decimal::exact_premium;
pub use quotes::{EstimatedSide, InferenceConfidence, QuoteEvidence, QuoteReason, SideInference};
pub use revisions::{TradeAction, TradeLink, TradeOccurrence, TradeStatus};

/// Exact raw condition tokens are interpreted only under an identified mapping.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MappedCondition {
    Regular,
    ComplexLeg,
    ProviderSweep,
    ProviderBlock,
    Correction,
    Cancellation,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConditionMapping {
    version: Token,
    codes: BTreeMap<String, MappedCondition>,
}

impl ConditionMapping {
    pub fn new(version: Token, codes: BTreeMap<String, MappedCondition>) -> Result<Self> {
        ensure!(
            !codes.contains_key(""),
            "an empty condition always means the provider supplied no condition"
        );
        Ok(Self { version, codes })
    }

    pub fn version(&self) -> &str {
        self.version.as_str()
    }

    fn interpret(&self, raw: &str) -> ConditionInterpretation {
        let meaning = match self.codes.get(raw) {
            Some(condition) => ConditionMeaning::Mapped(*condition),
            None if raw.is_empty() => ConditionMeaning::Empty,
            None => ConditionMeaning::Unrecognized,
        };
        ConditionInterpretation {
            mapping_version: self.version.clone(),
            meaning,
        }
    }
}

impl Default for ConditionMapping {
    fn default() -> Self {
        Self {
            version: "ibkr-opaque-v1"
                .parse()
                .expect("constant version is a token"),
            codes: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "status", content = "condition", rename_all = "snake_case")]
pub enum ConditionMeaning {
    Empty,
    Unrecognized,
    Mapped(MappedCondition),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ConditionInterpretation {
    pub mapping_version: Token,
    pub meaning: ConditionMeaning,
}

#[derive(Clone, Debug)]
pub struct VerificationPolicy {
    max_quote_age: TimeDelta,
    condition_mapping: ConditionMapping,
    block_contracts: Option<DecimalText>,
    institution_scale_premium: Option<DecimalText>,
}

impl VerificationPolicy {
    pub fn new(max_quote_age: TimeDelta) -> Self {
        Self {
            max_quote_age,
            condition_mapping: ConditionMapping::default(),
            block_contracts: None,
            institution_scale_premium: None,
        }
    }

    pub fn with_condition_mapping(mut self, mapping: ConditionMapping) -> Self {
        self.condition_mapping = mapping;
        self
    }

    pub fn condition_mapping(&self) -> &ConditionMapping {
        &self.condition_mapping
    }

    pub fn with_size_thresholds(
        mut self,
        block_contracts: Option<DecimalText>,
        institution_scale_premium: Option<DecimalText>,
    ) -> Self {
        self.block_contracts = block_contracts;
        self.institution_scale_premium = institution_scale_premium;
        self
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.max_quote_age >= TimeDelta::zero(),
            "negative quote age limit"
        );
        for threshold in [&self.block_contracts, &self.institution_scale_premium]
            .into_iter()
            .flatten()
        {
            ensure!(
                threshold.value() > Decimal::ZERO,
                "nonpositive size-label threshold"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ParticipantIdentity {
    Unknown,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct SuspectedScale {
    /// A size heuristic, independent of any provider block condition.
    pub block_contracts_threshold: Option<DecimalText>,
    /// A premium heuristic, independent of either participant's identity.
    pub institution_scale_premium_threshold: Option<DecimalText>,
}

#[derive(Clone, Debug, Serialize)]
pub struct VerifiedTrade {
    pub trade: HistoricalTick<HistoryTrade>,
    pub condition: ConditionInterpretation,
    pub status: TradeStatus,
    pub inference: SideInference,
    pub premium: Option<DecimalText>,
    pub identity: ParticipantIdentity,
    pub suspected_scale: SuspectedScale,
    pub link: Option<TradeLink>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct HistoricalCounts {
    pub verified: usize,
    /// Verified active prints with unknown direction.
    pub unknown: usize,
    /// Raw TRADES rows in a conflicting pagination interval.
    pub pending_trade_rows: usize,
    /// Reported unavailable intervals, counted separately for each tick kind.
    pub unavailable_intervals: usize,
    pub pending_intervals: usize,
    pub unresolved_amendments: usize,
    pub non_trade_markers: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct HistoricalEvidence {
    pub con_id: i32,
    pub multiplier: DecimalText,
    pub verified_at: DateTime<Utc>,
    pub max_quote_age_ms: i64,
    pub counts: HistoricalCounts,
    pub trades: Vec<VerifiedTrade>,
    pub issues: Vec<HistoricalIssue>,
}

/// Verify completed historical request evidence. Provider corrections are
/// reconciled only with explicit linkage and revision metadata.
pub fn verify_history(
    contract: &ConfirmedContract,
    ticks: &HistoricalOptionTicks,
    verified_at: DateTime<Utc>,
    policy: &VerificationPolicy,
    links: &[TradeLink],
) -> Result<HistoricalEvidence> {
    let ContractKind::Option(option) = &contract.kind else {
        anyhow::bail!("historical option verification requires an option contract");
    };
    ensure!(
        contract.con_id > 0 && option.multiplier.value() > Decimal::ZERO,
        "invalid contract ID or option multiplier"
    );
    policy.validate()?;

    for (occurred_at, obtained_at) in ticks
        .trades
        .iter()
        .map(|tick| (tick.row.occurred_at, tick.batch.obtained_at))
        .chain(
            ticks
                .quotes
                .iter()
                .map(|tick| (tick.row.occurred_at, tick.batch.obtained_at)),
        )
    {
        ensure!(
            occurred_at <= obtained_at && obtained_at <= verified_at,
            "historical evidence time is out of order"
        );
    }
    ensure!(
        ticks.trades.iter().all(|tick| {
            tick.row.price.value() >= Decimal::ZERO && tick.row.quantity.value() >= Decimal::ZERO
        }),
        "historical option rows require nonnegative price and size"
    );

    let quote_index = quotes::QuoteIndex::new(&ticks.quotes, &ticks.issues, policy.max_quote_age);
    let mut trades: Vec<_> = ticks
        .trades
        .iter()
        .map(|trade| {
            let condition = policy
                .condition_mapping
                .interpret(&trade.row.special_conditions);
            VerifiedTrade {
                trade: trade.clone(),
                condition,
                status: TradeStatus::Active,
                inference: SideInference::unknown(QuoteReason::InactiveTrade),
                premium: None,
                identity: ParticipantIdentity::Unknown,
                suspected_scale: SuspectedScale::default(),
                link: None,
            }
        })
        .collect();
    let disputed = |trade: &VerifiedTrade| {
        ticks.issues.iter().any(|issue| {
            issue.kind == HistoricalKind::Trades
                && issue.reason == HistoricalIssueReason::InconsistentOverlap
                && (issue.start..issue.end).contains(&trade.trade.row.occurred_at)
        })
    };
    revisions::reconcile(&mut trades, links, disputed)?;

    let mut counts = HistoricalCounts::default();
    for trade in &mut trades {
        if disputed(trade) {
            trade.status = TradeStatus::PendingVerification;
            trade.inference = SideInference::unknown(QuoteReason::TradeOverlapConflict);
            counts.pending_trade_rows += 1;
            continue;
        }
        match trade.status {
            TradeStatus::Active => {
                counts.verified += 1;
                derive_trade_evidence(trade, &option.multiplier, &quote_index, policy);
                counts.unknown += usize::from(trade.inference.direction == EstimatedSide::Unknown);
            }
            TradeStatus::UnresolvedAmendment => counts.unresolved_amendments += 1,
            TradeStatus::NonTradeMarker => counts.non_trade_markers += 1,
            _ => {}
        }
    }
    for issue in &ticks.issues {
        ensure!(
            issue.observed_at <= verified_at,
            "historical issue is newer than verification"
        );
        match issue.reason {
            HistoricalIssueReason::ExpiredContract
            | HistoricalIssueReason::Unsupported
            | HistoricalIssueReason::NoHistory => counts.unavailable_intervals += 1,
            _ => counts.pending_intervals += 1,
        }
    }

    Ok(HistoricalEvidence {
        con_id: contract.con_id,
        multiplier: option.multiplier.clone(),
        verified_at,
        max_quote_age_ms: policy.max_quote_age.num_milliseconds(),
        counts,
        trades,
        issues: ticks.issues.clone(),
    })
}

fn derive_trade_evidence(
    trade: &mut VerifiedTrade,
    multiplier: &DecimalText,
    quotes: &quotes::QuoteIndex<'_>,
    policy: &VerificationPolicy,
) {
    let raw = &trade.trade.row;
    trade.premium = exact_premium(&raw.price, &raw.quantity, multiplier);
    trade.inference =
        if trade.condition.meaning == ConditionMeaning::Mapped(MappedCondition::ComplexLeg) {
            SideInference::unknown(QuoteReason::ComplexLeg)
        } else if raw.unreported || raw.past_limit {
            SideInference::unknown(QuoteReason::ProviderTradeFlag)
        } else if trade.condition.meaning == ConditionMeaning::Unrecognized {
            SideInference::unknown(QuoteReason::ConditionUnrecognized)
        } else {
            quotes.infer(raw)
        };

    if matches!(
        trade.inference.reason,
        QuoteReason::ComplexLeg
            | QuoteReason::ProviderTradeFlag
            | QuoteReason::ConditionUnrecognized
    ) {
        return;
    }
    trade.suspected_scale = SuspectedScale {
        block_contracts_threshold: policy
            .block_contracts
            .as_ref()
            .filter(|threshold| raw.quantity.value() >= threshold.value())
            .cloned(),
        institution_scale_premium_threshold: policy
            .institution_scale_premium
            .as_ref()
            .filter(|threshold| {
                trade
                    .premium
                    .as_ref()
                    .is_some_and(|amount| amount.value() >= threshold.value())
            })
            .cloned(),
    };
}

#[cfg(test)]
mod tests;
