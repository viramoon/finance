//! Read-only queries over F3's explicitly authorized local data.
use crate::{ApiError, AppState, ErrorBody, error};
use axum::{
    Json, Router,
    extract::{Query, State, rejection::QueryRejection},
    http::StatusCode,
    routing::get,
};
use chrono::{DateTime, Utc};
use finance_collector::{
    market_ingress::{
        ProductPolicy, QualityReport, Watermarks, read_quality, read_watermarks, recording_policy,
        wal_root,
    },
    market_wal::{MAX_REPLAY_RECORDS, WalReader, WalRecord},
};
use finance_market_protocol::{Event, InstrumentId, MarketMessage, Message, Token, U64Text};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{path::Path, time::Duration};
use utoipa::{IntoParams, ToSchema};

const DEFAULT_LIST_LIMIT: usize = 100;
const MAX_LIST_LIMIT: usize = 500;
const DEFAULT_WAL_LIMIT: usize = 512;

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/market/products", get(products))
        .route("/api/v1/market/wal", get(wal))
        .route("/api/v1/market/projection/latest", get(projected))
        .route("/api/v1/market/watermarks", get(watermarks))
        .route("/api/v1/quality/gaps", get(gaps))
}

fn bad_query(message: &str) -> ApiError {
    error(StatusCode::BAD_REQUEST, "market_query_invalid", message)
}

fn not_found(code: &str, message: &str) -> ApiError {
    error(StatusCode::NOT_FOUND, code, message)
}

fn storage_error() -> ApiError {
    error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "market_storage_error",
        "Cannot read committed market data",
    )
}

fn parse_query<T>(query: Result<Query<T>, QueryRejection>) -> Result<T, ApiError> {
    query
        .map(|Query(query)| query)
        .map_err(|_| bad_query("Missing or invalid market query parameters"))
}

async fn read_local<T: Send + 'static>(
    state: AppState,
    read: impl FnOnce(&Path) -> Result<T, ApiError> + Send + 'static,
) -> Result<Json<T>, ApiError> {
    tokio::task::spawn_blocking(move || read(&state.data_root))
        .await
        .map_err(|_| storage_error())?
        .map(Json)
}

fn limit(limit: Option<usize>, default: usize, maximum: usize) -> Result<usize, ApiError> {
    let limit = limit.unwrap_or(default);
    if !(1..=maximum).contains(&limit) {
        return Err(bad_query("Page limit is out of range"));
    }
    Ok(limit)
}

fn list_page<T>(
    items: impl Iterator<Item = T>,
    offset: usize,
    limit: usize,
) -> (Vec<T>, Option<usize>) {
    let mut tail = items.skip(offset);
    let items = tail.by_ref().take(limit).collect();
    let next_offset = tail.next().map(|_| offset + limit);
    (items, next_offset)
}

fn require_recording(
    root: &Path,
    source: &Token,
    product: &Token,
) -> Result<ProductPolicy, ApiError> {
    recording_policy(root, source, product)
        .map_err(|_| storage_error())?
        .ok_or_else(|| not_found("market_product_not_found", "No recorded market product"))
}

fn open_wal(root: &Path, access: &ProductPolicy) -> Result<WalReader, ApiError> {
    WalReader::open(
        &wal_root(root),
        access.source.as_str(),
        access.product.as_str(),
    )
    .map_err(|_| storage_error())
}

fn check_license(record: &WalRecord, access: &ProductPolicy) -> Result<(), ApiError> {
    if record.event.license != access.license {
        return Err(storage_error());
    }
    Ok(())
}

#[derive(Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(crate) struct ProductQuery {
    #[param(inline)]
    source: Option<Token>,
    #[param(default = 0)]
    offset: Option<usize>,
    #[param(default = 100, minimum = 1, maximum = 500)]
    limit: Option<usize>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct MarketProducts {
    /// UTC time at which finance observed this query result.
    as_of: DateTime<Utc>,
    items: Vec<ProductPolicy>,
    #[schema(required = true)]
    next_offset: Option<usize>,
}

#[utoipa::path(get, path = "/api/v1/market/products", params(ProductQuery), responses(
    (status = 200, body = MarketProducts), (status = 400, body = ErrorBody), (status = 500, body = ErrorBody)
))]
pub(crate) async fn products(
    State(state): State<AppState>,
    query: Result<Query<ProductQuery>, QueryRejection>,
) -> Result<Json<MarketProducts>, ApiError> {
    let query = parse_query(query)?;
    let limit = limit(query.limit, DEFAULT_LIST_LIMIT, MAX_LIST_LIMIT)?;
    read_local(state, move |root| {
        let mut policies = Vec::new();
        for (source, product) in
            WalReader::products(&wal_root(root)).map_err(|_| storage_error())?
        {
            if query.source.as_ref().is_none_or(|filter| filter == &source)
                && let Some(policy) =
                    recording_policy(root, &source, &product).map_err(|_| storage_error())?
            {
                policies.push(policy);
            }
        }
        let (items, next_offset) = list_page(
            policies.into_iter(),
            query.offset.unwrap_or_default(),
            limit,
        );
        Ok(MarketProducts {
            as_of: Utc::now(),
            items,
            next_offset,
        })
    })
    .await
}

#[derive(Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(crate) struct WalQuery {
    #[param(inline)]
    source: Token,
    #[param(inline)]
    product: Token,
    /// Exclusive WAL cursor. Omit to start from position zero.
    #[param(inline)]
    after: Option<U64Text>,
    /// Inclusive WAL cutoff from a previous page. Omit to use the latest committed prefix.
    #[param(inline)]
    through: Option<U64Text>,
    #[param(default = 512, minimum = 1, maximum = 4096)]
    limit: Option<usize>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct MarketWalPage {
    /// UTC time at which finance observed this query result.
    as_of: DateTime<Utc>,
    access: ProductPolicy,
    durable_through: U64Text,
    through: U64Text,
    records: Vec<WalRecord>,
    /// Exclusive cursor for the next page within `through`. Null at the cutoff.
    #[schema(required = true, inline)]
    next_after: Option<U64Text>,
}

#[utoipa::path(get, path = "/api/v1/market/wal", params(WalQuery), responses(
    (status = 200, body = MarketWalPage), (status = 400, body = ErrorBody),
    (status = 404, body = ErrorBody), (status = 500, body = ErrorBody)
))]
pub(crate) async fn wal(
    State(state): State<AppState>,
    query: Result<Query<WalQuery>, QueryRejection>,
) -> Result<Json<MarketWalPage>, ApiError> {
    let query = parse_query(query)?;
    let limit = limit(query.limit, DEFAULT_WAL_LIMIT, MAX_REPLAY_RECORDS)?;
    read_local(state, move |root| {
        let access = require_recording(root, &query.source, &query.product)?;
        let reader = open_wal(root, &access)?;
        let durable = reader.durable_position();
        let after = query.after.map_or(0, U64Text::get);
        let through = query.through.map_or(durable, U64Text::get);
        if after > through || through > durable {
            return Err(bad_query(
                "WAL cursors must satisfy after <= through <= durable_through",
            ));
        }
        let count = usize::try_from(through - after)
            .unwrap_or(usize::MAX)
            .min(limit);
        let records = if count == 0 {
            Vec::new()
        } else {
            reader
                .read_after(after, count)
                .map_err(|_| storage_error())?
        };
        for record in &records {
            check_license(record, &access)?;
        }
        let next_after = records
            .last()
            .map(|record| record.position)
            .filter(|pos| pos.get() < through);
        Ok(MarketWalPage {
            as_of: Utc::now(),
            access,
            durable_through: durable.into(),
            through: through.into(),
            records,
            next_after,
        })
    })
    .await
}

#[derive(Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(crate) struct ProjectionQuery {
    #[param(inline)]
    source: Token,
    #[param(inline)]
    product: Token,
    #[param(inline)]
    channel: Token,
    #[param(inline)]
    instrument_id: InstrumentId,
    #[param(inline)]
    epoch: U64Text,
    #[param(inline)]
    seq: U64Text,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ProjectedMarketEvent {
    /// UTC time at which finance observed this query result.
    as_of: DateTime<Utc>,
    access: ProductPolicy,
    projected_through: U64Text,
    /// The committed original event or latest correction envelope for the supplied F1 key.
    record: WalRecord,
}

struct ProjectionMatch {
    through: U64Text,
    position: u64,
    revision: U64Text,
    kind: String,
}

fn projection_miss() -> ApiError {
    not_found(
        "market_projection_not_found",
        "No projected event at this F1 position",
    )
}

fn projection_match(root: &Path, query: &ProjectionQuery) -> Result<ProjectionMatch, ApiError> {
    let index = root
        .join("finance-state/market-projection")
        .join(query.source.as_str())
        .join(query.product.as_str())
        .join("projection.sqlite3");
    if !index.try_exists().map_err(|_| storage_error())? {
        return Err(projection_miss());
    }
    let conn = Connection::open_with_flags(index, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|_| storage_error())?;
    conn.busy_timeout(Duration::from_millis(250))
        .map_err(|_| storage_error())?;
    let tx = conn.unchecked_transaction().map_err(|_| storage_error())?;
    let version: u32 = tx
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|_| storage_error())?;
    if version != 1 {
        return Err(storage_error());
    }
    // Both queries see the same committed SQLite snapshot, even as the
    // projector advances in a different process.
    let through: String = tx
        .query_row(
            "SELECT projected_through FROM checkpoint WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .map_err(|_| storage_error())?;
    let through: U64Text = through.parse().map_err(|_| storage_error())?;
    let (position_text, revision, kind): (String, String, String) = tx
        .query_row(
            "SELECT position, revision, kind FROM latest
             WHERE channel = ?1 AND instrument_id = ?2 AND epoch = ?3 AND seq = ?4",
            params![
                query.channel.as_str(),
                query.instrument_id.as_str(),
                query.epoch.get().to_string(),
                query.seq.get().to_string()
            ],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(|_| storage_error())?
        .ok_or_else(projection_miss)?;
    let position = position_text.parse::<u64>().map_err(|_| storage_error())?;
    if position == 0 || position > through.get() || position_text != format!("{position:020}") {
        return Err(storage_error());
    }
    let revision = revision.parse().map_err(|_| storage_error())?;
    Ok(ProjectionMatch {
        through,
        position,
        revision,
        kind,
    })
}

fn event_matches(query: &ProjectionQuery, matched: &ProjectionMatch, event: &Event) -> bool {
    if event.channel != query.channel
        || event.instrument_id != query.instrument_id
        || event.revision != matched.revision
    {
        return false;
    }
    let (epoch, seq, kind) = match &event.message {
        Message::BarClosed(_) => (event.epoch, event.seq, "bar_closed"),
        Message::Trade(_) => (event.epoch, event.seq, "trade"),
        Message::Snapshot(_) => (event.epoch, event.seq, "snapshot"),
        Message::Delta(_) => (event.epoch, event.seq, "delta"),
        Message::PerpetualContext(_) => (event.epoch, event.seq, "perpetual_context"),
        Message::FundingRate(_) => (event.epoch, event.seq, "funding_rate"),
        Message::OpenInterest(_) => (event.epoch, event.seq, "open_interest"),
        Message::OptionHistoryTrade(_) => (event.epoch, event.seq, "option_history_trade"),
        Message::OptionHistoryQuote(_) => (event.epoch, event.seq, "option_history_quote"),
        Message::Correction(correction) => (
            correction.target_epoch,
            correction.target_seq,
            match &correction.replacement {
                MarketMessage::BarClosed(_) => "bar_closed",
                MarketMessage::Trade(_) => "trade",
                MarketMessage::Snapshot(_) => "snapshot",
                MarketMessage::Delta(_) => "delta",
            },
        ),
        _ => return false,
    };
    epoch == query.epoch && seq == query.seq && kind == matched.kind
}

#[utoipa::path(get, path = "/api/v1/market/projection/latest", params(ProjectionQuery), responses(
    (status = 200, body = ProjectedMarketEvent), (status = 400, body = ErrorBody),
    (status = 404, body = ErrorBody), (status = 500, body = ErrorBody)
))]
pub(crate) async fn projected(
    State(state): State<AppState>,
    query: Result<Query<ProjectionQuery>, QueryRejection>,
) -> Result<Json<ProjectedMarketEvent>, ApiError> {
    let query = parse_query(query)?;
    if query.epoch.get() == 0 {
        return Err(bad_query("Projection epoch must be greater than zero"));
    }
    read_local(state, move |root| {
        let access = require_recording(root, &query.source, &query.product)?;
        let matched = projection_match(root, &query)?;
        let reader = open_wal(root, &access)?;
        if matched.through.get() > reader.durable_position() {
            return Err(storage_error());
        }
        let record = reader
            .read_after(matched.position - 1, 1)
            .map_err(|_| storage_error())?
            .pop()
            .filter(|record| record.position.get() == matched.position)
            .ok_or_else(storage_error)?;
        check_license(&record, &access)?;
        if !event_matches(&query, &matched, &record.event) {
            return Err(storage_error());
        }
        Ok(ProjectedMarketEvent {
            as_of: Utc::now(),
            access,
            projected_through: matched.through,
            record,
        })
    })
    .await
}

#[derive(Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(crate) struct WatermarkQuery {
    #[param(inline)]
    source: Token,
    #[param(inline)]
    product: Token,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct MarketWatermarks {
    /// UTC time at which finance observed this query result.
    as_of: DateTime<Utc>,
    access: ProductPolicy,
    watermarks: Watermarks,
}

#[utoipa::path(get, path = "/api/v1/market/watermarks", params(WatermarkQuery), responses(
    (status = 200, body = MarketWatermarks), (status = 400, body = ErrorBody),
    (status = 404, body = ErrorBody), (status = 500, body = ErrorBody)
))]
pub(crate) async fn watermarks(
    State(state): State<AppState>,
    query: Result<Query<WatermarkQuery>, QueryRejection>,
) -> Result<Json<MarketWatermarks>, ApiError> {
    let query = parse_query(query)?;
    read_local(state, move |root| {
        let access = require_recording(root, &query.source, &query.product)?;
        let watermarks =
            read_watermarks(root, &query.source, &query.product).map_err(|_| storage_error())?;
        Ok(MarketWatermarks {
            as_of: Utc::now(),
            access,
            watermarks,
        })
    })
    .await
}

#[derive(Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(crate) struct GapQuery {
    #[param(inline)]
    source: Token,
    #[param(inline)]
    product: Token,
    #[param(inline)]
    channel: Option<Token>,
    #[param(inline)]
    instrument_id: Option<InstrumentId>,
    /// Inclusive UTC Unix nanosecond bound of the search window.
    #[param(inline)]
    from_ts: Option<U64Text>,
    /// Exclusive UTC Unix nanosecond bound of the search window.
    #[param(inline)]
    to_ts: Option<U64Text>,
    #[param(default = 0)]
    offset: Option<usize>,
    #[param(default = 100, minimum = 1, maximum = 500)]
    limit: Option<usize>,
}

impl GapQuery {
    fn includes(&self, item: &QualityReport) -> bool {
        self.channel
            .as_ref()
            .is_none_or(|channel| *channel == item.channel)
            && self
                .instrument_id
                .as_ref()
                .is_none_or(|id| *id == item.instrument_id)
            && self.from_ts.is_none_or(|from| from <= item.to_ts)
            && self.to_ts.is_none_or(|to| item.from_ts < to)
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct MarketQualityPage {
    /// UTC time at which finance observed this query result.
    as_of: DateTime<Utc>,
    access: ProductPolicy,
    items: Vec<QualityReport>,
    #[schema(required = true)]
    next_offset: Option<usize>,
}

#[utoipa::path(get, path = "/api/v1/quality/gaps", params(GapQuery), responses(
    (status = 200, body = MarketQualityPage), (status = 400, body = ErrorBody),
    (status = 404, body = ErrorBody), (status = 500, body = ErrorBody)
))]
pub(crate) async fn gaps(
    State(state): State<AppState>,
    query: Result<Query<GapQuery>, QueryRejection>,
) -> Result<Json<MarketQualityPage>, ApiError> {
    let query = parse_query(query)?;
    let limit = limit(query.limit, DEFAULT_LIST_LIMIT, MAX_LIST_LIMIT)?;
    if query
        .from_ts
        .zip(query.to_ts)
        .is_some_and(|(from, to)| from >= to)
    {
        return Err(bad_query("Quality from_ts must precede to_ts"));
    }
    read_local(state, move |root| {
        let access = require_recording(root, &query.source, &query.product)?;
        let reports =
            read_quality(root, &query.source, &query.product).map_err(|_| storage_error())?;
        let (items, next_offset) = list_page(
            reports.into_iter().filter(|item| query.includes(item)),
            query.offset.unwrap_or_default(),
            limit,
        );
        Ok(MarketQualityPage {
            as_of: Utc::now(),
            access,
            items,
            next_offset,
        })
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    #[test]
    fn projection_matches_separate_binance_context_observations() {
        let examples: Value =
            serde_json::from_str(include_str!("../../../protocol/v2/examples.json")).unwrap();
        for (kind, payload) in [
            (
                "funding_rate",
                json!({"rate": "-0.00001", "next_funding_ts": null}),
            ),
            ("open_interest", json!({"quantity": "12.3456789"})),
        ] {
            let mut raw = examples["events"][0].clone();
            raw["source"] = json!("binance");
            raw["product"] = json!("usdt-perp-context");
            raw["instrument_id"] = json!("BTC-USDT-PERP.BINANCE");
            raw["venue"] = json!("BINANCE");
            raw["channel"] = json!(kind);
            raw["kind"] = json!(kind);
            raw["exchange_ts"] = raw["receive_ts"].clone();
            raw["payload"] = payload;
            let event: Event = serde_json::from_value(raw).unwrap();
            let query = ProjectionQuery {
                source: event.source.clone(),
                product: event.product.clone(),
                channel: event.channel.clone(),
                instrument_id: event.instrument_id.clone(),
                epoch: event.epoch,
                seq: event.seq,
            };
            let mut matched = ProjectionMatch {
                through: 1.into(),
                position: 1,
                revision: 0.into(),
                kind: kind.to_owned(),
            };
            assert!(event_matches(&query, &matched, &event));
            matched.kind = "perpetual_context".into();
            assert!(!event_matches(&query, &matched, &event));
        }
    }
}
