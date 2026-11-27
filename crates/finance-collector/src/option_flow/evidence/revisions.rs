use std::collections::HashMap;

use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use finance_ib::historical::option_ticks::{HistoricalTick, HistoryTrade};
use finance_market_protocol::{NonEmptyText, U64Text};
use rust_decimal::Decimal;
use serde::Serialize;

use super::{ConditionMeaning, MappedCondition, VerifiedTrade};

/// The local within-request evidence key. This is never a provider trade ID.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct TradeOccurrence {
    pub page: u16,
    pub occurred_at: DateTime<Utc>,
    pub occurrence: u32,
}

impl From<&HistoricalTick<HistoryTrade>> for TradeOccurrence {
    fn from(tick: &HistoricalTick<HistoryTrade>) -> Self {
        Self {
            page: tick.batch.page,
            occurred_at: tick.row.occurred_at,
            occurrence: tick.occurrence,
        }
    }
}

/// Provider or trusted replay metadata for a single occurrence.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TradeAction {
    Original {
        native_id: NonEmptyText,
    },
    /// The linked historical row is a full replacement, including event time.
    Correction {
        target_native_id: NonEmptyText,
        revision: U64Text,
    },
    Cancellation {
        target_native_id: NonEmptyText,
        revision: U64Text,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TradeLink {
    pub occurrence: TradeOccurrence,
    pub action: TradeAction,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TradeStatus {
    Active,
    Superseded,
    Cancelled,
    AppliedCancellation,
    UnresolvedAmendment,
    NonTradeMarker,
}

pub(super) fn reconcile(trades: &mut [VerifiedTrade], links: &[TradeLink]) -> Result<()> {
    let mut occurrences = HashMap::new();
    for (idx, trade) in trades.iter_mut().enumerate() {
        ensure!(
            occurrences
                .insert(TradeOccurrence::from(&trade.trade), idx)
                .is_none(),
            "duplicate historical trade occurrence"
        );
        if is_amendment(trade.condition.meaning) {
            trade.status = TradeStatus::UnresolvedAmendment;
        } else if trade.trade.row.quantity.value() == Decimal::ZERO {
            // HMDS can use zero-size TRADES rows for halt/unhalt markers.
            trade.status = TradeStatus::NonTradeMarker;
        }
    }

    let mut targets = HashMap::new();
    let mut amendments: HashMap<&NonEmptyText, Vec<(u64, usize)>> = HashMap::new();
    for link in links {
        let &idx = occurrences
            .get(&link.occurrence)
            .context("link references an absent historical trade")?;
        let trade = &mut trades[idx];
        ensure!(
            trade.link.is_none(),
            "duplicate link for a historical occurrence"
        );
        trade.link = Some(link.clone());
        match &link.action {
            TradeAction::Original { native_id } => {
                ensure!(
                    trade.status == TradeStatus::Active,
                    "only a trade print can establish a native trade ID"
                );
                ensure!(
                    targets.insert(native_id, idx).is_none(),
                    "duplicate native trade ID"
                );
            }
            TradeAction::Correction {
                target_native_id,
                revision,
            }
            | TradeAction::Cancellation {
                target_native_id,
                revision,
            } => {
                ensure!(revision.get() > 0, "amendment revision must be positive");
                ensure!(
                    !matches!(link.action, TradeAction::Correction { .. })
                        || trade.trade.row.quantity.value() > Decimal::ZERO,
                    "a replacement trade requires positive size"
                );
                ensure!(
                    compatible(trade.condition.meaning, &link.action),
                    "condition and linked amendment contradict each other"
                );
                trade.status = TradeStatus::UnresolvedAmendment;
                amendments
                    .entry(target_native_id)
                    .or_default()
                    .push((revision.get(), idx));
            }
        }
    }

    for (target, revisions) in &mut amendments {
        revisions.sort_unstable();
        ensure!(
            !revisions.windows(2).any(|pair| pair[0].0 == pair[1].0),
            "duplicate revision for a native trade ID"
        );
        let Some(&original) = targets.get(target) else {
            continue;
        };
        let mut current = original;
        for &(_, idx) in revisions.iter() {
            if trades[original].trade.row.occurred_at > trades[idx].trade.batch.obtained_at {
                continue;
            }
            let cancelled = matches!(
                trades[idx].link.as_ref().map(|link| &link.action),
                Some(TradeAction::Cancellation { .. })
            );
            trades[current].status = if cancelled && trades[current].status == TradeStatus::Active {
                TradeStatus::Cancelled
            } else {
                TradeStatus::Superseded
            };
            trades[idx].status = if cancelled {
                TradeStatus::AppliedCancellation
            } else {
                TradeStatus::Active
            };
            current = idx;
        }
    }
    Ok(())
}

fn is_amendment(meaning: ConditionMeaning) -> bool {
    matches!(
        meaning,
        ConditionMeaning::Mapped(MappedCondition::Correction | MappedCondition::Cancellation)
    )
}

fn compatible(meaning: ConditionMeaning, action: &TradeAction) -> bool {
    !matches!(
        (meaning, action),
        (
            ConditionMeaning::Mapped(MappedCondition::Correction),
            TradeAction::Cancellation { .. }
        ) | (
            ConditionMeaning::Mapped(MappedCondition::Cancellation),
            TradeAction::Correction { .. }
        )
    )
}
