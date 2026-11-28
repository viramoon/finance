//! Shared wire format for the option-flow WebSocket and SSE transports.

use crate::market_options::flow::OptionFlowObservation;
use axum::response::sse::Event as AxumSseEvent;
use chrono::{DateTime, Utc};
use finance_collector::{
    line_budget::{BudgetStatus, CoverageInterval, CoverageReport, SchedulerPhase},
    market_ingress::ProductPolicy,
    option_flow::{
        ObservedFlow,
        observation::{
            BaselineCohort, BaselineVolumeRate, FlowObservation, L1Sample,
            OBSERVATION_RULE_VERSION, ObservationEvidence, ObservationStatus, VolumeComparison,
            VolumeIncrease,
        },
    },
};
use finance_ib::market::{Product, RequestKey};
use finance_market_protocol::{U64Text, schema_export::json_schema};
use serde::Serialize;
use serde_json::{Value, json};
use std::error::Error;
use utoipa::ToSchema;

pub(crate) const SNAPSHOT_PATH: &str = "/api/v1/options/flow/snapshot";

#[derive(Serialize, ToSchema)]
#[serde(tag = "event", rename_all = "snake_case")]
pub(crate) enum OptionFlowMessage {
    Observation {
        /// Opaque, contract-scoped resume cursor. Preserve it verbatim.
        #[schema(min_length = 1)]
        id: String,
        data: Box<OptionFlowObservationData>,
    },
    Status {
        /// Position at which live delivery starts or resumes.
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schema(value_type = String, min_length = 1)]
        id: Option<String>,
        data: OptionFlowStatusData,
    },
    ResetRequired {
        data: OptionFlowResetData,
    },
}

#[derive(Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct OptionFlowObservationData {
    schema_version: OptionFlowVersion,
    seq: U64Text,
    access: ProductPolicy,
    observation: OptionFlowObservation,
}

#[derive(Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct OptionFlowStatusData {
    schema_version: OptionFlowVersion,
    #[schema(minimum = 1)]
    con_id: i32,
    state: OptionFlowState,
}

#[derive(Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct OptionFlowResetData {
    schema_version: OptionFlowVersion,
    #[schema(minimum = 1)]
    con_id: i32,
    reason: OptionFlowResetReason,
    snapshot: &'static str,
}

#[derive(Serialize, ToSchema)]
enum OptionFlowVersion {
    #[serde(rename = "1.0")]
    V1,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
enum OptionFlowState {
    Live,
    Disabled,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
enum OptionFlowResetReason {
    CursorUnavailable,
}

impl OptionFlowMessage {
    pub(crate) fn observation(cursor: String, seq: u64, record: &ObservedFlow) -> Self {
        Self::Observation {
            id: cursor,
            data: Box::new(OptionFlowObservationData {
                schema_version: OptionFlowVersion::V1,
                seq: seq.into(),
                access: record.access.clone(),
                observation: OptionFlowObservation::from_record(record),
            }),
        }
    }

    pub(crate) fn status(con_id: i32, cursor: Option<String>) -> Self {
        let state = if cursor.is_some() {
            OptionFlowState::Live
        } else {
            OptionFlowState::Disabled
        };
        Self::Status {
            id: cursor,
            data: OptionFlowStatusData {
                schema_version: OptionFlowVersion::V1,
                con_id,
                state,
            },
        }
    }

    pub(crate) fn reset_required(con_id: i32) -> Self {
        Self::ResetRequired {
            data: OptionFlowResetData {
                schema_version: OptionFlowVersion::V1,
                con_id,
                reason: OptionFlowResetReason::CursorUnavailable,
                snapshot: SNAPSHOT_PATH,
            },
        }
    }

    pub(crate) fn to_sse_event(&self) -> Result<AxumSseEvent, serde_json::Error> {
        let (event, id, data) = match self {
            Self::Observation { id, data } => (
                "observation",
                Some(id.as_str()),
                serde_json::to_string(data)?,
            ),
            Self::Status { id, data } => ("status", id.as_deref(), serde_json::to_string(data)?),
            Self::ResetRequired { data } => ("reset_required", None, serde_json::to_string(data)?),
        };
        let encoded = AxumSseEvent::default().event(event).data(data);
        Ok(match id {
            Some(id) => encoded.id(id),
            None => encoded,
        })
    }
}

pub(crate) fn documents() -> Result<[(&'static str, Value); 3], Box<dyn Error>> {
    let mut ws =
        json_schema::<OptionFlowMessage>("urn:viramoon:finance:read-stream:1.0:option-flow-ws")?;
    // Utoipa leaves internally tagged variants open.
    ws["unevaluatedProperties"] = false.into();
    let status = ws["oneOf"]
        .as_array_mut()
        .and_then(|variants| {
            variants
                .iter_mut()
                .find(|variant| variant["properties"]["event"]["enum"][0] == "status")
        })
        .ok_or("missing option-flow status schema")?;
    status["if"] = json!({"properties": {"data": {"properties": {"state": {"const": "live"}}}}});
    status["then"] = json!({"required": ["id"]});
    status["else"] = json!({"not": {"required": ["id"]}});
    let snapshot = ws
        .pointer_mut("/$defs/OptionFlowResetData/properties/snapshot")
        .and_then(Value::as_object_mut)
        .ok_or("missing option-flow reset snapshot schema")?;
    snapshot.insert("const".into(), SNAPSHOT_PATH.into());
    let mut sse = ws.clone();
    sse["$id"] = "urn:viramoon:finance:read-stream:1.0:option-flow-sse".into();
    sse["description"] = "The event, optional id and JSON data fields of one SSE dispatch.".into();
    Ok([
        ("option-flow-ws.schema.json", ws),
        ("option-flow-sse.schema.json", sse),
        ("option-flow-stream-examples.json", synthetic_examples()?),
    ])
}

fn synthetic_examples() -> Result<Value, serde_json::Error> {
    let con_id = SYNTHETIC_CON_ID;
    let cursor = |seq: u64| format!("00000000000000000000000000000000.{con_id}.{seq}");
    let first = OptionFlowMessage::observation(
        cursor(u64::MAX - 1),
        u64::MAX - 1,
        &synthetic_observation(),
    );
    let mut second_record = fixture(2);
    second_record.coverage = None;
    let second = OptionFlowMessage::observation(cursor(u64::MAX), u64::MAX, &second_record);
    let start = OptionFlowMessage::status(con_id, Some(cursor(u64::MAX - 2)));
    let resume = OptionFlowMessage::status(con_id, Some(cursor(u64::MAX - 1)));
    let disabled = OptionFlowMessage::status(con_id, None);
    let reset = OptionFlowMessage::reset_required(con_id);
    serde_json::to_value(std::collections::BTreeMap::from([
        ("live", vec![&start, &first, &second]),
        ("replayed", vec![&resume, &second]),
        ("out_of_order", vec![&second, &first, &second]),
        ("disabled", vec![&disabled]),
        ("expired", vec![&reset]),
    ]))
}

const SYNTHETIC_CON_ID: i32 = 42;

pub(super) fn synthetic_observation() -> ObservedFlow {
    fixture(3)
}

// Invented IDs, fixed UTC time offsets, a virtual policy and hand-written decimals.
fn fixture(receipt_offset: i64) -> ObservedFlow {
    let at = |offset| DateTime::<Utc>::from_timestamp(1_760_000_000 + offset, 0).unwrap();
    let observed_at = at(receipt_offset);
    let sample = |value: &str, received_at| L1Sample {
        value: value.parse().unwrap(),
        received_at,
    };
    ObservedFlow {
        access: ProductPolicy {
            source: "ibkr".parse().unwrap(),
            product: "opra".parse().unwrap(),
            license: "fixture.synthetic".parse().unwrap(),
            display_live: true,
            record_local: false,
            derive_non_display: false,
            backup_personal_drive: false,
        },
        clue: FlowObservation {
            rule_version: OBSERVATION_RULE_VERSION,
            status: ObservationStatus::PendingVerification,
            con_id: SYNTHETIC_CON_ID,
            generation: u64::MAX.into(),
            cohort: BaselineCohort {
                underlying_con_id: 7,
                expiry: observed_at.date_naive(),
                session_segment: 3,
            },
            multiplier: "25".parse().unwrap(),
            observed_at,
            observation: ObservationEvidence::VolumeIncrease(Box::new(VolumeIncrease {
                earlier_cumulative_volume: sample("50", at(0)),
                later_cumulative_volume: sample("9007199254741043", observed_at),
                delta_contracts: "9007199254740993".parse().unwrap(),
                comparison: VolumeComparison {
                    elapsed_millis: u32::try_from(receipt_offset * 1_000).unwrap(),
                    baseline_samples: 3,
                    typical_rate: Some(BaselineVolumeRate {
                        contracts: 9007199254740993,
                        elapsed_millis: 6_000,
                    }),
                    elevated: Some(true),
                },
                sampled_last_price: Some(sample("1.000000000000000001", observed_at)),
                sampled_last_size: Some(sample("2.1250", observed_at)),
                estimated_premium: None,
            })),
        },
        coverage: Some(CoverageReport {
            key: RequestKey {
                con_id: SYNTHETIC_CON_ID,
                product: Product::Opra,
                parameters: Default::default(),
            },
            last_observed_at: Some(observed_at),
            covered: vec![],
            uncovered: vec![CoverageInterval {
                start: at(0),
                end: observed_at,
            }],
            unknown: vec![],
        }),
        budget: BudgetStatus {
            phase: SchedulerPhase::Online,
            as_of: observed_at,
            account_lines: 100,
            reserve_lines: 30,
            stock_lines: 9,
            hot_lines: 1,
            rotation_lines: 1,
            confirmed_options: 2,
            observed_options: 1,
            discovery_complete: false,
        },
    }
}
