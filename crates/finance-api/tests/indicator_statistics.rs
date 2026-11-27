use chrono::{DateTime, Duration, NaiveDate};
use finance_api::indicators::{
    Bar, IndicatorEngine, IndicatorError, IndicatorSpec, IndicatorValue, batch,
};
use rust_decimal::Decimal;
use serde_json::json;

fn spec(name: &str, period: usize) -> IndicatorSpec {
    IndicatorSpec::new(
        &format!("indicator.{name}@1.0.0"),
        &json!({"period": period}),
    )
    .unwrap()
}

fn bars(values: &[&str]) -> Vec<Bar> {
    values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let close = Decimal::from_str_exact(value).unwrap();
            let open_ts = DateTime::from_timestamp(1_767_312_000 + index as i64 * 60, 0).unwrap();
            Bar {
                open_ts,
                end_ts: open_ts + Duration::minutes(1),
                trading_day: NaiveDate::from_ymd_opt(2026, 1, 2).unwrap(),
                open: close,
                high: close,
                low: close,
                close,
                volume: Decimal::ONE,
            }
        })
        .collect()
}

fn matching_paths(specs: &[IndicatorSpec], bars: &[Bar]) -> Vec<Vec<IndicatorValue>> {
    let rows = batch(specs, bars).unwrap();
    let mut live = IndicatorEngine::new(specs);
    for (bar, expected) in bars.iter().zip(&rows) {
        assert_eq!(&live.push_closed(bar).unwrap(), expected);
    }
    for restart in 1..bars.len() {
        let mut replay = IndicatorEngine::new(specs);
        for bar in &bars[..restart] {
            replay.advance_closed(bar).unwrap();
        }
        for (bar, expected) in bars[restart..].iter().zip(&rows[restart..]) {
            assert_eq!(&replay.push_closed(bar).unwrap(), expected);
        }
    }
    rows
}

#[test]
fn bollinger_middle_matches_sma_for_the_mixed_scale_window() {
    let specs = [spec("sma", 2), spec("bollinger", 2)];
    let bars = bars(&["100000000000000000", "0.000000000002"]);
    let rows = matching_paths(&specs, &bars);
    assert_eq!(rows[1][1].values["middle"], rows[1][0].values["value"]);
    assert_eq!(
        rows[1][1].values["middle"].to_string(),
        "50000000000000000.000000000001"
    );
    // For two observations 2σ is exactly the absolute price difference.
    assert_eq!(rows[1][1].values["upper"].to_string(), "150000000000000000");
    assert_eq!(
        rows[1][1].values["lower"].to_string(),
        "-49999999999999999.999999999997"
    );
}

#[test]
fn bollinger_rails_round_once_at_the_finest_encodable_scale() {
    let specs = [spec("bollinger", 2)];
    // The exact upper value is 150000000000000000 - 0.0000000000051.
    // Rounding first at 12 places would manufacture a tie when reducing to 11.
    let positive = bars(&["100000000000000000", "0.0000000000102"]);
    let negative = bars(&["-100000000000000000", "-0.0000000000102"]);
    let upper = matching_paths(&specs, &positive)[1][0].values["upper"];
    let lower = matching_paths(&specs, &negative)[1][0].values["lower"];
    assert_eq!(upper.to_string(), "149999999999999999.99999999999");
    assert_eq!(lower, -upper);
}

#[test]
fn volatility_rounds_wide_deviation_and_rejects_overwide_per_bar_returns() {
    let specs = [spec("realized_volatility", 3)];
    let tiny = "0.000000000000000000000000001";
    let inputs = bars(&[tiny, "70", tiny, tiny]);
    let rows = matching_paths(&specs, &inputs);
    assert_eq!(
        rows[3][0].values["value"].to_string(),
        "40414518843273803515640414635"
    );

    let outside = bars(&[tiny, "80"]);
    assert_eq!(
        batch(&specs, &outside),
        Err(IndicatorError::NumericOverflow)
    );
    let mut replay = IndicatorEngine::new(&specs);
    replay.advance_closed(&outside[0]).unwrap();
    assert_eq!(
        replay.advance_closed(&outside[1]),
        Err(IndicatorError::NumericOverflow)
    );
    assert_eq!(
        replay.push_closed(&outside[1]),
        Err(IndicatorError::NumericOverflow)
    );
}
