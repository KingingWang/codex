//! Representation repair for `apply_patch` tool payloads.
//!
//! Models that must JSON-escape a patch body reliably make representation mistakes:
//! they decorate the outer delimiter lines as `*** Begin Patch ***`, wrap the whole
//! envelope in one Markdown code fence, or invent a field name for the wrapper object.
//! Each repair here fixes exactly one such wrapper mistake that has a single faithful
//! reading, and leaves genuine patch content byte-exact. Anything ambiguous is returned
//! unchanged (or rejected) so the parser produces the error instead of a guess.

/// Field names a function-shaped `apply_patch` call may use besides the canonical `input`.
const PATCH_FALLBACK_KEYS: [&str; 2] = ["patch", "content"];
/// Canonical first line of a patch envelope.
const PATCH_BEGIN: &str = "*** Begin Patch";
/// Canonical last line of a patch envelope.
const PATCH_END: &str = "*** End Patch";
/// Decoration some models append to the outer delimiter lines, e.g. `*** Begin Patch ***`.
const DECORATED_SUFFIX: &str = " ***";
/// The decorated form of [`PATCH_END`], spelled out so the suffix peel stays readable.
const PATCH_END_DECORATED: &str = "*** End Patch ***";
/// Markdown fence used by models that wrap a patch body in a code block.
const CODE_FENCE: &str = "```";
/// Operation headers proving a body is a real patch rather than prose mentioning one.
const OPERATION_PREFIXES: [&str; 3] = ["*** Add File: ", "*** Update File: ", "*** Delete File: "];

/// Peels one trailing `\r?\n` from `text`, reporting which break it was.
fn peel_trailing_break(text: &str) -> Option<(&'static str, &str)> {
    let before_break = text.strip_suffix('\n')?;
    match before_break.strip_suffix('\r') {
        Some(core) => Some(("\r\n", core)),
        None => Some(("\n", before_break)),
    }
}

/// Repairs decorated outer patch delimiters, leaving every other byte untouched.
///
/// Some routed models emit `*** Begin Patch ***` / `*** End Patch ***`, which the patch
/// parser rejects even though the body is a perfectly valid patch. That is an
/// unambiguous wrapper mistake: a trailing ` ***` on an outer delimiter line carries no
/// meaning in the patch grammar, so exactly one faithful reading exists.
///
/// The repair is deliberately narrow. It fires only when the WHOLE text is one complete
/// envelope (anchored at both ends) whose body carries at least one real
/// `*** Add|Update|Delete File: <path>` operation line. Incomplete envelopes, interior
/// content that happens to mention a delimiter, and text that is not a patch at all are
/// returned byte-exact, as is an already canonical envelope. Line breaks, including
/// `\r\n`, are preserved as found.
pub fn normalize_apply_patch_delimiters(text: &str) -> String {
    let Some(after_begin) = text.strip_prefix(PATCH_BEGIN) else {
        return text.to_string();
    };
    let (begin_decorated, after_begin) = match after_begin.strip_prefix(DECORATED_SUFFIX) {
        Some(rest) => (true, rest),
        None => (false, after_begin),
    };
    // Exactly one mandatory line break separates the begin line from the body.
    let begin_break = match after_begin.strip_prefix("\r\n") {
        Some(rest) => Some(("\r\n", rest)),
        None => after_begin.strip_prefix('\n').map(|rest| ("\n", rest)),
    };
    let Some((begin_break, rest)) = begin_break else {
        return text.to_string();
    };

    // The end delimiter is the last line, so peel from the end: an optional trailing
    // break, the end line itself, then the mandatory break separating it from the body.
    let (rest, trailing_break) = match peel_trailing_break(rest) {
        Some((line_break, core)) => (core, line_break),
        None => (rest, ""),
    };
    let (rest, end_decorated) = if let Some(core) = rest.strip_suffix(PATCH_END_DECORATED) {
        (core, true)
    } else if let Some(core) = rest.strip_suffix(PATCH_END) {
        (core, false)
    } else {
        return text.to_string();
    };
    let Some((end_break, body)) = peel_trailing_break(rest) else {
        return text.to_string();
    };

    let has_operation_line = body.lines().any(|line| {
        OPERATION_PREFIXES.iter().any(|prefix| {
            line.strip_prefix(prefix)
                .is_some_and(|path| !path.is_empty())
        })
    });
    if !has_operation_line || (!begin_decorated && !end_decorated) {
        return text.to_string();
    }
    format!("{PATCH_BEGIN}{begin_break}{body}{end_break}{PATCH_END}{trailing_break}")
}

/// Strips ONE complete outer Markdown code fence, leaving everything else untouched.
///
/// A model that wraps the whole patch in a ```` ```patch ```` code block has made one
/// wrapper mistake with one faithful reading: the fenced block's content. The trimmed
/// text must be a single complete fence - an opening ```` ``` ```` with an optional info
/// string, one break, content, one break, and a closing ```` ``` ```` at the very end -
/// otherwise the input is returned unchanged. Nested fences lose only their outermost
/// pair, and unterminated fences are left alone.
pub fn strip_outer_markdown_code_fence(text: &str) -> String {
    let trimmed = text.trim();
    let Some(after_open) = trimmed.strip_prefix(CODE_FENCE) else {
        return text.to_string();
    };
    // An info string cannot contain a break, so the opening fence ends at the first one.
    let Some(info_end) = after_open.find(['\n', '\r']) else {
        return text.to_string();
    };
    let after_info = &after_open[info_end..];
    let Some(after_open_break) = after_info
        .strip_prefix("\r\n")
        .or_else(|| after_info.strip_prefix('\n'))
    else {
        return text.to_string();
    };
    let inner_start = CODE_FENCE.len() + info_end + (after_info.len() - after_open_break.len());
    let Some(without_close_fence) = trimmed.strip_suffix(CODE_FENCE) else {
        return text.to_string();
    };
    let Some((_, body_region)) = peel_trailing_break(without_close_fence) else {
        return text.to_string();
    };
    if body_region.len() < inner_start {
        return text.to_string();
    }
    body_region[inner_start..].to_string()
}

/// Recovers the patch text from a function-shaped `apply_patch` call's arguments.
///
/// On wire protocols without freeform tools the model must send `{"input": "<patch>"}`.
/// When that strict shape fails to parse, this recovers the one faithful reading of what
/// the model meant: the canonical `input` string, or a single invented wrapper field
/// (`patch` / `content`), or - when the arguments are not a JSON object at all - the raw
/// trimmed string, which is already the patch body. The recovered text then gets one
/// outer Markdown fence stripped and its outer delimiters repaired.
///
/// Returns `None` when there is no usable string: the arguments carry no patch text, or
/// they are an object whose patch field is ambiguous (both fallback names present) or
/// absent. Refusing to guess keeps the failure with the model, which can resend a valid
/// call, instead of applying a patch nobody wrote.
pub fn patch_input_from_function_arguments(arguments: &str) -> Option<String> {
    let trimmed = arguments.trim();
    let parsed = serde_json::from_str::<serde_json::Value>(trimmed);
    let body = match &parsed {
        Ok(serde_json::Value::Object(map)) => {
            if let Some(input) = map.get("input").and_then(serde_json::Value::as_str) {
                input
            } else {
                let mut candidates = PATCH_FALLBACK_KEYS
                    .into_iter()
                    .filter_map(|key| map.get(key).and_then(serde_json::Value::as_str));
                let candidate = candidates.next()?;
                // Two invented field names is ambiguous; do not guess which is the patch.
                if candidates.next().is_some() {
                    return None;
                }
                candidate
            }
        }
        // Not JSON, or JSON that is not an object: the whole string is the patch body.
        _ => trimmed,
    };
    let unfenced = strip_outer_markdown_code_fence(body);
    let repaired = normalize_apply_patch_delimiters(&unfenced);
    (!repaired.trim().is_empty()).then_some(repaired)
}

#[cfg(test)]
#[path = "envelope_repair_tests.rs"]
mod tests;
