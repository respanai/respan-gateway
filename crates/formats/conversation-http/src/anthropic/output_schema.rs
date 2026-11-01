//! A JSON schema as Anthropic's `output_config.format` takes it: its `$ref`s
//! expanded, and without value constraints (`minimum`, `maxItems`, ...),
//! which it refuses; each constraint is restated in its node's description,
//! so the model still reads it, as Anthropic's SDK does.

use serde_json::{Map, Value, json};

/// The value constraints it refuses, in the order a note lists them.
const CONSTRAINTS: &[(&str, &str)] = &[
    ("maxItems", "maximum number of items"),
    ("minItems", "minimum number of items"),
    ("minimum", "minimum value"),
    ("maximum", "maximum value"),
    ("exclusiveMinimum", "exclusive minimum value"),
    ("exclusiveMaximum", "exclusive maximum value"),
    ("minLength", "minimum length"),
    ("maxLength", "maximum length"),
];
const MAPS: &[&str] = &["properties", "$defs"];
const LISTS: &[&str] = &["anyOf", "allOf", "oneOf"];

/// How deep `$ref`s nest, and how many are expanded, before a schema is
/// sent with its `$ref`s as they are.
const MAX_DEPTH: usize = 32;
const MAX_REFS: usize = 1000;

/// `schema` as Anthropic takes it.
pub(super) fn output_schema(schema: &Value) -> Value {
    let mut expanded = schema.clone();
    match expand_refs(&mut expanded) {
        Some(()) => without_constraints(&expanded),
        None => without_constraints(schema),
    }
}

/// Replaces each `$ref` with a copy of its definition, by its last segment,
/// and drops the definitions; a `$ref` it cannot resolve, or one inside its
/// own definition, stays. `None` if the expansion grows too large.
fn expand_refs(schema: &mut Value) -> Option<()> {
    let mut defs = Map::new();
    if let Some(fields) = schema.as_object_mut() {
        for key in ["$defs", "definitions"] {
            if let Some(Value::Object(block)) = fields.remove(key) {
                defs.extend(block);
            }
        }
    }
    let mut budget = MAX_REFS;
    expand(schema, &defs, &mut Vec::new(), &mut budget)
}

fn expand(
    node: &mut Value,
    defs: &Map<String, Value>,
    chain: &mut Vec<String>,
    budget: &mut usize,
) -> Option<()> {
    match node {
        Value::Object(fields) => {
            if let Some(at) = fields.get("$ref").and_then(Value::as_str) {
                let name = at.rsplit('/').next().unwrap_or(at).to_owned();
                let Some(target) = defs.get(&name).filter(|_| !chain.contains(&name)) else {
                    return Some(());
                };
                *budget = budget.checked_sub(1)?;
                if chain.len() == MAX_DEPTH {
                    return None;
                }
                let mut resolved = target.clone();
                chain.push(name);
                expand(&mut resolved, defs, chain, budget)?;
                chain.pop();
                *node = resolved;
                return Some(());
            }
            fields
                .values_mut()
                .try_for_each(|child| expand(child, defs, chain, budget))
        }
        Value::Array(items) => items
            .iter_mut()
            .try_for_each(|item| expand(item, defs, chain, budget)),
        _ => Some(()),
    }
}

/// `schema` with each value constraint moved into its node's description.
fn without_constraints(schema: &Value) -> Value {
    let Some(fields) = schema.as_object() else {
        return schema.clone();
    };
    let notes: Vec<String> = CONSTRAINTS
        .iter()
        .filter_map(|(key, label)| fields.get(*key).map(|value| format!("{label}: {value}")))
        .collect();
    let mut out = Map::new();
    if !notes.is_empty() {
        let note = format!("Note: {}.", notes.join(", "));
        let description = match fields.get("description").and_then(Value::as_str) {
            Some(said) if !said.is_empty() => format!("{said} {note}"),
            _ => note,
        };
        out.insert("description".into(), json!(description));
    }
    for (key, value) in fields {
        if CONSTRAINTS.iter().any(|(name, _)| name == key) || out.contains_key(key) {
            continue;
        }
        let value = match value {
            Value::Object(members) if MAPS.contains(&key.as_str()) => Value::Object(
                members
                    .iter()
                    .map(|(name, member)| (name.clone(), without_constraints(member)))
                    .collect(),
            ),
            Value::Object(_) if key == "items" => without_constraints(value),
            Value::Array(branches) if LISTS.contains(&key.as_str()) => {
                Value::Array(branches.iter().map(without_constraints).collect())
            }
            other => other.clone(),
        };
        out.insert(key.clone(), value);
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constraints_move_into_descriptions_at_every_depth() {
        let schema = json!({
            "type": "object",
            "properties": {
                "tags": { "type": "array", "maxItems": 3, "items": { "type": "string", "maxLength": 9 } },
                "n": { "type": "integer", "minimum": 1, "description": "How many." },
                "pick": { "anyOf": [{ "type": "number", "exclusiveMaximum": 2 }, { "type": "null" }] },
            },
            "required": ["tags"],
        });
        assert_eq!(
            output_schema(&schema),
            json!({
                "type": "object",
                "properties": {
                    "tags": { "description": "Note: maximum number of items: 3.", "type": "array",
                        "items": { "description": "Note: maximum length: 9.", "type": "string" } },
                    "n": { "description": "How many. Note: minimum value: 1.", "type": "integer" },
                    "pick": { "anyOf": [
                        { "description": "Note: exclusive maximum value: 2.", "type": "number" },
                        { "type": "null" },
                    ] },
                },
                "required": ["tags"],
            })
        );
    }

    #[test]
    fn refs_are_expanded_and_a_cycle_left_as_it_is() {
        let schema = json!({
            "type": "object",
            "properties": { "a": { "$ref": "#/$defs/A" }, "n": { "$ref": "#/definitions/Node" } },
            "$defs": { "A": { "type": "array", "minItems": 1 } },
            "definitions": { "Node": { "type": "object", "properties": { "next": { "$ref": "#/definitions/Node" } } } },
        });
        let out = output_schema(&schema);
        assert!(out.get("$defs").is_none() && out.get("definitions").is_none());
        assert_eq!(
            out["properties"]["a"],
            json!({ "description": "Note: minimum number of items: 1.", "type": "array" })
        );
        assert_eq!(
            out["properties"]["n"]["properties"]["next"],
            json!({ "$ref": "#/definitions/Node" })
        );
    }

    #[test]
    fn an_expansion_too_large_keeps_its_refs() {
        let mut defs = Map::new();
        for i in 0..40 {
            let next = format!("#/$defs/D{}", i + 1);
            defs.insert(
                format!("D{i}"),
                json!({ "anyOf": [{ "$ref": next }, { "$ref": next }] }),
            );
        }
        defs.insert("D40".into(), json!({ "type": "string", "maxLength": 1 }));
        let schema = json!({ "$ref": "#/$defs/D0", "$defs": defs });
        let out = output_schema(&schema);
        assert_eq!(out["$ref"], "#/$defs/D0");
        assert_eq!(
            out["$defs"]["D40"]["description"],
            "Note: maximum length: 1."
        );
    }
}
