use anyhow::{Context, Result, ensure};
use axum::{Json, Router, routing::get};
use finance_collector::{
    config::Settings,
    health::{HealthState, Progress, atomic_json, ready, status},
    source::{Bar, BarSource, ReplaySource, SourceStatus, now_ns},
    store::Store,
};
use std::{
    env, fs,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::{
    sync::watch,
    time::{MissedTickBehavior, interval, timeout},
};

fn replay_origin(state: &Path) -> Result<i64> {
    let path = state.join("replay-origin.json");
    match fs::read(&path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let start = now_ns();
            atomic_json(&path, &start)?;
            Ok(start)
        }
        Err(e) => Err(e.into()),
    }
}
fn make_source(cfg: &Settings, state: &Path) -> Result<(Box<dyn BarSource>, i64)> {
    ensure!(
        cfg.source == "replay",
        "unsupported collector source '{}'",
        cfg.source
    );
    let origin = replay_origin(state)?;
    Ok((
        Box::new(ReplaySource::new(
            &cfg.replay.fixture,
            origin,
            cfg.replay.interval_ms,
            cfg.replay.limit,
        )?),
        origin - 1,
    ))
}
async fn flush(store: &mut Store, bars: &mut Vec<Bar>, health: &HealthState) -> Result<()> {
    store.flush(bars).await?;
    let mut p = health.live.write().expect("progress lock");
    p.marks = store.marks.clone();
    p.buffered = 0;
    Ok(())
}
struct Collector<'a> {
    cfg: &'a Settings,
    health: HealthState,
    store: Store,
    source: Box<dyn BarSource>,
    initial_from: i64,
}
impl Collector<'_> {
    async fn session(&mut self, stop: &mut watch::Receiver<bool>) -> Result<()> {
        tokio::select! {
            _ = stop.changed() => return Ok(()),
            result = self.source.subscribe(&self.cfg.instruments) => result?,
        }
        self.health.live.write().expect("progress lock").source = self.source.status();
        // Subscribe BEFORE choosing cutoff so the streaming source queues simultaneous arrivals.
        let cutoff = now_ns();
        for instrument in &self.cfg.instruments {
            let mut from = self
                .store
                .marks
                .get(instrument)
                .copied()
                .unwrap_or(self.initial_from);
            while from < cutoff {
                if *stop.borrow() {
                    return Ok(());
                }
                let to = cutoff.min(
                    from.saturating_add(i64::from(self.cfg.history_chunk_seconds) * 1_000_000_000),
                );
                let mut bars = tokio::select! {
                    _=stop.changed()=>return Ok(()),
                    result=self.source.backfill(instrument,from,to)=>result?,
                };
                ensure!(
                    bars.iter()
                        .all(|b| b.instrument == *instrument && b.ts_ns > from && b.ts_ns <= to),
                    "source returned bars outside requested range"
                );
                flush(&mut self.store, &mut bars, &self.health).await?;
                from = to;
            }
        }
        self.health.live.write().expect("progress lock").phase = "live";
        let mut timer = interval(Duration::from_millis(self.cfg.flush_ms.into()));
        timer.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut bars = Vec::new();
        let mut exhausted = false;
        loop {
            if *stop.borrow() {
                break;
            }
            tokio::select! {
                _=stop.changed()=>break,
                _=timer.tick()=>flush(&mut self.store,&mut bars,&self.health).await?,
                result=self.source.next_bar(),if !exhausted=>{
                    match result {
                        Ok(Some(bar))=>{
                            bars.push(bar);
                            self.health.live.write().expect("progress lock").buffered=bars.len();
                            if bars.len()>=4096 {flush(&mut self.store,&mut bars,&self.health).await?;}
                        }
                        Ok(None)=>{exhausted=true;flush(&mut self.store,&mut bars,&self.health).await?;}
                        Err(e)=>{
                            self.health.live.write().expect("progress lock").phase="recovering";
                            flush(&mut self.store,&mut bars,&self.health).await?;
                            return Err(e);
                        }
                    }
                }
            }
        }
        flush(&mut self.store, &mut bars, &self.health).await
    }
    async fn run(mut self, mut stop: watch::Receiver<bool>) -> Result<()> {
        while !*stop.borrow() {
            self.health.live.write().expect("progress lock").phase = "recovering";
            if let Err(e) = self.session(&mut stop).await {
                eprintln!("collector recovery: {e:#}");
                self.store.check_process()?;
                let mut progress = self.health.live.write().expect("progress lock");
                progress.source.connected = false;
                progress.phase = "recovering";
            }
            if !*stop.borrow() {
                tokio::select! {_=stop.changed()=>{},_=tokio::time::sleep(Duration::from_secs(1))=>{}}
            }
        }
        self.store.close().await
    }
}
#[tokio::main]
async fn main() -> Result<()> {
    let cfg = Settings::load()?;
    let data = env::var_os("FERTILE_DATA_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| "/home/box/fertile/data".into());
    ensure!(data.is_absolute(), "data root must be absolute");
    ensure!(
        fs::metadata(&data).is_ok_and(|m| m.is_dir()),
        "data root must be an existing mounted directory: {}",
        data.display()
    );
    let state = data.join("finance-state/collector");
    let catalog = cfg.catalog(&data);
    fs::create_dir_all(&state)?;
    fs::create_dir_all(&catalog)?;
    let store = Store::start(&catalog, &state, &cfg.bar_spec, &cfg.source)
        .await
        .context("open collector store")?;
    let (source, initial_from) = make_source(&cfg, &state)?;
    let health = HealthState {
        instruments: cfg.instruments.clone().into(),
        max_age_seconds: cfg.max_age_seconds,
        live: Arc::new(RwLock::new(Progress {
            source: SourceStatus {
                connected: false,
                mode: cfg.source.clone(),
                detail: "starting".into(),
            },
            phase: "starting",
            marks: store.marks.clone(),
            writable: false,
            buffered: 0,
        })),
    };
    let app = Router::new()
        .route(
            "/healthz/live",
            get(|| async { Json(serde_json::json!({"status":"ok"})) }),
        )
        .route("/healthz/ready", get(ready))
        .route("/status", get(status))
        .with_state(health.clone());
    let listener = tokio::net::TcpListener::bind(cfg.bind).await?;
    let (shutdown, stop) = watch::channel(false);
    let mut listener_stop = stop.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = listener_stop.changed().await;
            })
            .await
    });
    let mut heartbeat_stop = stop.clone();
    let heartbeat_state = health.clone();
    let status_file = state.join("status.json");
    let heartbeat_path = status_file.clone();
    let heartbeat = tokio::spawn(async move {
        let mut timer = interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                _=heartbeat_stop.changed()=>break,
                _=timer.tick()=>{
                    let writable=tempfile::NamedTempFile::new_in(&catalog).is_ok();
                    heartbeat_state.live.write().expect("progress lock").writable=writable;
                    atomic_json(&heartbeat_path,&heartbeat_state.report())?;
                }
            }
        }
        Ok::<_, anyhow::Error>(())
    });
    let collector = Collector {
        cfg: &cfg,
        health: health.clone(),
        store,
        source,
        initial_from,
    };
    let mut task = Box::pin(collector.run(stop));
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut heartbeat = heartbeat;
    tokio::select! {
        _=term.recv()=>{},
        _=tokio::signal::ctrl_c()=>{},
        result=&mut task=>return result,
        result=&mut heartbeat=>{result??;anyhow::bail!("collector heartbeat stopped");}
    }
    shutdown.send(true)?;
    timeout(Duration::from_secs(cfg.shutdown_seconds.into()), task)
        .await
        .context("collector shutdown timed out")??;
    health.live.write().expect("progress lock").phase = "stopped";
    heartbeat.await??;
    atomic_json(&status_file, &health.report())?;
    server.await??;
    Ok(())
}
