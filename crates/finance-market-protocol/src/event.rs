use rust_decimal::Decimal;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::value::RawValue;
use utoipa::ToSchema;

use crate::{
    AssetKind, Capability, DecimalText, DecodeError, ErrorKind, Instrument, InstrumentId,
    MarketKind, NonEmptyText, SchemaVersion, Token, U64Text, ValidationError, Venue,
    decode_tagged_object,
    diagnostics::{Object, decode_at},
    instrument::validate_venue,
    require, require_schema_version, required_nullable, wire_enum, wire_object,
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
        #[serde(deserialize_with = "decode_tagged_object")]
        pub replacement: MarketMessage,
    }
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

wire_object! {
    pub struct PerpetualContext {
        pub funding_rate: DecimalText,
        pub open_interest: DecimalText,
        pub mark_price: DecimalText,
    }
}

wire_object! {
    /// The request and the occurrence within a complete source second.
    pub struct OptionHistoryBatch {
        pub requested_start_ts: U64Text,
        pub page: U64Text,
        pub occurrence: U64Text,
        #[serde(deserialize_with = "required_nullable")]
        #[schema(required = true, inline)]
        pub request_id: Option<U64Text>,
    }
}

wire_object! {
    /// Raw option trade callback evidence, including unmapped or empty condition codes.
    pub struct OptionHistoryTrade {
        pub batch: OptionHistoryBatch,
        pub price: DecimalText,
        pub quantity: DecimalText,
        pub contract_multiplier: DecimalText,
        pub exchange: String,
        pub special_conditions: String,
        pub condition_mapping_version: NonEmptyText,
        pub past_limit: bool,
        pub unreported: bool,
    }
}

wire_object! {
    /// Raw historical top-of-book evidence. Unavailable fields remain null.
    pub struct OptionHistoryQuote {
        pub batch: OptionHistoryBatch,
        #[serde(deserialize_with = "required_nullable")]
        #[schema(required = true, inline)]
        pub bid: Option<DecimalText>,
        #[serde(deserialize_with = "required_nullable")]
        #[schema(required = true, inline)]
        pub ask: Option<DecimalText>,
        #[serde(deserialize_with = "required_nullable")]
        #[schema(required = true, inline)]
        pub bid_quantity: Option<DecimalText>,
        #[serde(deserialize_with = "required_nullable")]
        #[schema(required = true, inline)]
        pub ask_quantity: Option<DecimalText>,
        pub bid_past_low: bool,
        pub ask_past_high: bool,
    }
}

/// The event kind tag and its corresponding payload.
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
    PerpetualContext(PerpetualContext),
    OptionHistoryTrade(OptionHistoryTrade),
    OptionHistoryQuote(OptionHistoryQuote),
}

// Generate the wire fields once for the normal Serde type and for the decoder
// that locates errors. The latter defers the tagged payload regardless of the
// order of JSON keys and reuses its existing typed payload deserializer.
macro_rules! event_record {
    (
        $(#[$attr:meta])*
        pub struct Event {
            $(
                $(#[serde($($serde:tt)*)])?
                $(#[schema($($schema:tt)*)])?
                pub $field:ident: $ty:ty,
            )*
        }
    ) => {
        wire_object! {
            $(#[$attr])*
            pub struct Event {
                $(
                    $(#[serde($($serde)*)])?
                    $(#[schema($($schema)*)])?
                    pub $field: $ty,
                )*
                #[serde(flatten)]
                pub message: Message,
            }
            validate = validate;
            decode = decode_tagged_object;
        }

        impl Event {
            pub(crate) fn decode_record(text: &str) -> Result<Self, DecodeError> {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Wire<'a> {
                    $(
                        $(#[serde($($serde)*)])?
                        $field: $ty,
                    )*
                    kind: String,
                    #[serde(borrow)]
                    payload: &'a RawValue,
                }
                let Object(Wire { $($field,)* kind, payload }) = decode_at(text, "")?;
                let message = Message::decode_payload(&kind, payload.get())?;
                let event = Self { $($field,)* message };
                event.validate()?;
                Ok(event)
            }
        }
    };
}

event_record! {
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
    }
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
        let version = match self.message {
            Message::PerpetualContext(_) => SchemaVersion::V2,
            Message::OptionHistoryTrade(_) | Message::OptionHistoryQuote(_) => SchemaVersion::V3,
            _ => SchemaVersion::V1,
        };
        require_schema_version(
            self.schema_version == version,
            "schema version must match the event kind (2.0 for perpetual_context, 3.0 for option history)",
        )?;
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
            Message::PerpetualContext(context) => {
                require(
                    self.asset_kind == AssetKind::Perpetual,
                    "asset_kind",
                    "context requires a perpetual instrument",
                )?;
                require(
                    context.open_interest.value() >= Decimal::ZERO,
                    "payload.open_interest",
                    "must be nonnegative",
                )?;
                require(
                    context.mark_price.value() > Decimal::ZERO,
                    "payload.mark_price",
                    "must be greater than zero",
                )?;
            }
            Message::OptionHistoryTrade(trade) => {
                self.validate_option_batch(&trade.batch)?;
                require(
                    trade.contract_multiplier.value() > Decimal::ZERO,
                    "payload.contract_multiplier",
                    "must be greater than zero",
                )?;
            }
            Message::OptionHistoryQuote(quote) => self.validate_option_batch(&quote.batch)?,
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

    fn validate_option_batch(&self, batch: &OptionHistoryBatch) -> Result<(), ValidationError> {
        require(
            self.asset_kind == AssetKind::Option,
            "asset_kind",
            "historical evidence requires an option instrument",
        )?;
        let exchange_ts = self.exchange_ts.ok_or_else(|| {
            ValidationError::new("exchange_ts", "required for historical option evidence")
        })?;
        require(
            exchange_ts.get() % 1_000_000_000 == 0 && exchange_ts <= self.receive_ts,
            "exchange_ts",
            "historical evidence requires source seconds at or before receipt",
        )?;
        require(
            batch.requested_start_ts <= exchange_ts,
            "payload.batch.requested_start_ts",
            "must be at or before the source tick",
        )?;
        require(
            batch.page.get() > 0,
            "payload.batch.page",
            "must be greater than zero",
        )?;
        require(
            batch.occurrence.get() > 0,
            "payload.batch.occurrence",
            "must be greater than zero",
        )
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
        if matches!(&self.message, Message::PerpetualContext(_)) {
            require(
                capability
                    .message_kinds
                    .contains(&MarketKind::PerpetualContext),
                "kind",
                "perpetual_context is unsupported by the source/product",
            )?;
        }
        let history_kind = match self.message {
            Message::OptionHistoryTrade(_) => Some(MarketKind::OptionHistoryTrade),
            Message::OptionHistoryQuote(_) => Some(MarketKind::OptionHistoryQuote),
            _ => None,
        };
        if let Message::OptionHistoryTrade(trade) = &self.message {
            require(
                trade.contract_multiplier.value() == instrument.contract_multiplier.value(),
                "payload.contract_multiplier",
                "does not match the supplied instrument",
            )?;
        }
        if let Some(kind) = history_kind {
            require(
                capability.message_kinds.contains(&kind),
                "kind",
                "historical option evidence kind is unsupported by the source/product",
            )?;
        }
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
        if self.stream_key() != previous.stream_key() {
            return Err(ValidationError::at_root(
                "stream",
                ErrorKind::ConstraintViolation,
                "cannot compare positions from different streams",
            ));
        }
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

impl MarketMessage {
    fn decode_payload(kind: &str, payload: &str, scope: &str) -> Result<Self, DecodeError> {
        let path = format!("{scope}/payload");
        match kind {
            "bar_closed" => decode_at(payload, &path).map(Self::BarClosed),
            "trade" => decode_at(payload, &path).map(Self::Trade),
            "snapshot" => decode_at(payload, &path).map(Self::Snapshot),
            "delta" => decode_at(payload, &path).map(Self::Delta),
            _ => Err(DecodeError {
                path: format!("{scope}/kind"),
                kind: ErrorKind::InvalidValue,
                reason: format!("unknown message kind {kind:?}"),
            }),
        }
    }
}

impl From<MarketMessage> for Message {
    fn from(message: MarketMessage) -> Self {
        match message {
            MarketMessage::BarClosed(bar) => Self::BarClosed(bar),
            MarketMessage::Trade(trade) => Self::Trade(trade),
            MarketMessage::Snapshot(snapshot) => Self::Snapshot(snapshot),
            MarketMessage::Delta(delta) => Self::Delta(delta),
        }
    }
}

impl Correction {
    fn decode_payload(payload: &str) -> Result<Self, DecodeError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Tagged<'a> {
            kind: String,
            #[serde(borrow)]
            payload: &'a RawValue,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire<'a> {
            target_epoch: U64Text,
            target_seq: U64Text,
            #[serde(borrow)]
            replacement: Object<Tagged<'a>>,
        }
        let Object(Wire {
            target_epoch,
            target_seq,
            replacement: Object(replacement),
        }) = decode_at(payload, "/payload")?;
        Ok(Self {
            target_epoch,
            target_seq,
            replacement: MarketMessage::decode_payload(
                &replacement.kind,
                replacement.payload.get(),
                "/payload/replacement",
            )?,
        })
    }
}

impl Message {
    fn decode_payload(kind: &str, payload: &str) -> Result<Self, DecodeError> {
        match kind {
            "correction" => Correction::decode_payload(payload).map(Self::Correction),
            "gap" => decode_at(payload, "/payload").map(Self::Gap),
            "reset" => decode_at(payload, "/payload").map(Self::Reset),
            "heartbeat" => decode_at(payload, "/payload").map(Self::Heartbeat),
            "status" => decode_at(payload, "/payload").map(Self::Status),
            "perpetual_context" => decode_at(payload, "/payload").map(Self::PerpetualContext),
            "option_history_trade" => decode_at(payload, "/payload").map(Self::OptionHistoryTrade),
            "option_history_quote" => decode_at(payload, "/payload").map(Self::OptionHistoryQuote),
            _ => MarketMessage::decode_payload(kind, payload, "").map(Self::from),
        }
    }

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
            Self::Gap(_)
            | Self::Reset(_)
            | Self::Heartbeat(_)
            | Self::Status(_)
            | Self::PerpetualContext(_)
            | Self::OptionHistoryTrade(_)
            | Self::OptionHistoryQuote(_) => None,
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
