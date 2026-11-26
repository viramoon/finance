//! Synthetic Protobuf inputs for tests of the production message bus.

use std::collections::VecDeque;
use std::future::pending;
use std::io::{self, ErrorKind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use prost::Message;
use tokio::sync::Notify;

use super::io::{AsyncIo, AsyncReconnect, AsyncStream};
use super::AsyncTcpMessageBus;
use crate::connection::{r#async::AsyncConnection, ConnectionMetadata};
use crate::contracts::tick_types::TickType;
use crate::messages::{encode_protobuf_message, IncomingMessages, OutgoingMessages, PROTOBUF_MSG_ID};
use crate::{proto, server_versions, Client, Error};

/// One successfully written market data operation, decoded from an outbound frame.
#[derive(Debug, Eq, PartialEq)]
pub enum MarketWrite {
    /// Session-level preference.
    SelectType(Option<i32>),
    /// Contract confirmation for the assigned request ID.
    Confirm(Option<i32>),
    /// Request for the given contract and stream options.
    Subscribe {
        /// Subscription's assigned request ID.
        request_id: Option<i32>,
        /// Requested contract ID.
        con_id: Option<i32>,
        /// Generic tick list passed to Gateway.
        generic_ticks: Option<String>,
        /// One-shot snapshot mode.
        snapshot: bool,
        /// Regulatory snapshot mode.
        regulatory_snapshot: bool,
    },
    /// Cancellation for the given request ID.
    Cancel(Option<i32>),
}

/// An in-memory transport connected to the production async client and message bus.
pub struct MarketReplay {
    stream: ReplayStream,
    bus: Arc<AsyncTcpMessageBus<ReplayStream>>,
}

impl MarketReplay {
    /// Create a synthetic post-handshake client. This neither opens a socket nor records frames to disk.
    pub fn open() -> Result<(Self, Client), Error> {
        Self::open_mode(ReplayStream::default())
    }

    /// Create a replay that reads frames in the production background dispatcher.
    pub fn open_with_dispatcher() -> Result<(Self, Client), Error> {
        Self::open_mode(ReplayStream {
            dispatcher: Some(Arc::new(DispatcherSignals::default())),
            ..ReplayStream::default()
        })
    }

    fn open_mode(stream: ReplayStream) -> Result<(Self, Client), Error> {
        let metadata = ConnectionMetadata {
            client_id: 91,
            server_version: server_versions::PROTOBUF_REST_MESSAGES_3,
            ..Default::default()
        };
        let connection = AsyncConnection::replay(stream.clone(), metadata.client_id, metadata.server_version);
        let bus = Arc::new(AsyncTcpMessageBus::new(connection)?);
        if stream.dispatcher.is_some() {
            bus.clone().process_messages(metadata.server_version, Duration::from_secs(1))?;
        }
        let client = Client::new(metadata, bus.clone())?;
        Ok((Self { stream, bus }, client))
    }

    /// Inject a request-scoped actual market type callback in its generated Protobuf representation.
    pub async fn market_data_type(&self, request_id: i32, value: i32) -> Result<(), Error> {
        self.inject(
            IncomingMessages::MarketDataType,
            proto::MarketDataType {
                req_id: Some(request_id),
                market_data_type: Some(value),
            },
        )
        .await
    }

    /// Inject a price tick with an optional quantity encoded as supplier decimal text.
    pub async fn price(&self, request_id: i32, tick_type: TickType, price: f64, size: Option<&str>) -> Result<(), Error> {
        self.inject(
            IncomingMessages::TickPrice,
            proto::TickPrice {
                req_id: Some(request_id),
                tick_type: Some(tick_type as i32),
                price: Some(price),
                size: size.map(str::to_owned),
                attr_mask: None,
            },
        )
        .await
    }

    /// Inject a size tick, including the option open interest tick types.
    pub async fn size(&self, request_id: i32, tick_type: TickType, size: Option<&str>) -> Result<(), Error> {
        self.inject(
            IncomingMessages::TickSize,
            proto::TickSize {
                req_id: Some(request_id),
                tick_type: Some(tick_type as i32),
                size: size.map(str::to_owned),
            },
        )
        .await
    }

    /// Fail the next write while leaving the mock read side and SDK connection status intact.
    pub fn fail_next_write(&self) {
        self.stream.frames.lock().unwrap().fail_next_write = true;
    }

    /// Cause the background dispatcher to reconnect to a peer that never acknowledges its handshake.
    pub fn disconnect_and_stall_reconnect(&self) {
        let signals = self.stream.dispatcher.as_ref().expect("requires background dispatcher");
        self.stream.frames.lock().unwrap().inbound.push_back(Err(Error::ConnectionReset));
        signals.inbound_ready.notify_one();
    }

    /// Wait for the reconnect handshake response read to become pending.
    pub async fn wait_for_stalled_handshake(&self) {
        let signals = self.stream.dispatcher.as_ref().expect("requires background dispatcher");
        signals.handshake_started.notified().await;
    }

    /// Whether the reconnect handshake response read is still in flight.
    pub fn handshake_is_pending(&self) -> bool {
        self.stream
            .dispatcher
            .as_ref()
            .is_some_and(|signals| signals.handshake_pending.load(Ordering::Acquire))
    }

    /// Number of in-memory reconnect attempts.
    pub fn reconnect_attempts(&self) -> usize {
        self.stream.frames.lock().unwrap().reconnect_attempts
    }

    /// Wait until the given number of requests has reached the replay transport.
    pub async fn wait_for_writes(&self, count: usize) {
        let signals = self.stream.dispatcher.as_ref().expect("requires background dispatcher");
        while self.stream.frames.lock().unwrap().outbound.len() < count {
            signals.outbound_ready.notified().await;
        }
    }

    /// Decode all successful client writes captured in memory.
    pub fn writes(&self) -> Vec<MarketWrite> {
        self.stream
            .frames
            .lock()
            .unwrap()
            .outbound
            .iter()
            .map(|frame| decode_write(frame))
            .collect()
    }

    async fn inject(&self, kind: IncomingMessages, message: impl Message) -> Result<(), Error> {
        let body = encode_protobuf_message(kind as i32, &message.encode_to_vec());
        self.stream.frames.lock().unwrap().inbound.push_back(Ok(body));
        if let Some(signals) = &self.stream.dispatcher {
            signals.inbound_ready.notify_one();
            Ok(())
        } else {
            self.bus.read_and_route_message().await
        }
    }
}

fn decode_write(frame: &[u8]) -> MarketWrite {
    let length = u32::from_be_bytes(frame[..4].try_into().unwrap()) as usize;
    assert_eq!(length, frame.len() - 4);
    let kind = i32::from_be_bytes(frame[4..8].try_into().unwrap()) - PROTOBUF_MSG_ID;
    let body = &frame[8..];
    const SELECT_TYPE: i32 = OutgoingMessages::RequestMarketDataType as i32;
    const CONFIRM: i32 = OutgoingMessages::RequestContractData as i32;
    const SUBSCRIBE: i32 = OutgoingMessages::RequestMarketData as i32;
    const CANCEL: i32 = OutgoingMessages::CancelMarketData as i32;
    match kind {
        SELECT_TYPE => MarketWrite::SelectType(proto::MarketDataTypeRequest::decode(body).unwrap().market_data_type),
        CONFIRM => MarketWrite::Confirm(proto::ContractDataRequest::decode(body).unwrap().req_id),
        SUBSCRIBE => {
            let request = proto::MarketDataRequest::decode(body).unwrap();
            MarketWrite::Subscribe {
                request_id: request.req_id,
                con_id: request.contract.and_then(|contract| contract.con_id),
                generic_ticks: request.generic_tick_list,
                snapshot: request.snapshot.unwrap_or_default(),
                regulatory_snapshot: request.regulatory_snapshot.unwrap_or_default(),
            }
        }
        CANCEL => MarketWrite::Cancel(proto::CancelMarketData::decode(body).unwrap().req_id),
        _ => panic!("unexpected outgoing message in market replay: {kind}"),
    }
}

#[derive(Debug, Default)]
struct ReplayFrames {
    inbound: VecDeque<Result<Vec<u8>, Error>>,
    outbound: Vec<Vec<u8>>,
    fail_next_write: bool,
    reconnect_attempts: usize,
    handshake_written: bool,
}

#[derive(Clone, Debug, Default)]
struct ReplayStream {
    frames: Arc<Mutex<ReplayFrames>>,
    dispatcher: Option<Arc<DispatcherSignals>>,
}

#[derive(Debug, Default)]
struct DispatcherSignals {
    inbound_ready: Notify,
    outbound_ready: Notify,
    handshake_started: Notify,
    handshake_pending: AtomicBool,
}

struct HandshakeRead<'a>(&'a DispatcherSignals);

impl Drop for HandshakeRead<'_> {
    fn drop(&mut self) {
        self.0.handshake_pending.store(false, Ordering::Release);
    }
}

#[async_trait]
impl AsyncIo for ReplayStream {
    async fn read_message(&self) -> Result<Vec<u8>, Error> {
        loop {
            let (frame, handshake_written) = {
                let mut frames = self.frames.lock().unwrap();
                (frames.inbound.pop_front(), frames.handshake_written)
            };
            if let Some(frame) = frame {
                return frame;
            }
            let Some(signals) = &self.dispatcher else {
                return Err(Error::EndOfStream);
            };
            if handshake_written {
                // Entered only after the SDK wrote its real reconnect handshake.
                let _read = HandshakeRead(signals);
                signals.handshake_pending.store(true, Ordering::Release);
                signals.handshake_started.notify_one();
                return pending().await;
            }
            signals.inbound_ready.notified().await;
        }
    }

    async fn write_all(&self, frame: &[u8]) -> Result<(), Error> {
        let mut frames = self.frames.lock().unwrap();
        if std::mem::take(&mut frames.fail_next_write) {
            return Err(io::Error::new(ErrorKind::BrokenPipe, "synthetic write failure").into());
        }
        if frame.starts_with(b"API\0") {
            frames.handshake_written = true;
        } else {
            frames.outbound.push(frame.to_vec());
            if let Some(signals) = &self.dispatcher {
                signals.outbound_ready.notify_one();
            }
        }
        Ok(())
    }
}

#[async_trait]
impl AsyncReconnect for ReplayStream {
    async fn reconnect(&self) -> Result<(), Error> {
        self.frames.lock().unwrap().reconnect_attempts += 1;
        if self.dispatcher.is_some() {
            Ok(())
        } else {
            Err(Error::ConnectionFailed)
        }
    }

    async fn sleep(&self, _duration: Duration) {}
}

impl AsyncStream for ReplayStream {}
