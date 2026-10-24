#!/bin/bash
# 修正版: Rust 工具链 + 指定版本 Flutter 3.24.5 + 源码 + bridge
set -uo pipefail
LOG=/var/log/rustdesk-builder/bootstrap.log
exec >>/var/log/rustdesk-builder/bootstrap.log 2>&1
echo
echo "===== phase2 开始 $(date '+%F %T') ====="

# ---------- 1. Rust ----------
echo "----- Rust 工具链 -----"
export PATH="$HOME/.cargo/bin:$PATH"
if ! command -v rustc >/dev/null 2>&1; then
  echo "  装 rustup..."
  curl -fsSL --proto '=https' --tlsv1.2 https://sh.rustup.rs -o /tmp/rustup.sh || {
    echo "  !! rustup.sh 下载失败"; }
  ls -l /tmp/rustup.sh
  sh /tmp/rustup.sh -y --default-toolchain stable --profile minimal --no-modify-path 2>&1 | tail -20
  echo "  rustup 退出码: $?"
fi
export PATH="$HOME/.cargo/bin:$PATH"
if command -v rustc >/dev/null 2>&1; then
  echo "  rustc: $(rustc --version)"
  echo "  cargo: $(cargo --version)"
else
  echo "  !! rustc 还是没有"
  ls -la "$HOME/.cargo" 2>/dev/null | head
  tail -20 /tmp/rustup.log 2>/dev/null
fi

# ---------- 2. Flutter 3.24.5 ----------
echo
echo "----- Flutter 3.24.5 -----"
FLUTTER_VERSION=3.24.5
FLUTTER_DIR=/opt/flutter
if [ ! -x "$FLUTTER_DIR/bin/flutter" ]; then
  URL=$(python3 - <<'PY'
import json, urllib.request
want = "3.24.5"
d = json.load(urllib.request.urlopen("https://storage.googleapis.com/flutter_infra_release/releases/releases_linux.json"))
for r in d["releases"]:
    if r.get("version") == want and r.get("channel") == "stable":
        print(d["base_url"] + "/" + r["archive"])
        break
else:
    print("")
PY
)
  if [ -z "$URL" ]; then
    echo "  !! 找不到 $FLUTTER_VERSION 的下载地址"
  else
    echo "  下载 $URL"
    curl -fL --retry 3 -o /tmp/flutter.tar.xz "$URL" 2>&1 | tail -2
    ls -lh /tmp/flutter.tar.xz
    rm -rf "$FLUTTER_DIR"
    mkdir -p /opt
    tar -xf /tmp/flutter.tar.xz -C /opt
    echo "  解包完成"
  fi
fi
if [ -x "$FLUTTER_DIR/bin/flutter" ]; then
  export PATH="$FLUTTER_DIR/bin:$PATH"
  git config --global --add safe.directory "$FLUTTER_DIR" 2>/dev/null
  flutter --version 2>&1 | head -3
  flutter config --no-analytics >/dev/null 2>&1
  flutter precache --linux --universal >/dev/null 2>&1
  echo "  precache 完成"
else
  echo "  !! Flutter 没装好"
fi

# ---------- 3. 源码 ----------
echo
echo "----- 远界源码 -----"
SRC=/opt/rustdesk-src
if [ ! -d "$SRC/src" ]; then
  rm -rf "$SRC"
  git clone --depth 1 https://github.com/rustdesk/rustdesk.git "$SRC" || echo "  !! 克隆失败"
fi
if [ -d "$SRC" ] && [ ! -d "$SRC/libs/hbb_common/src" ]; then
  git clone --depth 1 https://github.com/rustdesk/hbb_common.git "$SRC/libs/hbb_common" || echo "  !! hbb_common 克隆失败"
fi
if [ -d "$SRC/.git" ]; then
  echo "  rustdesk: $(cd $SRC && git rev-parse --short HEAD)"
  echo "  hbb_common: $(cd $SRC/libs/hbb_common && git rev-parse --short HEAD 2>/dev/null || echo '-')"
fi

# ---------- 4. bridge 代码生成 ----------
echo
echo "----- flutter_rust_bridge 代码生成 -----"
export PATH="$HOME/.cargo/bin:$FLUTTER_DIR/bin:$PATH"
cd "$SRC" 2>/dev/null || exit 1
if [ ! -f src/bridge_generated.rs ]; then
  echo "  装 cargo-expand 1.0.95 ..."
  cargo install cargo-expand --version 1.0.95 --locked 2>&1 | tail -3
  echo "  装 flutter_rust_bridge_codegen 1.80.1 ..."
  cargo install flutter_rust_bridge_codegen --version 1.80.1 --features uuid --locked 2>&1 | tail -3
  echo "  生成 bridge ..."
  # --c-output 是给移动端用的 C 头文件, 本仓库没有移动端平台工程了,
  # 生成到临时目录即可 (别写进源码树)。
  C_OUT="$(mktemp -d)/bridge_generated.h"
  flutter_rust_bridge_codegen \
    --rust-input ./src/flutter_ffi.rs \
    --dart-output ./flutter/lib/generated_bridge.dart \
    --c-output "$C_OUT" 2>&1 | tail -15
  ls -l src/bridge_generated.rs src/bridge_generated.io.rs flutter/lib/generated_bridge.dart 2>/dev/null
else
  echo "  bridge 已存在"
fi

echo
echo "===== phase2 结束 $(date '+%F %T') ====="
echo "PHASE2_DONE=1" >> "$LOG"
