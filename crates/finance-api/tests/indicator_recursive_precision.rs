use chrono::{DateTime, Duration};
use finance_api::indicators::{Bar, IndicatorEngine, IndicatorSpec, IndicatorValue, batch};
use rust_decimal::Decimal;
use serde_json::{Value, json};

fn spec(name: &str, params: Value) -> IndicatorSpec {
    IndicatorSpec::new(&format!("indicator.{name}@1.0.0"), &params).unwrap()
}

fn bar(index: i64, price: &str) -> Bar {
    let price = Decimal::from_str_exact(price).unwrap();
    let open_ts = DateTime::from_timestamp(1_767_312_000 + index * 60, 0).unwrap();
    Bar {
        open_ts,
        end_ts: open_ts + Duration::minutes(1),
        trading_day: open_ts.date_naive(),
        open: price,
        high: price,
        low: price,
        close: price,
        volume: Decimal::ONE,
    }
}

fn all_paths(specs: &[IndicatorSpec], bars: &[Bar]) -> Vec<Vec<IndicatorValue>> {
    let expected = batch(specs, bars).unwrap();
    for restart in 0..bars.len() {
        let mut engine = IndicatorEngine::new(specs);
        for bar in &bars[..restart] {
            engine.advance_closed(bar).unwrap();
        }
        for (bar, row) in bars[restart..].iter().zip(&expected[restart..]) {
            assert_eq!(&engine.push_closed(bar).unwrap(), row);
        }
    }
    expected
}

#[test]
fn unit_weight_averages_keep_the_new_sample_across_scales() {
    let large = "100000000000000000";
    let small = "0.000000000001";
    let bars = [bar(0, large), bar(1, small), bar(2, large), bar(3, small)];
    let ema = spec("ema", json!({"period": 1}));
    for (row, bar) in all_paths(&[ema], &bars).iter().zip(bars) {
        assert_eq!(row[0].values["value"].to_string(), bar.close.to_string());
    }

    let first = Bar {
        high: bars[0].high,
        ..bar(0, "0")
    };
    let second = Bar {
        low: Decimal::ZERO,
        ..bar(1, small)
    };
    let atr = spec("atr", json!({"period": 1}));
    let rows = all_paths(&[atr], &[first, second]);
    assert_eq!(rows[0][0].values["value"].to_string(), large);
    assert_eq!(rows[1][0].values["value"].to_string(), small);
}

#[test]
fn ema_and_macd_keep_unrounded_state_between_weighted_updates() {
    let bars = [
        bar(0, "100000000000000000"),
        bar(1, "0.000000000002"),
        bar(2, "0.000000000002"),
    ];
    let specs = [
        spec("ema", json!({"period": 2})),
        spec(
            "macd",
            json!({"fast_period": 1, "slow_period": 2, "signal_period": 1}),
        ),
    ];
    let rows = all_paths(&specs, &bars);
    assert!(rows[0].iter().all(|value| value.values.is_empty()));
    for (index, ema, line) in [
        (
            1,
            "50000000000000000.000000000001",
            "-49999999999999999.999999999999",
        ),
        (
            2,
            "16666666666666666.666666666668",
            "-16666666666666666.666666666666",
        ),
    ] {
        assert_eq!(rows[index][0].values["value"].to_string(), ema);
        assert_eq!(rows[index][1].values["macd"].to_string(), line);
        assert_eq!(rows[index][1].values["signal"].to_string(), line);
        assert_eq!(rows[index][1].values["histogram"], Decimal::ZERO);
    }
}
