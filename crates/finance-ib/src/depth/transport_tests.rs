//! Synthetic IBKR protobufs generated from fixed row constants via MarketReplay.

use std::{collections::VecDeque, path::Path, sync::Mutex};

use ibapi::replay::{MarketReplay, MarketWrite};
use tokio::time::timeout;

use super::*;
use crate::market::ContractKind;
use crate::{ConnectionManager, IbConfig, IbTransport, Transport};

const WAIT: Duration = Duration::from_secs(1);
const FIVE_ROWS: u32 = 5;
const AAPL_FIXTURE_ID: i32 = 91001;
const EXACT_LARGE_SIZE: &str = "9007199254740993";
const EXACT_FRACTION: &str = "0.01234567890123456789012345";

struct ReplayTransport(Mutex<VecDeque<PaperPeer>>);

#[async_trait]
impl Transport for ReplayTransport {
    type S = PaperPeer;

    async fn connect(&self, _: &str, _: i32) -> crate::Result<PaperPeer> {
        Ok(self.0.lock().unwrap().pop_front().expect("unexpected dial"))
    }
}

fn replay_market() -> (MarketReplay, ConnectionManager<ReplayTransport>) {
    let (replay, client) =
        MarketReplay::open_with_builder(IbTransport::paper_client_builder()).unwrap();
    let notices = client.notice_stream().unwrap();
    let peer = PaperPeer::new(client, notices);
    let config_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config");
    let config = IbConfig::from_layers(&config_dir, "cloudbox", |_| None).unwrap();
    let transport = ReplayTransport(Mutex::new(VecDeque::from([peer])));
    (replay, ConnectionManager::new(config, transport).unwrap())
}

async fn confirm(
    replay: &MarketReplay,
    market: &PaperMarket,
    symbol: DepthSymbol,
    con_id: i32,
) -> DepthRequest {
    let writes_before = replay.writes().len();
    let (result, ()) = timeout(WAIT, async {
        tokio::join!(market.confirm_depth(symbol), async {
            replay.wait_for_writes(writes_before + 1).await;
            let MarketWrite::Confirm(Some(id)) = replay.writes()[writes_before] else {
                panic!("expected Gateway contract confirmation");
            };
            replay
                .equity_contract(id, con_id, symbol.as_str(), "NASDAQ")
                .await
                .unwrap();
        })
    })
    .await
    .unwrap();
    DepthRequest::new(symbol, result.unwrap(), FIVE_ROWS).unwrap()
}

fn row(size: &str) -> ExactMarketDepth {
    ExactMarketDepth {
        position: 0,
        operation: 0,
        side: 1,
        price: Some(12.25),
        size: Some(size.into()),
        market_maker: Some("SYNTHETIC-MPID".into()),
        smart_depth: false,
    }
}

async fn next(line: &mut PaperDepthSubscription) -> Result<Option<DepthUpdate>, DepthError> {
    timeout(WAIT, line.next()).await.unwrap()
}

fn assert_row(
    update: Result<Option<DepthUpdate>, DepthError>,
    side: DepthSide,
    operation: DepthOperation,
    size: Option<&str>,
) {
    assert_eq!(
        update,
        Ok(Some(DepthUpdate::Row {
            side,
            operation,
            position: 0,
            level: size.map(|size| DepthLevel {
                price: "12.25".parse().unwrap(),
                size: size.parse().unwrap(),
            }),
        }))
    );
}

#[tokio::test]
async fn verified_paper_depth_routes_direct_nasdaq_and_preserves_wire_rows() {
    let (replay, manager) = replay_market();
    let (market, ()) = timeout(WAIT, async {
        tokio::join!(manager.open_verified_market(), async {
            replay.wait_for_writes(1).await;
            assert_eq!(replay.writes(), [MarketWrite::Accounts]);
            replay.managed_accounts("DU-SYNTH-DEPTH").await.unwrap();
        })
    })
    .await
    .unwrap();
    let market = market.unwrap();
    let aapl = confirm(&replay, &market, DepthSymbol::Aapl, AAPL_FIXTURE_ID).await;
    let nvda = confirm(&replay, &market, DepthSymbol::Nvda, 91002).await;
    let mut a = market.subscribe_depth(&aapl).await.unwrap();
    assert_eq!(
        replay.writes().last(),
        Some(&MarketWrite::SubscribeDepth {
            request_id: Some(a.request_id()),
            con_id: Some(AAPL_FIXTURE_ID),
            exchange: Some("ISLAND".into()),
            rows: Some(i32::try_from(FIVE_ROWS).unwrap()),
            smart_depth: false,
        })
    );
    let mut b = market.subscribe_depth(&nvda).await.unwrap();
    assert_ne!(a.request_id(), b.request_id());

    let mut ask = row(EXACT_FRACTION);
    ask.side = 0;
    replay.depth(b.request_id(), false, ask).await.unwrap();
    replay
        .depth(a.request_id(), true, row(EXACT_LARGE_SIZE))
        .await
        .unwrap();
    assert_row(
        next(&mut a).await,
        DepthSide::Bid,
        DepthOperation::Insert,
        Some(EXACT_LARGE_SIZE),
    );
    assert_row(
        next(&mut b).await,
        DepthSide::Ask,
        DepthOperation::Insert,
        Some(EXACT_FRACTION),
    );

    let mut update = row(EXACT_FRACTION);
    update.operation = 1;
    replay.depth(a.request_id(), true, update).await.unwrap();
    assert_row(
        next(&mut a).await,
        DepthSide::Bid,
        DepthOperation::Update,
        Some(EXACT_FRACTION),
    );
    let mut delete = row("unused");
    delete.operation = 2;
    delete.price = None;
    delete.size = None;
    replay.depth(a.request_id(), true, delete).await.unwrap();
    assert_row(
        next(&mut a).await,
        DepthSide::Bid,
        DepthOperation::Delete,
        None,
    );
    for line in [&mut a, &mut b] {
        line.cancel().await.unwrap();
        assert_eq!(
            replay.writes().last(),
            Some(&MarketWrite::CancelDepth {
                request_id: Some(line.request_id()),
                smart_depth: false,
            })
        );
    }
    market.close().await;
}

#[tokio::test]
async fn mixed_account_is_rejected_before_any_depth_request() {
    let (replay, manager) = replay_market();
    let (result, ()) = timeout(WAIT, async {
        tokio::join!(manager.open_verified_market(), async {
            replay.wait_for_writes(1).await;
            replay
                .managed_accounts("DU-SYNTH-DEPTH,SYNTHETIC-LIVE")
                .await
                .unwrap();
        })
    })
    .await
    .unwrap();
    assert!(matches!(result, Err(MarketError::UnsafeAccount)));
    assert_eq!(replay.writes(), [MarketWrite::Accounts]);
    assert!(replay.is_closed());
}

fn synthetic_request() -> DepthRequest {
    DepthRequest::new(
        DepthSymbol::Aapl,
        ConfirmedContract {
            con_id: AAPL_FIXTURE_ID,
            symbol: "AAPL".into(),
            primary_exchange: Some("NASDAQ".into()),
            kind: ContractKind::Equity,
            liquid_hours: Vec::new(),
        },
        FIVE_ROWS,
    )
    .unwrap()
}

async fn fixture_market() -> (MarketReplay, PaperMarket) {
    let (replay, client) = MarketReplay::open().unwrap();
    let notices = client.notice_stream().unwrap();
    let peer = Arc::new(PaperPeer::new(client, notices));
    let market = PaperMarket::new(peer, WAIT).await.unwrap();
    (replay, market)
}

async fn cached_line(market: &PaperMarket) -> PaperDepthSubscription {
    let request = synthetic_request();
    market
        .confirmed
        .lock()
        .await
        .insert(request.contract().con_id, request.contract().clone());
    market.subscribe_depth(&request).await.unwrap()
}

#[tokio::test]
async fn depth_request_rejects_an_unconfirmed_contract_without_a_wire_write() {
    let (replay, market) = fixture_market().await;
    assert!(matches!(
        market.subscribe_depth(&synthetic_request()).await,
        Err(DepthError::UnconfirmedContract)
    ));
    assert_eq!(replay.writes(), [MarketWrite::SelectType(Some(3))]);
    market.close().await;
}

#[tokio::test]
async fn depth_reset_and_halt_stop_old_rows_before_checked_cancel() {
    let (replay, market) = fixture_market().await;
    for code in [316, 317] {
        let mut line = cached_line(&market).await;
        let old_id = line.request_id();
        replay.notice(old_id, code).await.unwrap();
        replay.depth(old_id, true, row("99")).await.unwrap();
        assert_eq!(next(&mut line).await, Ok(Some(DepthUpdate::Reset)));
        assert_eq!(next(&mut line).await, Ok(None));
        line.cancel().await.unwrap();
        let mut replacement = market.subscribe_depth(&synthetic_request()).await.unwrap();
        assert_ne!(old_id, replacement.request_id());
        replay.depth(old_id, true, row("98")).await.unwrap();
        replay
            .depth(replacement.request_id(), true, row("97"))
            .await
            .unwrap();
        assert_row(
            next(&mut replacement).await,
            DepthSide::Bid,
            DepthOperation::Insert,
            Some("97"),
        );
        replacement.cancel().await.unwrap();
    }
    market.close().await;
}

#[tokio::test]
async fn depth_notices_distinguish_permission_capacity_and_competing_session() {
    let (replay, market) = fixture_market().await;
    for (code, expected) in [
        (354, DepthError::NotEntitled),
        (10089, DepthError::NotEntitled),
        (10090, DepthError::NotEntitled),
        (309, DepthError::Capacity),
        (10197, DepthError::CompetingSession),
        (420, DepthError::Pacing),
    ] {
        let mut line = cached_line(&market).await;
        replay.notice(line.request_id(), code).await.unwrap();
        assert_eq!(next(&mut line).await, Err(expected));
        line.cancel().await.unwrap();
    }
    market.close().await;
}

#[tokio::test]
async fn malformed_or_smart_depth_never_produces_a_direct_row() {
    let (replay, market) = fixture_market().await;
    let mut smart = row("10");
    smart.smart_depth = true;
    let mut not_a_price = row("10");
    not_a_price.price = Some(f64::NAN);
    let mut negative_position = row("10");
    negative_position.position = -1;
    for malformed in [smart, not_a_price, row("-1"), negative_position] {
        let mut line = cached_line(&market).await;
        replay
            .depth(line.request_id(), true, malformed)
            .await
            .unwrap();
        assert_eq!(next(&mut line).await, Err(DepthError::InvalidCallback));
        assert_eq!(next(&mut line).await, Ok(None));
        line.cancel().await.unwrap();
    }
    market.close().await;
}

#[tokio::test]
async fn sdk_depth_lag_signals_reset_before_any_surviving_delta() {
    let (replay, market) = fixture_market().await;
    let mut line = cached_line(&market).await;
    for _ in 0..2048 {
        replay
            .depth(line.request_id(), true, row("10"))
            .await
            .unwrap();
    }
    assert_eq!(next(&mut line).await, Ok(Some(DepthUpdate::Reset)));
    assert_eq!(next(&mut line).await, Ok(None));
    line.cancel().await.unwrap();
    market.close().await;
}

#[tokio::test]
async fn depth_cancel_write_failure_invalidates_session_before_replacement() {
    let (replay, market) = fixture_market().await;
    let mut line = cached_line(&market).await;
    let before = replay.writes();
    replay.fail_next_write();
    assert!(line.cancel().await.is_err());
    assert!(market.peer.client.is_connected());
    assert!(matches!(
        market.subscribe_depth(&synthetic_request()).await,
        Err(DepthError::Disconnected)
    ));
    assert_eq!(replay.writes(), before);
    market.close().await;
}
