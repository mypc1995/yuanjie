#!/bin/bash
LOG=/var/log/rustdesk-builder/bootstrap.log
exec >>/var/log/rustdesk-builder/bootstrap.log 2>&1
echo
echo "===== 修 pkg-config + 重启 phase2 $(date '+%H:%M:%S') ====="

# 麒麟是 lib64 布局, 库装在 /usr/local/lib64
LIBDIR=/usr/local/lib
[ -d /usr/local/lib64 ] && [ -f /usr/local/lib64/libaom.a ] && LIBDIR=/usr/local/lib64

cat > /usr/local/lib/pkgconfig/aom.pc <<EOF
prefix=/usr/local
exec_prefix=\${prefix}
libdir=$LIBDIR
includedir=\${prefix}/include

Name: aom
Description: Alliance for Open Media AV1 codec library
Version: 3.13.3
Libs: -L\${libdir} -laom
Cflags: -I\${includedir}
EOF
echo "  aom.pc -> libdir=$LIBDIR"

# libyuv 也在 lib, 确认一下
LD=''
if [ -f /usr/local/lib64/libyuv.a ]; then LD=/usr/local/lib64; elif [ -f /usr/local/lib/libyuv.a ]; then LD=/usr/local/lib; fi
[ -n "$LD" ] && cat > /usr/local/lib/pkgconfig/libyuv.pc <<EOF
prefix=/usr/local
exec_prefix=\${prefix}
libdir=$LD
includedir=\${prefix}/include

Name: libyuv
Description: libyuv - YUV scaling and conversion library
Version: 1916
Libs: -L\${libdir} -lyuv
Cflags: -I\${includedir}
EOF
echo "  libyuv.pc -> libdir=$LD"

export PKG_CONFIG_PATH=/usr/local/lib/pkgconfig:/usr/local/lib64/pkgconfig:/usr/local/share/pkgconfig
echo "--- pkg-config 验证 ---"
for p in vpx libyuv aom opus; do
  printf "  %-10s %s\n" "$p" "$(pkg-config --exists $p 2>/dev/null && echo "OK ($(pkg-config --modversion $p))" || echo MISSING)"
done
echo "--- 静态库能不能链上 (试编译一个小程序) ---"
cat > /tmp/t.c <<'EOF'
#include <aom/aom.h>
#include <libyuv.h>
#include <vpx/vpx_encoder.h>
#include <opus/opus.h>
int main(){ return aom_codec_version(); }
EOF
if gcc /tmp/t.c -o /tmp/t $(pkg-config --cflags --libs aom libyuv vpx opus 2>/dev/null) -L/usr/local/lib64 -L/usr/local/lib 2>/tmp/t-err.log; then
  echo "  链接测试 OK"
else
  echo "  链接测试失败:"; head -5 /tmp/t-err.log
fi

echo "--- 用 setsid 拉起 phase2 ---"
setsid nohup bash /tmp/phase2.sh >/dev/null 2>&1 < /dev/null &
disown 2>/dev/null || true
sleep 2
ps -ef | grep phase2 | grep -v grep | head -3
