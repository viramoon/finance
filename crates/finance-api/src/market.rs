//! Reads the collector's last published heartbeat without contacting a market data source.
use crate::AppState;
use axum::{Json, extract::State};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct InstrumentStatus {
    pub instrument: String,
    pub last_bar_at: Option<DateTime<Utc>>,
    pub age_seconds: Option<i64>,
    pub stale: bool,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct MarketStatus {
    /// ready, starting, recovering, degraded, stopped, expired, missing, unreadable or invalid
    pub state: String,
    pub source: Option<String>,
    pub updated_at: Option<DateTime<Utc>>,
    pub max_age_seconds: Option<u32>,
    pub instruments: Vec<InstrumentStatus>,
}

impl MarketStatus {
    fn unavailable(state: &str) -> Self {
        Self {
            state: state.into(),
            source: None,
            updated_at: None,
            max_age_seconds: None,
            instruments: vec![],
        }
    }
    fn refresh(&mut self, now: DateTime<Utc>) {
        if self
            .updated_at
            .is_none_or(|t| !(0..=10).contains(&(now - t).num_seconds()))
        {
            self.state = "expired".into();
        }
        for item in &mut self.instruments {
            item.age_seconds = item.last_bar_at.map(|t| (now - t).num_seconds());
            item.stale = item
                .age_seconds
                .zip(self.max_age_seconds)
                .is_none_or(|(age, limit)| !(0..i64::from(limit)).contains(&age));
            if item.stale && self.state == "ready" {
                self.state = "degraded".into();
            }
        }
    }
}
#[utoipa::path(get,path="/api/v1/market/status",responses((status = 200, description="Latest market state including unavailable / expired heartbeat",body=MarketStatus)))]
pub async fn status(State(app): State<AppState>) -> Json<MarketStatus> {
    let path = app.data_root.join("finance-state/collector/status.json");
    let mut output = match tokio::fs::read(path).await {
        Ok(bytes) => match serde_json::from_slice::<MarketStatus>(&bytes) {
            Ok(report) => report,
            Err(_) => return Json(MarketStatus::unavailable("invalid")),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Json(MarketStatus::unavailable("missing"));
        }
        Err(_) => return Json(MarketStatus::unavailable("unreadable")),
    };
    output.refresh(Utc::now());
    Json(output)
}
