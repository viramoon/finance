use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use finance_ib::{
    entitlements::{ProbeOptions, Reason, Verdict, probe_products},
    market::{
        ConfirmedContract, ContractKind, ContractQuery, GreeksSource, LiquidHours,
        MarketDataSource, MarketDataType, MarketError, MarketEvent, MarketNotice, MarketRequest,
        MarketSubscription, OptionContract, OptionGreeks, OptionRight, Product, QuoteField,
    },
};
use std::{
    collections::VecDeque,
    future::pending,
    path::Path,
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

// Deterministic synthetic fixture. Confirmed contracts and callbacks are composed below
// from constant literals, without captured sessions, external files, or random input.
const OPRA_CON_ID: i32 = 2_001_994_123;
const BID: &str = "462.218001000";
const ASK: &str = "463.219001000";
const OI: &str = "912345678";
const PRIVATE_OPTION: &str = "SYNTHETIC-PRIVATE-OPTION";
const REQUEST_TIME: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Fault {
    #[default]
    None,
    Cancel,
    ConfirmTimeout,
    SubscribeTimeout,
    SubscribeReportedTimeout,
}

#[derive(Default)]
struct SyntheticSource {
    confirmed: Mutex<Vec<ContractQuery>>,
    active: Arc<AtomicBool>,
    cancellations: Arc<AtomicUsize>,
    fault: Fault,
    uniform_data_types: Option<[MarketDataType; 2]>,
}

struct SyntheticLine {
    events: VecDeque<(Duration, MarketEvent)>,
    active: Arc<AtomicBool>,
    cancellations: Arc<AtomicUsize>,
    fault: Fault,
    ends: bool,
}

#[async_trait]
impl MarketDataSource for SyntheticSource {
    type Subscription = SyntheticLine;

    async fn confirm(&self, query: &ContractQuery) -> Result<ConfirmedContract, MarketError> {
        self.confirmed.lock().unwrap().push(query.clone());
        let (symbol, primary_exchange, con_id, kind) = match query {
            ContractQuery::Equity {
                symbol,
                primary_exchange,
            } => {
                if self.fault == Fault::ConfirmTimeout && symbol == "QQQ" {
                    return pending().await;
                }
                (
                    symbol.clone(),
                    Some(primary_exchange.clone()),
                    100,
                    ContractKind::Equity,
                )
            }
            ContractQuery::Option { con_id } => (
                PRIVATE_OPTION.into(),
                None,
                *con_id,
                ContractKind::Option(OptionContract {
                    expiry: "2099-12-31".parse().unwrap(),
                    strike: "456.000001".parse().unwrap(),
                    right: OptionRight::Call,
                    multiplier: "100".parse().unwrap(),
                    underlying_con_id: 100,
                }),
            ),
        };
        let start = Utc.with_ymd_and_hms(2000, 1, 1, 0, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2100, 1, 1, 0, 0, 0).unwrap();
        let interval = if symbol == "GOOGL" && self.uniform_data_types.is_none() {
            LiquidHours::Closed { start, end }
        } else {
            LiquidHours::Open { start, end }
        };
        Ok(ConfirmedContract {
            con_id,
            symbol,
            primary_exchange,
            kind,
            liquid_hours: vec![interval],
        })
    }

    async fn subscribe(&self, request: &MarketRequest) -> Result<Self::Subscription, MarketError> {
        assert_eq!(
            request.parameters().option_open_interest,
            request.key().product == Product::Opra
        );
        let symbol = request.contract().symbol.as_str();
        if symbol == "QQQ" {
            match self.fault {
                Fault::SubscribeTimeout => return pending().await,
                Fault::SubscribeReportedTimeout => return Err(MarketError::Timeout),
                _ => {}
            }
        }
        assert!(!self.active.swap(true, Ordering::SeqCst));
        let events = match (self.uniform_data_types, symbol) {
            (Some([_, opra]), PRIVATE_OPTION) => quotes(opra),
            (Some([equity, _]), _) => quotes(equity),
            (None, "SPY") => quotes(MarketDataType::Frozen),
            (None, "AAPL") => quotes(MarketDataType::Delayed),
            (None, "MSFT") => quotes(MarketDataType::DelayedFrozen),
            (None, "NVDA") => vec![(
                Duration::ZERO,
                MarketEvent::Notice(MarketNotice::NotEntitled),
            )],
            (None, "AMZN" | "GOOGL") => Vec::new(),
            (None, "META") => vec![
                (
                    Duration::ZERO,
                    MarketEvent::DataType(Some(MarketDataType::Realtime)),
                ),
                (Duration::ZERO, quote(QuoteField::BidPrice, BID, false)),
            ],
            (None, "TSLA") => {
                let mut events = quotes(MarketDataType::Realtime);
                events.push((
                    Duration::from_secs(7),
                    MarketEvent::Notice(MarketNotice::PartialEntitlement),
                ));
                events
            }
            (None, PRIVATE_OPTION) => {
                let mut events = quotes(MarketDataType::Realtime);
                events.extend([
                    (
                        Duration::ZERO,
                        MarketEvent::OpenInterest {
                            right: OptionRight::Call,
                            value: Some(OI.parse().unwrap()),
                            received_at: observed_at(),
                        },
                    ),
                    (
                        Duration::ZERO,
                        MarketEvent::Greeks(Box::new(OptionGreeks {
                            source: GreeksSource::Model,
                            delayed: false,
                            received_at: observed_at(),
                            delta: Some("0.4123456789".parse().unwrap()),
                            gamma: Some("0.07654321".parse().unwrap()),
                            vega: Some("0.32223344".parse().unwrap()),
                            theta: Some("-0.20123456".parse().unwrap()),
                            implied_volatility: None,
                            option_price: None,
                            underlying_price: None,
                            pv_dividend: None,
                        })),
                    ),
                ]);
                events
            }
            _ => quotes(MarketDataType::Realtime),
        };
        Ok(SyntheticLine {
            events: events.into(),
            active: self.active.clone(),
            cancellations: self.cancellations.clone(),
            fault: self.fault,
            ends: self.uniform_data_types.is_none() && symbol == "TSLA",
        })
    }
}

#[async_trait]
impl MarketSubscription for SyntheticLine {
    fn request_id(&self) -> i32 {
        20_001_994
    }

    async fn next(&mut self) -> Result<Option<MarketEvent>, MarketError> {
        let Some((delay, event)) = self.events.pop_front() else {
            return if self.ends { Ok(None) } else { pending().await };
        };
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        Ok(Some(event))
    }

    async fn cancel(&mut self) -> Result<(), MarketError> {
        self.cancellations.fetch_add(1, Ordering::SeqCst);
        if self.fault == Fault::Cancel {
            return Err(MarketError::Transport);
        }
        assert!(self.active.swap(false, Ordering::SeqCst));
        Ok(())
    }
}

fn observed_at() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 30, 15, 1, 23).unwrap()
}

fn quote(field: QuoteField, value: &str, delayed: bool) -> MarketEvent {
    MarketEvent::Quote {
        field,
        value: Some(value.parse().unwrap()),
        delayed,
        received_at: observed_at(),
    }
}

fn quotes(data_type: MarketDataType) -> Vec<(Duration, MarketEvent)> {
    vec![
        (Duration::ZERO, MarketEvent::DataType(Some(data_type))),
        (
            Duration::ZERO,
            quote(QuoteField::BidPrice, BID, data_type.is_delayed()),
        ),
        (
            Duration::ZERO,
            quote(QuoteField::AskPrice, ASK, data_type.is_delayed()),
        ),
    ]
}

#[tokio::test(start_paused = true)]
async fn product_probe_reports_actual_types_and_only_redacted_evidence() {
    let source = SyntheticSource::default();
    let report = probe_products(
        &source,
        ProbeOptions {
            opra_con_id: Some(OPRA_CON_ID),
            equity_l1_permission_confirmed: true,
            opra_permission_confirmed: true,
        },
        REQUEST_TIME,
    )
    .await;
    assert_eq!(report.failure, None);
    let [equity, opra] = &report.products;
    assert_eq!(
        (equity.product, equity.verdict),
        (Product::EquityL1, Verdict::NotEntitled)
    );
    let expected = [
        (
            "QQQ.NASDAQ",
            Some(1),
            Verdict::Passed,
            Reason::RealtimeQuote,
        ),
        ("SPY.ARCA", Some(2), Verdict::Pending, Reason::FrozenData),
        (
            "AAPL.NASDAQ",
            Some(3),
            Verdict::Delayed,
            Reason::DelayedQuote,
        ),
        (
            "MSFT.NASDAQ",
            Some(4),
            Verdict::Delayed,
            Reason::DelayedFrozenQuote,
        ),
        (
            "NVDA.NASDAQ",
            None,
            Verdict::NotEntitled,
            Reason::NotEntitled,
        ),
        (
            "AMZN.NASDAQ",
            None,
            Verdict::Pending,
            Reason::NoTypeCallback,
        ),
        ("GOOGL.NASDAQ", None, Verdict::Pending, Reason::MarketClosed),
        (
            "META.NASDAQ",
            Some(1),
            Verdict::Pending,
            Reason::NoValidBidAsk,
        ),
        (
            "TSLA.NASDAQ",
            Some(1),
            Verdict::Pending,
            Reason::PartialEntitlement,
        ),
    ];
    let actual: Vec<_> = equity
        .probes
        .iter()
        .map(|probe| {
            assert!(probe.contract_confirmed);
            assert_eq!(probe.greeks_received, None);
            (
                probe.target,
                probe.market_data_type,
                probe.verdict,
                probe.reason,
            )
        })
        .collect();
    assert_eq!(actual, expected);
    assert_eq!(
        (opra.product, opra.verdict),
        (Product::Opra, Verdict::Passed)
    );
    assert_eq!(opra.probes.len(), 1);
    let option = &opra.probes[0];
    assert_eq!(
        (option.market_data_type, option.quote_received_at),
        (Some(1), Some(observed_at()))
    );
    assert_eq!(
        (option.open_interest_received, option.greeks_received),
        (Some(true), Some(true))
    );
    assert_eq!(source.confirmed.lock().unwrap().len(), 10);
    assert_eq!(source.cancellations.load(Ordering::SeqCst), 10);
    assert!(!source.active.load(Ordering::SeqCst));
    let json = serde_json::to_string(&report).unwrap();
    for sensitive in [
        PRIVATE_OPTION,
        &OPRA_CON_ID.to_string(),
        BID,
        ASK,
        OI,
        "0.4123456789",
        "20001994",
    ] {
        assert!(!json.contains(sensitive));
    }
    let json: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(json["products"][0]["product"], "equity_l1");
    assert_eq!(json["products"][0]["verdict"], "not_entitled");
    assert_eq!(json["products"][1]["verdict"], "passed");
}

#[tokio::test(start_paused = true)]
async fn product_verdicts_report_delayed_and_delayed_frozen_quotes() {
    let source = SyntheticSource {
        uniform_data_types: Some([MarketDataType::Delayed, MarketDataType::DelayedFrozen]),
        ..SyntheticSource::default()
    };
    let report = probe_products(
        &source,
        ProbeOptions {
            opra_con_id: Some(OPRA_CON_ID),
            equity_l1_permission_confirmed: true,
            opra_permission_confirmed: true,
        },
        REQUEST_TIME,
    )
    .await;
    assert_eq!(report.failure, None);
    for (product, mode, reason) in [
        (Product::EquityL1, 3, Reason::DelayedQuote),
        (Product::Opra, 4, Reason::DelayedFrozenQuote),
    ] {
        let result = report
            .products
            .iter()
            .find(|row| row.product == product)
            .unwrap();
        assert_eq!(result.verdict, Verdict::Delayed);
        assert!(!result.probes.is_empty());
        assert!(result.probes.iter().all(|probe| {
            probe.bid_ask_valid
                && probe.market_data_type == Some(mode)
                && probe.verdict == Verdict::Delayed
                && probe.reason == reason
        }));
    }
    let json = serde_json::to_value(report).unwrap();
    assert_eq!(json["products"][0]["verdict"], "delayed");
    assert_eq!(json["products"][1]["verdict"], "delayed");
}

#[tokio::test(start_paused = true)]
async fn realtime_quotes_require_each_products_own_permission_confirmation() {
    for equity_confirmed in [true, false] {
        let source = SyntheticSource {
            uniform_data_types: Some([MarketDataType::Realtime; 2]),
            ..SyntheticSource::default()
        };
        let report = probe_products(
            &source,
            ProbeOptions {
                opra_con_id: Some(OPRA_CON_ID),
                equity_l1_permission_confirmed: equity_confirmed,
                opra_permission_confirmed: !equity_confirmed,
            },
            REQUEST_TIME,
        )
        .await;
        assert_eq!(report.failure, None);
        for (product, confirmed) in report
            .products
            .iter()
            .zip([equity_confirmed, !equity_confirmed])
        {
            let (verdict, reason) = if confirmed {
                (Verdict::Passed, Reason::RealtimeQuote)
            } else {
                (Verdict::Pending, Reason::PermissionUnconfirmed)
            };
            assert_eq!(product.account_permission_confirmed, confirmed);
            assert_eq!(product.verdict, verdict);
            assert!(!product.probes.is_empty());
            assert!(product.probes.iter().all(|probe| {
                probe.bid_ask_valid
                    && probe.market_data_type == Some(1)
                    && probe.verdict == verdict
                    && probe.reason == reason
            }));
        }
    }
}

#[tokio::test(start_paused = true)]
async fn realtime_callback_does_not_attest_consolidated_and_unselected_opra_is_pending() {
    let source = SyntheticSource::default();
    let report = probe_products(&source, ProbeOptions::default(), REQUEST_TIME).await;
    let [equity, opra] = &report.products;
    let qqq = &equity.probes[0];
    assert!(qqq.bid_ask_valid);
    assert_eq!(
        (qqq.verdict, qqq.reason),
        (Verdict::Pending, Reason::PermissionUnconfirmed)
    );
    assert_eq!(opra.verdict, Verdict::Pending);
    assert_eq!(opra.probes[0].reason, Reason::OptionNotSelected);
    assert_eq!(source.cancellations.load(Ordering::SeqCst), 9);
}

#[tokio::test(start_paused = true)]
async fn uncertain_subscribe_or_failed_cancel_stops_opening_lines() {
    for (fault, reason, cancellations) in [
        (Fault::SubscribeTimeout, Reason::SubscriptionTimeout, 0),
        (
            Fault::SubscribeReportedTimeout,
            Reason::SubscriptionTimeout,
            0,
        ),
        (Fault::Cancel, Reason::CancelFailed, 1),
    ] {
        let source = SyntheticSource {
            fault,
            ..SyntheticSource::default()
        };
        let report = probe_products(
            &source,
            ProbeOptions {
                opra_con_id: Some(OPRA_CON_ID),
                ..ProbeOptions::default()
            },
            REQUEST_TIME,
        )
        .await;
        assert_eq!(report.failure, Some(reason));
        assert_eq!(report.products[0].probes[0].reason, reason);
        assert!(
            report.products[0].probes[1..]
                .iter()
                .all(|probe| probe.reason == Reason::SweepStopped)
        );
        assert_eq!(report.products[1].probes[0].reason, Reason::SweepStopped);
        assert_eq!(source.confirmed.lock().unwrap().len(), 1);
        assert_eq!(source.cancellations.load(Ordering::SeqCst), cancellations);
    }
}

#[tokio::test(start_paused = true)]
async fn contract_confirmation_timeout_does_not_consume_a_line() {
    let source = SyntheticSource {
        fault: Fault::ConfirmTimeout,
        ..SyntheticSource::default()
    };
    let report = probe_products(&source, ProbeOptions::default(), REQUEST_TIME).await;
    assert_eq!(report.failure, None);
    let first = &report.products[0].probes[0];
    assert_eq!(first.reason, Reason::RequestTimeout);
    assert!(!first.contract_confirmed);
    assert_eq!(source.cancellations.load(Ordering::SeqCst), 8);
}

#[test]
fn entitlements_cli_requires_explicit_opt_in_and_does_not_echo_input() {
    let private_arg = "SYNTHETIC-PRIVATE-ARGUMENT";
    let output = Command::new(env!("CARGO_BIN_EXE_finance-ib"))
        .arg("entitlements")
        .env_clear()
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("requires IB_LIVE_TEST=1"));

    let output = Command::new(env!("CARGO_BIN_EXE_finance-ib"))
        .args(["entitlements", "--opra-con-id", private_arg])
        .env_clear()
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(!String::from_utf8_lossy(&output.stderr).contains(private_arg));
}

#[test]
fn product_permission_switches_are_separate() {
    assert_eq!(
        ProbeOptions::parse(["--confirm-opra-permission", "--opra-con-id", "123"]),
        Some(ProbeOptions {
            opra_con_id: Some(123),
            equity_l1_permission_confirmed: false,
            opra_permission_confirmed: true,
        })
    );
    assert!(ProbeOptions::parse(["--confirm-opra-permission"]).is_none());
    assert!(ProbeOptions::parse(["--opra-con-id", "0"]).is_none());
    assert!(ProbeOptions::parse(["--opra-con-id", "123", "--opra-con-id", "456"]).is_none());
}

#[test]
fn entitlements_cli_redacts_configuration_failure() {
    let private_endpoint = "SYNTHETIC-PRIVATE-ENDPOINT:4001";
    let output = Command::new(env!("CARGO_BIN_EXE_finance-ib"))
        .arg("entitlements")
        .env_clear()
        .env("IB_LIVE_TEST", "1")
        .env("IB_GATEWAY_ADDRESS", private_endpoint)
        .env(
            "FINANCE_CONFIG_DIR",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config"),
        )
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stderr.is_empty());
    assert!(!String::from_utf8_lossy(&output.stdout).contains(private_endpoint));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["failure"], "invalid_configuration");
    assert_eq!(report["products"][0]["verdict"], "pending");
    assert_eq!(report["products"][1]["verdict"], "pending");
    assert_eq!(
        report["products"][0]["probes"][0]["reason"],
        "invalid_configuration"
    );
    assert_eq!(
        report["products"][1]["probes"][0]["reason"],
        "option_not_selected"
    );
}

#[test]
fn entitlements_cli_rejects_sdk_recording_before_connect() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let recording_path = format!("/tmp/finance-ib-{}-{nonce}-recording", std::process::id());
    assert!(!Path::new(&recording_path).exists());

    let output = Command::new(env!("CARGO_BIN_EXE_finance-ib"))
        .arg("entitlements")
        .env_clear()
        .env("IB_LIVE_TEST", "1")
        .env("IBAPI_RECORDING_DIR", &recording_path)
        .env("IB_GATEWAY_ADDRESS", "127.0.0.2:1")
        .env("IB_CONNECT_TIMEOUT_MS", "100")
        .env(
            "FINANCE_CONFIG_DIR",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config"),
        )
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(output.stderr.is_empty());
    assert!(!String::from_utf8_lossy(&output.stdout).contains(&recording_path));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["failure"], "invalid_configuration");
    assert!(!Path::new(&recording_path).exists());
}
