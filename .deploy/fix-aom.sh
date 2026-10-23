#!/bin/bash
# 编译 aom。麒麟只有 GCC 7.3, aom 3.13 的 AVX2 intrinsics 编不过,
# 所以关掉 SIMD 优化 (AOM_TARGET_CPU=generic), 多版本回退。
set -uo pipefail
LOG=/var/log/rustdesk-builder/bootstrap.log
exec >>/var/log/rustdesk-builder/bootstrap.log 2>&1
echo
echo "----- 3b. aom 重新编译 (关 SIMD) $(date '+%H:%M:%S') -----"

build_aom() {
  local ver="$1" extra="$2"
  rm -rf /tmp/aom-src /tmp/aom-build /tmp/aom.tar.gz
  mkdir -p /tmp/aom-src
  if ! curl -fsSL --retry 3 -o /tmp/aom.tar.gz \
        "https://aomedia.googlesource.com/aom/+archive/${ver}.tar.gz"; then
    echo "  $ver: 下载失败"; return 1
  fi
  tar -xzf /tmp/aom.tar.gz -C /tmp/aom-src
  mkdir -p /tmp/aom-build && cd /tmp/aom-build
  # shellcheck disable=SC2086
  cmake /tmp/aom-src -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=/usr/local \
        -DENABLE_DOCS=0 -DENABLE_EXAMPLES=0 -DENABLE_TESTS=0 -DENABLE_TOOLS=0 \
        -DCONFIG_AV1_HIGHBITDEPTH=1 $extra >/tmp/aom-cmake.log 2>&1 || {
    echo "  $ver: cmake 失败"; tail -8 /tmp/aom-cmake.log; return 1; }
  if ! make -j"$(nproc)" >/tmp/aom-make.log 2>&1; then
    echo "  $ver: make 失败"; grep -E "错误|error:" /tmp/aom-make.log | head -5; return 1
  fi
  make install >>/tmp/aom-make.log 2>&1 || { echo "  $ver: install 失败"; return 1; }
  ldconfig
  [ -f /usr/local/lib/libaom.a ] || [ -f /usr/local/lib/libaom.so ] || {
    echo "  $ver: 装完没有库文件"; return 1; }
  echo "  $ver $extra -> 成功"
  return 0
}

for combo in "v3.13.3 -DAOM_TARGET_CPU=generic" \
             "v3.9.1 -DAOM_TARGET_CPU=generic" \
             "v3.0.0 -DAOM_TARGET_CPU=generic" \
             "v3.13.3 -DCMAKE_C_FLAGS=-mno-avx2 -DCMAKE_CXX_FLAGS=-mno-avx2"; do
  set -- $combo
  if build_aom "$1" "$2"; then
    AOM_OK=1
    break
  fi
done

if [ "${AOM_OK:-0}" = "1" ]; then
  mkdir -p /usr/local/lib/pkgconfig
  cat > /usr/local/lib/pkgconfig/aom.pc <<'EOF'
prefix=/usr/local
exec_prefix=${prefix}
libdir=${exec_prefix}/lib
includedir=${prefix}/include

Name: aom
Description: Alliance for Open Media AV1 codec library
Version: 3.13.3
Libs: -L${libdir} -laom
Cflags: -I${includedir}
EOF
  echo "  aom.pc 写好了"
else
  echo "  !! aom 全部方案失败"
fi

# libyuv 的 .pc 也要补 (上次的目录不存在)
mkdir -p /usr/local/lib/pkgconfig
if [ -f /usr/local/lib/libyuv.a ] || [ -f /usr/local/lib/libyuv.so ]; then
  cat > /usr/local/lib/pkgconfig/libyuv.pc <<'EOF'
prefix=/usr/local
exec_prefix=${prefix}
libdir=${exec_prefix}/lib
includedir=${prefix}/include

Name: libyuv
Description: libyuv - YUV scaling and conversion library
Version: 1916
Libs: -L${libdir} -lyuv
Cflags: -I${includedir}
EOF
  echo "  libyuv.pc 写好了"
fi

export PKG_CONFIG_PATH=/usr/local/lib/pkgconfig:/usr/local/share/pkgconfig
echo "--- pkg-config 最终验证 ---"
for p in vpx libyuv aom opus; do
  printf "  %-10s %s\n" "$p" "$(pkg-config --exists $p 2>/dev/null && echo "OK ($(pkg-config --modversion $p))" || echo MISSING)"
done
echo "----- 3b 结束 $(date '+%H:%M:%S') -----"
