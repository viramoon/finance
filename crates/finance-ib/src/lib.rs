pub mod config;
pub mod entitlements;
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
}
impl PaperPeer {
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
#[async_trait]
impl Session for PaperPeer {
    async fn accounts(&self) -> Result<Vec<String>> {
        self.client
            .managed_accounts()
            .await
            .map_err(|e| Error::Gateway(e.to_string()))
    }
    async fn server_time(&self) -> Result<i64> {
        self.client
            .server_time()
            .await
            .map(|d| d.unix_timestamp())
            .map_err(|e| Error::Gateway(e.to_string()))
    }
    async fn disconnected(&self) -> Error {
        let mut notices = self.notices.lock().await;
        while let Some(notice) = notices.next().await {
            if invalidates_market_session(notice.code) {
                return Error::Gateway(format!("IB connectivity notice {}", notice.code));
            }
        }
        Error::Gateway("IB notice stream ended".into())
    }
    async fn close(&self) {
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
        Client::builder().auto_reconnect(false)
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
            .map_err(|e| Error::Gateway(e.to_string()))?;
        Ok(PaperPeer {
            client,
            notices: tokio::sync::Mutex::new(notices),
        })
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
        // The SDK would otherwise record raw handshake and market frames.
        if std::env::var_os("IBAPI_RECORDING_DIR").is_some_and(|path| !path.is_empty()) {
            return Err(market::MarketError::InvalidConfiguration);
        }
        let (peer, _) = self
            .open(self.config.probe_client_id)
            .await
            .map_err(|e| match e {
                Error::UnsafeAccount => market::MarketError::UnsafeAccount,
                Error::Config(_) => market::MarketError::InvalidConfiguration,
                Error::Timeout(_) => market::MarketError::Timeout,
                _ => market::MarketError::Transport,
            })?;
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
        let bound = Duration::from_millis(self.config.connect_timeout_ms);
        let deadline = Instant::now() + bound;
        let session = Arc::new(
            timeout(bound, self.transport.connect(&self.config.address, id))
                .await
                .map_err(|_| Error::Timeout("connect"))??,
        );
        let accounts = tokio::time::timeout_at(deadline, session.accounts())
            .await
            .map_err(|_| Error::Timeout("managed_accounts"))
            .and_then(|a| a);
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
        timeout(
            Duration::from_millis(self.config.connect_timeout_ms),
            async {
                let (session, accounts) = self.open(self.config.probe_client_id).await?;
                let time = self.heartbeat(&session).await;
                session.close().await;
                Ok((time?, accounts))
            },
        )
        .await
        .map_err(|_| Error::Timeout("paper probe"))?
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
