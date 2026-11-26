use rust_decimal::Decimal;
use serde::{Deserialize, Deserializer, Serialize};
use utoipa::ToSchema;

use crate::{
    AssetKind, Capability, DecimalText, Instrument, InstrumentId, MarketKind, NonEmptyText,
    ObjectOnly, SchemaVersion, Token, U64Text, ValidationError, Venue, instrument::validate_venue,
    require, required_nullable, wire_enum, wire_object,
};

wire_enum! {
    pub enum TradeSide {
        Buy = "buy",
        Sell = "sell",
        Unknown = "unknown",
    }
}

wire_enum! {
    pub enum BookSide {
        Bid = "bid",
        Ask = "ask",
    }
}

wire_enum! {
    pub enum ConnectionState {
        Connected = "connected",
        Degraded = "degraded",
        Disconnected = "disconnected",
    }
}

wire_object! {
    pub struct ClosedBar {
        pub open_ts: U64Text,
        pub open: DecimalText,
        pub high: DecimalText,
        pub low: DecimalText,
        pub close: DecimalText,
        pub volume: DecimalText,
    }
}

wire_object! {
    pub struct Trade {
        pub price: DecimalText,
        pub quantity: DecimalText,
        pub side: TradeSide,
    }
}

wire_object! {
    pub struct BookLevel {
        pub price: DecimalText,
        pub quantity: DecimalText,
    }
}

wire_object! {
    pub struct BookSnapshot {
        pub bids: Vec<BookLevel>,
        pub asks: Vec<BookLevel>,
    }
}

wire_object! {
    pub struct BookUpdate {
        pub side: BookSide,
        pub price: DecimalText,
        pub quantity: DecimalText,
    }
}

wire_object! {
    pub struct BookDelta {
        #[schema(min_items = 1)]
        pub updates: Vec<BookUpdate>,
    }
}

/// The complete replacement value carried by a correction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(
    tag = "kind",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum MarketMessage {
    BarClosed(ClosedBar),
    Trade(Trade),
    Snapshot(BookSnapshot),
    Delta(BookDelta),
}

wire_object! {
    pub struct Correction {
        pub target_epoch: U64Text,
        pub target_seq: U64Text,
        #[serde(deserialize_with = "decode_replacement")]
        pub replacement: MarketMessage,
    }
}

fn decode_replacement<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<MarketMessage, D::Error> {
    let ObjectOnly(message) = ObjectOnly::deserialize(deserializer)?;
    Ok(message)
}

wire_object! {
    pub struct Gap {
        pub from_ts: U64Text,
        pub to_ts: U64Text,
        pub reason: NonEmptyText,
    }
}

wire_object! {
    pub struct Reset {
        pub reason: NonEmptyText,
    }
}

wire_object! {
    pub struct Heartbeat {}
}

wire_object! {
    pub struct Status {
        pub state: ConnectionState,
    }
}

/// The version 1.0 kind tag and its corresponding payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(
    tag = "kind",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Message {
    BarClosed(ClosedBar),
    Trade(Trade),
    Snapshot(BookSnapshot),
    Delta(BookDelta),
    Correction(Correction),
    Gap(Gap),
    Reset(Reset),
    Heartbeat(Heartbeat),
    Status(Status),
}

wire_object! {
    /// One event and its position in an instrument stream.
    pub struct Event {
        pub schema_version: SchemaVersion,
        pub channel: Token,
        pub source: Token,
        pub product: Token,
        pub instrument_id: InstrumentId,
        pub asset_kind: AssetKind,
        pub venue: Venue,
        #[serde(deserialize_with = "required_nullable")]
        #[schema(required = true, inline)]
        pub exchange_ts: Option<U64Text>,
        pub receive_ts: U64Text,
        pub first_available_ts: U64Text,
        pub epoch: U64Text,
        pub seq: U64Text,
        pub cursor: NonEmptyText,
        pub revision: U64Text,
        pub license: Token,
        #[serde(
            default,
            deserialize_with = "present",
            skip_serializing_if = "Option::is_none"
        )]
        #[schema(value_type = NonEmptyText, required = false)]
        pub native_id: Option<NonEmptyText>,
        #[serde(flatten)]
        pub message: Message,
    }
    validate = validate;
}

fn present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

/// The scope of epoch, sequence and resume positions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StreamKey<'a> {
    pub source: &'a Token,
    pub product: &'a Token,
    pub channel: &'a Token,
    pub instrument_id: &'a InstrumentId,
}

impl Event {
    pub fn stream_key(&self) -> StreamKey<'_> {
        StreamKey {
            source: &self.source,
            product: &self.product,
            channel: &self.channel,
            instrument_id: &self.instrument_id,
        }
    }

    pub fn validate(&self) -> Result<(), ValidationError> {
        validate_venue(&self.instrument_id, &self.venue)?;
        require(self.epoch.get() > 0, "epoch", "must be greater than zero")?;
        match &self.message {
            Message::Correction(correction) => {
                require(
                    self.revision.get() > 0,
                    "revision",
                    "a correction requires a positive revision",
                )?;
                require(
                    correction.target_epoch.get() > 0,
                    "payload.target_epoch",
                    "must be greater than zero",
                )?;
                require(
                    (correction.target_epoch, correction.target_seq) < (self.epoch, self.seq),
                    "payload.target_seq",
                    "correction target must precede the correction position",
                )?;
            }
            _ => require(
                self.revision.get() == 0,
                "revision",
                "only a correction can carry a positive revision",
            )?,
        }
        match &self.message {
            Message::Reset(_) => require(self.seq.get() == 0, "seq", "a reset starts at zero")?,
            Message::Gap(gap) => require(
                gap.from_ts <= gap.to_ts,
                "payload.to_ts",
                "must be at or after payload.from_ts",
            )?,
            _ => {}
        }

        if let Some(market) = self.message.market_payload() {
            let exchange_ts = self.exchange_ts.ok_or_else(|| {
                ValidationError::new("exchange_ts", "required for a market message or correction")
            })?;
            market.validate(exchange_ts, self.message.market_path())?;
        }
        Ok(())
    }

    /// Validate an event against the instrument and source/product definitions.
    pub fn validate_against(
        &self,
        instrument: &Instrument,
        capability: &Capability,
    ) -> Result<(), ValidationError> {
        self.validate()?;
        instrument.validate()?;
        capability.validate()?;
        require(
            self.instrument_id == instrument.instrument_id,
            "instrument_id",
            "does not match the supplied instrument",
        )?;
        require(
            self.asset_kind == instrument.asset_kind,
            "asset_kind",
            "does not match the supplied instrument",
        )?;
        require(
            self.source == capability.source,
            "source",
            "does not match the supplied capability",
        )?;
        require(
            self.product == capability.product,
            "product",
            "does not match the supplied capability",
        )?;
        if let Some(market) = self.message.market_payload() {
            let path = self.message.market_path();
            require(
                capability.message_kinds.contains(&market.kind()),
                if matches!(self.message, Message::Correction(_)) {
                    "payload.replacement.kind"
                } else {
                    "kind"
                },
                "market message kind is unsupported by the source/product",
            )?;
            market.validate_increments(instrument, capability, path)?;
        }
        Ok(())
    }

    /// Compare two validated events in the same stream to detect gaps and resets.
    pub fn validate_successor(&self, previous: &Self) -> Result<(), ValidationError> {
        require(
            self.stream_key() == previous.stream_key(),
            "stream",
            "cannot compare positions from different streams",
        )?;
        let is_reset = matches!(self.message, Message::Reset(_));
        if self.epoch != previous.epoch || is_reset {
            require(
                self.epoch > previous.epoch,
                "epoch",
                "a reset must advance the epoch and an epoch cannot regress",
            )?;
            require(is_reset, "epoch", "an epoch change requires a reset")?;
            return require(self.seq.get() == 0, "seq", "a reset starts at zero");
        }

        require(
            self.seq > previous.seq,
            "seq",
            "sequence regressed or repeated within the epoch",
        )?;
        require(
            self.seq.get() - previous.seq.get() == 1,
            "seq",
            "sequence gap within the epoch",
        )
    }
}

impl Message {
    fn market_payload(&self) -> Option<MarketPayload<'_>> {
        match self {
            Self::BarClosed(bar) => Some(MarketPayload::BarClosed(bar)),
            Self::Trade(trade) => Some(MarketPayload::Trade(trade)),
            Self::Snapshot(snapshot) => Some(MarketPayload::Snapshot(snapshot)),
            Self::Delta(delta) => Some(MarketPayload::Delta(delta)),
            Self::Correction(correction) => Some(match &correction.replacement {
                MarketMessage::BarClosed(bar) => MarketPayload::BarClosed(bar),
                MarketMessage::Trade(trade) => MarketPayload::Trade(trade),
                MarketMessage::Snapshot(snapshot) => MarketPayload::Snapshot(snapshot),
                MarketMessage::Delta(delta) => MarketPayload::Delta(delta),
            }),
            Self::Gap(_) | Self::Reset(_) | Self::Heartbeat(_) | Self::Status(_) => None,
        }
    }

    fn market_path(&self) -> &'static str {
        if matches!(self, Self::Correction(_)) {
            "payload.replacement.payload"
        } else {
            "payload"
        }
    }
}

enum MarketPayload<'a> {
    BarClosed(&'a ClosedBar),
    Trade(&'a Trade),
    Snapshot(&'a BookSnapshot),
    Delta(&'a BookDelta),
}

impl MarketPayload<'_> {
    fn kind(&self) -> MarketKind {
        match self {
            Self::BarClosed(_) => MarketKind::BarClosed,
            Self::Trade(_) => MarketKind::Trade,
            Self::Snapshot(_) => MarketKind::Snapshot,
            Self::Delta(_) => MarketKind::Delta,
        }
    }

    fn validate(&self, exchange_ts: U64Text, path: &str) -> Result<(), ValidationError> {
        match self {
            Self::BarClosed(bar) => {
                require(
                    bar.open_ts < exchange_ts,
                    format!("{path}.open_ts"),
                    "must be before the bar close exchange_ts",
                )?;
                let (open, high, low, close) = (
                    bar.open.value(),
                    bar.high.value(),
                    bar.low.value(),
                    bar.close.value(),
                );
                require(
                    low <= open.min(close) && high >= open.max(close),
                    path,
                    "bar low/high must enclose open and close",
                )?;
                require(
                    bar.volume.value() >= Decimal::ZERO,
                    format!("{path}.volume"),
                    "must be nonnegative",
                )
            }
            Self::Trade(trade) => require(
                trade.quantity.value() > Decimal::ZERO,
                format!("{path}.quantity"),
                "must be greater than zero",
            ),
            Self::Snapshot(snapshot) => {
                validate_book_side(&snapshot.bids, BookSide::Bid, &format!("{path}.bids"))?;
                validate_book_side(&snapshot.asks, BookSide::Ask, &format!("{path}.asks"))
            }
            Self::Delta(delta) => {
                require(
                    !delta.updates.is_empty(),
                    format!("{path}.updates"),
                    "must contain at least one book update",
                )?;
                for (index, update) in delta.updates.iter().enumerate() {
                    if update.quantity.value() < Decimal::ZERO {
                        return Err(ValidationError::new(
                            format!("{path}.updates.{index}.quantity"),
                            "must be nonnegative",
                        ));
                    }
                }
                Ok(())
            }
        }
    }

    fn validate_increments(
        &self,
        instrument: &Instrument,
        capability: &Capability,
        path: &str,
    ) -> Result<(), ValidationError> {
        match self {
            Self::BarClosed(bar) => {
                for (field, price) in [
                    ("open", &bar.open),
                    ("high", &bar.high),
                    ("low", &bar.low),
                    ("close", &bar.close),
                ] {
                    require_increment(price, &instrument.tick_size, TICK_MISMATCH, || {
                        format!("{path}.{field}")
                    })?;
                }
                require_increment(&bar.volume, &instrument.lot_size, LOT_MISMATCH, || {
                    format!("{path}.volume")
                })
            }
            Self::Trade(trade) => {
                validate_level_increments(&trade.price, &trade.quantity, instrument, || {
                    path.to_owned()
                })
            }
            Self::Snapshot(snapshot) => {
                for (side, levels) in [("bids", &snapshot.bids), ("asks", &snapshot.asks)] {
                    require(
                        levels.len() <= capability.max_depth as usize,
                        format!("{path}.{side}"),
                        "snapshot depth exceeds source/product max_depth",
                    )?;
                    for (index, level) in levels.iter().enumerate() {
                        validate_level_increments(
                            &level.price,
                            &level.quantity,
                            instrument,
                            || format!("{path}.{side}.{index}"),
                        )?;
                    }
                }
                Ok(())
            }
            Self::Delta(delta) => {
                for (index, update) in delta.updates.iter().enumerate() {
                    validate_level_increments(&update.price, &update.quantity, instrument, || {
                        format!("{path}.updates.{index}")
                    })?;
                }
                Ok(())
            }
        }
    }
}

fn validate_book_side(
    levels: &[BookLevel],
    side: BookSide,
    path: &str,
) -> Result<(), ValidationError> {
    let mut previous = None;
    for (index, level) in levels.iter().enumerate() {
        if level.quantity.value() <= Decimal::ZERO {
            return Err(ValidationError::new(
                format!("{path}.{index}.quantity"),
                "must be greater than zero",
            ));
        }
        let price = level.price.value();
        if let Some(last_price) = previous {
            let correctly_ordered = match side {
                BookSide::Bid => price < last_price,
                BookSide::Ask => price > last_price,
            };
            if !correctly_ordered {
                return Err(ValidationError::new(
                    format!("{path}.{index}.price"),
                    "bids must descend and asks must ascend without duplicate prices",
                ));
            }
        }
        previous = Some(price);
    }
    Ok(())
}

const TICK_MISMATCH: &str = "is not a multiple of instrument tick_size";
const LOT_MISMATCH: &str = "is not a multiple of instrument lot_size";

fn require_increment(
    value: &DecimalText,
    step: &DecimalText,
    reason: &'static str,
    field: impl FnOnce() -> String,
) -> Result<(), ValidationError> {
    if value.is_multiple_of(step) {
        Ok(())
    } else {
        Err(ValidationError::new(field(), reason))
    }
}

fn validate_level_increments(
    price: &DecimalText,
    quantity: &DecimalText,
    instrument: &Instrument,
    path: impl Fn() -> String,
) -> Result<(), ValidationError> {
    require_increment(price, &instrument.tick_size, TICK_MISMATCH, || {
        format!("{}.price", path())
    })?;
    require_increment(quantity, &instrument.lot_size, LOT_MISMATCH, || {
        format!("{}.quantity", path())
    })
}
