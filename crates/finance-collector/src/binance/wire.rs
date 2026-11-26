use std::collections::BTreeMap;

use anyhow::{Context, Result, ensure};
use finance_market_protocol::{
    AssetKind, BookDelta, BookLevel, BookSide, BookSnapshot, BookUpdate, Capability, DecimalText,
    DelayTier, Instrument, MarketKind, SchemaVersion, SettlementKind, SupplierSymbol,
};
use rust_decimal::Decimal;
use serde::Deserialize;

use crate::market_ingress::ProductPolicy;

pub const SOURCE: &str = "binance";
pub const SPOT_PRODUCT: &str = "spot";
pub const PERP_PRODUCT: &str = "usdt-perp";
pub const CONTEXT_PRODUCT: &str = "usdt-perp-context";
pub const SNAPSHOT_LIMIT: u32 = 1000;
pub const SYMBOLS: [&str; 3] = ["BTCUSDT", "ETHUSDT", "SOLUSDT"];

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "snake_case")]
pub enum Market {
    Spot,
    Perpetual,
}

pub fn capability(policy: &ProductPolicy, context: bool) -> Capability {
    Capability {
        schema_version: if context {
            SchemaVersion::V2
        } else {
            SchemaVersion::V1
        },
        source: policy.source.clone(),
        product: policy.product.clone(),
        message_kinds: if context {
            vec![MarketKind::FundingRate, MarketKind::OpenInterest]
        } else {
            vec![MarketKind::Trade, MarketKind::Snapshot, MarketKind::Delta]
        },
        max_depth: if context { 0 } else { SNAPSHOT_LIMIT },
        can_backfill: false,
        delay_tier: DelayTier::Realtime,
    }
}

#[derive(Deserialize)]
pub struct ExchangeInfo {
    pub symbols: Vec<SymbolInfo>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolInfo {
    pub symbol: String,
    base_asset: String,
    quote_asset: String,
    status: String,
    contract_type: Option<String>,
    margin_asset: Option<String>,
    filters: Vec<Filter>,
}

#[derive(Deserialize)]
#[serde(tag = "filterType")]
enum Filter {
    #[serde(rename = "PRICE_FILTER")]
    Price {
        #[serde(rename = "tickSize")]
        tick: DecimalText,
    },
    #[serde(rename = "LOT_SIZE")]
    Lot {
        #[serde(rename = "stepSize")]
        step: DecimalText,
    },
    #[serde(other)]
    Other,
}

impl SymbolInfo {
    pub fn instrument(&self, market: Market, policies: &[&ProductPolicy]) -> Result<Instrument> {
        let expected_base = self
            .symbol
            .strip_suffix("USDT")
            .context("invalid Binance symbol")?;
        ensure!(
            SYMBOLS.contains(&self.symbol.as_str())
                && self.base_asset == expected_base
                && self.quote_asset == "USDT"
                && self.status == "TRADING",
            "invalid Binance instrument metadata"
        );
        let perpetual = market == Market::Perpetual;
        ensure!(
            !perpetual
                || (self.contract_type.as_deref() == Some("PERPETUAL")
                    && self.margin_asset.as_deref() == Some("USDT")),
            "expected a linear USDT perpetual"
        );
        let mut tick = None;
        let mut lot = None;
        for filter in &self.filters {
            match filter {
                Filter::Price { tick: value } => ensure!(
                    tick.replace(value.clone()).is_none(),
                    "duplicate price filter"
                ),
                Filter::Lot { step } => {
                    ensure!(lot.replace(step.clone()).is_none(), "duplicate lot filter")
                }
                Filter::Other => {}
            }
        }
        let product_suffix = if perpetual { "-PERP" } else { "" };
        let instrument = Instrument {
            schema_version: SchemaVersion::V1,
            instrument_id: format!("{expected_base}-USDT{product_suffix}.BINANCE").parse()?,
            asset_kind: if perpetual {
                AssetKind::Perpetual
            } else {
                AssetKind::Spot
            },
            venue: "BINANCE".parse()?,
            quote_currency: "USDT".parse()?,
            tick_size: tick.context("missing price filter")?,
            lot_size: lot.context("missing lot filter")?,
            contract_multiplier: "1".parse()?,
            settlement_kind: if perpetual {
                SettlementKind::Cash
            } else {
                SettlementKind::None
            },
            settlement_currency: if perpetual {
                Some("USDT".parse()?)
            } else {
                None
            },
            calendar_id: "UTC-24X7".parse()?,
            supplier_symbols: policies
                .iter()
                .map(|policy| {
                    Ok(SupplierSymbol {
                        source: policy.source.clone(),
                        product: policy.product.clone(),
                        symbol: self.symbol.parse()?,
                    })
                })
                .collect::<Result<_>>()?,
        };
        instrument.validate()?;
        Ok(instrument)
    }
}

type Levels = Vec<(DecimalText, DecimalText)>;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub last_update_id: u64,
    #[serde(rename = "E")]
    pub event_ms: Option<u64>,
    bids: Levels,
    asks: Levels,
}

impl Snapshot {
    pub fn payload(&self) -> Result<BookSnapshot> {
        fn ordered(levels: &Levels, descending: bool) -> Result<Vec<BookLevel>> {
            ensure!(
                levels.len() <= SNAPSHOT_LIMIT as usize,
                "oversized depth snapshot"
            );
            let mut result = BTreeMap::new();
            for (price, quantity) in levels {
                ensure!(
                    price.value() > Decimal::ZERO && quantity.value() > Decimal::ZERO,
                    "invalid snapshot level"
                );
                let level = BookLevel {
                    price: price.clone(),
                    quantity: quantity.clone(),
                };
                ensure!(
                    result.insert(price.value(), level).is_none(),
                    "duplicate snapshot price"
                );
            }
            let mut result: Vec<_> = result.into_values().collect();
            if descending {
                result.reverse();
            }
            Ok(result)
        }
        Ok(BookSnapshot {
            bids: ordered(&self.bids, true)?,
            asks: ordered(&self.asks, false)?,
        })
    }
}

#[derive(Debug, Deserialize)]
pub struct Diff {
    #[serde(rename = "U")]
    pub first: u64,
    #[serde(rename = "u")]
    pub last: u64,
    #[serde(rename = "pu")]
    pub previous: Option<u64>,
    #[serde(rename = "E")]
    pub event_ms: u64,
    #[serde(rename = "b")]
    bids: Levels,
    #[serde(rename = "a")]
    asks: Levels,
}

impl Diff {
    pub fn sequence_valid(&self, market: Market) -> bool {
        self.first <= self.last
            && (market == Market::Spot
                || self.previous.is_some_and(|previous| previous < self.last))
    }

    pub fn payload(&self) -> Result<BookDelta> {
        let mut updates = Vec::new();
        for (side, levels) in [(BookSide::Bid, &self.bids), (BookSide::Ask, &self.asks)] {
            for (price, quantity) in levels {
                ensure!(
                    price.value() > Decimal::ZERO && quantity.value() >= Decimal::ZERO,
                    "invalid depth update"
                );
                updates.push(BookUpdate {
                    side,
                    price: price.clone(),
                    quantity: quantity.clone(),
                });
            }
        }
        Ok(BookDelta { updates })
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "e")]
pub enum StreamMessage {
    #[serde(rename = "depthUpdate")]
    Depth {
        #[serde(rename = "s")]
        symbol: String,
        #[serde(flatten)]
        diff: Diff,
    },
    #[serde(rename = "trade")]
    SpotTrade {
        #[serde(rename = "s")]
        symbol: String,
        #[serde(rename = "t")]
        id: u64,
        #[serde(rename = "T")]
        trade_ms: u64,
        #[serde(rename = "p")]
        price: DecimalText,
        #[serde(rename = "q")]
        quantity: DecimalText,
        #[serde(rename = "m")]
        buyer_maker: bool,
    },
    #[serde(rename = "aggTrade")]
    PerpetualTrade {
        #[serde(rename = "s")]
        symbol: String,
        #[serde(rename = "a")]
        id: u64,
        #[serde(rename = "T")]
        trade_ms: u64,
        #[serde(rename = "p")]
        price: DecimalText,
        #[serde(rename = "q")]
        quantity: DecimalText,
        #[serde(rename = "m")]
        buyer_maker: bool,
    },
    #[serde(rename = "markPriceUpdate")]
    Funding {
        #[serde(rename = "s")]
        symbol: String,
        #[serde(rename = "E")]
        event_ms: u64,
        #[serde(rename = "r")]
        rate: DecimalText,
        #[serde(rename = "T")]
        next_funding_ms: u64,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Interest {
    pub symbol: String,
    pub open_interest: DecimalText,
    pub time: u64,
}

pub fn ns(millis: u64) -> Result<u64> {
    millis
        .checked_mul(1_000_000)
        .context("Binance timestamp overflow")
}
