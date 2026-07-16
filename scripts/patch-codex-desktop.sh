#!/usr/bin/env bash
#
# patch-codex-desktop.sh — 让 macOS 版 Codex 桌面端显示 model_catalog_json 里的全部模型
#
# 背景
# -----
# 官方 Codex CLI 会正确读取 `~/.codex/config.toml` 里的 `model_catalog_json`，
# 通过 JSON-RPC `model/list` 返回的每个模型都是 `hidden:false`。
#
# 但桌面端 UI 是 Electron 渲染的 webview，模型选择器在 JS 端做了一次二次过滤：
#
#     // webview picker chunk（minified，hash 文件名每次发版都会变）
#     // 旧版本（~v0.5x）：
#     //   o=null,s=useHiddenModels&&authMethod!==`amazonBedrock`;
#     //   if (s) { availableModels.has(n.model) } else { !n.hidden }
#     //
#     // 新版本（v0.60+）：
#     //   c=null,l=useHiddenModels&&authMethod!==`amazonBedrock`,u=...
#     //   if (l) { t.has(n.model) } else { !n.hidden }
#     //
#     // 当 Statsig 动态配置命中（生产环境会命中），返回的
#     // {use_hidden_models:true, available_models:[gpt-*...]} 让过滤
#     // 走白名单分支，所有非 gpt 模型都被前端挡掉。
#
# 这个脚本把那个过滤标志变量强制设成 `false`，过滤就永远走 `!n.hidden` 分支，
# 让所有 `visibility:"list"` 的 catalog 模型都显示。
#
# 用法
# -----
#   scripts/patch-codex-desktop.sh             # patch（幂等，可重复运行）
#   scripts/patch-codex-desktop.sh --revert    # 从 .bak 还原
#   scripts/patch-codex-desktop.sh --help
#
# 需要：
#   - macOS（脚本会用 codesign、xattr、pgrep）
#   - asar 命令，或 npx（自动拉 @electron/asar）
#   - sudo 写权限（写入 /Applications 或 ~/Applications 下的 .app）
#
# 重要：
#   - 每次 Codex 桌面端自动更新都会覆盖被 patch 的 app.asar；升级后重跑一次即可
#   - 改 .app 内容会让原 notarization 失效；脚本会用 ad-hoc 重签 + 清 quarantine
#   - 如果 .app 自带 privileged helper / 嵌套 framework，ad-hoc 签可能签不全，
#     脚本会把 codesign 的 stderr 显示出来让你看
#   - 替换 app.asar 之前必须退出 Codex，脚本会先 pgrep 检查
#
# 退出码：
#   0  成功（含幂等 skip）
#   1  一般错误（找不到目标、找不到 .app、Codex 在跑、verify 失败 等）
#   2  参数错误
#   3  缺依赖（asar/npx 都没有）
#   4  patch 已应用但没找到 .bak（说明上次 install 中断，需手动处理）

set -euo pipefail

# ---------------------------------------------------------------------------
# 内容签名（用特征定位文件，不写死 hash 文件名）
#
#   核心模式（新旧版本通用）：
#     <assign_var> = <useHiddenModels_var> && <authMethod_var> !== `amazonBedrock`
#
#   旧版本（~v0.5x）：  o=null,s=X&&Y!==`amazonBedrock`;    （结尾是 ;）
#   新版本（v0.60+）：  c=null,l=X&&Y!==`amazonBedrock`,    （结尾是 ,）
#
#   patched：<assign_var>=false<terminator>
#
# 正则用 [;,] 匹配两种结尾，保持向前兼容。
# ---------------------------------------------------------------------------
UNPATCHED_RE='[A-Za-z_$][A-Za-z_$0-9]*=[A-Za-z_$][A-Za-z_$0-9]*&&[A-Za-z_$][A-Za-z_$0-9]*!==`amazonBedrock`[;,]'
PATCHED_RE='[A-Za-z_$][A-Za-z_$0-9]*=false[;,]'

# ---------------------------------------------------------------------------
# 模式解析
# ---------------------------------------------------------------------------
mode="patch"
case "${1:-}" in
  --revert) mode="revert" ;;
  -h|--help)
    cat <<'HELP'
patch-codex-desktop.sh — 让 macOS 版 Codex 桌面端显示 model_catalog_json 里的全部模型

用法：
  patch-codex-desktop.sh             # patch（幂等，可重复运行）
  patch-codex-desktop.sh --revert    # 从 .bak 还原
  patch-codex-desktop.sh --help

需要：
  - macOS（脚本会用 codesign、xattr、pgrep）
  - asar 命令，或 npx（自动拉 @electron/asar）
  - sudo 写权限（写入 /Applications 或 ~/Applications 下的 .app）

每次 Codex 桌面端自动更新都会覆盖被 patch 的 app.asar；升级后重跑一次即可。
改 .app 内容会让原 notarization 失效；脚本会用 ad-hoc 重签 + 清 quarantine。
HELP
    exit 0
    ;;
  "") : ;;
  *) echo "error: unknown argument: ${1:-} (try --help)" >&2; exit 2 ;;
esac

# ---------------------------------------------------------------------------
# 拒绝在非 macOS 上跑
# ---------------------------------------------------------------------------
if [ "$(uname)" != "Darwin" ]; then
  echo "error: this script targets macOS only (uname=$(uname))" >&2
  exit 1
fi

# ---------------------------------------------------------------------------
# 定位桌面端 .app
#
# 新版本（2025-07 起）OpenAI 把桌面端 .app 从 Codex.app 改名成 ChatGPT.app，
# 两者的 Contents/Resources/app.asar 布局一致，picker 过滤逻辑也相同。
# 这里同时支持新旧名字，优先找 ChatGPT.app（新），再回退 Codex.app（旧）。
# ---------------------------------------------------------------------------
APP="${CODEX_APP:-}"
if [ -z "$APP" ]; then
  for candidate in \
    "/Applications/ChatGPT.app" \
    "/Applications/Codex.app" \
    "$HOME/Applications/ChatGPT.app" \
    "$HOME/Applications/Codex.app"; do
    if [ -d "$candidate" ]; then
      APP="$candidate"
      break
    fi
  done
fi
if [ -z "$APP" ] || [ ! -d "$APP" ]; then
  echo "error: ChatGPT.app / Codex.app not found under /Applications or \$HOME/Applications" >&2
  echo "       set CODEX_APP=/path/to/ChatGPT.app to override" >&2
  exit 1
fi

ASAR="$APP/Contents/Resources/app.asar"
if [ ! -f "$ASAR" ]; then
  echo "error: app.asar not found at: $ASAR" >&2
  echo "       the .app layout may have changed; inspect Contents/Resources/" >&2
  exit 1
fi
ASAR_BAK="$ASAR.bak"

# ---------------------------------------------------------------------------
# 桌面端不能在跑
#
# 进程名取自 .app 的 basename（ChatGPT.app → ChatGPT，Codex.app → Codex），
# 这样新旧两个版本都能正确检测。
# ---------------------------------------------------------------------------
APP_PROC="$(basename "$APP" .app)"
if pgrep -x "$APP_PROC" >/dev/null 2>&1; then
  echo "error: $APP_PROC is running. Quit it first (Cmd+Q), then re-run this script." >&2
  exit 1
fi
if pgrep -f "$APP/Contents/MacOS/" >/dev/null 2>&1; then
  echo "error: a process under $APP is still running. Quit $APP_PROC first." >&2
  exit 1
fi

# ---------------------------------------------------------------------------
# 解析 asar 命令
# ---------------------------------------------------------------------------
resolve_asar_cmd() {
  if command -v asar >/dev/null 2>&1; then
    printf '%s\n' "asar"
    return 0
  fi
  if command -v npx >/dev/null 2>&1; then
    printf '%s\n' "npx -y @electron/asar"
    return 0
  fi
  return 1
}
if ! ASAR_CMD=$(resolve_asar_cmd); then
  echo "error: neither 'asar' nor 'npx' is available on PATH" >&2
  echo "       install Node (which ships npx), or run: npm install -g @electron/asar" >&2
  exit 3
fi

# ---------------------------------------------------------------------------
# 临时目录 & 清理
# ---------------------------------------------------------------------------
TMPDIR_PATCH=$(mktemp -d -t codex-patch.XXXXXX)
cleanup() { rm -rf "$TMPDIR_PATCH"; }
trap cleanup EXIT
EXTRACTED="$TMPDIR_PATCH/extracted"
PACKED="$TMPDIR_PATCH/app-patched.asar"
VERIFY_DIR="$TMPDIR_PATCH/verify"

# ---------------------------------------------------------------------------
# Revert 模式
# ---------------------------------------------------------------------------
if [ "$mode" = "revert" ]; then
  if [ ! -f "$ASAR_BAK" ]; then
    echo "error: no backup file at: $ASAR_BAK" >&2
    echo "       nothing to revert; the live asar may already be original" >&2
    exit 4
  fi
  echo "==> Restoring app.asar from backup..."
  sudo cp -p "$ASAR_BAK" "$ASAR"

  echo "==> Re-signing app (ad-hoc)..."
  if ! sudo codesign --force --deep --sign - "$APP"; then
    echo "warning: codesign reported errors above. App may fail to launch." >&2
    echo "         try manually: sudo codesign --force --deep --sign - '$APP'" >&2
  fi
  sudo xattr -dr com.apple.quarantine "$APP" 2>/dev/null || true

  echo
  echo "============================================"
  echo "  Reverted. Restart Codex desktop to apply."
  echo "============================================"
  exit 0
fi

# ---------------------------------------------------------------------------
# Patch 模式
# ---------------------------------------------------------------------------

echo "==> asar: $ASAR_CMD"
echo "==> app:  $APP"

# 1. extract ----------------------------------------------------------------
echo "==> Extracting app.asar to $EXTRACTED ..."
mkdir -p "$EXTRACTED"
# shellcheck disable=SC2086
$ASAR_CMD extract "$ASAR" "$EXTRACTED"

# 2. find target chunk ------------------------------------------------------
echo "==> Locating picker chunk by content signature..."
TARGET=$(
  grep -Elr --exclude='*.map' --exclude-dir=node_modules \
    -e "$UNPATCHED_RE" "$EXTRACTED" 2>/dev/null | head -n 1 || true
)

if [ -z "$TARGET" ]; then
  # 先看看是不是已经 patch 过
  ALREADY=$(
    grep -Elr --exclude='*.map' --exclude-dir=node_modules \
      -e "$PATCHED_RE" "$EXTRACTED" 2>/dev/null | while read -r candidate; do
        # 确认同一个文件里也有 amazonBedrock（排除误命中）
        if grep -qF 'amazonBedrock' "$candidate" 2>/dev/null; then
          printf '%s\n' "$candidate"
          break
        fi
      done || true
  )
  if [ -n "$ALREADY" ]; then
    echo "==> Already patched: ${ALREADY#$EXTRACTED/}"
    echo "==> No changes needed."
    exit 0
  fi

  # 真没找到 → fallback
  echo "error: unpatched signature not found in extracted bundle." >&2
  echo "       tried regex: $UNPATCHED_RE" >&2
  echo "       fallback: searching for any 'amazonBedrock' reference..." >&2
  FALLBACK=$(
    grep -Flr --exclude='*.map' --exclude-dir=node_modules \
      -e 'amazonBedrock' "$EXTRACTED" 2>/dev/null | head -n 5 || true
  )
  if [ -z "$FALLBACK" ]; then
    echo "       no 'amazonBedrock' references either. Bundle layout likely changed." >&2
    echo "       inspect $EXTRACTED manually." >&2
    exit 1
  fi
  echo "       'amazonBedrock' appears in:" >&2
  echo "$FALLBACK" | sed 's|^|         - |' >&2
  echo "       open those files and look for the picker filter logic." >&2
  exit 1
fi

echo "==> Target: ${TARGET#$EXTRACTED/}"

# 3. patch ------------------------------------------------------------------
echo "==> Applying patch in extracted bundle..."
# perl 替换：把 <var>=<var>&&<var>!==`amazonBedrock`[;,] 替换成 <var>=false[;,]
# 捕获赋值目标变量名，保留结尾分隔符（; 或 ,）
perl -i -pe 's/([A-Za-z_$][A-Za-z_$0-9]*)=[A-Za-z_$][A-Za-z_$0-9]*&&[A-Za-z_$][A-Za-z_$0-9]*!==`amazonBedrock`([;,])/$1=false$2/g' "$TARGET"

# 4. verify in extracted ----------------------------------------------------
# 确认 patch 生效：同一个文件里有 amazonBedrock 且 patched 模式存在
VERIFY_OK=false
if grep -qF 'amazonBedrock' "$TARGET"; then
  if grep -Eq "$PATCHED_RE" "$TARGET"; then
    # 还要确认 unpatched 模式已经消失
    if ! grep -Eq "$UNPATCHED_RE" "$TARGET"; then
      VERIFY_OK=true
    fi
  fi
fi
if [ "$VERIFY_OK" != "true" ]; then
  echo "error: perl substitution did not take effect in: $TARGET" >&2
  exit 1
fi
echo "==> Patch verified in extracted bundle."

# 5. repack -----------------------------------------------------------------
echo "==> Repacking asar..."
# shellcheck disable=SC2086
$ASAR_CMD pack "$EXTRACTED" "$PACKED"

# 6. verify packed ----------------------------------------------------------
mkdir -p "$VERIFY_DIR"
# shellcheck disable=SC2086
$ASAR_CMD extract "$PACKED" "$VERIFY_DIR"
if ! grep -Flr --exclude='*.map' --exclude-dir=node_modules \
     -e 'amazonBedrock' "$VERIFY_DIR" >/dev/null 2>&1; then
  echo "error: repacked asar lost amazonBedrock references" >&2
  exit 1
fi
# 确认 repacked 里 unpatched 模式不存在
if grep -Elr --exclude='*.map' --exclude-dir=node_modules \
     -e "$UNPATCHED_RE" "$VERIFY_DIR" >/dev/null 2>&1; then
  echo "error: repacked asar still contains unpatched pattern" >&2
  exit 1
fi
echo "==> Repacked asar verified."

# 7. backup original asar (only once) --------------------------------------
if [ ! -f "$ASAR_BAK" ]; then
  echo "==> Backing up original to: $ASAR_BAK"
  sudo cp -p "$ASAR" "$ASAR_BAK"
else
  echo "==> Backup already exists, keeping it: $ASAR_BAK"
fi

# 8. atomic install ---------------------------------------------------------
echo "==> Installing patched asar..."
sudo install -o root -g admin -m 0644 "$PACKED" "$ASAR.tmp.$$"
sudo mv -f "$ASAR.tmp.$$" "$ASAR"

# 9. re-sign + clean quarantine --------------------------------------------
echo "==> Re-signing app (ad-hoc). Watch the output for any nested failures..."
if ! sudo codesign --force --deep --sign - "$APP"; then
  echo "warning: codesign reported errors above." >&2
  echo "         App may fail to launch. Try manually:" >&2
  echo "           sudo codesign --force --deep --sign - '$APP'" >&2
  echo "         then:" >&2
  echo "           sudo xattr -dr com.apple.quarantine '$APP'" >&2
fi
sudo xattr -dr com.apple.quarantine "$APP" 2>/dev/null || true

# ---------------------------------------------------------------------------
echo
echo "============================================"
echo "  Patch complete."
echo
echo "  Next: launch Codex desktop. The model picker should now show every"
echo "  visibility:\"list\" model from your model_catalog_json."
echo
echo "  Revert with:"
echo "    $0 --revert"
echo
echo "  Note: re-run after every Codex auto-update (it overwrites app.asar)."
echo "============================================"
exit 0
