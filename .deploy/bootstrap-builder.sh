#!/bin/bash
# 在麒麟服务器上准备 远界 客户端构建环境。
# 幂等: 可以重复跑。日志写到 /var/log/rustdesk-builder/bootstrap.log
set -uo pipefail

LOG=/var/log/rustdesk-builder/bootstrap.log
mkdir -p /var/log/rustdesk-builder
exec >>"$LOG" 2>&1
echo "===== bootstrap 开始 $(date '+%F %T') ====="

step() { echo; echo "----- $* -----"; }

# 尝试多个包名, 装上第一个能找到的
try_install() {
  local label="$1"; shift
  for p in "$@"; do
    if rpm -q "$p" >/dev/null 2>&1; then
      echo "  $label: $p 已装"
      return 0
    fi
    if timeout 60 dnf --noplugins --setopt=timeout=10 --setopt=retries=0 -y install "$p" >/tmp/dnf.log 2>&1; then
      echo "  $label: 装了 $p"
      return 0
    fi
  done
  echo "  $label: 都没装上 (试过: $*)"
  return 1
}

step "1. 系统构建依赖"
try_install "clang"        clang clang-devel llvm-devel
try_install "ninja"        ninja-build
try_install "gcc-c++"      gcc-c++ gcc-c++-devel
try_install "gtk3"         gtk3 gtk3-devel
try_install "X11"          libX11-devel libX11 libX11-gtk3-devel
try_install "Xext"         libXext-devel
try_install "Xrandr"       libXrandr-devel
try_install "Xinerama"     libXinerama-devel
try_install "Xi"           libXi-devel
try_install "mesa-GL"      mesa-libGL-devel mesa-libGL
try_install "alsa"         alsa-lib-devel alsa-lib
try_install "stdc++"       libstdc++-devel gcc-c++
try_install "zlib"         zlib-devel zlib-ng-compat-devel zlib
try_install "lzma"         xz-devel liblzma-devel
try_install "libvpx"       libvpx-devel
try_install "opus"         opus-devel
try_install "jpeg-turbo"   libjpeg-turbo-devel
try_install "cmake/ninja/pkgconfig" cmake pkgconf-pkg-config make
try_install "nasm"         nasm

step "2. libyuv (Google, 麒麟源里没有, 从源码编)"
if pkg-config --exists libyuv 2>/dev/null; then
  echo "  libyuv 已就绪"
else
  cd /tmp
  curl -fsSL -o libyuv.tar.gz "https://codeload.github.com/lemenkov/libyuv/tar.gz/refs/heads/master" \
    || curl -fsSL -o libyuv.tar.gz "https://codeload.github.com/lemenkov/libyuv/tar.gz/refs/tags/v0.0.0"
  rm -rf /tmp/libyuv-*
  tar -xzf libyuv.tar.gz -C /tmp
  d=$(ls -d /tmp/libyuv-* | head -1)
  mkdir -p /tmp/libyuv-build && cd /tmp/libyuv-build
  cmake "$d" -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=/usr/local \
        -DBUILD_SHARED_LIBS=OFF -DLIBYUV_DISABLE_TESTS=ON >/dev/null 2>&1
  make -j"$(nproc)" >/dev/null 2>&1 && make install >/dev/null 2>&1
  ldconfig
  pkg-config --exists libyuv && echo "  libyuv 装好了" || echo "  libyuv 失败"
fi

step "3. libaom (AV1 解码, 麒麟源里没有, 从源码编)"
if pkg-config --exists aom 2>/dev/null; then
  echo "  aom 已就绪"
else
  cd /tmp
  AOM_VER=3.9.1
  curl -fsSL -o aom.tar.gz "https://codeload.github.com/Alliance-for-Open-Media/AOM/archive/refs/tags/v${AOM_VER}.tar.gz" \
    || curl -fsSL -o aom.tar.gz "https://codeload.github.com/jack7459/aom/archive/refs/heads/master"
  rm -rf /tmp/aom-*
  tar -xzf aom.tar.gz -C /tmp
  d=$(ls -d /tmp/aom-* | head -1)
  mkdir -p /tmp/aom-build && cd /tmp/aom-build
  cmake "$d" -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=/usr/local \
        -DENABLE_DOCS=0 -DENABLE_EXAMPLES=0 -DENABLE_TESTS=0 -DENABLE_TOOLS=0 \
        -DCONFIG_AV1_HIGHBITDEPTH=1 >/dev/null 2>&1
  make -j"$(nproc)" >/dev/null 2>&1 && make install >/dev/null 2>&1
  ldconfig
  pkg-config --exists aom && echo "  aom 装好了" || echo "  aom 失败"
fi

step "4. Rust 工具链"
if ! command -v rustc >/dev/null; then
  curl -fsSL --proto '=https' --tlsv1.2 https://sh.rustup.rs -o /tmp/rustup.sh
  sh /tmp/rustup.sh -y --default-toolchain stable --profile minimal --no-modify-path >/dev/null 2>&1
  . "$HOME/.cargo/env"
fi
export PATH="$HOME/.cargo/bin:$PATH"
rustc --version; cargo --version
rustup target add aarch64-unknown-linux-gnu >/dev/null 2>&1 || true

step "5. Flutter SDK 3.24.5"
FLUTTER_DIR=/opt/flutter
if [ ! -x "$FLUTTER_DIR/bin/flutter" ]; then
  cd /tmp
  URL=$(curl -fsSL https://storage.googleapis.com/flutter_infra_release/releases/releases_linux.json \
        | python3 -c "
import json,sys
d=json.load(sys.stdin)
h=d['current_release']['stable']
for r in d['releases']:
    if r['hash']==h and r['channel']=='stable':
        print(d['base_url']+'/'+r['archive']); break
")
  echo "  下载 $URL"
  curl -fL --retry 3 -o flutter.tar.xz "$URL"
  rm -rf /opt/flutter && mkdir -p /opt
  tar -xf flutter.tar.xz -C /opt
fi
export PATH="$FLUTTER_DIR/bin:$PATH"
git config --global --add safe.directory "$FLUTTER_DIR" 2>/dev/null || true
flutter --version 2>&1 | head -3
flutter config --no-analytics >/dev/null 2>&1 || true
flutter precache --linux --universal >/dev/null 2>&1 || true

step "6. 远界源码"
SRC=/opt/rustdesk-src
if [ ! -d "$SRC/.git" ]; then
  rm -rf "$SRC"
  git clone --depth 1 https://github.com/rustdesk/rustdesk.git "$SRC" || exit 1
fi
cd "$SRC"
if [ ! -d libs/hbb_common/src ]; then
  git clone --depth 1 https://github.com/rustdesk/hbb_common.git libs/hbb_common || exit 1
fi
echo "  源码: $(git rev-parse --short HEAD)  hbb_common: $(cd libs/hbb_common && git rev-parse --short HEAD)"

step "7. 检查 pkg-config 能否找到全部原生库"
for p in vpx libyuv aom opus; do
  printf "  %-10s %s\n" "$p" "$(pkg-config --exists $p 2>/dev/null && echo OK || echo MISSING)"
done

echo
echo "===== bootstrap 结束 $(date '+%F %T') ====="
echo "BUILD_ENV_READY=1" >> "$LOG"
