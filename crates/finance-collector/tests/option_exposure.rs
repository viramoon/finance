use chrono::{DateTime, NaiveDate, Utc};
use finance_collector::option_exposure::{
    ContractGammaGrid, DealerScenario, DecimalObservation, ExposureError, ExposureReport,
    ExposureTotals, FlipCrossing, FlipUnavailableReason, GammaFlip, GammaGrid, MODEL_VERSION,
    OpenInterestObservation, OptionChainSnapshot, OptionContractSnapshot, OptionRight,
    TimingPolicy, Wall, evaluate,
};
use finance_market_protocol::DecimalText;
use rust_decimal::Decimal;
use std::time::Duration;

// These fixtures are constructed entirely from invented constants. The session
// date and UTC date differ to exercise 0DTE classification near midnight UTC.
const SESSION_DATE: &str = "2025-03-04";
const LATER_EXPIRY: &str = "2025-03-07";
const OI_DATE: &str = "2025-03-03";
const VALUATION_TIME: &str = "2025-03-05T00:30:00Z";
const GREEK_TIME: &str = "2025-03-05T00:29:59Z";
const OI_TIME: &str = "2025-03-04T23:30:00Z";
const TIMING: TimingPolicy = TimingPolicy {
    max_spot_age_seconds: 5,
    max_gamma_age_seconds: 5,
    max_spot_gamma_skew_seconds: 3,
};

fn decimal(text: &str) -> DecimalText {
    text.parse().unwrap()
}

fn date(text: &str) -> NaiveDate {
    text.parse().unwrap()
}

fn time(text: &str) -> DateTime<Utc> {
    text.parse().unwrap()
}

fn observed(value: &str, observed_at: &str) -> DecimalObservation {
    DecimalObservation {
        value: decimal(value),
        observed_at: time(observed_at),
    }
}

fn contract(
    id: &str,
    right: OptionRight,
    expiry: &str,
    strike: &str,
    multiplier: &str,
    oi: Option<&str>,
    gamma: Option<&str>,
) -> OptionContractSnapshot {
    OptionContractSnapshot {
        instrument_id: id.parse().unwrap(),
        right,
        expiry: date(expiry),
        strike: decimal(strike),
        multiplier: decimal(multiplier),
        open_interest: oi.map(|value| OpenInterestObservation {
            value: decimal(value),
            observed_at: time(OI_TIME),
            business_date: Some(date(OI_DATE)),
        }),
        gamma: gamma.map(|value| observed(value, GREEK_TIME)),
    }
}

fn snapshot(contracts: Vec<OptionContractSnapshot>) -> OptionChainSnapshot {
    OptionChainSnapshot {
        underlying_id: "SYNTH.XSIM".parse().unwrap(),
        as_of: time(VALUATION_TIME),
        session_date: date(SESSION_DATE),
        spot: observed("125", VALUATION_TIME),
        universe_complete: true,
        contracts,
    }
}

fn hand_chain() -> OptionChainSnapshot {
    use OptionRight::{Call, Put};

    // S² × 0.01 = 156.25. The products OI × M × gamma are 20, 2, 16,
    // 1 and 4 respectively. Two different decimal lexemes denote strike 120.
    snapshot(vec![
        contract(
            "D0C120.XSIM",
            Call,
            SESSION_DATE,
            "120",
            "100",
            Some("10"),
            Some("0.02"),
        ),
        contract(
            "D0P120.XSIM",
            Put,
            SESSION_DATE,
            "120.00",
            "10",
            Some("5"),
            Some("0.04"),
        ),
        contract(
            "D3P130.XSIM",
            Put,
            LATER_EXPIRY,
            "130",
            "50",
            Some("8"),
            Some("0.04"),
        ),
        contract(
            "D3C130.XSIM",
            Call,
            LATER_EXPIRY,
            "130",
            "25",
            Some("2"),
            Some("0.02"),
        ),
        contract(
            "D3P120.XSIM",
            Put,
            LATER_EXPIRY,
            "120",
            "50",
            Some("4"),
            Some("0.02"),
        ),
    ])
}

fn run(chain: &OptionChainSnapshot, scenario: DealerScenario) -> ExposureReport {
    evaluate(chain, scenario, TIMING, None).unwrap()
}

fn amount(value: Option<&str>) -> Option<Decimal> {
    value.map(|value| decimal(value).value())
}

fn assert_totals(totals: &ExposureTotals, absolute: Option<&str>, gex: Option<&str>) {
    assert_eq!(
        totals.absolute.as_ref().map(DecimalText::value),
        amount(absolute)
    );
    assert_eq!(totals.gex.as_ref().map(DecimalText::value), amount(gex));
}

fn assert_wall(wall: &Option<Wall>, strike: &str, exposure: &str) {
    let wall = wall.as_ref().expect("expected a wall");
    assert_eq!(wall.strike.value(), decimal(strike).value());
    assert_eq!(wall.exposure.value(), decimal(exposure).value());
}

#[test]
fn hand_calculated_exposures_by_strike_expiry_and_zero_dte() {
    let chain = hand_chain();
    let report = run(&chain, DealerScenario::CallLongPutShort);
    assert_eq!(report.underlying_id, chain.underlying_id);
    assert_eq!(report.scenario, DealerScenario::CallLongPutShort);
    assert_eq!(report.model_version, MODEL_VERSION);
    assert_eq!(report.as_of, time(VALUATION_TIME));
    assert_eq!(report.session_date, date(SESSION_DATE));
    assert_ne!(report.as_of.date_naive(), report.session_date);

    assert_totals(&report.all.totals, Some("6718.75"), Some("-156.25"));
    assert_totals(&report.zero_dte.totals, Some("3437.5"), Some("2812.5"));
    assert_totals(&report.later.totals, Some("3281.25"), Some("-2968.75"));

    let [strike_120, strike_130] = report.all.by_strike.as_slice() else {
        panic!("expected two numerical strikes");
    };
    assert_eq!(strike_120.strike.value(), decimal("120").value());
    assert_eq!(strike_130.strike.value(), decimal("130").value());
    assert_totals(&strike_120.totals, Some("4062.5"), Some("2187.5"));
    assert_totals(&strike_130.totals, Some("2656.25"), Some("-2343.75"));

    let [expires_today, expires_later] = report.all.by_expiry.as_slice() else {
        panic!("expected two expiry dates");
    };
    assert_eq!(expires_today.expiry, date(SESSION_DATE));
    assert_eq!(expires_later.expiry, date(LATER_EXPIRY));
    assert_totals(&expires_today.totals, Some("3437.5"), Some("2812.5"));
    assert_totals(&expires_later.totals, Some("3281.25"), Some("-2968.75"));
    assert_eq!(report.zero_dte.by_expiry.len(), 1);
    assert_eq!(report.zero_dte.by_expiry[0].expiry, date(SESSION_DATE));
    assert_eq!(report.later.by_expiry.len(), 1);
    assert_eq!(report.later.by_expiry[0].expiry, date(LATER_EXPIRY));

    assert_wall(&report.all.walls.largest_absolute, "120", "4062.5");
    assert_wall(&report.all.walls.largest_positive, "120", "2187.5");
    assert_wall(&report.all.walls.most_negative, "130", "-2343.75");
    assert_wall(&report.zero_dte.walls.largest_absolute, "120", "3437.5");
    assert_wall(&report.later.walls.most_negative, "130", "-2343.75");

    for (view, count, oi) in [
        (&report.all, 5, "29"),
        (&report.zero_dte, 2, "15"),
        (&report.later, 3, "14"),
    ] {
        assert!(view.coverage.universe_complete);
        assert!(!view.coverage.partial);
        assert_eq!(view.coverage.candidate_contracts, count);
        assert_eq!(view.coverage.valid_contracts, count);
        assert_eq!(
            view.coverage.eligible_open_interest.value(),
            decimal(oi).value()
        );
        assert_eq!(
            view.coverage
                .total_open_interest
                .as_ref()
                .map(DecimalText::value),
            amount(Some(oi))
        );
        assert_eq!(view.coverage.open_interest_dates, [date(OI_DATE)]);
        assert_eq!(view.coverage.unconfirmed_open_interest_dates, 0);
    }
}

#[test]
fn dealer_scenarios_reverse_sign_without_changing_concentration() {
    let chain = hand_chain();
    let short = run(&chain, DealerScenario::DealerShort);
    let long = run(&chain, DealerScenario::DealerLong);
    let unsigned = run(&chain, DealerScenario::Absolute);
    assert_totals(&short.all.totals, Some("6718.75"), Some("-6718.75"));
    assert_totals(&long.all.totals, Some("6718.75"), Some("6718.75"));
    assert_totals(&unsigned.all.totals, Some("6718.75"), None);
    for (short_row, long_row) in short.all.by_strike.iter().zip(&long.all.by_strike) {
        assert_eq!(short_row.strike, long_row.strike);
        assert_eq!(short_row.totals.absolute, long_row.totals.absolute);
        assert_eq!(
            short_row.totals.gex.as_ref().unwrap().value(),
            -long_row.totals.gex.as_ref().unwrap().value()
        );
    }
    assert_wall(&short.all.walls.most_negative, "120", "-4062.5");
    assert_wall(&long.all.walls.largest_positive, "120", "4062.5");
    assert_wall(&unsigned.all.walls.largest_absolute, "120", "4062.5");
    assert!(short.all.walls.largest_positive.is_none());
    assert!(long.all.walls.most_negative.is_none());
    assert!(unsigned.all.walls.largest_positive.is_none());
    assert!(unsigned.all.walls.most_negative.is_none());
}

#[test]
fn coverage_distinguishes_missing_zero_and_asynchronous_inputs() {
    use OptionRight::{Call, Put};

    let mut stale_gamma = contract(
        "STALE.XSIM",
        Call,
        SESSION_DATE,
        "140",
        "10",
        Some("7"),
        Some("0.03"),
    );
    stale_gamma.gamma = Some(observed("0.03", "2025-03-05T00:29:50Z"));
    stale_gamma.open_interest.as_mut().unwrap().business_date = None;
    let mut missing_gamma = contract(
        "NO-GAMMA.XSIM",
        Call,
        SESSION_DATE,
        "150",
        "10",
        Some("5"),
        None,
    );
    missing_gamma.open_interest.as_mut().unwrap().business_date = Some(date(SESSION_DATE));
    let mut chain = snapshot(vec![
        contract(
            "VALID.XSIM",
            Call,
            SESSION_DATE,
            "100",
            "100",
            Some("10"),
            Some("0.01"),
        ),
        contract(
            "NO-OI.XSIM",
            Put,
            SESSION_DATE,
            "110",
            "10",
            None,
            Some("0.01"),
        ),
        contract(
            "ZERO-OI.XSIM",
            Put,
            SESSION_DATE,
            "120",
            "10",
            Some("0"),
            Some("0.01"),
        ),
        stale_gamma,
        missing_gamma,
        contract(
            "ZERO-GAMMA.XSIM",
            Call,
            SESSION_DATE,
            "160",
            "10",
            Some("4"),
            Some("0"),
        ),
    ]);
    let report = run(&chain, DealerScenario::DealerLong);
    let coverage = &report.zero_dte.coverage;
    assert!(coverage.partial);
    assert_eq!(coverage.candidate_contracts, 6);
    assert_eq!(coverage.valid_contracts, 3);
    assert_eq!(
        coverage.eligible_open_interest.value(),
        decimal("14").value()
    );
    assert!(coverage.total_open_interest.is_none());
    assert_eq!(coverage.exclusions.missing_open_interest, 1);
    assert_eq!(coverage.exclusions.missing_gamma, 1);
    assert_eq!(coverage.exclusions.stale_gamma, 1);
    assert_eq!(
        coverage.open_interest_dates,
        [date(OI_DATE), date(SESSION_DATE)]
    );
    assert_eq!(coverage.unconfirmed_open_interest_dates, 1);
    assert_eq!(coverage.observed.earliest, time(OI_TIME));
    assert_eq!(coverage.observed.latest, time(VALUATION_TIME));
    assert_eq!(
        coverage.open_interest_observed.as_ref().unwrap().latest,
        time(OI_TIME)
    );
    assert_eq!(
        coverage.gamma_observed.as_ref().unwrap().latest,
        time(GREEK_TIME)
    );
    assert_eq!(coverage.max_oi_gamma_skew, Some(Duration::from_secs(3599)));
    assert_totals(&report.zero_dte.totals, Some("1562.5"), Some("1562.5"));
    let by_strike = &report.zero_dte.by_strike;
    assert_totals(&by_strike[1].totals, None, None);
    assert_totals(&by_strike[2].totals, Some("0"), Some("0"));
    assert_totals(&by_strike[5].totals, Some("0"), Some("0"));
    assert_totals(&report.later.totals, None, None);

    chain.contracts.remove(1);
    let known = run(&chain, DealerScenario::DealerLong);
    assert!(known.all.coverage.partial);
    assert_eq!(
        known
            .all
            .coverage
            .total_open_interest
            .as_ref()
            .map(DecimalText::value),
        amount(Some("26"))
    );
    chain.universe_complete = false;
    let incomplete = run(&chain, DealerScenario::DealerLong);
    assert!(incomplete.all.coverage.total_open_interest.is_none());
    assert!(incomplete.all.coverage.partial);
}

#[test]
fn spot_and_gamma_timing_are_independently_reported() {
    let mut chain = snapshot(vec![contract(
        "TIMING.XSIM",
        OptionRight::Call,
        SESSION_DATE,
        "120",
        "100",
        Some("2"),
        Some("0.01"),
    )]);
    chain.contracts[0].gamma = Some(observed("0.01", "2025-03-05T00:29:56Z"));
    let skewed = run(&chain, DealerScenario::DealerLong);
    assert_eq!(skewed.all.coverage.exclusions.spot_gamma_skew, 1);
    assert_eq!(skewed.all.coverage.exclusions.stale_gamma, 0);
    assert_totals(&skewed.all.totals, None, None);

    chain.spot.observed_at = time("2025-03-05T00:29:54Z");
    let stale_spot = run(&chain, DealerScenario::DealerLong);
    assert_eq!(stale_spot.all.coverage.exclusions.stale_spot, 1);
    assert_eq!(stale_spot.all.coverage.exclusions.spot_gamma_skew, 0);
    assert_eq!(stale_spot.all.coverage.valid_contracts, 0);

    chain.spot.observed_at = time("2025-03-05T00:29:55Z");
    let boundary = run(&chain, DealerScenario::DealerLong);
    assert_eq!(boundary.all.coverage.valid_contracts, 1);
    assert_totals(&boundary.all.totals, Some("312.5"), Some("312.5"));
}

fn flip_chain() -> (OptionChainSnapshot, GammaGrid) {
    use OptionRight::{Call, Put};

    let chain = snapshot(vec![
        contract(
            "C.XSIM",
            Call,
            SESSION_DATE,
            "120",
            "100",
            Some("1"),
            Some("0.03"),
        ),
        contract(
            "P.XSIM",
            Put,
            SESSION_DATE,
            "120",
            "100",
            Some("1"),
            Some("0.03"),
        ),
    ]);
    // On this hypothetical spot grid, C − P changes from negative to positive
    // at 110, then goes negative between 130 and 140. Current observed gamma is
    // equal for C/P, so a calculation that reuses it cannot find either flip.
    let grid = GammaGrid {
        as_of: chain.as_of,
        model: "synthetic spot gamma grid".parse().unwrap(),
        annual_risk_free_rate: decimal("0.0375"),
        annual_dividend_yield: decimal("0.0125"),
        spots: ["100", "110", "120", "130", "140", "150"]
            .into_iter()
            .map(decimal)
            .collect(),
        contracts: vec![
            ContractGammaGrid {
                instrument_id: chain.contracts[1].instrument_id.clone(),
                implied_volatility: decimal("0.28"),
                gammas: ["0.03", "0.02", "0.01", "0.04", "0.06", "0.06"]
                    .into_iter()
                    .map(decimal)
                    .collect(),
            },
            ContractGammaGrid {
                instrument_id: chain.contracts[0].instrument_id.clone(),
                implied_volatility: decimal("0.25"),
                gammas: ["0.01", "0.02", "0.03", "0.05", "0.05", "0.01"]
                    .into_iter()
                    .map(decimal)
                    .collect(),
            },
        ],
    };
    (chain, grid)
}

#[test]
fn flip_uses_revalued_gamma_and_all_negative_has_no_crossing() {
    let (chain, grid) = flip_chain();
    let report = evaluate(
        &chain,
        DealerScenario::CallLongPutShort,
        TIMING,
        Some(&grid),
    )
    .unwrap();
    assert_totals(&report.all.totals, Some("937.5"), Some("0"));
    assert_eq!(
        report.all.flip,
        GammaFlip::Crossings {
            crossings: vec![
                FlipCrossing {
                    lower_spot: decimal("110"),
                    upper_spot: decimal("110")
                },
                FlipCrossing {
                    lower_spot: decimal("130"),
                    upper_spot: decimal("140")
                },
            ]
        }
    );
    assert_eq!(report.zero_dte.flip, report.all.flip);
    assert_eq!(
        report.later.flip,
        GammaFlip::Unavailable {
            reason: FlipUnavailableReason::NoEligibleContracts
        }
    );
    let provenance = report.gamma_grid.unwrap();
    assert_eq!(provenance.as_of, chain.as_of);
    assert_eq!(provenance.model.as_str(), "synthetic spot gamma grid");
    assert_eq!(provenance.annual_risk_free_rate, decimal("0.0375"));
    assert_eq!(provenance.annual_dividend_yield, decimal("0.0125"));
    assert_eq!(provenance.spots, grid.spots);
    let ivs: Vec<_> = provenance
        .implied_volatilities
        .iter()
        .map(|row| (row.instrument_id.as_str(), row.implied_volatility.as_str()))
        .collect();
    assert_eq!(ivs, [("C.XSIM", "0.25"), ("P.XSIM", "0.28")]);

    let short = evaluate(&chain, DealerScenario::DealerShort, TIMING, Some(&grid)).unwrap();
    assert_totals(&short.all.totals, Some("937.5"), Some("-937.5"));
    assert_eq!(short.all.flip, GammaFlip::NoFlip);
    let absolute = evaluate(&chain, DealerScenario::Absolute, TIMING, Some(&grid)).unwrap();
    assert_eq!(
        absolute.all.flip,
        GammaFlip::Unavailable {
            reason: FlipUnavailableReason::NotDirectionalScenario
        }
    );
}

#[test]
fn incomplete_flip_grid_never_reports_no_flip() {
    let (mut chain, mut grid) = flip_chain();
    assert_eq!(
        run(&chain, DealerScenario::CallLongPutShort).all.flip,
        GammaFlip::Unavailable {
            reason: FlipUnavailableReason::NoGammaGrid
        }
    );
    grid.contracts.pop();
    let partial = evaluate(
        &chain,
        DealerScenario::CallLongPutShort,
        TIMING,
        Some(&grid),
    )
    .unwrap();
    assert_eq!(
        partial.all.flip,
        GammaFlip::Unavailable {
            reason: FlipUnavailableReason::MissingContractGamma
        }
    );
    assert_eq!(partial.all.coverage.valid_contracts, 2);
    assert_totals(&partial.all.totals, Some("937.5"), Some("0"));

    chain.contracts[0].expiry = date(LATER_EXPIRY);
    let split = evaluate(
        &chain,
        DealerScenario::CallLongPutShort,
        TIMING,
        Some(&grid),
    )
    .unwrap();
    assert_eq!(split.zero_dte.flip, GammaFlip::NoFlip);
    assert_eq!(split.all.flip, partial.all.flip);
    assert_eq!(split.later.flip, partial.all.flip);

    grid.spots[0] = decimal("110.00");
    assert!(matches!(
        evaluate(
            &chain,
            DealerScenario::CallLongPutShort,
            TIMING,
            Some(&grid)
        ),
        Err(ExposureError::InvalidGrid(_))
    ));
}

#[test]
fn zero_grid_points_require_a_direction_change_to_count_as_a_flip() {
    let (chain, mut grid) = flip_chain();
    grid.contracts[0].gammas.fill(decimal("0.03"));
    grid.contracts[1].gammas = ["0", "0.03", "0.03", "0.04", "0.04", "0.04"]
        .into_iter()
        .map(decimal)
        .collect();
    let plateau = evaluate(
        &chain,
        DealerScenario::CallLongPutShort,
        TIMING,
        Some(&grid),
    )
    .unwrap();
    assert_eq!(
        plateau.all.flip,
        GammaFlip::Crossings {
            crossings: vec![FlipCrossing {
                lower_spot: decimal("110"),
                upper_spot: decimal("120"),
            }],
        }
    );
    grid.contracts[1].gammas = ["0.03", "0.02", "0.03", "0.01", "0.02", "0.03"]
        .into_iter()
        .map(decimal)
        .collect();
    let touches = evaluate(
        &chain,
        DealerScenario::CallLongPutShort,
        TIMING,
        Some(&grid),
    )
    .unwrap();
    assert_eq!(touches.all.flip, GammaFlip::NoFlip);

    grid.contracts[1].gammas.fill(decimal("0.03"));
    let flat = evaluate(
        &chain,
        DealerScenario::CallLongPutShort,
        TIMING,
        Some(&grid),
    )
    .unwrap();
    assert_eq!(flat.all.flip, GammaFlip::NoFlip);
}

#[test]
fn walls_use_numeric_order_for_ties_and_output_is_deterministic() {
    let mut chain = snapshot(vec![
        contract(
            "HIGH.XSIM",
            OptionRight::Call,
            SESSION_DATE,
            "1000",
            "12.5",
            Some("5"),
            Some("0.02"),
        ),
        contract(
            "LOW.XSIM",
            OptionRight::Put,
            LATER_EXPIRY,
            "99",
            "25",
            Some("5"),
            Some("0.01"),
        ),
    ]);
    let forward = run(&chain, DealerScenario::DealerShort);
    // Both rows yield 5 × 0.25 × 156.25 = 195.3125.
    assert_wall(&forward.all.walls.largest_absolute, "99", "195.3125");
    assert_wall(&forward.all.walls.most_negative, "99", "-195.3125");
    chain.contracts.reverse();
    let reverse = run(&chain, DealerScenario::DealerShort);
    assert_eq!(
        serde_json::to_string(&forward).unwrap(),
        serde_json::to_string(&reverse).unwrap()
    );
}

#[test]
fn rejects_invalid_snapshots_and_preserves_decimal_text() {
    let mut chain = snapshot(vec![contract(
        "EXACT.XSIM",
        OptionRight::Call,
        SESSION_DATE,
        "125",
        "1",
        Some("1"),
        Some("0.010000000000000001"),
    )]);
    chain.spot.value = decimal("100");
    let report = run(&chain, DealerScenario::DealerLong);
    assert_totals(
        &report.all.totals,
        Some("1.0000000000000001"),
        Some("1.0000000000000001"),
    );
    let encoded = serde_json::to_value(&report).unwrap();
    assert_eq!(
        encoded["all"]["totals"]["gex"].as_str(),
        Some("1.0000000000000001")
    );

    let invalid = |snapshot: &OptionChainSnapshot| {
        assert!(evaluate(snapshot, DealerScenario::DealerLong, TIMING, None).is_err());
    };
    chain.contracts.push(chain.contracts[0].clone());
    invalid(&chain);
    chain.contracts.pop();
    chain.contracts[0].expiry = date(OI_DATE);
    invalid(&chain);
    chain.contracts[0].expiry = date(SESSION_DATE);
    chain.contracts[0].open_interest.as_mut().unwrap().value = decimal("1.5");
    invalid(&chain);
    chain.contracts[0].open_interest.as_mut().unwrap().value = decimal("1");
    chain.contracts[0].gamma = Some(observed("-0.01", GREEK_TIME));
    invalid(&chain);
    chain.contracts[0].gamma = Some(observed("0.01", "2025-03-05T00:30:01Z"));
    invalid(&chain);
    chain.contracts[0].gamma = Some(observed("1", GREEK_TIME));
    chain.contracts[0].open_interest.as_mut().unwrap().value =
        decimal("9999999999999999999999999999");
    assert!(matches!(
        evaluate(&chain, DealerScenario::DealerLong, TIMING, None),
        Err(ExposureError::Arithmetic(_))
    ));
}

#[test]
fn exact_products_accept_large_intermediates_and_reject_unrepresentable_results() {
    let mut chain = snapshot(vec![contract(
        "SCALES.XSIM",
        OptionRight::Call,
        SESSION_DATE,
        "125",
        "1000000000000000000000000000",
        Some("1000000000000000000000000000"),
        Some("0.000000000000000000000000001"),
    )]);
    chain.spot.value = decimal("1");
    let exact = run(&chain, DealerScenario::DealerLong);
    assert_totals(
        &exact.all.totals,
        Some("10000000000000000000000000"),
        Some("10000000000000000000000000"),
    );

    chain.contracts[0].multiplier = decimal("0.000000000000000000000000001");
    chain.contracts[0].open_interest.as_mut().unwrap().value = decimal("1");
    assert!(matches!(
        evaluate(&chain, DealerScenario::DealerLong, TIMING, None),
        Err(ExposureError::Arithmetic(_))
    ));
}

#[test]
fn empty_and_real_zero_have_distinct_exposures() {
    let chain = snapshot(Vec::new());
    let empty = run(&chain, DealerScenario::DealerLong);
    assert_eq!(empty.all.coverage.valid_contracts, 0);
    assert_totals(&empty.all.totals, None, None);

    let chain = snapshot(vec![contract(
        "ZERO.XSIM",
        OptionRight::Call,
        SESSION_DATE,
        "125",
        "25",
        Some("0"),
        Some("0.03"),
    )]);
    let zero = run(&chain, DealerScenario::DealerLong);
    assert_eq!(zero.all.coverage.valid_contracts, 1);
    assert_eq!(zero.all.coverage.exclusions.missing_open_interest, 0);
    assert_totals(&zero.all.totals, Some("0"), Some("0"));
    assert_eq!(
        zero.all
            .coverage
            .total_open_interest
            .as_ref()
            .map(DecimalText::value),
        amount(Some("0"))
    );
}
