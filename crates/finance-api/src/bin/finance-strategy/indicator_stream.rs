//! Local, bounded NDJSON bridge to the F6 indicator engine.

use std::{
    collections::{BTreeMap, btree_map::Entry},
    fmt,
    io::{BufRead, Read, Write},
};

use chrono::{DateTime, NaiveDate, Utc};
use finance_api::indicators::{Bar, IndicatorEngine, IndicatorSpec};
use finance_market_protocol::{DecimalText, U64Text};
use serde::{Deserialize, Deserializer, Serialize, de};
use serde_json::{Map, Value};

const MAX_FRAME_BYTES: usize = 128 * 1024;
const MAX_BARS: usize = 256;
const INVALID: &str = "Invalid indicator stream request.";

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Start {
        series: u64,
        id: String,
        #[serde(deserialize_with = "unique_params")]
        params: Value,
        output: String,
    },
    Push {
        series: u64,
        bars: Vec<WireBar>,
    },
    Drop {
        series: u64,
    },
}

fn unique_params<'de, D: Deserializer<'de>>(input: D) -> Result<Value, D::Error> {
    struct Parameters;

    impl<'de> de::Visitor<'de> for Parameters {
        type Value = Value;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("an indicator parameter object")
        }

        fn visit_map<A: de::MapAccess<'de>>(self, mut input: A) -> Result<Value, A::Error> {
            let mut parameters = Map::new();
            while let Some((key, value)) = input.next_entry()? {
                if parameters.insert(key, value).is_some() {
                    return Err(de::Error::custom("duplicate indicator parameter"));
                }
            }
            Ok(Value::Object(parameters))
        }
    }

    input.deserialize_map(Parameters)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireBar {
    open_ts: U64Text,
    end_ts: U64Text,
    trading_day: String,
    open: DecimalText,
    high: DecimalText,
    low: DecimalText,
    close: DecimalText,
    volume: DecimalText,
}

impl WireBar {
    fn into_bar(self) -> Result<Bar, &'static str> {
        let trading_day =
            NaiveDate::parse_from_str(&self.trading_day, "%Y-%m-%d").map_err(|_| INVALID)?;
        if self.trading_day.len() != 10 || trading_day.to_string() != self.trading_day {
            return Err(INVALID);
        }
        Ok(Bar {
            open_ts: timestamp(self.open_ts)?,
            end_ts: timestamp(self.end_ts)?,
            trading_day,
            open: self.open.value(),
            high: self.high.value(),
            low: self.low.value(),
            close: self.close.value(),
            volume: self.volume.value(),
        })
    }
}

fn timestamp(value: U64Text) -> Result<DateTime<Utc>, &'static str> {
    const NANOS_PER_SECOND: u64 = 1_000_000_000;
    let nanos = value.get();
    DateTime::from_timestamp(
        (nanos / NANOS_PER_SECOND) as i64,
        (nanos % NANOS_PER_SECOND) as u32,
    )
    .ok_or(INVALID)
}

struct Series {
    engine: IndicatorEngine,
    output: &'static str,
}

#[derive(Serialize)]
struct Reply {
    value: Option<String>,
}

fn apply(series_map: &mut BTreeMap<u64, Series>, request: Request) -> Result<Reply, &'static str> {
    let value = match request {
        Request::Start {
            series,
            id,
            params,
            output,
        } => {
            let Entry::Vacant(entry) = series_map.entry(series) else {
                return Err(INVALID);
            };
            let spec = IndicatorSpec::new(&id, &params).map_err(|_| INVALID)?;
            let output = spec
                .definition()
                .outputs
                .iter()
                .copied()
                .find(|name| *name == output)
                .ok_or(INVALID)?;
            entry.insert(Series {
                engine: IndicatorEngine::new(&[spec]),
                output,
            });
            None
        }
        Request::Push { series, bars } => {
            let state = series_map.get_mut(&series).ok_or(INVALID)?;
            if !(1..=MAX_BARS).contains(&bars.len()) {
                return Err(INVALID);
            }
            let mut bars = bars.into_iter();
            let last = bars.next_back().ok_or(INVALID)?;
            for bar in bars {
                state
                    .engine
                    .advance_closed(&bar.into_bar()?)
                    .map_err(|_| INVALID)?;
            }
            let result = state
                .engine
                .push_closed(&last.into_bar()?)
                .map_err(|_| INVALID)?;
            result[0].values.get(state.output).map(ToString::to_string)
        }
        Request::Drop { series } => {
            series_map.remove(&series).ok_or(INVALID)?;
            None
        }
    };
    Ok(Reply { value })
}

pub(super) fn run(mut input: impl BufRead, mut output: impl Write) -> Result<(), &'static str> {
    const WRITE_FAILED: &str = "Could not write indicator stream reply.";
    let mut series_map = BTreeMap::new();
    let mut frame = Vec::new();
    loop {
        frame.clear();
        let length = input
            .by_ref()
            .take((MAX_FRAME_BYTES + 1) as u64)
            .read_until(b'\n', &mut frame)
            .map_err(|_| "Could not read indicator stream request.")?;
        if length == 0 {
            return Ok(());
        }
        if length > MAX_FRAME_BYTES || frame.last() != Some(&b'\n') {
            return Err(INVALID);
        }
        let request = serde_json::from_slice(&frame).map_err(|_| INVALID)?;
        let reply = apply(&mut series_map, request)?;
        serde_json::to_writer(&mut output, &reply).map_err(|_| WRITE_FAILED)?;
        output.write_all(b"\n").map_err(|_| WRITE_FAILED)?;
        output.flush().map_err(|_| WRITE_FAILED)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bar(open_ts: &str, end_ts: &str, price: &str) -> Value {
        json!({"open_ts": open_ts, "end_ts": end_ts, "trading_day": "2024-01-02",
            "open": price, "high": price, "low": price, "close": price, "volume": "100"})
    }

    #[test]
    fn selects_band_outputs_for_chunked_or_incremental_pushes() {
        let first = bar("1704205800000000000", "1704209400000000000", "10.00");
        let second = bar("1704209400000000000", "1704213000000000000", "12.00");
        let requests = [
            json!({"action": "start", "series": 0, "id": "indicator.bollinger@1.0.0", "params": {"period": 2}, "output": "upper"}),
            json!({"action": "start", "series": 1, "id": "indicator.bollinger@1.0.0", "params": {"period": 2}, "output": "lower"}),
            json!({"action": "push", "series": 0, "bars": [&first]}),
            json!({"action": "push", "series": 0, "bars": [&second]}),
            json!({"action": "push", "series": 1, "bars": [&first, &second]}),
            json!({"action": "drop", "series": 0}),
            json!({"action": "drop", "series": 1}),
        ];
        let input = requests
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        let mut output = Vec::new();
        run(input.as_bytes(), &mut output).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "{\"value\":null}\n{\"value\":null}\n{\"value\":null}\n{\"value\":\"13\"}\n{\"value\":\"9\"}\n{\"value\":null}\n{\"value\":null}\n"
        );
    }

    #[test]
    fn rejects_an_unknown_or_duplicate_field_and_unterminated_or_oversized_frames() {
        for frame in [
            "{\"action\":\"drop\",\"series\":0,\"synthetic\\nmarker\":0}\n",
            "{\"action\":\"start\",\"series\":0,\"id\":\"indicator.sma@1.0.0\",\"params\":{\"period\":2,\"period\":3},\"output\":\"value\"}\n",
            "{\"action\":\"drop\",\"series\":0}",
            &" ".repeat(MAX_FRAME_BYTES + 1),
        ] {
            let mut output = Vec::new();
            assert_eq!(run(frame.as_bytes(), &mut output), Err(INVALID));
            assert!(output.is_empty());
        }
    }

    #[test]
    fn rejects_numeric_bar_scalars() {
        let mut bar = bar("1704205800000000000", "1704209400000000000", "10.00");
        bar["close"] = json!(10.0);
        let input = format!(
            "{{\"action\":\"start\",\"series\":0,\"id\":\"indicator.sma@1.0.0\",\"params\":{{\"period\":1}},\"output\":\"value\"}}\n{}\n",
            json!({"action": "push", "series": 0, "bars": [bar]})
        );
        let mut output = Vec::new();
        assert_eq!(run(input.as_bytes(), &mut output), Err(INVALID));
        assert_eq!(output, b"{\"value\":null}\n");
    }
}
