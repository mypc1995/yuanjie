#!/bin/bash
echo "=== 可用 gcc 版本 ==="
gcc --version | head -1
ls /opt/rh 2>/dev/null | head
echo "=== 找 devtoolset / gcc-toolset ==="
timeout 60 dnf --noplugins --setopt=timeout=8 --setopt=retries=0 list --available 2>/dev/null | grep -iE "^(devtoolset|gcc-toolset)" | head -10
echo "=== aom 老版本能用 GCC7 编吗 (查 vcpkg 里的历史) ==="
ls /opt/rustdesk-src 2>/dev/null || echo "(源码还没拉好)"
echo "=== 磁盘 ==="
df -h / | tail -1
