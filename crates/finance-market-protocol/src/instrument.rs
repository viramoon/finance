use std::collections::HashSet;

use crate::{
    CalendarId, Currency, DecimalText, InstrumentId, NonEmptyText, SchemaVersion, Token,
    ValidationError, Venue, require, require_schema_version, required_nullable, wire_enum,
    wire_object,
};

wire_enum! {
    pub enum AssetKind {
        Stock = "stock",
        Etf = "etf",
        Index = "index",
        Spot = "spot",
        Perpetual = "perpetual",
        Option = "option",
        Future = "future",
    }
}

wire_enum! {
    pub enum SettlementKind {
        Cash = "cash",
        Physical = "physical",
        None = "none",
    }
}

wire_object! {
    pub struct SupplierSymbol {
        pub source: Token,
        pub product: Token,
        pub symbol: NonEmptyText,
    }
}

wire_object! {
    /// The stable identity and fixed-point increments for one instrument.
    pub struct Instrument {
        pub schema_version: SchemaVersion,
        pub instrument_id: InstrumentId,
        pub asset_kind: AssetKind,
        pub venue: Venue,
        pub quote_currency: Currency,
        pub tick_size: DecimalText,
        pub lot_size: DecimalText,
        pub contract_multiplier: DecimalText,
        pub settlement_kind: SettlementKind,
        #[serde(deserialize_with = "required_nullable")]
        #[schema(required = true, inline)]
        pub settlement_currency: Option<Currency>,
        pub calendar_id: CalendarId,
        pub supplier_symbols: Vec<SupplierSymbol>,
    }
    validate = validate;
}

impl Instrument {
    pub fn validate(&self) -> Result<(), ValidationError> {
        require_schema_version(
            self.schema_version == SchemaVersion::V1,
            "instrument uses version 1.0",
        )?;
        validate_venue(&self.instrument_id, &self.venue)?;
        for (field, value) in [
            ("tick_size", &self.tick_size),
            ("lot_size", &self.lot_size),
            ("contract_multiplier", &self.contract_multiplier),
        ] {
            require(
                value.value() > rust_decimal::Decimal::ZERO,
                field,
                "must be greater than zero",
            )?;
        }
        require(
            self.settlement_currency.is_none() == (self.settlement_kind == SettlementKind::None),
            "settlement_currency",
            "must be null exactly when settlement_kind is none",
        )?;
        let mut products = HashSet::new();
        for (index, mapping) in self.supplier_symbols.iter().enumerate() {
            if !products.insert((&mapping.source, &mapping.product)) {
                return Err(ValidationError::new(
                    format!("supplier_symbols.{index}"),
                    "duplicate source/product mapping",
                ));
            }
        }
        Ok(())
    }
}

pub(crate) fn validate_venue(id: &InstrumentId, venue: &Venue) -> Result<(), ValidationError> {
    require(
        id.venue_suffix() == venue.as_str(),
        "venue",
        "must match the instrument_id suffix",
    )
}

wire_enum! {
    /// Market data kinds a source/product can advertise in version 1.0.
    pub enum MarketKind {
        BarClosed = "bar_closed",
        Trade = "trade",
        Snapshot = "snapshot",
        Delta = "delta",
        FundingRate = "funding_rate",
        OpenInterest = "open_interest",
        PerpetualContext = "perpetual_context",
    }
}

wire_enum! {
    pub enum DelayTier {
        Realtime = "realtime",
        Delayed = "delayed",
        Replay = "replay",
    }
}

wire_object! {
    /// Advertised market data capability of a source/product pair.
    pub struct Capability {
        pub schema_version: SchemaVersion,
        pub source: Token,
        pub product: Token,
        #[schema(min_items = 1, max_items = 5)]
        pub message_kinds: Vec<MarketKind>,
        #[schema(maximum = 4294967295u32)]
        pub max_depth: u32,
        pub can_backfill: bool,
        pub delay_tier: DelayTier,
    }
    validate = validate;
}

impl MarketKind {
    fn is_separate_context(self) -> bool {
        matches!(self, Self::FundingRate | Self::OpenInterest)
    }
}

impl Capability {
    pub(crate) fn has_separate_context(&self) -> bool {
        self.message_kinds
            .iter()
            .any(|kind| kind.is_separate_context())
    }

    pub fn validate(&self) -> Result<(), ValidationError> {
        let separate = self.has_separate_context();
        require_schema_version(
            (self.schema_version == SchemaVersion::V2)
                == (separate || self.message_kinds.contains(&MarketKind::PerpetualContext)),
            "capability uses version 2.0 exactly when it supports perpetual context",
        )?;
        require(
            !self.message_kinds.is_empty(),
            "message_kinds",
            "must contain at least one market message kind",
        )?;
        for (index, kind) in self.message_kinds.iter().enumerate() {
            require(
                !separate || kind.is_separate_context(),
                format!("message_kinds.{index}"),
                "separate funding and open interest cannot share a capability with other kinds",
            )?;
            require(
                !self.message_kinds[..index].contains(kind),
                "message_kinds",
                "duplicate market message kind",
            )?;
        }
        let has_depth = self.message_kinds.contains(&MarketKind::Snapshot)
            || self.message_kinds.contains(&MarketKind::Delta);
        require(
            has_depth == (self.max_depth > 0),
            "max_depth",
            "must be positive for snapshot/delta products and zero for other products",
        )
    }
}
