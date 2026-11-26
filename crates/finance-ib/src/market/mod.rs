//! Read-only, request-scoped IBKR L1 for equity and US equity options.

mod ib;

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use finance_market_protocol::DecimalText;
use serde::Serialize;

pub use ib::{PaperDepthSubscription, PaperMarket};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Product {
    EquityL1,
    Opra,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContractQuery {
    Equity {
        symbol: String,
        primary_exchange: String,
    },
    Option {
        con_id: i32,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfirmedContract {
    pub con_id: i32,
    pub symbol: String,
    pub primary_exchange: Option<String>,
    pub kind: ContractKind,
    pub liquid_hours: Vec<LiquidHours>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContractKind {
    Equity,
    Option(OptionContract),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OptionContract {
    pub expiry: NaiveDate,
    pub strike: DecimalText,
    pub right: OptionRight,
    pub multiplier: DecimalText,
    pub underlying_con_id: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OptionRight {
    Call,
    Put,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LiquidHours {
    Open {
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    },
    Closed {
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MarketSession {
    Open,
    Closed,
    Unknown,
}

impl ConfirmedContract {
    pub fn product(&self) -> Product {
        match self.kind {
            ContractKind::Equity => Product::EquityL1,
            ContractKind::Option(_) => Product::Opra,
        }
    }

    pub fn session_at(&self, time: DateTime<Utc>) -> MarketSession {
        let mut session = MarketSession::Unknown;
        for interval in &self.liquid_hours {
            match *interval {
                LiquidHours::Open { start, end } if (start..end).contains(&time) => {
                    return MarketSession::Open;
                }
                LiquidHours::Closed { start, end } if (start..end).contains(&time) => {
                    session = MarketSession::Closed;
                }
                _ => {}
            }
        }
        session
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct RequestParameters {
    /// Request generic tick 101 on a streaming option line.
    pub option_open_interest: bool,
}

/// Key for sharing compatible market data lines within one active session.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RequestKey {
    pub con_id: i32,
    pub product: Product,
    pub parameters: RequestParameters,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MarketRequest {
    contract: ConfirmedContract,
    parameters: RequestParameters,
}

impl MarketRequest {
    pub fn new(
        contract: ConfirmedContract,
        parameters: RequestParameters,
    ) -> Result<Self, MarketError> {
        if contract.con_id <= 0
            || (parameters.option_open_interest && contract.product() != Product::Opra)
        {
            return Err(MarketError::InvalidRequest);
        }
        Ok(Self {
            contract,
            parameters,
        })
    }

    pub fn contract(&self) -> &ConfirmedContract {
        &self.contract
    }

    pub fn parameters(&self) -> RequestParameters {
        self.parameters
    }

    pub fn key(&self) -> RequestKey {
        RequestKey {
            con_id: self.contract.con_id,
            product: self.contract.product(),
            parameters: self.parameters,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum MarketError {
    #[error("market contract could not be confirmed")]
    UnconfirmedContract,
    #[error("option contract has expired")]
    ExpiredContract,
    #[error("market data permission denied")]
    NotEntitled,
    #[error("market request timed out")]
    Timeout,
    #[error("market session disconnected")]
    Disconnected,
    #[error("market transport failed")]
    Transport,
    #[error("invalid market request")]
    InvalidRequest,
    #[error("paper account verification failed")]
    UnsafeAccount,
    #[error("invalid paper market configuration")]
    InvalidConfiguration,
}

#[async_trait]
pub trait MarketDataSource: Send + Sync {
    type Subscription: MarketSubscription;

    async fn confirm(&self, query: &ContractQuery) -> Result<ConfirmedContract, MarketError>;
    async fn subscribe(&self, request: &MarketRequest) -> Result<Self::Subscription, MarketError>;
}

#[async_trait]
pub trait MarketSubscription: Send {
    fn request_id(&self) -> i32;
    async fn next(&mut self) -> Result<Option<MarketEvent>, MarketError>;
    async fn cancel(&mut self) -> Result<(), MarketError>;
}

/// Actual type reported by the Gateway for this request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MarketDataType {
    Realtime,
    Frozen,
    Delayed,
    DelayedFrozen,
}

impl MarketDataType {
    pub fn code(self) -> u8 {
        match self {
            Self::Realtime => 1,
            Self::Frozen => 2,
            Self::Delayed => 3,
            Self::DelayedFrozen => 4,
        }
    }

    pub fn is_delayed(self) -> bool {
        matches!(self, Self::Delayed | Self::DelayedFrozen)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuoteField {
    BidPrice,
    AskPrice,
    LastPrice,
    BidSize,
    AskSize,
    LastSize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GreeksSource {
    Bid,
    Ask,
    Last,
    Model,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OptionGreeks {
    pub source: GreeksSource,
    pub delayed: bool,
    pub received_at: DateTime<Utc>,
    pub implied_volatility: Option<DecimalText>,
    pub delta: Option<DecimalText>,
    pub gamma: Option<DecimalText>,
    pub vega: Option<DecimalText>,
    pub theta: Option<DecimalText>,
    pub option_price: Option<DecimalText>,
    pub underlying_price: Option<DecimalText>,
    pub pv_dividend: Option<DecimalText>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum MarketNotice {
    Other,
    DelayedAvailable,
    PartialEntitlement,
    Pacing,
    CompetingSession,
    NotEntitled,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MarketEvent {
    /// `None` represents an unrecognized data-type callback.
    DataType(Option<MarketDataType>),
    Quote {
        field: QuoteField,
        value: Option<DecimalText>,
        delayed: bool,
        received_at: DateTime<Utc>,
    },
    OpenInterest {
        right: OptionRight,
        value: Option<DecimalText>,
        received_at: DateTime<Utc>,
    },
    Greeks(Box<OptionGreeks>),
    Notice(MarketNotice),
}

/// Redacted evidence accumulated over one request. Arrival time uses the older
/// of the two quote prices, so a new update on one side cannot refresh both.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MarketEvidence {
    pub market_data_type: Option<MarketDataType>,
    pub bid_ask_valid: bool,
    pub quote_received_at: Option<DateTime<Utc>>,
    pub open_interest_received: bool,
    pub greeks_received: bool,
    pub notice: Option<MarketNotice>,
    pub saw_type_callback: bool,
}

#[derive(Default)]
struct QuoteSide {
    price: Option<(DecimalText, DateTime<Utc>)>,
    size_invalid: bool,
}

/// Local diagnostic reduction. Consumers needing values should read the events.
#[derive(Default)]
pub struct MarketObservation {
    evidence: MarketEvidence,
    bid: QuoteSide,
    ask: QuoteSide,
}

impl MarketObservation {
    pub fn apply(&mut self, event: MarketEvent) {
        match event {
            MarketEvent::DataType(kind) => {
                self.evidence.saw_type_callback = true;
                if self.evidence.market_data_type != kind {
                    self.bid = QuoteSide::default();
                    self.ask = QuoteSide::default();
                    self.evidence.market_data_type = kind;
                }
            }
            MarketEvent::Quote {
                field,
                value,
                delayed,
                received_at,
            } if self
                .evidence
                .market_data_type
                .is_some_and(|kind| kind.is_delayed() == delayed) =>
            {
                let positive = value.filter(|decimal| decimal.value() > Default::default());
                match field {
                    QuoteField::BidPrice => self.bid.price = positive.map(|p| (p, received_at)),
                    QuoteField::AskPrice => self.ask.price = positive.map(|p| (p, received_at)),
                    QuoteField::BidSize => self.bid.size_invalid = positive.is_none(),
                    QuoteField::AskSize => self.ask.size_invalid = positive.is_none(),
                    QuoteField::LastPrice | QuoteField::LastSize => {}
                }
            }
            MarketEvent::OpenInterest { value, .. } => {
                self.evidence.open_interest_received |= value.is_some();
            }
            MarketEvent::Greeks(greeks) => {
                self.evidence.greeks_received |=
                    [greeks.delta, greeks.gamma, greeks.vega, greeks.theta]
                        .iter()
                        .all(Option::is_some);
            }
            MarketEvent::Notice(notice) => {
                self.evidence.notice = self.evidence.notice.max(Some(notice));
            }
            _ => {}
        }
    }

    pub fn evidence(&self) -> MarketEvidence {
        let quote_received_at = self
            .bid
            .price
            .as_ref()
            .zip(self.ask.price.as_ref())
            .and_then(|((bid, bid_at), (ask, ask_at))| {
                (!self.bid.size_invalid && !self.ask.size_invalid && bid.value() <= ask.value())
                    .then(|| *bid_at.min(ask_at))
            });
        MarketEvidence {
            bid_ask_valid: quote_received_at.is_some(),
            quote_received_at,
            ..self.evidence
        }
    }
}
