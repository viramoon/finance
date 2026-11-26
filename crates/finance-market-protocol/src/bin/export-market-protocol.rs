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
    fn validated() -> Result<Self, Box<dyn Error>> {
        let examples: Self =
            serde_json::from_str(include_str!("../../fixtures/examples.input.json"))?;
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
        if version == "2.0" {
            root["$defs"]["MarketKind"]["description"] = "Perpetual context kinds.".into();
            root["properties"]["message_kinds"]["maxItems"] = 2.into();
            root["properties"]["max_depth"]["maximum"] = 0.into();
        }
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
    if let Some(versions) = root.pointer_mut("/$defs/SchemaVersion/enum") {
        *versions = serde_json::json!([version]);
    }
    let context = ["funding_rate", "open_interest"];
    if let Some(kinds) = root
        .pointer_mut("/$defs/MarketKind/enum")
        .and_then(Value::as_array_mut)
    {
        kinds.retain(|kind| {
            context.contains(&kind.as_str().unwrap_or_default()) == (version == "2.0")
        });
    }
    if let Some(message) = root
        .pointer_mut("/$defs/Message")
        .and_then(Value::as_object_mut)
    {
        let branches = message
            .get_mut("oneOf")
            .and_then(Value::as_array_mut)
            .ok_or("missing event kinds")?;
        let old_market = ["bar_closed", "trade", "snapshot", "delta", "correction"];
        branches.retain(|branch| {
            let kind = branch
                .pointer("/properties/kind/enum/0")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if version == "2.0" {
                !old_market.contains(&kind)
            } else {
                !context.contains(&kind)
            }
        });
        let unused: &[_] = if version == "2.0" {
            message.insert(
                "description".into(),
                "Perpetual context events and stream controls.".into(),
            );
            *root
                .pointer_mut("/allOf/1/properties/asset_kind")
                .ok_or("missing asset kind")? =
                serde_json::json!({"type": "string", "enum": ["perpetual"]});
            &[
                "AssetKind",
                "ClosedBar",
                "Trade",
                "BookSnapshot",
                "BookDelta",
                "BookLevel",
                "BookUpdate",
                "BookSide",
                "TradeSide",
                "Correction",
                "MarketMessage",
            ]
        } else {
            &["FundingRate", "OpenInterest"]
        };
        let definitions = root
            .pointer_mut("/$defs")
            .and_then(Value::as_object_mut)
            .ok_or("missing definitions")?;
        for name in unused {
            definitions.remove(*name);
        }
    }
    use_local_refs(&mut root);
    Ok(root)
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = env::args_os().skip(1);
    let directory = args
        .next()
        .ok_or("usage: export-market-protocol OUTPUT_DIRECTORY [--context-v2]")?;
    let context_v2 = match (args.next().as_deref(), args.next()) {
        (None, None) => false,
        (Some(flag), None) if flag == "--context-v2" => true,
        _ => return Err("usage: export-market-protocol OUTPUT_DIRECTORY [--context-v2]".into()),
    };
    let directory = PathBuf::from(directory);
    let version = if context_v2 { "2.0" } else { "1.0" };
    let mut documents = vec![
        (
            "capability.schema.json",
            schema::<Capability>("capability", version)?,
        ),
        ("event.schema.json", schema::<Event>("event", version)?),
    ];
    if !context_v2 {
        documents.extend([
            (
                "instrument.schema.json",
                schema::<Instrument>("instrument", version)?,
            ),
            (
                "examples.json",
                serde_json::to_value(Examples::validated()?)?,
            ),
        ]);
    }
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
