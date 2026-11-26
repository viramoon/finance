//! Synthetic Protobuf inputs for tests of the production message bus.

use std::collections::VecDeque;
use std::future::pending;
use std::io::{self, ErrorKind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use prost::Message;
use tokio::sync::{oneshot, Notify};

use super::io::{AsyncIo, AsyncReconnect, AsyncStream};
use super::AsyncTcpMessageBus;
use crate::connection::{r#async::AsyncConnection, ConnectionMetadata};
use crate::contracts::tick_types::TickType;
use crate::contracts::{ContractDetails, OptionChain};
use crate::messages::{encode_protobuf_message, IncomingMessages, OutgoingMessages, PROTOBUF_MSG_ID};
use crate::{proto, server_versions, Client, ClientBuilder, Error};

/// One successfully written market data operation, decoded from an outbound frame.
#[derive(Debug, Eq, PartialEq)]
pub enum MarketWrite {
    /// Explicit managed-account verification after the handshake.
    Accounts,
    /// Read-only connection probe heartbeat.
    ServerTime,
    /// Start the API during a scripted reconnect.
    StartApi(Option<i32>),
    /// Session-level preference.
    SelectType(Option<i32>),
    /// Contract confirmation for the assigned request ID.
    Confirm(Option<i32>),
    /// An option-chain metadata request.
    DiscoverOptions {
        /// ID used for definition callbacks.
        request_id: Option<i32>,
        /// Confirmed underlying identity.
        underlying_con_id: Option<i32>,
        /// Underlying symbol in the request.
        symbol: Option<String>,
        /// Futures or futures-options exchange in the request.
        exchange: Option<String>,
        /// Requested underlying security type.
        security_type: Option<String>,
    },
    /// One bounded request for listed options in an expiry and trading class.
    ConfirmOptionSeries {
        /// ID used for contract-details callbacks.
        request_id: Option<i32>,
        /// Requested expiry.
        expiry: Option<String>,
        /// Requested trading class.
        trading_class: Option<String>,
        /// Neither strike nor right was specified.
        wildcard_strike_and_right: bool,
    },
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
        Self::open_mode(ReplayStream::default(), Client::builder())
    }

    /// Create a replay that reads frames in the production background dispatcher.
    pub fn open_with_dispatcher() -> Result<(Self, Client), Error> {
        Self::open_with_builder(Client::builder())
    }

    /// Use a client builder's reconnect policy with the in-memory dispatcher.
    pub fn open_with_builder(builder: ClientBuilder) -> Result<(Self, Client), Error> {
        Self::open_mode(
            ReplayStream {
                dispatcher: Some(Arc::new(DispatcherSignals::default())),
                ..ReplayStream::default()
            },
            builder,
        )
    }

    fn open_mode(stream: ReplayStream, builder: ClientBuilder) -> Result<(Self, Client), Error> {
        let metadata = ConnectionMetadata {
            client_id: 91,
            server_version: server_versions::PROTOBUF_REST_MESSAGES_3,
            ..Default::default()
        };
        let mut connection = AsyncConnection::replay(stream.clone(), metadata.client_id, metadata.server_version);
        connection.terminal_notice = builder.terminal_notice;
        let bus = Arc::new(AsyncTcpMessageBus::new(connection)?);
        if stream.dispatcher.is_some() {
            bus.clone().process_messages(builder.auto_reconnect_enabled())?;
        }
        let client = Client::new(metadata, bus.clone())?;
        Ok((Self { stream, bus }, client))
    }

    /// Answer an equity confirmation from synthetic SMART contract metadata.
    pub async fn equity_contract(&self, request_id: i32, con_id: i32, symbol: &str, primary_exchange: &str) -> Result<(), Error> {
        self.inject(
            IncomingMessages::ContractData,
            proto::ContractData {
                req_id: Some(request_id),
                contract: Some(proto::Contract {
                    con_id: Some(con_id),
                    symbol: Some(symbol.to_owned()),
                    sec_type: Some("STK".into()),
                    exchange: Some("SMART".into()),
                    primary_exch: Some(primary_exchange.to_owned()),
                    currency: Some("USD".into()),
                    ..Default::default()
                }),
                contract_details: None,
            },
        )
        .await?;
        self.contract_data_end(request_id).await
    }

    /// Deliver a synthetic option-parameter row through the production dispatcher.
    pub async fn option_parameters(&self, request_id: i32, row: &OptionChain) -> Result<(), Error> {
        self.inject(
            IncomingMessages::SecurityDefinitionOptionParameter,
            proto::SecDefOptParameter {
                req_id: Some(request_id),
                exchange: Some(row.exchange.clone()),
                underlying_con_id: Some(row.underlying_contract_id),
                trading_class: Some(row.trading_class.clone()),
                multiplier: Some(row.multiplier.clone()),
                expirations: row.expirations.clone(),
                strikes: row.strikes.clone(),
            },
        )
        .await
    }

    /// Complete a synthetic option-parameter response.
    pub async fn option_parameters_end(&self, request_id: i32) -> Result<(), Error> {
        self.inject(
            IncomingMessages::SecurityDefinitionOptionParameterEnd,
            proto::SecDefOptParameterEnd { req_id: Some(request_id) },
        )
        .await
    }

    /// Deliver synthetic option details, including the underlying identity.
    pub async fn option_contract(&self, request_id: i32, row: &ContractDetails) -> Result<(), Error> {
        self.inject(
            IncomingMessages::ContractData,
            proto::ContractData {
                req_id: Some(request_id),
                contract: Some(proto::encoders::encode_contract(&row.contract)),
                contract_details: Some(proto::ContractDetails {
                    under_con_id: Some(row.under_contract_id),
                    under_sec_type: Some(row.under_security_type.clone()),
                    real_expiration_date: Some(row.real_expiration_date.clone()),
                    valid_exchanges: Some(row.valid_exchanges.join(",")),
                    ..Default::default()
                }),
            },
        )
        .await
    }

    /// Complete a synthetic contract-details response.
    pub async fn contract_data_end(&self, request_id: i32) -> Result<(), Error> {
        self.inject(IncomingMessages::ContractDataEnd, proto::ContractDataEnd { req_id: Some(request_id) })
            .await
    }

    /// Answer an explicit managed-account request with synthetic identities.
    pub async fn managed_accounts(&self, accounts: &str) -> Result<(), Error> {
        self.inject(
            IncomingMessages::ManagedAccounts,
            proto::ManagedAccounts {
                accounts_list: Some(accounts.into()),
            },
        )
        .await
    }

    /// Answer a heartbeat with an arbitrary synthetic Unix timestamp.
    pub async fn server_time(&self, timestamp: i64) -> Result<(), Error> {
        self.inject(
            IncomingMessages::CurrentTime,
            proto::CurrentTime {
                current_time: Some(timestamp),
            },
        )
        .await
    }

    /// Inject a global (-1) or request-scoped IB notice, without any real vendor text.
    pub async fn notice(&self, id: i32, code: i32) -> Result<(), Error> {
        self.inject(
            IncomingMessages::Error,
            proto::ErrorMessage {
                id: Some(id),
                error_code: Some(code),
                error_msg: Some("synthetic notice".into()),
                ..Default::default()
            },
        )
        .await
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
        self.fail_next_write_with(ErrorKind::BrokenPipe);
    }

    /// Return this local I/O error on the next write while the read side stays open.
    pub fn fail_next_write_with(&self, kind: ErrorKind) {
        self.stream.frames.lock().unwrap().write_error = Some(kind);
    }

    /// Accept the next outbound frame, but wait for release before completing its write.
    pub fn hold_next_write(&self) -> oneshot::Sender<()> {
        let (release, held) = oneshot::channel();
        self.stream.frames.lock().unwrap().held_write = Some(held);
        release
    }

    /// Cause the background dispatcher to reconnect to a peer that never acknowledges its handshake.
    pub fn disconnect_and_stall_reconnect(&self) {
        let signals = self.stream.dispatcher.as_ref().expect("requires background dispatcher");
        self.stream.frames.lock().unwrap().inbound.push_back(Err(Error::ConnectionReset));
        signals.inbound_ready.notify_one();
    }

    /// Trigger a socket loss with a complete synthetic handshake queued for a replacement connection.
    pub fn disconnect_and_answer_reconnect(&self, accounts: &str) {
        self.queue_reconnect_reply(accounts, None);
    }

    /// Put a notice inside a replacement handshake, before account information.
    pub fn disconnect_with_handshake_notice(&self, accounts: &str, code: i32) {
        self.queue_reconnect_reply(accounts, Some(code));
    }

    fn queue_reconnect_reply(&self, accounts: &str, code: Option<i32>) {
        let signals = self.stream.dispatcher.as_ref().expect("requires background dispatcher");
        let mut frames = self.stream.frames.lock().unwrap();
        frames.inbound.push_back(Err(Error::ConnectionReset));
        frames.inbound.push_back(Ok(format!(
            "{}\0{}\0",
            server_versions::PROTOBUF_REST_MESSAGES_3,
            "20000101 00:00:00 UTC"
        )
        .into_bytes()));
        if let Some(code) = code {
            frames.inbound.push_back(Ok(encode_protobuf_message(
                IncomingMessages::Error as i32,
                &proto::ErrorMessage {
                    id: Some(-1),
                    error_code: Some(code),
                    ..Default::default()
                }
                .encode_to_vec(),
            )));
        }
        frames.inbound.push_back(Ok(encode_protobuf_message(
            IncomingMessages::NextValidId as i32,
            &proto::NextValidId { order_id: Some(1) }.encode_to_vec(),
        )));
        frames.inbound.push_back(Ok(encode_protobuf_message(
            IncomingMessages::ManagedAccounts as i32,
            &proto::ManagedAccounts {
                accounts_list: Some(accounts.to_owned()),
            }
            .encode_to_vec(),
        )));
        frames.reconnect_answered = true;
        signals.inbound_ready.notify_one();
    }

    /// Return the account list parsed by the new handshake after dispatch resumed.
    pub async fn reconnected_accounts(&self) -> String {
        let signals = self.stream.dispatcher.as_ref().expect("requires background dispatcher");
        signals.reconnect_ready.notified().await;
        self.bus.connection.connection_metadata().await.managed_accounts
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

    /// Whether the bus released its underlying transport.
    pub fn is_closed(&self) -> bool {
        self.stream.frames.lock().unwrap().closed
    }

    /// Number of request channels still owned by the dispatcher.
    pub async fn registered_requests(&self) -> usize {
        self.bus.request_channels.read().await.len()
    }

    /// Capacity of each broadcast buffer for request-scoped response frames.
    pub fn request_buffer_capacity(&self) -> usize {
        super::BROADCAST_CHANNEL_CAPACITY
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
    const START_API: i32 = OutgoingMessages::StartApi as i32;
    const ACCOUNTS: i32 = OutgoingMessages::RequestManagedAccounts as i32;
    const SERVER_TIME: i32 = OutgoingMessages::RequestCurrentTime as i32;
    const SELECT_TYPE: i32 = OutgoingMessages::RequestMarketDataType as i32;
    const CONFIRM: i32 = OutgoingMessages::RequestContractData as i32;
    const DISCOVER_OPTIONS: i32 = OutgoingMessages::RequestSecurityDefinitionOptionalParameters as i32;
    const SUBSCRIBE: i32 = OutgoingMessages::RequestMarketData as i32;
    const CANCEL: i32 = OutgoingMessages::CancelMarketData as i32;
    match kind {
        ACCOUNTS => MarketWrite::Accounts,
        SERVER_TIME => MarketWrite::ServerTime,
        START_API => MarketWrite::StartApi(proto::StartApiRequest::decode(body).unwrap().client_id),
        SELECT_TYPE => MarketWrite::SelectType(proto::MarketDataTypeRequest::decode(body).unwrap().market_data_type),
        CONFIRM => {
            let request = proto::ContractDataRequest::decode(body).unwrap();
            match request.contract {
                Some(contract) if contract.sec_type.as_deref() == Some("OPT") && contract.con_id.is_none() => MarketWrite::ConfirmOptionSeries {
                    request_id: request.req_id,
                    expiry: contract.last_trade_date_or_contract_month,
                    trading_class: contract.trading_class,
                    wildcard_strike_and_right: contract.strike.is_none() && contract.right.is_none(),
                },
                _ => MarketWrite::Confirm(request.req_id),
            }
        }
        DISCOVER_OPTIONS => {
            let request = proto::SecDefOptParamsRequest::decode(body).unwrap();
            MarketWrite::DiscoverOptions {
                request_id: request.req_id,
                underlying_con_id: request.underlying_con_id,
                symbol: request.underlying_symbol,
                exchange: request.fut_fop_exchange,
                security_type: request.underlying_sec_type,
            }
        }
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
    write_error: Option<ErrorKind>,
    held_write: Option<oneshot::Receiver<()>>,
    closed: bool,
    reconnect_attempts: usize,
    handshake_written: bool,
    reconnect_answered: bool,
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
    reconnect_ready: Notify,
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
            let (frame, handshake_written, reconnect_answered) = {
                let mut frames = self.frames.lock().unwrap();
                (frames.inbound.pop_front(), frames.handshake_written, frames.reconnect_answered)
            };
            if let Some(frame) = frame {
                return frame;
            }
            let Some(signals) = &self.dispatcher else {
                return Err(Error::EndOfStream);
            };
            if handshake_written {
                if reconnect_answered {
                    signals.reconnect_ready.notify_one();
                } else {
                    // Entered only after the SDK wrote its real reconnect handshake.
                    let _read = HandshakeRead(signals);
                    signals.handshake_pending.store(true, Ordering::Release);
                    signals.handshake_started.notify_one();
                    return pending().await;
                }
            }
            signals.inbound_ready.notified().await;
        }
    }

    async fn write_all(&self, frame: &[u8]) -> Result<(), Error> {
        let held = {
            let mut frames = self.frames.lock().unwrap();
            if frames.closed {
                return Err(Error::Shutdown);
            }
            if let Some(kind) = frames.write_error.take() {
                return Err(io::Error::new(kind, "synthetic write failure").into());
            }
            if frame.starts_with(b"API\0") {
                frames.handshake_written = true;
            } else {
                frames.outbound.push(frame.to_vec());
                if let Some(signals) = &self.dispatcher {
                    signals.outbound_ready.notify_one();
                }
            }
            frames.held_write.take()
        };
        if let Some(held) = held {
            held.await.map_err(|_| Error::Shutdown)?;
        }
        Ok(())
    }

    async fn close(&self) {
        self.frames.lock().unwrap().closed = true;
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
