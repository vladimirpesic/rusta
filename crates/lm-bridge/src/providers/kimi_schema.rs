use std::collections::HashSet;

use serde_json::{Map, Value, json};

use crate::errors::LlmError;

/// Normalize a JSON Schema so Moonshot's validator accepts it.
///
/// Steps:
/// 1. Dereference local `$ref` pointers (`#`, `#/$defs/X`, `#/definitions/X`).
/// 2. Detect and preserve circular references (their definition buckets are kept).
/// 3. Remove definition buckets that are no longer referenced.
/// 4. Inject missing `type` fields and repair conflicting `enum`/`const` vs. `type`.
pub fn normalize_kimi_tool_schema(schema: &Value) -> Result<Value, LlmError> {
    let mut dereferenced = deref_json_schema(schema.clone())?;
    ensure_kimi_property_types(&mut dereferenced)?;
    Ok(dereferenced)
}

// ---------------------------------------------------------------------------
// Dereferencing
// ---------------------------------------------------------------------------

fn deref_json_schema(schema: Value) -> Result<Value, LlmError> {
    let root = schema;
    let mut visited = HashSet::new();
    let mut result = resolve_node(&root, root.clone(), &mut visited)?;

    if let Value::Object(obj) = &mut result {
        if !has_unresolved_def_ref(&Value::Object(obj.clone()), "$defs") {
            obj.remove("$defs");
        }
        if !has_unresolved_def_ref(&Value::Object(obj.clone()), "definitions") {
            obj.remove("definitions");
        }
    }

    Ok(result)
}

fn resolve_node(
    root: &Value,
    node: Value,
    visited: &mut HashSet<String>,
) -> Result<Value, LlmError> {
    match node {
        Value::Object(obj) => {
            if let Some(ref_val) = obj.get("$ref").and_then(|v| v.as_str())
                && is_local_json_pointer_ref(ref_val)
            {
                if visited.contains(ref_val) {
                    // Circular reference — leave the $ref intact so the
                    // definition bucket remains resolvable.
                    return Ok(Value::Object(obj));
                }

                if let Some(target) = resolve_local_json_pointer(root, ref_val) {
                    visited.insert(ref_val.to_string());
                    let resolved = resolve_node(root, target.clone(), visited)?;
                    visited.remove(ref_val);

                    // Merge sibling keywords; the local node takes precedence.
                    if let Value::Object(mut r_obj) = resolved {
                        for (k, v) in obj {
                            if k == "$ref" {
                                continue;
                            }
                            let processed = resolve_node(root, v, visited)?;
                            r_obj.insert(k, processed);
                        }
                        return Ok(Value::Object(r_obj));
                    }
                    return Ok(resolved);
                }
            }

            let mut new_obj = Map::new();
            for (k, v) in obj {
                new_obj.insert(k, resolve_node(root, v, visited)?);
            }
            Ok(Value::Object(new_obj))
        }
        Value::Array(arr) => arr
            .into_iter()
            .map(|v| resolve_node(root, v, visited))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        other => Ok(other),
    }
}

fn is_local_json_pointer_ref(reference: &str) -> bool {
    reference == "#" || reference.starts_with("#/")
}

fn resolve_local_json_pointer<'a>(root: &'a Value, reference: &str) -> Option<&'a Value> {
    // Strip the leading '#' to obtain a JSON Pointer path.
    let pointer = &reference[1..];
    root.pointer(pointer)
}

fn has_unresolved_def_ref(node: &Value, bucket: &str) -> bool {
    match node {
        Value::Object(obj) => {
            if let Some(Value::String(r)) = obj.get("$ref")
                && r.starts_with(&format!("#/{}/", bucket))
            {
                return true;
            }
            for (k, v) in obj {
                if k == bucket {
                    continue;
                }
                if has_unresolved_def_ref(v, bucket) {
                    return true;
                }
            }
            false
        }
        Value::Array(arr) => arr.iter().any(|v| has_unresolved_def_ref(v, bucket)),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Type inference / repair
// ---------------------------------------------------------------------------

const TYPE_COMPLETION_SKIP_KEYS: &[&str] = &[
    "$ref", "allOf", "anyOf", "else", "if", "not", "oneOf", "then",
];

const OBJECT_STRUCTURE_KEYS: &[&str] = &[
    "$defs",
    "definitions",
    "dependencies",
    "dependentSchemas",
    "patternProperties",
    "properties",
    "additionalProperties",
    "propertyNames",
    "unevaluatedProperties",
    "dependentRequired",
    "maxProperties",
    "minProperties",
    "required",
];

const ARRAY_STRUCTURE_KEYS: &[&str] = &[
    "additionalItems",
    "contains",
    "prefixItems",
    "items",
    "unevaluatedItems",
    "maxContains",
    "maxItems",
    "minContains",
    "minItems",
    "uniqueItems",
];

const STRING_STRUCTURE_KEYS: &[&str] = &[
    "contentEncoding",
    "contentMediaType",
    "contentSchema",
    "format",
    "maxLength",
    "minLength",
    "pattern",
];

const NUMERIC_STRUCTURE_KEYS: &[&str] = &[
    "exclusiveMaximum",
    "exclusiveMinimum",
    "maximum",
    "minimum",
    "multipleOf",
];

#[derive(Debug, Clone, Copy)]
enum SchemaSlotKind {
    Single,
    Array,
    Map,
    SchemaOrArray,
}

const CHILD_SCHEMA_SLOTS: &[(&str, SchemaSlotKind)] = &[
    ("$defs", SchemaSlotKind::Map),
    ("definitions", SchemaSlotKind::Map),
    ("dependencies", SchemaSlotKind::Map),
    ("dependentSchemas", SchemaSlotKind::Map),
    ("patternProperties", SchemaSlotKind::Map),
    ("properties", SchemaSlotKind::Map),
    ("additionalItems", SchemaSlotKind::Single),
    ("additionalProperties", SchemaSlotKind::Single),
    ("contains", SchemaSlotKind::Single),
    ("contentSchema", SchemaSlotKind::Single),
    ("else", SchemaSlotKind::Single),
    ("if", SchemaSlotKind::Single),
    ("not", SchemaSlotKind::Single),
    ("propertyNames", SchemaSlotKind::Single),
    ("then", SchemaSlotKind::Single),
    ("unevaluatedItems", SchemaSlotKind::Single),
    ("unevaluatedProperties", SchemaSlotKind::Single),
    ("allOf", SchemaSlotKind::Array),
    ("anyOf", SchemaSlotKind::Array),
    ("oneOf", SchemaSlotKind::Array),
    ("prefixItems", SchemaSlotKind::Array),
    ("items", SchemaSlotKind::SchemaOrArray),
];

fn ensure_kimi_property_types(schema: &mut Value) -> Result<(), LlmError> {
    if !schema.is_object() {
        return Err(LlmError::SchemaNormalization(
            "JSON Schema root must normalize to an object".to_string(),
        ));
    }
    normalize_property(schema);
    Ok(())
}

fn recurse_schema(node: &mut Value) {
    if !node.is_object() {
        return;
    }
    visit_child_schemas(node, normalize_property);
}

fn visit_child_schemas(node: &mut Value, mut visit: impl FnMut(&mut Value)) {
    let Some(obj) = node.as_object_mut() else {
        return;
    };

    for &(key, kind) in CHILD_SCHEMA_SLOTS {
        match kind {
            SchemaSlotKind::Single => {
                if let Some(child) = obj.get_mut(key)
                    && child.is_object()
                {
                    visit(child);
                }
            }
            SchemaSlotKind::Array => {
                if let Some(arr) = obj.get_mut(key).and_then(|v| v.as_array_mut()) {
                    for item in arr {
                        visit(item);
                    }
                }
            }
            SchemaSlotKind::Map => {
                if let Some(map) = obj.get_mut(key).and_then(|v| v.as_object_mut()) {
                    for child in map.values_mut() {
                        visit(child);
                    }
                }
            }
            SchemaSlotKind::SchemaOrArray => {
                if let Some(child) = obj.get_mut(key) {
                    if let Some(arr) = child.as_array_mut() {
                        for item in arr {
                            visit(item);
                        }
                    } else {
                        visit(child);
                    }
                }
            }
        }
    }
}

fn normalize_property(node: &mut Value) {
    if !node.is_object() {
        return;
    }

    let obj = node.as_object_mut().expect("checked is_object");
    let has_type = obj.contains_key("type");
    let skip = has_any_key(obj, TYPE_COMPLETION_SKIP_KEYS);

    if !has_type && !skip {
        if let Some(t) = infer_type_for_node(obj) {
            obj.insert("type".to_string(), json!(t));
        }
    } else if !skip
        && let Some(Value::String(current_type)) = obj.get("type").cloned()
        && let Some(inferred) = infer_type_from_enum_or_const(obj)
        && inferred != current_type
    {
        obj.insert("type".to_string(), json!(inferred));
        remove_irrelevant_structure_keys(obj, inferred);
    }

    recurse_schema(node);
}

fn infer_type_for_node(obj: &Map<String, Value>) -> Option<&'static str> {
    if let Some(inferred) = infer_type_from_enum_or_const(obj) {
        return Some(inferred);
    }
    Some(infer_type_from_structure(obj))
}

fn infer_type_from_enum_or_const(obj: &Map<String, Value>) -> Option<&'static str> {
    if let Some(Value::Array(vals)) = obj.get("enum")
        && !vals.is_empty()
    {
        return infer_type_from_values(vals);
    }
    if let Some(val) = obj.get("const") {
        return infer_type_from_values(std::slice::from_ref(val));
    }
    None
}

fn infer_type_from_structure(obj: &Map<String, Value>) -> &'static str {
    if has_any_key(obj, OBJECT_STRUCTURE_KEYS) {
        return "object";
    }
    if has_any_key(obj, ARRAY_STRUCTURE_KEYS) {
        return "array";
    }
    if has_any_key(obj, STRING_STRUCTURE_KEYS) {
        return "string";
    }
    if has_any_key(obj, NUMERIC_STRUCTURE_KEYS) {
        return "number";
    }
    "string"
}

fn infer_type_from_values(values: &[Value]) -> Option<&'static str> {
    let mut types = HashSet::new();
    for value in values {
        types.insert(json_value_schema_type(value)?);
    }
    // JSON Schema treats integer as a subset of number.
    if types.contains("number") {
        types.remove("integer");
    }
    if types.len() == 1 {
        types.into_iter().next()
    } else {
        None
    }
}

fn json_value_schema_type(value: &Value) -> Option<&'static str> {
    match value {
        Value::Null => Some("null"),
        Value::Bool(_) => Some("boolean"),
        Value::String(_) => Some("string"),
        Value::Array(_) => Some("array"),
        Value::Object(_) => Some("object"),
        Value::Number(n) => {
            if n.is_i64() || n.is_u64() {
                Some("integer")
            } else {
                Some("number")
            }
        }
    }
}

fn remove_irrelevant_structure_keys(obj: &mut Map<String, Value>, new_type: &str) {
    if new_type != "object" {
        for key in OBJECT_STRUCTURE_KEYS {
            obj.remove(*key);
        }
    }
    if new_type != "array" {
        for key in ARRAY_STRUCTURE_KEYS {
            obj.remove(*key);
        }
    }
}

fn has_any_key(obj: &Map<String, Value>, keys: &[&str]) -> bool {
    obj.keys().any(|k| keys.contains(&k.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_injects_object_type_from_properties() {
        let schema = json!({
            "properties": {
                "name": { "type": "string" },
                "nested": {
                    "properties": { "x": { "type": "integer" } }
                }
            }
        });
        let normalized = normalize_kimi_tool_schema(&schema).unwrap();
        assert_eq!(normalized["type"], "object");
        assert_eq!(normalized["properties"]["nested"]["type"], "object");
    }

    #[test]
    fn test_derefs_local_ref() {
        let schema = json!({
            "$defs": {
                "Foo": { "type": "string" }
            },
            "type": "object",
            "properties": {
                "foo": { "$ref": "#/$defs/Foo" }
            }
        });
        let normalized = normalize_kimi_tool_schema(&schema).unwrap();
        assert!(normalized.get("$defs").is_none());
        assert_eq!(normalized["properties"]["foo"]["type"], "string");
    }

    #[test]
    fn test_derefs_nested_defs() {
        let schema = json!({
            "$defs": {
                "Outer": {
                    "type": "object",
                    "properties": {
                        "inner": { "$ref": "#/$defs/Inner" }
                    }
                },
                "Inner": { "type": "integer" }
            },
            "type": "object",
            "properties": {
                "outer": { "$ref": "#/$defs/Outer" }
            }
        });
        let normalized = normalize_kimi_tool_schema(&schema).unwrap();
        assert!(normalized.get("$defs").is_none());
        assert_eq!(
            normalized["properties"]["outer"]["properties"]["inner"]["type"],
            "integer"
        );
    }

    #[test]
    fn test_preserves_definitions_on_circular_ref() {
        let schema = json!({
            "$defs": {
                "Node": {
                    "type": "object",
                    "properties": {
                        "next": { "$ref": "#/$defs/Node" }
                    }
                }
            },
            "type": "object",
            "properties": {
                "root": { "$ref": "#/$defs/Node" }
            }
        });
        let normalized = normalize_kimi_tool_schema(&schema).unwrap();
        assert!(normalized.get("$defs").is_some());
        let root = &normalized["properties"]["root"];
        assert_eq!(root["type"], "object");
        assert_eq!(root["properties"]["next"]["$ref"], "#/$defs/Node");
    }

    #[test]
    fn test_infers_type_from_enum_values() {
        let schema = json!({
            "type": "object",
            "properties": {
                "mode": { "enum": ["fast", "slow"] }
            }
        });
        let normalized = normalize_kimi_tool_schema(&schema).unwrap();
        assert_eq!(normalized["properties"]["mode"]["type"], "string");
    }

    #[test]
    fn test_infers_type_from_const() {
        let schema = json!({
            "type": "object",
            "properties": {
                "enabled": { "const": true }
            }
        });
        let normalized = normalize_kimi_tool_schema(&schema).unwrap();
        assert_eq!(normalized["properties"]["enabled"]["type"], "boolean");
    }

    #[test]
    fn test_repairs_conflicting_type_and_enum() {
        let schema = json!({
            "type": "object",
            "properties": {
                "status": {
                    "type": "object",
                    "properties": { "unused": { "type": "string" } },
                    "enum": ["ok", "error"]
                }
            }
        });
        let normalized = normalize_kimi_tool_schema(&schema).unwrap();
        let status = &normalized["properties"]["status"];
        assert_eq!(status["type"], "string");
        assert!(status.get("properties").is_none());
        assert_eq!(status["enum"], json!(["ok", "error"]));
    }

    #[test]
    fn test_fallback_to_string_for_typeless_leaf() {
        let schema = json!({
            "type": "object",
            "properties": {
                "note": { "description": "A free-form note" }
            }
        });
        let normalized = normalize_kimi_tool_schema(&schema).unwrap();
        assert_eq!(normalized["properties"]["note"]["type"], "string");
    }

    #[test]
    fn test_skips_combinator_nodes_for_type_inference() {
        let schema = json!({
            "type": "object",
            "properties": {
                "value": { "anyOf": [{ "type": "string" }, { "type": "number" }] }
            }
        });
        let normalized = normalize_kimi_tool_schema(&schema).unwrap();
        assert!(normalized["properties"]["value"].get("type").is_none());
    }
}
