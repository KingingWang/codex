//! Normalization of model-emitted tool-call arguments into the JSON-object
//! shape that provider wire formats require.
//!
//! Codex records tool-call arguments as opaque strings, because the two tool
//! flavors carry different payloads: JSON-schema function calls carry a JSON
//! object, while freeform (custom) tools such as `apply_patch` carry raw text
//! like a patch body. Provider wire formats are stricter than our internal
//! representation:
//!
//! - Chat Completions requires `function.arguments` to be a JSON object string.
//! - The Anthropic Messages API requires `tool_use.input` to be a JSON object,
//!   and rejects anything else with
//!   `tool_use.input: Input should be a valid dictionary`.
//!
//! Both wire formats therefore need the same repair: keep valid JSON objects
//! verbatim, treat empty arguments as an empty object, and wrap any other
//! value (bare strings, arrays, numbers, non-JSON text) in a single-key object.

use serde_json::Value;

/// Key used to carry a wrapped non-object argument value for shell-style tools.
const SHELL_ARGUMENT_KEY: &str = "cmd";
/// Key used to carry a wrapped non-object argument value for all other tools.
const DEFAULT_ARGUMENT_KEY: &str = "input";

/// Normalizes recorded tool-call arguments into a JSON object [`Value`].
///
/// Valid JSON objects are returned as-is. Empty arguments become an empty
/// object. Every other value is wrapped via [`wrap_non_object_tool_arguments`]
/// so the result is always an object, which is what both the Anthropic
/// Messages API and Chat Completions expect.
pub fn tool_arguments_to_json_object(name: &str, arguments: &str) -> Value {
    let trimmed = arguments.trim();
    if trimmed.is_empty() {
        return Value::Object(Default::default());
    }

    match serde_json::from_str::<Value>(trimmed) {
        Ok(Value::Object(map)) => Value::Object(map),
        Ok(value) => wrap_non_object_tool_arguments(name, value),
        // Non-JSON text (e.g. a freeform `apply_patch` patch body, or legacy
        // bare command strings): wrap the trimmed text so leading/trailing
        // whitespace is not smuggled into the value.
        Err(_) => wrap_non_object_tool_arguments(name, Value::String(trimmed.to_string())),
    }
}

/// Wraps a non-object argument value in a single-key JSON object, choosing the
/// key that matches the tool's own schema so the model sees a familiar shape.
pub fn wrap_non_object_tool_arguments(name: &str, value: Value) -> Value {
    let key = match name {
        "exec_command" | "shell" => SHELL_ARGUMENT_KEY,
        _ => DEFAULT_ARGUMENT_KEY,
    };
    serde_json::json!({ key: value })
}

#[cfg(test)]
#[path = "tool_arguments_tests.rs"]
mod tests;
