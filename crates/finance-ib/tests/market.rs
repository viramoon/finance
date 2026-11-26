use chrono::{DateTime, Utc};
use finance_ib::market::{
    ConfirmedContract, ContractKind, GreeksSource, LiquidHours, MarketDataType, MarketError,
    MarketEvent, MarketNotice, MarketObservation, MarketRequest, MarketSession, OptionContract,
    OptionGreeks, OptionRight, Product, QuoteField, RequestParameters,
};

// The dates, contract identifiers, and quote values below are fixed synthetic inputs.
fn at(second: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(1_800_000_000 + second, 0).unwrap()
}

fn quote(field: QuoteField, value: Option<&str>, delayed: bool, second: i64) -> MarketEvent {
    MarketEvent::Quote {
        field,
        value: value.map(|value| value.parse().unwrap()),
        delayed,
        received_at: at(second),
    }
}

fn equity() -> ConfirmedContract {
    ConfirmedContract {
        con_id: 42,
        symbol: "SYNTH".into(),
        primary_exchange: Some("NASDAQ".into()),
        kind: ContractKind::Equity,
        liquid_hours: vec![
            LiquidHours::Closed {
                start: at(0),
                end: at(20),
            },
            LiquidHours::Open {
                start: at(5),
                end: at(15),
            },
        ],
    }
}

#[test]
fn request_key_separates_products_and_generic_ticks() {
    let equity = equity();
    let option = ConfirmedContract {
        kind: ContractKind::Option(OptionContract {
            expiry: at(0).date_naive(),
            strike: "100.0001".parse().unwrap(),
            right: OptionRight::Call,
            multiplier: "100".parse().unwrap(),
            underlying_con_id: 7,
        }),
        ..equity.clone()
    };
    let default = RequestParameters::default();
    let oi = RequestParameters {
        option_open_interest: true,
    };
    let equity_key = MarketRequest::new(equity.clone(), default).unwrap().key();
    let option_key = MarketRequest::new(option.clone(), default).unwrap().key();
    let oi_key = MarketRequest::new(option, oi).unwrap().key();
    assert_eq!(equity_key.product, Product::EquityL1);
    assert_eq!(option_key.product, Product::Opra);
    assert_eq!(equity_key.con_id, option_key.con_id);
    assert_ne!(equity_key, option_key);
    assert_ne!(option_key, oi_key);
    assert_eq!(
        MarketRequest::new(equity.clone(), oi),
        Err(MarketError::InvalidRequest)
    );
    assert_eq!(
        MarketRequest::new(
            ConfirmedContract {
                con_id: 0,
                ..equity
            },
            default
        ),
        Err(MarketError::InvalidRequest)
    );
}

#[test]
fn known_liquid_hours_are_half_open_and_unknown_dates_stay_unknown() {
    let contract = equity();
    for (second, session) in [
        (-1, MarketSession::Unknown),
        (0, MarketSession::Closed),
        (4, MarketSession::Closed),
        (5, MarketSession::Open),
        (14, MarketSession::Open),
        (15, MarketSession::Closed),
        (20, MarketSession::Unknown),
    ] {
        assert_eq!(contract.session_at(at(second)), session);
    }
}

#[test]
fn quote_requires_an_actual_callback_and_resets_when_the_type_changes() {
    let mut observation = MarketObservation::default();
    observation.apply(quote(QuoteField::BidPrice, Some("99.99"), false, 0));
    observation.apply(quote(QuoteField::AskPrice, Some("100.01"), false, 1));
    assert!(!observation.evidence().saw_type_callback);
    assert!(!observation.evidence().bid_ask_valid);

    observation.apply(MarketEvent::DataType(Some(MarketDataType::Realtime)));
    observation.apply(quote(QuoteField::AskPrice, Some("100.01"), false, 2));
    assert!(!observation.evidence().bid_ask_valid);
    observation.apply(quote(QuoteField::BidPrice, Some("99.99"), false, 3));
    observation.apply(MarketEvent::DataType(Some(MarketDataType::Realtime)));
    assert_eq!(observation.evidence().quote_received_at, Some(at(2)));

    observation.apply(MarketEvent::DataType(Some(MarketDataType::Delayed)));
    observation.apply(quote(QuoteField::BidPrice, Some("99.99"), false, 4));
    observation.apply(quote(QuoteField::AskPrice, Some("100.01"), true, 5));
    assert!(!observation.evidence().bid_ask_valid);
    observation.apply(quote(QuoteField::BidPrice, Some("99.99"), true, 6));
    assert_eq!(observation.evidence().quote_received_at, Some(at(5)));

    observation.apply(MarketEvent::DataType(None));
    assert!(observation.evidence().saw_type_callback);
    assert_eq!(observation.evidence().market_data_type, None);
    assert!(!observation.evidence().bid_ask_valid);
}

#[test]
fn invalidated_or_crossed_quotes_cannot_be_used_as_evidence() {
    let mut observation = MarketObservation::default();
    observation.apply(MarketEvent::DataType(Some(MarketDataType::Frozen)));
    observation.apply(quote(
        QuoteField::AskPrice,
        Some("100.0000000000000000000000001"),
        false,
        1,
    ));
    observation.apply(quote(
        QuoteField::BidPrice,
        Some("100.0000000000000000000000002"),
        false,
        2,
    ));
    assert!(!observation.evidence().bid_ask_valid);
    observation.apply(quote(
        QuoteField::BidPrice,
        Some("100.0000000000000000000000000"),
        false,
        3,
    ));
    observation.apply(quote(QuoteField::BidSize, Some("0"), false, 4));
    assert!(!observation.evidence().bid_ask_valid);
    observation.apply(quote(QuoteField::BidSize, Some("0.25"), false, 5));
    assert!(observation.evidence().bid_ask_valid);
    observation.apply(quote(QuoteField::AskPrice, None, false, 6));
    assert!(!observation.evidence().bid_ask_valid);
}

#[test]
fn option_fields_record_availability_without_exposing_values_in_evidence() {
    let mut observation = MarketObservation::default();
    observation.apply(MarketEvent::OpenInterest {
        right: OptionRight::Put,
        value: None,
        received_at: at(0),
    });
    let mut greeks = OptionGreeks {
        source: GreeksSource::Model,
        delayed: false,
        received_at: at(1),
        implied_volatility: Some("0.19".parse().unwrap()),
        delta: Some("-0.35".parse().unwrap()),
        gamma: Some("0.0123".parse().unwrap()),
        vega: Some("0.03125".parse().unwrap()),
        theta: None,
        option_price: None,
        underlying_price: None,
        pv_dividend: None,
    };
    observation.apply(MarketEvent::Greeks(Box::new(greeks.clone())));
    assert!(!observation.evidence().open_interest_received);
    assert!(!observation.evidence().greeks_received);

    observation.apply(MarketEvent::OpenInterest {
        right: OptionRight::Put,
        value: Some("1234".parse().unwrap()),
        received_at: at(2),
    });
    greeks.theta = Some("-0.149".parse().unwrap());
    observation.apply(MarketEvent::Greeks(Box::new(greeks)));
    observation.apply(MarketEvent::Notice(MarketNotice::PartialEntitlement));
    observation.apply(MarketEvent::Notice(MarketNotice::DelayedAvailable));
    assert!(observation.evidence().open_interest_received);
    assert!(observation.evidence().greeks_received);
    assert_eq!(
        observation.evidence().notice,
        Some(MarketNotice::PartialEntitlement)
    );
}
