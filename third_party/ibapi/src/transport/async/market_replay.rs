//! Synthetic Protobuf inputs for tests of the production message bus.

use std::collections::VecDeque;
use std::io::{self, ErrorKind};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use prost::Message;

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
        let stream = ReplayStream::default();
        let metadata = ConnectionMetadata {
            client_id: 91,
            server_version: server_versions::PROTOBUF_REST_MESSAGES_3,
            ..Default::default()
        };
        let connection = AsyncConnection::replay(stream.clone(), metadata.client_id, metadata.server_version);
        let bus = Arc::new(AsyncTcpMessageBus::new(connection)?);
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
        self.stream.0.lock().unwrap().fail_next_write = true;
    }

    /// Decode all successful client writes captured in memory.
    pub fn writes(&self) -> Vec<MarketWrite> {
        self.stream.0.lock().unwrap().outbound.iter().map(|frame| decode_write(frame)).collect()
    }

    async fn inject(&self, kind: IncomingMessages, message: impl Message) -> Result<(), Error> {
        let body = encode_protobuf_message(kind as i32, &message.encode_to_vec());
        self.stream.0.lock().unwrap().inbound.push_back(body);
        self.bus.read_and_route_message().await
    }
}

fn decode_write(frame: &[u8]) -> MarketWrite {
    let length = u32::from_be_bytes(frame[..4].try_into().unwrap()) as usize;
    assert_eq!(length, frame.len() - 4);
    let kind = i32::from_be_bytes(frame[4..8].try_into().unwrap()) - PROTOBUF_MSG_ID;
    let body = &frame[8..];
    const SELECT_TYPE: i32 = OutgoingMessages::RequestMarketDataType as i32;
    const SUBSCRIBE: i32 = OutgoingMessages::RequestMarketData as i32;
    const CANCEL: i32 = OutgoingMessages::CancelMarketData as i32;
    match kind {
        SELECT_TYPE => MarketWrite::SelectType(proto::MarketDataTypeRequest::decode(body).unwrap().market_data_type),
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
    inbound: VecDeque<Vec<u8>>,
    outbound: Vec<Vec<u8>>,
    fail_next_write: bool,
}

#[derive(Clone, Debug, Default)]
struct ReplayStream(Arc<Mutex<ReplayFrames>>);

#[async_trait]
impl AsyncIo for ReplayStream {
    async fn read_message(&self) -> Result<Vec<u8>, Error> {
        self.0.lock().unwrap().inbound.pop_front().ok_or(Error::EndOfStream)
    }

    async fn write_all(&self, frame: &[u8]) -> Result<(), Error> {
        let mut frames = self.0.lock().unwrap();
        if std::mem::take(&mut frames.fail_next_write) {
            return Err(io::Error::new(ErrorKind::BrokenPipe, "synthetic write failure").into());
        }
        frames.outbound.push(frame.to_vec());
        Ok(())
    }
}

#[async_trait]
impl AsyncReconnect for ReplayStream {
    async fn reconnect(&self) -> Result<(), Error> {
        Err(Error::ConnectionFailed)
    }

    async fn sleep(&self, _duration: Duration) {}
}

impl AsyncStream for ReplayStream {}
