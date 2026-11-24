use crate::source::{SourceStatus, Watermarks};
use axum::{Json, extract::State, http::StatusCode};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::{
    fs::File,
    io::{BufWriter, Write},
    path::Path,
    sync::{Arc, RwLock},
};

#[derive(Clone)]
pub struct HealthState {
    pub instruments: Arc<[String]>,
    pub max_age_seconds: u32,
    pub live: Arc<RwLock<Progress>>,
}
#[derive(Clone)]
pub struct Progress {
    pub source: SourceStatus,
    pub marks: Watermarks,
    pub phase: &'static str,
    pub writable: bool,
    pub buffered: usize,
}
#[derive(Serialize)]
pub struct Instrument {
    instrument: String,
    last_bar_at: Option<DateTime<Utc>>,
    age_seconds: Option<i64>,
    stale: bool,
}
#[derive(Serialize)]
pub struct Check {
    name: String,
    ok: bool,
}
#[derive(Serialize)]
pub struct Report {
    pub state: String,
    pub source: String,
    pub source_detail: String,
    pub updated_at: DateTime<Utc>,
    pub max_age_seconds: u32,
    pub instruments: Vec<Instrument>,
    pub checks: Vec<Check>,
    pub buffered: usize,
}
impl HealthState {
    pub fn report(&self) -> Report {
        let now = Utc::now();
        let p = self.live.read().expect("progress lock");
        let instruments: Vec<_> = self
            .instruments
            .iter()
            .map(|instrument| {
                let last = p
                    .marks
                    .get(instrument)
                    .map(|ns| DateTime::<Utc>::from_timestamp_nanos(*ns));
                let age = last.map(|t| (now - t).num_seconds());
                Instrument {
                    instrument: instrument.clone(),
                    last_bar_at: last,
                    age_seconds: age,
                    stale: age.is_none_or(|a| !(0..i64::from(self.max_age_seconds)).contains(&a)),
                }
            })
            .collect();
        let mut checks = vec![
            Check {
                name: "source_connected".into(),
                ok: p.source.connected,
            },
            Check {
                name: "backfill_complete".into(),
                ok: p.phase == "live",
            },
            Check {
                name: "catalog_writable".into(),
                ok: p.writable,
            },
        ];
        checks.extend(instruments.iter().map(|i| Check {
            name: format!("fresh:{}", i.instrument),
            ok: !i.stale,
        }));
        let state = if p.phase != "live" {
            p.phase
        } else if checks.iter().all(|c| c.ok) {
            "ready"
        } else {
            "degraded"
        };
        Report {
            state: state.to_string(),
            source: p.source.mode.clone(),
            source_detail: p.source.detail.clone(),
            updated_at: now,
            max_age_seconds: self.max_age_seconds,
            instruments,
            checks,
            buffered: p.buffered,
        }
    }
}
pub fn atomic_json(path: &Path, value: &impl Serialize) -> anyhow::Result<()> {
    let dir = path.parent().expect("output has parent");
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    {
        let mut writer = BufWriter::new(file.as_file_mut());
        serde_json::to_writer(&mut writer, value)?;
        writer.flush()?;
    }
    file.as_file().sync_all()?;
    file.persist(path)?;
    File::open(dir)?.sync_all()?;
    Ok(())
}
pub async fn ready(State(health): State<HealthState>) -> (StatusCode, Json<Report>) {
    let report = health.report();
    (
        if report.state == "ready" {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        },
        Json(report),
    )
}
pub async fn status(State(health): State<HealthState>) -> Json<Report> {
    Json(health.report())
}
