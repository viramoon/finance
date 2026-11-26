use anyhow::{Result, ensure};
use finance_market_protocol::{
    BookLevel, DecimalText, PerpetualContext, Trade, TradeSide, U64Text,
};
use rust_decimal::Decimal;
use serde::Deserialize;

use super::Coin;

#[derive(Deserialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
pub enum TransportEvent {
    Metadata { data: String, receive_ts: U64Text },
    Connected { receive_ts: U64Text },
    Disconnected { receive_ts: U64Text },
    Frame { data: String, receive_ts: U64Text },
}

#[derive(Deserialize)]
pub(super) struct Metadata {
    pub universe: Vec<Asset>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Asset {
    pub name: String,
    pub sz_decimals: u32,
    #[serde(default)]
    pub is_delisted: bool,
}

#[derive(Deserialize)]
#[serde(tag = "channel", content = "data")]
pub(super) enum Frame {
    #[serde(rename = "l2Book")]
    Book(Book),
    #[serde(rename = "trades")]
    Trades(Vec<VenueTrade>),
    #[serde(rename = "activeAssetCtx")]
    Context(Context),
    #[serde(rename = "error")]
    Error,
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
pub(super) struct Book {
    pub coin: Coin,
    pub time: u64,
    pub levels: [Vec<Level>; 2],
}

#[derive(Deserialize)]
pub(super) struct Level {
    px: DecimalText,
    sz: DecimalText,
}

impl TryFrom<Level> for BookLevel {
    type Error = anyhow::Error;

    fn try_from(level: Level) -> Result<Self> {
        ensure!(level.px.value() > Decimal::ZERO, "nonpositive book price");
        Ok(Self {
            price: level.px,
            quantity: level.sz,
        })
    }
}

#[derive(Deserialize)]
pub(super) struct VenueTrade {
    pub coin: Coin,
    pub time: u64,
    pub tid: u64,
    side: Option<String>,
    px: DecimalText,
    sz: DecimalText,
}

impl TryFrom<VenueTrade> for Trade {
    type Error = anyhow::Error;

    fn try_from(trade: VenueTrade) -> Result<Self> {
        ensure!(trade.px.value() > Decimal::ZERO, "nonpositive trade price");
        Ok(Self {
            price: trade.px,
            quantity: trade.sz,
            side: match trade.side.as_deref() {
                Some("B") => TradeSide::Buy,
                Some("A") => TradeSide::Sell,
                _ => TradeSide::Unknown,
            },
        })
    }
}

#[derive(Deserialize)]
pub(super) struct Context {
    pub coin: Coin,
    pub ctx: VenueContext,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct VenueContext {
    pub funding: DecimalText,
    pub open_interest: DecimalText,
    pub mark_px: DecimalText,
}

impl From<VenueContext> for PerpetualContext {
    fn from(context: VenueContext) -> Self {
        Self {
            funding_rate: context.funding,
            open_interest: context.open_interest,
            mark_price: context.mark_px,
        }
    }
}
