use chrono::{DateTime, Duration, NaiveDate};
use finance_api::indicators::{
    Bar, IndicatorEngine, IndicatorError, IndicatorSpec, IndicatorValue, batch,
};
use rust_decimal::Decimal;
use serde_json::{Value, json};

fn spec(name: &str, params: Value) -> IndicatorSpec {
    IndicatorSpec::new(&format!("indicator.{name}@1.0.0"), &params).unwrap()
}

fn decimal(text: &str) -> Decimal {
    Decimal::from_str_exact(text).unwrap()
}

fn bar(index: i64, price: Decimal, volume: Decimal) -> Bar {
    let open_ts = DateTime::from_timestamp(1_767_312_000 + index * 60, 0).unwrap();
    Bar {
        open_ts,
        end_ts: open_ts + Duration::minutes(1),
        trading_day: NaiveDate::from_ymd_opt(2026, 1, 2).unwrap(),
        open: price,
        high: price,
        low: price,
        close: price,
        volume,
    }
}

fn matching_paths(specs: &[IndicatorSpec], bars: &[Bar]) -> Vec<Vec<IndicatorValue>> {
    let expected = batch(specs, bars).unwrap();
    let mut incremental = IndicatorEngine::new(specs);
    for (bar, point) in bars.iter().zip(&expected) {
        assert_eq!(&incremental.push_closed(bar).unwrap(), point);
    }

    for restart_at in 0..bars.len() {
        let mut restarted = IndicatorEngine::new(specs);
        for bar in &bars[..restart_at] {
            restarted.advance_closed(bar).unwrap();
        }
        for (bar, point) in bars[restart_at..].iter().zip(&expected[restart_at..]) {
            assert_eq!(&restarted.push_closed(bar).unwrap(), point);
        }
    }
    expected
}

#[test]
fn moving_means_keep_small_observations_after_large_value_expires() {
    // Synthetic close/volume pairs from the review's cross-scale vector.
    let large = decimal("100000000000000000");
    let small = decimal("0.000000000001");
    let bars = [
        bar(0, large, large),
        bar(1, small, small),
        bar(2, small, small),
    ];
    let specs = [
        spec("sma", json!({"period":2})),
        spec("volume_sma", json!({"period":2})),
    ];
    let points = matching_paths(&specs, &bars);
    for (point, expected) in [
        (&points[1], "50000000000000000"),
        (&points[2], "0.000000000001"),
    ] {
        for value in point {
            assert_eq!(value.values["value"].to_string(), expected);
        }
    }
}

#[test]
fn vwap_keeps_a_small_weight_with_a_large_price() {
    // The second bar's normalized weight is below Decimal's minimum quantum,
    // but its contribution to the final average rounds to 0.01.
    let large = decimal("1000000000000000000000000000");
    let bars = [bar(0, Decimal::ZERO, large), bar(1, large, decimal("0.01"))];
    let points = matching_paths(&[spec("vwap", json!({}))], &bars);
    assert_eq!(points[0][0].values["value"].to_string(), "0");
    assert_eq!(points[1][0].values["value"].to_string(), "0.01");
}

#[test]
fn exact_averages_fail_when_the_rounded_coefficient_cannot_fit_decimal() {
    let specs = [spec("sma", json!({"period":2}))];
    let first = bar(0, Decimal::MAX, Decimal::ONE);
    let second = bar(1, Decimal::MAX - Decimal::ONE, Decimal::ONE);
    let error = Err(IndicatorError::NumericOverflow);
    assert_eq!(batch(&specs, &[first, second]), error);

    let mut engine = IndicatorEngine::new(&specs);
    engine.advance_closed(&first).unwrap();
    assert_eq!(
        engine.advance_closed(&second),
        Err(IndicatorError::NumericOverflow)
    );
    assert_eq!(
        engine.push_closed(&second),
        Err(IndicatorError::NumericOverflow)
    );
    let retry = bar(1, Decimal::MAX, Decimal::ONE);
    assert_eq!(
        engine.push_closed(&retry).unwrap()[0].values["value"],
        Decimal::MAX
    );
}
