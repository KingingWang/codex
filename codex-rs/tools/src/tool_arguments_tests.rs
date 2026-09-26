use super::tool_arguments_to_json_object;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn preserves_json_object_arguments() {
    assert_eq!(
        tool_arguments_to_json_object("shell", r#"{"cmd":["ls","-l"]}"#),
        json!({"cmd": ["ls", "-l"]})
    );
}

#[test]
fn empty_arguments_become_empty_object() {
    assert_eq!(tool_arguments_to_json_object("shell", "   "), json!({}));
}

#[test]
fn freeform_patch_body_is_wrapped_under_input() {
    let patch = "*** Begin Patch\n*** End Patch";
    assert_eq!(
        tool_arguments_to_json_object("apply_patch", patch),
        json!({"input": patch})
    );
}

#[test]
fn bare_command_string_is_wrapped_under_cmd_for_shell_tools() {
    assert_eq!(
        tool_arguments_to_json_object("shell", "ls -l"),
        json!({"cmd": "ls -l"})
    );
}

#[test]
fn non_object_json_values_are_wrapped() {
    assert_eq!(
        tool_arguments_to_json_object("apply_patch", "[1,2]"),
        json!({"input": [1, 2]})
    );
    assert_eq!(
        tool_arguments_to_json_object("apply_patch", "\"quoted\""),
        json!({"input": "quoted"})
    );
}
