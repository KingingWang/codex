use codex_tools::FreeformTool;
use codex_tools::FreeformToolFormat;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;

const APPLY_PATCH_LARK_GRAMMAR: &str = include_str!("../../../assets/tools/apply_patch.lark");

/// Arguments for the JSON/function-shaped `apply_patch` tool used on wire
/// protocols that do not support freeform custom tools (Chat Completions,
/// Anthropic).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyPatchToolArgs {
    pub input: String,
}

const APPLY_PATCH_JSON_TOOL_DESCRIPTION: &str = r#"Use the `apply_patch` tool to edit files.
Your patch language is a stripped-down, file-oriented diff format designed to be easy to parse and safe to apply. You can think of it as a high-level envelope:

*** Begin Patch
[ one or more file sections ]
*** End Patch

Within that envelope, you get a sequence of file operations.
You MUST include a header to specify the action you are taking.
Each operation starts with one of three headers:

*** Add File: <path> - create a new file. Every following line is a + line (the initial contents).
*** Delete File: <path> - remove an existing file. Nothing follows.
*** Update File: <path> - patch an existing file in place (optionally with a rename).

May be immediately followed by *** Move to: <new path> if you want to rename the file.
Then one or more hunks, each introduced by @@ (optionally followed by a hunk header).
Within a hunk each line starts with:

- " " (space) for context lines
- "-" for removed lines
- "+" for added lines

By default, show 3 lines of code immediately above and 3 lines immediately below each change. If a change is within 3 lines of a previous change, do NOT duplicate the first change's trailing context lines in the second change's leading context lines.
If 3 lines of context is insufficient to uniquely identify the snippet of code within the file, use the @@ operator to indicate the class or function to which the snippet belongs.

A full patch can combine several operations:

*** Begin Patch
*** Add File: hello.txt
+Hello world
*** Update File: src/app.py
*** Move to: src/main.py
@@ def greet():
-print("Hi")
+print("Hello, world!")
*** Delete File: obsolete.txt
*** End Patch

It is important to remember:

- You must include a header with your intended action (Add/Delete/Update)
- You must prefix new lines with `+` even when creating a new file
- File references can only be relative, NEVER ABSOLUTE.
"#;

const APPLY_PATCH_ENVIRONMENT_ID_INSTRUCTION: &str = r#"
Multiple environments are available in this session. Include an
`*** Environment ID: <environment_id>` line immediately after `*** Begin Patch`
to select which environment the patch applies to.
"#;

/// Returns a custom tool that can be used to edit files. Well-suited for GPT-5 models
/// https://platform.openai.com/docs/guides/function-calling#custom-tools
pub fn create_apply_patch_freeform_tool(include_environment_id: bool) -> ToolSpec {
    let definition = if include_environment_id {
        APPLY_PATCH_LARK_GRAMMAR.replace(
            "start: begin_patch hunk+ end_patch",
            "start: begin_patch environment_id? hunk+ end_patch\nenvironment_id: \"*** Environment ID: \" filename LF",
        )
    } else {
        APPLY_PATCH_LARK_GRAMMAR.to_string()
    };
    ToolSpec::Freeform(FreeformTool {
        name: "apply_patch".to_string(),
        description: "The `apply_patch` tool can be used to edit files. This is a FREEFORM tool, so do not wrap the patch in JSON.".to_string(),
        defer_loading: None,
        format: FreeformToolFormat {
            r#type: "grammar".to_string(),
            syntax: "lark".to_string(),
            definition,
        },
    })
}

/// Returns a JSON function tool that can be used to edit files. Used on wire
/// protocols without freeform custom tool support (Chat Completions, Anthropic).
pub fn create_apply_patch_json_tool(include_environment_id: bool) -> ToolSpec {
    let mut description = APPLY_PATCH_JSON_TOOL_DESCRIPTION.to_string();
    if include_environment_id {
        description.push_str(APPLY_PATCH_ENVIRONMENT_ID_INSTRUCTION);
    }
    let properties = BTreeMap::from([(
        "input".to_string(),
        JsonSchema::string(Some(
            "The entire contents of the apply_patch command".to_string(),
        )),
    )]);

    ToolSpec::Function(ResponsesApiTool {
        name: "apply_patch".to_string(),
        description,
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["input".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

#[cfg(test)]
#[path = "apply_patch_spec_tests.rs"]
mod tests;
