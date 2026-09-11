#!/usr/bin/env bash
#
# replace-codex-desktop.sh — 用 fork 最新 release 的裸二进制替换 macOS 桌面端内嵌的 codex CLI
#
# 背景
# -----
# ChatGPT.app / Codex.app 桌面端内置了一个 codex CLI：
#
#     /Applications/ChatGPT.app/Contents/Resources/codex
#
# 想让桌面端跑 fork 的版本，过去只能手动：
#
#     rm /Applications/ChatGPT.app/Contents/Resources/codex
#     ln -s /opt/homebrew/bin/codex /Applications/ChatGPT.app/Contents/Resources/codex
#
# 这个脚本把这件事自动化，并且不依赖 npm / Homebrew / zstd：
#   1. 从 fork 的 GitHub Release 固定链接下载当前架构的裸二进制
#      （release 由 rust-release-simple{,-macos,-windows} 三个 workflow
#        共享同一 tag 发布，因此 releases/latest/download/... 是固定 URL）；
#   2. 安装到 ~/.codex/fork-desktop/codex（本脚本托管路径，属主是当前用户）；
#   3. 把 app 内嵌路径备份为 codex.bak，替换为指向托管路径的软链；
#   4. 清 quarantine（默认不重签，理由见下面「关于签名」）。
#
# 幂等：目标已经是软链（说明之前替换过，或用户手动替换过）时默认直接跳过，
# 不做任何修改。
#
# 关于签名
# --------
# 只替换 Contents/Resources/codex 不会让 app 起不来，因为三道检查的作用域都不
# 覆盖这个改动：
#   - 内核 exec 时只验被启动的 Mach-O 自身签名，不验 bundle 的资源封印；
#   - Gatekeeper 的整包评估只在首次启动（带 quarantine 时）发生一次；
#   - library validation 只管加载进进程的 dylib，不管 spawn 出来的子进程。
#
# bundle 封印确实会破（codesign --verify 和 spctl --assess 都会失败），但启动
# 路径上没有人去查它。所以默认只清 quarantine：那一步覆盖的是「装好还没启动过
# 就改 bundle」的情况，此时首次启动会撞上整包评估并弹「已损坏」。
#
# ad-hoc 重签反而是更大的扰动：它把主程序的 Developer ID 身份换成 ad-hoc，
# cdhash 变化会牵连按签名身份绑定的钥匙串 ACL、TCC 授权和自动更新校验。
# 不重签还有个好处：--revert 还原原始二进制后，bundle 内容与原签名重新自洽，
# 公证记录也跟着恢复。
#
# 确实撞到 Gatekeeper 拦截时用 --resign 兜底。另外，如果 app 当前已经是 ad-hoc
# 签名（说明之前跑过旧版脚本、原始身份已丢失），脚本会继续 ad-hoc 重签以保持
# 封印自洽——此时已经没有可保留的东西了。
#
# 用法
# -----
#   scripts/replace-codex-desktop.sh                     # 替换（幂等，已是软链则跳过）
#   scripts/replace-codex-desktop.sh --update            # 已指向托管路径时，原地升级二进制（无需 sudo 改 app）
#   scripts/replace-codex-desktop.sh --force             # 强制重指软链 + 重装二进制
#   scripts/replace-codex-desktop.sh --revert            # 从 codex.bak 还原官方二进制
#   scripts/replace-codex-desktop.sh --from-file ./codex # 用本地二进制替换（离线 / 自测）
#   scripts/replace-codex-desktop.sh --release <tag>     # 用指定 release，而不是 latest
#   scripts/replace-codex-desktop.sh --resign            # 额外 ad-hoc 重签整个 bundle（默认不重签）
#   scripts/replace-codex-desktop.sh --help
#
# 环境变量：
#   CODEX_APP          覆盖 .app 路径（默认依次找 ChatGPT.app / Codex.app）
#   CODEX_FORK_REPO    覆盖 "owner/repo"（默认 KingingWang/codex）
#   CODEX_FORK_DIR     托管二进制目录（默认 ~/.codex/fork-desktop）
#   CODEX_RELEASE_URL  完整覆盖下载 URL（优先于 --release）
#   CODEX_RESIGN       设成非空即等价于 --resign（方便 curl | bash 的用法）
#
# 需要：
#   - macOS（会用 codesign、xattr、pgrep、file）
#   - curl（macOS 自带）
#   - sudo 写权限（写入 /Applications 下的 .app；--update 只动用户目录，不需要 sudo 加成）
#
# 重要：
#   - 桌面端自动更新会还原 Contents/Resources/codex；升级后重跑一次本脚本即可
#   - 替换前必须退出桌面端，脚本会 pgrep 检查
#
# 退出码：
#   0  成功（含幂等 skip）
#   1  一般错误（找不到 app / 二进制、目标布局变化、verify 失败 等）
#   2  参数错误
#   4  --revert 时没东西可还原

set -euo pipefail

# ---------------------------------------------------------------------------
# 配置
# ---------------------------------------------------------------------------
REPO="${CODEX_FORK_REPO:-KingingWang/codex}"
FORK_DIR="${CODEX_FORK_DIR:-$HOME/.codex/fork-desktop}"
MANAGED_BIN="$FORK_DIR/codex"
RELEASE_TAG=""
FROM_FILE=""

mode="patch"
RESIGN=false
[ -n "${CODEX_RESIGN:-}" ] && RESIGN=true
while [ $# -gt 0 ]; do
  case "$1" in
    --update) mode="update" ;;
    --force) mode="force" ;;
    --revert) mode="revert" ;;
    --resign) RESIGN=true ;;
    --release)
      [ $# -ge 2 ] || { echo "error: --release requires a tag" >&2; exit 2; }
      RELEASE_TAG="$2"; shift ;;
    --from-file)
      [ $# -ge 2 ] || { echo "error: --from-file requires a path" >&2; exit 2; }
      FROM_FILE="$2"; shift ;;
    -h|--help)
      cat <<'HELP'
replace-codex-desktop.sh — 用 fork release 二进制替换 macOS 桌面端内嵌的 codex

用法：
  replace-codex-desktop.sh              # 替换（幂等，已是软链则跳过）
  replace-codex-desktop.sh --update     # 已替换时原地升级托管二进制
  replace-codex-desktop.sh --force      # 强制重新替换
  replace-codex-desktop.sh --revert     # 从 codex.bak 还原
  replace-codex-desktop.sh --from-file <path>   # 用本地二进制（离线）
  replace-codex-desktop.sh --release <tag>      # 指定 release，而非 latest
  replace-codex-desktop.sh --resign             # 额外 ad-hoc 重签（默认不重签）

环境变量：CODEX_APP / CODEX_FORK_REPO / CODEX_FORK_DIR / CODEX_RELEASE_URL
           CODEX_RESIGN=1 等价于 --resign
HELP
      exit 0 ;;
    *) echo "error: unknown argument: $1 (try --help)" >&2; exit 2 ;;
  esac
  shift
done

if [ -n "$FROM_FILE" ] && [ -n "$RELEASE_TAG" ]; then
  echo "error: --from-file and --release are mutually exclusive" >&2
  exit 2
fi

if [ "$RESIGN" = true ] && [ "$mode" = "update" ]; then
  echo "error: --resign does not apply to --update (the app bundle is not touched)" >&2
  exit 2
fi

die() { echo "error: $*" >&2; exit 1; }
step() { echo "==> $*"; }

# ---------------------------------------------------------------------------
# 只支持 macOS
# ---------------------------------------------------------------------------
[ "$(uname)" = "Darwin" ] || die "this script targets macOS only (uname=$(uname))"

# ---------------------------------------------------------------------------
# 架构 → release 资产名
# ---------------------------------------------------------------------------
case "$(uname -m)" in
  arm64) ASSET="codex-macos-aarch64"; WANT_ARCH="arm64" ;;
  x86_64) ASSET="codex-macos-x86_64"; WANT_ARCH="x86_64" ;;
  *) die "unsupported architecture: $(uname -m)" ;;
esac

# ---------------------------------------------------------------------------
# 定位桌面端 .app（新名字 ChatGPT.app 优先，旧名字 Codex.app 兜底）
# ---------------------------------------------------------------------------
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
[ -n "$APP" ] && [ -d "$APP" ] || die "ChatGPT.app / Codex.app not found (set CODEX_APP=/path/to/ChatGPT.app)"

TARGET="$APP/Contents/Resources/codex"
BAK="$TARGET.bak"

# ---------------------------------------------------------------------------
# 桌面端不能在跑
# ---------------------------------------------------------------------------
ensure_app_not_running() {
  local proc
  proc="$(basename "$APP" .app)"
  if pgrep -x "$proc" >/dev/null 2>&1; then
    die "$proc is running. Quit it first (Cmd+Q), then re-run this script."
  fi
  if pgrep -f "$APP/Contents/MacOS/" >/dev/null 2>&1; then
    die "a process under $APP is still running. Quit $proc first."
  fi
}

# ---------------------------------------------------------------------------
# 临时目录 & 清理
# ---------------------------------------------------------------------------
TMPDIR_DL=$(mktemp -d -t codex-replace.XXXXXX)
cleanup() { rm -rf "$TMPDIR_DL"; }
trap cleanup EXIT
TMP_BIN="$TMPDIR_DL/codex"
FETCHED_BIN=""
BIN_VERSION=""
SIG_NOTE=""
DID_RESIGN=false

# ---------------------------------------------------------------------------
# 下载（或使用本地文件）+ 校验
# ---------------------------------------------------------------------------
fetch_binary() {
  if [ -n "$FROM_FILE" ]; then
    [ -f "$FROM_FILE" ] || die "--from-file not found: $FROM_FILE"
    FETCHED_BIN="$FROM_FILE"
    step "Using local binary: $FROM_FILE"
  else
    local url
    if [ -n "${CODEX_RELEASE_URL:-}" ]; then
      url="$CODEX_RELEASE_URL"
    elif [ -n "$RELEASE_TAG" ]; then
      url="https://github.com/${REPO}/releases/download/${RELEASE_TAG}/${ASSET}"
    else
      url="https://github.com/${REPO}/releases/latest/download/${ASSET}"
    fi
    step "Downloading $url"
    curl -fL --retry 3 --retry-delay 1 -o "$TMP_BIN" "$url" \
      || die "download failed: $url
       hint: 该架构的 release 资产可能还没构建完，稍等 workflow 跑完后重试"
    FETCHED_BIN="$TMP_BIN"
  fi

  # 架构校验（x86_64 二进制在 Apple Silicon 上 --version 也能经 Rosetta 跑，
  # 所以不能只靠 --version 冒烟，必须先查 Mach-O 头部）
  local file_out
  file_out="$(file -b "$FETCHED_BIN")"
  case "$file_out" in
    *"Mach-O"*"${WANT_ARCH}"*) : ;;
    *) die "binary is not a macOS ${WANT_ARCH} executable: ${file_out}" ;;
  esac

  chmod 0755 "$FETCHED_BIN" 2>/dev/null || true
  if ! BIN_VERSION="$("$FETCHED_BIN" --version 2>&1 | head -n 1)"; then
    die "smoke test failed: '$FETCHED_BIN --version' could not run"
  fi
  step "Binary verified: $BIN_VERSION ($WANT_ARCH)"
}

# ---------------------------------------------------------------------------
# 安装到托管路径（用户目录，不需要 sudo；临时文件 + mv 保证原子替换）
# ---------------------------------------------------------------------------
install_managed() {
  mkdir -p "$FORK_DIR"
  install -m 0755 "$FETCHED_BIN" "$MANAGED_BIN.tmp.$$"
  mv -f "$MANAGED_BIN.tmp.$$" "$MANAGED_BIN"
  xattr -d com.apple.quarantine "$MANAGED_BIN" 2>/dev/null || true
  step "Managed binary installed: $MANAGED_BIN"
}

# ---------------------------------------------------------------------------
# 清 quarantine（默认动作）；必要时 ad-hoc 重签（--resign，或 app 已是 ad-hoc）
# ---------------------------------------------------------------------------
# 判定当前 bundle 是否已经是 ad-hoc 签名。这里刻意不用管道：grep -q 命中后提前
# 退出会让 pipefail 把成功误判成失败。
app_is_adhoc_signed() {
  local info
  info="$(codesign -dvv "$APP" 2>&1 || true)"
  [[ "$info" == *$'\n'Signature=adhoc* || "$info" == Signature=adhoc* ]]
}

unquarantine_app() {
  step "Clearing com.apple.quarantine on app bundle"
  sudo xattr -dr com.apple.quarantine "$APP" 2>/dev/null || true

  if [ "$RESIGN" != true ]; then
    if app_is_adhoc_signed; then
      step "App is already ad-hoc signed; re-signing to keep the bundle seal consistent"
    else
      SIG_NOTE="original signature preserved (--resign to ad-hoc re-sign)"
      DID_RESIGN=false
      step "Keeping the app's original signature; not re-signing"
      return 0
    fi
  fi

  DID_RESIGN=true
  SIG_NOTE="ad-hoc re-signed (original notarization no longer applies)"
  step "Re-signing app (ad-hoc). Watch for nested signing failures..."
  if ! sudo codesign --force --deep --sign - "$APP"; then
    echo "warning: codesign reported errors; app may fail to launch." >&2
    echo "         try manually: sudo codesign --force --deep --sign - '$APP'" >&2
  fi
}

verify_target() {
  local v
  if ! v="$("$TARGET" --version 2>&1 | head -n 1)"; then
    die "post-install verify failed: '$TARGET --version' could not run"
  fi
  step "Post-install verify: $v"
}

# ---------------------------------------------------------------------------
# Revert 模式
# ---------------------------------------------------------------------------
if [ "$mode" = "revert" ]; then
  ensure_app_not_running
  if [ -L "$TARGET" ] && [ -f "$BAK" ]; then
    step "Removing symlink: $TARGET -> $(readlink "$TARGET")"
    sudo rm "$TARGET"
    step "Restoring original from backup: $BAK"
    sudo mv "$BAK" "$TARGET"
    unquarantine_app
    echo
    echo "============================================"
    echo "  Reverted. Restart Codex desktop to apply."
    if [ "$DID_RESIGN" = false ]; then
      echo "  No re-sign: if this was the only change to the bundle, the app's"
      echo "  original signature and notarization are valid again."
    fi
    echo "============================================"
    exit 0
  elif [ -L "$TARGET" ]; then
    die "target is a symlink but no backup at $BAK
       (上次可能是手动 rm 替换的，官方二进制没留备份；请重装桌面端恢复)"
  elif [ -f "$BAK" ]; then
    die "backup $BAK exists but target is a real file; inspect manually before reverting"
  else
    echo "Nothing to revert: $TARGET is the original binary and no backup exists."
    exit 4
  fi
fi

# ---------------------------------------------------------------------------
# 主流程：按目标当前状态分派
# ---------------------------------------------------------------------------
step "app:    $APP"
step "target: $TARGET"

if [ -L "$TARGET" ]; then
  # —— 已经是软链：之前替换过（本脚本或手动）。默认跳过。 ——
  LINK_DEST="$(readlink "$TARGET")"
  case "$mode" in
    patch)
      echo "==> Already replaced: $TARGET -> ${LINK_DEST:-<dangling>}"
      if [ "${LINK_DEST:-}" = "$MANAGED_BIN" ]; then
        echo "    already managed by this script; nothing to do."
        echo "    (use --update to refresh the binary, --force to redo)"
      else
        echo "    symlink is not managed by this script; leaving it alone."
        echo "    (use --force to re-point it to $MANAGED_BIN)"
      fi
      exit 0
      ;;
    update)
      [ "${LINK_DEST:-}" = "$MANAGED_BIN" ] \
        || die "target symlinks to ${LINK_DEST:-<dangling>}, not $MANAGED_BIN
       refusing --update on a link this script does not manage (use --force)"
      ensure_app_not_running
      fetch_binary
      install_managed
      verify_target
      echo
      echo "============================================"
      echo "  Updated: $MANAGED_BIN"
      echo "  $BIN_VERSION"
      echo "============================================"
      exit 0
      ;;
    force)
      ensure_app_not_running
      fetch_binary
      install_managed
      step "Re-pointing symlink -> $MANAGED_BIN"
      sudo ln -sfn "$MANAGED_BIN" "$TARGET"
      unquarantine_app
      verify_target
      ;;
  esac
elif [ -e "$TARGET" ]; then
  # —— 官方原始二进制（实体文件）：备份 + 换软链 ——
  [ "$mode" != "update" ] \
    || die "target is the original binary (not replaced yet); run without --update first"
  [ ! -e "$BAK" ] \
    || die "backup $BAK already exists while target is a real file; inspect manually"

  ensure_app_not_running
  fetch_binary
  install_managed

  step "Backing up original: $BAK"
  sudo mv "$TARGET" "$BAK"
  step "Installing symlink -> $MANAGED_BIN"
  sudo ln -s "$MANAGED_BIN" "$TARGET"
  unquarantine_app
  verify_target
else
  die "codex binary not found at: $TARGET
       the app layout may have changed. Inspect with:
         find \"$APP/Contents\" -name 'codex*' -type f"
fi

echo
echo "============================================"
echo "  Replacement complete."
echo
echo "  Managed binary : $MANAGED_BIN"
echo "  Version        : $BIN_VERSION"
echo "  Original backup: $BAK (if created)"
echo "  Signature      : $SIG_NOTE"
echo
echo "  Next: launch Codex desktop."
echo "  Upgrade later:  $0 --update"
echo "  Roll back:      $0 --revert"
echo
echo "  Note: Codex auto-updates restore the embedded binary; re-run this"
echo "  script after every desktop app update."
echo "============================================"
exit 0
