//! Build standalone JSON Schemas from the protocol's Rust wire types.

use crate::{Event, SchemaVersion};
use serde_json::{Map, Value, json};
use std::error::Error;
use utoipa::ToSchema;

/// Context observations reserved for the v2.0 event envelope.
pub const V2_CONTEXT_KINDS: [&str; 3] = ["perpetual_context", "funding_rate", "open_interest"];
/// Historical option observations reserved for the v3.0 event envelope.
pub const V3_OPTION_HISTORY_KINDS: [&str; 2] = ["option_history_trade", "option_history_quote"];
/// Controls shared by the v1.0 and v2.0 event envelopes.
pub const CONTROL_KINDS: [&str; 4] = ["gap", "reset", "heartbeat", "status"];

fn use_local_refs(value: &mut Value) {
    match value {
        Value::Object(object) => {
            if let Some(Value::String(reference)) = object.get_mut("$ref")
                && let Some(name) = reference.strip_prefix("#/components/schemas/")
            {
                *reference = format!("#/$defs/{name}");
            }
            for item in object.values_mut() {
                use_local_refs(item);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(use_local_refs),
        _ => {}
    }
}

pub fn json_schema<T: ToSchema>(id: &str) -> Result<Value, Box<dyn Error>> {
    let mut root = serde_json::to_value(T::schema())?;
    let object = root
        .as_object_mut()
        .ok_or("schema root must be an object")?;
    let mut components = Vec::new();
    T::schemas(&mut components);
    let mut definitions = Map::new();
    for (name, component) in components {
        let component = serde_json::to_value(component)?;
        if let Some(previous) = definitions.insert(name.clone(), component.clone())
            && previous != component
        {
            return Err(format!("conflicting schema definition for {name}").into());
        }
    }
    object.insert(
        "$schema".into(),
        "https://json-schema.org/draft/2020-12/schema".into(),
    );
    object.insert("$id".into(), id.into());
    if !definitions.is_empty() {
        object.insert("$defs".into(), Value::Object(definitions));
    }
    use_local_refs(&mut root);
    Ok(root)
}

pub fn versioned_json_schema<T: ToSchema>(
    id: &str,
    version: SchemaVersion,
) -> Result<Value, Box<dyn Error>> {
    let mut root = json_schema::<T>(id)?;
    *root
        .pointer_mut("/$defs/SchemaVersion/enum")
        .ok_or("schema is missing SchemaVersion")? = json!([version]);
    Ok(root)
}

/// Export the v1.0 event format used by the first read stream contract.
pub fn event_schema(id: &str) -> Result<Value, Box<dyn Error>> {
    event_schema_for_version(id, SchemaVersion::V1)
}

pub fn event_schema_for_version(id: &str, version: SchemaVersion) -> Result<Value, Box<dyn Error>> {
    let mut root = versioned_json_schema::<Event>(id, version)?;
    close_event_envelopes(&mut root, "", "/$defs/MarketMessage")?;
    root.pointer_mut("/$defs/Message/oneOf")
        .and_then(Value::as_array_mut)
        .ok_or("event schema is missing the message union")?
        .retain(|branch| {
            let kind = branch["properties"]["kind"]["enum"][0]
                .as_str()
                .unwrap_or_default();
            match version {
                SchemaVersion::V1 => {
                    !V2_CONTEXT_KINDS.contains(&kind) && !V3_OPTION_HISTORY_KINDS.contains(&kind)
                }
                SchemaVersion::V2 => {
                    CONTROL_KINDS.contains(&kind) || V2_CONTEXT_KINDS.contains(&kind)
                }
                SchemaVersion::V3 => V3_OPTION_HISTORY_KINDS.contains(&kind),
            }
        });

    let defs = root["$defs"]
        .as_object_mut()
        .ok_or("missing event definitions")?;
    if version != SchemaVersion::V3 {
        for name in [
            "OptionHistoryBatch",
            "OptionHistoryQuote",
            "OptionHistoryTrade",
        ] {
            defs.remove(name);
        }
    }
    if version != SchemaVersion::V2 {
        for name in ["PerpetualContext", "FundingRate", "OpenInterest"] {
            defs.remove(name);
        }
    }
    if version == SchemaVersion::V3 {
        for name in ["ConnectionState", "Gap", "Heartbeat", "Reset", "Status"] {
            defs.remove(name);
        }
    }
    if version == SchemaVersion::V1 {
        defs["Message"]["description"] =
            "The version 1.0 kind tag and its corresponding payload.".into();
    } else {
        for name in [
            "AssetKind",
            "BookDelta",
            "BookLevel",
            "BookSide",
            "BookSnapshot",
            "BookUpdate",
            "ClosedBar",
            "Correction",
            "MarketMessage",
            "Trade",
            "TradeSide",
        ] {
            defs.remove(name);
        }
        let asset = if version == SchemaVersion::V2 {
            "perpetual"
        } else {
            "option"
        };
        root["allOf"][1]["properties"]["asset_kind"] = json!({"const": asset});
        root["allOf"][1]["properties"]["revision"] = json!({"const": "0"});
    }
    if version == SchemaVersion::V3 {
        root["allOf"][1]["properties"]["exchange_ts"] = json!({"$ref": "#/$defs/U64Text"});
    }
    Ok(root)
}

/// Close the flattened F1 envelope and its correction replacement union.
pub fn close_event_envelopes(
    document: &mut Value,
    event_pointer: &str,
    replacement_pointer: &str,
) -> Result<(), Box<dyn Error>> {
    // Serde flattens the kind/payload pair into the envelope. Closing an
    // individual allOf branch would reject fields owned by its sibling.
    for pointer in [event_pointer, replacement_pointer] {
        document
            .pointer_mut(pointer)
            .and_then(Value::as_object_mut)
            .ok_or_else(|| format!("missing F1 envelope schema at {pointer:?}"))?
            .insert("unevaluatedProperties".into(), false.into());
    }
    Ok(())
}
