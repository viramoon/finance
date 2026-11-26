//! Asynchronous transport implementation

mod io;
pub(crate) use io::{AsyncStream, AsyncTcpSocket};

#[cfg(feature = "test-support")]
pub mod market_replay;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use log::{debug, error, info, warn};
use tokio::sync::{broadcast, mpsc, watch, Mutex, RwLock};
use tokio::task;
use tokio_stream::wrappers::{errors::BroadcastStreamRecvError, BroadcastStream};

use crate::connection::r#async::AsyncConnection;
use crate::messages::{shared_channel_configuration, IncomingMessages, Notice, OutgoingMessages, ResponseMessage};
use crate::Error;

use super::common::log_orphan;
use super::routing::{
    determine_routing, is_warning_error, order_routing_strategy, DecodedError, OrderRoutingStrategy, RoutingDecision, UNSPECIFIED_REQUEST_ID,
};
use super::RoutedItem;

/// Default capacity for broadcast channels
/// This should be large enough to handle bursts of messages without lagging
pub(crate) const BROADCAST_CHANNEL_CAPACITY: usize = 1024;

/// Cleanup signal for removing channels when subscriptions are dropped
#[derive(Debug, Clone)]
pub enum CleanupSignal {
    Request(i32),
    Order(i32),
    Shared(OutgoingMessages),
    OrderUpdateStream,
}

/// Asynchronous message bus trait
#[async_trait]
pub trait AsyncMessageBus: Send + Sync {
    async fn send_request(&self, request_id: i32, message: Vec<u8>) -> Result<AsyncInternalSubscription, Error>;

    async fn send_order_request(&self, order_id: i32, message: Vec<u8>) -> Result<AsyncInternalSubscription, Error>;

    async fn send_shared_request(&self, message_type: OutgoingMessages, message: Vec<u8>) -> Result<AsyncInternalSubscription, Error>;

    async fn send_message(&self, message: Vec<u8>) -> Result<(), Error>;

    #[allow(dead_code)]
    async fn cancel_subscription(&self, request_id: i32, message: Vec<u8>) -> Result<(), Error>;
    #[allow(dead_code)]
    async fn cancel_order_subscription(&self, order_id: i32, message: Vec<u8>) -> Result<(), Error>;

    async fn create_order_update_subscription(&self) -> Result<AsyncInternalSubscription, Error>;

    fn notice_subscribe(&self) -> crate::subscriptions::notice_stream::async_impl::NoticeStream;

    async fn ensure_shutdown(&self);

    fn request_shutdown_sync(&self);

    fn is_connected(&self) -> bool;
}

/// Internal subscription for async implementation.
///
/// Holds a `BroadcastStream<RoutedItem>` for poll-based consumption plus a
/// `template_receiver` kept solely so `Clone` can `resubscribe()` to produce an
/// independent stream. We cannot store the `Sender` instead — that would keep
/// the channel alive past the external sender's drop, breaking the
/// "channel closes when senders drop" termination contract.
pub struct AsyncInternalSubscription {
    /// Held only for `Clone` via `resubscribe()`. Never polled directly.
    template_receiver: broadcast::Receiver<RoutedItem>,
    pub(crate) stream: BroadcastStream<RoutedItem>,
    cleanup_sender: Option<mpsc::UnboundedSender<CleanupSignal>>,
    cleanup_signal: Option<CleanupSignal>,
    cleanup_sent: bool,
}

impl Clone for AsyncInternalSubscription {
    fn clone(&self) -> Self {
        // For clones, both template and stream start at the current tail of
        // the broadcast channel — clones see future messages, not the
        // original's history.
        let new_template = self.template_receiver.resubscribe();
        let new_polling = self.template_receiver.resubscribe();
        Self {
            template_receiver: new_template,
            stream: BroadcastStream::new(new_polling),
            cleanup_sender: self.cleanup_sender.clone(),
            cleanup_signal: self.cleanup_signal.clone(),
            cleanup_sent: false, // Each clone should handle its own cleanup
        }
    }
}

impl AsyncInternalSubscription {
    /// Construct an internal subscription wrapping a broadcast receiver.
    ///
    /// **Receiver positioning matters.** The receiver you pass is what feeds
    /// the stream — its position in the broadcast channel determines what
    /// the subscription sees. Pass the receiver paired with the messages
    /// you want consumed (typically the `rx` returned alongside the `tx`).
    /// **Do not** pass `rx.resubscribe()` unless you specifically want to
    /// skip messages already queued in `rx`; `resubscribe()` positions a
    /// fresh receiver at the channel's *current tail*, which silently
    /// hides already-queued items and causes "test ran to completion but
    /// the assertion never fired" failures.
    #[cfg(test)]
    pub(crate) fn new(receiver: broadcast::Receiver<RoutedItem>) -> Self {
        let template = receiver.resubscribe();
        Self {
            template_receiver: template,
            stream: BroadcastStream::new(receiver),
            cleanup_sender: None,
            cleanup_signal: None,
            cleanup_sent: false,
        }
    }

    pub(crate) fn with_cleanup(
        receiver: broadcast::Receiver<RoutedItem>,
        cleanup_sender: mpsc::UnboundedSender<CleanupSignal>,
        cleanup_signal: CleanupSignal,
    ) -> Self {
        let template = receiver.resubscribe();
        Self {
            template_receiver: template,
            stream: BroadcastStream::new(receiver),
            cleanup_sender: Some(cleanup_sender),
            cleanup_signal: Some(cleanup_signal),
            cleanup_sent: false,
        }
    }

    pub async fn next(&mut self) -> Option<Result<ResponseMessage, Error>> {
        loop {
            match self.next_complete().await? {
                Err(Error::ResponseLagged(_)) => continue,
                result => return Some(result),
            }
        }
    }

    /// Read a finite response without silently skipping lost callbacks.
    pub(crate) async fn next_complete(&mut self) -> Option<Result<ResponseMessage, Error>> {
        loop {
            match self.stream.next().await? {
                Ok(item) => {
                    if let Some(legacy) = item.into_legacy() {
                        return Some(legacy);
                    }
                }
                Err(BroadcastStreamRecvError::Lagged(skipped)) => return Some(Err(Error::ResponseLagged(skipped))),
            }
        }
    }

    /// Receive the next typed envelope (Response / Notice / Error) without
    /// the legacy projection. Kept because `src/transport/async_tests.rs`
    /// stub fixtures drive the channel directly via this helper.
    #[cfg(test)]
    pub(crate) async fn next_routed(&mut self) -> Option<RoutedItem> {
        loop {
            match self.stream.next().await? {
                Ok(item) => return Some(item),
                Err(_lagged) => continue,
            }
        }
    }

    /// Non-blocking poll for "is anything immediately available?". Returns
    /// `None` if the stream is pending or closed. Used by test fixtures that
    /// assert no cross-talk between subscriptions.
    #[cfg(test)]
    pub(crate) fn try_next_routed(&mut self) -> Option<RoutedItem> {
        use futures::FutureExt;
        loop {
            match self.stream.next().now_or_never()? {
                Some(Ok(item)) => return Some(item),
                Some(Err(_lagged)) => continue,
                None => return None,
            }
        }
    }

    /// Manually send cleanup signal
    fn send_cleanup_signal(&mut self) {
        if !self.cleanup_sent {
            if let (Some(sender), Some(signal)) = (&self.cleanup_sender, &self.cleanup_signal) {
                let _ = sender.send(signal.clone());
                self.cleanup_sent = true;
            }
        }
    }
}

/// Send cleanup signal when subscription is dropped
impl Drop for AsyncInternalSubscription {
    fn drop(&mut self) {
        self.send_cleanup_signal();
    }
}

type BroadcastSender = broadcast::Sender<RoutedItem>;

/// Asynchronous TCP message bus implementation
pub struct AsyncTcpMessageBus<S: AsyncStream = AsyncTcpSocket> {
    connection: Arc<AsyncConnection<S>>,
    /// Maps request IDs to their response channels
    request_channels: Arc<RwLock<HashMap<i32, BroadcastSender>>>,
    /// Maps IncomingMessages to broadcast senders (like sync does)
    shared_channel_senders: Arc<RwLock<HashMap<IncomingMessages, Vec<BroadcastSender>>>>,
    /// Maps OutgoingMessages to receivers for client subscription
    shared_channel_receivers: Arc<RwLock<HashMap<OutgoingMessages, broadcast::Receiver<RoutedItem>>>>,
    /// Maps order IDs to their response channels
    order_channels: Arc<RwLock<HashMap<i32, BroadcastSender>>>,
    /// Maps execution IDs to their response channels (for commission reports)
    execution_channels: Arc<RwLock<HashMap<String, BroadcastSender>>>,
    /// Optional channel for order update stream
    order_update_stream: Arc<RwLock<Option<BroadcastSender>>>,
    /// Channel for cleanup signals
    cleanup_sender: mpsc::UnboundedSender<CleanupSignal>,
    /// Handle to the message processing task
    process_task: Mutex<Option<task::JoinHandle<()>>>,
    /// Latched shutdown signal for the entire message loop, including reconnect.
    shutdown_requested: watch::Sender<bool>,
    connected: Arc<AtomicBool>,
}

impl<S: AsyncStream> Drop for AsyncTcpMessageBus<S> {
    fn drop(&mut self) {
        debug!("dropping async tcp message bus");
        self.request_shutdown_sync();
    }
}

impl<S: AsyncStream> AsyncTcpMessageBus<S> {
    /// Create a new async TCP message bus
    pub fn new(connection: AsyncConnection<S>) -> Result<Self, Error> {
        let (cleanup_sender, cleanup_receiver) = mpsc::unbounded_channel();

        // Pre-create broadcast channels for all shared channels (like sync does)
        let mut shared_channel_senders = HashMap::new();
        let mut shared_channel_receivers = HashMap::new();

        for mapping in shared_channel_configuration::CHANNEL_MAPPINGS {
            let (sender, receiver) = broadcast::channel(BROADCAST_CHANNEL_CAPACITY);
            shared_channel_receivers.insert(mapping.request, receiver);

            // Map each response type to the sender (multiple response types can share same sender)
            for response_type in mapping.responses {
                shared_channel_senders.entry(*response_type).or_insert_with(Vec::new).push(sender.clone());
            }
        }

        let message_bus = Self {
            connection: Arc::new(connection),
            request_channels: Arc::new(RwLock::new(HashMap::new())),
            shared_channel_senders: Arc::new(RwLock::new(shared_channel_senders)),
            shared_channel_receivers: Arc::new(RwLock::new(shared_channel_receivers)),
            order_channels: Arc::new(RwLock::new(HashMap::new())),
            execution_channels: Arc::new(RwLock::new(HashMap::new())),
            order_update_stream: Arc::new(RwLock::new(None)),
            cleanup_sender,
            process_task: Mutex::new(None),
            shutdown_requested: watch::Sender::new(false),
            connected: Arc::new(AtomicBool::new(true)),
        };

        // Start cleanup task
        let request_channels = message_bus.request_channels.clone();
        let order_channels = message_bus.order_channels.clone();
        let order_update_stream = message_bus.order_update_stream.clone();

        task::spawn(async move {
            let mut receiver = cleanup_receiver;
            while let Some(signal) = receiver.recv().await {
                match signal {
                    CleanupSignal::Request(request_id) => {
                        let mut channels = request_channels.write().await;
                        channels.remove(&request_id);
                        debug!("Cleaned up request channel for ID: {request_id}");
                    }
                    CleanupSignal::Order(order_id) => {
                        let mut channels = order_channels.write().await;
                        channels.remove(&order_id);
                        debug!("Cleaned up order channel for ID: {order_id}");
                    }
                    CleanupSignal::Shared(message_type) => {
                        // Shared channels are persistent and should not be removed
                        // They are created at initialization and reused across multiple requests
                        debug!("Subscription for shared channel {:?} ended (channel remains active)", message_type);
                    }
                    CleanupSignal::OrderUpdateStream => {
                        let mut stream = order_update_stream.write().await;
                        *stream = None;
                        debug!("Cleaned up order update stream ownership");
                    }
                }
            }
        });

        Ok(message_bus)
    }

    /// Start processing messages from TWS
    pub fn process_messages(self: Arc<Self>, auto_reconnect: bool) -> Result<(), Error> {
        // Publish the handle before returning the client. An immediate disconnect
        // must be able to join the task even before its first poll.
        let mut process_task = self.process_task.try_lock().map_err(|_| Error::Shutdown)?;
        if *self.shutdown_requested.borrow() {
            return Err(Error::Shutdown);
        }
        if process_task.is_some() {
            return Err(Error::InvalidArgument("message processing already started".into()));
        }
        let message_bus = self.clone();
        let mut shutdown = self.shutdown_requested.subscribe();
        *process_task = Some(task::spawn(async move {
            // Shutdown permanently discards this session, so it can interrupt
            // a frame read as well as reconnect backoff, dial, or handshake.
            tokio::select! {
                biased;
                _ = shutdown.wait_for(|requested| *requested) => {
                    debug!("Shutdown requested, stopping message processing");
                }
                _ = message_bus.dispatch_messages(auto_reconnect) => {}
            }
            message_bus.request_shutdown_sync();
            message_bus.finish_shutdown().await;
        }));

        Ok(())
    }

    async fn dispatch_messages(&self, auto_reconnect: bool) {
        use crate::client::error_handler::{is_connection_error, is_timeout_error};

        loop {
            match self.read_and_route_message().await {
                Ok(()) => {}
                Err(err) if is_timeout_error(&err) => {}
                Err(err) if auto_reconnect && is_connection_error(&err) => {
                    error!("Connection error detected, attempting to reconnect: {err:?}");
                    self.connected.store(false, Ordering::Relaxed);

                    match self.connection.reconnect().await {
                        Ok(()) => {
                            info!("Successfully reconnected to TWS/Gateway");
                            self.connected.store(true, Ordering::Relaxed);
                            self.reset_channels().await;
                        }
                        Err(err) => {
                            error!("Failed to reconnect to TWS/Gateway: {err:?}");
                            break;
                        }
                    }
                }
                Err(Error::Shutdown) => break,
                Err(err) => {
                    error!("Error processing message (shutting down): {err:?}");
                    break;
                }
            }
        }
    }

    /// Read a message and route it to the appropriate channel
    pub(crate) async fn read_and_route_message(&self) -> Result<(), Error> {
        let message = self.connection.read_message().await?;

        // Use common routing logic
        match determine_routing(&message) {
            RoutingDecision::ByRequestId(request_id) => self.route_to_request_channel(request_id, message).await,
            RoutingDecision::ByOrderId(order_id) => self.route_to_order_channel(order_id, message).await,
            RoutingDecision::ByMessageType(message_type) => self.route_to_shared_channel(message_type, message).await,
            RoutingDecision::SharedMessage(message_type) => self.route_to_shared_channel(message_type, message).await,
            RoutingDecision::Error(payload) => self.route_error_message(message, payload).await,
            RoutingDecision::Shutdown => Err(Error::Shutdown),
        }
    }

    /// Reset all channels after reconnection
    async fn reset_channels(&self) {
        debug!("resetting message bus channels");

        {
            let channels = self.request_channels.read().await;
            for (_, sender) in channels.iter() {
                let _ = sender.send(Error::ConnectionReset.into());
            }
        }

        {
            let channels = self.order_channels.read().await;
            for (_, sender) in channels.iter() {
                let _ = sender.send(Error::ConnectionReset.into());
            }
        }

        {
            let mut channels = self.request_channels.write().await;
            channels.clear();
        }

        {
            let mut channels = self.order_channels.write().await;
            channels.clear();
        }

        {
            let mut channels = self.execution_channels.write().await;
            channels.clear();
        }
    }

    /// Drop the senders so that outstanding subscriptions terminate.
    async fn finish_shutdown(&self) {
        // Release the socket even while old subscription handles still own the bus.
        self.connection.socket.close().await;
        {
            let mut channels = self.request_channels.write().await;
            channels.clear();
        }

        {
            let mut channels = self.order_channels.write().await;
            channels.clear();
        }

        {
            let mut channels = self.execution_channels.write().await;
            channels.clear();
        }

        {
            let mut channels = self.shared_channel_senders.write().await;
            channels.clear();
        }

        {
            let mut channels = self.shared_channel_receivers.write().await;
            channels.clear();
        }

        {
            let mut order_update_stream = self.order_update_stream.write().await;
            *order_update_stream = None;
        }
    }

    /// Route error message using routing decision
    async fn route_error_message(&self, message: ResponseMessage, payload: DecodedError) -> Result<(), Error> {
        if self.connection.terminal_notice.is_some_and(|matches| matches(payload.error_code)) {
            let notice = Notice::from(payload);
            let _ = self.connection.notice_sender.send(notice);
            return Err(Error::Shutdown);
        }
        let sent_to_update_stream = self.send_order_update(&message).await;
        let request_id = payload.request_id;
        let is_warning = is_warning_error(payload.error_code);

        if request_id == UNSPECIFIED_REQUEST_ID {
            let notice = Notice::from(payload);
            super::common::log_unrouted_notice(&notice);
            let _ = self.connection.notice_sender.send(notice);
        } else {
            let item = if is_warning {
                RoutedItem::Notice(Notice::from(payload))
            } else {
                RoutedItem::Error(Error::from(payload))
            };
            self.deliver_to_request_id(request_id, item, sent_to_update_stream).await;
        }

        Ok(())
    }

    /// Deliver a pre-classified Notice or Error to its owning subscription.
    /// Tries the request-channel first, falls back to the order-channel for
    /// notices/errors that arrive bound to an order_id.
    async fn deliver_to_request_id(&self, request_id: i32, item: RoutedItem, sent_to_update_stream: bool) {
        {
            let channels = self.request_channels.read().await;
            if let Some(sender) = channels.get(&request_id) {
                let _ = sender.send(item);
                return;
            }
        }
        {
            let order_channels = self.order_channels.read().await;
            if let Some(sender) = order_channels.get(&request_id) {
                let _ = sender.send(item);
                return;
            }
        }
        if !sent_to_update_stream {
            log_orphan(request_id, &item);
        }
    }

    /// Route message to request-specific channel
    async fn route_to_request_channel(&self, request_id: i32, message: ResponseMessage) -> Result<(), Error> {
        let channels = self.request_channels.read().await;
        if let Some(sender) = channels.get(&request_id) {
            let _ = sender.send(message.into());
        }
        Ok(())
    }

    /// Route message to order-specific channel
    async fn route_to_order_channel(&self, order_id: i32, message: ResponseMessage) -> Result<(), Error> {
        let routed = self.send_order_update(&message).await;
        let strategy = order_routing_strategy(message.message_type());

        match strategy {
            OrderRoutingStrategy::ExecutionData => {
                // Try order_id channel first, then request_id, storing execution_id mapping
                if let Some(actual_order_id) = message.order_id() {
                    let channels = self.order_channels.read().await;
                    if let Some(sender) = channels.get(&actual_order_id) {
                        self.store_execution_mapping(&message, sender).await;
                        let _ = sender.send(message.into());
                        return Ok(());
                    }
                }
                if let Some(req_id) = message.request_id() {
                    let channels = self.request_channels.read().await;
                    if let Some(sender) = channels.get(&req_id) {
                        self.store_execution_mapping(&message, sender).await;
                        let _ = sender.send(message.into());
                        return Ok(());
                    }
                }
                if !routed {
                    warn!("could not route ExecutionData message {:?}", message);
                }
            }
            OrderRoutingStrategy::ExecutionDataEnd => {
                if let Some(actual_order_id) = message.order_id() {
                    let channels = self.order_channels.read().await;
                    if let Some(sender) = channels.get(&actual_order_id) {
                        let _ = sender.send(message.into());
                        return Ok(());
                    }
                }
                if let Some(req_id) = message.request_id() {
                    let channels = self.request_channels.read().await;
                    if let Some(sender) = channels.get(&req_id) {
                        let _ = sender.send(message.into());
                        return Ok(());
                    }
                }
                warn!("could not route ExecutionDataEnd message {:?}", message);
            }
            OrderRoutingStrategy::OrderOrShared => {
                if let Some(actual_order_id) = message.order_id() {
                    let channels = self.order_channels.read().await;
                    if let Some(sender) = channels.get(&actual_order_id) {
                        let _ = sender.send(message.into());
                        return Ok(());
                    }
                    drop(channels);

                    let shared_channels = self.shared_channel_senders.read().await;
                    if let Some(senders) = shared_channels.get(&message.message_type()) {
                        for sender in senders {
                            let _ = sender.send(message.clone().into());
                        }
                        return Ok(());
                    }
                }
                if !routed {
                    warn!("could not route message {:?}", message);
                }
            }
            OrderRoutingStrategy::ByExecutionId => {
                if let Some(execution_id) = message.execution_id() {
                    let exec_channels = self.execution_channels.read().await;
                    if let Some(sender) = exec_channels.get(&execution_id) {
                        let _ = sender.send(message.into());
                        return Ok(());
                    }
                }
            }
            OrderRoutingStrategy::SharedOnly => {
                let shared_channels = self.shared_channel_senders.read().await;
                if let Some(senders) = shared_channels.get(&message.message_type()) {
                    for sender in senders {
                        let _ = sender.send(message.clone().into());
                    }
                    return Ok(());
                }
                if !routed {
                    warn!("could not route message {:?}", message);
                }
            }
            OrderRoutingStrategy::ByOrderId => {
                if order_id >= 0 {
                    let channels = self.order_channels.read().await;
                    if let Some(sender) = channels.get(&order_id) {
                        let _ = sender.send(message.into());
                        return Ok(());
                    }
                }
                if !routed {
                    warn!("could not route message {:?}", message);
                }
            }
        }

        Ok(())
    }

    /// Store execution_id -> sender mapping for commission report routing
    async fn store_execution_mapping(&self, message: &ResponseMessage, sender: &BroadcastSender) {
        if let Some(execution_id) = message.execution_id() {
            let mut exec_channels = self.execution_channels.write().await;
            exec_channels.insert(execution_id, sender.clone());
        }
    }

    /// Route message to shared channel
    async fn route_to_shared_channel(&self, message_type: IncomingMessages, message: ResponseMessage) -> Result<(), Error> {
        // Send order-related messages to order update stream
        match message_type {
            IncomingMessages::OpenOrder
            | IncomingMessages::OrderStatus
            | IncomingMessages::ExecutionData
            | IncomingMessages::CommissionsReport
            | IncomingMessages::CompletedOrder => {
                self.send_order_update(&message).await;
            }
            _ => {}
        }

        // Route to all senders for this message type (like sync does)
        let channels = self.shared_channel_senders.read().await;
        if let Some(senders) = channels.get(&message_type) {
            // Broadcast to all subscribers
            for sender in senders {
                if let Err(e) = sender.send(message.clone().into()) {
                    warn!("error sending to shared channel for {message_type:?}: {e}");
                }
            }
        }

        Ok(())
    }

    /// Send message to order update stream if it exists
    async fn send_order_update(&self, message: &ResponseMessage) -> bool {
        let order_update_stream = self.order_update_stream.read().await;
        if let Some(sender) = order_update_stream.as_ref() {
            if let Err(e) = sender.send(message.clone().into()) {
                warn!("error sending to order update stream: {e}");
                return false;
            }
            return true;
        }
        false
    }
}

#[async_trait]
impl<S: AsyncStream> AsyncMessageBus for AsyncTcpMessageBus<S> {
    async fn send_request(&self, request_id: i32, message: Vec<u8>) -> Result<AsyncInternalSubscription, Error> {
        let (sender, receiver) = broadcast::channel(BROADCAST_CHANNEL_CAPACITY);

        {
            let mut channels = self.request_channels.write().await;
            channels.insert(request_id, sender);
        }

        // Own registration before the write can fail or its future can be dropped.
        let subscription = AsyncInternalSubscription::with_cleanup(receiver, self.cleanup_sender.clone(), CleanupSignal::Request(request_id));
        self.send_message(message).await?;
        Ok(subscription)
    }

    async fn send_order_request(&self, order_id: i32, message: Vec<u8>) -> Result<AsyncInternalSubscription, Error> {
        let (sender, receiver) = broadcast::channel(BROADCAST_CHANNEL_CAPACITY);

        {
            let mut channels = self.order_channels.write().await;
            channels.insert(order_id, sender);
        }

        self.send_message(message).await?;

        Ok(AsyncInternalSubscription::with_cleanup(
            receiver,
            self.cleanup_sender.clone(),
            CleanupSignal::Order(order_id),
        ))
    }

    async fn send_shared_request(&self, message_type: OutgoingMessages, message: Vec<u8>) -> Result<AsyncInternalSubscription, Error> {
        let receiver = {
            let channels = self.shared_channel_receivers.read().await;
            if let Some(receiver) = channels.get(&message_type) {
                receiver.resubscribe()
            } else {
                return Err(Error::InvalidArgument(format!(
                    "No shared channel configured for message type: {:?}",
                    message_type
                )));
            }
        };

        self.send_message(message).await?;

        Ok(AsyncInternalSubscription::with_cleanup(
            receiver,
            self.cleanup_sender.clone(),
            CleanupSignal::Shared(message_type),
        ))
    }

    async fn send_message(&self, message: Vec<u8>) -> Result<(), Error> {
        let mut shutdown = self.shutdown_requested.subscribe();
        tokio::select! {
            biased;
            _ = shutdown.wait_for(|requested| *requested) => Err(Error::Shutdown),
            result = self.connection.write_message(&message) => result,
        }
    }

    async fn cancel_subscription(&self, request_id: i32, message: Vec<u8>) -> Result<(), Error> {
        self.send_message(message).await?;

        // Single write lock: the previous version held a read guard while
        // awaiting the write upgrade and self-deadlocked on the same task.
        let mut channels = self.request_channels.write().await;
        if let Some(sender) = channels.get(&request_id) {
            let _ = sender.send(Error::Cancelled.into());
        }
        channels.remove(&request_id);

        Ok(())
    }

    async fn cancel_order_subscription(&self, order_id: i32, message: Vec<u8>) -> Result<(), Error> {
        self.send_message(message).await?;

        let mut channels = self.order_channels.write().await;
        if let Some(sender) = channels.get(&order_id) {
            let _ = sender.send(Error::Cancelled.into());
        }
        channels.remove(&order_id);

        Ok(())
    }

    async fn create_order_update_subscription(&self) -> Result<AsyncInternalSubscription, Error> {
        let mut order_update_stream = self.order_update_stream.write().await;

        if order_update_stream.is_some() {
            return Err(Error::AlreadySubscribed);
        }

        let (sender, receiver) = broadcast::channel(BROADCAST_CHANNEL_CAPACITY);

        *order_update_stream = Some(sender);

        Ok(AsyncInternalSubscription::with_cleanup(
            receiver,
            self.cleanup_sender.clone(),
            CleanupSignal::OrderUpdateStream,
        ))
    }

    fn notice_subscribe(&self) -> crate::subscriptions::notice_stream::async_impl::NoticeStream {
        crate::subscriptions::notice_stream::async_impl::NoticeStream::new(self.connection.notice_sender.subscribe())
    }

    async fn ensure_shutdown(&self) {
        debug!("ensure_shutdown called");
        self.request_shutdown_sync();

        let mut process_task = self.process_task.lock().await;
        if let Some(handle) = process_task.as_mut() {
            // Keep the handle registered while joining. Another close can still
            // wait for the driver if this caller abandons its close future.
            let result = handle.await;
            *process_task = None;
            match result {
                Ok(()) => return,
                Err(err) => warn!("Error joining processing task: {err}"),
            }
        }
        self.finish_shutdown().await;
    }

    fn request_shutdown_sync(&self) {
        debug!("sync shutdown requested");
        self.connected.store(false, Ordering::Relaxed);
        self.shutdown_requested.send_replace(true);
    }

    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed) && !*self.shutdown_requested.borrow()
    }
}

#[cfg(test)]
pub(crate) use memory::MemoryStream;
