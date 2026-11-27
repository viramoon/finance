use std::{collections::HashMap, path::Path};

use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use finance_ib::{
    historical::option_ticks::{
        HistoricalBatch, HistoricalIssue, HistoricalIssueReason, HistoricalKind,
        HistoricalOptionSource, HistoricalOptionTicks, HistoricalWindow, fetch_option_ticks,
    },
    market::{ConfirmedContract, ContractKind, LiquidHours, Product},
};
use finance_market_protocol::{
    AssetKind, Capability, DelayTier, Event, Instrument, InstrumentId, MarketKind, Message,
    NonEmptyText, OptionHistoryBatch, OptionHistoryQuote, OptionHistoryTrade, Reset, SchemaVersion,
    Token, U64Text,
};

use crate::{
    line_budget::CoverageReport,
    market_ingress::{GapCause, MarketIngress, ProductPolicy, QualityReport, RecoveryOutcome},
};

use super::{
    FlowUse,
    evidence::{HistoricalEvidence, VerificationPolicy, verify_history},
    nanos,
};

pub const OPRA_HISTORY: &str = "opra-history";

pub struct HistoricalFlow {
    pub evidence: HistoricalEvidence,
    pub observation_coverage: Option<CoverageReport>,
    pub durable_through: Option<U64Text>,
}

/// One F3 publisher for historical option evidence. The publisher writes
/// source callbacks plus acquisition metadata, with inference kept separately.
pub struct OptionHistoryIngress {
    ingress: MarketIngress,
    policy: ProductPolicy,
    capability: Capability,
    positions: HashMap<(InstrumentId, HistoricalKind), u64>,
    epoch: u64,
}

impl OptionHistoryIngress {
    pub fn open(data_root: &Path, policy: ProductPolicy, usage: FlowUse) -> Result<Self> {
        usage.authorize(&policy, OPRA_HISTORY)?;
        let recording_root;
        let data_root = if policy.record_local {
            recording_root = data_root
                .canonicalize()
                .context("option history data root")?;
            recording_root.as_path()
        } else {
            data_root
        };
        let capability = Capability {
            schema_version: SchemaVersion::V3,
            source: policy.source.clone(),
            product: policy.product.clone(),
            message_kinds: vec![
                MarketKind::OptionHistoryTrade,
                MarketKind::OptionHistoryQuote,
            ],
            max_depth: 0,
            can_backfill: true,
            delay_tier: DelayTier::Replay,
        };
        let ingress = MarketIngress::open(data_root, policy.clone())?;
        Ok(Self {
            epoch: ingress.writer_generation().unwrap_or(1),
            positions: HashMap::new(),
            ingress,
            policy,
            capability,
        })
    }

    pub fn ingress(&mut self) -> &mut MarketIngress {
        &mut self.ingress
    }

    pub async fn verify(
        &mut self,
        source: &impl HistoricalOptionSource,
        contract: &ConfirmedContract,
        instrument: &Instrument,
        window: HistoricalWindow,
        policy: &VerificationPolicy,
        coverage: Option<CoverageReport>,
    ) -> Result<HistoricalFlow> {
        self.validate_binding(contract, instrument, coverage.as_ref())?;
        let ticks = fetch_option_ticks(source, contract, window).await;
        let evidence = verify_history(contract, &ticks, source.now(), policy, &[])?;
        self.publish(instrument, &ticks, policy.condition_mapping().version())?;
        let durable_through = self.ingress.flush()?.map(Into::into);
        for issue in &ticks.issues {
            self.ingress
                .report_quality(issue_report(instrument, issue)?)?;
        }
        if let Some(coverage) = &coverage
            && !ticks
                .issues
                .iter()
                .any(|issue| issue.kind == HistoricalKind::Trades)
        {
            self.report_backfilled(instrument, &contract.liquid_hours, coverage, window, &ticks)?;
        }
        Ok(HistoricalFlow {
            evidence,
            observation_coverage: coverage,
            durable_through,
        })
    }

    fn validate_binding(
        &self,
        contract: &ConfirmedContract,
        instrument: &Instrument,
        coverage: Option<&CoverageReport>,
    ) -> Result<()> {
        instrument.validate()?;
        let ContractKind::Option(option) = &contract.kind else {
            anyhow::bail!("historical option evidence requires an option contract");
        };
        let con_id = contract.con_id.to_string();
        ensure!(
            contract.con_id > 0
                && instrument.asset_kind == AssetKind::Option
                && instrument.quote_currency.as_str() == "USD"
                && instrument.contract_multiplier.value() == option.multiplier.value()
                && instrument.supplier_symbols.iter().any(|mapping| {
                    mapping.source == self.policy.source
                        && mapping.product == self.policy.product
                        && mapping.symbol.as_str() == con_id
                }),
            "historical option instrument does not match the confirmed IBKR contract"
        );
        ensure!(
            coverage.is_none_or(|report| report.key.con_id == contract.con_id
                && report.key.product == Product::Opra),
            "option observation coverage belongs to a different contract"
        );
        Ok(())
    }

    fn report_backfilled(
        &mut self,
        instrument: &Instrument,
        sessions: &[LiquidHours],
        coverage: &CoverageReport,
        window: HistoricalWindow,
        ticks: &HistoricalOptionTicks,
    ) -> Result<()> {
        for gap in &coverage.uncovered {
            let start = gap.start.max(window.start());
            let end = gap.end.min(window.end());
            if start >= end {
                continue;
            }
            if !sessions.iter().any(|session| {
                matches!(session,
                    LiquidHours::Open { start: open, end: close } if *open <= start && end <= *close
                )
            }) {
                continue;
            }
            let count = ticks
                .trades
                .iter()
                .filter(|tick| (start..end).contains(&tick.row.occurred_at))
                .count();
            let (from_ts, to_ts) = quality_bounds(start, end)?;
            self.ingress.report_quality(QualityReport {
                channel: channel(HistoricalKind::Trades),
                instrument_id: instrument.instrument_id.clone(),
                from_ts,
                to_ts,
                cause: GapCause::SourceGap,
                outcome: Some(RecoveryOutcome::TickBackfilled),
                count: Some((count as u64).into()),
            })?;
        }
        Ok(())
    }

    fn publish(
        &mut self,
        instrument: &Instrument,
        ticks: &HistoricalOptionTicks,
        mapping_version: &str,
    ) -> Result<()> {
        if !(self.policy.record_local || self.policy.display_live) {
            return Ok(());
        }
        let mapping_version: NonEmptyText = mapping_version.parse()?;
        let trades = ticks.trades.iter().map(|tick| {
            Ok((
                HistoricalKind::Trades,
                tick.row.occurred_at,
                tick.batch.obtained_at,
                Message::OptionHistoryTrade(OptionHistoryTrade {
                    batch: batch(&tick.batch, tick.occurrence)?,
                    price: tick.row.price.clone(),
                    quantity: tick.row.quantity.clone(),
                    contract_multiplier: instrument.contract_multiplier.clone(),
                    exchange: tick.row.exchange.clone(),
                    special_conditions: tick.row.special_conditions.clone(),
                    condition_mapping_version: mapping_version.clone(),
                    past_limit: tick.row.past_limit,
                    unreported: tick.row.unreported,
                }),
            ))
        });
        let quotes = ticks.quotes.iter().map(|tick| {
            Ok((
                HistoricalKind::BidAsk,
                tick.row.occurred_at,
                tick.batch.obtained_at,
                Message::OptionHistoryQuote(OptionHistoryQuote {
                    batch: batch(&tick.batch, tick.occurrence)?,
                    bid: tick.row.bid.clone(),
                    ask: tick.row.ask.clone(),
                    bid_quantity: tick.row.bid_quantity.clone(),
                    ask_quantity: tick.row.ask_quantity.clone(),
                    bid_past_low: tick.row.bid_past_low,
                    ask_past_high: tick.row.ask_past_high,
                }),
            ))
        });
        let mut rows = trades.chain(quotes).collect::<Result<Vec<_>>>()?;
        rows.sort_by_key(|(_, _, obtained, _)| *obtained);

        let epoch = self.epoch;
        let mut positions = HashMap::new();
        let mut events = Vec::with_capacity(rows.len() + 2);
        for (kind, occurred, obtained, message) in rows {
            let key = (instrument.instrument_id.clone(), kind);
            let sequence = positions
                .entry(key.clone())
                .or_insert_with(|| self.positions.get(&key).copied());
            let mut build = |seq: u64, schema_version, exchange_ts, message| -> Result<()> {
                let receive_ts = nanos(obtained)?.into();
                let event = Event {
                    schema_version,
                    source: self.policy.source.clone(),
                    product: self.policy.product.clone(),
                    channel: channel(kind),
                    instrument_id: instrument.instrument_id.clone(),
                    asset_kind: instrument.asset_kind,
                    venue: instrument.venue.clone(),
                    exchange_ts,
                    receive_ts,
                    first_available_ts: receive_ts,
                    epoch: epoch.into(),
                    seq: seq.into(),
                    cursor: format!("{epoch}:{seq}").parse()?,
                    revision: 0.into(),
                    license: self.policy.license.clone(),
                    native_id: None,
                    message,
                };
                event.validate_against(instrument, &self.capability)?;
                events.push((kind, event));
                Ok(())
            };
            if sequence.is_none() {
                build(
                    0,
                    SchemaVersion::V1,
                    None,
                    Message::Reset(Reset {
                        reason: "option_history_publisher_started".parse()?,
                    }),
                )?;
            }
            let next = sequence
                .unwrap_or(0)
                .checked_add(1)
                .context("option history sequence exhausted")?;
            build(
                next,
                SchemaVersion::V3,
                Some(nanos(occurred)?.into()),
                message,
            )?;
            *sequence = Some(next);
        }
        for (kind, event) in events {
            let sequence = event.seq.get();
            self.ingress.publish(event, instrument, &self.capability)?;
            self.positions
                .insert((instrument.instrument_id.clone(), kind), sequence);
        }
        Ok(())
    }
}

#[cfg(test)]
mod path_tests;
#[cfg(test)]
mod tests;

fn channel(kind: HistoricalKind) -> Token {
    match kind {
        HistoricalKind::Trades => "option-history-trades",
        HistoricalKind::BidAsk => "option-history-quotes",
    }
    .parse()
    .expect("history channel token")
}

fn batch(batch: &HistoricalBatch, occurrence: u32) -> Result<OptionHistoryBatch> {
    Ok(OptionHistoryBatch {
        requested_start_ts: nanos(batch.requested_start)?.into(),
        page: u64::from(batch.page).into(),
        occurrence: u64::from(occurrence).into(),
        request_id: batch
            .provider_request_id
            .map(u64::try_from)
            .transpose()?
            .map(Into::into),
    })
}

fn issue_report(instrument: &Instrument, issue: &HistoricalIssue) -> Result<QualityReport> {
    let (cause, outcome) = match issue.reason {
        HistoricalIssueReason::ExpiredContract
        | HistoricalIssueReason::Unsupported
        | HistoricalIssueReason::NoHistory => (
            GapCause::SourceGap,
            Some(RecoveryOutcome::HistoryUnrecoverable),
        ),
        HistoricalIssueReason::PermissionDenied => (GapCause::LicenseSkip, None),
        HistoricalIssueReason::Pacing => (GapCause::SourceRateLimit, None),
        HistoricalIssueReason::Disconnected => (GapCause::Disconnected, None),
        _ => (GapCause::SourceGap, None),
    };
    let (from_ts, to_ts) = quality_bounds(issue.start, issue.end)?;
    Ok(QualityReport {
        channel: channel(issue.kind),
        instrument_id: instrument.instrument_id.clone(),
        from_ts,
        to_ts,
        cause,
        outcome,
        count: None,
    })
}

fn quality_bounds(start: DateTime<Utc>, end: DateTime<Utc>) -> Result<(U64Text, U64Text)> {
    ensure!(start < end, "historical quality interval must be nonempty");
    // F9b and historical requests are half-open. F3 reports include both endpoints.
    Ok((nanos(start)?.into(), (nanos(end)? - 1).into()))
}
