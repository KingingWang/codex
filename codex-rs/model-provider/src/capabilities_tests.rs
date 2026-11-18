use super::*;
use crate::create_model_provider;
use codex_model_provider_info::ModelProviderCapabilities;
use pretty_assertions::assert_eq;

#[test]
fn capability_overrides_preserve_unspecified_provider_defaults() {
    for info in [
        ModelProviderInfo::default(),
        ModelProviderInfo::create_openai_provider(/*base_url*/ None),
        ModelProviderInfo {
            name: "Azure".to_string(),
            ..Default::default()
        },
    ] {
        let defaults = create_model_provider(info.clone(), /*auth_manager*/ None).capabilities();
        let provider = create_model_provider(
            ModelProviderInfo {
                capabilities: Some(ModelProviderCapabilities {
                    external_web_access: Some(false),
                    ..Default::default()
                }),
                ..info
            },
            /*auth_manager*/ None,
        );
        assert_eq!(
            provider.capabilities(),
            ProviderCapabilities {
                external_web_access: false,
                ..defaults
            },
        );
    }
}

#[test]
fn namespace_tools_default_is_canonical_openai_only() {
    let openai = create_model_provider(
        ModelProviderInfo::create_openai_provider(/*base_url*/ None),
        /*auth_manager*/ None,
    );
    assert!(openai.capabilities().namespace_tools);

    let third_party =
        create_model_provider(ModelProviderInfo::default(), /*auth_manager*/ None);
    assert!(!third_party.capabilities().namespace_tools);
}

#[test]
fn namespace_tools_override_wins_over_provider_default() {
    let flattened_openai = create_model_provider(
        ModelProviderInfo {
            capabilities: Some(ModelProviderCapabilities {
                namespace_tools: Some(false),
                ..Default::default()
            }),
            ..ModelProviderInfo::create_openai_provider(/*base_url*/ None)
        },
        /*auth_manager*/ None,
    );
    assert!(!flattened_openai.capabilities().namespace_tools);

    let namespaced_third_party = create_model_provider(
        ModelProviderInfo {
            capabilities: Some(ModelProviderCapabilities {
                namespace_tools: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        },
        /*auth_manager*/ None,
    );
    assert!(namespaced_third_party.capabilities().namespace_tools);
}
#[test]
fn gemini_does_not_inherit_openai_hosted_capabilities() {
    let mut info = codex_model_provider_info::ModelProviderInfo::create_openai_provider(Some(
        "https://example.test/v1beta".into(),
    ));
    info.wire_api = codex_model_provider_info::WireApi::Gemini;
    let capabilities = super::ProviderCapabilities::from_config(&info);
    assert!(!capabilities.namespace_tools);
    assert!(!capabilities.image_generation);
    assert!(!capabilities.web_search);
    assert_eq!(
        capabilities.remote_compaction,
        codex_model_provider_info::RemoteCompactionSupport::Unsupported
    );
}
