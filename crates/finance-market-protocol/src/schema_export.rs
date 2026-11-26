//! Build standalone JSON Schemas from the protocol's Rust wire types.

use crate::Event;
use serde_json::{Map, Value};
use std::error::Error;
use utoipa::ToSchema;

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

pub fn event_schema(id: &str) -> Result<Value, Box<dyn Error>> {
    let mut root = json_schema::<Event>(id)?;
    close_event_envelopes(&mut root, "", "/$defs/MarketMessage")?;
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
