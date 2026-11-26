use std::cell::Cell;
use std::sync::Mutex;

use anyhow::{Result, bail};
use async_trait::async_trait;
use finance_collector::option_history::{
    BarWindow, ConfirmedOption, OptionBars, PaperOptionHistory, TradeBar, fetch_ibkr_chain,
};
use finance_market_protocol::U64Text;

const BARS: &str = include_str!("../../../tests/fixtures/option-history/ibkr-trade-bars.json");
const START: u64 = 1_741_363_200_000_000_000;
const MINUTE: u64 = 60_000_000_000;

fn window() -> BarWindow {
    BarWindow {
        start: START.into(),
        end: (START + 2 * MINUTE).into(),
    }
}

fn option(con_id: i32, expires_at: U64Text) -> ConfirmedOption {
    ConfirmedOption {
        instrument_id: format!("SYNTH-OPT-{con_id}.OPRA").parse().unwrap(),
        con_id,
        expires_at,
    }
}

#[derive(Default)]
struct Replay {
    rows: Vec<TradeBar>,
    requests: Mutex<Vec<(ConfirmedOption, BarWindow)>>,
}

#[async_trait]
impl PaperOptionHistory for Replay {
    async fn fetch_trade_bars(
        &self,
        contract: &ConfirmedOption,
        window: BarWindow,
    ) -> Result<Vec<TradeBar>> {
        self.requests
            .lock()
            .unwrap()
            .push((contract.clone(), window));
        match contract.con_id {
            1 => Ok(self.rows.clone()),
            2 => Ok(vec![]),
            _ => bail!("synthetic permission failure"),
        }
    }
}

#[tokio::test]
async fn active_contract_fetches_sorted_closed_bars_with_exact_prices_and_volume() {
    let source = Replay {
        rows: serde_json::from_str(BARS).unwrap(),
        ..Replay::default()
    };
    let window = window();
    let active = option(1, (window.end.get() + MINUTE).into());
    let mut history = fetch_ibkr_chain(&source, std::slice::from_ref(&active), window, || {
        window.end
    })
    .await
    .unwrap();
    let result = history.remove(0);
    assert_eq!(result.instrument_id, active.instrument_id);
    assert_eq!(*source.requests.lock().unwrap(), [(active, window)]);
    let OptionBars::Available(bars) = result.bars.unwrap() else {
        panic!("expected available bars");
    };
    assert_eq!(bars.len(), 2);
    assert_eq!(bars[0].data.open_ts, window.start);
    assert_eq!(bars[0].close_ts, (START + MINUTE).into());
    assert_eq!(bars[1].close_ts, window.end);
    let wire = serde_json::to_value(&bars[0]).unwrap();
    assert_eq!(wire["close_ts"], (START + MINUTE).to_string());
    assert_eq!(wire["data"]["close"], "1.300000000000000001");
    assert_eq!(wire["data"]["volume"], "9007199254740993");
}

#[tokio::test]
async fn expiry_including_the_exact_cutoff_skips_the_paper_request() {
    let source = Replay::default();
    let window = window();
    let now = (window.end.get() + MINUTE).into();
    let contracts = [option(1, window.end), option(2, now)];
    let results = fetch_ibkr_chain(&source, &contracts, window, || now)
        .await
        .unwrap();
    assert_eq!(results.len(), contracts.len());
    for (result, contract) in results.into_iter().zip(&contracts) {
        assert_eq!(result.instrument_id, contract.instrument_id);
        assert_eq!(result.bars.unwrap(), OptionBars::Expired);
    }
    assert!(source.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn clock_is_checked_again_for_each_contract_in_the_chain() {
    let source = Replay::default();
    let window = window();
    let expiry = (window.end.get() + MINUTE).into();
    let now = Cell::new(window.end);
    let calls = Cell::new(0);
    let contracts = [option(1, expiry), option(2, expiry)];
    let results = fetch_ibkr_chain(&source, &contracts, window, || {
        calls.set(calls.get() + 1);
        if calls.get() == 3 {
            now.set(expiry);
        }
        now.get()
    })
    .await
    .unwrap();
    assert_eq!(results[0].bars.as_ref().unwrap(), &OptionBars::NoBars);
    assert_eq!(results[1].bars.as_ref().unwrap(), &OptionBars::Expired);
    assert_eq!(
        *source.requests.lock().unwrap(),
        [(contracts[0].clone(), window)]
    );
}

#[tokio::test]
async fn provider_failure_is_distinct_from_no_bars_and_does_not_abort_the_chain() {
    let source = Replay::default();
    let window = window();
    let expiry = (window.end.get() + MINUTE).into();
    let contracts = [option(3, expiry), option(2, expiry), option(1, window.end)];
    let results = fetch_ibkr_chain(&source, &contracts, window, || window.end)
        .await
        .unwrap();
    assert_eq!(
        results[0].bars.as_ref().unwrap_err().to_string(),
        "synthetic permission failure"
    );
    assert_eq!(results[1].bars.as_ref().unwrap(), &OptionBars::NoBars);
    assert_eq!(results[2].bars.as_ref().unwrap(), &OptionBars::Expired);
    let calls = source.requests.lock().unwrap();
    assert_eq!(
        *calls,
        [
            (contracts[0].clone(), window),
            (contracts[1].clone(), window)
        ]
    );
}

#[tokio::test]
async fn invalid_windows_and_contracts_fail_before_any_paper_request() {
    let source = Replay::default();
    let window = window();
    let expiry = (window.end.get() + MINUTE).into();
    let valid = option(1, expiry);
    let now = || window.end;
    let empty = BarWindow {
        end: window.start,
        ..window
    };
    assert!(
        fetch_ibkr_chain(&source, std::slice::from_ref(&valid), empty, now)
            .await
            .is_err()
    );
    assert!(
        fetch_ibkr_chain(&source, std::slice::from_ref(&valid), window, || window
            .start)
        .await
        .is_err()
    );
    for invalid in [option(0, expiry), valid.clone()] {
        assert!(
            fetch_ibkr_chain(&source, &[valid.clone(), invalid], window, now)
                .await
                .is_err()
        );
    }
    assert!(source.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn malformed_and_conflicting_bars_fail_the_contract_read() {
    let rows: Vec<TradeBar> = serde_json::from_str(BARS).unwrap();
    let mut invalid = rows[0].clone();
    invalid.data.volume = "-1".parse().unwrap();
    let mut conflicting = rows[0].clone();
    conflicting.data.volume = "1".parse().unwrap();
    let window = window();
    let active = [option(1, (window.end.get() + MINUTE).into())];
    for (row, expected_error) in [
        (invalid, "invalid IBKR option bar"),
        (conflicting, "conflicting IBKR option bars"),
    ] {
        let source = Replay {
            rows: vec![rows[0].clone(), row],
            ..Replay::default()
        };
        let results = fetch_ibkr_chain(&source, &active, window, || window.end)
            .await
            .unwrap();
        assert!(
            results[0]
                .bars
                .as_ref()
                .unwrap_err()
                .to_string()
                .starts_with(expected_error)
        );
    }
}

mod databento {
    use std::num::NonZeroU64;

    use finance_collector::option_history::databento::{
        DATASET, DatabentoOpra, EstimateConfig, EstimateRequest, Status,
    };

    const ASSUMPTIONS: &str = "\
price_source: 'Synthetic test tariff, no vendor rate'
price_as_of: '2026-11-26'
schemas:
  ohlcv-1m:
    bytes_per_contract_day: 40000
    usd_micros_per_gb: 1250001
  trades:
    bytes_per_contract_day: 1000000
    usd_micros_per_gb: 2500000
";

    fn config() -> EstimateConfig {
        serde_yaml::from_str(ASSUMPTIONS).unwrap()
    }

    fn request() -> EstimateRequest {
        EstimateRequest {
            contracts_by_underlying: [
                ("SYNTHA.XNAS".parse().unwrap(), 2.try_into().unwrap()),
                ("SYNTHB.XNAS".parse().unwrap(), 3.try_into().unwrap()),
            ]
            .into(),
            start_date: "2024-02-28".parse().unwrap(),
            end_date_exclusive: "2024-03-01".parse().unwrap(),
            schema: "ohlcv-1m".parse().unwrap(),
        }
    }

    #[test]
    fn missing_configuration_disables_the_adapter() {
        let adapter = DatabentoOpra::default();
        assert_eq!(adapter.status(), Status::Disabled);
        assert_eq!(
            adapter.estimate(&request()).unwrap_err().to_string(),
            "Databento OPRA is disabled"
        );
        assert_eq!(DatabentoOpra::new(None).unwrap().status(), Status::Disabled);
    }

    #[test]
    fn configured_estimate_scales_by_contracts_days_and_schema_with_price_provenance() {
        let adapter = DatabentoOpra::new(Some(config())).unwrap();
        assert_eq!(adapter.status(), Status::EstimateOnly);
        let mut request = request();
        let bars = adapter.estimate(&request).unwrap();
        assert_eq!(bars.dataset, DATASET);
        assert_eq!(bars.schema, request.schema);
        assert_eq!(bars.contract_count, 5);
        assert_eq!(bars.calendar_days, 2);
        assert_eq!(bars.estimated_bytes, 400_000);
        assert_eq!(bars.estimated_cost_usd.to_string(), "0.000500000400000");
        assert_eq!(
            bars.price_source.as_str(),
            "Synthetic test tariff, no vendor rate"
        );
        assert_eq!(bars.price_as_of.to_string(), "2026-11-26");

        request.schema = "trades".parse().unwrap();
        let trades = adapter.estimate(&request).unwrap();
        assert_eq!(trades.estimated_bytes, 10_000_000);
        assert_eq!(trades.estimated_cost_usd.to_string(), "0.025000000000000");

        request.end_date_exclusive = "2024-03-03".parse().unwrap();
        request
            .contracts_by_underlying
            .remove(&"SYNTHA.XNAS".parse().unwrap());
        let smaller_chain = adapter.estimate(&request).unwrap();
        assert_eq!(smaller_chain.calendar_days, 4);
        assert_eq!(smaller_chain.contract_count, 3);
        assert_eq!(smaller_chain.estimated_bytes, 12_000_000);
        assert_eq!(
            smaller_chain.estimated_cost_usd.to_string(),
            "0.030000000000000"
        );
    }

    #[test]
    fn config_requires_dated_pricing_and_rejects_account_fields() {
        let missing_date = ASSUMPTIONS.replace("price_as_of: '2026-11-26'\n", "");
        assert!(serde_yaml::from_str::<EstimateConfig>(&missing_date).is_err());
        let account_config = format!("{ASSUMPTIONS}account: unsupported\n");
        assert!(serde_yaml::from_str::<EstimateConfig>(&account_config).is_err());
        let mut incomplete = config();
        incomplete.schemas.clear();
        assert!(DatabentoOpra::new(Some(incomplete)).is_err());
        let mut incomplete = config();
        incomplete.price_source = " ".parse().unwrap();
        assert!(DatabentoOpra::new(Some(incomplete)).is_err());
    }

    #[test]
    fn rejects_unknown_schema_invalid_ranges_empty_underlyings_and_overflow() {
        let mut config = config();
        config
            .schemas
            .get_mut(&"trades".parse().unwrap())
            .unwrap()
            .bytes_per_contract_day = NonZeroU64::MAX;
        let adapter = DatabentoOpra::new(Some(config)).unwrap();
        let mut request = request();
        request.schema = "unsupported".parse().unwrap();
        assert!(
            adapter
                .estimate(&request)
                .unwrap_err()
                .to_string()
                .contains("no budget")
        );
        request.schema = "trades".parse().unwrap();
        assert!(
            adapter
                .estimate(&request)
                .unwrap_err()
                .to_string()
                .contains("overflows")
        );
        request.end_date_exclusive = request.start_date;
        assert!(
            adapter
                .estimate(&request)
                .unwrap_err()
                .to_string()
                .contains("date range")
        );
        request.end_date_exclusive = "2024-02-27".parse().unwrap();
        assert!(
            adapter
                .estimate(&request)
                .unwrap_err()
                .to_string()
                .contains("date range")
        );
        request.end_date_exclusive = "2024-03-01".parse().unwrap();
        request.contracts_by_underlying.clear();
        assert!(
            adapter
                .estimate(&request)
                .unwrap_err()
                .to_string()
                .contains("underlying")
        );
    }
}
