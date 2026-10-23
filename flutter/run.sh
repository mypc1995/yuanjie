#!/usr/bin/env bash
# 生成 bridge 代码并跑客户端。
# 只做 Windows / Linux 桌面端 —— 移动端和 macOS 的平台工程已经移出本仓库。

set -e

cargo install flutter_rust_bridge_codegen --version 1.80.1 --features uuid --locked
flutter pub get

# --c-output 是给移动端用的 C 头文件, 本仓库没有那些平台工程,
# 生成到临时目录即可 (别写进源码树)。
C_OUT="$(mktemp -d)/bridge_generated.h"
~/.cargo/bin/flutter_rust_bridge_codegen \
  --rust-input ../src/flutter_ffi.rs \
  --dart-output ./lib/generated_bridge.dart \
  --c-output "$C_OUT"

# call `flutter clean` if cargo build fails
cargo build --locked --features flutter
flutter run $@
