// Synthetic account replies and notices. The fixture replaces dialing with a
// queue of distinct SDK connections, each using the production paper builder.
use super::*;
use crate::market::{
    ContractKind, ContractQuery, MarketDataSource, MarketError, MarketRequest, MarketSubscription,
    OptionMinuteSource,
};
use finance_market_protocol::U64Text;
use ibapi::replay::{MarketReplay, MarketWrite, ReplayHistoricalBar};
use std::{collections::VecDeque, sync::Mutex};
use time::macros::datetime;

struct ReplayTransport(Mutex<VecDeque<PaperPeer>>);

#[async_trait]
impl Transport for ReplayTransport {
    type S = PaperPeer;
    async fn connect(&self, _: &str, _: i32) -> Result<PaperPeer> {
        Ok(self.0.lock().unwrap().pop_front().expect("unexpected dial"))
    }
}

fn manager<const N: usize>() -> (ConnectionManager<ReplayTransport>, [MarketReplay; N]) {
    let mut peers = VecDeque::new();
    let replays = std::array::from_fn(|_| {
        let (replay, client) =
            MarketReplay::open_with_builder(IbTransport::paper_client_builder()).unwrap();
        let notices = client.notice_stream().unwrap();
        peers.push_back(PaperPeer::new(client, notices));
        replay
    });
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config");
    let config = IbConfig::from_layers(&dir, "cloudbox", |_| None).unwrap();
    (
        ConnectionManager::new(config, ReplayTransport(Mutex::new(peers))).unwrap(),
        replays,
    )
}

async fn answer_accounts(replay: &MarketReplay, accounts: &str) {
    replay.wait_for_writes(1).await;
    assert_eq!(replay.writes(), [MarketWrite::Accounts]);
    replay.managed_accounts(accounts).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn verified_paper_option_history_requests_and_decodes_synthetic_bars() {
    const WAIT: Duration = Duration::from_secs(3);
    let (manager, [replay]) = manager();
    let (market, ()) = timeout(WAIT, async {
        tokio::join!(
            manager.open_verified_market(),
            answer_accounts(&replay, "DU-SYNTH-HISTORY")
        )
    })
    .await
    .expect("paper account verification did not finish");
    let market = market.unwrap();

    let con_id = 91002;
    let query = ContractQuery::Option { con_id };
    let expiry = Utc::now() + chrono::Duration::days(2);
    let expiry_date = expiry.format("%Y%m%d").to_string();
    let cutoff = u64::try_from(expiry.timestamp_nanos_opt().unwrap())
        .unwrap()
        .into();
    let (confirmed, ()) = timeout(WAIT, async {
        tokio::join!(market.confirm(&query), async {
            replay.wait_for_writes(3).await;
            let MarketWrite::Confirm(Some(id)) = replay.writes()[2] else {
                panic!("expected an option confirmation request");
            };
            replay
                .option_contract(id, con_id, &expiry_date)
                .await
                .unwrap();
        })
    })
    .await
    .expect("option confirmation did not finish");
    let confirmed = confirmed.unwrap();
    assert_eq!(confirmed.con_id, con_id);
    assert!(matches!(confirmed.kind, ContractKind::Option(_)));

    let start = datetime!(2025-03-07 16:00 UTC);
    let end = datetime!(2025-03-07 16:02 UTC);
    let ns = |instant: time::OffsetDateTime| {
        U64Text::from(u64::try_from(instant.unix_timestamp_nanos()).unwrap())
    };
    let window = ns(start)..ns(end);
    let root = tempfile::tempdir().unwrap();
    let mut history = std::pin::pin!(market.option_trade_minutes(
        &confirmed,
        window.clone(),
        cutoff,
        root.path(),
    ));
    tokio::select! {
        biased;
        result = &mut history => panic!("history completed before sending a request: {result:?}"),
        result = timeout(WAIT, replay.wait_for_writes(4)) => {
            result.expect("no history request reached the replay transport");
        }
    }
    let writes = replay.writes();
    let MarketWrite::Historical {
        request_id: Some(id),
        ..
    } = writes[3]
    else {
        panic!("expected a one-shot history request");
    };
    assert_eq!(
        writes[3],
        MarketWrite::Historical {
            request_id: Some(id),
            con_id: Some(con_id),
            sec_type: Some("OPT".into()),
            exchange: Some("SMART".into()),
            currency: Some("USD".into()),
            include_expired: false,
            end: Some("20250307 16:02:00 UTC".into()),
            duration: Some("1 D".into()),
            bar_size: Some("1 min".into()),
            what_to_show: Some("TRADES".into()),
            use_rth: true,
            format_date: Some(2),
            keep_up_to_date: false,
        }
    );

    // Hand-authored in-memory rows. Only the two middle bars fit the window.
    let rows = [(-60, "99"), (0, "15.125"), (60, "17"), (120, "98")]
        .into_iter()
        .map(|(offset, volume)| ReplayHistoricalBar {
            date: Some((start.unix_timestamp() + offset).to_string()),
            open: Some(1.125),
            high: Some(1.5),
            low: Some(1.0),
            close: Some(1.25),
            volume: Some(volume.into()),
            ..Default::default()
        })
        .collect();
    replay
        .historical_bars(id, rows, "20250307 15:59:00 UTC", "20250307 16:02:00 UTC")
        .await
        .unwrap();
    let bars = timeout(WAIT, &mut history)
        .await
        .expect("history did not finish after the end marker")
        .unwrap();
    assert_eq!(bars.len(), 2);
    assert_eq!(bars[0].data.open_ts, window.start);
    assert_eq!(bars[0].close_ts, ns(datetime!(2025-03-07 16:01 UTC)));
    assert_eq!(bars[1].data.open_ts, bars[0].close_ts);
    assert_eq!(bars[1].close_ts, window.end);
    for (bar, volume) in bars.iter().zip(["15.125", "17"]) {
        assert_eq!(bar.data.open.as_str(), "1.125");
        assert_eq!(bar.data.high.as_str(), "1.5");
        assert_eq!(bar.data.low.as_str(), "1");
        assert_eq!(bar.data.close.as_str(), "1.25");
        assert_eq!(bar.data.volume.as_str(), volume);
    }
    assert_eq!(writes.len(), 4);
    assert_eq!(
        market
            .option_trade_minutes(&confirmed, window.clone(), window.end, root.path())
            .await,
        Err(MarketError::ExpiredContract)
    );
    assert_eq!(replay.writes(), writes);
    market.close().await;
    assert!(replay.is_closed());
}

#[tokio::test(start_paused = true)]
async fn each_new_paper_market_rechecks_accounts_and_its_own_contracts() {
    let (manager, [first, replacement, fresh]) = manager();
    let (market, ()) = tokio::join!(
        manager.open_verified_market(),
        answer_accounts(&first, "DU-SYNTH-A")
    );
    let market = market.unwrap();
    let query = ContractQuery::Equity {
        symbol: "SYNTH".into(),
        primary_exchange: "NASDAQ".into(),
    };
    let (contract, ()) = tokio::join!(market.confirm(&query), async {
        first.wait_for_writes(3).await;
        let MarketWrite::Confirm(Some(id)) = first.writes()[2] else {
            panic!("expected contract lookup")
        };
        first
            .equity_contract(id, 91001, "SYNTH", "NASDAQ")
            .await
            .unwrap();
    });
    let cached = MarketRequest::new(contract.unwrap(), Default::default()).unwrap();
    let mut old_line = market.subscribe(&cached).await.unwrap();
    first.disconnect_and_answer_reconnect("SYNTHETIC-LIVE");
    assert_eq!(old_line.next().await, Err(MarketError::Disconnected));
    market.close().await;
    assert!(first.is_closed());
    assert_eq!(first.reconnect_attempts(), 0);

    let (denied, ()) = tokio::join!(
        manager.open_verified_market(),
        answer_accounts(&replacement, "DU-SYNTH-B,SYNTHETIC-LIVE")
    );
    assert!(matches!(denied, Err(MarketError::UnsafeAccount)));
    assert_eq!(replacement.writes(), [MarketWrite::Accounts]);
    assert!(replacement.is_closed());

    let (new_market, ()) = tokio::join!(
        manager.open_verified_market(),
        answer_accounts(&fresh, "DU-SYNTH-C")
    );
    let new_market = new_market.unwrap();
    assert!(matches!(
        new_market.subscribe(&cached).await,
        Err(MarketError::UnconfirmedContract)
    ));
    assert!(matches!(
        market.subscribe(&cached).await,
        Err(MarketError::Disconnected)
    ));
    assert_eq!(old_line.cancel().await, Err(MarketError::Disconnected));
    assert_eq!(
        fresh.writes(),
        [MarketWrite::Accounts, MarketWrite::SelectType(Some(3))]
    );
    new_market.close().await;
}

#[tokio::test(start_paused = true)]
async fn account_verification_distinguishes_loss_empty_reply_and_silence() {
    for (fault, expected) in [
        ("socket", MarketError::Disconnected),
        ("notice", MarketError::Disconnected),
        ("empty", MarketError::UnsafeAccount),
        ("silent", MarketError::Timeout),
        ("mode write", MarketError::Disconnected),
    ] {
        let (manager, [replay]) = manager();
        let (opened, ()) = tokio::join!(manager.open_verified_market(), async {
            replay.wait_for_writes(1).await;
            match fault {
                "socket" => replay.disconnect_and_stall_reconnect(),
                "notice" => replay.notice(-1, 1100).await.unwrap(),
                "empty" => replay.managed_accounts("").await.unwrap(),
                "mode write" => {
                    replay.fail_next_write();
                    replay.managed_accounts("DU-SYNTH").await.unwrap();
                }
                "silent" => {}
                _ => unreachable!(),
            }
        });
        assert!(matches!(opened, Err(error) if error == expected), "{fault}");
        assert!(replay.is_closed());
        assert_eq!(replay.reconnect_attempts(), 0);
        assert_eq!(replay.writes(), [MarketWrite::Accounts]);
    }
}

#[tokio::test(start_paused = true)]
async fn failed_account_verification_preserves_global_pacing() {
    let (manager, [replay]) = manager();
    let (pacing, observed) = watch::channel(None);
    let (opened, sent_at) =
        tokio::join!(manager.open_verified_market_with_pacing(&pacing), async {
            replay.wait_for_writes(1).await;
            let sent_at = Instant::now();
            replay.notice(-1, 100).await.unwrap();
            replay.notice(-1, 1100).await.unwrap();
            sent_at
        });
    assert!(matches!(opened, Err(MarketError::Disconnected)));
    assert_eq!(replay.writes(), [MarketWrite::Accounts]);
    assert!(replay.is_closed());
    assert!(observed.borrow().is_some_and(|at| at >= sent_at));
}

#[tokio::test(start_paused = true)]
async fn failed_market_type_selection_preserves_global_pacing() {
    let (manager, [replay]) = manager();
    let (pacing, observed) = watch::channel(None);
    let (opened, (_release, sent_at)) =
        tokio::join!(manager.open_verified_market_with_pacing(&pacing), async {
            replay.wait_for_writes(1).await;
            let release = replay.hold_next_write();
            replay.managed_accounts("DU-SYNTH").await.unwrap();
            replay.wait_for_writes(2).await;
            let sent_at = Instant::now();
            replay.notice(-1, 100).await.unwrap();
            replay.notice(-1, 1100).await.unwrap();
            (release, sent_at)
        });
    assert!(matches!(opened, Err(MarketError::Disconnected)));
    assert_eq!(
        replay.writes(),
        [MarketWrite::Accounts, MarketWrite::SelectType(Some(3))]
    );
    assert!(replay.is_closed());
    assert!(observed.borrow().is_some_and(|at| at >= sent_at));
}

#[tokio::test(start_paused = true)]
async fn initial_connect_failure_or_timeout_is_classified_without_a_peer() {
    #[derive(Clone, Copy, Debug)]
    enum InitialFault {
        Disconnect,
        Silent,
        InvalidConfiguration,
        UnsafeAccount,
    }

    #[async_trait]
    impl Transport for InitialFault {
        type S = PaperPeer;

        async fn connect(&self, _: &str, _: i32) -> Result<Self::S> {
            match self {
                Self::Disconnect => Err(Error::Disconnected("synthetic startup loss".into())),
                Self::Silent => std::future::pending().await,
                Self::InvalidConfiguration => Err(Error::Config("synthetic invalid setup".into())),
                Self::UnsafeAccount => Err(Error::UnsafeAccount),
            }
        }
    }

    let (fixture, []) = manager();
    for (fault, expected, legacy) in [
        (
            InitialFault::Disconnect,
            MarketError::HandshakeFailed,
            MarketError::Disconnected,
        ),
        (
            InitialFault::Silent,
            MarketError::HandshakeFailed,
            MarketError::Timeout,
        ),
        (
            InitialFault::InvalidConfiguration,
            MarketError::InvalidConfiguration,
            MarketError::InvalidConfiguration,
        ),
        (
            InitialFault::UnsafeAccount,
            MarketError::UnsafeAccount,
            MarketError::UnsafeAccount,
        ),
    ] {
        let manager = ConnectionManager::new(fixture.config.clone(), fault).unwrap();
        let (pacing, observed) = watch::channel(None);
        let opened = manager.open_verified_market_with_pacing(&pacing).await;
        assert!(
            matches!(opened, Err(error) if error == expected),
            "{fault:?}"
        );
        assert_eq!(*observed.borrow(), None);
        let legacy_opened = manager.open_verified_market().await;
        assert!(
            matches!(legacy_opened, Err(error) if error == legacy),
            "{fault:?}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn manager_stops_recovery_if_replacement_fails_paper_verification() {
    #[derive(Default)]
    struct Hooks {
        stages: Vec<RecoveryStage>,
        offline: usize,
    }
    #[async_trait]
    impl Recovery<PaperPeer> for Hooks {
        async fn restore(&mut self, stage: RecoveryStage, _: Arc<PaperPeer>) -> Result<()> {
            self.stages.push(stage);
            Err(Error::Recovery("synthetic stream failure".into()))
        }
        async fn offline(&mut self) {
            self.offline += 1;
        }
    }
    let (manager, [first, replacement]) = manager();
    let mut hooks = Hooks::default();
    let (result, ()) = tokio::join!(manager.run(&mut hooks), async {
        answer_accounts(&first, "DU-SYNTH").await;
        answer_accounts(&replacement, "SYNTHETIC-LIVE").await;
    });
    assert!(matches!(result, Err(Error::UnsafeAccount)));
    let status = manager.subscribe().borrow().clone();
    assert_eq!(status.state, Phase::Disconnected);
    assert_eq!(status.disconnects, 1);
    assert_eq!(status.last_error, Some(Error::UnsafeAccount.to_string()));
    assert_eq!(hooks.stages, [RecoveryStage::Subscriptions]);
    assert_eq!(hooks.offline, 1);
    assert!(first.is_closed() && replacement.is_closed());
    assert_eq!(replacement.writes(), [MarketWrite::Accounts]);
}

#[tokio::test(start_paused = true)]
async fn connection_probe_closes_after_heartbeat_loss_or_either_deadline() {
    for (read_ms, notice) in [(500, true), (500, false), (10, false)] {
        let (mut manager, [replay]) = manager();
        manager.config.connect_timeout_ms = 100;
        manager.config.read_timeout_ms = read_ms;
        let (probe, ()) = tokio::join!(manager.probe(), async {
            answer_accounts(&replay, "DU-SYNTH").await;
            replay.wait_for_writes(2).await;
            if notice {
                replay.notice(-1, 1100).await.unwrap();
            }
        });
        match (notice, read_ms) {
            (true, _) => assert!(matches!(probe, Err(Error::Disconnected(_)))),
            (false, 10) => assert!(matches!(probe, Err(Error::Timeout("server_time")))),
            _ => assert!(matches!(probe, Err(Error::Timeout("paper probe")))),
        }
        assert_eq!(
            replay.writes(),
            [MarketWrite::Accounts, MarketWrite::ServerTime]
        );
        assert!(replay.is_closed());
    }
}

#[tokio::test(start_paused = true)]
async fn buffered_heartbeat_cannot_validate_a_closed_peer() {
    let (manager, [replay, healthy]) = manager();
    let (session, ()) = tokio::join!(
        manager.open(manager.config.probe_client_id),
        answer_accounts(&replay, "DU-SYNTH")
    );
    let (peer, _) = session.unwrap();
    let mut heartbeat = std::pin::pin!(peer.server_time());
    tokio::select! {
        biased;
        result = &mut heartbeat => panic!("unexpected heartbeat: {result:?}"),
        _ = replay.wait_for_writes(2) => {}
    }
    replay.server_time(1000).await.unwrap();
    replay.disconnect_and_stall_reconnect();
    timeout(Duration::from_secs(1), async {
        while !replay.is_closed() {
            sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    assert!(matches!(heartbeat.await, Err(Error::Disconnected(_))));
    peer.close().await;

    let (result, ()) = tokio::join!(manager.probe(), async {
        answer_accounts(&healthy, "DU-SYNTH").await;
        healthy.wait_for_writes(2).await;
        healthy.server_time(1000).await.unwrap();
    });
    assert_eq!(result.unwrap(), (1000, vec![String::from("DU-SYNTH")]));
    assert!(healthy.is_closed());
}
