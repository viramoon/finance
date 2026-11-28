use std::{collections::HashMap, sync::Mutex, time::Duration};

use async_trait::async_trait;
use chrono::{Duration as ChronoDuration, Utc};
use tokio::sync::{mpsc, oneshot, watch};

use crate::market::{ConfirmedContract, ContractKind};

use super::*;

const ALLOWED: DepthPermissions = DepthPermissions {
    equity_l1_confirmed: true,
    totalview_confirmed: true,
    eds_confirmed: true,
};

const NASDAQ_WATCHLIST: [(&str, DepthSymbol); 8] = [
    ("QQQ", DepthSymbol::Qqq),
    ("AAPL", DepthSymbol::Aapl),
    ("MSFT", DepthSymbol::Msft),
    ("NVDA", DepthSymbol::Nvda),
    ("AMZN", DepthSymbol::Amzn),
    ("GOOGL", DepthSymbol::Googl),
    ("META", DepthSymbol::Meta),
    ("TSLA", DepthSymbol::Tsla),
];

type Callback = Result<Option<DepthUpdate>, DepthError>;

#[derive(Clone, Debug, Eq, PartialEq)]
enum Trace {
    Confirm(DepthSymbol),
    Open(DepthSymbol, i32),
    Cancel(DepthSymbol, i32),
    Publish(DepthSymbol, DepthState, Option<i32>, usize),
}

#[derive(Default)]
struct Replay {
    trace: Mutex<Vec<Trace>>,
    data: Mutex<ReplayData>,
}

#[derive(Default)]
struct ReplayData {
    lines: HashMap<DepthSymbol, mpsc::UnboundedSender<Callback>>,
    next_id: i32,
    peak: usize,
    fail_cancel: Option<DepthSymbol>,
    fail_open: Option<(DepthSymbol, DepthError)>,
    cancel_gate: Option<(DepthSymbol, oneshot::Receiver<()>)>,
}

struct ReplaySubscription<'a> {
    source: &'a Replay,
    symbol: DepthSymbol,
    id: i32,
    callbacks: mpsc::UnboundedReceiver<Callback>,
}

impl Replay {
    fn record(&self, trace: Trace) {
        self.trace.lock().unwrap().push(trace);
    }

    fn inject(&self, symbol: DepthSymbol, callback: Callback) {
        self.data.lock().unwrap().lines[&symbol]
            .send(callback)
            .unwrap();
    }
}

#[async_trait]
impl<'a> DepthSource for &'a Replay {
    type Subscription = ReplaySubscription<'a>;

    async fn confirm_depth(&self, symbol: DepthSymbol) -> Result<ConfirmedContract, DepthError> {
        self.record(Trace::Confirm(symbol));
        let (index, (ticker, _)) = NASDAQ_WATCHLIST
            .iter()
            .enumerate()
            .find(|(_, (_, expected))| *expected == symbol)
            .expect("symbol in the synthetic Nasdaq watchlist");
        Ok(ConfirmedContract {
            con_id: 1000 + index as i32,
            symbol: (*ticker).into(),
            primary_exchange: Some("NASDAQ".into()),
            kind: ContractKind::Equity,
            liquid_hours: Vec::new(),
        })
    }

    async fn subscribe_depth(
        &self,
        request: &DepthRequest,
    ) -> Result<Self::Subscription, DepthError> {
        let (_, symbol) = NASDAQ_WATCHLIST
            .into_iter()
            .find(|(ticker, _)| *ticker == request.contract().symbol)
            .expect("requested ticker in the synthetic Nasdaq watchlist");
        let (sender, callbacks) = mpsc::unbounded_channel();
        let mut data = self.data.lock().unwrap();
        if let Some((failed, error)) = data.fail_open
            && failed == symbol
        {
            return Err(error);
        }
        assert!(data.lines.insert(symbol, sender).is_none());
        data.peak = data.peak.max(data.lines.len());
        data.next_id += 1;
        let id = data.next_id;
        self.record(Trace::Open(symbol, id));
        Ok(ReplaySubscription {
            source: self,
            symbol,
            id,
            callbacks,
        })
    }
}

#[async_trait]
impl DepthSubscription for ReplaySubscription<'_> {
    fn request_id(&self) -> i32 {
        self.id
    }

    async fn next(&mut self) -> Callback {
        self.callbacks.recv().await.unwrap_or(Ok(None))
    }

    async fn cancel(&mut self) -> Result<(), DepthError> {
        self.source.record(Trace::Cancel(self.symbol, self.id));
        let gate = {
            let mut data = self.source.data.lock().unwrap();
            if data.fail_cancel == Some(self.symbol) {
                return Err(DepthError::Transport);
            }
            assert!(data.lines.remove(&self.symbol).is_some());
            if data
                .cancel_gate
                .as_ref()
                .is_some_and(|(symbol, _)| *symbol == self.symbol)
            {
                data.cancel_gate.take().map(|(_, gate)| gate)
            } else {
                None
            }
        };
        if let Some(gate) = gate {
            gate.await.unwrap();
        }
        Ok(())
    }
}

fn row(
    side: DepthSide,
    operation: DepthOperation,
    position: u32,
    price: &str,
    size: &str,
) -> Callback {
    Ok(Some(DepthUpdate::Row {
        side,
        operation,
        position,
        level: (operation != DepthOperation::Delete).then(|| DepthLevel {
            price: price.parse().unwrap(),
            size: size.parse().unwrap(),
        }),
    }))
}

async fn frame(
    frames: &mut mpsc::UnboundedReceiver<DepthFrame>,
    symbol: DepthSymbol,
    predicate: impl Fn(&DepthFrame) -> bool,
) -> DepthFrame {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let frame = frames.recv().await.expect("depth publisher ended");
            if frame.symbol == symbol && predicate(&frame) {
                return frame;
            }
        }
    })
    .await
    .expect("expected display frame")
}

async fn opened(frames: &mut mpsc::UnboundedReceiver<DepthFrame>, symbol: DepthSymbol) -> i32 {
    frame(frames, symbol, |frame| {
        frame.state == DepthState::Awaiting && frame.request_id.is_some()
    })
    .await
    .request_id
    .unwrap()
}

async fn run_replay(
    source: &Replay,
    focus: watch::Receiver<DepthFocus>,
    permissions: DepthPermissions,
    rows: u32,
    frames: mpsc::UnboundedSender<DepthFrame>,
) -> Result<(), DepthError> {
    run_depth(&source, focus, permissions, rows, |frame| {
        source.record(Trace::Publish(
            frame.symbol,
            frame.state,
            frame.request_id,
            frame.returned_rows(),
        ));
        frames.send(frame).unwrap();
    })
    .await
}

#[tokio::test]
async fn every_nasdaq_symbol_can_follow_with_two_fixed_slots_and_spy_closes_the_follow_slot() {
    assert_eq!(DepthSymbol::ALL.len(), NASDAQ_WATCHLIST.len());
    let symbols = NASDAQ_WATCHLIST.map(|(ticker, expected)| {
        assert!(DepthSymbol::ALL.contains(&expected), "missing {ticker}");
        assert_eq!(expected.as_str(), ticker);
        let parsed = ticker.parse::<DepthSymbol>().unwrap();
        assert_eq!(parsed, expected, "incorrect mapping for {ticker}");
        parsed
    });
    let replay = Replay::default();
    let defaults = DepthFocus::default();
    let (focus, receiver) = watch::channel(defaults);
    let (sender, mut frames) = mpsc::unbounded_channel();

    let (result, ()) = tokio::join!(run_replay(&replay, receiver, ALLOWED, 5, sender), async {
        let qqq = opened(&mut frames, symbols[0]).await;
        let aapl = opened(&mut frames, symbols[1]).await;
        assert_eq!(
            "SPY".parse::<DepthSymbol>(),
            Err(DepthError::UnsupportedSymbol)
        );
        assert_eq!(
            DepthFocus::new(&symbols[..3], None),
            Err(DepthError::InvalidFocus)
        );
        assert_eq!(
            DepthFocus::new(&[DepthSymbol::Qqq; 2], None),
            Err(DepthError::InvalidFocus)
        );

        for symbol in symbols.into_iter().skip(2) {
            focus.send_replace(defaults.with_followed(Some(symbol)));
            opened(&mut frames, symbol).await;
            assert_eq!(
                replay.data.lock().unwrap().lines.len(),
                MAX_DEPTH_SUBSCRIPTIONS
            );
        }

        // Each pinned page reuses its existing line and releases the follow slot.
        for (symbol, request_id) in [(symbols[0], qqq), (symbols[1], aapl)] {
            focus.send_replace(defaults.with_followed(Some(symbol)));
            frame(&mut frames, DepthSymbol::Tsla, |frame| {
                frame.state == DepthState::Inactive
            })
            .await;
            replay.inject(
                symbol,
                row(DepthSide::Bid, DepthOperation::Insert, 0, "12.01", "1"),
            );
            assert_eq!(
                frame(&mut frames, symbol, |frame| frame.state
                    == DepthState::Receiving)
                .await
                .request_id,
                Some(request_id)
            );
            focus.send_replace(defaults.with_followed(Some(DepthSymbol::Tsla)));
            opened(&mut frames, DepthSymbol::Tsla).await;
        }
        focus.send_replace(defaults.with_followed("SPY".parse().ok()));
        frame(&mut frames, DepthSymbol::Tsla, |frame| {
            frame.state == DepthState::Inactive
        })
        .await;
        drop(focus);
    });

    assert_eq!(result, Ok(()));
    let data = replay.data.lock().unwrap();
    assert_eq!(data.peak, 3);
    assert!(data.lines.is_empty());
    let trace = replay.trace.lock().unwrap();
    for symbol in symbols {
        let count = trace
            .iter()
            .filter(|event| matches!(event, Trace::Open(open, _) if *open == symbol))
            .count();
        assert_eq!(count, if symbol == DepthSymbol::Tsla { 3 } else { 1 });
    }
    for pair in symbols[2..].windows(2) {
        let cancel_previous = trace
            .iter()
            .position(|event| matches!(event, Trace::Cancel(symbol, _) if *symbol == pair[0]))
            .unwrap();
        let open_next = trace
            .iter()
            .position(|event| matches!(event, Trace::Open(symbol, _) if *symbol == pair[1]))
            .unwrap();
        assert!(cancel_previous < open_next);
    }
}

#[tokio::test]
async fn positional_operations_preserve_decimal_rows_and_label_actual_nasdaq_depth() {
    let replay = Replay::default();
    let symbol = DepthSymbol::Aapl;
    let (focus, receiver) = watch::channel(DepthFocus::new(&[symbol], None).unwrap());
    let (sender, mut frames) = mpsc::unbounded_channel();

    let (result, ()) = tokio::join!(run_replay(&replay, receiver, ALLOWED, 2, sender), async {
        opened(&mut frames, symbol).await;
        let callbacks = [
            row(
                DepthSide::Bid,
                DepthOperation::Insert,
                0,
                "12.010",
                "1.0001",
            ),
            row(
                DepthSide::Bid,
                DepthOperation::Insert,
                1,
                "12.010",
                "2.0001",
            ),
            row(
                DepthSide::Ask,
                DepthOperation::Insert,
                0,
                "12.030",
                "9007199254740993.01",
            ),
        ];
        for callback in callbacks {
            replay.inject(symbol, callback);
        }
        let book = frame(&mut frames, symbol, |frame| frame.asks.len() == 1).await;
        assert_eq!(book.venue(), "NASDAQ");
        assert_eq!(book.returned_rows(), 2);
        assert_eq!(book.bids[0].price.as_str(), "12.010");
        assert_eq!(book.bids[1].price.as_str(), "12.010");
        assert_eq!(book.asks[0].size.as_str(), "9007199254740993.01");
        assert_eq!(book.market_data_type(), None);
        let at = book.received_at.unwrap();
        assert!(!book.is_stale(at, Duration::from_secs(2)));
        assert!(book.is_stale(at + ChronoDuration::seconds(2), Duration::from_secs(2)));
        assert!(at <= Utc::now());

        replay.inject(
            symbol,
            row(DepthSide::Bid, DepthOperation::Insert, 0, "12.020", "3"),
        );
        let book = frame(&mut frames, symbol, |frame| {
            frame.state == DepthState::Receiving
        })
        .await;
        assert_eq!(book.bids[0].size.as_str(), "3");
        assert_eq!(book.bids[1].size.as_str(), "1.0001");

        replay.inject(
            symbol,
            row(DepthSide::Bid, DepthOperation::Update, 1, "12.010", "4.01"),
        );
        let book = frame(&mut frames, symbol, |frame| {
            frame.state == DepthState::Receiving
        })
        .await;
        assert_eq!(book.bids[1].size.as_str(), "4.01");

        replay.inject(
            symbol,
            row(DepthSide::Bid, DepthOperation::Delete, 0, "", ""),
        );
        let book = frame(&mut frames, symbol, |frame| {
            frame.state == DepthState::Receiving
        })
        .await;
        assert_eq!(book.bids[0].size.as_str(), "4.01");
        assert_eq!(book.returned_rows(), 1);
        assert_eq!(book.requested_rows, 2);
        drop(focus);
    });
    assert_eq!(result, Ok(()));
}

#[tokio::test]
async fn reset_and_invalid_positions_clear_before_reopening_and_repeated_empty_reset_stops() {
    let replay = Replay::default();
    let symbol = DepthSymbol::Aapl;
    let (focus, receiver) = watch::channel(DepthFocus::new(&[symbol], None).unwrap());
    let (sender, mut frames) = mpsc::unbounded_channel();

    let (result, ()) = tokio::join!(run_replay(&replay, receiver, ALLOWED, 5, sender), async {
        let initial = opened(&mut frames, symbol).await;
        replay.inject(
            symbol,
            row(DepthSide::Ask, DepthOperation::Insert, 0, "12.010", "1"),
        );
        let previous = frame(&mut frames, symbol, |frame| {
            frame.state == DepthState::Receiving
        })
        .await;
        assert_eq!(previous.returned_rows(), 1);
        replay.inject(symbol, Ok(Some(DepthUpdate::Reset)));
        let empty = frame(&mut frames, symbol, |frame| {
            frame.state == DepthState::Reset
        })
        .await;
        assert!(empty.bids.is_empty() && empty.asks.is_empty());
        assert_eq!(empty.request_id, Some(initial));
        assert!(empty.received_at.is_none());
        let replacement = opened(&mut frames, symbol).await;
        assert_ne!(initial, replacement);

        replay.inject(
            symbol,
            row(DepthSide::Bid, DepthOperation::Insert, 0, "11.990", "2"),
        );
        let current = frame(&mut frames, symbol, |frame| {
            frame.state == DepthState::Receiving
        })
        .await;
        assert!(current.asks.is_empty());
        assert_eq!(current.request_id, Some(replacement));
        replay.inject(
            symbol,
            row(DepthSide::Bid, DepthOperation::Update, 3, "12.010", "1"),
        );
        let empty = frame(&mut frames, symbol, |frame| {
            frame.state == DepthState::Reset
        })
        .await;
        assert_eq!(empty.returned_rows(), 0);
        let recovering = opened(&mut frames, symbol).await;
        replay.inject(symbol, Err(DepthError::InvalidCallback));
        let quarantined = frame(&mut frames, symbol, |frame| {
            frame.state == DepthState::Failed(DepthError::InvalidCallback)
        })
        .await;
        assert_eq!(quarantined.returned_rows(), 0);
        assert!(replay.data.lock().unwrap().lines.is_empty());

        let trace = replay.trace.lock().unwrap();
        for (old, new) in [(initial, replacement), (replacement, recovering)] {
            assert!(trace.windows(3).any(|window| window
                == [
                    Trace::Publish(symbol, DepthState::Reset, Some(old), 0),
                    Trace::Cancel(symbol, old),
                    Trace::Open(symbol, new),
                ]));
        }
        assert_eq!(
            trace
                .iter()
                .filter(|entry| matches!(entry, Trace::Open(..)))
                .count(),
            3
        );
        drop(focus);
    });
    assert_eq!(result, Ok(()));
}

#[tokio::test]
async fn missing_each_permission_claim_prevents_even_contract_confirmation() {
    for permissions in [
        DepthPermissions {
            equity_l1_confirmed: false,
            ..ALLOWED
        },
        DepthPermissions {
            totalview_confirmed: false,
            ..ALLOWED
        },
        DepthPermissions {
            eds_confirmed: false,
            ..ALLOWED
        },
    ] {
        let replay = Replay::default();
        let symbol = DepthSymbol::Aapl;
        let (focus, receiver) = watch::channel(DepthFocus::new(&[symbol], None).unwrap());
        let (sender, mut frames) = mpsc::unbounded_channel();
        let (result, ()) = tokio::join!(
            run_replay(&replay, receiver, permissions, 5, sender),
            async {
                let empty = frame(&mut frames, symbol, |_| true).await;
                assert_eq!(empty.permissions, permissions);
                assert_eq!(empty.state, DepthState::PermissionUnconfirmed);
                assert_eq!(empty.returned_rows(), 0);
                drop(focus);
            }
        );
        assert_eq!(result, Ok(()));
        let trace = replay.trace.lock().unwrap();
        assert!(
            trace
                .iter()
                .all(|event| matches!(event, Trace::Publish(..)))
        );
    }
}

#[tokio::test]
async fn disconnect_clears_every_book_and_leaves_reconnect_to_the_session_owner() {
    let replay = Replay::default();
    let defaults = DepthFocus::default();
    let (_focus, receiver) = watch::channel(defaults);
    let (sender, mut frames) = mpsc::unbounded_channel();

    let (result, ()) = tokio::join!(run_replay(&replay, receiver, ALLOWED, 5, sender), async {
        opened(&mut frames, DepthSymbol::Qqq).await;
        opened(&mut frames, DepthSymbol::Aapl).await;
        for symbol in [DepthSymbol::Qqq, DepthSymbol::Aapl] {
            replay.inject(
                symbol,
                row(DepthSide::Bid, DepthOperation::Insert, 0, "12.01", "1"),
            );
            frame(&mut frames, symbol, |frame| {
                frame.state == DepthState::Receiving
            })
            .await;
        }
        replay.inject(DepthSymbol::Aapl, Err(DepthError::Disconnected));
        for symbol in [DepthSymbol::Qqq, DepthSymbol::Aapl] {
            let empty = frame(&mut frames, symbol, |frame| {
                frame.state == DepthState::Failed(DepthError::Disconnected)
            })
            .await;
            assert_eq!(empty.returned_rows(), 0);
        }
    });
    assert_eq!(result, Err(DepthError::Disconnected));
    assert!(replay.data.lock().unwrap().lines.is_empty());
}

#[tokio::test]
async fn failed_cancel_stops_focus_replacement_before_another_line_opens() {
    let replay = Replay::default();
    let symbol = DepthSymbol::Nvda;
    let defaults = DepthFocus::default();
    let (focus, receiver) = watch::channel(defaults.with_followed(Some(symbol)));
    let (sender, mut frames) = mpsc::unbounded_channel();
    let (result, ()) = tokio::join!(run_replay(&replay, receiver, ALLOWED, 5, sender), async {
        opened(&mut frames, symbol).await;
        replay.data.lock().unwrap().fail_cancel = Some(symbol);
        focus.send_replace(defaults.with_followed(Some(DepthSymbol::Meta)));
        let empty = frame(&mut frames, symbol, |frame| {
            frame.state == DepthState::Inactive
        })
        .await;
        assert_eq!(empty.returned_rows(), 0);
    });
    assert_eq!(result, Err(DepthError::CancelFailed));
    let trace = replay.trace.lock().unwrap();
    assert!(
        !trace
            .iter()
            .any(|event| matches!(event, Trace::Confirm(DepthSymbol::Meta)))
    );
}

#[tokio::test]
async fn uncertain_subscribe_failure_stops_the_session_before_another_line_opens() {
    for error in [DepthError::Timeout, DepthError::Transport] {
        let replay = Replay::default();
        replay.data.lock().unwrap().fail_open = Some((DepthSymbol::Qqq, error));
        let (_focus, receiver) = watch::channel(DepthFocus::default());
        let (sender, mut frames) = mpsc::unbounded_channel();
        assert_eq!(
            run_replay(&replay, receiver, ALLOWED, 5, sender).await,
            Err(error)
        );
        let failed = frame(&mut frames, DepthSymbol::Qqq, |frame| {
            frame.state == DepthState::Failed(error)
        })
        .await;
        assert_eq!(failed.returned_rows(), 0);
        let trace = replay.trace.lock().unwrap();
        assert!(
            !trace
                .iter()
                .any(|event| matches!(event, Trace::Confirm(DepthSymbol::Aapl)))
        );
    }
}

#[tokio::test]
async fn superseded_focus_waits_for_in_flight_cancel_and_only_opens_the_latest_symbol() {
    let replay = Replay::default();
    let defaults = DepthFocus::default();
    let original = DepthSymbol::Nvda;
    let (focus, receiver) = watch::channel(defaults.with_followed(Some(original)));
    let (sender, mut frames) = mpsc::unbounded_channel();
    let (result, ()) = tokio::join!(run_replay(&replay, receiver, ALLOWED, 5, sender), async {
        let old_id = opened(&mut frames, original).await;
        let (finish_cancel, gate) = oneshot::channel();
        replay.data.lock().unwrap().cancel_gate = Some((original, gate));
        focus.send_replace(defaults.with_followed(Some(DepthSymbol::Tsla)));
        frame(&mut frames, original, |frame| {
            frame.state == DepthState::Inactive
        })
        .await;
        assert!(
            replay
                .trace
                .lock()
                .unwrap()
                .contains(&Trace::Cancel(original, old_id))
        );

        focus.send_replace(defaults.with_followed(Some(DepthSymbol::Meta)));
        tokio::task::yield_now().await;
        assert!(
            !replay
                .trace
                .lock()
                .unwrap()
                .contains(&Trace::Confirm(DepthSymbol::Meta))
        );
        finish_cancel
            .send(())
            .expect("checked cancellation must finish");
        opened(&mut frames, DepthSymbol::Meta).await;
        drop(focus);
    });
    assert_eq!(result, Ok(()));
    let trace = replay.trace.lock().unwrap();
    assert!(!trace.iter().any(|event| matches!(
        event,
        Trace::Confirm(DepthSymbol::Tsla) | Trace::Open(DepthSymbol::Tsla, _)
    )));
}

#[tokio::test]
async fn focus_switch_during_a_reset_cancel_does_not_reopen_the_old_symbol() {
    let replay = Replay::default();
    let symbol = DepthSymbol::Nvda;
    let defaults = DepthFocus::default();
    let (focus, receiver) = watch::channel(defaults.with_followed(Some(symbol)));
    let (sender, mut frames) = mpsc::unbounded_channel();
    let (result, ()) = tokio::join!(run_replay(&replay, receiver, ALLOWED, 5, sender), async {
        opened(&mut frames, symbol).await;
        let (cancel, cancelled) = oneshot::channel();
        replay.data.lock().unwrap().cancel_gate = Some((symbol, cancelled));
        replay.inject(symbol, Ok(Some(DepthUpdate::Reset)));
        frame(&mut frames, symbol, |frame| {
            frame.state == DepthState::Reset
        })
        .await;
        let replacement = DepthSymbol::Meta;
        focus.send_replace(defaults.with_followed(Some(replacement)));
        cancel.send(()).unwrap();
        opened(&mut frames, replacement).await;
        drop(focus);
    });
    assert_eq!(result, Ok(()));
    let trace = replay.trace.lock().unwrap();
    assert_eq!(
        trace
            .iter()
            .filter(|event| matches!(event, Trace::Open(open, _) if *open == symbol))
            .count(),
        1
    );
}
