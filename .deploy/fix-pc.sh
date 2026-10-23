#!/bin/bash
# 补 libyuv / aom 的 pkg-config 文件
# 这两个库编译时不装 .pc, 但 scrap 的 build.rs 用 pkg_config::probe_library 找它们,
# 找不到直接 panic, 所以必须手工补上。
set -uo pipefail
LOG=/var/log/rustdesk-builder/bootstrap.log
exec >>/var/log/rustdesk-builder/bootstrap.log 2>&1
echo
echo "----- 2c. 补 pkg-config 文件 $(date '+%H:%M:%S') -----"

echo "--- 装了什么 ---"
ls -l /usr/local/lib/libyuv* 2>/dev/null | head -3
ls -l /usr/local/lib/libaom* 2>/dev/null | head -3
ls -l /usr/local/include/libyuv.h 2>/dev/null
ls -ld /usr/local/include/aom 2>/dev/null
ls /usr/local/lib/pkgconfig/ 2>/dev/null | head -10

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
else
  echo "  !! libyuv 库文件不存在, 编译失败过"
fi

if [ -f /usr/local/lib/libaom.a ] || [ -f /usr/local/lib/libaom.so ]; then
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
  echo "  !! aom 库文件不存在"
fi

export PKG_CONFIG_PATH=/usr/local/lib/pkgconfig:/usr/local/share/pkgconfig
ldconfig
echo "--- pkg-config 验证 ---"
for p in vpx libyuv aom opus; do
  printf "  %-10s %s\n" "$p" "$(pkg-config --exists $p 2>/dev/null && echo "OK ($(pkg-config --modversion $p))" || echo MISSING)"
done
echo "--- 头文件 ---"
for h in /usr/local/include/vpx/vpx_ffi.h /usr/local/include/libyuv/yuv_ffi.h /usr/local/include/aom/aom_ffi.h; do
  printf "  %-44s %s\n" "$h" "$([ -f "$h" ] && echo OK || echo MISSING)"
done
echo "----- 2c 结束 $(date '+%H:%M:%S') -----"
