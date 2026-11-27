use super::*;
use crate::{
    ConnectionManager, IbConfig, IbTransport, Transport,
    market::{ContractQuery, MarketDataSource, PaperMarket},
};
use ibapi::{
    contracts::{ContractDetails, OptionRight},
    market_data::historical::{TickAttributeBidAsk, TickAttributeLast},
    replay::{MarketReplay, MarketWrite},
};
use std::{sync::Mutex, time::Duration};
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(2);
const CON_ID: i32 = 91_002;
const SECOND: i64 = 1_735_917_000;
const EXACT_CONTRACTS: &str = "9007199254740993";
const EXACT_FRACTION: &str = "0.01234567890123456789012345";

struct ReplayTransport(Mutex<Option<PaperPeer>>);

#[async_trait]
impl Transport for ReplayTransport {
    type S = PaperPeer;

    async fn connect(&self, _: &str, _: i32) -> crate::Result<PaperPeer> {
        Ok(self.0.lock().unwrap().take().expect("unexpected dial"))
    }
}

async fn confirmed_paper() -> (MarketReplay, PaperMarket, ConfirmedContract) {
    let (replay, client) =
        MarketReplay::open_with_builder(IbTransport::paper_client_builder()).unwrap();
    let notices = client.notice_stream().unwrap();
    let peer = PaperPeer::new(client, notices);
    let config_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config");
    let config = IbConfig::from_layers(&config_dir, "cloudbox", |_| None).unwrap();
    let manager = ConnectionManager::new(config, ReplayTransport(Mutex::new(Some(peer)))).unwrap();
    let (market, ()) = timeout(WAIT, async {
        tokio::join!(manager.open_verified_market(), async {
            replay.wait_for_writes(1).await;
            assert_eq!(replay.writes(), [MarketWrite::Accounts]);
            replay.managed_accounts("DU-SYNTH-HISTORY").await.unwrap();
        })
    })
    .await
    .unwrap();
    let market = market.unwrap();
    let contract = ContractQuery::Option { con_id: CON_ID };
    let (confirmed, ()) = timeout(WAIT, async {
        tokio::join!(market.confirm(&contract), async {
            replay.wait_for_writes(3).await;
            let MarketWrite::Confirm(Some(id)) = replay.writes()[2] else {
                panic!("expected paper contract confirmation");
            };
            // All wire values are deterministic synthetic constants.
            let details = ContractDetails {
                contract: Contract {
                    contract_id: CON_ID,
                    symbol: "SYNTH".into(),
                    security_type: SecurityType::Option,
                    exchange: "SMART".into(),
                    currency: "USD".into(),
                    multiplier: "150".into(),
                    right: Some(OptionRight::Call),
                    strike: 17.0,
                    ..Contract::default()
                },
                real_expiration_date: "20990116".into(),
                under_contract_id: 91_001,
                under_security_type: "STK".into(),
                ..ContractDetails::default()
            };
            replay.option_contract(id, &details).await.unwrap();
            replay.contract_data_end(id).await.unwrap();
        })
    })
    .await
    .unwrap();
    (replay, market, confirmed.unwrap())
}

fn request(kind: HistoricalKind) -> HistoricalPageRequest {
    HistoricalPageRequest {
        kind,
        start: DateTime::from_timestamp(SECOND, 0).unwrap(),
        limit: PAGE_LIMIT,
    }
}

async fn request_id(replay: &MarketReplay, history_kind: &str) -> i32 {
    replay.wait_for_writes(4).await;
    let writes = replay.writes();
    let MarketWrite::HistoricalTicks {
        request_id: Some(request_id),
        ..
    } = &writes[3]
    else {
        panic!("expected a read-only historical tick request");
    };
    assert_eq!(
        writes[3],
        MarketWrite::HistoricalTicks {
            request_id: Some(*request_id),
            con_id: Some(CON_ID),
            exchange: Some("SMART".into()),
            security_type: Some("OPT".into()),
            start: Some("20250103 15:10:00 UTC".into()),
            end: None,
            limit: Some(i32::from(PAGE_LIMIT)),
            kind: Some(history_kind.into()),
            regular_hours: true,
            ignore_size: false,
        },
    );
    *request_id
}

fn raw_trade() -> ExactTickLast {
    ExactTickLast {
        timestamp: Some(SECOND),
        tick_attribute_last: TickAttributeLast {
            past_limit: true,
            unreported: false,
        },
        price: Some(1.25),
        size: Some(EXACT_CONTRACTS.into()),
        exchange: Some("SYNTHX".into()),
        special_conditions: Some(String::new()),
    }
}

#[tokio::test]
async fn paper_trade_pages_require_confirmed_contract_and_keep_exact_wire_size() {
    let (replay, market, confirmed) = confirmed_paper().await;
    let root = tempfile::tempdir().unwrap();
    let source = market.option_history(root.path());
    let mut unconfirmed = confirmed.clone();
    unconfirmed.symbol = "OTHER".into();
    assert_eq!(
        source
            .option_tick_page(&unconfirmed, request(HistoricalKind::Trades))
            .await,
        Err(HistoricalIssueReason::UnconfirmedContract),
    );
    assert_eq!(replay.writes().len(), 3);
    let (reply, request_id) = timeout(WAIT, async {
        tokio::join!(
            source.option_tick_page(&confirmed, request(HistoricalKind::Trades)),
            async {
                let id = request_id(&replay, "TRADES").await;
                replay
                    .historical_trades(id, vec![raw_trade()], false)
                    .await
                    .unwrap();
                replay
                    .historical_trades(id, vec![raw_trade()], true)
                    .await
                    .unwrap();
                id
            }
        )
    })
    .await
    .unwrap();
    let reply = reply.unwrap();
    assert_eq!(reply.provider_request_id, Some(request_id));
    let HistoricalRows::Trades(rows) = reply.rows else {
        panic!("expected trades");
    };
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], rows[1]);
    assert_eq!(rows[0].quantity.as_str(), EXACT_CONTRACTS);
    assert_eq!(rows[0].price.as_str(), "1.25");
    assert_eq!(rows[0].exchange, "SYNTHX");
    assert!(rows[0].special_conditions.is_empty());
    assert!(rows[0].past_limit);
    market.close().await;
}

#[tokio::test]
async fn paper_bid_ask_page_keeps_decimal_sizes_and_quote_attributes() {
    let (replay, market, confirmed) = confirmed_paper().await;
    let root = tempfile::tempdir().unwrap();
    let source = market.option_history(root.path());
    let (reply, ()) = timeout(WAIT, async {
        tokio::join!(
            source.option_tick_page(&confirmed, request(HistoricalKind::BidAsk)),
            async {
                let id = request_id(&replay, "BID_ASK").await;
                replay
                    .historical_quotes(
                        id,
                        vec![ExactTickBidAsk {
                            timestamp: Some(SECOND),
                            tick_attribute_bid_ask: TickAttributeBidAsk {
                                ask_past_high: true,
                                bid_past_low: false,
                            },
                            price_bid: Some(1.125),
                            price_ask: Some(1.25),
                            size_bid: Some(EXACT_CONTRACTS.into()),
                            size_ask: Some(EXACT_FRACTION.into()),
                        }],
                        true,
                    )
                    .await
                    .unwrap();
            }
        )
    })
    .await
    .unwrap();
    let HistoricalRows::BidAsk(rows) = reply.unwrap().rows else {
        panic!("expected quotes");
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].bid.as_ref().unwrap().as_str(), "1.125");
    assert_eq!(
        rows[0].bid_quantity.as_ref().unwrap().as_str(),
        EXACT_CONTRACTS
    );
    assert_eq!(
        rows[0].ask_quantity.as_ref().unwrap().as_str(),
        EXACT_FRACTION
    );
    assert!(rows[0].ask_past_high);
    market.close().await;
}

#[tokio::test]
async fn paper_no_history_notice_discards_partial_rows_without_done_marker() {
    let (replay, market, confirmed) = confirmed_paper().await;
    let root = tempfile::tempdir().unwrap();
    let source = market.option_history(root.path());
    let (reply, ()) = timeout(WAIT, async {
        tokio::join!(
            source.option_tick_page(&confirmed, request(HistoricalKind::Trades)),
            async {
                let id = request_id(&replay, "TRADES").await;
                replay
                    .historical_trades(id, vec![raw_trade()], false)
                    .await
                    .unwrap();
                replay
                    .notice_text(id, 162, "Synthetic HMDS query returned no data")
                    .await
                    .unwrap();
            }
        )
    })
    .await
    .unwrap();
    assert_eq!(reply, Err(HistoricalIssueReason::NoHistory));
    market.close().await;
}

#[tokio::test]
async fn paper_explicitly_completed_empty_page_is_distinct_from_no_history_notice() {
    let (replay, market, confirmed) = confirmed_paper().await;
    let root = tempfile::tempdir().unwrap();
    let source = market.option_history(root.path());
    let (reply, ()) = timeout(WAIT, async {
        tokio::join!(
            source.option_tick_page(&confirmed, request(HistoricalKind::Trades)),
            async {
                let id = request_id(&replay, "TRADES").await;
                replay
                    .historical_trades(id, Vec::new(), true)
                    .await
                    .unwrap();
            }
        )
    })
    .await
    .unwrap();
    assert_eq!(reply.unwrap().rows, HistoricalRows::Trades(Vec::new()));
    market.close().await;
}

#[tokio::test(start_paused = true)]
async fn paper_tick_buffer_overflow_discards_page_even_after_done_marker() {
    let (replay, market, confirmed) = confirmed_paper().await;
    let root = tempfile::tempdir().unwrap();
    let source = market.option_history(root.path());
    let release = replay.hold_next_write();
    let (reply, ()) = timeout(WAIT, async {
        tokio::join!(
            source.option_tick_page(&confirmed, request(HistoricalKind::Trades)),
            async {
                let id = request_id(&replay, "TRADES").await;
                for _ in 0..=replay.request_buffer_capacity() {
                    replay
                        .historical_trades(id, vec![raw_trade()], false)
                        .await
                        .unwrap();
                }
                replay
                    .historical_trades(id, Vec::new(), true)
                    .await
                    .unwrap();
                // A separate global notice proves the dispatcher finished
                // handling all scripted frames before the consumer can poll.
                let mut notices = source.peer.client.notice_stream().unwrap();
                replay.notice(-1, 2104).await.unwrap();
                assert_eq!(notices.next().await.unwrap().code, 2104);
                release.send(()).unwrap();
            }
        )
    })
    .await
    .unwrap();
    assert_eq!(reply, Err(HistoricalIssueReason::IncompletePage));
    market.close().await;
}
