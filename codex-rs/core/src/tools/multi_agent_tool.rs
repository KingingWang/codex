//! Applies captured Multi-Agent V2 catalog overrides and namespaces to tool specifications.
//! V2 messages remain plaintext even when a model catalog advertises encrypted parameters.

use crate::session::session::Session;
use crate::tools::context::ToolInvocation;
use crate::tools::registry::CoreToolRuntime;
use codex_tools::AdditionalProperties;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiNamespace;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ToolExecutor;
use codex_tools::ToolExposure;
use codex_tools::ToolName;
use codex_tools::ToolSearchInfo;
use codex_tools::ToolSpec;
use futures::future::BoxFuture;
use std::sync::Arc;

pub(crate) const MULTI_AGENT_V2_NAMESPACE_DESCRIPTION: &str =
    "Tools for spawning and managing sub-agents.";

pub(super) fn multi_agent_v2_handler(
    handler: impl CoreToolRuntime + 'static,
    namespace: Option<&str>,
    description_override: Option<&str>,
    parameters_override: Option<&str>,
) -> Arc<dyn CoreToolRuntime> {
    let parameters_override = parameters_override.map(|parameters| -> Result<JsonSchema, &str> {
        let mut parameters = super::catalog_parameters::parse(parameters)?;
        if let ToolSpec::Function(tool) = handler.spec()
            && let Some(properties) = tool.parameters.properties
        {
            if properties.contains_key("message")
                && !parameters
                    .properties
                    .as_ref()
                    .is_some_and(|properties| properties.contains_key("message"))
            {
                return Err("schema omits the message parameter");
            }
        }
        clear_encryption_annotations(&mut parameters);
        Ok(parameters)
    });
    if let Some(Err(reason)) = &parameters_override {
        tracing::warn!(tool = %handler.tool_name(), reason, "Invalid catalog tool parameters; using bundled parameters");
    }
    let parameters_override = parameters_override.and_then(Result::ok);
    if namespace.is_none() && description_override.is_none() && parameters_override.is_none() {
        return Arc::new(handler);
    }
    Arc::new(MultiAgentV2ToolOverrides {
        handler: Arc::new(handler),
        namespace: namespace.map(str::to_owned),
        description_override: description_override.map(str::to_owned),
        parameters_override,
    })
}

fn clear_encryption_annotations(schema: &mut JsonSchema) {
    schema.encrypted = None;
    for properties in [
        &mut schema.properties,
        &mut schema.defs,
        &mut schema.definitions,
    ]
    .into_iter()
    .flatten()
    {
        for child in properties.values_mut() {
            clear_encryption_annotations(child);
        }
    }
    if let Some(items) = &mut schema.items {
        clear_encryption_annotations(items);
    }
    if let Some(AdditionalProperties::Schema(child)) = &mut schema.additional_properties {
        clear_encryption_annotations(child);
    }
    for variants in [&mut schema.any_of, &mut schema.one_of, &mut schema.all_of]
        .into_iter()
        .flatten()
    {
        for child in variants.iter_mut() {
            clear_encryption_annotations(child);
        }
    }
}

struct MultiAgentV2ToolOverrides {
    handler: Arc<dyn CoreToolRuntime>,
    namespace: Option<String>,
    description_override: Option<String>,
    parameters_override: Option<JsonSchema>,
}

impl ToolExecutor<ToolInvocation> for MultiAgentV2ToolOverrides {
    fn tool_name(&self) -> ToolName {
        let tool_name = self.handler.tool_name();
        match &self.namespace {
            Some(namespace) => ToolName::namespaced(namespace.clone(), tool_name.name),
            None => tool_name,
        }
    }

    fn spec(&self) -> ToolSpec {
        let mut spec = self.handler.spec();
        if let ToolSpec::Function(tool) = &mut spec {
            if let Some(description) = &self.description_override {
                tool.description.clone_from(description);
            }
            if let Some(parameters) = &self.parameters_override {
                tool.parameters.clone_from(parameters);
            }
        }
        match (&self.namespace, spec) {
            (Some(namespace), ToolSpec::Function(tool)) => {
                ToolSpec::Namespace(ResponsesApiNamespace {
                    name: namespace.clone(),
                    description: MULTI_AGENT_V2_NAMESPACE_DESCRIPTION.to_string(),
                    tools: vec![ResponsesApiNamespaceTool::Function(tool)],
                })
            }
            (_, spec) => spec,
        }
    }

    fn exposure(&self) -> ToolExposure {
        self.handler.exposure()
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        self.handler.supports_parallel_tool_calls()
    }

    fn search_info(&self) -> Option<ToolSearchInfo> {
        self.handler.search_info()
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        self.handler.handle(invocation)
    }
}

impl CoreToolRuntime for MultiAgentV2ToolOverrides {
    fn wait_until_ready<'a>(&'a self, session: &'a Arc<Session>) -> Option<BoxFuture<'a, ()>> {
        self.handler.wait_until_ready(session)
    }

    fn matches_kind(&self, payload: &crate::tools::context::ToolPayload) -> bool {
        self.handler.matches_kind(payload)
    }

    fn create_diff_consumer(
        &self,
    ) -> Option<Box<dyn crate::tools::registry::ToolArgumentDiffConsumer>> {
        self.handler.create_diff_consumer()
    }
}

#[cfg(test)]
mod tests {
    use super::clear_encryption_annotations;
    use codex_tools::JsonSchema;
    use serde_json::json;

    #[test]
    fn catalog_encryption_annotations_are_removed_without_changing_instance_data() {
        let mut schema: JsonSchema = serde_json::from_value(json!({
            "type": "object",
            "encrypted": true,
            "properties": {
                "message": {"type": "string", "encrypted": true},
                "items": {"type": "array", "items": {"type": "string", "encrypted": true}}
            },
            "additionalProperties": {"type": "string", "encrypted": true},
            "anyOf": [{"type": "object", "encrypted": true}],
            "oneOf": [{"type": "object", "encrypted": true}],
            "allOf": [{"type": "object", "encrypted": true}],
            "$defs": {"message": {"type": "string", "encrypted": true}},
            "definitions": {"message": {"type": "string", "encrypted": true}},
            "enum": [{"encrypted": true}]
        }))
        .expect("valid catalog schema");
        clear_encryption_annotations(&mut schema);
        let value = serde_json::to_value(schema).expect("serialize schema");
        for pointer in [
            "/encrypted",
            "/properties/message/encrypted",
            "/properties/items/items/encrypted",
            "/additionalProperties/encrypted",
            "/anyOf/0/encrypted",
            "/oneOf/0/encrypted",
            "/allOf/0/encrypted",
            "/$defs/message/encrypted",
            "/definitions/message/encrypted",
        ] {
            assert!(value.pointer(pointer).is_none(), "{pointer}");
        }
        assert_eq!(value["enum"], json!([{"encrypted": true}]));
    }
}
