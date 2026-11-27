pub mod config;
pub mod entitlements;
pub mod historical;
pub mod market;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
pub use config::IbConfig;
use ibapi::Client;
use serde::Serialize;
use std::{sync::Arc, time::Duration};
use tokio::{
    sync::watch,
    time::{Instant, sleep, timeout},
};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("configuration: {0}")]
    Config(String),
    #[error("paper account verification failed")]
    UnsafeAccount,
    #[error("{0} timed out")]
    Timeout(&'static str),
    #[error("gateway: {0}")]
    Gateway(String),
    #[error("gateway disconnected: {0}")]
    Disconnected(String),
    #[error("recovery: {0}")]
    Recovery(String),
}
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Disconnected,
    Connecting,
    Connected,
    Degraded,
    Reconnecting,
}
#[derive(Clone, Debug, Serialize)]
pub struct ConnectionStatus {
    pub state: Phase,
    pub last_connected_at: Option<DateTime<Utc>>,
    pub last_heartbeat_at: Option<DateTime<Utc>>,
    pub disconnects: u64,
    pub last_error: Option<String>,
}
#[async_trait]
pub trait Session: Send + Sync + 'static {
    async fn accounts(&self) -> Result<Vec<String>>;
    async fn server_time(&self) -> Result<i64>;
    /// Transport-level notices can report an upstream farm failure on a healthy socket.
    async fn disconnected(&self) -> Error {
        std::future::pending().await
    }
    async fn close(&self);
}
/// The Gateway handle exposes only the read operations used by finance.
///
/// ```compile_fail
/// fn raw_client(peer: &finance_ib::PaperPeer) {
///     let _ = &peer.client;
/// }
/// ```
pub struct PaperPeer {
    client: Client,
    notices: tokio::sync::Mutex<ibapi::subscriptions::NoticeStream>,
    invalidated: watch::Sender<bool>,
    market_pacing: watch::Sender<Option<Instant>>,
}
impl PaperPeer {
    fn new(client: Client, notices: ibapi::subscriptions::NoticeStream) -> Self {
        Self {
            client,
            notices: tokio::sync::Mutex::new(notices),
            invalidated: watch::Sender::new(false),
            market_pacing: watch::Sender::new(None),
        }
    }

    fn invalidate(&self) {
        self.invalidated.send_replace(true);
    }

    fn is_active(&self) -> bool {
        if *self.invalidated.borrow() {
            return false;
        }
        if self.client.is_connected() {
            return true;
        }
        self.invalidate();
        false
    }

    fn checked_reply<T>(&self, reply: std::result::Result<T, ibapi::Error>) -> Result<T> {
        // SDK one-shot helpers can yield buffered data (or an empty account
        // list) after the dispatcher has already torn down the connection.
        if self.is_active() {
            reply.map_err(Error::from)
        } else {
            Err(Error::Disconnected(
                "connection closed during request".into(),
            ))
        }
    }

    pub fn historical_data<'a>(
        &'a self,
        contract: &'a ibapi::contracts::Contract,
        bar_size: ibapi::market_data::historical::BarSize,
    ) -> ibapi::market_data::historical::HistoricalDataBuilder<'a, Client> {
        self.client.historical_data(contract, bar_size)
    }

    pub async fn switch_market_data_type(
        &self,
        mode: ibapi::market_data::MarketDataType,
    ) -> std::result::Result<(), ibapi::Error> {
        self.client.switch_market_data_type(mode).await
    }
}
pub fn invalidates_market_session(code: i32) -> bool {
    matches!(code, 1100 | 1101 | 1102 | 1300 | 2103 | 2105 | 2110)
}

fn is_disconnect(error: &ibapi::Error) -> bool {
    use ibapi::Error as SdkError;
    error.is_connection_lost()
        || matches!(error, SdkError::Io(io) if io.kind() == std::io::ErrorKind::NotConnected)
        || matches!(
            error,
            SdkError::ConnectionFailed
                | SdkError::ConnectionRejected(_)
                | SdkError::EndOfStream
                | SdkError::UnexpectedEndOfStream
                | SdkError::Shutdown
        )
        || matches!(error, SdkError::Notice(notice) if invalidates_market_session(notice.code))
}

impl From<ibapi::Error> for Error {
    fn from(error: ibapi::Error) -> Self {
        match error {
            ibapi::Error::Notice(notice) if invalidates_market_session(notice.code) => {
                Self::Disconnected(format!("IB connectivity notice {}", notice.code))
            }
            error if is_disconnect(&error) => Self::Disconnected(error.to_string()),
            error => Self::Gateway(error.to_string()),
        }
    }
}

#[async_trait]
impl Session for PaperPeer {
    async fn accounts(&self) -> Result<Vec<String>> {
        self.checked_reply(self.client.managed_accounts().await)
    }
    async fn server_time(&self) -> Result<i64> {
        self.checked_reply(self.client.server_time().await)
            .map(|d| d.unix_timestamp())
    }
    async fn disconnected(&self) -> Error {
        let mut invalidated = self.invalidated.subscribe();
        let notice = async {
            let mut notices = self.notices.lock().await;
            while let Some(notice) = notices.next().await {
                if invalidates_market_session(notice.code) {
                    return format!("IB connectivity notice {}", notice.code);
                }
                if market::market_notice_kind(&notice) == market::MarketNotice::Pacing {
                    self.market_pacing.send_replace(Some(Instant::now()));
                }
            }
            "IB notice stream ended".into()
        };
        let socket = async {
            while self.client.is_connected() {
                sleep(Duration::from_millis(100)).await;
            }
        };
        let reason = tokio::select! {
            biased;
            _ = invalidated.wait_for(|closed| *closed) => {
                return Error::Disconnected("session closed".into());
            }
            reason = notice => reason,
            _ = socket => "socket closed".into(),
        };
        self.invalidate();
        Error::Disconnected(reason)
    }
    async fn close(&self) {
        self.invalidate();
        self.client.disconnect().await
    }
}
#[async_trait]
pub trait Transport: Send + Sync {
    type S: Session;
    async fn connect(&self, address: &str, id: i32) -> Result<Self::S>;
}
pub struct IbTransport;
impl IbTransport {
    fn paper_client_builder() -> ibapi::ClientBuilder {
        // ConnectionManager verifies an account on each new connection. The
        // SDK must never silently replace the socket behind that verification.
        Client::builder()
            .auto_reconnect(false)
            .disconnect_on_notice(invalidates_market_session)
    }
}
#[async_trait]
impl Transport for IbTransport {
    type S = PaperPeer;
    async fn connect(&self, address: &str, id: i32) -> Result<PaperPeer> {
        let (client, notices) = Self::paper_client_builder()
            .address(address)
            .client_id(id)
            .connect_with_notice_stream()
            .await
            .map_err(Error::from)?;
        Ok(PaperPeer::new(client, notices))
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum RecoveryStage {
    Subscriptions,
    Backfill,
    Reconciliation,
}
#[async_trait]
pub trait Recovery<S: Session>: Send {
    async fn restore(&mut self, stage: RecoveryStage, session: Arc<S>) -> Result<()>;
    /// Failure of a subscription can precede socket heartbeat failure.
    async fn check(&mut self) -> Result<()> {
        Ok(())
    }
    async fn offline(&mut self) {}
}
pub struct ConnectionManager<T: Transport> {
    config: IbConfig,
    transport: T,
    status: watch::Sender<ConnectionStatus>,
}
impl ConnectionManager<IbTransport> {
    /// Open a dedicated market-client session on the configured probe client ID.
    pub async fn open_market(
        &self,
    ) -> std::result::Result<market::PaperMarket, market::MarketError> {
        Self::require_market_recording_disabled()?;
        self.open_verified_market().await
    }

    /// Open a verified paper session and report observed global pacing notices
    /// through a channel owned by the caller. The channel remains available
    /// after failures during account verification or market type selection.
    /// An SDK connect failure or timeout before a peer is available returns
    /// `MarketError::HandshakeFailed`, since no notice can reach the channel.
    pub async fn open_market_with_pacing(
        &self,
        pacing: &watch::Sender<Option<Instant>>,
    ) -> std::result::Result<market::PaperMarket, market::MarketError> {
        Self::require_market_recording_disabled()?;
        self.open_verified_market_with_pacing(pacing).await
    }

    fn require_market_recording_disabled() -> std::result::Result<(), market::MarketError> {
        // The SDK would otherwise record raw handshake and market frames.
        if std::env::var_os("IBAPI_RECORDING_DIR").is_some_and(|path| !path.is_empty()) {
            return Err(market::MarketError::InvalidConfiguration);
        }
        Ok(())
    }
}
impl<T: Transport<S = PaperPeer>> ConnectionManager<T> {
    async fn open_verified_market(
        &self,
    ) -> std::result::Result<market::PaperMarket, market::MarketError> {
        self.open_market_attempt(None).await
    }

    async fn open_verified_market_with_pacing(
        &self,
        pacing: &watch::Sender<Option<Instant>>,
    ) -> std::result::Result<market::PaperMarket, market::MarketError> {
        self.open_market_attempt(Some(pacing)).await
    }

    async fn open_market_attempt(
        &self,
        pacing: Option<&watch::Sender<Option<Instant>>>,
    ) -> std::result::Result<market::PaperMarket, market::MarketError> {
        let deadline = Instant::now() + Duration::from_millis(self.config.connect_timeout_ms);
        let mut peer = self
            .connect(self.config.probe_client_id, deadline)
            .await
            .map_err(|error| match error {
                // A caller tracking cooldowns needs to know when the SDK
                // could not hand us the startup notices.
                Error::UnsafeAccount | Error::Config(_) => market_connection_error(error),
                _ if pacing.is_some() => market::MarketError::HandshakeFailed,
                _ => market_connection_error(error),
            })?;
        if let Some(pacing) = pacing {
            peer.market_pacing = pacing.clone();
        }
        let (peer, _) = self
            .verify_accounts(peer, deadline)
            .await
            .map_err(market_connection_error)?;
        match market::PaperMarket::new(
            peer.clone(),
            Duration::from_millis(self.config.read_timeout_ms),
        )
        .await
        {
            Ok(market) => Ok(market),
            Err(error) => {
                peer.close().await;
                Err(error)
            }
        }
    }
}

fn market_connection_error(error: Error) -> market::MarketError {
    match error {
        Error::UnsafeAccount => market::MarketError::UnsafeAccount,
        Error::Config(_) => market::MarketError::InvalidConfiguration,
        Error::Timeout(_) => market::MarketError::Timeout,
        Error::Disconnected(_) => market::MarketError::Disconnected,
        _ => market::MarketError::Transport,
    }
}

impl<T: Transport> ConnectionManager<T> {
    pub fn new(config: IbConfig, transport: T) -> Result<Self> {
        config.validate()?;
        let (status, _) = watch::channel(ConnectionStatus {
            state: Phase::Disconnected,
            last_connected_at: None,
            last_heartbeat_at: None,
            disconnects: 0,
            last_error: None,
        });
        Ok(Self {
            config,
            transport,
            status,
        })
    }
    pub fn subscribe(&self) -> watch::Receiver<ConnectionStatus> {
        self.status.subscribe()
    }
    fn phase(&self, phase: Phase) {
        self.status.send_modify(|s| s.state = phase);
        tracing::info!(?phase, "IB session transition");
    }
    async fn open(&self, id: i32) -> Result<(Arc<T::S>, Vec<String>)> {
        let deadline = Instant::now() + Duration::from_millis(self.config.connect_timeout_ms);
        let session = self.connect(id, deadline).await?;
        self.verify_accounts(session, deadline).await
    }
    async fn connect(&self, id: i32, deadline: Instant) -> Result<T::S> {
        tokio::time::timeout_at(deadline, self.transport.connect(&self.config.address, id))
            .await
            .map_err(|_| Error::Timeout("connect"))?
    }
    async fn verify_accounts(
        &self,
        session: T::S,
        deadline: Instant,
    ) -> Result<(Arc<T::S>, Vec<String>)> {
        let session = Arc::new(session);
        let accounts = tokio::select! {
            biased;
            error = session.disconnected() => Err(error),
            result = tokio::time::timeout_at(deadline, session.accounts()) =>
                result.unwrap_or(Err(Error::Timeout("managed_accounts"))),
        };
        match accounts {
            Ok(accounts)
                if !accounts.is_empty()
                    && accounts
                        .iter()
                        .all(|s| s.starts_with(&self.config.account_prefix)) =>
            {
                Ok((session, accounts))
            }
            other => {
                session.close().await;
                Err(other.err().unwrap_or(Error::UnsafeAccount))
            }
        }
    }
    async fn heartbeat(&self, session: &T::S) -> Result<i64> {
        timeout(
            Duration::from_millis(self.config.read_timeout_ms),
            session.server_time(),
        )
        .await
        .map_err(|_| Error::Timeout("server_time"))?
    }
    pub async fn probe(&self) -> Result<(i64, Vec<String>)> {
        let deadline = Instant::now() + Duration::from_millis(self.config.connect_timeout_ms);
        let (session, accounts) = self.open(self.config.probe_client_id).await?;
        let time = tokio::select! {
            biased;
            error = session.disconnected() => Err(error),
            result = tokio::time::timeout_at(deadline, self.heartbeat(&session)) =>
                result.unwrap_or(Err(Error::Timeout("paper probe"))),
        };
        session.close().await;
        Ok((time?, accounts))
    }
    /// Run until the caller cancels this future or paper validation fails. Each live
    /// session is disconnected before retry. Only a verified session reaches hooks.
    pub async fn run(&self, recovery: &mut impl Recovery<T::S>) -> Result<()> {
        let mut attempts = 0u32;
        let mut outage = Instant::now();
        loop {
            self.phase(Phase::Connecting);
            let failure = match self.open(self.config.collector_client_id).await {
                Err(Error::UnsafeAccount) => {
                    self.status
                        .send_modify(|s| s.last_error = Some(Error::UnsafeAccount.to_string()));
                    self.phase(Phase::Disconnected);
                    return Err(Error::UnsafeAccount);
                }
                Err(e) => e,
                Ok((session, _)) => {
                    let serving = async {
                        for stage in [
                            RecoveryStage::Subscriptions,
                            RecoveryStage::Backfill,
                            RecoveryStage::Reconciliation,
                        ] {
                            recovery.restore(stage, session.clone()).await?;
                        }
                        self.heartbeat(&session).await?;
                        self.status.send_modify(|s| {
                            s.last_connected_at = Some(Utc::now());
                            s.last_heartbeat_at = s.last_connected_at;
                        });
                        self.phase(Phase::Connected);
                        tracing::info!(
                            recovery_ms = outage.elapsed().as_millis() as u64,
                            "IB recovery completed"
                        );
                        attempts = 0;
                        loop {
                            sleep(Duration::from_millis(self.config.heartbeat_ms)).await;
                            self.heartbeat(&session).await?;
                            recovery.check().await?;
                            self.status
                                .send_modify(|s| s.last_heartbeat_at = Some(Utc::now()));
                        }
                    };
                    let result = tokio::select! {
                        biased;
                        e=session.disconnected()=>Err(e),
                        r=serving=>r,
                    };
                    self.phase(Phase::Degraded);
                    outage = Instant::now();
                    self.status.send_modify(|s| s.disconnects += 1);
                    recovery.offline().await;
                    session.close().await;
                    match result {
                        Err(e) => e,
                        Ok(()) => unreachable!(),
                    }
                }
            };
            let kind = match &failure {
                Error::Timeout(_) => "timeout",
                Error::Recovery(_) => "recovery",
                _ => "transport",
            };
            self.status
                .send_modify(|s| s.last_error = Some(failure.to_string()));
            let wait = self
                .config
                .backoff(attempts, rand::random_range(0..=self.config.jitter_ms));
            self.phase(Phase::Reconnecting);
            tracing::warn!(error=%failure,class=kind,retry_ms=wait.as_millis() as u64,"IB connection unavailable");
            sleep(wait).await;
            attempts = attempts.saturating_add(1);
        }
    }
}

#[cfg(test)]
mod connection_replay_tests;
