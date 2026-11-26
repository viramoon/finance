use chrono::{DateTime, Duration, NaiveDate};
use finance_api::indicators::{
    Bar, IndicatorEngine, IndicatorError, IndicatorSpec, IndicatorValue, ValueStatus, batch,
    registry,
};
use finance_market_protocol::{Event, Message, decode_json};
use rust_decimal::Decimal;
use serde_json::{Value, json};

fn specs() -> Vec<IndicatorSpec> {
    [
        ("sma", json!({"period": 3})),
        ("ema", json!({"period": 3})),
        ("rsi_wilder", json!({"period": 2, "field": "close"})),
        ("bollinger", json!({"period": 3})),
        (
            "macd",
            json!({"fast_period": 2, "slow_period": 3, "signal_period": 2}),
        ),
        ("atr", json!({"period": 2})),
        ("vwap", json!({})),
        ("close_return", json!({"period": 2})),
        ("realized_volatility", json!({"period": 2})),
        ("volume_sma", json!({"period": 3})),
    ]
    .into_iter()
    .map(|(name, params)| spec(name, params))
    .collect()
}

fn spec(name: &str, params: Value) -> IndicatorSpec {
    IndicatorSpec::new(&format!("indicator.{name}@1.0.0"), &params).unwrap()
}

fn decimal(text: &str) -> Decimal {
    Decimal::from_str_exact(text).unwrap()
}

fn bar(index: i64, close: impl Into<Decimal>, volume: impl Into<Decimal>) -> Bar {
    let close = close.into();
    let open_ts = DateTime::from_timestamp(1_767_312_000 + index * 60, 0).unwrap();
    Bar {
        open_ts,
        end_ts: open_ts + Duration::minutes(1),
        trading_day: NaiveDate::from_ymd_opt(2026, 1, 2).unwrap(),
        open: close,
        high: close + Decimal::ONE,
        low: close - Decimal::ONE,
        close,
        volume: volume.into(),
    }
}

fn assert_values(value: &IndicatorValue, expected: &[(&str, &str)]) {
    assert_eq!(value.status, ValueStatus::Ready);
    assert_eq!(value.values.len(), expected.len());
    for (key, text) in expected {
        assert_eq!(value.values[key], decimal(text), "{key}");
        assert_eq!(value.values[key].to_string(), *text, "{key}");
    }
}

#[test]
fn all_registered_indicators_match_known_values_and_declared_warmup() {
    let specs = specs();
    assert_eq!(registry().len(), specs.len());
    let bars = [1, 2, 4, 3, 5, 8]
        .into_iter()
        .enumerate()
        .map(|(row, price)| bar(row as i64, price, row + 1))
        .collect::<Vec<_>>();
    let rows = batch(&specs, &bars).unwrap();
    for (column, spec) in specs.iter().enumerate() {
        let first = rows
            .iter()
            .position(|row| row[column].status != ValueStatus::Warmup);
        assert_eq!(
            first,
            Some(spec.prior_bars() as usize),
            "{}",
            spec.definition().id
        );
        for value in rows.iter().map(|row| &row[column]) {
            assert!(!value.provisional);
            if value.status != ValueStatus::Ready {
                assert!(value.values.is_empty());
            }
        }
    }
    assert_values(&rows[2][0], &[("value", "2.333333333333")]);
    assert_values(&rows[2][1], &[("value", "2.333333333333")]);
    assert_values(&rows[3][1], &[("value", "2.666666666667")]);
    assert_values(&rows[2][2], &[("value", "100")]);
    assert_values(&rows[3][2], &[("value", "60")]);
    assert_values(&rows[4][2], &[("value", "84.615384615385")]);
    assert_values(
        &rows[2][3],
        &[
            ("middle", "2.333333333333"),
            ("upper", "4.827771591183"),
            ("lower", "-0.161104924516"),
        ],
    );
    assert_values(
        &rows[3][4],
        &[
            ("macd", "0.388888888889"),
            ("signal", "0.611111111111"),
            ("histogram", "-0.222222222222"),
        ],
    );
    assert_values(&rows[2][5], &[("value", "2.5")]);
    assert_values(&rows[3][5], &[("value", "2.25")]);
    assert_values(&rows[2][6], &[("value", "2.833333333333")]);
    assert_values(&rows[2][7], &[("value", "3")]);
    assert_values(&rows[3][7], &[("value", "0.5")]);
    assert_values(&rows[2][8], &[("value", "0")]);
    assert_values(&rows[3][8], &[("value", "0.883883476483")]);
    assert_values(&rows[2][9], &[("value", "2")]);
    assert_values(&rows[3][9], &[("value", "3")]);
}

fn synthetic_event(seq: i64) -> String {
    // Deterministic arithmetic fixture with seed 7, twenty price steps, two
    // separated trading days and a missing minute. No external market inputs.
    let minute = seq + i64::from(seq >= 7) + if seq >= 13 { 1440 } else { 0 };
    let open = 1_767_312_000_000_000_000_i64 + minute * 60_000_000_000;
    let end = open + 60_000_000_000;
    let close = 100 + (seq * 7 + 3).rem_euclid(19);
    json!({
        "schema_version":"1.0", "channel":"bars", "source":"fixture", "product":"synthetic-bars",
        "instrument_id":"SYNTH.XNYS", "asset_kind":"stock", "venue":"XNYS",
        "exchange_ts":end.to_string(), "receive_ts":end.to_string(), "first_available_ts":end.to_string(),
        "epoch":"1", "seq":seq.to_string(), "cursor":format!("fixture:{seq}"), "revision":"0",
        "license":"fixture.synthetic", "kind":"bar_closed", "payload":{
            "open_ts":open.to_string(), "open":close.to_string(), "high":(close+2).to_string(),
            "low":(close-1).to_string(), "close":close.to_string(), "volume":format!("{}.125",seq % 5),
        },
    }).to_string()
}

fn replay_bar(event: &str) -> Bar {
    let event: Event = decode_json(event).unwrap();
    let Message::BarClosed(payload) = event.message else {
        panic!("expected a synthetic closed bar");
    };
    let open_ts = DateTime::from_timestamp_nanos(payload.open_ts.get().try_into().unwrap());
    let end_ts =
        DateTime::from_timestamp_nanos(event.exchange_ts.unwrap().get().try_into().unwrap());
    Bar {
        open_ts,
        end_ts,
        trading_day: open_ts.date_naive(),
        open: payload.open.value(),
        high: payload.high.value(),
        low: payload.low.value(),
        close: payload.close.value(),
        volume: payload.volume.value(),
    }
}

#[test]
fn synthetic_f1_events_match_batch_incremental_and_every_restart_point() {
    let specs = specs();
    let events: Vec<_> = (0..20).map(synthetic_event).collect();
    let bars: Vec<_> = events.iter().map(|event| replay_bar(event)).collect();
    let expected = batch(&specs, &bars).unwrap();
    let mut live = IndicatorEngine::new(&specs);
    for (index, event) in events.iter().enumerate() {
        assert_eq!(
            live.push_closed(&replay_bar(event)).unwrap(),
            expected[index]
        );
    }
    for restart_at in 0..events.len() {
        let mut restarted = IndicatorEngine::new(&specs);
        for event in &events[..restart_at] {
            restarted.advance_closed(&replay_bar(event)).unwrap();
        }
        for (event, value) in events[restart_at..].iter().zip(&expected[restart_at..]) {
            assert_eq!(restarted.push_closed(&replay_bar(event)).unwrap(), *value);
        }
    }
}

#[test]
fn small_valid_quantities_and_price_changes_do_not_underflow_indicator_state() {
    let vwap = spec("vwap", json!({}));
    let tiny_lot = decimal("0.000000000000000000000000001");
    let vwap_rows = batch(
        &[vwap],
        &[
            bar(0, decimal("0.01"), tiny_lot),
            bar(1, decimal("0.03"), tiny_lot),
        ],
    )
    .unwrap();
    assert_values(&vwap_rows[0][0], &[("value", "0.01")]);
    assert_values(&vwap_rows[1][0], &[("value", "0.02")]);

    let rsi = spec("rsi_wilder", json!({"period": 100, "field": "close"}));
    let bars: Vec<_> = (0..=100)
        .map(|index| {
            let close = if index == 0 {
                Decimal::ONE
            } else {
                Decimal::ONE + tiny_lot
            };
            bar(index, close, 1)
        })
        .collect();
    let rsi_rows = batch(&[rsi], &bars).unwrap();
    assert_eq!(rsi_rows[99][0].status, ValueStatus::Warmup);
    assert_values(&rsi_rows[100][0], &[("value", "100")]);
}

#[test]
fn provisional_revisions_do_not_commit_and_warmup_is_independent() {
    let specs = specs();
    let mut engine = IndicatorEngine::new(&specs);
    let [first, second, third, fourth] =
        std::array::from_fn(|index| bar(index as i64, 100 + index, 2 + index));
    let first_preview = engine.preview(&first).unwrap();
    assert_eq!(first_preview[0].status, ValueStatus::Warmup);
    assert_eq!(first_preview[6].status, ValueStatus::Ready);
    assert!(first_preview.iter().all(|value| value.provisional));
    engine.push_closed(&first).unwrap();
    engine.push_closed(&second).unwrap();
    let old = engine.preview(&bar(2, 170, 4)).unwrap();
    let new = engine.preview(&third).unwrap();
    assert_ne!(old[0].values, new[0].values);
    let closed = engine.push_closed(&third).unwrap();
    for (preview, closed) in new.iter().zip(closed) {
        assert_eq!(preview.status, closed.status);
        assert_eq!(preview.values, closed.values);
        assert!(preview.provisional);
        assert!(!closed.provisional);
    }
    assert_eq!(
        engine.push_closed(&fourth).unwrap(),
        batch(&specs, &[first, second, third, fourth]).unwrap()[3]
    );
}

#[test]
fn zero_base_and_flat_series_are_undefined_without_poisoning_future_windows() {
    let specs = [
        spec("rsi_wilder", json!({"period": 2, "field": "close"})),
        spec("close_return", json!({"period": 1})),
        spec("realized_volatility", json!({"period": 2})),
    ];
    let closes = [0, 0, 0, 2, 1, 1];
    let bars: Vec<_> = closes
        .into_iter()
        .enumerate()
        .map(|(index, close)| bar(index as i64, close, 1))
        .collect();
    let rows = batch(&specs, &bars).unwrap();
    assert_eq!(rows[0][1].status, ValueStatus::Warmup);
    assert!(
        rows[2]
            .iter()
            .all(|value| value.status == ValueStatus::Undefined && value.values.is_empty())
    );
    assert_values(&rows[3][0], &[("value", "100")]);
    assert_values(&rows[4][1], &[("value", "-0.5")]);
    assert_eq!(rows[4][2].status, ValueStatus::Undefined);
    assert_values(&rows[5][2], &[("value", "0.353553390593")]);
}

#[test]
fn vwap_uses_exchange_trading_day_and_missing_bars_are_skipped() {
    let specs = [spec("vwap", json!({})), spec("sma", json!({"period": 2}))];
    let first = Bar {
        high: decimal("20"),
        low: decimal("8"),
        ..bar(0, 11, 2)
    };
    let second = bar(13, 23, 4);
    let midnight = 24 * 60;
    let midnight_same_session = bar(midnight, 77, 0);
    let next_session = Bar {
        trading_day: first.trading_day.succ_opt().unwrap(),
        ..bar(midnight + 1, 70, 0)
    };
    let last = Bar {
        trading_day: next_session.trading_day,
        ..bar(midnight + 2, 41, 5)
    };
    let rows = batch(
        &specs,
        &[first, second, midnight_same_session, next_session, last],
    )
    .unwrap();
    assert_values(&rows[0][0], &[("value", "13")]);
    assert_values(&rows[1][0], &[("value", "19.666666666667")]);
    assert_values(&rows[1][1], &[("value", "17")]);
    assert_eq!(rows[2][0], rows[1][0]);
    assert_eq!(rows[3][0].status, ValueStatus::Undefined);
    assert_values(&rows[4][0], &[("value", "41")]);
}

#[test]
fn decimal_ties_and_tiny_deviation_keep_the_advertised_precision() {
    let specs = [
        spec("sma", json!({"period": 1})),
        spec("bollinger", json!({"period": 2})),
    ];
    let rows = batch(
        &specs,
        &[
            bar(0, decimal("1.0000000000002"), 1),
            bar(1, decimal("1.0000000000008"), 1),
            bar(2, decimal("-1.0000000000015"), 1),
        ],
    )
    .unwrap();
    assert_values(
        &rows[1][1],
        &[("middle", "1"), ("upper", "1.000000000001"), ("lower", "1")],
    );
    assert_values(&rows[2][0], &[("value", "-1.000000000002")]);
}

#[test]
fn invalid_bars_and_overflow_leave_engine_at_the_last_commit() {
    let specs = [
        spec("sma", json!({"period": 1})),
        spec("ema", json!({"period": 2})),
    ];
    let mut engine = IndicatorEngine::new(&specs);
    let first = bar(0, 10, 1);
    engine.push_closed(&first).unwrap();
    assert_eq!(engine.push_closed(&first), Err(IndicatorError::OutOfOrder));
    let invalid = Bar {
        volume: Decimal::NEGATIVE_ONE,
        ..bar(1, 20, 1)
    };
    assert_eq!(engine.preview(&invalid), Err(IndicatorError::InvalidBar));
    let regressed = Bar {
        trading_day: first.trading_day.pred_opt().unwrap(),
        ..bar(1, 20, 1)
    };
    assert_eq!(
        engine.push_closed(&regressed),
        Err(IndicatorError::OutOfOrder)
    );
    let extreme = Bar {
        open: Decimal::MAX,
        high: Decimal::MAX,
        low: Decimal::MAX,
        close: Decimal::MAX,
        ..bar(1, 1, 1)
    };
    assert_eq!(
        engine.push_closed(&extreme),
        Err(IndicatorError::NumericOverflow)
    );
    let second = bar(1, 20, 1);
    assert_eq!(
        engine.push_closed(&second).unwrap(),
        batch(&specs, &[first, second]).unwrap()[1]
    );
}

#[test]
fn window_overflows_fail_atomically_during_batch_incremental_and_restart() {
    let tiny = decimal("0.000000000000000000000000001");
    let cases = [
        (
            spec("close_return", json!({"period": 1})),
            vec![tiny, Decimal::ONE_HUNDRED],
        ),
        (
            spec("bollinger", json!({"period": 2})),
            vec![Decimal::ZERO, Decimal::MAX],
        ),
        (
            spec("realized_volatility", json!({"period": 3})),
            vec![tiny, Decimal::from(70), tiny, Decimal::from(70)],
        ),
    ];
    for (overflow_spec, closes) in cases {
        let name = overflow_spec.definition().id;
        let specs = [spec("sma", json!({"period": 1})), overflow_spec];
        let mut bars: Vec<_> = closes
            .into_iter()
            .enumerate()
            .map(|(index, close)| Bar {
                open: close,
                high: close,
                low: close,
                close,
                ..bar(index as i64, 1, 1)
            })
            .collect();
        let last = bars.pop().unwrap();
        let mut live = IndicatorEngine::new(&specs);
        let mut restarted = IndicatorEngine::new(&specs);
        for bar in &bars {
            live.push_closed(bar).unwrap();
            restarted.advance_closed(bar).unwrap();
        }
        assert_eq!(
            batch(&specs, bars.iter().chain([&last])),
            Err(IndicatorError::NumericOverflow),
            "{name} batch"
        );
        assert_eq!(
            live.push_closed(&last),
            Err(IndicatorError::NumericOverflow),
            "{name} incremental"
        );
        assert_eq!(
            restarted.advance_closed(&last),
            Err(IndicatorError::NumericOverflow),
            "{name} restart"
        );
        assert_eq!(
            restarted.preview(&last),
            Err(IndicatorError::NumericOverflow),
            "{name} preview"
        );

        let close = bars.last().unwrap().close;
        let corrected = Bar {
            open: close,
            high: close,
            low: close,
            close,
            ..last
        };
        let expected = batch(&specs, bars.iter().chain([&corrected])).unwrap();
        assert_eq!(
            &live.push_closed(&corrected).unwrap(),
            expected.last().unwrap()
        );
        assert_eq!(
            &restarted.push_closed(&corrected).unwrap(),
            expected.last().unwrap()
        );
    }
}

#[test]
fn supported_input_scale_accepts_boundaries_and_trailing_zeros() {
    let smallest = Decimal::new(1, 27);
    let padded = Decimal::new(10, 28);
    assert_eq!(padded.scale(), 28);
    let bars = [
        Bar {
            open: -smallest,
            high: smallest,
            low: -padded,
            close: padded,
            volume: smallest,
            ..bar(0, 0, 0)
        },
        Bar {
            open: Decimal::MAX,
            high: Decimal::MAX,
            low: Decimal::MAX,
            close: Decimal::MAX,
            volume: Decimal::MAX,
            ..bar(1, 0, 0)
        },
    ];
    let specs = [
        spec("sma", json!({"period": 1})),
        spec("volume_sma", json!({"period": 1})),
    ];
    let values = batch(&specs, &bars).unwrap();
    assert_values(&values[0][0], &[("value", "0")]);
    assert_eq!(values[1][0].values["value"], Decimal::MAX);
    assert_eq!(values[1][1].values["value"], Decimal::MAX);
    let mut live = IndicatorEngine::new(&specs);
    for (bar, expected) in bars.iter().zip(&values) {
        assert_eq!(bar.validate(), Ok(()));
        assert_eq!(live.push_closed(bar).unwrap(), *expected);
    }
    let mut replay = IndicatorEngine::new(&specs);
    replay.advance_closed(&bars[0]).unwrap();
    assert_eq!(replay.push_closed(&bars[1]).unwrap(), values[1]);
}

#[test]
fn rejects_outside_input_scale_before_calculation_on_every_path() {
    let too_fine = Decimal::new(1, 28);
    let wide = bar(1, 0, 1);
    let flat = Bar {
        high: Decimal::ZERO,
        low: Decimal::ZERO,
        ..wide
    };
    let outside = [
        Bar {
            open: too_fine,
            ..wide
        },
        Bar {
            high: too_fine,
            ..flat
        },
        Bar {
            low: -too_fine,
            ..flat
        },
        Bar {
            close: too_fine,
            ..wide
        },
        Bar {
            volume: too_fine,
            ..wide
        },
    ];
    let specs = [spec("sma", json!({"period": 2}))];
    let first = bar(0, 8, 1);
    let expected = batch(&specs, &[first, wide]).unwrap();
    for invalid in outside {
        let error = IndicatorError::NumericInputOutOfRange;
        assert_eq!(invalid.validate(), Err(error));
        assert_eq!(batch(&specs, &[first, invalid]), Err(error));
        let mut live = IndicatorEngine::new(&specs);
        let mut replay = IndicatorEngine::new(&specs);
        live.push_closed(&first).unwrap();
        replay.advance_closed(&first).unwrap();
        assert_eq!(live.push_closed(&invalid), Err(error));
        assert_eq!(replay.advance_closed(&invalid), Err(error));
        assert_eq!(replay.preview(&invalid), Err(error));
        assert_eq!(live.push_closed(&wide).unwrap(), expected[1]);
        assert_eq!(replay.push_closed(&wide).unwrap(), expected[1]);
    }
}

#[test]
fn registry_rejects_unknown_versions_and_invalid_parameter_shapes() {
    let invalid = [
        (
            "indicator.sma@2.0.0",
            json!({"period": 2}),
            IndicatorError::UnknownIndicator,
        ),
        (
            "indicator.rsi_wilder@1.0.0",
            json!({"period": 1, "field": "close"}),
            IndicatorError::InvalidParameters,
        ),
        (
            "indicator.rsi_wilder@1.0.0",
            json!({"period": 2, "field": "open"}),
            IndicatorError::InvalidParameters,
        ),
        (
            "indicator.ema@1.0.0",
            json!({"period": "2"}),
            IndicatorError::InvalidParameters,
        ),
        (
            "indicator.ema@1.0.0",
            json!({"period": 2521}),
            IndicatorError::InvalidParameters,
        ),
        (
            "indicator.vwap@1.0.0",
            json!({"period": 1}),
            IndicatorError::InvalidParameters,
        ),
        (
            "indicator.macd@1.0.0",
            json!({"fast_period": 2, "slow_period": 2, "signal_period": 2}),
            IndicatorError::MacdPeriodOrder,
        ),
    ];
    for (id, params, expected) in invalid {
        assert_eq!(IndicatorSpec::new(id, &params).unwrap_err(), expected);
    }
}
