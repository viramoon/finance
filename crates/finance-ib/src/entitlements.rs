use crate::{
    ConnectionManager, IbConfig, IbTransport,
    market::{
        ContractQuery, MarketDataSource, MarketDataType, MarketError, MarketEvidence, MarketNotice,
        MarketObservation, MarketRequest, MarketSession, MarketSubscription, Product,
        RequestParameters,
    },
};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::time::Duration;
use tokio::time::{Instant, timeout, timeout_at};

pub const USAGE: &str = "finance-ib entitlements [--opra-con-id ID] [--confirm-equity-l1-permission] [--confirm-opra-permission]";

/// The two confirmations record a separate manual check of the account's product permissions.
/// A successful realtime callback alone cannot establish consolidated coverage.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProbeOptions {
    pub opra_con_id: Option<i32>,
    pub equity_l1_permission_confirmed: bool,
    pub opra_permission_confirmed: bool,
}

impl ProbeOptions {
    pub fn parse<I, S>(args: I) -> Option<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let mut options = Self::default();
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            match arg.as_ref().to_str()? {
                "--opra-con-id" if options.opra_con_id.is_none() => {
                    let con_id = args.next()?.as_ref().to_str()?.parse().ok()?;
                    if con_id <= 0 {
                        return None;
                    }
                    options.opra_con_id = Some(con_id);
                }
                "--confirm-equity-l1-permission" if !options.equity_l1_permission_confirmed => {
                    options.equity_l1_permission_confirmed = true;
                }
                "--confirm-opra-permission" if !options.opra_permission_confirmed => {
                    options.opra_permission_confirmed = true;
                }
                _ => return None,
            }
        }
        if options.opra_permission_confirmed && options.opra_con_id.is_none() {
            return None;
        }
        Some(options)
    }
}

struct EquityTarget {
    instrument_id: &'static str,
    symbol: &'static str,
    primary_exchange: &'static str,
}

const EQUITIES: [EquityTarget; 9] = [
    EquityTarget {
        instrument_id: "QQQ.NASDAQ",
        symbol: "QQQ",
        primary_exchange: "NASDAQ",
    },
    EquityTarget {
        instrument_id: "SPY.ARCA",
        symbol: "SPY",
        primary_exchange: "ARCA",
    },
    EquityTarget {
        instrument_id: "AAPL.NASDAQ",
        symbol: "AAPL",
        primary_exchange: "NASDAQ",
    },
    EquityTarget {
        instrument_id: "MSFT.NASDAQ",
        symbol: "MSFT",
        primary_exchange: "NASDAQ",
    },
    EquityTarget {
        instrument_id: "NVDA.NASDAQ",
        symbol: "NVDA",
        primary_exchange: "NASDAQ",
    },
    EquityTarget {
        instrument_id: "AMZN.NASDAQ",
        symbol: "AMZN",
        primary_exchange: "NASDAQ",
    },
    EquityTarget {
        instrument_id: "GOOGL.NASDAQ",
        symbol: "GOOGL",
        primary_exchange: "NASDAQ",
    },
    EquityTarget {
        instrument_id: "META.NASDAQ",
        symbol: "META",
        primary_exchange: "NASDAQ",
    },
    EquityTarget {
        instrument_id: "TSLA.NASDAQ",
        symbol: "TSLA",
        primary_exchange: "NASDAQ",
    },
];

const OPRA_TARGET: &str = "operator_selected_option";
const OBSERVATION_WINDOW: Duration = Duration::from_secs(8);

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Passed,
    Delayed,
    Pending,
    NotEntitled,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    RealtimeQuote,
    DelayedQuote,
    DelayedFrozenQuote,
    FrozenData,
    NoTypeCallback,
    UnknownMarketDataType,
    NoValidBidAsk,
    MarketClosed,
    MarketHoursUnknown,
    PermissionUnconfirmed,
    NotEntitled,
    PartialEntitlement,
    DelayedFallbackAvailable,
    CompetingSession,
    Pacing,
    OptionNotSelected,
    UnconfirmedContract,
    ExpiredContract,
    InvalidRequest,
    RequestTimeout,
    SubscriptionTimeout,
    StreamEnded,
    TransportFailure,
    GatewayDisconnected,
    UnsafeAccount,
    InvalidConfiguration,
    CancelFailed,
    SweepStopped,
}

impl From<MarketError> for Reason {
    fn from(error: MarketError) -> Self {
        match error {
            MarketError::NotEntitled => Self::NotEntitled,
            MarketError::UnconfirmedContract => Self::UnconfirmedContract,
            MarketError::ExpiredContract => Self::ExpiredContract,
            MarketError::InvalidRequest => Self::InvalidRequest,
            MarketError::Timeout => Self::RequestTimeout,
            MarketError::Disconnected => Self::GatewayDisconnected,
            MarketError::Transport => Self::TransportFailure,
            MarketError::UnsafeAccount => Self::UnsafeAccount,
            MarketError::InvalidConfiguration => Self::InvalidConfiguration,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ProbeEvidence {
    pub target: &'static str,
    pub verdict: Verdict,
    pub reason: Reason,
    pub contract_confirmed: bool,
    pub market_session: Option<MarketSession>,
    pub market_data_type: Option<u8>,
    pub quote_received_at: Option<DateTime<Utc>>,
    pub bid_ask_valid: bool,
    pub open_interest_received: Option<bool>,
    pub greeks_received: Option<bool>,
}

impl ProbeEvidence {
    fn pending(target: &'static str, reason: Reason) -> Self {
        Self {
            target,
            verdict: if reason == Reason::NotEntitled {
                Verdict::NotEntitled
            } else {
                Verdict::Pending
            },
            reason,
            contract_confirmed: false,
            market_session: None,
            market_data_type: None,
            quote_received_at: None,
            bid_ask_valid: false,
            open_interest_received: (target == OPRA_TARGET).then_some(false),
            greeks_received: (target == OPRA_TARGET).then_some(false),
        }
    }

    fn from_observation(
        target: &'static str,
        permission_confirmed: bool,
        session: MarketSession,
        evidence: MarketEvidence,
        failure: Option<Reason>,
    ) -> Self {
        let (verdict, reason) = assess(permission_confirmed, session, &evidence, failure);
        Self {
            target,
            verdict,
            reason,
            contract_confirmed: true,
            market_session: Some(session),
            market_data_type: evidence.market_data_type.map(MarketDataType::code),
            quote_received_at: evidence.quote_received_at,
            bid_ask_valid: evidence.bid_ask_valid,
            open_interest_received: (target == OPRA_TARGET)
                .then_some(evidence.open_interest_received),
            greeks_received: (target == OPRA_TARGET).then_some(evidence.greeks_received),
        }
    }
}

fn assess(
    permission_confirmed: bool,
    session: MarketSession,
    evidence: &MarketEvidence,
    failure: Option<Reason>,
) -> (Verdict, Reason) {
    if failure == Some(Reason::NotEntitled) || evidence.notice == Some(MarketNotice::NotEntitled) {
        return (Verdict::NotEntitled, Reason::NotEntitled);
    }
    if let Some(reason) = failure.filter(|reason| *reason != Reason::StreamEnded) {
        return (Verdict::Pending, reason);
    }
    match evidence.notice {
        Some(MarketNotice::PartialEntitlement) => {
            return (Verdict::Pending, Reason::PartialEntitlement);
        }
        Some(MarketNotice::CompetingSession) => {
            return (Verdict::Pending, Reason::CompetingSession);
        }
        Some(MarketNotice::Pacing) => return (Verdict::Pending, Reason::Pacing),
        _ => {}
    }
    if let Some(reason) = failure {
        return (Verdict::Pending, reason);
    }
    match session {
        MarketSession::Closed => return (Verdict::Pending, Reason::MarketClosed),
        MarketSession::Unknown => return (Verdict::Pending, Reason::MarketHoursUnknown),
        MarketSession::Open => {}
    }
    match evidence.market_data_type {
        None if evidence.saw_type_callback => (Verdict::Pending, Reason::UnknownMarketDataType),
        None => (Verdict::Pending, Reason::NoTypeCallback),
        Some(MarketDataType::Frozen) => (Verdict::Pending, Reason::FrozenData),
        Some(_) if !evidence.bid_ask_valid => (Verdict::Pending, Reason::NoValidBidAsk),
        Some(MarketDataType::Delayed) => (Verdict::Delayed, Reason::DelayedQuote),
        Some(MarketDataType::DelayedFrozen) => (Verdict::Delayed, Reason::DelayedFrozenQuote),
        Some(MarketDataType::Realtime)
            if evidence.notice == Some(MarketNotice::DelayedAvailable) =>
        {
            (Verdict::Pending, Reason::DelayedFallbackAvailable)
        }
        Some(MarketDataType::Realtime) if permission_confirmed => {
            (Verdict::Passed, Reason::RealtimeQuote)
        }
        Some(MarketDataType::Realtime) => (Verdict::Pending, Reason::PermissionUnconfirmed),
    }
}

#[derive(Debug, Serialize)]
pub struct ProductReport {
    pub product: Product,
    pub verdict: Verdict,
    pub account_permission_confirmed: bool,
    pub probes: Vec<ProbeEvidence>,
}

impl ProductReport {
    fn new(
        product: Product,
        account_permission_confirmed: bool,
        probes: Vec<ProbeEvidence>,
    ) -> Self {
        let verdict = [Verdict::NotEntitled, Verdict::Pending, Verdict::Delayed]
            .into_iter()
            .find(|verdict| probes.iter().any(|probe| probe.verdict == *verdict))
            .unwrap_or(Verdict::Passed);
        Self {
            product,
            verdict,
            account_permission_confirmed,
            probes,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ProbeReport {
    pub failure: Option<Reason>,
    pub products: [ProductReport; 2],
}

impl ProbeReport {
    fn new(
        options: ProbeOptions,
        equities: Vec<ProbeEvidence>,
        opra: ProbeEvidence,
        failure: Option<Reason>,
    ) -> Self {
        Self {
            failure,
            products: [
                ProductReport::new(
                    Product::EquityL1,
                    options.equity_l1_permission_confirmed,
                    equities,
                ),
                ProductReport::new(Product::Opra, options.opra_permission_confirmed, vec![opra]),
            ],
        }
    }

    fn unavailable(options: ProbeOptions, reason: Reason) -> Self {
        let opra_reason = if options.opra_con_id.is_some() {
            reason
        } else {
            Reason::OptionNotSelected
        };
        Self::new(
            options,
            EQUITIES
                .iter()
                .map(|equity| ProbeEvidence::pending(equity.instrument_id, reason))
                .collect(),
            ProbeEvidence::pending(OPRA_TARGET, opra_reason),
            Some(reason),
        )
    }
}

/// Run a bounded, one-line-at-a-time permission test through the paper account guard.
pub async fn probe_paper(options: ProbeOptions) -> ProbeReport {
    let config = match IbConfig::load() {
        Ok(config) => config,
        Err(_) => return ProbeReport::unavailable(options, Reason::InvalidConfiguration),
    };
    let read_timeout = Duration::from_millis(config.read_timeout_ms);
    let manager = match ConnectionManager::new(config, IbTransport) {
        Ok(manager) => manager,
        Err(_) => return ProbeReport::unavailable(options, Reason::InvalidConfiguration),
    };
    let market = match manager.open_market().await {
        Ok(market) => market,
        Err(error) => return ProbeReport::unavailable(options, error.into()),
    };
    probe_open_market(&market, options, read_timeout).await
}

pub(crate) async fn probe_open_market(
    market: &crate::market::PaperMarket,
    options: ProbeOptions,
    read_timeout: Duration,
) -> ProbeReport {
    // Each paper operation observes session loss. Finish the current row and
    // retain earlier evidence even when a global connectivity notice wins.
    let report = probe_products(market, options, read_timeout).await;
    market.close().await;
    report
}

/// Run the private probe using a read-only market source. Subscriptions are never concurrent.
pub async fn probe_products(
    source: &impl MarketDataSource,
    options: ProbeOptions,
    read_timeout: Duration,
) -> ProbeReport {
    let mut failure = None;
    let mut equities = Vec::with_capacity(EQUITIES.len());
    for equity in EQUITIES {
        if failure.is_some() {
            equities.push(ProbeEvidence::pending(
                equity.instrument_id,
                Reason::SweepStopped,
            ));
            continue;
        }
        let (evidence, stop) = probe_target(
            source,
            equity.instrument_id,
            ContractQuery::Equity {
                symbol: equity.symbol.into(),
                primary_exchange: equity.primary_exchange.into(),
            },
            options.equity_l1_permission_confirmed,
            read_timeout,
        )
        .await;
        equities.push(evidence);
        failure = stop;
    }
    let opra = match (options.opra_con_id, failure) {
        (None, _) => ProbeEvidence::pending(OPRA_TARGET, Reason::OptionNotSelected),
        (Some(_), Some(_)) => ProbeEvidence::pending(OPRA_TARGET, Reason::SweepStopped),
        (Some(con_id), None) => {
            let (evidence, stop) = probe_target(
                source,
                OPRA_TARGET,
                ContractQuery::Option { con_id },
                options.opra_permission_confirmed,
                read_timeout,
            )
            .await;
            failure = stop;
            evidence
        }
    };
    ProbeReport::new(options, equities, opra, failure)
}

async fn probe_target(
    source: &impl MarketDataSource,
    target: &'static str,
    query: ContractQuery,
    permission_confirmed: bool,
    read_timeout: Duration,
) -> (ProbeEvidence, Option<Reason>) {
    let contract = match timeout(read_timeout, source.confirm(&query)).await {
        Ok(Ok(contract)) => contract,
        Ok(Err(error)) => return target_failure(target, error.into(), None),
        Err(_) => return target_failure(target, Reason::RequestTimeout, None),
    };
    let session = contract.session_at(Utc::now());
    let request = match MarketRequest::new(
        contract,
        RequestParameters {
            option_open_interest: target == OPRA_TARGET,
        },
    ) {
        Ok(request) => request,
        Err(error) => return target_failure(target, error.into(), Some(session)),
    };
    let mut subscription = match timeout(read_timeout, source.subscribe(&request)).await {
        Ok(Ok(subscription)) => subscription,
        Ok(Err(MarketError::Timeout)) | Err(_) => {
            return target_failure(target, Reason::SubscriptionTimeout, Some(session));
        }
        Ok(Err(error)) => return target_failure(target, error.into(), Some(session)),
    };
    let started_at = Utc::now();
    let mut observation = MarketObservation::default();
    let deadline = Instant::now() + OBSERVATION_WINDOW;
    let failure = loop {
        if Instant::now() >= deadline {
            break None;
        }
        match timeout_at(deadline, subscription.next()).await {
            Ok(Ok(Some(event))) => observation.apply(event),
            Ok(Ok(None)) => break Some(Reason::StreamEnded),
            Ok(Err(error)) => break Some(error.into()),
            Err(_) => break None,
        }
    };
    let evidence = observation.evidence();
    let finished_at = Utc::now();
    let sessions = [
        started_at,
        evidence.quote_received_at.unwrap_or(started_at),
        finished_at,
    ]
    .map(|at| request.contract().session_at(at));
    let session = if sessions.contains(&MarketSession::Closed) {
        MarketSession::Closed
    } else if sessions.contains(&MarketSession::Unknown) {
        MarketSession::Unknown
    } else {
        MarketSession::Open
    };
    let stop = match (failure, timeout(read_timeout, subscription.cancel()).await) {
        (Some(Reason::GatewayDisconnected), _) | (_, Ok(Err(MarketError::Disconnected))) => {
            Some(Reason::GatewayDisconnected)
        }
        (_, Ok(Ok(()))) => None,
        (_, Ok(Err(_)) | Err(_)) => Some(Reason::CancelFailed),
    };
    (
        ProbeEvidence::from_observation(
            target,
            permission_confirmed,
            session,
            evidence,
            if failure == Some(Reason::NotEntitled) {
                failure
            } else {
                stop.or(failure)
            },
        ),
        stop,
    )
}

fn target_failure(
    target: &'static str,
    reason: Reason,
    session: Option<MarketSession>,
) -> (ProbeEvidence, Option<Reason>) {
    let mut evidence = ProbeEvidence::pending(target, reason);
    evidence.contract_confirmed = session.is_some();
    evidence.market_session = session;
    // A transport error starting a line leaves its allocation unknown. A
    // contract lookup has no line to release.
    let stop = match reason {
        Reason::SubscriptionTimeout | Reason::GatewayDisconnected => Some(reason),
        Reason::TransportFailure if session.is_some() => Some(reason),
        _ => None,
    };
    (evidence, stop)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market::MarketEvent;

    #[test]
    fn synthetic_diagnostics_distinguish_pending_reasons_and_explicit_denial() {
        for (session, callbacks, failure, verdict, reason) in [
            (
                MarketSession::Unknown,
                vec![],
                None,
                Verdict::Pending,
                Reason::MarketHoursUnknown,
            ),
            (
                MarketSession::Open,
                vec![MarketEvent::DataType(None)],
                None,
                Verdict::Pending,
                Reason::UnknownMarketDataType,
            ),
            (
                MarketSession::Open,
                vec![MarketEvent::Notice(MarketNotice::CompetingSession)],
                Some(Reason::StreamEnded),
                Verdict::Pending,
                Reason::CompetingSession,
            ),
            (
                MarketSession::Open,
                vec![MarketEvent::Notice(MarketNotice::Pacing)],
                Some(Reason::StreamEnded),
                Verdict::Pending,
                Reason::Pacing,
            ),
            (
                MarketSession::Open,
                vec![],
                Some(Reason::StreamEnded),
                Verdict::Pending,
                Reason::StreamEnded,
            ),
            (
                MarketSession::Closed,
                vec![],
                Some(MarketError::NotEntitled.into()),
                Verdict::NotEntitled,
                Reason::NotEntitled,
            ),
        ] {
            let mut observation = MarketObservation::default();
            for callback in callbacks {
                observation.apply(callback);
            }
            assert_eq!(
                assess(true, session, &observation.evidence(), failure),
                (verdict, reason),
            );
        }
    }

    #[test]
    fn delayed_fallback_notice_prevents_realtime_pass_with_valid_quotes() {
        // Fixed synthetic summary from a request with a type-1 callback and valid sides.
        let evidence = MarketEvidence {
            market_data_type: Some(MarketDataType::Realtime),
            bid_ask_valid: true,
            notice: Some(MarketNotice::DelayedAvailable),
            saw_type_callback: true,
            ..MarketEvidence::default()
        };
        assert_eq!(
            assess(true, MarketSession::Open, &evidence, None),
            (Verdict::Pending, Reason::DelayedFallbackAvailable),
        );
    }
}
