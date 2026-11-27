use chrono::{DateTime, Duration};
use finance_api::indicators::{Bar, IndicatorEngine, IndicatorSpec, batch};
use rust_decimal::Decimal;
use serde_json::json;

fn rsi_spec() -> IndicatorSpec {
    IndicatorSpec::new(
        "indicator.rsi_wilder@1.0.0",
        &json!({"period": 2, "field": "close"}),
    )
    .unwrap()
}

fn bars(closes: impl IntoIterator<Item = Decimal>) -> Vec<Bar> {
    closes
        .into_iter()
        .enumerate()
        .map(|(index, close)| {
            let open_ts = DateTime::from_timestamp(1_767_312_000 + index as i64 * 60, 0).unwrap();
            Bar {
                open_ts,
                end_ts: open_ts + Duration::minutes(1),
                trading_day: open_ts.date_naive(),
                open: close,
                high: close,
                low: close,
                close,
                volume: Decimal::ONE,
            }
        })
        .collect()
}

fn prices_with_flat_tail(length: usize) -> impl Iterator<Item = Decimal> {
    [100, 103, 102]
        .map(Decimal::from)
        .into_iter()
        .chain(std::iter::repeat_n(Decimal::from(102), length))
}

#[test]
fn wilder_rsi_preserves_its_ratio_on_a_long_flat_run() {
    // Seed period-scaled gains and losses at 3 and 1. Flat bars scale
    // both by 1/2, so the ratio is 75 even after hundreds of halvings.
    let bars = bars(prices_with_flat_tail(256));
    let values = batch(&[rsi_spec()], &bars).unwrap();
    for (index, row) in values.iter().enumerate().skip(2) {
        assert_eq!(row[0].values["value"], Decimal::from(75), "bar {index}");
    }
}

#[test]
fn wilder_rsi_keeps_decayed_mass_when_a_tiny_price_move_ends_a_flat_run() {
    let step_up = Decimal::from_str_exact("102.00000000000000000000000001").unwrap();
    let bars = bars(prices_with_flat_tail(87).chain([step_up, Decimal::from(102)]));
    let specs = [rsi_spec()];
    let expected = batch(&specs, &bars).unwrap();
    let resumed = bars.len() - 2;
    // After 87 flat bars and one further Wilder update, the old totals
    // contribute 3*2^-88 and 2^-88 alongside the new gain of 1e-26.
    assert_eq!(
        expected[resumed][0].values["value"].to_string(),
        "85.905269510179"
    );
    assert_eq!(
        expected[resumed + 1][0].values["value"].to_string(),
        "45.879235375189"
    );

    let mut incremental = IndicatorEngine::new(&specs);
    for (bar, expected) in bars.iter().zip(&expected) {
        assert_eq!(incremental.push_closed(bar).unwrap(), *expected);
    }
    let mut restarted = IndicatorEngine::new(&specs);
    for bar in &bars[..resumed] {
        restarted.advance_closed(bar).unwrap();
    }
    for (bar, expected) in bars[resumed..].iter().zip(&expected[resumed..]) {
        assert_eq!(restarted.push_closed(bar).unwrap(), *expected);
    }
}
