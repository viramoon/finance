use axum::{
    Router,
    body::{Body, BodyDataStream, to_bytes},
    http::{Request, StatusCode, header},
};
use chrono::{DateTime, TimeDelta, Utc};
use finance_api::{
    ApiDoc, AppState,
    market_options::MarketReadSources,
    option_flow_stream::{OptionFlowStream, with_shutdown},
    router_with_market_reads,
};
use finance_collector::{
    line_budget::{BudgetStatus, CoverageInterval, CoverageReport, SchedulerPhase},
    market_ingress::ProductPolicy,
    option_flow::{
        ObservedFlow,
        observation::{
            BaselineCohort, FlowObservation, L1Sample, OBSERVATION_RULE_VERSION,
            ObservationEvidence, ObservationStatus, QuoteWidening,
        },
    },
};
use finance_ib::market::{Product, RequestKey};
use futures_util::{FutureExt, StreamExt};
use serde_json::{Value, json};
use std::{io, sync::Arc, time::Duration};
use tokio::{
    io::{DuplexStream, duplex},
    sync::{mpsc, watch},
    task::JoinHandle,
    time::timeout,
};
use tokio_tungstenite::{WebSocketStream, client_async, tungstenite::Message};
use tower::ServiceExt;
use utoipa::OpenApi;

const BASE: &str = "/api/v1/options/flow";
const START: &str = "2025-03-07T16:00:00Z";
const END: &str = "2025-03-07T16:00:10Z";
const CONTRACT: i32 = 42;
const PRECISE_BID: &str = "1.000000000000000001";
const WAIT: Duration = Duration::from_secs(5);

fn policy() -> ProductPolicy {
    ProductPolicy {
        source: "ibkr".parse().unwrap(),
        product: "opra".parse().unwrap(),
        license: "fixture.synthetic".parse().unwrap(),
        display_live: true,
        record_local: false,
        derive_non_display: false,
        backup_personal_drive: false,
    }
}

// Invented contract IDs and quote samples from a fixed clock. No capture or random prices.
fn observation(con_id: i32, second: i64) -> ObservedFlow {
    let observed_at: DateTime<Utc> =
        START.parse::<DateTime<Utc>>().unwrap() + TimeDelta::seconds(second);
    let sample = |value: &str| L1Sample {
        value: value.parse().unwrap(),
        received_at: observed_at,
    };
    ObservedFlow {
        access: policy(),
        clue: FlowObservation {
            rule_version: OBSERVATION_RULE_VERSION,
            status: ObservationStatus::PendingVerification,
            con_id,
            generation: u64::MAX.into(),
            cohort: BaselineCohort {
                underlying_con_id: 7,
                expiry: observed_at.date_naive(),
                session_segment: 0,
            },
            multiplier: "100".parse().unwrap(),
            observed_at,
            observation: ObservationEvidence::QuoteWidening(Box::new(QuoteWidening {
                bid: sample(PRECISE_BID),
                ask: sample("1.100000000000000001"),
                bid_size: sample("9007199254740993"),
                ask_size: sample("10"),
                spread: "0.1".parse().unwrap(),
                typical_spread: "0.05".parse().unwrap(),
                baseline_samples: 3,
            })),
        },
        budget: BudgetStatus {
            phase: SchedulerPhase::Online,
            as_of: observed_at,
            account_lines: 100,
            reserve_lines: 30,
            stock_lines: 9,
            hot_lines: 1,
            rotation_lines: 0,
            confirmed_options: 2,
            observed_options: 1,
            discovery_complete: false,
        },
        coverage: Some(CoverageReport {
            key: RequestKey {
                con_id,
                product: Product::Opra,
                parameters: Default::default(),
            },
            last_observed_at: Some(observed_at),
            covered: vec![],
            uncovered: vec![CoverageInterval {
                start: observed_at - TimeDelta::seconds(1),
                end: observed_at,
            }],
            unknown: vec![],
        }),
    }
}

fn app(root: &tempfile::TempDir, source: Option<Arc<OptionFlowStream>>) -> Router {
    router_with_market_reads(
        AppState {
            data_root: root.path().join("data"),
            research_root: root.path().join("research"),
        },
        MarketReadSources {
            flow_stream: source,
            ..Default::default()
        },
    )
}

type Client = WebSocketStream<DuplexStream>;

// Run the real HTTP upgrade and WebSocket codecs over in-memory byte streams.
struct TestListener(mpsc::UnboundedReceiver<DuplexStream>);

impl axum::serve::Listener for TestListener {
    type Io = DuplexStream;
    type Addr = ();

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        (self.0.recv().await.expect("test connection"), ())
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        Ok(())
    }
}

struct Server {
    connections: mpsc::UnboundedSender<DuplexStream>,
    task: JoinHandle<()>,
}

impl Server {
    fn start(app: Router) -> Self {
        let (connections, incoming) = mpsc::unbounded_channel();
        let task = tokio::spawn(async { axum::serve(TestListener(incoming), app).await.unwrap() });
        Self { connections, task }
    }

    async fn ws(&self, query: &str) -> Client {
        let (client, server) = duplex(4096);
        self.connections.send(server).unwrap();
        let (socket, response) = timeout(
            WAIT,
            client_async(
                format!("ws://localhost{BASE}/ws?con_id={CONTRACT}{query}"),
                client,
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "private, no-store"
        );
        socket
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn next_ws(socket: &mut Client) -> Message {
    timeout(WAIT, socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
}

async fn ws_message(socket: &mut Client) -> Value {
    let Message::Text(text) = next_ws(socket).await else {
        panic!("expected a text frame");
    };
    serde_json::from_str(&text).unwrap()
}

async fn sse_body(app: &Router, query: &str, last_id: Option<&str>) -> BodyDataStream {
    let mut request = Request::get(format!("{BASE}/sse?con_id={CONTRACT}{query}"));
    if let Some(id) = last_id {
        request = request.header("Last-Event-ID", id);
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "private, no-store"
    );
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/event-stream"
    );
    response.into_body().into_data_stream()
}

async fn sse_message(body: &mut BodyDataStream) -> Value {
    let event = timeout(WAIT, body.next()).await.unwrap().unwrap().unwrap();
    let wire = std::str::from_utf8(&event).unwrap();
    assert!(wire.ends_with("\n\n"));
    let mut envelope = json!({});
    for line in wire.lines().filter(|line| !line.is_empty()) {
        let (field, value) = line.split_once(": ").unwrap();
        envelope[field] = match field {
            "event" | "id" => json!(value),
            "data" => serde_json::from_str(value).unwrap(),
            _ => panic!("unexpected SSE field {field}"),
        };
    }
    envelope
}

async fn json_get(app: &Router, uri: &str, status: StatusCode) -> Value {
    let response = app
        .clone()
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), status);
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "private, no-store"
    );
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn websocket_push_and_sse_last_event_id_resume_preserve_order_and_precision() {
    let root = tempfile::tempdir().unwrap();
    let source = Arc::new(OptionFlowStream::new(policy()).unwrap());
    source.publish(observation(CONTRACT, 1)).unwrap();
    let app = app(&root, Some(source.clone()));
    let server = Server::start(app.clone());
    let mut ws = server.ws("").await;
    let status = ws_message(&mut ws).await;
    assert_eq!(status["event"], "status");
    assert_eq!(status["data"]["state"], "live");
    assert_eq!(status["data"]["schema_version"], "1.0");
    assert!(status["id"].is_string());

    source.publish(observation(CONTRACT + 1, 4)).unwrap();
    source.publish(observation(CONTRACT, 5)).unwrap();
    let first = ws_message(&mut ws).await;
    assert_eq!(first["event"], "observation");
    assert_eq!(first["data"]["seq"], "3");
    assert_eq!(first["data"]["access"], json!(policy()));
    let observation_payload = &first["data"]["observation"];
    assert_eq!(observation_payload["coverage"]["access"], json!(policy()));
    assert_eq!(
        observation_payload["clue"]["generation"],
        u64::MAX.to_string()
    );
    let quote = &observation_payload["clue"]["observation"]["evidence"];
    assert_eq!(quote["bid"]["value"], PRECISE_BID);
    assert_eq!(quote["bid_size"]["value"], "9007199254740993");
    let checkpoint = first["id"].as_str().unwrap();
    drop(ws);

    // Publication sequence wins when observation receipt times are out of order.
    source.publish(observation(CONTRACT, 3)).unwrap();
    let mut sse = sse_body(&app, "&cursor=stale-query", Some(checkpoint)).await;
    assert_eq!(sse_message(&mut sse).await["id"], checkpoint);
    let replayed = sse_message(&mut sse).await;
    assert_eq!(replayed["data"]["seq"], "4");
    assert!(
        replayed["data"]["observation"]["clue"]["observed_at"]
            .as_str()
            .unwrap()
            < observation_payload["clue"]["observed_at"].as_str().unwrap()
    );
    source.publish(observation(CONTRACT, 3)).unwrap();
    let live = sse_message(&mut sse).await;
    assert_eq!(live["data"]["seq"], "5");
    assert_eq!(live["data"]["observation"], replayed["data"]["observation"]);
    assert_ne!(live["id"], replayed["id"]);

    // Reconnecting from an earlier acknowledged point repeats the same event ID.
    let mut retry = server.ws(&format!("&cursor={checkpoint}")).await;
    assert_eq!(ws_message(&mut retry).await["id"], checkpoint);
    assert_eq!(ws_message(&mut retry).await, replayed);
    assert_eq!(ws_message(&mut retry).await, live);
    assert_eq!(root.path().read_dir().unwrap().count(), 0);
}

#[tokio::test]
async fn expired_cursor_terminates_both_transports_and_rest_checkpoint_recovers() {
    let root = tempfile::tempdir().unwrap();
    let source = Arc::new(OptionFlowStream::new(policy()).unwrap());
    let app = app(&root, Some(source.clone()));
    let mut fresh = sse_body(&app, "", None).await;
    let first = sse_message(&mut fresh).await;
    let expired = first["id"].as_str().unwrap();
    for _ in 0..100 {
        source.publish(observation(CONTRACT, 2)).unwrap();
    }
    source.publish(observation(CONTRACT + 1, 3)).unwrap();
    source.publish(observation(CONTRACT, 10)).unwrap();
    let expected_reset = json!({
        "event": "reset_required",
        "data": {
            "schema_version": "1.0",
            "con_id": CONTRACT,
            "reason": "cursor_unavailable",
            "snapshot": format!("{BASE}/snapshot")
        }
    });
    assert_eq!(sse_message(&mut fresh).await, expected_reset);
    assert!(timeout(WAIT, fresh.next()).await.unwrap().is_none());
    let mut expired_sse = sse_body(&app, "", Some(expired)).await;
    assert_eq!(sse_message(&mut expired_sse).await, expected_reset);
    assert!(timeout(WAIT, expired_sse.next()).await.unwrap().is_none());
    let server = Server::start(app.clone());
    let mut expired_ws = server.ws(&format!("&cursor={expired}")).await;
    assert_eq!(ws_message(&mut expired_ws).await, expected_reset);
    assert_eq!(next_ws(&mut expired_ws).await, Message::Close(None));

    let uri = format!("{BASE}/snapshot?con_id={CONTRACT}&start={START}&end={END}");
    let snapshot = json_get(&app, &uri, StatusCode::OK).await;
    let page = &snapshot["snapshot"];
    assert_eq!(page["access"], json!(policy()));
    assert_eq!(page["con_id"], CONTRACT);
    assert_eq!(page["start"], START);
    assert_eq!(page["end"], END);
    assert!(page["next_offset"].is_null());
    assert_eq!(page["items"].as_array().unwrap().len(), 98);
    assert_eq!(
        page["items"][0]["clue"]["observed_at"],
        "2025-03-07T16:00:02Z"
    );

    source.publish(observation(CONTRACT, 8)).unwrap();
    let checkpoint = snapshot["cursor"].as_str().unwrap();
    let mut resumed = sse_body(&app, &format!("&cursor={checkpoint}"), Some("")).await;
    assert_eq!(sse_message(&mut resumed).await["id"], checkpoint);
    let next = sse_message(&mut resumed).await;
    assert_eq!(next["data"]["seq"], "103");
    assert_eq!(
        next["data"]["observation"]["clue"]["observed_at"],
        "2025-03-07T16:00:08Z"
    );
}

#[tokio::test]
async fn disabled_collection_is_idle_and_shutdown_releases_both_transports() {
    let root = tempfile::tempdir().unwrap();
    let (stop, shutdown) = watch::channel(false);
    let app = with_shutdown(app(&root, None), shutdown);
    let server = Server::start(app.clone());
    let mut ws = server.ws("&cursor=previous-instance").await;
    let mut sse = sse_body(&app, "", Some("previous-instance")).await;
    let disabled = json!({
        "event": "status",
        "data": {"schema_version": "1.0", "con_id": CONTRACT, "state": "disabled"}
    });
    assert_eq!(ws_message(&mut ws).await, disabled);
    assert_eq!(sse_message(&mut sse).await, disabled);
    assert!(sse.next().now_or_never().is_none());
    assert!(ws.next().now_or_never().is_none());
    let uri = format!("{BASE}/snapshot?con_id={CONTRACT}&start={START}&end={END}");
    let error = json_get(&app, &uri, StatusCode::SERVICE_UNAVAILABLE).await;
    assert_eq!(error["error"]["code"], "market_read_source_unavailable");
    assert_eq!(root.path().read_dir().unwrap().count(), 0);

    stop.send_replace(true);
    assert!(timeout(WAIT, sse.next()).await.unwrap().is_none());
    assert_eq!(next_ws(&mut ws).await, Message::Close(None));
}

#[tokio::test]
async fn stream_queries_validate_contract_window_and_sse_resume_header() {
    let root = tempfile::tempdir().unwrap();
    let app = app(&root, None);
    for path in [
        format!("{BASE}/sse?con_id=0"),
        format!("{BASE}/sse?con_id={CONTRACT}&unknown=1"),
        format!("{BASE}/snapshot?con_id={CONTRACT}&start={END}&end={START}"),
    ] {
        let body = json_get(&app, &path, StatusCode::BAD_REQUEST).await;
        assert_eq!(body["error"]["code"], "market_read_query_invalid");
    }
    let response = app
        .oneshot(
            Request::get(format!("{BASE}/sse?con_id={CONTRACT}"))
                .header("Last-Event-ID", "first")
                .header("Last-Event-ID", "second")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[test]
fn openapi_advertises_all_three_read_only_entry_points() {
    let api = serde_json::to_value(ApiDoc::openapi()).unwrap();
    let paths = &api["paths"];
    let ws = &paths[format!("{BASE}/ws")]["get"];
    let sse = &paths[format!("{BASE}/sse")]["get"];
    let snapshot = &paths[format!("{BASE}/snapshot")]["get"];
    assert!(ws["responses"]["101"].is_object());
    assert!(sse["responses"]["200"].is_object());
    assert!(
        sse["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .any(|parameter| {
                parameter["name"] == "Last-Event-ID" && parameter["in"] == "header"
            })
    );
    assert_eq!(
        snapshot["responses"]["200"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/OptionFlowStreamSnapshot"
    );
    assert!(api["components"]["schemas"]["OptionFlowStreamSnapshot"].is_object());
}
