use super::*;
use crate::{
    IbTransport, PaperPeer,
    market::{MarketDataSource, MarketRequest, MarketSubscription, OptionRight, RequestParameters},
};
use ibapi::{
    contracts::OptionRight as IbRight,
    replay::{MarketReplay, MarketWrite},
};
use std::{sync::Arc, time::Duration};
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(2);
const EQUITY: i32 = 91001;
const EXPIRY: &str = "20990210";

// Every identifier, chain row and callback is generated from fixed synthetic
// constants. No account, socket, recording or real contract data is used.
fn definition() -> OptionChain {
    OptionChain {
        underlying_contract_id: EQUITY,
        trading_class: "SYNTH".into(),
        multiplier: "100".into(),
        exchange: "SMART".into(),
        expirations: vec![EXPIRY.into(), "20990217".into()],
        strikes: vec![10.125, 11.25],
    }
}

fn listed(con_id: i32) -> ContractDetails {
    ContractDetails {
        contract: Contract {
            contract_id: con_id,
            symbol: "SYNTH".into(),
            security_type: SecurityType::Option,
            trading_class: "SYNTH".into(),
            multiplier: "100".into(),
            right: Some(IbRight::Call),
            strike: 10.125,
            exchange: "SYNTH_VENUE".into(),
            ..Contract::default()
        },
        real_expiration_date: EXPIRY.into(),
        valid_exchanges: vec!["SMART".into()],
        under_security_type: "STK".into(),
        under_contract_id: EQUITY,
        ..ContractDetails::default()
    }
}

async fn market_with_equity() -> (MarketReplay, PaperMarket, ConfirmedContract) {
    let (replay, client) =
        MarketReplay::open_with_builder(IbTransport::paper_client_builder()).unwrap();
    let notices = client.notice_stream().unwrap();
    let peer = Arc::new(PaperPeer::new(client, notices));
    let market = PaperMarket::new(peer, WAIT).await.unwrap();
    let answer = async {
        replay.wait_for_writes(2).await;
        let Some(MarketWrite::Confirm(Some(id))) = replay.writes().pop() else {
            panic!("expected equity confirmation");
        };
        replay
            .equity_contract(id, EQUITY, "SYNTH", "NASDAQ")
            .await
            .unwrap();
    };
    let query = ContractQuery::Equity {
        symbol: "SYNTH".into(),
        primary_exchange: "NASDAQ".into(),
    };
    let (equity, ()) = timeout(WAIT, async { tokio::join!(market.confirm(&query), answer) })
        .await
        .unwrap();
    (replay, market, equity.unwrap())
}

#[test]
fn discovery_merges_smart_hints_and_filters_expired_and_wrong_underlyings() {
    let mut repeated = definition();
    repeated.expirations = vec![
        EXPIRY.into(),
        EXPIRY.into(),
        "20990209".into(),
        "bad".into(),
    ];
    repeated.strikes = vec![10.125, 12.375, 0.0, f64::INFINITY];
    repeated.multiplier = "100.0".into();
    let mut adjusted = definition();
    adjusted.trading_class = "SYNTH1".into();
    adjusted.multiplier = "125".into();
    let rows = vec![
        repeated,
        adjusted,
        OptionChain {
            exchange: "SYNTH_VENUE".into(),
            ..definition()
        },
        OptionChain {
            underlying_contract_id: EQUITY + 1,
            ..definition()
        },
        OptionChain {
            multiplier: "0".into(),
            ..definition()
        },
        definition(),
    ];
    let before_midnight_in_new_york = "2099-02-11T04:59:59Z".parse().unwrap();
    let today = option_market_date(before_midnight_in_new_york).unwrap();
    let series = discovered_series(EQUITY, rows, today);
    assert_eq!(today.format("%Y%m%d").to_string(), EXPIRY);
    assert_eq!(series.len(), 4);
    assert_eq!(series[0].expiry, today);
    assert_eq!(series[0].trading_class, "SYNTH");
    assert_eq!(series[0].multiplier.as_str(), "100");
    let strikes = |series: &OptionSeries| {
        series
            .strikes
            .iter()
            .map(|strike| strike.as_str().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(strikes(&series[0]), ["10.125", "11.25", "12.375"]);
    assert_eq!(series[1].trading_class, "SYNTH1");
    assert_eq!(series[1].multiplier.as_str(), "125");
    assert_eq!(strikes(&series[2]), ["10.125", "11.25"]);
}

#[tokio::test(start_paused = true)]
async fn protobuf_chain_replay_subscribes_only_listed_confirmed_con_ids() {
    let (replay, market, equity) = market_with_equity().await;
    let answer_series = async {
        replay.wait_for_writes(3).await;
        let request = replay.writes().pop().unwrap();
        let MarketWrite::DiscoverOptions {
            request_id: Some(id),
            ..
        } = request
        else {
            panic!("expected reqSecDefOptParams");
        };
        assert_eq!(
            request,
            MarketWrite::DiscoverOptions {
                request_id: Some(id),
                underlying_con_id: Some(EQUITY),
                symbol: Some("SYNTH".into()),
                exchange: Some(String::new()),
                security_type: Some("STK".into()),
            }
        );
        replay.option_parameters(id, &definition()).await.unwrap();
        replay.option_parameters_end(id).await.unwrap();
    };
    let (series, ()) = timeout(WAIT, async {
        tokio::join!(market.option_series(&equity), answer_series)
    })
    .await
    .unwrap();
    let series = series.unwrap();
    assert_eq!(series.len(), 2);
    let outbound = series_query(&equity, &series[0], Utc::now()).unwrap();
    assert_eq!(outbound.currency.as_str(), "USD");
    assert_eq!(outbound.exchange.as_str(), "SMART");
    assert_eq!(outbound.multiplier, "100");

    let answer_confirm = async {
        replay.wait_for_writes(4).await;
        let request = replay.writes().pop().unwrap();
        let MarketWrite::ConfirmOptionSeries {
            request_id: Some(id),
            ..
        } = request
        else {
            panic!("expected an expiry-scoped contract-details request");
        };
        assert_eq!(
            request,
            MarketWrite::ConfirmOptionSeries {
                request_id: Some(id),
                expiry: Some(EXPIRY.into()),
                trading_class: Some("SYNTH".into()),
                wildcard_strike_and_right: true,
            }
        );
        let mut wrong_underlying = listed(91003);
        wrong_underlying.under_contract_id = EQUITY + 1;
        let mut wrong_strike = listed(91004);
        wrong_strike.contract.strike = 12.375;
        let mut wrong_expiry = listed(91005);
        wrong_expiry.real_expiration_date = "20990217".into();
        let mut wrong_multiplier = listed(91006);
        wrong_multiplier.contract.multiplier = "125".into();
        let mut conflict = listed(91007);
        conflict.contract.right = Some(IbRight::Put);
        for row in [
            listed(91002),
            wrong_underlying,
            wrong_strike,
            wrong_expiry,
            wrong_multiplier,
            listed(91002),
            listed(91007),
            conflict,
        ] {
            replay.option_contract(id, &row).await.unwrap();
        }
        replay.contract_data_end(id).await.unwrap();
    };
    let (contracts, ()) = timeout(WAIT, async {
        tokio::join!(market.confirm_series(&equity, &series[0]), answer_confirm)
    })
    .await
    .unwrap();
    let mut contracts = contracts.unwrap();
    assert_eq!(contracts.len(), 1);
    let confirmed = contracts.pop().unwrap();
    assert_eq!(confirmed.con_id, 91002);
    let ContractKind::Option(ref terms) = confirmed.kind else {
        panic!("expected listed option terms");
    };
    assert_eq!(terms.strike.as_str(), "10.125");
    assert_eq!(terms.right, OptionRight::Call);

    let parameters = RequestParameters {
        option_open_interest: true,
    };
    let mut unconfirmed = confirmed.clone();
    unconfirmed.con_id = 91003;
    assert!(matches!(
        market
            .subscribe(&MarketRequest::new(unconfirmed, parameters).unwrap())
            .await,
        Err(MarketError::UnconfirmedContract)
    ));
    let mut line = market
        .subscribe(&MarketRequest::new(confirmed, parameters).unwrap())
        .await
        .unwrap();
    assert_eq!(
        replay.writes().pop(),
        Some(MarketWrite::Subscribe {
            request_id: Some(line.request_id()),
            con_id: Some(91002),
            generic_ticks: Some("101".into()),
            snapshot: false,
            regulatory_snapshot: false,
        })
    );
    line.cancel().await.unwrap();
    market.close().await;
}

#[tokio::test(start_paused = true)]
async fn discovery_rejects_unconfirmed_equity_and_reports_request_pacing() {
    let (replay, market, equity) = market_with_equity().await;
    let mut unconfirmed = equity.clone();
    unconfirmed.con_id += 1;
    assert_eq!(
        market.option_series(&unconfirmed).await,
        Err(MarketError::UnconfirmedContract)
    );
    assert_eq!(replay.writes().len(), 2);

    let answer = async {
        replay.wait_for_writes(3).await;
        let Some(MarketWrite::DiscoverOptions {
            request_id: Some(id),
            ..
        }) = replay.writes().pop()
        else {
            panic!("expected option-chain request");
        };
        replay.option_parameters(id, &definition()).await.unwrap();
        replay.notice(id, 101).await.unwrap();
    };
    let (series, ()) = timeout(WAIT, async {
        tokio::join!(market.option_series(&equity), answer)
    })
    .await
    .unwrap();
    assert_eq!(series, Err(MarketError::Pacing));
    market.close().await;
}

#[tokio::test(start_paused = true)]
async fn unfinished_series_does_not_confirm_partial_contract_details() {
    let (replay, market, equity) = market_with_equity().await;
    let series = discovered_series(
        EQUITY,
        vec![definition()],
        option_market_date(Utc::now()).unwrap(),
    );
    let answer = async {
        replay.wait_for_writes(3).await;
        let Some(MarketWrite::ConfirmOptionSeries {
            request_id: Some(id),
            ..
        }) = replay.writes().pop()
        else {
            panic!("expected option series detail request");
        };
        replay.option_contract(id, &listed(91002)).await.unwrap();
        // Deliberately omit ContractDataEnd.
    };
    let (contracts, ()) = tokio::join!(market.confirm_series(&equity, &series[0]), answer);
    assert_eq!(contracts, Err(MarketError::Timeout));
    assert!(!market.confirmed.lock().await.contains_key(&91002));
    market.close().await;
}
