#!/bin/bash
LOG=/var/log/rustdesk-builder/bootstrap.log
exec >>/var/log/rustdesk-builder/bootstrap.log 2>&1
echo
echo "===== 收拾 $(date '+%H:%M:%S') ====="

echo "--- 停掉跑偏的 Flutter 3.47 下载 (要用 3.24.5) ---"
pkill -f "flutter_linux_3.47" 2>/dev/null && echo "  已停 3.47 下载"
pkill -f "bootstrap-builder.sh" 2>/dev/null && echo "  已停旧 bootstrap"
rm -f /tmp/flutter.tar.xz
echo "  清掉 /tmp/flutter.tar.xz"

echo "--- phase2 在跑吗 ---"
ps -ef | grep -E "phase2|rustup|flutter_linux" | grep -v grep || echo "  phase2 没在跑"

echo "--- aom 到底装到哪了 ---"
find /usr/local /tmp/aom-build -name "libaom*" 2>/dev/null | head -10
ls -l /usr/local/lib/ 2>/dev/null | head -20
echo "--- aom 的 cmake 配置 ---"
grep -E "AOM_TARGET_CPU|BUILD_SHARED|CMAKE_INSTALL_PREFIX" /tmp/aom-build/CMakeCache.txt 2>/dev/null | head -5
echo "--- aom 编译产物 ---"
ls -la /tmp/aom-build/*.a /tmp/aom-build/*.so* 2>/dev/null | head -5
find /tmp/aom-build -name "libaom*" 2>/dev/null | head -5
echo "--- aom make install 日志尾 ---"
grep -E "Installing|Up-to-date" /tmp/aom-make.log 2>/dev/null | tail -8
