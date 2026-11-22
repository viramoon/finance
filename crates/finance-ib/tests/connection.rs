use async_trait::async_trait;
use finance_ib::{
    ConnectionManager, Error, IbConfig, Phase, Recovery, RecoveryStage, Result, Session, Transport,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::sync::Notify;

fn config() -> IbConfig {
    IbConfig {
        address: "127.0.0.1:4002".into(),
        collector_client_id: 92,
        probe_client_id: 91,
        trader_client_id: 93,
        account_prefix: "DU".into(),
        connect_timeout_ms: 100,
        heartbeat_ms: 20,
        read_timeout_ms: 10,
        backoff_initial_ms: 5,
        backoff_max_ms: 20,
        jitter_ms: 0,
    }
}
struct Mock {
    account: String,
    close: Arc<AtomicUsize>,
    attempts: AtomicUsize,
    silent: bool,
}
struct Socket {
    account: String,
    close: Arc<AtomicUsize>,
    count: AtomicUsize,
    fail: bool,
    silent: bool,
}
#[async_trait]
impl Transport for Mock {
    type S = Socket;
    async fn connect(&self, _: &str, _: i32) -> Result<Socket> {
        Ok(Socket {
            account: self.account.clone(),
            close: self.close.clone(),
            count: AtomicUsize::new(0),
            fail: self.attempts.fetch_add(1, Ordering::SeqCst) == 0,
            silent: self.silent,
        })
    }
}
#[async_trait]
impl Session for Socket {
    async fn accounts(&self) -> Result<Vec<String>> {
        Ok(vec![self.account.clone()])
    }
    async fn server_time(&self) -> Result<i64> {
        if self.fail && self.count.fetch_add(1, Ordering::SeqCst) > 0 {
            if self.silent {
                std::future::pending::<()>().await;
            }
            return Err(Error::Gateway("peer disconnected".into()));
        }
        Ok(1000)
    }
    async fn disconnected(&self) -> Error {
        if self.fail && self.account == "DU-FARM" {
            tokio::time::sleep(Duration::from_millis(10)).await;
            return Error::Gateway("IB connectivity notice 1100".into());
        }
        std::future::pending().await
    }
    async fn close(&self) {
        self.close.fetch_add(1, Ordering::SeqCst);
    }
}
struct Hooks {
    stages: Arc<Mutex<Vec<RecoveryStage>>>,
    done: Arc<Notify>,
}
#[async_trait]
impl Recovery<Socket> for Hooks {
    async fn restore(&mut self, stage: RecoveryStage, _: Arc<Socket>) -> Result<()> {
        let mut events = self.stages.lock().unwrap();
        events.push(stage);
        if events.len() == 6 {
            self.done.notify_one();
        }
        Ok(())
    }
}
#[tokio::test(start_paused = true)]
async fn disconnect_and_silent_heartbeat_both_recover_in_order() {
    for silent in [false, true] {
        let closed = Arc::new(AtomicUsize::new(0));
        let manager = ConnectionManager::new(
            config(),
            Mock {
                account: "DU123".into(),
                attempts: 0.into(),
                close: closed.clone(),
                silent,
            },
        )
        .unwrap();
        let status = manager.subscribe();
        let stages = Arc::new(Mutex::new(vec![]));
        let done = Arc::new(Notify::new());
        let mut hooks = Hooks {
            stages: stages.clone(),
            done: done.clone(),
        };
        let job = tokio::spawn(async move { manager.run(&mut hooks).await });
        tokio::time::timeout(Duration::from_secs(1), done.notified())
            .await
            .unwrap();
        assert_eq!(
            *stages.lock().unwrap(),
            [
                RecoveryStage::Subscriptions,
                RecoveryStage::Backfill,
                RecoveryStage::Reconciliation
            ]
            .repeat(2)
        );
        let state = status.borrow().clone();
        assert_eq!(state.state, Phase::Connected);
        assert_eq!(state.disconnects, 1);
        assert!(state.last_error.unwrap().contains(if silent {
            "timed out"
        } else {
            "disconnected"
        }));
        assert!(state.last_heartbeat_at.is_some());
        assert_eq!(closed.load(Ordering::SeqCst), 1);
        job.abort();
    }
}
#[tokio::test]
async fn refuses_live_accounts_before_callbacks() {
    let closes = Arc::new(AtomicUsize::new(0));
    let manager = ConnectionManager::new(
        config(),
        Mock {
            account: "U123".into(),
            attempts: 0.into(),
            close: closes.clone(),
            silent: false,
        },
    )
    .unwrap();
    assert!(matches!(manager.probe().await, Err(Error::UnsafeAccount)));
    assert_eq!(closes.load(Ordering::SeqCst), 1);
}
#[test]
fn bounded_backoff_and_configuration() {
    let mut c = config();
    c.jitter_ms = 2;
    assert_eq!(c.backoff(0, 1), Duration::from_millis(6));
    assert_eq!(c.backoff(1, 99), Duration::from_millis(12));
    assert_eq!(c.backoff(100, 1), Duration::from_millis(20));
    c.address = "127.0.0.1:4001".into();
    assert!(c.validate().is_err());
    c = config();
    c.probe_client_id = c.collector_client_id;
    assert!(c.validate().is_err());
    c = config();
    c.read_timeout_ms = 0;
    assert!(c.validate().is_err());
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config");
    let cloud = IbConfig::from_layers(&dir, "cloudbox", |_| None).unwrap();
    let stable = IbConfig::from_layers(&dir, "stable", |_| None).unwrap();
    assert!(cloud.heartbeat_ms < stable.heartbeat_ms);
    let port = IbConfig::from_layers(&dir, "stable", |n| {
        (n == "IB_GATEWAY_ADDRESS").then(|| "127.0.0.1:4001".into())
    });
    assert!(port.is_err());
}

#[tokio::test(start_paused = true)]
async fn farm_notice_restores_subscriptions_even_with_a_live_socket() {
    let manager = ConnectionManager::new(
        config(),
        Mock {
            account: "DU-FARM".into(),
            attempts: 0.into(),
            close: Arc::new(0.into()),
            silent: true,
        },
    )
    .unwrap();
    let status = manager.subscribe();
    let done = Arc::new(Notify::new());
    let mut hooks = Hooks {
        stages: Default::default(),
        done: done.clone(),
    };
    let task = tokio::spawn(async move { manager.run(&mut hooks).await });
    tokio::time::timeout(Duration::from_secs(1), done.notified())
        .await
        .unwrap();
    assert!(
        status
            .borrow()
            .last_error
            .as_deref()
            .unwrap()
            .contains("1100")
    );
    assert!(finance_ib::invalidates_market_session(1101));
    assert!(!finance_ib::invalidates_market_session(2104));
    task.abort();
}
