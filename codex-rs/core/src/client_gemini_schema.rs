// Adapted from OpenCode's packages/ai/src/protocols/utils/gemini-json-schema.ts.
//
// MIT License
// Copyright (c) 2025 opencode
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

//! The few JSON Schema shapes Gemini rejects even in parametersJsonSchema.
//! Preserve constraints and value payloads; normalize legacy draft syntax and
//! only cut recursive references that cannot terminate.

use serde_json::Map;
use serde_json::Value;

fn value_keyword(key: &str) -> bool {
    matches!(
        key,
        "const" | "default" | "enum" | "examples" | "dependentRequired"
    )
}

fn schema_map(key: &str) -> bool {
    matches!(
        key,
        "properties"
            | "patternProperties"
            | "$defs"
            | "definitions"
            | "dependentSchemas"
            | "dependencies"
    )
}

fn normalize_node(value: &Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.iter().map(normalize_node).collect()),
        Value::Object(object) => {
            let mut result = Map::new();
            for (key, value) in object {
                let normalized = if value_keyword(key) {
                    value.clone()
                } else if schema_map(key) && value.is_object() {
                    // Recurse into known schema maps.
                    Value::Object(
                        value
                            .as_object()
                            .into_iter()
                            .flatten()
                            .map(|(name, schema)| (name.clone(), normalize_node(schema)))
                            .collect(),
                    )
                } else if key == "required" {
                    if let (Some(properties), Some(required)) = (
                        object.get("properties").and_then(Value::as_object),
                        value.as_array(),
                    ) {
                        Value::Array(
                            required
                                .iter()
                                .filter(|name| {
                                    name.as_str()
                                        .is_some_and(|name| properties.contains_key(name))
                                })
                                .cloned()
                                .collect(),
                        )
                    } else {
                        value.clone()
                    }
                } else if matches!(key.as_str(), "exclusiveMinimum" | "exclusiveMaximum")
                    && value.is_boolean()
                {
                    if value == &Value::Bool(true) {
                        let bound = if key == "exclusiveMinimum" {
                            "minimum"
                        } else {
                            "maximum"
                        };
                        let Some(number) = object.get(bound).filter(|value| value.is_number())
                        else {
                            continue;
                        };
                        number.clone()
                    } else {
                        continue;
                    }
                } else if (key == "minimum"
                    && object.get("exclusiveMinimum") == Some(&Value::Bool(true)))
                    || (key == "maximum"
                        && object.get("exclusiveMaximum") == Some(&Value::Bool(true)))
                {
                    continue;
                } else if key == "items" && value.is_array() {
                    result.insert("prefixItems".into(), normalize_node(value));
                    continue;
                } else if key == "additionalItems"
                    && object.get("items").is_some_and(Value::is_array)
                {
                    result.insert("items".into(), normalize_node(value));
                    continue;
                } else {
                    // Other keywords may contain schemas, and literal payloads
                    // such as default, const, and examples remain unchanged
                    // under recursive normalization.
                    normalize_node(value)
                };
                result.insert(key.clone(), normalized);
            }
            Value::Object(result)
        }
        _ => value.clone(),
    }
}

fn cut_loops(value: &mut Value, target: &str, safe: bool) {
    match value {
        Value::Array(values) => {
            for value in values {
                cut_loops(value, target, safe);
            }
        }
        Value::Object(object) => {
            if object.get("$ref").and_then(Value::as_str) == Some(target) && !safe {
                *value = Value::Object(Map::new());
                tracing::debug!("Gemini schema normalization removed a non-terminating reference");
                return;
            }
            let required = object
                .get("required")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let empty_array = object
                .get("minItems")
                .and_then(Value::as_u64)
                .unwrap_or_default()
                == 0;
            for (key, value) in object {
                if value_keyword(key) {
                    continue;
                }
                if key == "items" {
                    cut_loops(value, target, safe || empty_array);
                } else if key == "properties" && value.is_object() {
                    for (name, child) in value.as_object_mut().into_iter().flatten() {
                        cut_loops(
                            child,
                            target,
                            safe || !required.contains(&Value::String(name.clone())),
                        );
                    }
                } else if schema_map(key) && value.is_object() {
                    for (_, child) in value.as_object_mut().into_iter().flatten() {
                        cut_loops(child, target, safe);
                    }
                } else {
                    cut_loops(value, target, safe);
                }
            }
        }
        _ => {}
    }
}

pub(super) fn normalize(schema: Value) -> Value {
    let mut result = normalize_node(&schema);
    for bag in ["$defs", "definitions"] {
        if let Some(definitions) = result.get_mut(bag).and_then(Value::as_object_mut) {
            for (name, definition) in definitions {
                let name = name.replace('~', "~0").replace('/', "~1");
                cut_loops(definition, &format!("#/{bag}/{name}"), false);
            }
        }
    }
    cut_loops(&mut result, "#", false);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn gemini_schema_normalizes_legacy_drafts_without_touching_literal_values() {
        let result = normalize(json!({
            "type": "object", "required": ["ttl", "missing"],
            "properties": {
                "ttl": {"type": "number", "minimum": 0, "exclusiveMinimum": true, "maximum": 60, "exclusiveMaximum": false},
                "pair": {"type": "array", "items": [{"type": "string"}], "additionalItems": false},
                "cart": {"default": {"items": ["literal"], "required": ["not-a-schema"]}}
            },
            "additionalProperties": false, "dependentRequired": {"ttl": ["pair"]}
        }));
        assert_eq!(result["required"], json!(["ttl"]));
        assert_eq!(
            result["properties"]["ttl"],
            json!({"type": "number", "exclusiveMinimum": 0, "maximum": 60})
        );
        assert_eq!(
            result["properties"]["pair"]["prefixItems"],
            json!([{"type": "string"}])
        );
        assert_eq!(result["properties"]["pair"]["items"], false);
        assert_eq!(
            result["properties"]["cart"]["default"]["items"],
            json!(["literal"])
        );
        assert_eq!(result["additionalProperties"], false);
        assert_eq!(result["dependentRequired"], json!({"ttl": ["pair"]}));
    }

    #[test]
    fn gemini_schema_keeps_optional_recursion_and_cuts_non_terminating_loops() {
        let result = normalize(json!({
            "type": "object", "properties": {"optional": {"$ref": "#"}, "required": {"$ref": "#"}},
            "required": ["required"],
            "$defs": {
                "Node": {"properties": {"child": {"$ref": "#/$defs/Node"}}},
                "Loop": {"properties": {"next": {"$ref": "#/$defs/Loop"}}, "required": ["next"]}
            }
        }));
        assert_eq!(result["properties"]["optional"], json!({"$ref": "#"}));
        assert_eq!(result["properties"]["required"], json!({}));
        assert_eq!(
            result["$defs"]["Node"]["properties"]["child"],
            json!({"$ref": "#/$defs/Node"})
        );
        assert_eq!(result["$defs"]["Loop"]["properties"]["next"], json!({}));
    }
}
