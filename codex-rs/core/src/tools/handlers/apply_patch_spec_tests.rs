use super::*;
use pretty_assertions::assert_eq;

#[test]
fn create_apply_patch_freeform_tool_matches_expected_spec() {
    assert_eq!(
        create_apply_patch_freeform_tool(/*include_environment_id*/ false),
        ToolSpec::Freeform(FreeformTool {
            name: "apply_patch".to_string(),
            description:
                "The `apply_patch` tool can be used to edit files. This is a FREEFORM tool, so do not wrap the patch in JSON."
                    .to_string(),
            defer_loading: None,
            format: FreeformToolFormat {
                r#type: "grammar".to_string(),
                syntax: "lark".to_string(),
                definition: APPLY_PATCH_LARK_GRAMMAR.to_string(),
            },
        })
    );
}

#[test]
fn create_apply_patch_freeform_tool_includes_environment_id_when_requested() {
    let ToolSpec::Freeform(tool) =
        create_apply_patch_freeform_tool(/*include_environment_id*/ true)
    else {
        panic!("expected freeform tool");
    };

    assert!(tool.format.definition.contains("environment_id?"));
    assert!(
        tool.format
            .definition
            .contains("\"*** Environment ID: \" filename LF")
    );
}

#[test]
fn create_apply_patch_json_tool_matches_expected_spec() {
    assert_eq!(
        create_apply_patch_json_tool(/*include_environment_id*/ false),
        ToolSpec::Function(ResponsesApiTool {
            name: "apply_patch".to_string(),
            description: APPLY_PATCH_JSON_TOOL_DESCRIPTION.to_string(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(
                BTreeMap::from([(
                    "input".to_string(),
                    JsonSchema::string(Some(
                        "The entire contents of the apply_patch command".to_string(),
                    )),
                )]),
                Some(vec!["input".to_string()]),
                Some(false.into()),
            ),
            output_schema: None,
        })
    );
}

#[test]
fn create_apply_patch_json_tool_includes_environment_instruction_when_requested() {
    let ToolSpec::Function(tool) =
        create_apply_patch_json_tool(/*include_environment_id*/ true)
    else {
        panic!("expected function tool");
    };

    assert!(tool.description.contains("*** Environment ID:"));
}
