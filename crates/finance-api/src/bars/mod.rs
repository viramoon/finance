//! Chart queries over revision-resolved, display-approved bar projections.

mod source;

pub use source::{
    BarProvenance, BarSeriesKey, BarSnapshot, BarsReadError, BarsSource, DisplayPolicy,
    LocalBarsSource, ProjectedBar, ProjectionVersion,
};

use crate::{
    ApiError, ErrorBody, error,
    indicators::{
        IndicatorDefinition, IndicatorEngine, IndicatorError, IndicatorSpec, IndicatorValue,
        ValueStatus, registry,
    },
};
use axum::{
    Json, Router,
    extract::{Query, Request, State},
    http::{HeaderValue, StatusCode, Uri, header::CACHE_CONTROL},
    middleware::{self, Next},
    response::Response,
    routing::get,
};
use chrono::{DateTime, NaiveDate, Utc};
use finance_market_protocol::{InstrumentId, Token};
use serde::{Deserialize, Deserializer, Serialize, de};
use serde_json::{Map, Value};
use std::{collections::BTreeMap, fmt, sync::Arc};
use utoipa::{IntoParams, ToSchema};

const MAX_QUERY_BYTES: usize = 8 * 1024;
const MAX_INDICATORS: usize = 16;
const DEFAULT_LIMIT: usize = 500;
const MAX_LIMIT: usize = 1_000;
const MAX_INDICATOR_WORK: usize = 5_000_000;

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
struct BarsQuery {
    instrument_id: InstrumentId,
    source: Token,
    product: Token,
    /// Existing projected period, e.g. `1m`, `1h`, or `1d`.
    timeframe: Token,
    /// Exact registered price-basis label used by the publisher.
    adjustment: Token,
    /// Inclusive lower bound on bar open time, RFC3339.
    start: DateTime<Utc>,
    /// Exclusive upper bound on bar open time, RFC3339.
    end: DateTime<Utc>,
    #[param(default = false)]
    include_provisional: Option<bool>,
    #[param(default = 500, minimum = 1, maximum = 1000)]
    limit: Option<usize>,
    #[param(ignore)]
    indicators: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct IndicatorRequest {
    #[serde(rename = "use")]
    reference: String,
    #[serde(deserialize_with = "unique_params")]
    #[schema(value_type = Object)]
    params: Value,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ParameterValue {
    Integer(u64),
    Text(String),
}

fn unique_params<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Value, D::Error> {
    struct UniqueParams;

    impl<'de> de::Visitor<'de> for UniqueParams {
        type Value = Value;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("an indicator parameter object")
        }

        fn visit_map<A: de::MapAccess<'de>>(self, mut access: A) -> Result<Value, A::Error> {
            let mut params = Map::new();
            while let Some((name, value)) = access.next_entry::<String, ParameterValue>()? {
                let value = match value {
                    ParameterValue::Integer(integer) => Value::from(integer),
                    ParameterValue::Text(text) => Value::from(text),
                };
                if params.insert(name, value).is_some() {
                    return Err(de::Error::custom("duplicate indicator parameter"));
                }
            }
            Ok(Value::Object(params))
        }
    }

    deserializer.deserialize_map(UniqueParams)
}

struct ChartQuery {
    series: BarSeriesKey,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    include_provisional: bool,
    limit: usize,
    indicators: Vec<IndicatorRequest>,
    specs: Vec<IndicatorSpec>,
}

impl ChartQuery {
    fn parse(uri: &Uri, require_indicators: bool) -> Result<Self, ApiError> {
        if uri
            .query()
            .is_some_and(|query| query.len() > MAX_QUERY_BYTES)
        {
            return Err(invalid_query());
        }
        let Query(query) = Query::<BarsQuery>::try_from_uri(uri).map_err(|_| invalid_query())?;
        let limit = query.limit.unwrap_or(DEFAULT_LIMIT);
        if query.start >= query.end
            || !(1..=MAX_LIMIT).contains(&limit)
            || !valid_period(query.timeframe.as_str())
        {
            return Err(invalid_query());
        }
        let indicators = query
            .indicators
            .as_deref()
            .map(serde_json::from_str::<Vec<IndicatorRequest>>)
            .transpose()
            .map_err(|_| invalid_indicator())?
            .unwrap_or_default();
        if indicators.len() > MAX_INDICATORS || (require_indicators && indicators.is_empty()) {
            return Err(invalid_indicator());
        }
        let specs = indicators
            .iter()
            .map(|indicator| IndicatorSpec::new(&indicator.reference, &indicator.params))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| invalid_indicator())?;
        Ok(Self {
            series: BarSeriesKey {
                instrument_id: query.instrument_id,
                source: query.source,
                product: query.product,
                timeframe: query.timeframe,
                adjustment: query.adjustment,
            },
            start: query.start,
            end: query.end,
            include_provisional: query.include_provisional.unwrap_or(false),
            limit,
            indicators,
            specs,
        })
    }
}

fn valid_period(text: &str) -> bool {
    // v1 addresses intraday bars and single-session daily bars. The publisher
    // supplies actual boundaries (including shortened exchange sessions).
    let Some((number, unit)) = text.split_at_checked(text.len().saturating_sub(1)) else {
        return false;
    };
    let maximum = match unit {
        "s" => 86_400,
        "m" => 1_440,
        "h" => 24,
        "d" => 1,
        _ => return false,
    };
    !number.starts_with('0')
        && number
            .parse::<u32>()
            .is_ok_and(|count| (1..=maximum).contains(&count))
}

#[derive(Debug, Serialize, ToSchema)]
#[schema(as = ChartCoverage)]
pub struct Coverage {
    /// All closed observations from the publisher's declared calculation origin.
    history_complete: bool,
    /// Earliest closed bar in the snapshot, if any. Internal gaps may exist.
    #[schema(required = true)]
    first_closed_open_ts: Option<DateTime<Utc>>,
    #[schema(required = true)]
    last_closed_end_ts: Option<DateTime<Utc>>,
    closed_bars: usize,
    returned_closed_bars: usize,
    returned_provisional: bool,
}

#[derive(Debug, Serialize, ToSchema)]
struct ChartContext {
    series: BarSeriesKey,
    provenance: BarProvenance,
    /// Publication time of the latest revision-resolved snapshot.
    as_of: DateTime<Utc>,
    coverage: Coverage,
    /// Inclusive start time for the next page, or null when exhausted.
    #[schema(required = true)]
    next_start: Option<DateTime<Utc>>,
    /// In request order, with the same indexes as each row's indicator values.
    indicators: Vec<IndicatorRequest>,
}

#[derive(Debug, Serialize, ToSchema)]
struct ChartValue {
    status: ValueStatus,
    /// Named exact decimal strings. Empty while warming up or when undefined.
    values: BTreeMap<&'static str, String>,
}

impl From<IndicatorValue> for ChartValue {
    fn from(value: IndicatorValue) -> Self {
        Self {
            status: value.status,
            values: value
                .values
                .into_iter()
                .map(|(field, value)| (field, value.to_string()))
                .collect(),
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
struct ChartBar {
    bar: ProjectedBar,
    provisional: bool,
    indicator_values: Vec<ChartValue>,
}

#[derive(Debug, Serialize, ToSchema)]
pub(super) struct ChartBars {
    #[serde(flatten)]
    context: ChartContext,
    rows: Vec<ChartBar>,
}

#[derive(Debug, Serialize, ToSchema)]
struct ValueRow {
    open_ts: DateTime<Utc>,
    end_ts: DateTime<Utc>,
    /// Availability of the bar revision. Derived values use the full latest snapshot.
    first_available_ts: DateTime<Utc>,
    #[schema(value_type = String, format = Date)]
    trading_day: NaiveDate,
    provisional: bool,
    indicator_values: Vec<ChartValue>,
}

#[derive(Debug, Serialize, ToSchema)]
pub(super) struct ChartValues {
    #[serde(flatten)]
    context: ChartContext,
    rows: Vec<ValueRow>,
}

#[derive(Serialize, ToSchema)]
pub(super) struct IndicatorCatalog {
    definitions: &'static [IndicatorDefinition],
}

pub(crate) fn routes<S>(source: Arc<dyn BarsSource>) -> Router<S> {
    Router::new()
        .route("/api/v1/market/bars", get(chart))
        .route("/api/v1/indicators", get(catalog))
        .route("/api/v1/indicators/values", get(values))
        .layer(middleware::from_fn(no_store))
        .with_state(source)
}

async fn no_store(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    response
}

/// Read a page of chart bars and optionally calculate registered indicators.
#[utoipa::path(get, path = "/api/v1/market/bars", params(
    BarsQuery,
    ("indicators" = Option<String>, Query, description = "Percent-encoded JSON array of use and params objects.")
), responses(
    (status = 200, body = ChartBars),
    (status = 400, body = ErrorBody),
    (status = 404, body = ErrorBody),
    (status = 422, body = ErrorBody),
    (status = 500, body = ErrorBody)
))]
pub(super) async fn chart(
    State(source): State<Arc<dyn BarsSource>>,
    uri: Uri,
) -> Result<Json<ChartBars>, ApiError> {
    query(source, ChartQuery::parse(&uri, false)?)
        .await
        .map(Json)
}

/// Discover exact indicator versions, parameter ranges and calculation rules.
#[utoipa::path(get, path = "/api/v1/indicators", responses((status = 200, body = IndicatorCatalog)))]
pub(super) async fn catalog() -> Json<IndicatorCatalog> {
    Json(IndicatorCatalog {
        definitions: registry(),
    })
}

/// Read indicator points for a chart series. At least one indicator is required.
#[utoipa::path(get, path = "/api/v1/indicators/values", params(
    BarsQuery,
    ("indicators" = String, Query, description = "Percent-encoded JSON array of at least one use and params object.")
), responses(
    (status = 200, body = ChartValues),
    (status = 400, body = ErrorBody),
    (status = 404, body = ErrorBody),
    (status = 422, body = ErrorBody),
    (status = 500, body = ErrorBody)
))]
pub(super) async fn values(
    State(source): State<Arc<dyn BarsSource>>,
    uri: Uri,
) -> Result<Json<ChartValues>, ApiError> {
    let ChartBars { context, rows } = query(source, ChartQuery::parse(&uri, true)?).await?;
    let rows = rows
        .into_iter()
        .map(|row| ValueRow {
            open_ts: row.bar.open_ts,
            end_ts: row.bar.end_ts,
            first_available_ts: row.bar.first_available_ts,
            trading_day: row.bar.trading_day,
            provisional: row.provisional,
            indicator_values: row.indicator_values,
        })
        .collect();
    Ok(Json(ChartValues { context, rows }))
}

async fn query(source: Arc<dyn BarsSource>, query: ChartQuery) -> Result<ChartBars, ApiError> {
    tokio::task::spawn_blocking(move || calculate(&*source, query))
        .await
        .map_err(|_| {
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "bars_query_failed",
                "Cannot process bars query",
            )
        })?
}

fn calculate(source: &dyn BarsSource, query: ChartQuery) -> Result<ChartBars, ApiError> {
    let snapshot = source.snapshot(&query.series).map_err(source_error)?;
    if !snapshot.valid(&query.series) {
        return Err(source_error(BarsReadError::Invalid));
    }
    if !snapshot.history_complete && !query.specs.is_empty() {
        return Err(error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "indicator_history_unavailable",
            "This projection lacks the full prefix required for indicators",
        ));
    }

    let visible_start = snapshot
        .closed
        .partition_point(|bar| bar.open_ts < query.start);
    let visible_end = snapshot
        .closed
        .partition_point(|bar| bar.open_ts < query.end);
    let page_end = visible_end.min(visible_start + query.limit);
    let visible_preview = snapshot
        .provisional
        .filter(|bar| query.include_provisional && (query.start..query.end).contains(&bar.open_ts));
    let returned_closed = page_end - visible_start;
    let returned_provisional = visible_preview.is_some() && returned_closed < query.limit;
    let next_start = if page_end < visible_end {
        Some(snapshot.closed[page_end].open_ts)
    } else if returned_provisional {
        None
    } else {
        visible_preview.as_ref().map(|bar| bar.open_ts)
    };
    let context = ChartContext {
        series: snapshot.series,
        provenance: snapshot.provenance,
        as_of: snapshot.as_of,
        coverage: Coverage {
            history_complete: snapshot.history_complete,
            first_closed_open_ts: snapshot.closed.first().map(|bar| bar.open_ts),
            last_closed_end_ts: snapshot.closed.last().map(|bar| bar.end_ts),
            closed_bars: snapshot.closed.len(),
            returned_closed_bars: returned_closed,
            returned_provisional,
        },
        next_start,
        indicators: query.indicators,
    };

    let mut rows = Vec::with_capacity(returned_closed + usize::from(returned_provisional));
    if returned_closed == 0 && !returned_provisional {
        return Ok(ChartBars { context, rows });
    }

    let bars_to_evaluate = page_end + usize::from(returned_provisional);
    let indicator_work: usize = query
        .specs
        .iter()
        .map(|spec| {
            let warmup = bars_to_evaluate.min(spec.prior_bars() as usize);
            warmup + (bars_to_evaluate - warmup) * spec.observations_per_update()
        })
        .sum();
    if indicator_work > MAX_INDICATOR_WORK {
        return Err(error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "indicator_work_limit",
            "Indicator query requires too much work. Reduce limit or select fewer indicators",
        ));
    }

    let mut engine = (!query.specs.is_empty()).then(|| IndicatorEngine::new(&query.specs));
    if let Some(engine) = &mut engine {
        for bar in &snapshot.closed[..visible_start] {
            engine
                .advance_closed(&bar.indicator_bar())
                .map_err(calculation_error)?;
        }
    }
    for bar in snapshot
        .closed
        .into_iter()
        .take(page_end)
        .skip(visible_start)
    {
        let output = engine
            .as_mut()
            .map(|engine| engine.push_closed(&bar.indicator_bar()))
            .transpose()
            .map_err(calculation_error)?
            .unwrap_or_default();
        rows.push(chart_row(bar, false, output));
    }
    if let Some(bar) = visible_preview.filter(|_| returned_provisional) {
        let output = engine
            .as_ref()
            .map(|engine| engine.preview(&bar.indicator_bar()))
            .transpose()
            .map_err(calculation_error)?
            .unwrap_or_default();
        rows.push(chart_row(bar, true, output));
    }
    Ok(ChartBars { context, rows })
}

fn chart_row(bar: ProjectedBar, provisional: bool, output: Vec<IndicatorValue>) -> ChartBar {
    ChartBar {
        bar,
        provisional,
        indicator_values: output.into_iter().map(ChartValue::from).collect(),
    }
}

fn calculation_error(_: IndicatorError) -> ApiError {
    error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "indicator_calculation_failed",
        "Cannot calculate indicators for this projection",
    )
}

fn invalid_query() -> ApiError {
    error(
        StatusCode::BAD_REQUEST,
        "invalid_bars_query",
        "Invalid series, range or page parameters",
    )
}

fn invalid_indicator() -> ApiError {
    error(
        StatusCode::BAD_REQUEST,
        "invalid_indicator",
        "Invalid indicator selection or parameters",
    )
}

fn source_error(error_kind: BarsReadError) -> ApiError {
    match error_kind {
        BarsReadError::Missing => error(
            StatusCode::NOT_FOUND,
            "bars_not_found",
            "No authorized bars projection for this series",
        ),
        BarsReadError::Unreadable => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "bars_unreadable",
            "Cannot read bars projection",
        ),
        BarsReadError::Invalid => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "bars_invalid",
            "Invalid bars projection",
        ),
    }
}
