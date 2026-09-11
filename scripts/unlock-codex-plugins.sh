#!/bin/bash
#
# unlock-codex-plugins.sh
# ------------------------
# Unlock ALL official plugin marketplaces in Codex macOS desktop,
# regardless of login method (API key vs ChatGPT login).
#
# What it patches: the webview filter that hides `openai-curated` and
# `openai-curated-remote` when you sign in with an API key. After patching,
# `h` (the hidden-marketplace list) stays empty, so every marketplace shows.
#
# Signature: by default this only clears com.apple.quarantine and leaves the
# app's Developer ID signature alone. Replacing app.asar does break the bundle
# seal, but nothing on the launch path checks it, whereas an ad-hoc re-sign
# swaps the main executable's identity and invalidates Keychain ACLs / TCC
# grants / update checks that are bound to it. Pass --resign (or set
# CODEX_RESIGN=1) if Gatekeeper actually blocks launch.
#
# Idempotent: if already patched it does NOTHING (no repack, no re-sign),
# so re-running is 100% safe and never triggers Keychain prompts.
# Re-run it once after every Codex auto-update.
#
# Usage:
#   unlock-codex-plugins.sh             # patch (idempotent)
#   unlock-codex-plugins.sh --resign    # additionally ad-hoc re-sign the bundle
#   unlock-codex-plugins.sh --help
#
set -euo pipefail

RESIGN=false
[ -n "${CODEX_RESIGN:-}" ] && RESIGN=true
RESIGNED=false
while [ $# -gt 0 ]; do
    case "$1" in
        --resign) RESIGN=true ;;
        -h|--help)
            cat <<'HELP'
unlock-codex-plugins.sh — unlock all official plugin marketplaces in Codex macOS desktop

Usage:
  unlock-codex-plugins.sh             # patch (idempotent)
  unlock-codex-plugins.sh --resign    # additionally ad-hoc re-sign the bundle
  unlock-codex-plugins.sh --help

Environment: CODEX_APP (default: auto-detect ChatGPT.app, fall back to Codex.app),
             CODEX_RESIGN=1 (same as --resign)

By default the app's Developer ID signature is left untouched: only
com.apple.quarantine is cleared. Add --resign if Gatekeeper blocks launch.
HELP
            exit 0 ;;
        *) echo "ERROR: unknown argument: $1 (try --help)" >&2; exit 2 ;;
    esac
    shift
done

# ---------- locate the desktop .app ----------
# OpenAI renamed the desktop app from Codex.app to ChatGPT.app (2025-07). Both
# share the same Contents/Resources/app.asar layout and the same webview
# filter, so prefer the new name and fall back to the old one. CODEX_APP
# overrides the lookup.
APP="${CODEX_APP:-}"
if [ -z "$APP" ]; then
    for candidate in \
        "/Applications/ChatGPT.app" \
        "/Applications/Codex.app" \
        "$HOME/Applications/ChatGPT.app" \
        "$HOME/Applications/Codex.app"; do
        if [ -d "$candidate" ]; then APP="$candidate"; break; fi
    done
fi
ASAR="$APP/Contents/Resources/app.asar"
ASAR_BAK="$APP/Contents/Resources/app.asar.bak"
TMPDIR=$(mktemp -d /tmp/codex-plugin-unlock.XXXXXX)
EXTRACTED="$TMPDIR/extracted"
PATCHED="$TMPDIR/app-patched.asar"

cleanup() { rm -rf "$TMPDIR"; }
trap cleanup EXIT

# ---------- helpers ----------
green() { printf "\033[32m%s\033[0m\n" "$1"; }
red()   { printf "\033[31m%s\033[0m\n" "$1"; }
dim()   { printf "\033[2m%s\033[0m\n" "$1"; }

# Is the bundle already ad-hoc signed? Deliberately avoids a pipeline: grep -q
# exits early on a match, and pipefail would then report failure.
app_is_adhoc_signed() {
    local info
    info="$(codesign -dvv "$APP" 2>&1 || true)"
    [[ "$info" == *$'\n'Signature=adhoc* || "$info" == Signature=adhoc* ]]
}

# ---------- preflight ----------
if [ -z "$APP" ] || [ ! -d "$APP" ]; then
    red "ERROR: ChatGPT.app / Codex.app not found under /Applications or \$HOME/Applications."
    red "       set CODEX_APP=/path/to/ChatGPT.app to override."
    exit 1
fi
[ -f "$ASAR" ] || { red "ERROR: $ASAR not found."; exit 1; }

# ---------- auto-install @electron/asar if missing ----------
if ! command -v asar >/dev/null 2>&1; then
    green "==> 'asar' CLI not found. Installing @electron/asar..."
    if ! command -v npm >/dev/null 2>&1; then
        red "ERROR: npm not found. Install Node.js first, or run: npm install -g @electron/asar"
        exit 1
    fi
    npm install -g @electron/asar
    command -v asar >/dev/null 2>&1 || { red "ERROR: asar install failed."; exit 1; }
    green "==> @electron/asar installed."
fi

# ---------- extract ----------
green "==> Extracting app.asar..."
mkdir -p "$EXTRACTED"
asar extract "$ASAR" "$EXTRACTED"

# ---------- locate the plugin marketplace file ----------
# Stable string literals (NOT minified) → survive rebuilds / hash renames.
PLUGIN_FILE=$(grep -rlE 'list-plugins' "$EXTRACTED"/webview/assets/*.js 2>/dev/null \
    | xargs grep -l 'curated-marketplace' 2>/dev/null | head -1)

if [ -z "$PLUGIN_FILE" ]; then
    red "ERROR: plugin marketplace chunk not found (bundle layout changed?). Aborting."
    exit 1
fi
dim "    target: $(basename "$PLUGIN_FILE")"

# ---------- detection (variable-agnostic, backreference-anchored) ----------
# Unpatched signature:  authMethod??null),<arrVar>=<default>;<cond>?<aVar>=<val>:<cond2>&&(<aVar>=<val2>);
# Patched   signature:  authMethod??null),<arrVar>=<default>;0;
NEEDS_PATCH_SIG='authMethod\?\?null\),[A-Za-z_$][A-Za-z_$0-9]*=[A-Za-z_$][A-Za-z_$0-9]*;[A-Za-z_$][A-Za-z_$0-9]*\?[A-Za-z_$][A-Za-z_$0-9]*=[A-Za-z_$][A-Za-z_$0-9]*:[A-Za-z_$][A-Za-z_$0-9]*&&\([A-Za-z_$][A-Za-z_$0-9]*=[A-Za-z_$][A-Za-z_$0-9]*\);'
ALREADY_PATCHED_SIG='authMethod\?\?null\),[A-Za-z_$][A-Za-z_$0-9]*=[A-Za-z_$][A-Za-z_$0-9]*;0;'

if grep -qE "$ALREADY_PATCHED_SIG" "$PLUGIN_FILE"; then
    green "==> Already patched. Nothing to do — re-running is always safe."
    exit 0
fi

if ! grep -qE "$NEEDS_PATCH_SIG" "$PLUGIN_FILE"; then
    red "ERROR: unpatched signature not found either. Bundle structure changed — manual review needed."
    exit 1
fi

# ---------- apply patch ----------
green "==> Patching: neutralize the hidden-marketplace assignment..."
cp -p "$PLUGIN_FILE" "$PLUGIN_FILE.bak"

perl -i -pe \
    's/(authMethod\?\?null\),[A-Za-z_$][A-Za-z_$0-9]*=[A-Za-z_$][A-Za-z_$0-9]*;)[A-Za-z_$][A-Za-z_$0-9]*\?[A-Za-z_$][A-Za-z_$0-9]*=[A-Za-z_$][A-Za-z_$0-9]*:[A-Za-z_$][A-Za-z_$0-9]*&&\([A-Za-z_$][A-Za-z_$0-9]*=[A-Za-z_$][A-Za-z_$0-9]*\);/${1}0;/g' \
    "$PLUGIN_FILE"

grep -qE "$ALREADY_PATCHED_SIG" "$PLUGIN_FILE" \
    || { red "ERROR: patch did not apply correctly."; cp -p "$PLUGIN_FILE.bak" "$PLUGIN_FILE"; exit 1; }
green "==> Patch verified in extracted files."

# ---------- repack ----------
green "==> Repacking asar..."
( cd "$EXTRACTED" && asar pack . "$PATCHED" )

# ---------- verify repacked asar ----------
VERIFY_DIR="$TMPDIR/verify"
mkdir -p "$VERIFY_DIR"
asar extract "$PATCHED" "$VERIFY_DIR" >/dev/null 2>&1
if ! find "$VERIFY_DIR" -name "use-plugins-*.js" -exec grep -lE "$ALREADY_PATCHED_SIG" {} \; | grep -q .; then
    red "ERROR: patch missing in repacked asar. Aborting (original untouched)."
    exit 1
fi
green "==> Repacked asar verified."

# ---------- backup (first time only) ----------
if [ ! -f "$ASAR_BAK" ]; then
    sudo cp -p "$ASAR" "$ASAR_BAK"
    green "==> Original backed up to app.asar.bak"
else
    dim "    app.asar.bak already exists — keeping the first backup."
fi

# ---------- replace + clear quarantine ----------
green "==> Replacing app.asar..."
sudo cp "$PATCHED" "$ASAR"

green "==> Clearing com.apple.quarantine..."
sudo xattr -dr com.apple.quarantine "$APP" 2>/dev/null || true

if [ "$RESIGN" != true ] && ! app_is_adhoc_signed; then
    green "==> Keeping the app's original signature; not re-signing (--resign to force)"
else
    green "==> Re-signing app (ad-hoc)..."
    sudo codesign --force --deep --sign - "$APP" 2>/dev/null \
        || { red "WARNING: codesign failed. Run manually:\n  sudo codesign --force --deep --sign - '$APP'"; }
    RESIGNED=true
fi

echo ""
green "================ DONE ================"
green "  Plugin marketplaces unlocked."
green "  Restart Codex desktop. With an API key"
green "  you should now see openai-curated /
  openai-curated-remote marketplaces too."
dim ""
dim "  Rollback: sudo cp '$ASAR_BAK' '$ASAR'"
if [ "$RESIGNED" = true ]; then
    dim "            (then re-sign: sudo codesign --force --deep --sign - '$APP')"
    dim "  Note: first launch after this patch may ask for"
    dim "  Keychain access once (due to the ad-hoc re-sign)."
    dim "  Click 'Always Allow' or re-login once to silence it."
else
    dim "            (no re-sign needed: original signature kept)"
    dim "  Note: the app's Developer ID signature was left"
    dim "  untouched, so Keychain/TCC grants keep working."
fi
