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
#     let a=[],o=null,s=<i>&&<e>!==`amazonBedrock`;   // i = useHiddenModels
#     r.forEach(n => {
#       if (s ? availableModels.has(n.model) : !n.hidden) { ... 显示 ... }
#     });
#
# 当 Statsig 动态配置 `107580212` 命中（生产环境会命中），返回的
# `{use_hidden_models:true, available_models:[gpt-*...]}` 让 `s=true`，过滤
# 走白名单分支，所有非 gpt 模型（GLM/Qwen/Claude/Kimi/...）都被前端挡掉，
# 跟 CLI 后端完全无关。
#
# 这个脚本把那一行的 `s` 强制设成 `false`，过滤就永远走 `!n.hidden` 分支，
# 让所有 `visibility:"list"` 的 catalog 模型都显示。
#
# VS Code 扩展上的同一处 patch 已经在 codex fork 里落地（见
# `scripts/patch-codex-extension.sh`）；这个脚本是桌面端版本。
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
#   原始：    o=null,s=<var>&&<var>!==`amazonBedrock`;
#   patched： o=null,s=false;
#
# 这两个串在 VS Code 扩展和桌面端应该是一致的（同一份 webview bundle）。
# 如果 minifier 变量名换了，签名会失败，脚本会落到 fallback 搜索。
# ---------------------------------------------------------------------------
UNPATCHED_RE='o=null,s=[A-Za-z_$][A-Za-z_$0-9]*&&[A-Za-z_$][A-Za-z_$0-9]*!==`amazonBedrock`;'
PATCHED_MARKER='o=null,s=false;'

# ---------------------------------------------------------------------------
# 模式解析（先解析，再校验环境，错误信息更清晰）
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
# 拒绝在非 macOS 上跑：避免有人在 Linux/WSL 上误执行后 sudo cp 出乱子
# ---------------------------------------------------------------------------
if [ "$(uname)" != "Darwin" ]; then
  echo "error: this script targets macOS only (uname=$(uname))" >&2
  echo "       for VS Code extension, use patch-codex-extension.sh instead:" >&2
  echo "         https://raw.githubusercontent.com/KingingWang/codex/main/scripts/patch-codex-extension.sh" >&2
  exit 1
fi

# ---------------------------------------------------------------------------
# 定位 Codex.app
# 优先级：环境变量 CODEX_APP → /Applications/Codex.app → ~/Applications/Codex.app
# ---------------------------------------------------------------------------
APP="${CODEX_APP:-}"
if [ -z "$APP" ]; then
  if [ -d "/Applications/Codex.app" ]; then
    APP="/Applications/Codex.app"
  elif [ -d "$HOME/Applications/Codex.app" ]; then
    APP="$HOME/Applications/Codex.app"
  fi
fi
if [ -z "$APP" ] || [ ! -d "$APP" ]; then
  echo "error: Codex.app not found under /Applications or \$HOME/Applications" >&2
  echo "       set CODEX_APP=/path/to/Codex.app to override" >&2
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
# Codex 不能在跑——替换运行中的 .app 资源是 racy 的
# 桌面端的进程名通常是 "Codex"，主二进制在 .app/Contents/MacOS/ 下
# ---------------------------------------------------------------------------
if pgrep -x "Codex" >/dev/null 2>&1; then
  echo "error: Codex is running. Quit it first (Cmd+Q), then re-run this script." >&2
  exit 1
fi
# 兜底：按 .app 路径搜，进程名可能不同（Insiders、自定义重命名等）
if pgrep -f "$APP/Contents/MacOS/" >/dev/null 2>&1; then
  echo "error: a process under $APP is still running. Quit Codex first." >&2
  exit 1
fi

# ---------------------------------------------------------------------------
# 解析 asar 命令
# 优先用 PATH 里的 asar，没有则用 npx -y @electron/asar（自动下载，无需 sudo）
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
# 注意：ASAR_CMD 一定要不带引号展开（"npx -y @electron/asar" 是 4 个 token）

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
  # codesign 的 stderr 故意不吞，让用户看到所有子组件的签名状态
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
# 不写死 webview/assets/ ——桌面端 bundle 路径可能不一样，全树搜更稳。
# 但跳过 sourcemap 和 node_modules，把搜索从几秒压到几百毫秒。
#
# pipeline 末尾必须 `|| true`：在 `set -o pipefail` 下，grep 找不到 → 退出 1，
# 整个 pipeline 退出 1，`set -e` 会立刻终止脚本，跳过下面友好的错误处理。
echo "==> Locating picker chunk by content signature..."
TARGET=$(
  grep -Elr --exclude='*.map' --exclude-dir=node_modules \
    -e "$UNPATCHED_RE" "$EXTRACTED" 2>/dev/null | head -n 1 || true
)

if [ -z "$TARGET" ]; then
  # 先看看是不是已经 patch 过（避免误报 "bundle layout changed"）
  ALREADY=$(
    grep -Flr --exclude='*.map' --exclude-dir=node_modules \
      -e "$PATCHED_MARKER" "$EXTRACTED" 2>/dev/null | head -n 1 || true
  )
  if [ -n "$ALREADY" ]; then
    echo "==> Already patched: ${ALREADY#$EXTRACTED/}"
    echo "==> No changes needed."
    exit 0
  fi

  # 真没找到 → 用更宽松的锚点定位"哪一行 JS 引用了 amazonBedrock"
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
# perl 看到的字符串：
#   s/o=null,s=[A-Za-z_$][A-Za-z_$0-9]*&&[A-Za-z_$][A-Za-z_$0-9]*!==`amazonBedrock`;/o=null,s=false;/g
# $ 在字符类里是字面量；backtick 是字面 backtick；分号是字面分号。
# 单引号在 bash 里防止 $ 和 ` 被展开。
perl -i -pe 's/o=null,s=[A-Za-z_$][A-Za-z_$0-9]*&&[A-Za-z_$][A-Za-z_$0-9]*!==`amazonBedrock`;/o=null,s=false;/g' "$TARGET"

# 4. verify in extracted ----------------------------------------------------
if ! grep -qF -- "$PATCHED_MARKER" "$TARGET"; then
  echo "error: perl substitution did not take effect in: $TARGET" >&2
  exit 1
fi
echo "==> Patch verified in extracted bundle."

# 5. repack -----------------------------------------------------------------
echo "==> Repacking asar..."
# shellcheck disable=SC2086
$ASAR_CMD pack "$EXTRACTED" "$PACKED"

# 6. verify packed ----------------------------------------------------------
# 关键修复点：之前那个脚本在这里写错了 asar extract 的目标路径，
# 导致 verify 永远失败。这里 extract 到一个干净的 VERIFY_DIR。
mkdir -p "$VERIFY_DIR"
# shellcheck disable=SC2086
$ASAR_CMD extract "$PACKED" "$VERIFY_DIR"
if ! grep -Flr --exclude='*.map' --exclude-dir=node_modules \
     -e "$PATCHED_MARKER" "$VERIFY_DIR" >/dev/null 2>&1; then
  echo "error: repacked asar does not contain the patch" >&2
  echo "       (asar pack/extract round-trip may have dropped the file)" >&2
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
# 先 sudo install 到一个临时名，再 mv 覆盖；mv 是原子的，避免半状态。
echo "==> Installing patched asar..."
# macOS 标准：/Applications/*.app 里的资源典型归属是 root:admin 0644。
# `admin` 是 macOS 内置系统组，对任何 sudo 用户都存在；这里不需要为
# 其他平台做兜底（脚本已经在最前面通过 uname 守卫拒绝非 macOS）。
# 不用 cp -p：$PACKED 是当前用户写的，cp -p 会把 owner 也带过来变成 user:admin，
# 跟系统装的 Codex 不一致，可能让后续 codesign 报权限异常。install 强制
# 归属更安全。
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
# 清 quarantine：第一次启动 macOS 才不会弹"已损坏"
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
