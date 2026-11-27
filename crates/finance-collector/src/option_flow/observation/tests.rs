use chrono::TimeZone;
use finance_ib::market::{OptionContract, OptionRight, RequestParameters};

use super::*;

// Fixed synthetic conIds, receipt times, and quote/volume values.
fn time(seconds_from_open: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2040, 1, 20, 14, 30, 0).unwrap() + TimeDelta::seconds(seconds_from_open)
}

fn contract(con_id: i32, underlying_con_id: i32, expiry_day: u32) -> ConfirmedContract {
    ConfirmedContract {
        con_id,
        symbol: "SYNTH".into(),
        primary_exchange: None,
        kind: ContractKind::Option(OptionContract {
            expiry: NaiveDate::from_ymd_opt(2040, 1, expiry_day).unwrap(),
            strike: "100".parse().unwrap(),
            right: OptionRight::Call,
            multiplier: "12.5".parse().unwrap(),
            underlying_con_id,
        }),
        liquid_hours: vec![LiquidHours::Open {
            start: time(0),
            end: time(23_400),
        }],
    }
}

fn update(contract: &ConfirmedContract, generation: u64, message: LineMessage) -> LineUpdate {
    LineUpdate {
        key: RequestKey {
            product: contract.product(),
            con_id: contract.con_id,
            parameters: RequestParameters {
                option_open_interest: true,
            },
        },
        generation,
        message,
    }
}

fn data_type(
    engine: &mut ObservationEngine,
    contract: &ConfirmedContract,
    generation: u64,
    kind: MarketDataType,
) {
    let event = LineMessage::Event(MarketEvent::DataType(Some(kind)));
    assert_eq!(
        engine.apply(&update(contract, generation, event), Some(contract)),
        None
    );
}

fn quote(
    engine: &mut ObservationEngine,
    contract: &ConfirmedContract,
    generation: u64,
    field: QuoteField,
    value: &str,
    second: i64,
) -> Option<FlowObservation> {
    let event = LineMessage::Event(MarketEvent::Quote {
        field,
        value: Some(value.parse().unwrap()),
        delayed: false,
        received_at: time(second),
    });
    engine.apply(&update(contract, generation, event), Some(contract))
}

fn volume(
    engine: &mut ObservationEngine,
    contract: &ConfirmedContract,
    generation: u64,
    cumulative: &str,
    second: i64,
) -> Option<FlowObservation> {
    quote(
        engine,
        contract,
        generation,
        QuoteField::CumulativeVolume,
        cumulative,
        second,
    )
}

fn volume_evidence(clue: &FlowObservation) -> &VolumeIncrease {
    match &clue.observation {
        ObservationEvidence::VolumeIncrease(evidence) => evidence,
        ObservationEvidence::QuoteWidening(_) => panic!("expected a cumulative-volume clue"),
    }
}

#[test]
fn cumulative_jump_stays_a_sampled_window_and_estimates_premium_exactly() {
    let option = contract(1, 10, 21);
    let mut engine = ObservationEngine::default();
    data_type(&mut engine, &option, 1, MarketDataType::Realtime);
    assert!(volume(&mut engine, &option, 1, "9007199254740993", 1).is_none());
    assert!(quote(&mut engine, &option, 1, QuoteField::LastPrice, "1.2345", 2).is_none());
    assert!(quote(&mut engine, &option, 1, QuoteField::LastSize, "2", 3).is_none());
    let clue = volume(&mut engine, &option, 1, "9007199254741000", 4).unwrap();
    let evidence = volume_evidence(&clue);
    assert_eq!(clue.status, ObservationStatus::PendingVerification);
    assert_eq!(clue.window(), (time(1), time(4)));
    assert_eq!(evidence.delta_contracts.as_str(), "7");
    assert_eq!(
        evidence.sampled_last_size.as_ref().unwrap().value.as_str(),
        "2"
    );
    assert_eq!(
        evidence.sampled_last_price.as_ref().unwrap().received_at,
        time(2)
    );
    assert_eq!(
        evidence.estimated_premium.as_ref().unwrap().as_str(),
        "108.01875"
    );
    assert_eq!(evidence.comparison.elevated, None);

    let json = serde_json::to_value(&clue).unwrap();
    assert_eq!(json["status"], "pending_verification");
    assert_eq!(json["observation"]["kind"], "volume_increase");
    assert_eq!(json["observation"]["evidence"]["delta_contracts"], "7");
    assert!(json.get("trade_id").is_none() && json.get("participant").is_none());
    let summary = engine.summary(time(10));
    assert_eq!(summary.as_of, time(10));
    assert_eq!(summary.last_observed_at, Some(time(4)));
    assert_eq!(
        summary.counts,
        ObservationCounts {
            observations: 1,
            volume_increases: 1,
            ..Default::default()
        }
    );
}

#[test]
fn equal_receipt_times_keep_the_volume_clue_without_an_inferred_rate() {
    let option = contract(1, 10, 21);
    let mut engine = ObservationEngine::default();
    data_type(&mut engine, &option, 1, MarketDataType::Realtime);
    assert!(volume(&mut engine, &option, 1, "20", 10).is_none());
    let clue = volume(&mut engine, &option, 1, "24", 10).unwrap();
    assert_eq!(clue.window(), (time(10), time(10)));
    let evidence = volume_evidence(&clue);
    assert_eq!(evidence.delta_contracts.as_str(), "4");
    assert_eq!(evidence.comparison.elapsed_millis, 0);
    assert_eq!(evidence.comparison.elevated, None);
}

#[test]
fn rotation_lag_type_change_and_counter_reset_do_not_bridge_missing_observations() {
    let option = contract(1, 10, 21);
    let mut engine = ObservationEngine::default();
    data_type(&mut engine, &option, 1, MarketDataType::Realtime);
    assert!(volume(&mut engine, &option, 1, "1", 1).is_none());
    data_type(&mut engine, &option, 1, MarketDataType::Frozen);
    assert!(volume(&mut engine, &option, 1, "6", 2).is_none());
    data_type(&mut engine, &option, 1, MarketDataType::Realtime);
    assert!(volume(&mut engine, &option, 1, "8", 3).is_none());
    assert!(
        engine
            .apply(&update(&option, 1, LineMessage::Closed), None)
            .is_none()
    );
    data_type(&mut engine, &option, 2, MarketDataType::Delayed);
    assert!(volume(&mut engine, &option, 2, "500", 2).is_none());
    data_type(&mut engine, &option, 2, MarketDataType::Realtime);
    assert!(volume(&mut engine, &option, 2, "510", 3).is_none());
    assert!(volume(&mut engine, &option, 2, "1", 4).is_none());
    assert!(volume(&mut engine, &option, 2, "10.1", 5).is_none());
    assert!(volume(&mut engine, &option, 2, "90", 6).is_none());
    assert!(volume(&mut engine, &option, 2, "190", 37).is_none());
    assert!(volume(&mut engine, &option, 2, "180", 35).is_none());
    engine.reader_lagged();
    assert!(volume(&mut engine, &option, 2, "200", 38).is_none());
    data_type(&mut engine, &option, 3, MarketDataType::Realtime);
    assert!(volume(&mut engine, &option, 3, "300", 39).is_none());
    assert!(quote(&mut engine, &option, 3, QuoteField::LastPrice, "2.10", 40).is_none());
    let clue = volume(&mut engine, &option, 3, "310", 47).unwrap();
    let evidence = volume_evidence(&clue);
    assert_eq!(clue.window(), (time(39), time(47)));
    assert_eq!(evidence.delta_contracts.as_str(), "10");
    assert_eq!(evidence.sampled_last_price, None);
    assert_eq!(evidence.estimated_premium, None);
    assert_eq!(
        engine.summary(time(48)).counts,
        ObservationCounts {
            observations: 1,
            volume_increases: 1,
            reader_lags: 1,
            ..Default::default()
        }
    );
}

#[test]
fn rolling_volume_rate_compares_matching_underlying_expiry_and_session_segment() {
    let mut engine = ObservationEngine::default();
    let option = contract(1, 10, 21);
    data_type(&mut engine, &option, 1, MarketDataType::Realtime);
    for (second, count) in [(0, "0"), (1, "1"), (3, "3"), (6, "6")] {
        let _ = volume(&mut engine, &option, 1, count, second);
    }

    let peer = contract(2, 10, 21);
    data_type(&mut engine, &peer, 2, MarketDataType::Realtime);
    assert!(volume(&mut engine, &peer, 2, "200", 7).is_none());
    let clue = volume(&mut engine, &peer, 2, "210", 11).unwrap();
    let comparison = &volume_evidence(&clue).comparison;
    assert_eq!(comparison.baseline_samples, 3);
    assert_eq!(comparison.elapsed_millis, 4000);
    assert_eq!(comparison.elevated, Some(true));
    assert_eq!(engine.summary(time(11)).counts.elevated_volume, 1);

    for (peer, generation, start, expected_segment) in [
        (contract(3, 11, 21), 3, 13, 0),
        (contract(4, 10, 22), 4, 15, 0),
        (contract(2, 10, 21), 2, 1799, 1),
    ] {
        data_type(&mut engine, &peer, generation, MarketDataType::Realtime);
        let _ = volume(&mut engine, &peer, generation, "500", start);
        let anchor = start + 1;
        if expected_segment == 1 {
            assert!(volume(&mut engine, &peer, generation, "600", anchor).is_none());
        }
        let clue = volume(&mut engine, &peer, generation, "700", anchor + 1).unwrap();
        assert_eq!(clue.cohort.session_segment, expected_segment);
        assert_eq!(volume_evidence(&clue).comparison.baseline_samples, 0);
        assert_eq!(volume_evidence(&clue).comparison.elevated, None);
    }
}

#[test]
fn widening_requires_fresh_unlocked_quotes_and_positive_sizes() {
    let mut engine = ObservationEngine::default();
    let option = contract(1, 10, 21);
    data_type(&mut engine, &option, 1, MarketDataType::Realtime);
    use QuoteField::*;
    for (field, value, second) in [
        (BidPrice, "1.00", 1),
        (AskPrice, "1.10", 1),
        (BidSize, "10", 1),
        (AskSize, "10", 1),
        (AskPrice, "1.11", 2),
        (AskPrice, "1.09", 3),
        (AskPrice, "1.09", 4), // same top of book supplies no new baseline sample
        (AskPrice, "1.00", 5), // locked
        (AskPrice, "0.99", 6), // crossed
        (AskSize, "0", 6),
        (AskPrice, "1.50", 7), // stale bid and invalid ask size
        (BidPrice, "1.00", 7),
        (BidSize, "10", 7),
    ] {
        assert!(quote(&mut engine, &option, 1, field, value, second).is_none());
    }
    let clue = quote(&mut engine, &option, 1, AskSize, "15", 8).unwrap();
    let ObservationEvidence::QuoteWidening(evidence) = &clue.observation else {
        panic!("expected a spread clue");
    };
    assert_eq!(evidence.spread.as_str(), "0.5");
    assert_eq!(evidence.typical_spread.as_str(), "0.1");
    assert_eq!(evidence.baseline_samples, 3);
    assert_eq!(evidence.ask_size.value.as_str(), "15");
    assert_eq!(clue.window(), (time(7), time(8)));
    assert_eq!(engine.summary(time(9)).counts.quote_widenings, 1);
}
