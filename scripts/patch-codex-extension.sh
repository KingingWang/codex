#!/usr/bin/env bash
#
# patch-codex-extension.sh — show every catalog model in the Codex VS Code picker.
#
# Background
# ----------
# The Rust CLI honors `model_catalog_json` correctly: every model with
# `visibility: "list"` is returned by JSON-RPC `model/list` with `hidden: false`.
#
# The VS Code extension's webview applies an additional filter in the picker
# chunk (`webview/assets/models-and-reasoning-efforts-*.js`, hashed filename):
#
#     let a=[],o=null,s=i&&e!==`amazonBedrock`;   // i = useHiddenModels
#     r.forEach(n => {
#       if (s ? availableModels.has(n.model) : !n.hidden) { ... push to picker ... }
#     });
#
# When Statsig dynamic config 107580212 returns `use_hidden_models: true` with an
# `available_models` whitelist (gpt-* only), every non-gpt catalog entry gets
# filtered out — even though the CLI happily serves them.
#
# This script forces `s=false` so the picker always uses the `!n.hidden` branch,
# letting every `visibility: "list"` model from `model_catalog_json` show up
# regardless of the Statsig gate.
#
# Why a script (and not a fork patch)
# -----------------------------------
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
# match by the hashed filename (`models-and-reasoning-efforts-DYpBSExA.js`),
# because that hash rotates every release.
#
#   Original:  ...let a=[],o=null,s=<i>&&<e>!==`amazonBedrock`;...
#   Patched:   ...let a=[],o=null,s=false;...
UNPATCHED_RE='o=null,s=[A-Za-z_$][A-Za-z_$0-9]*&&[A-Za-z_$][A-Za-z_$0-9]*!==`amazonBedrock`;'
PATCHED_MARKER='o=null,s=false;'

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

The VS Code extension webview applies an additional filter in the picker chunk
(webview/assets/models-and-reasoning-efforts-*.js, hashed filename). When
Statsig dynamic config 107580212 returns use_hidden_models:true with an
available_models whitelist (gpt-* only), every non-gpt catalog entry gets
filtered out.

This script forces s=false so the picker always uses the !n.hidden branch,
letting every visibility:"list" model show up regardless of the Statsig gate.

After patching, run "Developer: Reload Window" in VS Code to reload webview.
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
# For patch: we want files where the UNPATCHED pattern is present.
# For revert: we want files where the PATCHED marker is present and a .bak exists.
declare -a targets=()
for g in "${CANDIDATE_GLOBS[@]}"; do
  # Intentionally unquoted: we want glob expansion. If nothing matches, the
  # literal pattern fails the -d test below and is skipped.
  for ext_dir in $g; do
    [ -d "$ext_dir" ] || continue
    assets="$ext_dir/webview/assets"
    [ -d "$assets" ] || continue

    # Collect candidate files. In patch mode we look for EITHER the unpatched
    # signature or our own patched marker, so re-runs hit the explicit [skip]
    # branch with a clear message instead of "no picker found".
    if [ "$mode" = "patch" ]; then
      while IFS= read -r f; do
        targets+=("$f")
      done < <(grep -El -e "$UNPATCHED_RE" -e "$PATCHED_MARKER" -- "$assets"/*.js 2>/dev/null || true)
    else
      while IFS= read -r f; do
        [ -f "$f.bak" ] && targets+=("$f")
      done < <(grep -Fl -- "$PATCHED_MARKER" "$assets"/*.js 2>/dev/null || true)
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
  - Extension layout changed upstream; update CANDIDATE_GLOBS in this script.
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
      if grep -qF -- "$PATCHED_MARKER" "$f"; then
        echo "[skip] already patched: $f"
        skipped=$((skipped + 1))
        continue
      fi
      # Back up once. Never overwrite an existing .bak — it may be the only
      # pristine copy from a previous version after an upgrade clobbered it.
      if [ ! -f "$f.bak" ]; then
        cp -p "$f" "$f.bak"
      fi
      if perl -i -pe 's/o=null,s=[A-Za-z_$][A-Za-z_$0-9]*&&[A-Za-z_$][A-Za-z_$0-9]*!==`amazonBedrock`;/o=null,s=false;/g' "$f" \
        && grep -qF -- "$PATCHED_MARKER" "$f"; then
        echo "[ok]   patched: $f"
        ok=$((ok + 1))
      else
        echo "[fail] perl substitution did not take effect: $f" >&2
        # Restore pristine content if we created the backup this run.
        [ -f "$f.bak" ] && cp -p "$f.bak" "$f"
        failed=$((failed + 1))
      fi
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
