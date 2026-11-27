//! Read-only Nasdaq TotalView depth for the eight v1 Nasdaq listings.

mod focus;
pub mod permission;

use std::{str::FromStr, time::Duration};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use finance_market_protocol::DecimalText;
use serde::Serialize;

use crate::market::{ConfirmedContract, ContractKind, ContractQuery, MarketDataType, MarketError};

pub use crate::market::PaperDepthSubscription;
pub use focus::{DepthFocus, run_depth};
pub use permission::DepthPermissions;

pub const VENUE: &str = "NASDAQ";
pub const DEFAULT_DEPTH_ROWS: u32 = 5;
pub const MAX_DEPTH_ROWS: u32 = 50;
pub const PINNED_DEPTH_SLOTS: usize = 2;
pub const MAX_DEPTH_SUBSCRIPTIONS: usize = PINNED_DEPTH_SLOTS + 1;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum DepthSymbol {
    Qqq,
    Aapl,
    Msft,
    Nvda,
    Amzn,
    Googl,
    Meta,
    Tsla,
}

impl DepthSymbol {
    pub const ALL: [Self; 8] = [
        Self::Qqq,
        Self::Aapl,
        Self::Msft,
        Self::Nvda,
        Self::Amzn,
        Self::Googl,
        Self::Meta,
        Self::Tsla,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Qqq => "QQQ",
            Self::Aapl => "AAPL",
            Self::Msft => "MSFT",
            Self::Nvda => "NVDA",
            Self::Amzn => "AMZN",
            Self::Googl => "GOOGL",
            Self::Meta => "META",
            Self::Tsla => "TSLA",
        }
    }

    pub fn contract_query(self) -> ContractQuery {
        ContractQuery::Equity {
            symbol: self.as_str().into(),
            primary_exchange: VENUE.into(),
        }
    }
}

impl FromStr for DepthSymbol {
    type Err = DepthError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|symbol| symbol.as_str() == text)
            .ok_or(DepthError::UnsupportedSymbol)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DepthRequest {
    contract: ConfirmedContract,
    rows: u32,
}

impl DepthRequest {
    pub(crate) fn new(
        symbol: DepthSymbol,
        contract: ConfirmedContract,
        rows: u32,
    ) -> Result<Self, DepthError> {
        if contract.symbol != symbol.as_str()
            || contract.con_id <= 0
            || !matches!(contract.primary_exchange.as_deref(), Some(VENUE | "ISLAND"))
            || contract.kind != ContractKind::Equity
        {
            return Err(DepthError::UnconfirmedContract);
        }
        if !(1..=MAX_DEPTH_ROWS).contains(&rows) {
            return Err(DepthError::InvalidRequest);
        }
        Ok(Self { contract, rows })
    }

    pub fn contract(&self) -> &ConfirmedContract {
        &self.contract
    }

    pub fn rows(&self) -> u32 {
        self.rows
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, thiserror::Error)]
#[serde(rename_all = "snake_case")]
pub enum DepthError {
    #[error("stock is outside the Nasdaq depth watchlist")]
    UnsupportedSymbol,
    #[error("depth focus must have up to two distinct pinned symbols")]
    InvalidFocus,
    #[error("depth request is invalid")]
    InvalidRequest,
    #[error("Nasdaq depth contract could not be confirmed")]
    UnconfirmedContract,
    #[error("Nasdaq depth permission denied")]
    NotEntitled,
    #[error("Nasdaq depth capacity exhausted")]
    Capacity,
    #[error("market data is active in a competing session")]
    CompetingSession,
    #[error("Nasdaq depth pacing limit reached")]
    Pacing,
    #[error("Nasdaq depth request timed out")]
    Timeout,
    #[error("Nasdaq depth session disconnected")]
    Disconnected,
    #[error("Nasdaq depth transport failed")]
    Transport,
    #[error("Nasdaq depth stream ended")]
    StreamEnded,
    #[error("invalid Nasdaq depth callback")]
    InvalidCallback,
    #[error("Nasdaq depth cancel failed and the owner must stop before reopening")]
    CancelFailed,
}

impl From<MarketError> for DepthError {
    fn from(error: MarketError) -> Self {
        match error {
            MarketError::UnconfirmedContract | MarketError::ExpiredContract => {
                Self::UnconfirmedContract
            }
            MarketError::NotEntitled => Self::NotEntitled,
            MarketError::Timeout => Self::Timeout,
            MarketError::Disconnected => Self::Disconnected,
            MarketError::Transport => Self::Transport,
            MarketError::InvalidRequest
            | MarketError::UnsafeAccount
            | MarketError::InvalidConfiguration => Self::InvalidRequest,
        }
    }
}

/// Session adapter for a single depth owner. F9b can wrap these operations with
/// its shared broker-session and budget management.
#[async_trait]
pub trait DepthSource: Send + Sync {
    type Subscription: DepthSubscription;

    async fn confirm_depth(&self, symbol: DepthSymbol) -> Result<ConfirmedContract, DepthError>;

    async fn subscribe_depth(
        &self,
        request: &DepthRequest,
    ) -> Result<Self::Subscription, DepthError>;
}

#[async_trait]
pub trait DepthSubscription: Send {
    fn request_id(&self) -> i32;

    /// Must be cancellation safe when selected alongside focus changes and
    /// callbacks from other subscriptions.
    async fn next(&mut self) -> Result<Option<DepthUpdate>, DepthError>;

    async fn cancel(&mut self) -> Result<(), DepthError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DepthSide {
    Bid,
    Ask,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DepthOperation {
    Insert,
    Update,
    Delete,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DepthUpdate {
    Row {
        side: DepthSide,
        operation: DepthOperation,
        position: u32,
        level: Option<DepthLevel>,
    },
    Reset,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DepthLevel {
    pub price: DecimalText,
    pub size: DecimalText,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DepthState {
    PermissionUnconfirmed,
    Awaiting,
    Receiving,
    Reset,
    Inactive,
    Failed(DepthError),
}

/// Current display state, including the exact ordered rows on each side.
/// Multiple rows can share a price. No exchange timestamp or source sequence
/// is supplied by IBKR for these callbacks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DepthFrame {
    pub symbol: DepthSymbol,
    pub permissions: DepthPermissions,
    pub state: DepthState,
    pub request_id: Option<i32>,
    pub requested_rows: u32,
    pub received_at: Option<DateTime<Utc>>,
    pub bids: Vec<DepthLevel>,
    pub asks: Vec<DepthLevel>,
}

impl DepthFrame {
    pub fn venue(&self) -> &'static str {
        VENUE
    }

    /// Per-side depth reached by the larger of the two books.
    pub fn returned_rows(&self) -> usize {
        self.bids.len().max(self.asks.len())
    }

    /// IBKR supplies no request-specific market data type for depth. A row is
    /// evidence of receipt, independent of the L1 market data mode.
    pub fn market_data_type(&self) -> Option<MarketDataType> {
        None
    }

    pub fn is_stale(&self, now: DateTime<Utc>, max_age: Duration) -> bool {
        self.received_at.is_some_and(|at| {
            !now.signed_duration_since(at)
                .to_std()
                .is_ok_and(|age| age < max_age)
        })
    }
}
