use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use chrono::TimeDelta;
use finance_ib::{
    historical::option_ticks::{
        HistoricalPage, HistoricalPageRequest, HistoricalRows, HistoryQuote, HistoryTrade,
    },
    market::{OptionContract, OptionRight, RequestKey, RequestParameters},
};
use tempfile::tempdir;

use super::*;
use crate::{
    line_budget::CoverageInterval,
    market_ingress::wal_root,
    market_wal::WalReader,
    option_flow::{
        IBKR,
        evidence::{EstimatedSide, QuoteReason, SuspectedScale, TradeStatus},
    },
};

fn at(second: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(1_760_000_000 + second, 0).unwrap()
}

fn instrument() -> Instrument {
    let sample: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../finance-market-protocol/fixtures/examples-v3.input.json"
    ))
    .unwrap();
    serde_json::from_value(sample["instruments"][0].clone()).unwrap()
}

fn contract() -> ConfirmedContract {
    ConfirmedContract {
        con_id: 42,
        symbol: "SYNTH".into(),
        primary_exchange: None,
        kind: ContractKind::Option(OptionContract {
            expiry: at(100).date_naive(),
            strike: "25".parse().unwrap(),
            right: OptionRight::Call,
            multiplier: "25".parse().unwrap(),
            underlying_con_id: 41,
        }),
        liquid_hours: vec![LiquidHours::Open {
            start: at(0),
            end: at(10),
        }],
    }
}

fn policy(record_local: bool) -> ProductPolicy {
    ProductPolicy {
        source: IBKR.parse().unwrap(),
        product: OPRA_HISTORY.parse().unwrap(),
        license: "synthetic".parse().unwrap(),
        display_live: true,
        record_local,
        derive_non_display: false,
        backup_personal_drive: false,
    }
}

#[derive(Default)]
struct Replay(AtomicUsize);

#[async_trait]
impl HistoricalOptionSource for Replay {
    fn now(&self) -> DateTime<Utc> {
        at(100)
    }

    async fn option_tick_page(
        &self,
        _: &ConfirmedContract,
        request: HistoricalPageRequest,
    ) -> Result<HistoricalPage, HistoricalIssueReason> {
        self.0.fetch_add(1, Ordering::Relaxed);
        let rows = match request.kind {
            HistoricalKind::Trades => {
                let trade = HistoryTrade {
                    occurred_at: at(3),
                    price: "1.25".parse().unwrap(),
                    quantity: "2".parse().unwrap(),
                    exchange: "XTEST".into(),
                    special_conditions: String::new(),
                    past_limit: false,
                    unreported: false,
                };
                HistoricalRows::Trades(vec![trade.clone(), trade])
            }
            HistoricalKind::BidAsk => HistoricalRows::BidAsk(vec![HistoryQuote {
                occurred_at: at(2),
                bid: Some("1.125".parse().unwrap()),
                ask: Some("1.25".parse().unwrap()),
                bid_quantity: Some("3".parse().unwrap()),
                ask_quantity: Some("6".parse().unwrap()),
                bid_past_low: false,
                ask_past_high: false,
            }]),
        };
        Ok(HistoricalPage {
            rows,
            obtained_at: self.now(),
            provider_request_id: Some(7),
        })
    }
}

struct ConflictingReplay;

#[async_trait]
impl HistoricalOptionSource for ConflictingReplay {
    fn now(&self) -> DateTime<Utc> {
        at(100)
    }

    async fn option_tick_page(
        &self,
        contract: &ConfirmedContract,
        request: HistoricalPageRequest,
    ) -> Result<HistoricalPage, HistoricalIssueReason> {
        let mut page = Replay::default()
            .option_tick_page(contract, request)
            .await?;
        if let HistoricalRows::Trades(trades) = &mut page.rows {
            // Page one is full. Page two returns the same complete second
            // with conflicting multiplicity before the next requested start.
            if request.start == at(2) {
                trades.resize(1_000, trades[0].clone());
            } else {
                assert_eq!(request.start, at(4));
                trades.truncate(1);
            }
        }
        Ok(page)
    }
}

fn window() -> HistoricalWindow {
    HistoricalWindow::new(at(2), at(8), at(0), 1).unwrap()
}

fn uncovered(start: DateTime<Utc>, end: DateTime<Utc>) -> CoverageReport {
    CoverageReport {
        key: RequestKey {
            con_id: 42,
            product: Product::Opra,
            parameters: RequestParameters::default(),
        },
        last_observed_at: None,
        covered: vec![],
        uncovered: vec![CoverageInterval { start, end }],
        unknown: vec![],
    }
}

#[test]
fn a_one_nanosecond_history_window_covers_only_its_start() {
    let start = at(2);
    let end = start + TimeDelta::nanoseconds(1);
    let (from_ts, to_ts) = quality_bounds(start, end).unwrap();
    assert_eq!(from_ts, to_ts);
    assert_eq!(from_ts.get(), nanos(start).unwrap());
    assert!(quality_bounds(start, start).is_err());
}

#[test]
fn zero_dte_precedes_later_and_unavailable_contracts() {
    let today = at(100).date_naive();
    let expiring = contract();
    let with_expiry = |con_id, expiry| {
        let mut other = expiring.clone();
        other.con_id = con_id;
        let ContractKind::Option(option) = &mut other.kind else {
            unreachable!()
        };
        option.expiry = expiry;
        other
    };
    let mut candidates = [
        with_expiry(43, today.succ_opt().unwrap()),
        with_expiry(41, today.pred_opt().unwrap()),
        expiring,
    ];
    crate::option_flow::prioritize_history(&mut candidates, today);
    assert_eq!(candidates.map(|contract| contract.con_id), [42, 43, 41]);
}

#[test]
fn recording_refuses_a_git_worktree_data_root() {
    let root = tempdir().unwrap();
    std::fs::create_dir(root.path().join(".git")).unwrap();
    std::fs::write(root.path().join(".git/HEAD"), "ref: refs/heads/synthetic\n").unwrap();
    let runtime = root.path().join("runtime");
    std::fs::create_dir(&runtime).unwrap();
    assert!(OptionHistoryIngress::open(&runtime, policy(true), FlowUse::PrivateDisplay).is_err());
    assert!(runtime.read_dir().unwrap().next().is_none());
}

#[tokio::test]
async fn missed_observation_recovers_two_prints_and_quote_via_f3() {
    let dir = tempdir().unwrap();
    let access = policy(true);
    let mut history =
        OptionHistoryIngress::open(dir.path(), access, FlowUse::PrivateDisplay).unwrap();
    let coverage = uncovered(at(0), at(5));
    let result = history
        .verify(
            &Replay::default(),
            &contract(),
            &instrument(),
            window(),
            &VerificationPolicy::new(TimeDelta::seconds(2)),
            Some(coverage.clone()),
        )
        .await
        .unwrap();
    assert_eq!(result.observation_coverage, Some(coverage));
    assert_eq!(result.evidence.verified_at, at(100));
    assert_eq!(result.evidence.counts.verified, 2);
    assert_eq!(result.evidence.counts.unknown, 0);
    assert!(result.evidence.trades.iter().all(|trade| {
        trade.inference.direction == EstimatedSide::EstimatedBuy
            && trade.premium.as_ref().unwrap().value() == "62.50".parse().unwrap()
    }));
    assert_eq!(result.durable_through.unwrap().get(), 5);
    let reader = WalReader::open(&wal_root(dir.path()), IBKR, OPRA_HISTORY).unwrap();
    let records = reader.read_after(0, 32).unwrap();
    let trades: Vec<_> = records
        .iter()
        .filter_map(|record| match &record.event.message {
            Message::OptionHistoryTrade(trade) => Some((record, trade)),
            _ => None,
        })
        .collect();
    assert_eq!(trades[0].0.event.exchange_ts, trades[1].0.event.exchange_ts);
    assert_eq!(trades[0].1.batch.occurrence.get(), 1);
    assert_eq!(trades[1].1.batch.occurrence.get(), 2);
    assert_eq!(trades[0].1.contract_multiplier.as_str(), "25");
    assert_eq!(trades[0].1.special_conditions, "");
    assert_eq!(
        trades[0].1.condition_mapping_version.as_str(),
        "ibkr-opaque-v1"
    );
    assert!(matches!(
        &records[4].event.message,
        Message::OptionHistoryQuote(_)
    ));
    let quality = history.ingress().quality().unwrap();
    assert_eq!(quality.len(), 1);
    assert_eq!(quality[0].from_ts.get(), nanos(at(2)).unwrap());
    assert_eq!(quality[0].to_ts.get(), nanos(at(5)).unwrap() - 1);
    assert_eq!(quality[0].count.unwrap().get(), 2);
    assert_eq!(quality[0].outcome, Some(RecoveryOutcome::TickBackfilled));

    // A restarted writer advances the stream epoch while retaining the prior receipts.
    drop(history);
    let mut restarted =
        OptionHistoryIngress::open(dir.path(), policy(true), FlowUse::PrivateDisplay).unwrap();
    restarted
        .verify(
            &Replay::default(),
            &contract(),
            &instrument(),
            window(),
            &VerificationPolicy::new(TimeDelta::seconds(2)),
            None,
        )
        .await
        .unwrap();
    let restarted = WalReader::open(&wal_root(dir.path()), IBKR, OPRA_HISTORY).unwrap();
    let after_restart = restarted.read_after(5, 32).unwrap();
    after_restart[0]
        .event
        .validate_successor(&trades[1].0.event)
        .unwrap();
    assert_eq!(after_restart[0].event.epoch.get(), 2);
}

#[tokio::test]
async fn conflicting_pages_keep_raw_trades_but_do_not_verify_or_backfill_them() {
    let dir = tempdir().unwrap();
    let mut history =
        OptionHistoryIngress::open(dir.path(), policy(true), FlowUse::PrivateDisplay).unwrap();
    let request = HistoricalWindow::new(at(2), at(8), at(0), 2).unwrap();
    let verification = VerificationPolicy::new(TimeDelta::seconds(2))
        .with_size_thresholds(Some("1".parse().unwrap()), Some("1".parse().unwrap()));
    let result = history
        .verify(
            &ConflictingReplay,
            &contract(),
            &instrument(),
            request,
            &verification,
            Some(uncovered(at(2), at(8))),
        )
        .await
        .unwrap();

    assert_eq!(result.evidence.trades.len(), 1_000);
    assert_eq!(result.evidence.counts.verified, 0);
    assert_eq!(result.evidence.counts.unknown, 0);
    assert_eq!(result.evidence.counts.pending_trade_rows, 1_000);
    assert_eq!(result.evidence.counts.pending_intervals, 1);
    assert!(result.evidence.trades.iter().all(|trade| {
        trade.status == TradeStatus::PendingVerification
            && trade.inference.reason == QuoteReason::TradeOverlapConflict
            && trade.inference.direction == EstimatedSide::Unknown
            && trade.suspected_scale == SuspectedScale::default()
            && trade.premium.is_none()
    }));
    let [issue] = result.evidence.issues.as_slice() else {
        panic!("expected one TRADES conflict");
    };
    assert_eq!(issue.reason, HistoricalIssueReason::InconsistentOverlap);

    let quality = history.ingress().quality().unwrap();
    assert_eq!(quality.len(), 1);
    assert_eq!(quality[0].from_ts.get(), nanos(request.start()).unwrap());
    assert_eq!(quality[0].to_ts.get(), nanos(request.end()).unwrap() - 1);
    assert_eq!(quality[0].outcome, None);
    let reader = WalReader::open(&wal_root(dir.path()), IBKR, OPRA_HISTORY).unwrap();
    let records = reader.read_after(0, 1_003).unwrap();
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record.event.message, Message::OptionHistoryTrade(_)))
            .count(),
        1_000
    );
}

#[tokio::test]
async fn expired_and_display_only_history_do_not_create_wal_or_call_expired_source() {
    let dir = tempdir().unwrap();
    let mut history =
        OptionHistoryIngress::open(dir.path(), policy(false), FlowUse::PrivateDisplay).unwrap();
    let mut expired = contract();
    let ContractKind::Option(option) = &mut expired.kind else {
        unreachable!()
    };
    option.expiry = option.expiry.pred_opt().unwrap();
    let source = Replay::default();
    let result = history
        .verify(
            &source,
            &expired,
            &instrument(),
            window(),
            &VerificationPolicy::new(TimeDelta::seconds(2)),
            None,
        )
        .await
        .unwrap();
    assert_eq!(source.0.load(Ordering::Relaxed), 0);
    assert_eq!(result.evidence.counts.verified, 0);
    assert_eq!(result.evidence.counts.unavailable_intervals, 2);
    let quality = history.ingress().quality().unwrap();
    assert_eq!(quality.len(), 2);
    assert_eq!(quality[0].channel, channel(HistoricalKind::Trades));
    assert_eq!(quality[0].from_ts.get(), nanos(window().start()).unwrap());
    assert_eq!(quality[1].channel, channel(HistoricalKind::BidAsk));
    assert_eq!(
        quality[1].from_ts.get(),
        nanos(window().quote_start()).unwrap()
    );
    for report in &quality {
        assert_eq!(report.outcome, Some(RecoveryOutcome::HistoryUnrecoverable));
        assert_eq!(report.to_ts.get(), nanos(window().end()).unwrap() - 1);
    }

    let result = history
        .verify(
            &source,
            &contract(),
            &instrument(),
            window(),
            &VerificationPolicy::new(TimeDelta::seconds(2)),
            None,
        )
        .await
        .unwrap();
    assert_eq!(result.evidence.counts.verified, 2);
    assert!(result.durable_through.is_none());
    assert_eq!(history.ingress().display_events().count(), 5);
    assert!(dir.path().read_dir().unwrap().next().is_none());
    assert!(
        OptionHistoryIngress::open(dir.path(), policy(true), FlowUse::NonDisplayDerivation)
            .is_err()
    );
}
