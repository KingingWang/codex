//! Provider capability defaults and resolution of custom provider overrides.

use codex_api::is_azure_responses_provider;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::RemoteCompactionSupport;
use codex_model_provider_info::WireApi;

/// Optional provider-backed features that Codex may expose at runtime.
///
/// These capabilities are a provider-owned upper bound. Model and turn settings
/// can further restrict the functionality exposed to the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderCapabilities {
    /// Whether requests may include the Responses API `reasoning.summary` parameter.
    pub reasoning_summaries: bool,
    pub namespace_tools: bool,
    pub image_generation: bool,
    pub web_search: bool,
    pub external_web_access: bool,
    pub remote_compaction: RemoteCompactionSupport,
}

impl Default for ProviderCapabilities {
    fn default() -> Self {
        Self {
            reasoning_summaries: true,
            namespace_tools: true,
            image_generation: true,
            web_search: true,
            external_web_access: true,
            remote_compaction: RemoteCompactionSupport::Unsupported,
        }
    }
}

impl ProviderCapabilities {
    pub(crate) fn from_config(info: &ModelProviderInfo) -> Self {
        if info.wire_api == WireApi::Gemini {
            // This adapter implements function tools, not OpenAI hosted tools,
            // compaction endpoints, or the Responses namespace dialect.
            return Self {
                namespace_tools: false,
                image_generation: false,
                web_search: false,
                external_web_access: false,
                remote_compaction: RemoteCompactionSupport::Unsupported,
            };
        }
        let defaults = Self {
            // Tool namespaces are an OpenAI Responses dialect. Third-party
            // gateways typically flatten or reject them (for example two MCP
            // tools named `js` in different namespaces 400 as a duplicated
            // function name), so only the canonical OpenAI backend keeps them
            // by default; other providers receive flattened `namespace__name`
            // declarations unless explicitly overridden.
            namespace_tools: info.is_openai() || info.uses_openai_actor_authorization(),
            remote_compaction: if info.is_openai()
                || is_azure_responses_provider(&info.name, info.base_url.as_deref())
            {
                RemoteCompactionSupport::V2
            } else {
                RemoteCompactionSupport::Unsupported
            },
            ..Self::default()
        };
        let overrides = info.capabilities.unwrap_or_default();
        Self {
            namespace_tools: overrides
                .namespace_tools
                .unwrap_or(defaults.namespace_tools),
            external_web_access: overrides
                .external_web_access
                .unwrap_or(defaults.external_web_access),
            remote_compaction: overrides
                .remote_compaction
                .unwrap_or(defaults.remote_compaction),
            ..defaults
        }
    }
}

#[cfg(test)]
#[path = "capabilities_tests.rs"]
mod tests;
