//! The read-only boundary between IB's SDK and finance market requests.

#[path = "chain.rs"]
mod chain;

use super::{
    ConfirmedContract, ContractKind, ContractQuery, GreeksSource, LiquidHours, MarketDataSource,
    MarketDataType, MarketError, MarketEvent, MarketNotice, MarketRequest, MarketSubscription,
    OptionContract, OptionGreeks, OptionRight, QuoteField,
};
use crate::{PaperPeer, Session};
use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use finance_market_protocol::DecimalText;
use ibapi::{
    contracts::{
        Contract, ContractDetails, OptionComputation, OptionRight as IbRight, SecurityType,
        tick_types::TickType,
    },
    market_data::{
        MarketDataType as IbDataType,
        realtime::{ExactTickTypes, TickTypes, generic_tick},
    },
    prelude::StreamExt,
    subscriptions::{Subscription, SubscriptionItem},
};
use std::{collections::HashMap, future::Future, sync::Arc, time::Duration};
use time::{Date, PrimitiveDateTime, Time, macros::format_description};
use time_tz::{OffsetDateTimeExt, PrimitiveDateTimeExt, Tz, timezones};
use tokio::{
    sync::{Mutex, watch},
    time::{Instant, timeout},
};

/// A verified paper session with streaming L1 access. Obtain it from the
/// connection manager so account verification precedes market requests.
pub struct PaperMarket {
    peer: Arc<PaperPeer>,
    timeout: Duration,
    confirmed: Mutex<HashMap<i32, ConfirmedContract>>,
}

impl PaperMarket {
    pub(crate) async fn new(peer: Arc<PaperPeer>, bound: Duration) -> Result<Self, MarketError> {
        let market = Self {
            peer,
            timeout: bound,
            confirmed: Mutex::new(HashMap::new()),
        };
        // This is a session-wide preference. Gateway can still return type 1
        // for an entitled request. Only callbacks identify the actual type.
        market
            .peer
            .while_connected(bounded_request(
                bound,
                market
                    .peer
                    .client
                    .switch_market_data_type(IbDataType::Delayed),
            ))
            .await?;
        Ok(market)
    }

    pub async fn close(&self) {
        self.peer.close().await;
    }

    /// The session owner can select on this alongside its market requests.
    /// Global farm notices are separate from request-scoped data and notices.
    pub async fn disconnected(&self) -> MarketError {
        self.peer.disconnected().await;
        MarketError::Disconnected
    }

    /// The latest connection-wide pacing notice seen by the session observer.
    /// Poll `disconnected` alongside session operations to drive that observer.
    pub fn pacing_notices(&self) -> watch::Receiver<Option<Instant>> {
        self.peer.market_pacing.subscribe()
    }
}

impl PaperPeer {
    // Observe queued notices before starting IO, interrupt pending IO, and
    // latch a disconnect reported by the operation even on a writable socket.
    async fn while_connected<T>(
        &self,
        operation: impl Future<Output = Result<T, MarketError>>,
    ) -> Result<T, MarketError> {
        let result = tokio::select! {
            biased;
            _ = self.disconnected() => return Err(MarketError::Disconnected),
            result = operation => result,
        };
        if matches!(result, Err(MarketError::Disconnected)) || !self.is_active() {
            self.invalidate();
            return Err(MarketError::Disconnected);
        }
        result
    }
}

async fn bounded_request<T>(
    bound: Duration,
    request: impl Future<Output = Result<T, ibapi::Error>>,
) -> Result<T, MarketError> {
    timeout(bound, request)
        .await
        .map_err(|_| MarketError::Timeout)?
        .map_err(transport_error)
}

fn sdk_query(query: &ContractQuery) -> Result<Contract, MarketError> {
    match query {
        ContractQuery::Equity {
            symbol,
            primary_exchange,
        } if !symbol.is_empty()
            && symbol
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b" .-".contains(&b))
            && !primary_exchange.is_empty()
            && primary_exchange
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit()) =>
        {
            Ok(Contract::stock(symbol)
                .primary(primary_exchange)
                .in_currency("USD")
                .build())
        }
        ContractQuery::Option { con_id } if *con_id > 0 => Ok(Contract {
            contract_id: *con_id,
            security_type: SecurityType::Option,
            ..Contract::default()
        }),
        _ => Err(MarketError::InvalidRequest),
    }
}

fn transport_error(error: ibapi::Error) -> MarketError {
    match error {
        error if crate::is_disconnect(&error) => MarketError::Disconnected,
        ibapi::Error::ResponseLagged(_) => MarketError::IncompleteResponse,
        ibapi::Error::Io(io)
            if matches!(
                io.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            ) =>
        {
            MarketError::Timeout
        }
        ibapi::Error::Notice(notice) if notice.code == 200 => MarketError::UnconfirmedContract,
        ibapi::Error::Notice(notice) => match request_notice_kind(&notice) {
            MarketNotice::NotEntitled => MarketError::NotEntitled,
            MarketNotice::Pacing => MarketError::Pacing,
            _ => MarketError::Transport,
        },
        _ => MarketError::Transport,
    }
}

fn notice_kind(code: i32) -> MarketNotice {
    match code {
        354 | 10168 | 10186 => MarketNotice::NotEntitled,
        10089 | 10167 => MarketNotice::DelayedAvailable,
        10090 => MarketNotice::PartialEntitlement,
        10197 => MarketNotice::CompetingSession,
        100 | 101 | 420 => MarketNotice::Pacing,
        _ => MarketNotice::Other,
    }
}

pub(crate) fn request_notice_kind(notice: &ibapi::Notice) -> MarketNotice {
    if notice.code == 162 && notice.message.to_ascii_lowercase().contains("pacing") {
        return MarketNotice::Pacing;
    }
    notice_kind(notice.code)
}

#[async_trait]
impl MarketDataSource for PaperMarket {
    type Subscription = PaperSubscription;

    async fn confirm(&self, query: &ContractQuery) -> Result<ConfirmedContract, MarketError> {
        self.peer
            .while_connected(async {
                let contract = sdk_query(query)?;
                let details =
                    bounded_request(self.timeout, self.peer.client.contract_details(&contract))
                        .await?;
                let confirmation = confirm_details(query, details, Utc::now())?;
                self.confirmed
                    .lock()
                    .await
                    .insert(confirmation.con_id, confirmation.clone());
                Ok(confirmation)
            })
            .await
    }

    async fn subscribe(&self, request: &MarketRequest) -> Result<Self::Subscription, MarketError> {
        if !self.peer.is_active() {
            return Err(MarketError::Disconnected);
        }
        let confirmed = request.contract();
        if self.confirmed.lock().await.get(&confirmed.con_id) != Some(confirmed) {
            return Err(MarketError::UnconfirmedContract);
        }
        if let ContractKind::Option(option) = &confirmed.kind {
            check_expiry(option.expiry, Utc::now())?;
        }
        let interest_right = match (&confirmed.kind, request.parameters().option_open_interest) {
            (ContractKind::Option(option), true) => Some(option.right),
            (ContractKind::Equity, true) => return Err(MarketError::InvalidRequest),
            (_, false) => None,
        };
        let contract = sdk_market_contract(confirmed);
        let mut builder = self.peer.client.market_data(&contract).streaming();
        if interest_right.is_some() {
            builder = builder.generic_ticks(&[generic_tick::OPTION_OPEN_INTEREST]);
        }
        let mut write = LineWrite::new(&self.peer);
        let subscription = self
            .peer
            .while_connected(bounded_request(self.timeout, builder.subscribe_exact()))
            .await?;
        let request_id = subscription.request_id().ok_or(MarketError::Transport)?;
        write.completed = true;
        Ok(PaperSubscription {
            subscription: Some(subscription),
            peer: self.peer.clone(),
            timeout: self.timeout,
            request_id,
            interest_right,
            pending: None,
        })
    }
}

fn contract_matches(query: &ContractQuery, details: &ContractDetails) -> bool {
    let contract = &details.contract;
    if contract.contract_id <= 0
        || contract.symbol.as_str().is_empty()
        || contract.currency.as_str() != "USD"
        || !(contract.exchange.as_str() == "SMART"
            || details.valid_exchanges.iter().any(|venue| venue == "SMART"))
    {
        return false;
    }
    match query {
        ContractQuery::Equity {
            symbol,
            primary_exchange,
        } => {
            let primary = contract.primary_exchange.as_str();
            contract.security_type == SecurityType::Stock
                && contract.symbol.as_str() == symbol
                && (primary == primary_exchange
                    || matches!(
                        (primary, primary_exchange.as_str()),
                        ("NASDAQ", "ISLAND") | ("ISLAND", "NASDAQ")
                    ))
        }
        ContractQuery::Option { con_id } => {
            contract.security_type == SecurityType::Option
                && contract.contract_id == *con_id
                && details.under_security_type == "STK"
                && details.under_contract_id > 0
        }
    }
}

fn confirm_details(
    query: &ContractQuery,
    details: Vec<ContractDetails>,
    now: DateTime<Utc>,
) -> Result<ConfirmedContract, MarketError> {
    let mut matches = details
        .into_iter()
        .filter(|details| contract_matches(query, details));
    let Some(details) = matches.next() else {
        return Err(MarketError::UnconfirmedContract);
    };
    if matches.next().is_some() {
        return Err(MarketError::UnconfirmedContract);
    }
    let kind = match query {
        ContractQuery::Equity { .. } => ContractKind::Equity,
        ContractQuery::Option { .. } => ContractKind::Option(option_details(&details, now)?),
    };
    Ok(confirmed_contract(details, kind))
}

fn confirmed_contract(details: ContractDetails, kind: ContractKind) -> ConfirmedContract {
    let liquid_hours = liquid_hours(&details.liquid_hours, &details.time_zone_id);
    let contract = details.contract;
    ConfirmedContract {
        con_id: contract.contract_id,
        symbol: contract.symbol.as_str().to_owned(),
        primary_exchange: if contract.primary_exchange.is_empty() {
            None
        } else {
            Some(contract.primary_exchange.as_str().to_owned())
        },
        kind,
        liquid_hours,
    }
}

fn sdk_market_contract(confirmed: &ConfirmedContract) -> Contract {
    let security_type = match confirmed.kind {
        ContractKind::Equity => SecurityType::Stock,
        ContractKind::Option(_) => SecurityType::Option,
    };
    Contract {
        contract_id: confirmed.con_id,
        security_type,
        exchange: "SMART".into(),
        currency: "USD".into(),
        ..Contract::default()
    }
}

fn option_details(
    details: &ContractDetails,
    now: DateTime<Utc>,
) -> Result<OptionContract, MarketError> {
    let contract = &details.contract;
    let expiry_text = if details.real_expiration_date.is_empty() {
        &contract.last_trade_date_or_contract_month
    } else {
        &details.real_expiration_date
    };
    let expiry = NaiveDate::parse_from_str(expiry_text, "%Y%m%d")
        .map_err(|_| MarketError::UnconfirmedContract)?;
    check_expiry(expiry, now)?;
    let strike = positive_decimal(contract.strike).ok_or(MarketError::UnconfirmedContract)?;
    let multiplier: DecimalText = contract
        .multiplier
        .parse()
        .map_err(|_| MarketError::UnconfirmedContract)?;
    if multiplier.value() <= 0.into() {
        return Err(MarketError::UnconfirmedContract);
    }
    let right = match contract.right {
        Some(IbRight::Call) => OptionRight::Call,
        Some(IbRight::Put) => OptionRight::Put,
        _ => return Err(MarketError::UnconfirmedContract),
    };
    Ok(OptionContract {
        expiry,
        strike,
        right,
        multiplier,
        underlying_con_id: details.under_contract_id,
    })
}

fn check_expiry(expiry: NaiveDate, now: DateTime<Utc>) -> Result<(), MarketError> {
    if expiry < option_market_date(now)? {
        return Err(MarketError::ExpiredContract);
    }
    Ok(())
}

/// Eastern civil date used for U.S. equity option expiries.
pub fn option_market_date(now: DateTime<Utc>) -> Result<NaiveDate, MarketError> {
    // U.S. option dates are Eastern civil dates, including in summer.
    let now = time::OffsetDateTime::from_unix_timestamp(now.timestamp())
        .map_err(|_| MarketError::InvalidRequest)?
        .to_timezone(timezones::db::america::NEW_YORK);
    NaiveDate::from_ymd_opt(
        now.year(),
        u32::from(u8::from(now.month())),
        u32::from(now.day()),
    )
    .ok_or(MarketError::InvalidRequest)
}

fn liquid_hours(rows: &[String], zone: &str) -> Vec<LiquidHours> {
    let tz = match zone {
        "EST" | "EDT" | "EST5EDT" => Some(timezones::db::america::NEW_YORK),
        "CST" | "CDT" | "CST6CDT" => Some(timezones::db::america::CHICAGO),
        name => timezones::get_by_name(name),
    };
    let Some(tz) = tz else {
        return Vec::new();
    };
    rows.iter()
        .flat_map(|row| row.split(';'))
        .filter_map(|row| liquid_hours_row(row, tz))
        .flatten()
        .collect()
}

fn local_utc(date: Date, clock: Time, tz: &Tz) -> Option<DateTime<Utc>> {
    let time_tz::OffsetResult::Some(datetime) =
        PrimitiveDateTime::new(date, clock).assume_timezone(tz)
    else {
        return None;
    };
    DateTime::from_timestamp(datetime.unix_timestamp(), 0)
}

fn parse_date(text: &str) -> Option<Date> {
    Date::parse(text, format_description!("[year][month][day]")).ok()
}

fn parse_endpoint(text: &str, default_date: Date) -> Option<(Date, Time)> {
    let (date, clock) = match text.split_once(':') {
        Some((date, clock)) => (parse_date(date)?, clock),
        None => (default_date, text),
    };
    Some((
        date,
        Time::parse(clock, format_description!("[hour][minute]")).ok()?,
    ))
}

fn liquid_hours_row(row: &str, tz: &Tz) -> Option<Vec<LiquidHours>> {
    let (date, intervals) = row.split_once(':')?;
    let date = parse_date(date)?;
    let day_start = local_utc(date, Time::MIDNIGHT, tz)?;
    let day_end = local_utc(date.next_day()?, Time::MIDNIGHT, tz)?;
    if intervals == "CLOSED" {
        return Some(vec![LiquidHours::Closed {
            start: day_start,
            end: day_end,
        }]);
    }

    let mut sessions = Vec::new();
    for interval in intervals.split(',') {
        let (start, end) = interval.split_once('-')?;
        let (start_date, start_time) = parse_endpoint(start, date)?;
        let (mut end_date, end_time) = parse_endpoint(end, start_date)?;
        if !end.contains(':') && end_time < start_time {
            end_date = end_date.next_day()?;
        }
        let start = local_utc(start_date, start_time, tz)?;
        let end = local_utc(end_date, end_time, tz)?;
        if start < day_start || start >= day_end || start >= end {
            return None;
        }
        sessions.push((start, end));
    }
    sessions.sort_unstable();
    let mut cursor = day_start;
    let mut hours = Vec::new();
    for (start, end) in sessions {
        if cursor < start {
            hours.push(LiquidHours::Closed {
                start: cursor,
                end: start,
            });
        }
        hours.push(LiquidHours::Open { start, end });
        cursor = cursor.max(end);
    }
    if cursor < day_end {
        hours.push(LiquidHours::Closed {
            start: cursor,
            end: day_end,
        });
    }
    Some(hours)
}

/// A single cancellable request. `request_id` is unique within the paper session.
pub struct PaperSubscription {
    subscription: Option<Subscription<ExactTickTypes>>,
    peer: Arc<PaperPeer>,
    timeout: Duration,
    request_id: i32,
    interest_right: Option<OptionRight>,
    pending: Option<MarketEvent>,
}

// An interrupted stream request or cancellation may already be on the wire.
// No other line can rely on this session's allocation until it is reopened.
struct LineWrite<'a> {
    peer: &'a PaperPeer,
    completed: bool,
}

impl<'a> LineWrite<'a> {
    fn new(peer: &'a PaperPeer) -> Self {
        Self {
            peer,
            completed: false,
        }
    }
}

impl Drop for LineWrite<'_> {
    fn drop(&mut self) {
        if !self.completed {
            self.peer.invalidate();
        }
    }
}

impl Drop for PaperSubscription {
    fn drop(&mut self) {
        if self.subscription.is_some() {
            // The SDK makes a best-effort cancel on drop, without a write result.
            self.peer.invalidate();
        }
    }
}

#[async_trait]
impl MarketSubscription for PaperSubscription {
    fn request_id(&self) -> i32 {
        self.request_id
    }

    async fn next(&mut self) -> Result<Option<MarketEvent>, MarketError> {
        self.peer
            .while_connected(async {
                if let Some(event) = self.pending.take() {
                    return Ok(Some(event));
                }
                let Some(subscription) = &mut self.subscription else {
                    return Ok(None);
                };
                while let Some(item) = subscription.next().await {
                    match item {
                        Ok(SubscriptionItem::Data(tick)) => {
                            let (event, pending) =
                                tick_events(tick, self.interest_right, Utc::now());
                            if let Some(event) = event {
                                self.pending = pending;
                                return Ok(Some(event));
                            }
                        }
                        Ok(SubscriptionItem::Notice(notice))
                        | Err(ibapi::Error::Notice(notice)) => {
                            if crate::invalidates_market_session(notice.code) {
                                return Err(MarketError::Disconnected);
                            }
                            return Ok(Some(MarketEvent::Notice(request_notice_kind(&notice))));
                        }
                        Err(error) => return Err(transport_error(error)),
                    }
                }
                Ok(None)
            })
            .await
    }

    async fn cancel(&mut self) -> Result<(), MarketError> {
        self.pending = None;
        let Some(subscription) = self.subscription.take() else {
            return self.peer.while_connected(async { Ok(()) }).await;
        };
        let mut write = LineWrite::new(&self.peer);
        self.peer
            .while_connected(bounded_request(self.timeout, subscription.try_cancel()))
            .await?;
        write.completed = true;
        Ok(())
    }
}

// The SDK has already decoded values as binary64. Convert once into the shortest
// round-trippable decimal without doing financial arithmetic in binary64.
fn decimal(value: f64) -> Option<DecimalText> {
    let decimal: DecimalText = value.to_string().parse().ok()?;
    (decimal.as_str().parse::<f64>().ok()? == value).then_some(decimal)
}

fn positive_decimal(value: f64) -> Option<DecimalText> {
    decimal(value).filter(|decimal| decimal.value() > 0.into())
}

fn nonnegative_decimal(value: f64) -> Option<DecimalText> {
    decimal(value).filter(|decimal| decimal.value() >= 0.into())
}

fn quantity(value: Option<String>) -> Option<DecimalText> {
    let value: DecimalText = value?.try_into().ok()?;
    (value.value() >= 0.into()).then_some(value)
}

fn generic_interest(value: f64) -> Option<DecimalText> {
    // TickGeneric uses binary64 on the wire. Recover its exact integer when
    // representable, without a rounded f64 display string for large counts.
    if value >= 0.0 && value < u128::MAX as f64 && value.fract() == 0.0 {
        return (value as u128).to_string().try_into().ok();
    }
    None
}

fn tick_events(
    tick: ExactTickTypes,
    interest_right: Option<OptionRight>,
    now: DateTime<Utc>,
) -> (Option<MarketEvent>, Option<MarketEvent>) {
    let single = match tick {
        ExactTickTypes::Other(TickTypes::MarketDataType(data_type)) => {
            Some(MarketEvent::DataType(match data_type {
                IbDataType::Realtime => Some(MarketDataType::Realtime),
                IbDataType::Frozen => Some(MarketDataType::Frozen),
                IbDataType::Delayed => Some(MarketDataType::Delayed),
                IbDataType::DelayedFrozen => Some(MarketDataType::DelayedFrozen),
                IbDataType::Unknown => None,
            }))
        }
        ExactTickTypes::Price { tick, size } => {
            let price = quote_event(&tick.tick_type, positive_decimal(tick.price), now);
            let size = size
                .and_then(|(tick_type, size)| quote_event(&tick_type, quantity(Some(size)), now));
            return if price.is_some() {
                (price, size)
            } else {
                (size, None)
            };
        }
        ExactTickTypes::Size { tick_type, size } => {
            let value = quantity(size);
            if let Some((field, delayed)) = quote_field(&tick_type) {
                Some(MarketEvent::Quote {
                    field,
                    value,
                    delayed,
                    received_at: now,
                })
            } else {
                interest_event(&tick_type, value, interest_right, now)
            }
        }
        ExactTickTypes::Generic { tick_type, value } => interest_event(
            &tick_type,
            value.and_then(generic_interest),
            interest_right,
            now,
        ),
        ExactTickTypes::Other(TickTypes::OptionComputation(tick)) => option_greeks(tick, now)
            .map(Box::new)
            .map(MarketEvent::Greeks),
        ExactTickTypes::Other(_) => None,
    };
    (single, None)
}

fn quote_event(
    tick_type: &TickType,
    value: Option<DecimalText>,
    received_at: DateTime<Utc>,
) -> Option<MarketEvent> {
    let (field, delayed) = quote_field(tick_type)?;
    Some(MarketEvent::Quote {
        field,
        value,
        delayed,
        received_at,
    })
}

fn quote_field(tick_type: &TickType) -> Option<(QuoteField, bool)> {
    use QuoteField::*;
    Some(match tick_type {
        TickType::Bid => (BidPrice, false),
        TickType::Ask => (AskPrice, false),
        TickType::Last => (LastPrice, false),
        TickType::BidSize => (BidSize, false),
        TickType::AskSize => (AskSize, false),
        TickType::LastSize => (LastSize, false),
        TickType::DelayedBid => (BidPrice, true),
        TickType::DelayedAsk => (AskPrice, true),
        TickType::DelayedLast => (LastPrice, true),
        TickType::DelayedBidSize => (BidSize, true),
        TickType::DelayedAskSize => (AskSize, true),
        TickType::DelayedLastSize => (LastSize, true),
        _ => return None,
    })
}

fn interest_event(
    tick_type: &TickType,
    value: Option<DecimalText>,
    interest_right: Option<OptionRight>,
    received_at: DateTime<Utc>,
) -> Option<MarketEvent> {
    let right = interest_right?;
    match (tick_type, right) {
        (TickType::OptionCallOpenInterest, OptionRight::Call)
        | (TickType::OptionPutOpenInterest, OptionRight::Put) => Some(MarketEvent::OpenInterest {
            right,
            value: value.filter(|decimal| decimal.value().fract().is_zero()),
            received_at,
        }),
        _ => None,
    }
}

fn option_greeks(tick: OptionComputation, received_at: DateTime<Utc>) -> Option<OptionGreeks> {
    let (source, delayed) = match tick.field {
        TickType::BidOption => (GreeksSource::Bid, false),
        TickType::AskOption => (GreeksSource::Ask, false),
        TickType::LastOption => (GreeksSource::Last, false),
        TickType::ModelOption => (GreeksSource::Model, false),
        TickType::DelayedBidOption => (GreeksSource::Bid, true),
        TickType::DelayedAskOption => (GreeksSource::Ask, true),
        TickType::DelayedLastOption => (GreeksSource::Last, true),
        TickType::DelayedModelOption => (GreeksSource::Model, true),
        _ => return None,
    };
    Some(OptionGreeks {
        source,
        delayed,
        received_at,
        implied_volatility: tick.implied_volatility.and_then(nonnegative_decimal),
        delta: tick
            .delta
            .and_then(decimal)
            .filter(|value| value.value().abs() <= 1.into()),
        gamma: tick.gamma.and_then(nonnegative_decimal),
        vega: tick.vega.and_then(nonnegative_decimal),
        theta: tick.theta.and_then(decimal),
        option_price: tick.option_price.and_then(nonnegative_decimal),
        underlying_price: tick.underlying_price.and_then(positive_decimal),
        pv_dividend: tick.present_value_dividend.and_then(nonnegative_decimal),
    })
}

#[cfg(test)]
#[path = "replay_tests.rs"]
mod replay_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market::MarketSession;
    use ibapi::market_data::realtime::TickPrice;

    // Deterministic synthetic fixtures. Contract IDs, prices and hours are
    // hand-authored to exercise SDK mapping boundaries. No recordings or seed.
    fn utc(time: &str) -> DateTime<Utc> {
        time.parse().unwrap()
    }

    fn equity() -> ContractDetails {
        ContractDetails {
            contract: Contract {
                contract_id: 91001,
                symbol: "SYNTH".into(),
                primary_exchange: "ISLAND".into(),
                ..Contract::default()
            },
            ..ContractDetails::default()
        }
    }

    fn option() -> ContractDetails {
        ContractDetails {
            contract: Contract {
                contract_id: 91002,
                symbol: "SYNTH".into(),
                security_type: SecurityType::Option,
                right: Some(IbRight::Put),
                strike: 10.125,
                multiplier: "100".into(),
                exchange: "SYNTH_ROUTE".into(),
                ..Contract::default()
            },
            real_expiration_date: "20400210".into(),
            valid_exchanges: vec!["SMART".into()],
            under_security_type: "STK".into(),
            under_contract_id: 91001,
            ..ContractDetails::default()
        }
    }

    #[test]
    fn confirms_unique_smart_usd_equity_and_rejects_mismatches() {
        let query = ContractQuery::Equity {
            symbol: "SYNTH".into(),
            primary_exchange: "NASDAQ".into(),
        };
        let now = utc("2040-01-20T15:00:00Z");
        let detail = equity();
        let confirmed = confirm_details(&query, vec![detail.clone()], now).unwrap();
        let outbound = sdk_market_contract(&confirmed);
        assert_eq!(confirmed.con_id, 91001);
        assert_eq!(confirmed.primary_exchange.as_deref(), Some("ISLAND"));
        assert_eq!(outbound.exchange.as_str(), "SMART");
        assert_eq!(confirmed.kind, ContractKind::Equity);
        assert_eq!(
            confirm_details(&query, vec![detail.clone(), detail], now),
            Err(MarketError::UnconfirmedContract)
        );
        let mut detail = equity();
        detail.contract.currency = "CAD".into();
        assert_eq!(
            confirm_details(&query, vec![detail], now),
            Err(MarketError::UnconfirmedContract)
        );
        let mut detail = equity();
        detail.contract.primary_exchange = "NYSE".into();
        assert_eq!(
            confirm_details(&query, vec![detail], now),
            Err(MarketError::UnconfirmedContract)
        );
    }

    #[test]
    fn confirms_opra_con_id_terms_and_rejects_wrong_or_expired_contract() {
        let query = ContractQuery::Option { con_id: 91002 };
        let now = utc("2040-02-11T02:00:00Z"); // Feb 10 in New York.
        let contract = confirm_details(&query, vec![option()], now).unwrap();
        let outbound = sdk_market_contract(&contract);
        assert_eq!(outbound.exchange.as_str(), "SMART");
        let ContractKind::Option(terms) = contract.kind else {
            panic!("expected synthetic option terms");
        };
        assert_eq!(terms.strike.as_str(), "10.125");
        assert_eq!(terms.multiplier.as_str(), "100");
        assert_eq!(terms.right, OptionRight::Put);
        assert_eq!(terms.underlying_con_id, 91001);
        assert_eq!(
            confirm_details(&query, vec![option()], utc("2040-02-11T05:00:00Z")),
            Err(MarketError::ExpiredContract)
        );
        let mut detail = option();
        detail.contract.contract_id += 1;
        assert_eq!(
            confirm_details(&query, vec![detail], now),
            Err(MarketError::UnconfirmedContract)
        );
        let mut detail = option();
        detail.under_security_type = "IND".into();
        assert_eq!(
            confirm_details(&query, vec![detail], now),
            Err(MarketError::UnconfirmedContract)
        );
        let mut detail = option();
        detail.valid_exchanges.clear();
        assert_eq!(
            confirm_details(&query, vec![detail], now),
            Err(MarketError::UnconfirmedContract)
        );
    }

    #[test]
    fn liquid_hours_use_contract_timezone_and_explicit_session_evidence() {
        let mut detail = equity();
        detail.time_zone_id = "EST".into();
        detail.liquid_hours = vec![
            "20400120:0930-1300;20400121:CLOSED".into(),
            "20400720:0930-20400720:1600".into(),
            "20400721:0930-not-a-time".into(),
        ];
        let query = ContractQuery::Equity {
            symbol: "SYNTH".into(),
            primary_exchange: "NASDAQ".into(),
        };
        let contract = confirm_details(&query, vec![detail], utc("2040-01-20T15:00:00Z")).unwrap();
        for (time, expected) in [
            ("2040-01-20T14:29:59Z", MarketSession::Closed),
            ("2040-01-20T14:30:00Z", MarketSession::Open),
            ("2040-01-20T18:00:00Z", MarketSession::Closed),
            ("2040-01-21T15:00:00Z", MarketSession::Closed),
            ("2040-01-22T15:00:00Z", MarketSession::Unknown),
            ("2040-07-20T13:30:00Z", MarketSession::Open),
            ("2040-07-20T20:00:00Z", MarketSession::Closed),
            ("2040-07-21T16:00:00Z", MarketSession::Unknown),
        ] {
            assert_eq!(contract.session_at(utc(time)), expected, "{time}");
        }
        assert!(liquid_hours(&["20400121:CLOSED".into()], "invalid-zone").is_empty());
    }

    #[test]
    fn native_market_type_callback_is_the_only_type_evidence() {
        let now = utc("2040-01-20T15:00:00Z");
        for (native, expected) in [
            (IbDataType::Realtime, Some(MarketDataType::Realtime)),
            (IbDataType::Frozen, Some(MarketDataType::Frozen)),
            (IbDataType::Delayed, Some(MarketDataType::Delayed)),
            (
                IbDataType::DelayedFrozen,
                Some(MarketDataType::DelayedFrozen),
            ),
            (IbDataType::Unknown, None),
        ] {
            assert_eq!(
                tick_events(
                    ExactTickTypes::Other(TickTypes::MarketDataType(native)),
                    None,
                    now
                ),
                (Some(MarketEvent::DataType(expected)), None)
            );
        }
        let (price, size) = tick_events(
            ExactTickTypes::Price {
                tick: TickPrice {
                    tick_type: TickType::DelayedBid,
                    price: 5.125,
                    ..TickPrice::default()
                },
                size: Some((TickType::DelayedBidSize, "2.25".into())),
            },
            None,
            now,
        );
        for (event, field, expected) in [
            (price, QuoteField::BidPrice, "5.125"),
            (size, QuoteField::BidSize, "2.25"),
        ] {
            assert_eq!(
                event,
                Some(MarketEvent::Quote {
                    field,
                    value: Some(expected.parse().unwrap()),
                    delayed: true,
                    received_at: now,
                })
            );
        }
        assert!(decimal(f64::MAX).is_none());
        assert!(decimal(f64::NAN).is_none());
        assert!(positive_decimal(-1.0).is_none());
    }

    #[test]
    fn option_observations_preserve_precision_and_report_missing_measurements() {
        let now = utc("2040-01-20T15:00:00Z");
        let right = Some(OptionRight::Put);
        let size = |tick_type, size: Option<&str>| ExactTickTypes::Size {
            tick_type,
            size: size.map(str::to_owned),
        };
        assert_eq!(
            tick_events(
                size(TickType::OptionPutOpenInterest, Some("123")),
                right,
                now
            )
            .0,
            Some(MarketEvent::OpenInterest {
                right: OptionRight::Put,
                value: Some("123".parse().unwrap()),
                received_at: now
            })
        );
        assert_eq!(
            tick_events(
                size(TickType::OptionCallOpenInterest, Some("123")),
                right,
                now
            ),
            (None, None)
        );
        for value in [Some("-1"), None, Some("1.5"), Some("NaN")] {
            assert_eq!(
                tick_events(size(TickType::OptionPutOpenInterest, value), right, now).0,
                Some(MarketEvent::OpenInterest {
                    right: OptionRight::Put,
                    value: None,
                    received_at: now
                })
            );
        }
        assert_eq!(
            tick_events(size(TickType::OptionPutOpenInterest, Some("0")), right, now).0,
            Some(MarketEvent::OpenInterest {
                right: OptionRight::Put,
                value: Some("0".parse().unwrap()),
                received_at: now,
            })
        );
        assert_eq!(
            generic_interest(1_000_000_000_000_000_128.0)
                .unwrap()
                .as_str(),
            "1000000000000000128"
        );
        let greeks = option_greeks(
            OptionComputation {
                field: TickType::DelayedModelOption,
                implied_volatility: Some(-1.0),
                delta: Some(0.0),
                option_price: Some(0.0),
                present_value_dividend: None,
                gamma: Some(0.0125),
                vega: Some(f64::MAX),
                theta: Some(-0.25),
                underlying_price: Some(123.5),
                tick_attribute: None,
            },
            now,
        )
        .unwrap();
        assert_eq!(greeks.source, GreeksSource::Model);
        assert!(greeks.delayed);
        assert_eq!(greeks.delta.unwrap().as_str(), "0");
        assert_eq!(greeks.option_price.unwrap().as_str(), "0");
        assert_eq!(greeks.gamma.unwrap().as_str(), "0.0125");
        assert_eq!(greeks.theta.unwrap().as_str(), "-0.25");
        assert!(greeks.implied_volatility.is_none());
        assert!(greeks.vega.is_none());
    }

    #[test]
    fn permission_notices_distinguish_delayed_fallback_and_competing_session() {
        assert_eq!(notice_kind(354), MarketNotice::NotEntitled);
        assert_eq!(notice_kind(10168), MarketNotice::NotEntitled);
        assert_eq!(notice_kind(10089), MarketNotice::DelayedAvailable);
        assert_eq!(notice_kind(10167), MarketNotice::DelayedAvailable);
        assert_eq!(notice_kind(10090), MarketNotice::PartialEntitlement);
        assert_eq!(notice_kind(10197), MarketNotice::CompetingSession);
        assert_eq!(notice_kind(420), MarketNotice::Pacing);
    }
}
