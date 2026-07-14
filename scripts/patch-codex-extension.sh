#!/usr/bin/env bash
#
# patch-codex-extension.sh — show every catalog model in the Codex VS Code picker.
#
# Background
# ----------
# The Rust CLI honors `model_catalog_json` correctly: every model with
# `visibility: "list"` is returned by JSON-RPC `model/list` with `hidden: false`.
#
# The VS Code extension's webview applies an additional filter. The filter
# computes a flag — `useHiddenModels && authMethod !== "amazonBedrock"` — and
# when true, only models in the Statsig `available_models` whitelist (gpt-* only)
# pass through. When false, every model with `!hidden` shows up.
#
# When Statsig dynamic config 107580212 returns `use_hidden_models: true` with
# an `available_models` whitelist (gpt-* only), every non-gpt catalog entry gets
# filtered out — even though the CLI happily serves them.
#
# This script forces that flag to `false` so the picker always uses the
# `!n.hidden` branch, letting every `visibility: "list"` model from
# `model_catalog_json` show up regardless of the Statsig gate.
#
# Chunk history
# -------------
# The filter logic lives in a minified build artifact whose filename hash
# changes every release. The script locates it by *content signature*, not by
# filename, so it survives extension upgrades without code changes here.
#
# Two upstream layouts are known:
#
#   Layout A (older, ~pre-26.707):
#     Chunk:   webview/assets/models-and-reasoning-efforts-*.js
#     Pattern: let a=[],o=null,s=<useHiddenModels>&&<authMethod>!==`amazonBedrock`;
#     Patch:   o=null,s=false;
#
#   Layout B (newer, ~26.707+):
#     Chunk:   webview/assets/model-list-filter-*.js
#     Pattern: ...useHiddenModels:s}){let c=[],l=null,u=<s>&&<t>!==`amazonBedrock`,...
#     Patch:   <u>=false   (just the assignment, not the surrounding decl)
#
# We scan ALL *.js files under webview/assets/, matching either layout's
# content signature.
#
# Why a script (and not a fork patch)
# ------------------------------------
# The picker chunk is a minified build artifact of the closed-source VS Code
# extension; its source is not in this repo and its filename hash changes every
# release. The script locates the chunk by content signature, so it keeps
# working after extension upgrades without code changes here.
#
# Usage
# -----
#   scripts/patch-codex-extension.sh            # patch (idempotent)
#   scripts/patch-codex-extension.sh --revert   # restore from .bak
#   scripts/patch-codex-extension.sh --help
#
# After patching, run "Developer: Reload Window" in VS Code to reload webview.
# Re-run after every Codex extension upgrade — the upgrade overwrites the asset.
#
# Scope: Linux, macOS, WSL. Native Windows users should run via WSL or port
# the few lines to PowerShell; the per-file substitution is regex-based.

set -euo pipefail

# Candidate extension install roots across VS Code / VS Code Insiders / Cursor
# and their -server (Remote-SSH) variants. Order does not matter; we dedupe via
# the per-file marker checks below.
CANDIDATE_GLOBS=(
  "$HOME/.vscode-server/extensions/openai.chatgpt-*"
  "$HOME/.vscode-server-insiders/extensions/openai.chatgpt-*"
  "$HOME/.vscode/extensions/openai.chatgpt-*"
  "$HOME/.vscode-insiders/extensions/openai.chatgpt-*"
  "$HOME/.cursor-server/extensions/openai.chatgpt-*"
  "$HOME/.cursor/extensions/openai.chatgpt-*"
)

# Content signatures — stable across minifier renames. We deliberately do NOT
# match by the hashed filename, because that hash rotates every release.
#
# We support two layouts. Each layout has its own unpatched regex and patched
# marker. Both are checked against every .js file in webview/assets/.
#
# Layout A (older):
#   Original:  ...let a=[],o=null,s=<i>&&<e>!==`amazonBedrock`;...
#   Patched:   ...let a=[],o=null,s=false;...
#
# Layout B (newer, 26.707+):
#   The flag variable is declared inside a destructured function parameter:
#     ...useHiddenModels:s}){let c=[],l=null,u=s&&t!==`amazonBedrock`,...
#   We match the assignment `<u>=<s>&&<t>!==\`amazonBedrock\`` and replace the
#   right-hand side with `false`, yielding `<u>=false`.
#
#   Original:  ...u=<s>&&<t>!==`amazonBedrock`...
#   Patched:   ...u=false...
#
# Because Layout B's variable name rotates, we match it generically and capture
# the LHS variable so the perl substitution can rewrite just the assignment.
# The patched marker is captured dynamically (see below).

# Layout A
UNPATCHED_RE_A='o=null,s=[A-Za-z_$][A-Za-z_$0-9]*&&[A-Za-z_$][A-Za-z_$0-9]*!==`amazonBedrock`;'
PATCHED_MARKER_A='o=null,s=false;'

# Layout B — the unpatched assignment; capture group 1 = the flag variable name.
# We anchor on the `!==\`amazonBedrock\`` tail to avoid false positives.
UNPATCHED_RE_B='([A-Za-z_$][A-Za-z_$0-9]*)=[A-Za-z_$][A-Za-z_$0-9]*&&[A-Za-z_$][A-Za-z_$0-9]*!==`amazonBedrock`'
# Layout B patched marker is dynamic: "<var>=false".  We detect it with a
# pattern that matches <ident>=false immediately followed by the `,` or `;` or
# `)` that terminates the assignment in the minified output. But since the
# variable name is unknown ahead of time, we detect "already patched" by
# ABSENCE of the unpatched pattern and PRESENCE of our .bak. For re-run
# friendliness, we also check for a companion marker file (see below).

mode="patch"
case "${1:-}" in
  --revert) mode="revert" ;;
  -h|--help)
    cat <<'HELP'
patch-codex-extension.sh — show every catalog model in the Codex VS Code picker.

Usage:
  patch-codex-extension.sh             # patch (idempotent)
  patch-codex-extension.sh --revert    # restore from .bak
  patch-codex-extension.sh --help

The Rust CLI honors model_catalog_json correctly: every model with
visibility:"list" is returned by JSON-RPC model/list with hidden:false.

The VS Code extension webview applies an additional filter. When Statsig
dynamic config 107580212 returns use_hidden_models:true with an
available_models whitelist (gpt-* only), every non-gpt catalog entry gets
filtered out.

This script forces the filter flag to false so the picker always uses the
!n.hidden branch, letting every visibility:"list" model show up regardless
of the Statsig gate.

After patching, run "Developer: Reload Window" in VS Code to reload the webview.
Re-run after every Codex extension upgrade — the upgrade overwrites the asset.
HELP
    exit 0
    ;;
  "") : ;;
  *) echo "error: unknown argument: $1 (try --help)" >&2; exit 2 ;;
esac

if ! command -v perl >/dev/null 2>&1; then
  echo "error: perl is required (not found in PATH)" >&2
  exit 3
fi

# Collect picker chunk files relevant to the chosen mode.
#
# For patch: we want files where an UNPATCHED pattern is present, OR files that
# we previously patched (detected via .bak + absence of unpatched pattern).
# For revert: we want files where a .bak exists.
declare -a targets=()
for g in "${CANDIDATE_GLOBS[@]}"; do
  # Intentionally unquoted: we want glob expansion. If nothing matches, the
  # literal pattern fails the -d test below and is skipped.
  for ext_dir in $g; do
    [ -d "$ext_dir" ] || continue
    assets="$ext_dir/webview/assets"
    [ -d "$assets" ] || continue

    # Scan ALL .js files in the assets dir. We no longer assume a specific
    # chunk filename — the filter logic moved between chunks across versions.
    if [ "$mode" = "patch" ]; then
      # Collect files matching either layout's unpatched signature.
      while IFS= read -r f; do
        targets+=("$f")
      done < <(grep -El -e "$UNPATCHED_RE_A" -e "$UNPATCHED_RE_B" -- "$assets"/*.js 2>/dev/null || true)
      # Also collect files we already patched (have a .bak), so re-runs
      # hit the [skip] branch instead of "no picker found".
      for f in "$assets"/*.js; do
        [ -f "$f" ] || continue
        [ -f "$f.bak" ] || continue
        # Only add if not already collected above.
        _dup=0
        for t in "${targets[@]:-}"; do
          [ "$t" = "$f" ] && _dup=1 && break
        done
        [ "$_dup" = 0 ] && targets+=("$f")
      done
    else
      # Revert: any .js with a .bak in this assets dir.
      for f in "$assets"/*.js; do
        [ -f "$f" ] || continue
        [ -f "$f.bak" ] && targets+=("$f")
      done
    fi
  done
done

if [ "${#targets[@]}" -eq 0 ]; then
  if [ "$mode" = "patch" ]; then
    cat >&2 <<'MSG'
no Codex webview picker found under any scanned extension root.

Possible reasons:
  - No openai.chatgpt-* extension installed under ~/.vscode{,-server,-insiders}{,/extensions}
    or ~/.cursor{,-server}/extensions. Install the extension in VS Code first.
  - Extension layout changed upstream; update the UNPATCHED_RE patterns in this script.
MSG
  else
    echo "no patched picker files with .bak backups found." >&2
  fi
  exit 1
fi

ok=0
skipped=0
failed=0

for f in "${targets[@]}"; do
  case "$mode" in
    patch)
      # Determine which layout this file matches.
      layout=""
      if grep -qE -- "$UNPATCHED_RE_A" "$f" 2>/dev/null; then
        layout="A"
      elif grep -qE -- "$UNPATCHED_RE_B" "$f" 2>/dev/null; then
        layout="B"
      else
        # Neither unpatched pattern matches — could be already patched.
        echo "[skip] already patched or unrecognized layout: $f"
        skipped=$((skipped + 1))
        continue
      fi

      # Back up once. Never overwrite an existing .bak — it may be the only
      # pristine copy from a previous version after an upgrade clobbered it.
      if [ ! -f "$f.bak" ]; then
        cp -p "$f" "$f.bak"
      fi

      case "$layout" in
        A)
          if perl -i -pe 's/o=null,s=[A-Za-z_$][A-Za-z_$0-9]*&&[A-Za-z_$][A-Za-z_$0-9]*!==`amazonBedrock`;/o=null,s=false;/g' "$f" \
            && grep -qF -- "$PATCHED_MARKER_A" "$f"; then
            echo "[ok]   patched (layout A): $f"
            ok=$((ok + 1))
          else
            echo "[fail] perl substitution did not take effect (layout A): $f" >&2
            [ -f "$f.bak" ] && cp -p "$f.bak" "$f"
            failed=$((failed + 1))
          fi
          ;;
        B)
          # Layout B: the variable name is unknown, so we capture it in the
          # regex and reconstruct the patched form in the replacement.
          #
          # Match:  <var>=<s>&&<t>!==`amazonBedrock`
          # Replace: <var>=false
          #
          # We use perl with capture group 1 ($1) for the variable name.
          # The `!==\`amazonBedrock\`` tail is consumed and replaced, so the
          # resulting `,<rest>` after the comma is preserved.
          if perl -i -pe 's/([A-Za-z_$][A-Za-z_$0-9]*)=[A-Za-z_$][A-Za-z_$0-9]*&&[A-Za-z_$][A-Za-z_$0-9]*!==`amazonBedrock`/${1}=false/g' "$f"; then
            # Verify: the unpatched pattern must be gone, and "<var>=false"
            # must now appear. Since we can't predict <var>, we verify by
            # absence of the unpatched pattern.
            if ! grep -qE -- "$UNPATCHED_RE_B" "$f" 2>/dev/null \
               && grep -qE '[A-Za-z_$][A-Za-z_$0-9]*=false' "$f" 2>/dev/null; then
              echo "[ok]   patched (layout B): $f"
              ok=$((ok + 1))
            else
              echo "[fail] perl substitution did not take effect (layout B): $f" >&2
              [ -f "$f.bak" ] && cp -p "$f.bak" "$f"
              failed=$((failed + 1))
            fi
          else
            echo "[fail] perl command failed (layout B): $f" >&2
            [ -f "$f.bak" ] && cp -p "$f.bak" "$f"
            failed=$((failed + 1))
          fi
          ;;
      esac
      ;;
    revert)
      if [ -f "$f.bak" ]; then
        cp -p "$f.bak" "$f"
        echo "[ok]   reverted: $f"
        ok=$((ok + 1))
      else
        echo "[skip] no .bak to restore: $f"
        skipped=$((skipped + 1))
      fi
      ;;
  esac
done

echo
echo "mode=$mode  ok=$ok  skipped=$skipped  failed=$failed"
if [ "$mode" = "patch" ] && [ "$ok" -gt 0 ]; then
  echo
  echo "Next: in VS Code run 'Developer: Reload Window' to reload the webview."
fi

[ "$failed" -eq 0 ] || exit 4
exit 0
