use super::*;
use crate::entitlements::{ProbeOptions, probe_open_market};
use crate::market::RequestParameters;
use ibapi::replay::{MarketReplay, MarketWrite};

const WAIT: Duration = Duration::from_secs(1);
const COMBINED_SIZE: &str = "9007199254740993";
const STANDALONE_SIZE: &str = "0.01234567890123456789012345";

// All identities, quantities, and callback sequence values are synthetic constants.
// Replay replaces the socket. SDK request serialization, dispatch, and subscription
// decoding and the finance subscription are the production implementations.
async fn paper_market() -> (MarketReplay, PaperMarket) {
    let (replay, client) = MarketReplay::open().unwrap();
    let notices = Mutex::new(client.notice_stream().unwrap());
    let peer = Arc::new(PaperPeer { client, notices });
    let market = PaperMarket::new(peer, WAIT).await.unwrap();
    assert_eq!(replay.writes(), [MarketWrite::SelectType(Some(3))]);
    (replay, market)
}

async fn request(market: &PaperMarket, con_id: i32, kind: ContractKind) -> MarketRequest {
    let option_open_interest = matches!(kind, ContractKind::Option(_));
    let contract = ConfirmedContract {
        con_id,
        symbol: "SYNTH".into(),
        primary_exchange: Some("NASDAQ".into()),
        kind,
        liquid_hours: Vec::new(),
    };
    market
        .confirmed
        .lock()
        .await
        .insert(con_id, contract.clone());
    MarketRequest::new(
        contract,
        RequestParameters {
            option_open_interest,
        },
    )
    .unwrap()
}

async fn next_event(line: &mut PaperSubscription) -> MarketEvent {
    timeout(WAIT, line.next())
        .await
        .expect("SDK did not deliver the frame to the market line")
        .expect("market event error")
        .expect("market stream ended")
}

#[tokio::test]
async fn protobuf_market_types_route_by_request_id() {
    let (replay, market) = paper_market().await;
    let a = request(&market, 91001, ContractKind::Equity).await;
    let b = request(&market, 91002, ContractKind::Equity).await;
    let mut a = market.subscribe(&a).await.unwrap();
    let mut b = market.subscribe(&b).await.unwrap();
    assert_ne!(a.request_id(), b.request_id());

    for (request_id, code) in [
        (b.request_id(), 4),
        (a.request_id(), 1),
        (b.request_id(), 3),
        (a.request_id(), 2),
    ] {
        replay.market_data_type(request_id, code).await.unwrap();
    }

    for (line, types) in [
        (&mut a, [MarketDataType::Realtime, MarketDataType::Frozen]),
        (
            &mut b,
            [MarketDataType::DelayedFrozen, MarketDataType::Delayed],
        ),
    ] {
        for actual in types {
            assert_eq!(next_event(line).await, MarketEvent::DataType(Some(actual)));
        }
        line.cancel().await.unwrap();
    }
    market.close().await;
}

#[tokio::test]
async fn protobuf_size_text_reaches_finance_without_binary64_rounding() {
    let (replay, market) = paper_market().await;
    let option = ContractKind::Option(OptionContract {
        expiry: NaiveDate::MAX,
        strike: "30".parse().unwrap(),
        right: OptionRight::Call,
        multiplier: "100".parse().unwrap(),
        underlying_con_id: 91001,
    });
    let request = request(&market, 91002, option).await;
    let mut line = market.subscribe(&request).await.unwrap();
    let id = line.request_id();
    assert_eq!(
        replay.writes().last(),
        Some(&MarketWrite::Subscribe {
            request_id: Some(id),
            con_id: Some(91002),
            generic_ticks: Some("101".into()),
            snapshot: false,
            regulatory_snapshot: false,
        })
    );
    replay
        .price(id, TickType::Bid, 12.25, Some(COMBINED_SIZE))
        .await
        .unwrap();
    replay
        .size(id, TickType::AskSize, Some(STANDALONE_SIZE))
        .await
        .unwrap();

    for (expected_field, expected_text) in [
        (QuoteField::BidPrice, "12.25"),
        (QuoteField::BidSize, COMBINED_SIZE),
        (QuoteField::AskSize, STANDALONE_SIZE),
    ] {
        let MarketEvent::Quote { field, value, .. } = next_event(&mut line).await else {
            panic!("expected a quote event");
        };
        assert_eq!(field, expected_field);
        assert_eq!(value.unwrap().as_str(), expected_text);
    }

    for expected in [Some(COMBINED_SIZE), Some("0"), None] {
        replay
            .size(id, TickType::OptionCallOpenInterest, expected)
            .await
            .unwrap();
        let MarketEvent::OpenInterest { right, value, .. } = next_event(&mut line).await else {
            panic!("expected an option open interest event");
        };
        assert_eq!(right, OptionRight::Call);
        assert_eq!(value.as_ref().map(DecimalText::as_str), expected);
    }
    line.cancel().await.unwrap();
    market.close().await;
}

#[tokio::test]
async fn sdk_cancel_write_failure_invalidates_session_before_another_request() {
    let (replay, market) = paper_market().await;
    let a = request(&market, 91001, ContractKind::Equity).await;
    let b = request(&market, 91002, ContractKind::Equity).await;
    let mut first = market.subscribe(&a).await.unwrap();
    first.cancel().await.unwrap();
    assert_eq!(
        replay.writes().last(),
        Some(&MarketWrite::Cancel(Some(first.request_id())))
    );

    let mut failing = market.subscribe(&a).await.unwrap();
    let before_failure = replay.writes();
    replay.fail_next_write();
    assert!(
        failing.cancel().await.is_err(),
        "cancel hid the SDK write failure"
    );
    assert!(
        market.peer.client.is_connected(),
        "fixture must leave the SDK connection flag set"
    );
    assert!(matches!(
        failing.cancel().await,
        Err(MarketError::Disconnected)
    ));
    assert!(matches!(
        market.subscribe(&b).await,
        Err(MarketError::Disconnected)
    ));
    assert_eq!(replay.writes(), before_failure);
    market.close().await;
}

#[tokio::test(start_paused = true)]
async fn entitlement_probe_reports_disconnection_during_stalled_sdk_reconnect() {
    let read_timeout = Duration::from_secs(5);
    let (replay, client) = MarketReplay::open_with_dispatcher().unwrap();
    let notices = Mutex::new(client.notice_stream().unwrap());
    let peer = Arc::new(PaperPeer { client, notices });
    let market = PaperMarket::new(peer, read_timeout).await.unwrap();

    let stall_reconnect = async {
        replay.wait_for_writes(2).await;
        assert!(matches!(
            replay.writes().last(),
            Some(MarketWrite::Confirm(Some(_)))
        ));
        replay.disconnect_and_stall_reconnect();
        replay.wait_for_stalled_handshake().await;
        assert!(replay.handshake_is_pending());
    };
    let (report, ()) = timeout(WAIT, async {
        tokio::join!(
            probe_open_market(&market, ProbeOptions::default(), read_timeout),
            stall_reconnect
        )
    })
    .await
    .expect("the probe did not close its disconnected SDK session");

    let report = serde_json::to_value(report).unwrap();
    assert_eq!(report["failure"], "gateway_disconnected");
    assert_eq!(report["products"][0]["verdict"], "pending");
    assert_eq!(
        report["products"][1]["probes"][0]["reason"],
        "option_not_selected"
    );
    // Keep the market and replay alive to detect background IO surviving close.
    assert!(!replay.handshake_is_pending());
    assert_eq!(replay.reconnect_attempts(), 1);
    assert_eq!(replay.writes().len(), 2);
    assert!(!market.peer.client.is_connected());
}

#[tokio::test(start_paused = true)]
async fn immediate_sdk_close_joins_dispatcher_without_reconnect() {
    let (replay, client) = MarketReplay::open_with_dispatcher().unwrap();
    replay.disconnect_and_stall_reconnect();
    timeout(WAIT, client.disconnect())
        .await
        .expect("the SDK did not stop its newly started dispatcher");
    sleep(Duration::from_millis(1)).await;
    assert!(!client.is_connected());
    assert_eq!(replay.reconnect_attempts(), 0);
    assert!(!replay.handshake_is_pending());
}

#[tokio::test(start_paused = true)]
async fn paper_market_rejects_old_requests_without_reconnecting() {
    const REPLACEMENT_ACCOUNTS: &str = "SYNTHETIC-LIVE";

    // The same script can complete auto-reconnect with a different account
    // and leave a client connected before any caller observes its socket loss.
    let (control, client) = MarketReplay::open_with_dispatcher().unwrap();
    control.disconnect_and_answer_reconnect(REPLACEMENT_ACCOUNTS);
    assert_eq!(
        timeout(WAIT, control.reconnected_accounts()).await.unwrap(),
        REPLACEMENT_ACCOUNTS
    );
    assert!(client.is_connected());
    assert_eq!(control.reconnect_attempts(), 1);
    assert_eq!(control.writes(), [MarketWrite::StartApi(Some(91))]);
    client.disconnect().await;

    let (replay, client) =
        MarketReplay::open_with_builder(crate::IbTransport::paper_client_builder()).unwrap();
    let notices = Mutex::new(client.notice_stream().unwrap());
    let market = PaperMarket::new(Arc::new(PaperPeer { client, notices }), WAIT)
        .await
        .unwrap();
    let cached = request(&market, 91001, ContractKind::Equity).await;
    let mut previous_line = market.subscribe(&cached).await.unwrap();
    let original_writes = replay.writes();
    replay.disconnect_and_answer_reconnect(REPLACEMENT_ACCOUNTS);

    // Observe SDK channel teardown without polling the finance session/watch
    // or marking the old verification invalid from an active line reader.
    let sdk_line = previous_line.subscription.as_mut().unwrap();
    let _ = timeout(WAIT, sdk_line.next())
        .await
        .expect("dispatcher did not close or reset the old line");

    let query = ContractQuery::Equity {
        symbol: "SYNTH".into(),
        primary_exchange: "NASDAQ".into(),
    };
    let (confirmed, subscribed) = tokio::join!(market.confirm(&query), market.subscribe(&cached));
    assert_eq!(replay.writes(), original_writes);
    assert_eq!(replay.reconnect_attempts(), 0);
    assert_eq!(confirmed, Err(MarketError::Disconnected));
    assert!(matches!(subscribed, Err(MarketError::Disconnected)));
    market.close().await;
}

#[tokio::test(start_paused = true)]
async fn active_probe_reports_disconnection_before_session_poll() {
    let (replay, client) =
        MarketReplay::open_with_builder(crate::IbTransport::paper_client_builder()).unwrap();
    let notices = Mutex::new(client.notice_stream().unwrap());
    let market = PaperMarket::new(Arc::new(PaperPeer { client, notices }), WAIT)
        .await
        .unwrap();

    let disconnect_active_line = async {
        replay.wait_for_writes(2).await;
        let request_id = match replay.writes().last() {
            Some(MarketWrite::Confirm(Some(request_id))) => *request_id,
            _ => panic!("the probe did not start equity confirmation"),
        };
        replay
            .equity_contract(request_id, 91001, "QQQ", "NASDAQ")
            .await
            .unwrap();
        replay.wait_for_writes(3).await;
        assert!(matches!(
            replay.writes().last(),
            Some(MarketWrite::Subscribe {
                con_id: Some(91001),
                ..
            })
        ));
        replay.disconnect_and_stall_reconnect();
    };
    let started = tokio::time::Instant::now();
    let (report, ()) = timeout(WAIT, async {
        tokio::join!(
            probe_open_market(&market, ProbeOptions::default(), WAIT),
            disconnect_active_line
        )
    })
    .await
    .expect("the probe did not report loss of the active line");

    assert!(started.elapsed() < Duration::from_millis(100));
    let report = serde_json::to_value(report).unwrap();
    assert_eq!(report["failure"], "gateway_disconnected");
    assert_eq!(
        report["products"][0]["probes"][0]["contract_confirmed"],
        true
    );
    assert_eq!(
        report["products"][0]["probes"][0]["reason"],
        "gateway_disconnected"
    );
    assert_eq!(
        report["products"][0]["probes"][1]["reason"],
        "sweep_stopped"
    );
    assert_eq!(replay.reconnect_attempts(), 0);
}
