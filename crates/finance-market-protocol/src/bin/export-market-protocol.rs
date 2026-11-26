use finance_market_protocol::{Capability, Event, Instrument};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{collections::HashMap, env, error::Error, fs, io, path::PathBuf};
use utoipa::ToSchema;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Examples {
    instruments: Vec<Instrument>,
    capabilities: Vec<Capability>,
    events: Vec<Event>,
}

impl Examples {
    fn validated(input: &str) -> Result<Self, Box<dyn Error>> {
        let examples: Self = serde_json::from_str(input)?;
        let mut instruments = HashMap::new();
        for instrument in &examples.instruments {
            if instruments
                .insert(instrument.instrument_id.as_str(), instrument)
                .is_some()
            {
                return Err(format!(
                    "duplicate example instrument {}",
                    instrument.instrument_id.as_str()
                )
                .into());
            }
        }
        let mut capabilities = HashMap::new();
        for capability in &examples.capabilities {
            let key = (capability.source.as_str(), capability.product.as_str());
            if capabilities.insert(key, capability).is_some() {
                return Err(format!("duplicate example capability {key:?}").into());
            }
        }
        let mut latest = HashMap::new();
        for event in &examples.events {
            let source = event.source.as_str();
            let product = event.product.as_str();
            let instrument_id = event.instrument_id.as_str();
            let instrument = instruments
                .get(instrument_id)
                .ok_or_else(|| format!("example references unknown instrument {instrument_id}"))?;
            let capability = capabilities.get(&(source, product)).ok_or_else(|| {
                format!("example references unknown source/product {source}/{product}")
            })?;
            event.validate_against(instrument, capability)?;
            if let Some(previous) = latest.insert(event.stream_key(), event) {
                event.validate_successor(previous)?;
            }
        }
        Ok(examples)
    }
}

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

fn schema<T: ToSchema>(id: &str, version: &str) -> Result<Value, Box<dyn Error>> {
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
    object.insert(
        "$id".into(),
        format!("urn:viramoon:finance:market-protocol:{version}:{id}").into(),
    );
    if !definitions.is_empty() {
        object.insert("$defs".into(), Value::Object(definitions));
    }
    if id == "event" {
        // Serde flattens the kind/payload pair into the envelope. Closing an
        // individual allOf branch would reject fields owned by its sibling.
        object.insert("unevaluatedProperties".into(), false.into());
        root.pointer_mut("/$defs/MarketMessage")
            .and_then(Value::as_object_mut)
            .ok_or("event schema is missing the correction replacement union")?
            .insert("unevaluatedProperties".into(), false.into());
    }
    if id == "capability" {
        // Preserve the wire list order while enforcing the runtime uniqueness
        // check as a structural rule on the array.
        root.pointer_mut("/properties/message_kinds")
            .and_then(Value::as_object_mut)
            .ok_or("capability schema is missing message_kinds")?
            .insert("uniqueItems".into(), true.into());
        // Utoipa emits the signed int32 format for u32. The numeric range
        // already captures the unsigned depth type without the format hint.
        root.pointer_mut("/properties/max_depth")
            .and_then(Value::as_object_mut)
            .ok_or("capability schema is missing max_depth")?
            .remove("format");
    }
    use_local_refs(&mut root);
    root["$defs"]["SchemaVersion"]["enum"] = serde_json::json!([version]);
    if id == "event" {
        root.pointer_mut("/$defs/Message/oneOf")
            .and_then(Value::as_array_mut)
            .ok_or("event schema is missing the message union")?
            .retain(|branch| {
                let context = branch["properties"]["kind"]["enum"][0] == "perpetual_context";
                context == (version == "2.0")
            });
        let defs = root["$defs"].as_object_mut().unwrap();
        if version == "1.0" {
            defs.remove("PerpetualContext");
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
                "ConnectionState",
                "Correction",
                "Gap",
                "Heartbeat",
                "MarketMessage",
                "Reset",
                "Status",
                "Trade",
                "TradeSide",
            ] {
                defs.remove(name);
            }
            root["allOf"][1]["properties"]["asset_kind"] =
                serde_json::json!({"const": "perpetual"});
            root["allOf"][1]["properties"]["revision"] = serde_json::json!({"const": "0"});
        }
    }
    if id == "capability" {
        if version == "1.0" {
            root["$defs"]["MarketKind"]["enum"]
                .as_array_mut()
                .unwrap()
                .retain(|kind| kind != "perpetual_context");
            root["properties"]["message_kinds"]["maxItems"] = 4.into();
        } else {
            root["$defs"]["MarketKind"]["description"] =
                "Market data kinds a source/product can advertise in version 2.0.".into();
            root["properties"]["message_kinds"]["contains"] =
                serde_json::json!({"const": "perpetual_context"});
        }
    }
    Ok(root)
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = env::args_os().skip(1);
    let Some(directory) = args.next() else {
        return Err("usage: export-market-protocol OUTPUT_DIRECTORY [--v2]".into());
    };
    let version = match args.next() {
        None => "1.0",
        Some(flag) if flag == "--v2" && args.next().is_none() => "2.0",
        _ => return Err("usage: export-market-protocol OUTPUT_DIRECTORY [--v2]".into()),
    };
    let directory = PathBuf::from(directory);
    let mut documents = Vec::new();
    let input = if version == "1.0" {
        documents.push((
            "instrument.schema.json",
            schema::<Instrument>("instrument", version)?,
        ));
        include_str!("../../fixtures/examples.input.json")
    } else {
        include_str!("../../fixtures/examples-v2.input.json")
    };
    documents.extend([
        (
            "capability.schema.json",
            schema::<Capability>("capability", version)?,
        ),
        ("event.schema.json", schema::<Event>("event", version)?),
        (
            "examples.json",
            serde_json::to_value(Examples::validated(input)?)?,
        ),
    ]);
    fs::create_dir_all(&directory)?;
    for (file, mut document) in documents {
        // Canonical key ordering stays stable when other workspace crates enable
        // serde_json's preserve_order feature.
        document.sort_all_objects();
        let mut bytes = serde_json::to_vec_pretty(&document)?;
        bytes.push(b'\n');
        let path = directory.join(file);
        fs::write(&path, bytes).map_err(|error| {
            io::Error::new(error.kind(), format!("{}: {error}", path.display()))
        })?;
    }
    Ok(())
}
