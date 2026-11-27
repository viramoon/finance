use finance_market_protocol::{
    Capability, Event, Instrument, SchemaVersion,
    schema_export::{event_schema_for_version, versioned_json_schema},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::HashMap, env, error::Error, fs, io, path::PathBuf};

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

fn capability_schema(id: &str, version: SchemaVersion) -> Result<Value, Box<dyn Error>> {
    let mut root = versioned_json_schema::<Capability>(id, version)?;
    // Preserve the wire list order while enforcing the runtime uniqueness
    // check as a structural rule on the array.
    root.pointer_mut("/properties/message_kinds")
        .and_then(Value::as_object_mut)
        .ok_or("capability schema is missing message_kinds")?
        .insert("uniqueItems".into(), true.into());
    // Utoipa emits the signed int32 format for u32.
    root.pointer_mut("/properties/max_depth")
        .and_then(Value::as_object_mut)
        .ok_or("capability schema is missing max_depth")?
        .remove("format");
    let kinds = root["$defs"]["MarketKind"]["enum"]
        .as_array_mut()
        .ok_or("capability schema is missing market kinds")?;
    kinds.retain(|kind| {
        let history = kind == "option_history_trade" || kind == "option_history_quote";
        match version {
            SchemaVersion::V1 => !history && kind != "perpetual_context",
            SchemaVersion::V2 => !history,
            SchemaVersion::V3 => history,
        }
    });
    match version {
        SchemaVersion::V1 => {
            root["properties"]["message_kinds"]["maxItems"] = 4.into();
        }
        SchemaVersion::V2 => {
            root["$defs"]["MarketKind"]["description"] =
                "Market data kinds a source/product can advertise in version 2.0.".into();
            root["properties"]["message_kinds"]["contains"] = json!({"const": "perpetual_context"});
            root["properties"]["message_kinds"]["maxItems"] = 5.into();
        }
        SchemaVersion::V3 => {
            root["$defs"]["MarketKind"]["description"] =
                "Historical option evidence kinds a source/product can advertise.".into();
            root["properties"]["message_kinds"]["maxItems"] = 2.into();
            root["properties"]["max_depth"] = json!({"const": 0});
        }
    }
    Ok(root)
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = env::args_os().skip(1);
    let Some(directory) = args.next() else {
        return Err("usage: export-market-protocol OUTPUT_DIRECTORY [--v2 | --v3]".into());
    };
    let (version, wire_version) = match args.next() {
        None => (SchemaVersion::V1, "1.0"),
        Some(flag) if flag == "--v2" && args.next().is_none() => (SchemaVersion::V2, "2.0"),
        Some(flag) if flag == "--v3" && args.next().is_none() => (SchemaVersion::V3, "3.0"),
        _ => return Err("usage: export-market-protocol OUTPUT_DIRECTORY [--v2 | --v3]".into()),
    };
    let id = |name| format!("urn:viramoon:finance:market-protocol:{wire_version}:{name}");
    let directory = PathBuf::from(directory);
    let mut documents = Vec::new();
    let input = if version == SchemaVersion::V1 {
        documents.push((
            "instrument.schema.json",
            versioned_json_schema::<Instrument>(&id("instrument"), version)?,
        ));
        include_str!("../../fixtures/examples.input.json")
    } else if version == SchemaVersion::V2 {
        include_str!("../../fixtures/examples-v2.input.json")
    } else {
        include_str!("../../fixtures/examples-v3.input.json")
    };
    documents.extend([
        (
            "capability.schema.json",
            capability_schema(&id("capability"), version)?,
        ),
        (
            "event.schema.json",
            event_schema_for_version(&id("event"), version)?,
        ),
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
