//! Private read access to signals committed by the offline/live decision process.
use crate::{ApiError, AppState, ErrorBody, error, read_store, strategy_bundle::valid_recipe_id};
use axum::{
    Json, Router,
    extract::{Query, State, rejection::QueryRejection},
    http::StatusCode,
    routing::get,
};
use chrono::NaiveDate;
use finance_market_protocol::{DecimalText, InstrumentId, NonEmptyText, Token, U64Text};
use rusqlite::{Connection, OptionalExtension, Row, params, types::Type};
use serde::{Deserialize, Serialize};
use std::{error::Error, io::Read, path::Path, str::FromStr};
use utoipa::{IntoParams, ToSchema};

const PUBLICATION_PATH: &str = "finance-state/decisions/read-v1.json";
const DATABASE_PATH: &str = "finance-state/decisions/decisions.sqlite3";
const MAX_PUBLICATION_BYTES: u64 = 4096;

#[derive(Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(crate) struct SignalQuery {
    /// Exclusive cursor from the same published plan.
    after: Option<String>,
    #[param(minimum = 1, maximum = 100)]
    limit: Option<u16>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum SignalKind {
    SmaCrossover,
    StrategyBundle,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Publication {
    schema_version: u8,
    kind: SignalKind,
    plan_sha256: String,
    access: DecisionAccess,
}

#[derive(Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct DecisionAccess {
    license: Token,
    record_local: bool,
    derive_non_display: bool,
    redistribute: bool,
}

#[derive(Serialize, ToSchema)]
struct SmaSignal {
    replay_offset: U64Text,
    source: Token,
    product: Token,
    channel: Token,
    instrument_id: InstrumentId,
    epoch: U64Text,
    seq: U64Text,
    cursor: NonEmptyText,
    license: Token,
    exchange_ts: U64Text,
    first_available_ts: U64Text,
    decision_ts: U64Text,
    target_shares: DecimalText,
}

#[derive(Serialize, ToSchema)]
struct BundleSignal {
    replay_offset: U64Text,
    session: NaiveDate,
    signal_ts: U64Text,
    recipe: String,
    instrument_id: InstrumentId,
    target_shares: DecimalText,
    reason: Option<String>,
}

#[derive(Serialize, ToSchema)]
#[serde(tag = "kind", content = "signal", rename_all = "snake_case")]
enum DecisionSignal {
    SmaCrossover(SmaSignal),
    StrategyBundle(BundleSignal),
}

impl DecisionSignal {
    fn cursor(&self, plan: &str) -> String {
        match self {
            Self::SmaCrossover(row) => format!("{plan}@{}", row.replay_offset.get()),
            Self::StrategyBundle(row) => format!(
                "{plan}@{}@{}@{}",
                row.replay_offset.get(),
                row.recipe,
                row.instrument_id
            ),
        }
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct DecisionSignalPage {
    #[schema(minimum = 1, maximum = 1)]
    schema_version: u8,
    #[schema(pattern = "^[0-9a-f]{64}$")]
    plan_sha256: String,
    access: DecisionAccess,
    items: Vec<DecisionSignal>,
    next_cursor: Option<String>,
}

fn invalid_query() -> ApiError {
    error(
        StatusCode::BAD_REQUEST,
        "invalid_decision_query",
        "Invalid signal pagination",
    )
}

fn not_published() -> ApiError {
    error(
        StatusCode::NOT_FOUND,
        "decision_not_published",
        "No approved decision signals are published",
    )
}

fn storage_error() -> ApiError {
    error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "decision_storage_error",
        "Cannot read committed decision signals",
    )
}

fn publication(root: &Path) -> Result<Publication, ApiError> {
    let file = read_store::open_file(root, Path::new(PUBLICATION_PATH))
        .map_err(|_| storage_error())?
        .ok_or_else(not_published)?;
    let mut data = Vec::new();
    file.take(MAX_PUBLICATION_BYTES + 1)
        .read_to_end(&mut data)
        .map_err(|_| storage_error())?;
    if data.len() as u64 > MAX_PUBLICATION_BYTES {
        return Err(storage_error());
    }
    let publication: Publication = serde_json::from_slice(&data).map_err(|_| storage_error())?;
    let hash = publication.plan_sha256.as_bytes();
    if publication.schema_version != 1
        || hash.len() != 64
        || !hash
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
    {
        return Err(storage_error());
    }
    let access = &publication.access;
    if !access.record_local || !access.derive_non_display || access.redistribute {
        return Err(not_published());
    }
    Ok(publication)
}

fn offset(text: &str) -> Result<i64, ApiError> {
    text.parse::<U64Text>()
        .ok()
        .and_then(|value| i64::try_from(value.get()).ok())
        .ok_or_else(invalid_query)
}

fn stored_offset(row: &Row<'_>) -> rusqlite::Result<U64Text> {
    let value: i64 = row.get(0)?;
    u64::try_from(value)
        .map(U64Text::from)
        .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, value))
}

fn stored_text<T: FromStr>(row: &Row<'_>, idx: usize) -> rusqlite::Result<T>
where
    T::Err: Error + Send + Sync + 'static,
{
    row.get::<_, String>(idx)?
        .parse()
        .map_err(|err| rusqlite::Error::FromSqlConversionFailure(idx, Type::Text, Box::new(err)))
}

fn verified_sma_offset(conn: &Connection, published: &Publication) -> Result<i64, ApiError> {
    if !read_store::has_table(conn, "sma_replay_license").map_err(|_| storage_error())? {
        return Err(not_published());
    }
    conn.query_row(
        "SELECT checkpoint.replay_offset
         FROM decision_checkpoint AS checkpoint
         JOIN sma_replay_license AS verified USING (id, replay_offset, replay_sha256)
         WHERE checkpoint.id=1 AND checkpoint.plan_sha256=?1 AND verified.license=?2",
        params![published.plan_sha256, published.access.license.as_str()],
        |row| row.get(0),
    )
    .optional()
    .map_err(|_| storage_error())?
    .ok_or_else(not_published)
}

fn sma_signals(
    conn: &Connection,
    published: &Publication,
    after: Option<&str>,
    limit: u16,
) -> Result<Vec<DecisionSignal>, ApiError> {
    let after = after.map(offset).transpose()?.unwrap_or(-1);
    let through = verified_sma_offset(conn, published)?;
    read_signals(
        conn,
        "decisions",
        "SELECT replay_offset,source,product,channel,instrument_id,epoch,seq,cursor,license,
                exchange_ts,first_available_ts,decision_ts,target_shares
         FROM decisions WHERE plan_sha256=?1 AND replay_offset>?2 AND license=?3 AND replay_offset<=?4
         ORDER BY replay_offset LIMIT ?5",
        params![
            published.plan_sha256,
            after,
            published.access.license.as_str(),
            through,
            limit
        ],
        |row| {
            Ok(DecisionSignal::SmaCrossover(SmaSignal {
                replay_offset: stored_offset(row)?,
                source: stored_text(row, 1)?,
                product: stored_text(row, 2)?,
                channel: stored_text(row, 3)?,
                instrument_id: stored_text(row, 4)?,
                epoch: stored_text(row, 5)?,
                seq: stored_text(row, 6)?,
                cursor: stored_text(row, 7)?,
                license: stored_text(row, 8)?,
                exchange_ts: stored_text(row, 9)?,
                first_available_ts: stored_text(row, 10)?,
                decision_ts: stored_text(row, 11)?,
                target_shares: stored_text(row, 12)?,
            }))
        },
    )
}

fn bundle_signals(
    conn: &Connection,
    published: &Publication,
    after: Option<&str>,
    limit: u16,
) -> Result<Vec<DecisionSignal>, ApiError> {
    let (offset, recipe, instrument) = if let Some(after) = after {
        let mut parts = after.split('@');
        match (parts.next(), parts.next(), parts.next(), parts.next()) {
            (Some(position), Some(recipe), Some(instrument), None)
                if valid_recipe_id(recipe) && instrument.parse::<InstrumentId>().is_ok() =>
            {
                (offset(position)?, recipe, instrument)
            }
            _ => return Err(invalid_query()),
        }
    } else {
        (-1, "", "")
    };
    read_signals(conn, "bundle_decisions",
        "SELECT replay_offset,session,signal_ts,recipe,instrument_id,target_shares,reason
         FROM bundle_decisions WHERE plan_sha256=?1 AND (replay_offset,recipe,instrument_id)>(?2,?3,?4)
         ORDER BY replay_offset,recipe,instrument_id LIMIT ?5",
        params![published.plan_sha256, offset, recipe, instrument, limit],
        |row| Ok(DecisionSignal::StrategyBundle(BundleSignal {
            replay_offset: stored_offset(row)?,
            session: stored_text(row, 1)?,
            signal_ts: stored_text(row, 2)?,
            recipe: row.get(3)?,
            instrument_id: stored_text(row, 4)?,
            target_shares: stored_text(row, 5)?,
            reason: row.get(6)?,
        })),
    )
}

fn read_signals(
    conn: &Connection,
    table: &str,
    sql: &str,
    params: impl rusqlite::Params,
    map: impl FnMut(&Row<'_>) -> rusqlite::Result<DecisionSignal>,
) -> Result<Vec<DecisionSignal>, ApiError> {
    if !read_store::has_table(conn, table).map_err(|_| storage_error())? {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(sql).map_err(|_| storage_error())?;
    stmt.query_map(params, map)
        .map_err(|_| storage_error())?
        .collect::<rusqlite::Result<_>>()
        .map_err(|_| storage_error())
}

fn load(root: &Path, query: SignalQuery, limit: u16) -> Result<DecisionSignalPage, ApiError> {
    let published = publication(root)?;
    let after = query
        .after
        .as_deref()
        .map(|after| {
            after
                .split_once('@')
                .filter(|(plan, _)| *plan == published.plan_sha256)
                .map(|(_, rest)| rest)
                .ok_or_else(invalid_query)
        })
        .transpose()?;
    let mut conn = read_store::open(root, Path::new(DATABASE_PATH))
        .map_err(|_| storage_error())?
        .ok_or_else(not_published)?;
    let tx = conn.transaction().map_err(|_| storage_error())?;
    let mut items = match published.kind {
        SignalKind::SmaCrossover => sma_signals(&tx, &published, after, limit + 1)?,
        SignalKind::StrategyBundle => bundle_signals(&tx, &published, after, limit + 1)?,
    };
    let next_cursor = if items.len() > usize::from(limit) {
        items.pop();
        items.last().map(|row| row.cursor(&published.plan_sha256))
    } else {
        None
    };
    Ok(DecisionSignalPage {
        schema_version: 1,
        plan_sha256: published.plan_sha256,
        access: published.access,
        items,
        next_cursor,
    })
}

#[utoipa::path(get, path = "/api/v1/decisions/signals", operation_id = "decision_signals", params(SignalQuery),
    responses((status=200,body=DecisionSignalPage),(status=400,body=ErrorBody),
              (status=404,body=ErrorBody),(status=500,body=ErrorBody)))]
pub(crate) async fn signals(
    State(state): State<AppState>,
    query: Result<Query<SignalQuery>, QueryRejection>,
) -> Result<Json<DecisionSignalPage>, ApiError> {
    let Query(query) = query.map_err(|_| invalid_query())?;
    let limit = query.limit.unwrap_or(20);
    if !(1..=100).contains(&limit) || query.after.as_ref().is_some_and(|after| after.len() > 256) {
        return Err(invalid_query());
    }
    tokio::task::spawn_blocking(move || load(&state.data_root, query, limit).map(Json))
        .await
        .map_err(|_| storage_error())?
}

pub(crate) fn routes() -> Router<AppState> {
    Router::new().route("/api/v1/decisions/signals", get(signals))
}
