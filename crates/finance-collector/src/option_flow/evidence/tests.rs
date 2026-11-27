use chrono::NaiveDate;
use finance_ib::{
    historical::option_ticks::{HistoricalBatch, HistoricalKind, HistoryQuote},
    market::{OptionContract, OptionRight},
};

use super::*;

// These fixed test vectors are constructed from invented prices, sizes,
// condition tokens and clocks. They contain no observed market data.
fn at(second: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(1_800_000_000 + second, 0).unwrap()
}

fn batch(kind: HistoricalKind) -> HistoricalBatch {
    HistoricalBatch {
        kind,
        requested_start: at(0),
        obtained_at: at(20),
        provider_request_id: None,
        page: 0,
    }
}

fn contract() -> ConfirmedContract {
    ConfirmedContract {
        con_id: 70,
        symbol: "SYNTH".into(),
        primary_exchange: None,
        liquid_hours: vec![],
        kind: ContractKind::Option(OptionContract {
            expiry: NaiveDate::from_ymd_opt(2027, 3, 1).unwrap(),
            strike: "1".parse().unwrap(),
            right: OptionRight::Call,
            multiplier: "25".parse().unwrap(),
            underlying_con_id: 60,
        }),
    }
}

fn trade(occurrence: u32, price: &str, conditions: &str) -> HistoricalTick<HistoryTrade> {
    HistoricalTick {
        row: HistoryTrade {
            occurred_at: at(4),
            price: price.parse().unwrap(),
            quantity: "2".parse().unwrap(),
            exchange: "SYNTH_EXCHANGE".into(),
            special_conditions: conditions.into(),
            past_limit: false,
            unreported: false,
        },
        batch: batch(HistoricalKind::Trades),
        occurrence,
    }
}

fn quote(second: i64, bid: &str, ask: &str) -> HistoricalTick<HistoryQuote> {
    HistoricalTick {
        row: HistoryQuote {
            occurred_at: at(second),
            bid: Some(bid.parse().unwrap()),
            ask: Some(ask.parse().unwrap()),
            bid_quantity: Some("12".parse().unwrap()),
            ask_quantity: Some("24".parse().unwrap()),
            bid_past_low: false,
            ask_past_high: false,
        },
        batch: batch(HistoricalKind::BidAsk),
        occurrence: 0,
    }
}

fn policy() -> VerificationPolicy {
    const SYNTHETIC_CONDITIONS: [(&str, MappedCondition); 4] = [
        ("SYNTH_LEG", MappedCondition::ComplexLeg),
        ("SYNTH_SWEEP", MappedCondition::ProviderSweep),
        ("SYNTH_CORRECTION", MappedCondition::Correction),
        ("SYNTH_CANCEL", MappedCondition::Cancellation),
    ];
    VerificationPolicy::new(TimeDelta::seconds(2)).with_condition_mapping(
        ConditionMapping::new(
            "test-map-v1".parse().unwrap(),
            SYNTHETIC_CONDITIONS
                .into_iter()
                .map(|(code, condition)| (code.to_owned(), condition))
                .collect(),
        )
        .unwrap(),
    )
}

fn issue(kind: HistoricalKind, reason: HistoricalIssueReason) -> HistoricalIssue {
    HistoricalIssue {
        kind,
        start: at(3),
        end: at(6),
        reason,
        observed_at: at(21),
    }
}

fn verify(ticks: HistoricalOptionTicks, links: &[TradeLink]) -> HistoricalEvidence {
    verify_history(&contract(), &ticks, at(23), &policy(), links).unwrap()
}

#[test]
fn same_second_same_price_two_trades_remain_verified() {
    let report = verify(
        HistoricalOptionTicks {
            trades: vec![trade(0, "1.00005", ""), trade(1, "1.00005", "")],
            quotes: vec![quote(2, "1.0", "1.00005")],
            issues: vec![],
        },
        &[],
    );
    assert_eq!(report.counts.verified, 2);
    assert_eq!(report.counts.unknown, 0);
    for entry in &report.trades {
        assert_eq!(entry.inference.direction, EstimatedSide::EstimatedBuy);
        assert_eq!(entry.inference.confidence, InferenceConfidence::Estimate);
        assert_eq!(entry.premium.as_ref().unwrap().as_str(), "50.0025");
    }
    assert_ne!(
        report.trades[0].trade.occurrence,
        report.trades[1].trade.occurrence
    );

    let wire = serde_json::to_value(&report).unwrap();
    assert_eq!(
        wire["verified_at"],
        at(23).to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    );
    assert_eq!(
        wire["trades"][0]["trade"]["row"]["occurred_at"],
        at(4).to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    );
    assert_eq!(
        wire["trades"][0]["trade"]["batch"]["obtained_at"],
        at(20).to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    );
    assert_eq!(wire["trades"][0]["premium"], "50.0025");
    assert_eq!(wire["trades"][0]["identity"], "unknown");
}

#[test]
fn trade_occurrence_positions_restart_at_each_second() {
    let before = trade(1, "2", "");
    let mut after = before.clone();
    after.row.occurred_at = at(5);
    assert_ne!(
        TradeOccurrence::from(&before),
        TradeOccurrence::from(&after)
    );
    let report = verify(
        HistoricalOptionTicks {
            trades: vec![before, after],
            quotes: vec![],
            issues: vec![],
        },
        &[],
    );
    assert_eq!(report.counts.verified, 2);
}

#[test]
fn zero_size_halt_markers_are_preserved_without_trade_inference() {
    let mut halt = trade(1, "0", "");
    halt.row.quantity = "0".parse().unwrap();
    halt.row.past_limit = true;
    let mut unhalt = halt.clone();
    unhalt.row.occurred_at = at(5);
    unhalt.row.past_limit = false;
    let report = verify(
        HistoricalOptionTicks {
            trades: vec![halt, unhalt],
            quotes: vec![quote(3, "1", "2")],
            issues: vec![],
        },
        &[],
    );
    assert_eq!(report.counts.verified, 0);
    assert_eq!(report.counts.non_trade_markers, 2);
    for marker in &report.trades {
        assert_eq!(marker.status, TradeStatus::NonTradeMarker);
        assert!(marker.premium.is_none());
        assert_eq!(marker.inference.direction, EstimatedSide::Unknown);
    }
}

#[test]
fn complex_leg_and_empty_condition_keep_distinct_evidence() {
    let ticks = HistoricalOptionTicks {
        trades: vec![
            trade(0, "2", "SYNTH_LEG"),
            trade(1, "2", ""),
            trade(2, "2", "SYNTH_SWEEP"),
            trade(3, "2", "OTHER"),
        ],
        quotes: vec![quote(2, "1.8", "2")],
        issues: vec![],
    };
    let thresholds =
        policy().with_size_thresholds(Some("2".parse().unwrap()), Some("100".parse().unwrap()));
    let report = verify_history(&contract(), &ticks, at(23), &thresholds, &[]).unwrap();
    let [leg, empty, sweep, unknown] = report.trades.as_slice() else {
        panic!("expected four rows")
    };
    assert_eq!(report.counts.verified, 4);
    assert_eq!(report.counts.unknown, 2);
    assert_eq!(leg.inference.reason, QuoteReason::ComplexLeg);
    assert_eq!(leg.condition.mapping_version.as_str(), "test-map-v1");
    assert_eq!(leg.trade.row.special_conditions, "SYNTH_LEG");
    assert_eq!(leg.suspected_scale, SuspectedScale::default());
    assert_eq!(empty.condition.meaning, ConditionMeaning::Empty);
    assert_eq!(empty.trade.row.special_conditions, "");
    assert_eq!(
        empty
            .suspected_scale
            .block_contracts_threshold
            .as_ref()
            .unwrap()
            .as_str(),
        "2"
    );
    assert_eq!(
        empty
            .suspected_scale
            .institution_scale_premium_threshold
            .as_ref()
            .unwrap()
            .as_str(),
        "100"
    );
    assert_eq!(empty.identity, ParticipantIdentity::Unknown);
    assert_eq!(
        sweep.condition.meaning,
        ConditionMeaning::Mapped(MappedCondition::ProviderSweep)
    );
    assert_eq!(unknown.condition.meaning, ConditionMeaning::Unrecognized);
    assert_eq!(unknown.trade.row.special_conditions, "OTHER");
    assert_eq!(unknown.inference.reason, QuoteReason::ConditionUnrecognized);
}

#[test]
fn stale_quote_produces_unknown_with_age() {
    let report = verify(
        HistoricalOptionTicks {
            trades: vec![trade(0, "2", "")],
            quotes: vec![quote(1, "1.8", "2")],
            issues: vec![],
        },
        &[],
    );
    let inference = &report.trades[0].inference;
    assert_eq!(inference.direction, EstimatedSide::Unknown);
    assert_eq!(inference.reason, QuoteReason::StaleQuote);
    assert_eq!(inference.quote.as_ref().unwrap().age_ms, 3000);
    assert_eq!(report.counts.unknown, 1);
}

#[test]
fn quote_pairing_requires_fresh_valid_unambiguous_predecessor() {
    for (price, quotes, reason, direction) in [
        (
            "2",
            vec![quote(2, "1", "2"), quote(4, "1", "2")],
            QuoteReason::SameSecondUnordered,
            EstimatedSide::Unknown,
        ),
        (
            "2",
            vec![quote(2, "1", "2"), quote(2, "1.1", "2")],
            QuoteReason::PriorQuoteOrderUnknown,
            EstimatedSide::Unknown,
        ),
        (
            "2",
            vec![quote(2, "2", "2")],
            QuoteReason::LockedQuote,
            EstimatedSide::Unknown,
        ),
        (
            "2",
            vec![quote(2, "2.1", "2")],
            QuoteReason::CrossedQuote,
            EstimatedSide::Unknown,
        ),
        (
            "2",
            vec![quote(2, "0", "2")],
            QuoteReason::InvalidQuote,
            EstimatedSide::Unknown,
        ),
        (
            "2",
            vec![quote(5, "1", "2")],
            QuoteReason::MissingQuote,
            EstimatedSide::Unknown,
        ),
        (
            "1.5",
            vec![quote(2, "1", "2")],
            QuoteReason::Midpoint,
            EstimatedSide::Unknown,
        ),
        (
            "1.75",
            vec![quote(2, "1", "2")],
            QuoteReason::CloserToAsk,
            EstimatedSide::EstimatedBuy,
        ),
        (
            "1.25",
            vec![quote(2, "1", "2")],
            QuoteReason::CloserToBid,
            EstimatedSide::EstimatedSell,
        ),
        (
            "1",
            vec![quote(2, "1", "2")],
            QuoteReason::AtOrBelowBid,
            EstimatedSide::EstimatedSell,
        ),
    ] {
        let report = verify(
            HistoricalOptionTicks {
                trades: vec![trade(0, price, "")],
                quotes,
                issues: vec![],
            },
            &[],
        );
        assert_eq!(report.trades[0].inference.reason, reason);
        assert_eq!(report.trades[0].inference.direction, direction);
    }
}

#[test]
fn quote_gap_blocks_estimate_even_with_fresh_prior_quote() {
    let report = verify(
        HistoricalOptionTicks {
            trades: vec![trade(0, "2", "")],
            quotes: vec![quote(2, "1.8", "2")],
            issues: vec![issue(
                HistoricalKind::BidAsk,
                HistoricalIssueReason::IncompletePage,
            )],
        },
        &[],
    );
    assert_eq!(report.counts.pending_intervals, 1);
    assert_eq!(report.counts.verified, 1);
    assert_eq!(report.counts.unknown, 1);
    assert_eq!(
        report.trades[0].inference.reason,
        QuoteReason::QuoteCoverageUnknown
    );
}

#[test]
fn unlinked_correction_and_cancellation_do_not_delete_matching_prints() {
    let report = verify(
        HistoricalOptionTicks {
            trades: vec![
                trade(0, "2", ""),
                trade(1, "2", ""),
                trade(2, "2", "SYNTH_CORRECTION"),
                trade(3, "2", "SYNTH_CANCEL"),
            ],
            quotes: vec![],
            issues: vec![],
        },
        &[],
    );
    assert_eq!(report.counts.verified, 2);
    assert_eq!(report.counts.unresolved_amendments, 2);
    assert_eq!(report.trades[2].status, TradeStatus::UnresolvedAmendment);
    assert_eq!(report.trades[3].status, TradeStatus::UnresolvedAmendment);
}

#[test]
fn explicit_revision_and_cancellation_are_auditable() {
    let original = trade(0, "2", "");
    let unaffected = trade(1, "2", "");
    let corrected = trade(2, "1", "SYNTH_CORRECTION");
    let cancelled = trade(3, "1", "SYNTH_CANCEL");
    let native_id: finance_market_protocol::NonEmptyText = "synthetic-original".parse().unwrap();
    let links = [
        TradeLink {
            occurrence: TradeOccurrence::from(&original),
            action: TradeAction::Original {
                native_id: native_id.clone(),
            },
        },
        TradeLink {
            occurrence: TradeOccurrence::from(&corrected),
            action: TradeAction::Correction {
                target_native_id: native_id.clone(),
                revision: 1.into(),
            },
        },
        TradeLink {
            occurrence: TradeOccurrence::from(&cancelled),
            action: TradeAction::Cancellation {
                target_native_id: native_id,
                revision: 2.into(),
            },
        },
    ];
    let quote = quote(2, "1", "2");
    let revised = verify(
        HistoricalOptionTicks {
            trades: vec![original.clone(), unaffected.clone(), corrected.clone()],
            quotes: vec![quote.clone()],
            issues: vec![],
        },
        &links[..2],
    );
    assert_eq!(revised.trades[0].status, TradeStatus::Superseded);
    assert_eq!(revised.trades[2].status, TradeStatus::Active);
    assert_eq!(
        revised.trades[2].inference.direction,
        EstimatedSide::EstimatedSell
    );
    assert_eq!(revised.trades[2].premium.as_ref().unwrap().as_str(), "50");

    let report = verify(
        HistoricalOptionTicks {
            trades: vec![original, unaffected, corrected, cancelled],
            quotes: vec![quote],
            issues: vec![],
        },
        &links,
    );
    assert_eq!(report.counts.verified, 1);
    assert_eq!(report.trades[1].status, TradeStatus::Active);
    assert_eq!(report.trades[2].status, TradeStatus::Cancelled);
    assert_eq!(report.trades[3].status, TradeStatus::AppliedCancellation);
    assert_eq!(report.trades[3].link.as_ref(), Some(&links[2]));
}

#[test]
fn expired_history_reports_unavailable_interval_without_synthetic_prints() {
    let report = verify(
        HistoricalOptionTicks {
            trades: vec![],
            quotes: vec![],
            issues: vec![issue(
                HistoricalKind::Trades,
                HistoricalIssueReason::ExpiredContract,
            )],
        },
        &[],
    );
    assert_eq!(report.counts.unavailable_intervals, 1);
    assert_eq!(report.counts.verified, 0);
    assert_eq!(report.counts.unknown, 0);
    assert_eq!(report.issues[0].observed_at, at(21));
}
