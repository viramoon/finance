use super::*;
use crate::market::{OptionContract, OptionRight};
use chrono::NaiveDate;
use std::{collections::VecDeque, sync::Mutex};

// All test rows are constructed from fixed synthetic constants. Repetition is
// deliberate: two trade records can agree on every broker field.
const START: i64 = 1_735_917_000;

fn at(offset: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(START + offset, 0).unwrap()
}

fn contract(expiry: NaiveDate) -> ConfirmedContract {
    ConfirmedContract {
        con_id: 91_002,
        symbol: "SYNTH".into(),
        primary_exchange: None,
        kind: ContractKind::Option(OptionContract {
            expiry,
            strike: "17".parse().unwrap(),
            right: OptionRight::Call,
            multiplier: "150".parse().unwrap(),
            underlying_con_id: 91_001,
        }),
        liquid_hours: Vec::new(),
    }
}

fn trade(offset: i64, quantity: &str) -> HistoryTrade {
    HistoryTrade {
        occurred_at: at(offset),
        price: "1.25".parse().unwrap(),
        quantity: quantity.parse().unwrap(),
        exchange: "SYNTHX".into(),
        special_conditions: String::new(),
        past_limit: false,
        unreported: false,
    }
}

fn quote(offset: i64) -> HistoryQuote {
    HistoryQuote {
        occurred_at: at(offset),
        bid: Some("1.20".parse().unwrap()),
        ask: Some("1.25".parse().unwrap()),
        bid_quantity: Some("2".parse().unwrap()),
        ask_quantity: Some("3".parse().unwrap()),
        bid_past_low: false,
        ask_past_high: false,
    }
}

fn page(rows: HistoricalRows, id: i32) -> HistoricalPage {
    HistoricalPage {
        rows,
        obtained_at: at(100),
        provider_request_id: Some(id),
    }
}

struct Replay {
    pages: Mutex<VecDeque<Result<HistoricalPage, HistoricalIssueReason>>>,
    requests: Mutex<Vec<HistoricalPageRequest>>,
}

impl Replay {
    fn new(pages: impl IntoIterator<Item = Result<HistoricalPage, HistoricalIssueReason>>) -> Self {
        Self {
            pages: Mutex::new(pages.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl HistoricalOptionSource for Replay {
    async fn option_tick_page(
        &self,
        _contract: &ConfirmedContract,
        request: HistoricalPageRequest,
    ) -> Result<HistoricalPage, HistoricalIssueReason> {
        self.requests.lock().unwrap().push(request);
        self.pages
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected wire read")
    }

    fn now(&self) -> DateTime<Utc> {
        at(100)
    }
}

#[tokio::test]
async fn completed_seconds_preserve_two_identical_trades_across_overlapped_pages() {
    let mut first = vec![trade(1, "1"); usize::from(PAGE_LIMIT) - 1];
    first.extend([trade(2, "2"), trade(2, "2")]);
    let replay = Replay::new([
        Ok(page(HistoricalRows::Trades(first), 19)),
        Ok(page(
            HistoricalRows::Trades(vec![
                trade(2, "2"),
                trade(2, "2.0"),
                trade(3, "3"),
                trade(7, "4"),
            ]),
            20,
        )),
        Ok(page(
            HistoricalRows::BidAsk(vec![quote(-1), quote(2), quote(7)]),
            21,
        )),
    ]);
    let window = HistoricalWindow::new(at(0), at(7), at(-2), 4).unwrap();
    let ticks = fetch_option_ticks(&replay, &contract(NaiveDate::MAX), window).await;
    assert_eq!(ticks.trades.len(), usize::from(PAGE_LIMIT) + 2);
    let same_second = &ticks.trades[usize::from(PAGE_LIMIT) - 1..usize::from(PAGE_LIMIT) + 1];
    assert_eq!(same_second[0].row, same_second[1].row);
    assert_eq!(
        (same_second[0].occurrence, same_second[1].occurrence),
        (1, 2)
    );
    assert_eq!(same_second[0].batch.provider_request_id, Some(19));
    assert_eq!(
        ticks.trades.last().unwrap().batch.provider_request_id,
        Some(20)
    );
    assert_eq!(ticks.trades.last().unwrap().row.occurred_at, at(3));
    assert_eq!(ticks.quotes.len(), 2);
    assert_eq!(ticks.quotes[0].row.occurred_at, at(-1));
    assert_eq!(ticks.quotes[0].batch.obtained_at, at(100));
    assert!(ticks.issues.is_empty());

    let requests = replay.requests.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .map(|request| request.start)
            .collect::<Vec<_>>(),
        [at(0), at(3), at(-2)]
    );
    assert_eq!(
        requests
            .iter()
            .map(|request| request.kind)
            .collect::<Vec<_>>(),
        [
            HistoricalKind::Trades,
            HistoricalKind::Trades,
            HistoricalKind::BidAsk
        ]
    );
    assert!(requests.iter().all(|request| request.limit == PAGE_LIMIT));
}

#[tokio::test]
async fn expired_contract_is_unrecoverable_without_history_requests() {
    let expired = option_market_date(at(100)).unwrap().pred_opt().unwrap();
    let replay = Replay::new([]);
    let window = HistoricalWindow::new(at(0), at(9), at(-1), 2).unwrap();
    let ticks = fetch_option_ticks(&replay, &contract(expired), window).await;
    assert!(ticks.trades.is_empty() && ticks.quotes.is_empty());
    assert!(replay.requests.lock().unwrap().is_empty());
    assert_eq!(
        ticks.issues,
        [
            HistoricalIssue {
                kind: HistoricalKind::Trades,
                start: at(0),
                end: at(9),
                observed_at: at(100),
                reason: HistoricalIssueReason::ExpiredContract
            },
            HistoricalIssue {
                kind: HistoricalKind::BidAsk,
                start: at(-1),
                end: at(9),
                observed_at: at(100),
                reason: HistoricalIssueReason::ExpiredContract
            },
        ]
    );
}

#[tokio::test]
async fn incomplete_or_limited_history_records_coverage_gaps_per_kind() {
    let replay = Replay::new([
        Ok(page(
            HistoricalRows::Trades(vec![trade(2, "1"); usize::from(PAGE_LIMIT)]),
            19,
        )),
        Err(HistoricalIssueReason::NoHistory),
    ]);
    let window = HistoricalWindow::new(at(0), at(9), at(-1), 1).unwrap();
    let result = fetch_option_ticks(&replay, &contract(NaiveDate::MAX), window).await;
    assert_eq!(result.trades.len(), usize::from(PAGE_LIMIT));
    assert_eq!(
        result.issues,
        [
            HistoricalIssue {
                kind: HistoricalKind::Trades,
                start: at(3),
                end: at(9),
                observed_at: at(100),
                reason: HistoricalIssueReason::PageLimit
            },
            HistoricalIssue {
                kind: HistoricalKind::BidAsk,
                start: at(-1),
                end: at(9),
                observed_at: at(100),
                reason: HistoricalIssueReason::NoHistory
            },
        ]
    );
}

#[tokio::test]
async fn conflicting_complete_second_overlap_yields_uncertainty() {
    let rows = vec![trade(2, "1"); usize::from(PAGE_LIMIT)];
    let replay = Replay::new([
        Ok(page(HistoricalRows::Trades(rows), 19)),
        Ok(page(
            HistoricalRows::Trades(vec![trade(2, "1"), trade(4, "2")]),
            20,
        )),
        Ok(page(HistoricalRows::BidAsk(Vec::new()), 21)),
    ]);
    let window = HistoricalWindow::new(at(0), at(9), at(-1), 2).unwrap();
    let result = fetch_option_ticks(&replay, &contract(NaiveDate::MAX), window).await;
    assert_eq!(result.trades.len(), usize::from(PAGE_LIMIT));
    assert_eq!(
        result.issues,
        [HistoricalIssue {
            kind: HistoricalKind::Trades,
            start: at(0),
            end: at(9),
            observed_at: at(100),
            reason: HistoricalIssueReason::InconsistentOverlap
        },]
    );
}

#[test]
fn validated_windows_are_second_aligned_and_single_market_date() {
    assert!(HistoricalWindow::new(at(0), at(9), at(-1), 1).is_ok());
    assert!(HistoricalWindow::new(at(0), at(9), at(-1), 65).is_err());
    assert!(HistoricalWindow::new(at(0), at(9), at(1), 1).is_err());
    assert!(HistoricalWindow::new(at(0), at(86_400), at(0), 1).is_err());
    assert!(HistoricalWindow::new(at(0) + TimeDelta::milliseconds(1), at(9), at(-1), 1).is_err());
}
