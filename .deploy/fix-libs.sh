#!/bin/bash
# 补装 libyuv 和 aom (上一次的 bootstrap 用了错的 URL)
set -uo pipefail
LOG=/var/log/rustdesk-builder/bootstrap.log
exec >>/var/log/rustdesk-builder/bootstrap.log 2>&1
echo
echo "----- 2b. libyuv / aom 补装 (修正 URL) $(date '+%H:%M:%S') -----"

echo "--- libyuv ---"
if pkg-config --exists libyuv 2>/dev/null; then
  echo "  已就绪"
else
  rm -rf /tmp/libyuv-src /tmp/libyuv-build /tmp/libyuv.tar.gz
  mkdir -p /tmp/libyuv-src
  # chromium.googlesource 的 +archive 格式: 解开后文件直接铺在目录里, 没有顶层目录
  if curl -fsSL --retry 3 -o /tmp/libyuv.tar.gz \
       "https://chromium.googlesource.com/libyuv/libyuv/+archive/d98915a654d3564e4802a0004add46221c4e4348.tar.gz"; then
    tar -xzf /tmp/libyuv.tar.gz -C /tmp/libyuv-src
    ls /tmp/libyuv-src | head -5
    mkdir -p /tmp/libyuv-build && cd /tmp/libyuv-build
    cmake /tmp/libyuv-src -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=/usr/local \
          -DBUILD_SHARED_LIBS=OFF >/tmp/libyuv-cmake.log 2>&1
    make -j"$(nproc)" >/tmp/libyuv-make.log 2>&1
    make install >>/tmp/libyuv-make.log 2>&1
    ldconfig
    if pkg-config --exists libyuv; then echo "  libyuv OK: $(pkg-config --modversion libyuv)"; else
      echo "  libyuv 失败, cmake log:"; tail -15 /tmp/libyuv-cmake.log; fi
  else
    echo "  下载失败"
  fi
fi

echo "--- aom ---"
if pkg-config --exists aom 2>/dev/null; then
  echo "  已就绪"
else
  rm -rf /tmp/aom-src /tmp/aom-build /tmp/aom.tar.gz
  mkdir -p /tmp/aom-src
  if curl -fsSL --retry 3 -o /tmp/aom.tar.gz \
       "https://aomedia.googlesource.com/aom/+archive/v3.13.3.tar.gz"; then
    tar -xzf /tmp/aom.tar.gz -C /tmp/aom-src
    ls /tmp/aom-src | head -5
    mkdir -p /tmp/aom-build && cd /tmp/aom-build
    cmake /tmp/aom-src -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=/usr/local \
          -DENABLE_DOCS=0 -DENABLE_EXAMPLES=0 -DENABLE_TESTS=0 -DENABLE_TOOLS=0 \
          -DCONFIG_AV1_HIGHBITDEPTH=1 >/tmp/aom-cmake.log 2>&1
    make -j"$(nproc)" >/tmp/aom-make.log 2>&1
    make install >>/tmp/aom-make.log 2>&1
    ldconfig
    if pkg-config --exists aom; then echo "  aom OK: $(pkg-config --modversion aom)"; else
      echo "  aom 失败, cmake log:"; tail -15 /tmp/aom-cmake.log; fi
  else
    echo "  下载失败"
  fi
fi

echo "--- 结果 ---"
for p in vpx libyuv aom opus; do
  printf "  %-10s %s\n" "$p" "$(pkg-config --exists $p 2>/dev/null && echo OK || echo MISSING)"
done
echo "----- 2b 结束 $(date '+%H:%M:%S') -----"
