//! Deterministic fault sequences on the in-memory SDK transport.
use super::*;

async fn dispatcher_market(builder: ibapi::ClientBuilder) -> (MarketReplay, PaperMarket) {
    let (replay, client) = MarketReplay::open_with_builder(builder).unwrap();
    let notices = client.notice_stream().unwrap();
    let market = PaperMarket::new(Arc::new(PaperPeer::new(client, notices)), WAIT)
        .await
        .unwrap();
    (replay, market)
}

async fn confirm_id(replay: &MarketReplay, count: usize) -> i32 {
    replay.wait_for_writes(count).await;
    match replay.writes()[count - 1] {
        MarketWrite::Confirm(Some(id)) => id,
        ref other => panic!("expected a contract lookup, got {other:?}"),
    }
}

async fn answer_qqq(replay: &MarketReplay) -> i32 {
    let id = confirm_id(replay, 2).await;
    replay
        .equity_contract(id, 91001, "QQQ", "NASDAQ")
        .await
        .unwrap();
    replay.wait_for_writes(3).await;
    let id = match replay.writes()[2] {
        MarketWrite::Subscribe {
            request_id: Some(id),
            con_id: Some(91001),
            ..
        } => id,
        ref other => panic!("expected a QQQ line, got {other:?}"),
    };
    replay.market_data_type(id, 1).await.unwrap();
    replay.price(id, TickType::Bid, 10.25, None).await.unwrap();
    replay.price(id, TickType::Ask, 10.50, None).await.unwrap();
    id
}

#[tokio::test(start_paused = true)]
async fn successful_cancel_write_detects_socket_loss_before_the_session_poll() {
    use crate::entitlements::{Reason, Verdict};

    // The default SDK lets us keep a real cancel flush in progress while its
    // read side enters reconnect. The paper builder itself never reconnects.
    let (replay, market) = dispatcher_market(ibapi::Client::builder()).await;
    let lose_socket_during_flush = async {
        let id = answer_qqq(&replay).await;
        let release = replay.hold_next_write();
        replay.wait_for_writes(4).await;
        assert_eq!(replay.writes()[3], MarketWrite::Cancel(Some(id)));
        let now = tokio::time::Instant::now();
        replay.disconnect_and_stall_reconnect();
        replay.wait_for_stalled_handshake().await;
        assert!(!market.peer.client.is_connected());
        release.send(()).unwrap();
        now
    };
    let (report, flush_started) = timeout(Duration::from_secs(10), async {
        tokio::join!(
            probe_open_market(&market, ProbeOptions::default(), WAIT),
            lose_socket_during_flush
        )
    })
    .await
    .unwrap();
    assert!(flush_started.elapsed() < Duration::from_millis(100));
    assert_eq!(report.failure, Some(Reason::GatewayDisconnected));
    let first = &report.products[0].probes[0];
    assert!(first.contract_confirmed && first.bid_ask_valid);
    assert_eq!(first.market_data_type, Some(1));
    assert_eq!(
        (first.verdict, first.reason),
        (Verdict::Pending, Reason::GatewayDisconnected)
    );
    assert!(
        report.products[0].probes[1..]
            .iter()
            .all(|p| p.reason == Reason::SweepStopped)
    );
    assert_eq!(
        report.products[1].probes[0].reason,
        Reason::OptionNotSelected
    );
    assert_eq!(replay.writes().len(), 4);
    assert!(replay.is_closed() && !replay.handshake_is_pending());
}

#[tokio::test(start_paused = true)]
async fn global_and_request_scoped_disconnects_preserve_completed_rows() {
    use crate::entitlements::Reason;
    for (global, during_read, code) in [
        (true, false, 1100),
        (false, false, 1100),
        (false, false, 2103),
        (true, true, 2105),
        (false, true, 2103),
    ] {
        let (replay, market) = dispatcher_market(crate::IbTransport::paper_client_builder()).await;
        let interrupt_spy = async {
            replay.notice(-1, 2104).await.unwrap(); // A healthy farm notice during QQQ.
            answer_qqq(&replay).await;
            let mut id = confirm_id(&replay, 5).await;
            if during_read {
                replay
                    .equity_contract(id, 91002, "SPY", "ARCA")
                    .await
                    .unwrap();
                replay.wait_for_writes(6).await;
                id = match replay.writes()[5] {
                    MarketWrite::Subscribe {
                        request_id: Some(id),
                        ..
                    } => id,
                    ref other => panic!("expected an active SPY line, got {other:?}"),
                };
            }
            replay
                .notice(if global { -1 } else { id }, code)
                .await
                .unwrap();
        };
        let options = ProbeOptions {
            opra_con_id: Some(91003),
            ..ProbeOptions::default()
        };
        let (report, ()) = timeout(Duration::from_secs(10), async {
            tokio::join!(probe_open_market(&market, options, WAIT), interrupt_spy)
        })
        .await
        .unwrap();
        let probes = &report.products[0].probes;
        assert_eq!(report.failure, Some(Reason::GatewayDisconnected));
        assert!(probes[0].contract_confirmed && probes[0].bid_ask_valid);
        assert_eq!(
            (probes[0].market_data_type, probes[0].reason),
            (Some(1), Reason::MarketHoursUnknown)
        );
        assert_eq!(probes[1].contract_confirmed, during_read);
        assert_eq!(probes[1].reason, Reason::GatewayDisconnected);
        assert!(probes[2..].iter().all(|p| p.reason == Reason::SweepStopped));
        assert_eq!(report.products[1].probes[0].reason, Reason::SweepStopped);
        assert_eq!(replay.reconnect_attempts(), 0);
        assert!(replay.is_closed());
    }
}

#[tokio::test(start_paused = true)]
async fn write_errors_classify_the_operation_and_latch_uncertain_lines() {
    use std::io::ErrorKind;
    let query = ContractQuery::Equity {
        symbol: "SYNTH".into(),
        primary_exchange: "NASDAQ".into(),
    };
    for (kind, error) in [
        (ErrorKind::BrokenPipe, MarketError::Disconnected),
        (ErrorKind::NotConnected, MarketError::Disconnected),
        (ErrorKind::TimedOut, MarketError::Timeout),
        (ErrorKind::Other, MarketError::Transport),
    ] {
        let (replay, client) = MarketReplay::open().unwrap();
        let notices = client.notice_stream().unwrap();
        replay.fail_next_write_with(kind);
        assert!(
            matches!(PaperMarket::new(Arc::new(PaperPeer::new(client, notices)), WAIT).await, Err(e) if e == error)
        );
        assert!(replay.writes().is_empty());

        let (replay, market) = paper_market().await;
        let cached = request(&market, 91001, ContractKind::Equity).await;
        replay.fail_next_write_with(kind);
        assert_eq!(market.confirm(&query).await, Err(error));
        assert!(market.peer.client.is_connected());
        if error == MarketError::Disconnected {
            assert!(matches!(
                market.subscribe(&cached).await,
                Err(MarketError::Disconnected)
            ));
            assert_eq!(replay.writes().len(), 1);
        } else {
            market
                .subscribe(&cached)
                .await
                .unwrap()
                .cancel()
                .await
                .unwrap();
        }
        market.close().await;

        for during_cancel in [false, true] {
            let (replay, market) = paper_market().await;
            let cached = request(&market, 91001, ContractKind::Equity).await;
            let mut sibling = market.subscribe(&cached).await.unwrap();
            let mut line = if during_cancel {
                Some(market.subscribe(&cached).await.unwrap())
            } else {
                None
            };
            let before = replay.writes();
            replay.fail_next_write_with(kind);
            let result = match &mut line {
                Some(line) => line.cancel().await,
                None => market.subscribe(&cached).await.map(drop),
            };
            assert_eq!(result, Err(error));
            assert!(market.peer.client.is_connected());
            assert_eq!(sibling.next().await, Err(MarketError::Disconnected));
            assert_eq!(market.confirm(&query).await, Err(MarketError::Disconnected));
            assert!(matches!(
                market.subscribe(&cached).await,
                Err(MarketError::Disconnected)
            ));
            assert_eq!(replay.writes(), before);
            market.close().await;
        }
    }
}

#[tokio::test(start_paused = true)]
async fn interrupted_subscribe_and_cancel_wake_a_pending_sibling() {
    for during_cancel in [false, true] {
        for caller_deadline in [false, true] {
            let (replay, market) =
                dispatcher_market(crate::IbTransport::paper_client_builder()).await;
            let cached = request(&market, 91001, ContractKind::Equity).await;
            let mut sibling = market.subscribe(&cached).await.unwrap();
            let mut line = if during_cancel {
                Some(market.subscribe(&cached).await.unwrap())
            } else {
                None
            };
            let before = replay.writes().len();
            let release = replay.hold_next_write();
            let operation = async {
                match &mut line {
                    Some(line) => line.cancel().await,
                    None => market.subscribe(&cached).await.map(drop),
                }
            };
            let interrupt = async {
                let bound = if caller_deadline { WAIT / 10 } else { WAIT * 2 };
                match timeout(bound, operation).await {
                    Err(_) => assert!(caller_deadline),
                    Ok(result) => {
                        assert!(!caller_deadline);
                        assert_eq!(result, Err(MarketError::Timeout));
                    }
                }
            };
            let (read, ()) = timeout(WAIT * 3, async {
                tokio::join!(biased; sibling.next(), interrupt)
            })
            .await
            .unwrap();
            assert_eq!(read, Err(MarketError::Disconnected));
            assert!(
                release.send(()).is_err(),
                "a timed-out SDK write kept running"
            );
            assert!(matches!(
                market.subscribe(&cached).await,
                Err(MarketError::Disconnected)
            ));
            assert_eq!(replay.writes().len(), before + 1);
            assert!(market.peer.client.is_connected());
            timeout(WAIT, async {
                while replay.registered_requests().await != 1 {
                    sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .expect("only the retained sibling should own a request channel");
            market.close().await;
            assert!(replay.is_closed());
        }
    }
}

#[tokio::test]
async fn explicit_cancel_is_idempotent_and_dropping_an_active_line_ends_the_session() {
    let (replay, market) = paper_market().await;
    let cached = request(&market, 91001, ContractKind::Equity).await;
    let mut line = market.subscribe(&cached).await.unwrap();
    replay
        .price(line.request_id(), TickType::Bid, 10.25, Some("123"))
        .await
        .unwrap();
    assert!(matches!(
        next_event(&mut line).await,
        MarketEvent::Quote {
            field: QuoteField::BidPrice,
            ..
        }
    ));
    line.cancel().await.unwrap();
    assert_eq!(line.next().await, Ok(None)); // The buffered size cannot escape after cancel.
    line.cancel().await.unwrap();
    drop(line);
    assert_eq!(replay.writes().len(), 3);
    drop(market.subscribe(&cached).await.unwrap());
    assert_eq!(market.disconnected().await, MarketError::Disconnected);
    assert!(matches!(
        market.subscribe(&cached).await,
        Err(MarketError::Disconnected)
    ));
    market.close().await;
    assert!(replay.is_closed());
}

#[tokio::test(start_paused = true)]
async fn close_releases_transport_with_an_outstanding_write_and_retained_handles() {
    let (replay, market) = dispatcher_market(crate::IbTransport::paper_client_builder()).await;
    let cached = request(&market, 91001, ContractKind::Equity).await;
    let mut line = market.subscribe(&cached).await.unwrap();
    let release = replay.hold_next_write();
    // Keep the SDK handle alive and its write pending independently of finance's monitor.
    let sdk_line = line.subscription.as_ref().unwrap().clone();
    let mut cancel = std::pin::pin!(sdk_line.try_cancel());
    tokio::select! {
        biased;
        result = &mut cancel => panic!("write was not held: {result:?}"),
        _ = replay.wait_for_writes(3) => {}
    }
    // Close must finish even if this caller parks the future and never polls it.
    timeout(WAIT, market.close()).await.unwrap();
    assert!(matches!(cancel.await, Err(ibapi::Error::Shutdown)));
    assert!(release.send(()).is_err());
    assert!(replay.is_closed());
    assert_eq!(line.next().await, Err(MarketError::Disconnected));
    assert_eq!(line.cancel().await, Err(MarketError::Disconnected));
    assert_eq!(replay.writes().len(), 3);
}

#[tokio::test(start_paused = true)]
async fn probe_reports_the_phase_of_an_uncertain_subscription_write() {
    use crate::entitlements::Reason;
    use std::io::ErrorKind;
    for (kind, reason) in [
        (ErrorKind::Other, Reason::TransportFailure),
        (ErrorKind::TimedOut, Reason::SubscriptionTimeout),
        (ErrorKind::BrokenPipe, Reason::GatewayDisconnected),
    ] {
        let (replay, market) = dispatcher_market(crate::IbTransport::paper_client_builder()).await;
        let (report, ()) = timeout(WAIT * 2, async {
            tokio::join!(
                probe_open_market(&market, ProbeOptions::default(), WAIT),
                async {
                    let id = confirm_id(&replay, 2).await;
                    replay.fail_next_write_with(kind);
                    replay
                        .equity_contract(id, 91001, "QQQ", "NASDAQ")
                        .await
                        .unwrap();
                }
            )
        })
        .await
        .unwrap();
        let first = &report.products[0].probes[0];
        assert!(first.contract_confirmed);
        assert_eq!((report.failure, first.reason), (Some(reason), reason));
        assert!(
            report.products[0].probes[1..]
                .iter()
                .all(|p| p.reason == Reason::SweepStopped)
        );
        assert_eq!(replay.writes().len(), 2);
        assert!(replay.is_closed());
    }
}

#[tokio::test(start_paused = true)]
async fn explicit_permission_denial_survives_a_separate_cancel_failure() {
    use crate::entitlements::{Reason, Verdict};
    use std::io::ErrorKind;
    for (kind, reason) in [
        (ErrorKind::BrokenPipe, Reason::GatewayDisconnected),
        (ErrorKind::Other, Reason::CancelFailed),
        (ErrorKind::TimedOut, Reason::CancelFailed),
    ] {
        let (replay, market) = dispatcher_market(crate::IbTransport::paper_client_builder()).await;
        let (report, ()) = timeout(WAIT * 2, async {
            tokio::join!(
                probe_open_market(&market, ProbeOptions::default(), WAIT),
                async {
                    let id = answer_qqq(&replay).await;
                    replay.fail_next_write_with(kind);
                    replay.notice(id, 354).await.unwrap();
                }
            )
        })
        .await
        .unwrap();
        assert_eq!(report.failure, Some(reason));
        let equity = &report.products[0];
        assert_eq!(equity.verdict, Verdict::NotEntitled);
        assert_eq!(equity.probes[0].reason, Reason::NotEntitled);
        assert!(equity.probes[0].bid_ask_valid);
        assert!(
            equity.probes[1..]
                .iter()
                .all(|p| p.reason == Reason::SweepStopped)
        );
        assert_eq!(replay.writes().len(), 3);
        assert!(replay.is_closed());
    }
}

#[tokio::test(start_paused = true)]
async fn terminal_notices_end_idle_and_handshaking_connections_without_a_watcher() {
    let (replay, market) = dispatcher_market(crate::IbTransport::paper_client_builder()).await;
    let cached = request(&market, 91001, ContractKind::Equity).await;
    // This code arrives after its request is gone. No consumer is polling any market API.
    replay.notice(99999, 2103).await.unwrap();
    timeout(WAIT, async {
        while !replay.is_closed() {
            sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    assert!(matches!(
        market.subscribe(&cached).await,
        Err(MarketError::Disconnected)
    ));
    assert_eq!(replay.writes(), [MarketWrite::SelectType(Some(3))]);
    market.close().await;

    // Drive the same SDK startup handshake as an initial connect using replay's replacement dial.
    let builder = crate::IbTransport::paper_client_builder().auto_reconnect(true);
    let (replay, client) = MarketReplay::open_with_builder(builder).unwrap();
    replay.disconnect_with_handshake_notice("DU-SYNTH", 1100);
    timeout(WAIT, async {
        while !replay.is_closed() {
            sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    assert!(!client.is_connected());
    assert_eq!(replay.reconnect_attempts(), 1);
    assert_eq!(replay.writes(), [MarketWrite::StartApi(Some(91))]);
    client.disconnect().await;
}
