//! Private WS and SSE delivery of already-observed OPRA L1 clues.

mod store;
pub(crate) mod wire;

pub use store::{OptionFlowStream, PublishError};

use crate::{
    ApiError, AppState, ErrorBody,
    market_options::{
        ReadSources,
        flow::{OptionFlowObservation, OptionFlowObservationPage, OptionFlowWindow},
        invalid, parse, unavailable,
    },
};
use axum::{
    Extension, Json, Router,
    extract::{
        Query, WebSocketUpgrade,
        rejection::QueryRejection,
        ws::{Message, WebSocket},
    },
    http::HeaderMap,
    response::{IntoResponse, Sse, sse::KeepAlive},
    routing::get,
};
use chrono::{DateTime, Utc};
use futures_util::stream;
use serde::{Deserialize, Serialize};
use std::{future::pending, time::Duration};
use store::Subscription;
use tokio::{
    sync::watch,
    time::{Instant, interval_at, timeout},
};
use utoipa::{IntoParams, ToSchema};
use wire::{OptionFlowMessage, SNAPSHOT_PATH};

const KEEP_ALIVE: Duration = Duration::from_secs(15);

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/options/flow/ws", get(ws))
        .route("/api/v1/options/flow/sse", get(sse))
        .route(SNAPSHOT_PATH, get(snapshot))
}

#[derive(Clone)]
pub(crate) struct StreamShutdown(watch::Receiver<bool>);

/// Close active option-flow connections when the hosting API begins graceful shutdown.
pub fn with_shutdown(router: Router, shutdown: watch::Receiver<bool>) -> Router {
    router.layer(Extension(StreamShutdown(shutdown)))
}

#[derive(Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(crate) struct StreamQuery {
    #[param(minimum = 1)]
    con_id: i32,
    /// Opaque cursor from the same contract's stream or recovery snapshot.
    cursor: Option<String>,
}

impl StreamQuery {
    fn checked(query: Result<Query<Self>, QueryRejection>) -> Result<Self, ApiError> {
        let query = parse(query)?;
        if query.con_id <= 0 {
            return Err(invalid());
        }
        Ok(query)
    }
}

#[derive(Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(crate) struct SnapshotQuery {
    #[param(minimum = 1)]
    con_id: i32,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct OptionFlowStreamSnapshot {
    /// Resume after this snapshot using the same contract.
    cursor: String,
    snapshot: OptionFlowObservationPage,
}

#[utoipa::path(
    get, path = "/api/v1/options/flow/snapshot", params(SnapshotQuery),
    responses(
        (status = 200, body = OptionFlowStreamSnapshot),
        (status = 400, body = ErrorBody),
        (status = 503, body = ErrorBody)
    )
)]
pub(crate) async fn snapshot(
    Extension(sources): ReadSources,
    query: Result<Query<SnapshotQuery>, QueryRejection>,
) -> Result<Json<OptionFlowStreamSnapshot>, ApiError> {
    let query = parse(query)?;
    let window = OptionFlowWindow::checked(query.con_id, query.start, query.end)?;
    let source = sources.flow_stream.as_ref().ok_or_else(unavailable)?;
    let snapshot = source.snapshot(window);
    Ok(Json(OptionFlowStreamSnapshot {
        cursor: snapshot.cursor,
        snapshot: OptionFlowObservationPage::new(
            window,
            source.access(),
            snapshot.as_of,
            snapshot
                .records
                .iter()
                .map(|record| OptionFlowObservation::from_record(record)),
            None,
        ),
    }))
}

#[utoipa::path(
    get, path = "/api/v1/options/flow/ws", params(StreamQuery),
    responses(
        (status = 101, description = "Option-flow WS 1.0. See option-flow-ws.schema.json."),
        (status = 400, description = "Invalid query or WebSocket handshake"),
        (status = 426, description = "HTTP upgrade required")
    )
)]
pub(crate) async fn ws(
    Extension(sources): ReadSources,
    shutdown: Option<Extension<StreamShutdown>>,
    query: Result<Query<StreamQuery>, QueryRejection>,
    upgrade: WebSocketUpgrade,
) -> Result<impl IntoResponse, ApiError> {
    let query = StreamQuery::checked(query)?;
    let connection = Connection::new(
        sources.flow_stream.as_deref(),
        query.con_id,
        query.cursor.as_deref(),
        shutdown,
    );
    Ok(upgrade
        .max_message_size(1024)
        .max_frame_size(1024)
        .on_upgrade(move |socket| send_ws(socket, connection)))
}

async fn send_ws(mut socket: WebSocket, mut connection: Connection) {
    let mut keep_alive = interval_at(Instant::now() + KEEP_ALIVE, KEEP_ALIVE);
    loop {
        let frame = tokio::select! {
            message = connection.next() => match message {
                Some(message) => match serde_json::to_string(&message) {
                    Ok(json) => Message::Text(json.into()),
                    Err(_) => break,
                },
                None => {
                    let _ = timeout(KEEP_ALIVE, socket.send(Message::Close(None))).await;
                    break;
                }
            },
            received = socket.recv() => match received {
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                _ => break,
            },
            _ = keep_alive.tick() => Message::Ping(Default::default()),
        };
        if !matches!(timeout(KEEP_ALIVE, socket.send(frame)).await, Ok(Ok(()))) {
            break;
        }
    }
}

#[utoipa::path(
    get, path = "/api/v1/options/flow/sse",
    params(
        StreamQuery,
        ("Last-Event-ID" = Option<String>, Header, description = "Opaque resume cursor. Takes precedence over the query cursor when present and nonempty.")
    ),
    responses(
        (status = 200, description = "Option-flow SSE 1.0. See option-flow-sse.schema.json.", content_type = "text/event-stream"),
        (status = 400, body = ErrorBody)
    )
)]
pub(crate) async fn sse(
    Extension(sources): ReadSources,
    shutdown: Option<Extension<StreamShutdown>>,
    headers: HeaderMap,
    query: Result<Query<StreamQuery>, QueryRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let query = StreamQuery::checked(query)?;
    let mut ids = headers.get_all("last-event-id").iter();
    let last_id = ids
        .next()
        .map(|id| id.to_str())
        .transpose()
        .map_err(|_| invalid())?;
    if ids.next().is_some() {
        return Err(invalid());
    }
    let connection = Connection::new(
        sources.flow_stream.as_deref(),
        query.con_id,
        last_id
            .filter(|id| !id.is_empty())
            .or(query.cursor.as_deref()),
        shutdown,
    );
    let frames = stream::unfold(connection, |mut connection| async move {
        connection
            .next()
            .await
            .map(|message| (message.to_sse_event(), connection))
    });
    Ok(Sse::new(frames).keep_alive(KeepAlive::new().interval(KEEP_ALIVE)))
}

struct Connection {
    con_id: i32,
    initial: Option<OptionFlowMessage>,
    state: Delivery,
    shutdown: Option<StreamShutdown>,
}

enum Delivery {
    Live(Subscription),
    Disabled,
    Done,
}

impl Connection {
    fn new(
        source: Option<&OptionFlowStream>,
        con_id: i32,
        cursor: Option<&str>,
        shutdown: Option<Extension<StreamShutdown>>,
    ) -> Self {
        let (initial, state) = match source.map(|stream| stream.subscribe(con_id, cursor)) {
            Some(Ok(subscription)) => (
                OptionFlowMessage::status(con_id, Some(subscription.cursor())),
                Delivery::Live(subscription),
            ),
            Some(Err(_)) => (OptionFlowMessage::reset_required(con_id), Delivery::Done),
            None => (OptionFlowMessage::status(con_id, None), Delivery::Disabled),
        };
        Self {
            con_id,
            initial: Some(initial),
            state,
            shutdown: shutdown.map(|Extension(shutdown)| shutdown),
        }
    }

    async fn next(&mut self) -> Option<OptionFlowMessage> {
        let delivery = async {
            if let Some(initial) = self.initial.take() {
                return Some(initial);
            }
            match &mut self.state {
                Delivery::Live(subscription) => match subscription.next().await {
                    Ok(Some(event)) => Some(OptionFlowMessage::observation(
                        event.cursor,
                        event.seq,
                        &event.record,
                    )),
                    Ok(None) => None,
                    Err(_) => {
                        self.state = Delivery::Done;
                        Some(OptionFlowMessage::reset_required(self.con_id))
                    }
                },
                Delivery::Disabled => pending().await,
                Delivery::Done => None,
            }
        };
        if let Some(shutdown) = &mut self.shutdown {
            tokio::select! {
                biased;
                _ = shutdown.0.wait_for(|stop| *stop) => None,
                message = delivery => message,
            }
        } else {
            delivery.await
        }
    }
}
