//! Versioned market WebSocket and status SSE messages for finance consumers.
//!
//! A market WebSocket text message uses an unchanged F1 `Event`. The
//! status SSE envelope maps its three fields to the SSE event, id and data
//! lines. Delivery and resume retention are handled by the transport.

use crate::{
    market::{InstrumentStatus, MarketStatus},
    option_flow_stream::wire,
};
use axum::response::sse::Event as AxumSseEvent;
use chrono::DateTime;
use finance_market_protocol::{
    Event, U64Text,
    schema_export::{event_schema, json_schema},
};
use serde::{Deserialize, Serialize};
use std::{error::Error, fs, path::Path};
use utoipa::ToSchema;

#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
pub enum StatusSseMessage {
    MarketStatus {
        /// Event ID scoped to the delivery stream. Preserve it as text.
        id: U64Text,
        data: MarketStatusData,
    },
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct MarketStatusData {
    pub schema_version: StatusSseVersion,
    pub status: MarketStatus,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[schema(as = SchemaVersion)]
pub enum StatusSseVersion {
    #[serde(rename = "1.0")]
    V1,
}

impl StatusSseMessage {
    pub fn market_status(id: u64, status: MarketStatus) -> Self {
        Self::MarketStatus {
            id: id.into(),
            data: MarketStatusData {
                schema_version: StatusSseVersion::V1,
                status,
            },
        }
    }

    pub fn to_sse_event(&self) -> Result<AxumSseEvent, serde_json::Error> {
        match self {
            Self::MarketStatus { id, data } => Ok(AxumSseEvent::default()
                .event("market_status")
                .id(id.get().to_string())
                .data(serde_json::to_string(data)?)),
        }
    }
}

#[derive(Deserialize)]
struct MarketExamples {
    events: Vec<Event>,
}

#[derive(Serialize)]
struct ReadStreamExamples {
    market_ws: Vec<Event>,
    market_status_sse: Vec<StatusSseMessage>,
}

impl ReadStreamExamples {
    fn synthetic() -> Result<Self, serde_json::Error> {
        let market: MarketExamples = serde_json::from_slice(include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../protocol/v1/examples.json"
        )))?;
        let fixed_time = DateTime::from_timestamp(1_760_000_000, 0);
        let ready = MarketStatus {
            state: "ready".into(),
            source: Some("replay".into()),
            updated_at: fixed_time,
            max_age_seconds: Some(10),
            instruments: vec![InstrumentStatus {
                instrument: "SYNTH.XNYS".into(),
                last_bar_at: fixed_time,
                age_seconds: Some(0),
                stale: false,
            }],
        };
        let missing = MarketStatus {
            state: "missing".into(),
            source: None,
            updated_at: None,
            max_age_seconds: None,
            instruments: vec![],
        };
        Ok(Self {
            market_ws: market.events,
            market_status_sse: vec![
                StatusSseMessage::market_status(u64::MAX - 1, ready),
                StatusSseMessage::market_status(u64::MAX, missing),
            ],
        })
    }
}

/// Export schemas and fixed synthetic examples without opening runtime data.
pub fn export(directory: &Path) -> Result<(), Box<dyn Error>> {
    let mut status_schema =
        json_schema::<StatusSseMessage>("urn:viramoon:finance:read-stream:1.0:market-status-sse")?;
    // Utoipa's tagged enum retains an open variant object even when the
    // deserializer denies unknown envelope fields.
    status_schema["unevaluatedProperties"] = false.into();
    let documents = [
        (
            "market-ws.schema.json",
            event_schema("urn:viramoon:finance:read-stream:1.0:market-ws")?,
        ),
        ("market-status-sse.schema.json", status_schema),
        (
            "read-stream-examples.json",
            serde_json::to_value(ReadStreamExamples::synthetic()?)?,
        ),
    ];
    fs::create_dir_all(directory)?;
    for (name, mut document) in documents.into_iter().chain(wire::documents()?) {
        // Keep export order independent of the workspace's serde_json features.
        document.sort_all_objects();
        let mut bytes = serde_json::to_vec_pretty(&document)?;
        bytes.push(b'\n');
        fs::write(directory.join(name), bytes)?;
    }
    Ok(())
}
