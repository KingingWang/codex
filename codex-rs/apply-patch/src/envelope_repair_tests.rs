use super::normalize_apply_patch_delimiters;
use super::patch_input_from_function_arguments;
use super::strip_outer_markdown_code_fence;
use pretty_assertions::assert_eq;

const CANONICAL_PATCH: &str = "\
*** Begin Patch
*** Update File: src/main.rs
@@
-old
+new
*** End Patch
";

#[test]
fn canonical_envelope_is_byte_exact_passthrough() {
    assert_eq!(
        normalize_apply_patch_delimiters(CANONICAL_PATCH),
        CANONICAL_PATCH
    );
}

#[test]
fn decorated_delimiters_are_repaired_and_body_is_untouched() {
    let decorated = "\
*** Begin Patch ***
*** Update File: docs/notes.md
@@
-*** Begin Patch ***
+*** End Patch ***
 *** keep this interior decoration
*** End Patch ***
";
    let expected = "\
*** Begin Patch
*** Update File: docs/notes.md
@@
-*** Begin Patch ***
+*** End Patch ***
 *** keep this interior decoration
*** End Patch
";
    assert_eq!(normalize_apply_patch_delimiters(decorated), expected);
}

#[test]
fn only_the_begin_line_decorated_is_repaired() {
    let decorated = "*** Begin Patch ***\n*** Add File: a.txt\n+hi\n*** End Patch\n";
    let expected = "*** Begin Patch\n*** Add File: a.txt\n+hi\n*** End Patch\n";
    assert_eq!(normalize_apply_patch_delimiters(decorated), expected);
}

#[test]
fn decorated_delimiters_without_operation_line_are_unchanged() {
    let prose = "*** Begin Patch ***\nthis is not a patch body\n*** End Patch ***\n";
    assert_eq!(normalize_apply_patch_delimiters(prose), prose);
}

#[test]
fn decorated_operation_line_without_path_is_unchanged() {
    let dangling = "*** Begin Patch ***\n*** Update File: \n*** End Patch ***\n";
    assert_eq!(normalize_apply_patch_delimiters(dangling), dangling);
}

#[test]
fn incomplete_envelope_is_unchanged() {
    let missing_end = "*** Begin Patch ***\n*** Update File: a.txt\n@@\n-old\n+new\n";
    assert_eq!(
        normalize_apply_patch_delimiters(missing_end),
        missing_end,
        "an envelope without `*** End Patch` must not be rewritten"
    );

    let missing_begin = "*** Update File: a.txt\n@@\n-old\n+new\n*** End Patch ***\n";
    assert_eq!(
        normalize_apply_patch_delimiters(missing_begin),
        missing_begin
    );
}

#[test]
fn text_that_merely_mentions_an_envelope_is_unchanged() {
    let prose =
        "Apply this patch:\n*** Begin Patch ***\n*** Update File: a.txt\n*** End Patch ***\n";
    assert_eq!(normalize_apply_patch_delimiters(prose), prose);

    let trailing_prose = "*** Begin Patch ***\n*** Update File: a.txt\n*** End Patch ***\nDone!\n";
    assert_eq!(
        normalize_apply_patch_delimiters(trailing_prose),
        trailing_prose
    );
}

#[test]
fn trailing_line_break_is_preserved_or_left_absent() {
    let with_break = "*** Begin Patch ***\n*** Delete File: gone.txt\n*** End Patch ***\n";
    assert_eq!(
        normalize_apply_patch_delimiters(with_break),
        "*** Begin Patch\n*** Delete File: gone.txt\n*** End Patch\n"
    );

    let without_break = "*** Begin Patch ***\n*** Delete File: gone.txt\n*** End Patch ***";
    assert_eq!(
        normalize_apply_patch_delimiters(without_break),
        "*** Begin Patch\n*** Delete File: gone.txt\n*** End Patch"
    );

    // One extra trailing break is not part of the anchored envelope.
    let extra_break = "*** Begin Patch ***\n*** Delete File: gone.txt\n*** End Patch ***\n\n";
    assert_eq!(normalize_apply_patch_delimiters(extra_break), extra_break);
}

#[test]
fn crlf_line_breaks_survive_delimiter_repair() {
    let decorated = "*** Begin Patch ***\r\n*** Add File: a.txt\r\n+hi\r\n*** End Patch ***\r\n";
    assert_eq!(
        normalize_apply_patch_delimiters(decorated),
        "*** Begin Patch\r\n*** Add File: a.txt\r\n+hi\r\n*** End Patch\r\n"
    );
}

#[test]
fn one_outer_markdown_fence_is_stripped() {
    let fenced = "```patch\n*** Begin Patch\n*** Add File: a.txt\n+hi\n*** End Patch\n```";
    assert_eq!(
        strip_outer_markdown_code_fence(fenced),
        "*** Begin Patch\n*** Add File: a.txt\n+hi\n*** End Patch"
    );

    // Surrounding whitespace and an empty info string are both fine.
    let padded = "  \n```\nbody line\n```\n  ";
    assert_eq!(strip_outer_markdown_code_fence(padded), "body line");
}

#[test]
fn nested_fence_loses_only_its_outer_pair() {
    let nested = "```\n```patch\ninner\n```\n```";
    assert_eq!(
        strip_outer_markdown_code_fence(nested),
        "```patch\ninner\n```"
    );
}

#[test]
fn unterminated_or_unfenced_text_is_left_alone() {
    let unterminated = "```patch\n*** Begin Patch\n*** End Patch\n";
    assert_eq!(
        strip_outer_markdown_code_fence(unterminated),
        unterminated,
        "a fence that never closes is content, not a wrapper"
    );

    let empty_fence = "```\n```";
    assert_eq!(strip_outer_markdown_code_fence(empty_fence), empty_fence);

    assert_eq!(
        strip_outer_markdown_code_fence(CANONICAL_PATCH),
        CANONICAL_PATCH
    );
}

#[test]
fn input_json_is_unwrapped_with_escapes_round_tripped() {
    let arguments = r#"{"input":"*** Begin Patch\n*** Update File: \u6587\u6863/\u8bf4\u660e.md\n@@\n-old \"quoted\"\n+new\n*** End Patch\n"}"#;
    assert_eq!(
        patch_input_from_function_arguments(arguments),
        Some(
            "*** Begin Patch\n*** Update File: 文档/说明.md\n@@\n-old \"quoted\"\n+new\n*** End Patch\n"
                .to_string()
        )
    );
}

#[test]
fn canonical_input_json_is_unchanged_apart_from_unwrapping() {
    let arguments = serde_json::json!({ "input": CANONICAL_PATCH }).to_string();
    assert_eq!(
        patch_input_from_function_arguments(&arguments),
        Some(CANONICAL_PATCH.to_string())
    );
}

#[test]
fn decorated_and_fenced_input_json_is_fully_repaired() {
    let arguments = serde_json::json!({
        "input": "```patch\n*** Begin Patch ***\n*** Add File: a.txt\n+hi\n*** End Patch ***\n```"
    })
    .to_string();
    assert_eq!(
        patch_input_from_function_arguments(&arguments),
        Some("*** Begin Patch\n*** Add File: a.txt\n+hi\n*** End Patch".to_string())
    );
}

#[test]
fn single_fallback_field_is_accepted() {
    for key in ["patch", "content"] {
        let arguments = serde_json::json!({ key: CANONICAL_PATCH }).to_string();
        assert_eq!(
            patch_input_from_function_arguments(&arguments),
            Some(CANONICAL_PATCH.to_string()),
            "models invent `{key}` for the freeform wrapper field"
        );
    }
}

#[test]
fn ambiguous_or_missing_wrapper_fields_are_not_guessed() {
    let both =
        r#"{"patch":"*** Begin Patch\n*** End Patch","content":"*** Begin Patch\n*** End Patch"}"#;
    assert_eq!(patch_input_from_function_arguments(both), None);

    let unrelated = r#"{"diff":"*** Begin Patch\n*** End Patch"}"#;
    assert_eq!(patch_input_from_function_arguments(unrelated), None);

    let non_string_input = r#"{"input":42}"#;
    assert_eq!(patch_input_from_function_arguments(non_string_input), None);

    let blank_input = r#"{"input":"   "}"#;
    assert_eq!(patch_input_from_function_arguments(blank_input), None);

    assert_eq!(patch_input_from_function_arguments(""), None);
    assert_eq!(patch_input_from_function_arguments("   \n "), None);
}

#[test]
fn non_json_arguments_are_treated_as_the_patch_body() {
    let raw = "*** Begin Patch ***\n*** Add File: a.txt\n+hi\n*** End Patch ***";
    assert_eq!(
        patch_input_from_function_arguments(raw),
        Some("*** Begin Patch\n*** Add File: a.txt\n+hi\n*** End Patch".to_string())
    );

    // The raw-argument fallback trims surrounding whitespace before repairing, which is
    // safe because the trailing break after `*** End Patch` is optional in the grammar.
    assert_eq!(
        patch_input_from_function_arguments(&format!("\n{CANONICAL_PATCH}  ")),
        Some(CANONICAL_PATCH.trim().to_string())
    );

    // JSON that is not an object is not a wrapper, so the text itself is the body.
    assert_eq!(
        patch_input_from_function_arguments("42"),
        Some("42".to_string())
    );
}

#[test]
fn repaired_envelope_is_accepted_by_the_patch_parser() {
    let arguments =
        r#"{"input":"*** Begin Patch ***\n*** Add File: hello.txt\n+hello\n*** End Patch ***"}"#;
    let repaired = patch_input_from_function_arguments(arguments).expect("recovered patch text");
    let args = crate::parse_patch(&repaired).expect("repaired envelope must parse");
    assert_eq!(args.patch, repaired);
    assert_eq!(args.hunks.len(), 1);
}
