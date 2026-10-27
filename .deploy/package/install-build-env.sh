#!/usr/bin/env bash
# 在麒麟/统信服务器上装「客户端构建环境」—— 让面板能直接在这台机器上编客户端。
#
# 用法:
#   sudo bash install-build-env.sh
#
# 装完面板的「构建」页选 Linux x64 就能出包。
# 幂等: 重复跑不会重装已有的东西。
#
# 注意: 这一步要联网(下载工具链和依赖源码)。纯离线环境请走
# 02-build-agent-windows 那条路(Windows 构建机)。
set -uo pipefail

LOG=/var/log/rustdesk-builder/build-env.log
mkdir -p /var/log/rustdesk-builder
exec >>"$LOG" 2>&1
echo "===== build env 开始 $(date '+%F %T') ====="

FLUTTER_VERSION=3.24.5
FLUTTER_DIR=/opt/flutter
LIBYUV_REV=d98915a654d3564e4802a0004add46221c4e4348

step() { echo; echo "----- $* -----"; }
ok()   { echo "  [OK] $*"; }
bad()  { echo "  [!]  $*"; }

# ---------------------------------------------------------------- 开发包
step "1. 系统开发包"
# 源里没有的包会把整批 yum 装单回滚, 所以先逐个探存在性。
# 已知的坑: libv4l-devel / patchelf / xorg-x11-server-Xvfb 在麒麟源里没有。
NEEDED=(
  clang clang-devel llvm-devel
  ninja-build gcc-c++ libstdc++-devel make cmake nasm pkgconf
  gtk3 gtk3-devel
  dbus dbus-devel
  libX11-devel libXext-devel libXrandr-devel libXinerama-devel
  libXi-devel libXtst-devel libXdamage-devel libXfixes-devel libXcursor-devel
  mesa-libGL-devel
  alsa-lib alsa-lib-devel
  zlib-devel xz-devel
  libvpx-devel opus-devel libjpeg-turbo-devel
  uuid-devel openssl-devel
  gstreamer1-devel gstreamer1-plugins-base-devel
)
AVAIL=""
for p in "${NEEDED[@]}"; do
  if rpm -q "$p" >/dev/null 2>&1; then
    continue
  fi
  if yum info "$p" >/dev/null 2>&1; then
    AVAIL="$AVAIL $p"
  else
    bad "源里没有, 跳过: $p"
  fi
done
if [[ -n "${AVAIL// /}" ]]; then
  echo "  安装:${AVAIL}"
  # 先整批装(快)。整批里只要有一个装不上就全退, 所以失败了再逐个装,
  # 把能装的都装上 —— 之前的教训: 一批里混进源里没有的包, 整批都没装上。
  if ! yum install -y $AVAIL >/tmp/buildenv-yum.log 2>&1; then
    bad "整批安装失败, 改成逐个装"
    for p in $AVAIL; do
      yum install -y "$p" >/dev/null 2>&1 || bad "装不上: $p"
    done
  fi
else
  ok "开发包都齐了"
fi

# ---------------------------------------------------------------- libyuv
step "2. libyuv (带 -fPIC)"
export PKG_CONFIG_PATH=/usr/local/lib/pkgconfig:/usr/local/lib64/pkgconfig
build_libyuv() {
  rm -rf /tmp/libyuv-src /tmp/libyuv-build
  mkdir -p /tmp/libyuv-src /tmp/libyuv-build
  if ! curl -fsSL --retry 3 -o /tmp/libyuv.tar.gz \
       "https://chromium.googlesource.com/libyuv/libyuv/+archive/${LIBYUV_REV}.tar.gz"; then
    bad "libyuv 下载失败"; return 1
  fi
  tar -xzf /tmp/libyuv.tar.gz -C /tmp/libyuv-src
  cd /tmp/libyuv-build || return 1
  # -fPIC 必须加: 客户端核心是 librustdesk.so(共享库), 链非 PIC 的静态库会报
  # "relocation R_X86_64_32 ... can not be used when making a shared object"
  cmake /tmp/libyuv-src -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=/usr/local \
        -DCMAKE_POSITION_INDEPENDENT_CODE=ON >/tmp/libyuv-cmake.log 2>&1 || {
    bad "libyuv cmake 失败"; tail -5 /tmp/libyuv-cmake.log; return 1; }
  make -j"$(nproc)" >/tmp/libyuv-make.log 2>&1 || {
    bad "libyuv make 失败"; grep -E "error:" /tmp/libyuv-make.log | head -5; return 1; }
  make install >>/tmp/libyuv-make.log 2>&1 || { bad "libyuv install 失败"; return 1; }
  ldconfig
  return 0
}
if pkg-config --exists libyuv 2>/dev/null && [ "$(readelf -r /usr/local/lib/libyuv.a 2>/dev/null | grep -c 'R_X86_64_32 ')" = "0" ]; then
  ok "libyuv 已就绪(且是 PIC)"
else
  build_libyuv && ok "libyuv 装好了" || bad "libyuv 没成功"
fi
mkdir -p /usr/local/lib/pkgconfig
cat > /usr/local/lib/pkgconfig/libyuv.pc <<'EOF'
prefix=/usr/local
exec_prefix=${prefix}
libdir=/usr/local/lib
includedir=/usr/local/include

Name: libyuv
Description: libyuv - YUV scaling and conversion library
Version: 1916
Libs: -L${libdir} -lyuv
Cflags: -I${includedir}
EOF

# ---------------------------------------------------------------- libaom
step "3. libaom (带 -fPIC)"
# 麒麟只有 GCC 7.3, aom 3.13 的 AVX2 intrinsics 编不过, 所以关掉 SIMD
# (AOM_TARGET_CPU=generic)。这个参数别改, 改了就编不过。
build_aom() {
  for ver in v3.13.3 v3.9.1 v3.0.0; do
    rm -rf /tmp/aom-src /tmp/aom-build
    mkdir -p /tmp/aom-src /tmp/aom-build
    if ! curl -fsSL --retry 3 -o /tmp/aom.tar.gz \
          "https://aomedia.googlesource.com/aom/+archive/${ver}.tar.gz"; then
      bad "$ver 下载失败"; continue
    fi
    tar -xzf /tmp/aom.tar.gz -C /tmp/aom-src
    cd /tmp/aom-build || continue
    cmake /tmp/aom-src -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=/usr/local \
          -DENABLE_DOCS=0 -DENABLE_EXAMPLES=0 -DENABLE_TESTS=0 -DENABLE_TOOLS=0 \
          -DCONFIG_AV1_HIGHBITDEPTH=1 \
          -DCMAKE_POSITION_INDEPENDENT_CODE=ON -DBUILD_SHARED_LIBS=ON \
          -DAOM_TARGET_CPU=generic >/tmp/aom-cmake.log 2>&1 || {
      bad "$ver cmake 失败"; tail -5 /tmp/aom-cmake.log; continue; }
    make -j"$(nproc)" >/tmp/aom-make.log 2>&1 || {
      bad "$ver make 失败"; grep -E "error:" /tmp/aom-make.log | head -5; continue; }
    make install >>/tmp/aom-make.log 2>&1 || { bad "$ver install 失败"; continue; }
    ldconfig
    ok "aom $ver 装好了"
    return 0
  done
  return 1
}
if pkg-config --exists aom 2>/dev/null && [ "$(readelf -r /usr/local/lib64/libaom.a 2>/dev/null | grep -c 'R_X86_64_32 ')" = "0" ]; then
  ok "aom 已就绪(且是 PIC)"
else
  build_aom || bad "aom 没成功"
fi
mkdir -p /usr/local/lib64/pkgconfig /usr/local/lib/pkgconfig
cat > /usr/local/lib64/pkgconfig/aom.pc <<'EOF'
prefix=/usr/local
exec_prefix=${prefix}
libdir=${exec_prefix}/lib64
includedir=${prefix}/include

Name: aom
Description: Alliance for Open Media AV1 codec library
Version: 3.13.3
Libs: -L${libdir} -laom
Libs.private: -lm -lpthread
Cflags: -I${includedir}
EOF
cp /usr/local/lib64/pkgconfig/aom.pc /usr/local/lib/pkgconfig/aom.pc

# ---------------------------------------------------------------- Rust
step "4. Rust 工具链"
if ! /root/.cargo/bin/cargo --version >/dev/null 2>&1; then
  curl -fsSL --proto '=https' --tlsv1.2 https://sh.rustup.rs -o /tmp/rustup.sh \
    && sh /tmp/rustup.sh -y --default-toolchain stable --profile minimal --no-modify-path \
    && ok "rustup 装好了"
else
  ok "rustc 已就绪"
fi
export PATH="/root/.cargo/bin:$PATH"
rustc --version 2>&1 | head -1
cargo --version 2>&1 | head -1

# ---------------------------------------------------------------- Flutter
step "5. Flutter SDK $FLUTTER_VERSION"
if [ ! -x "$FLUTTER_DIR/bin/flutter" ]; then
  URL="https://storage.googleapis.com/flutter_infra_release/releases/stable/linux/flutter_linux_${FLUTTER_VERSION}-stable.tar.xz"
  echo "  下载 $URL"
  if curl -fL --retry 3 -o /tmp/flutter.tar.xz "$URL"; then
    rm -rf /opt/flutter
    tar -xf /tmp/flutter.tar.xz -C /opt
    ok "flutter 解压完成"
  else
    bad "flutter 下载失败"
  fi
else
  ok "flutter 已就绪"
fi
export PATH="$FLUTTER_DIR/bin:/root/.cargo/bin:$PATH"
git config --global --add safe.directory "$FLUTTER_DIR" 2>/dev/null || true
# --no-version-check 必须带。Flutter 的版本新鲜度检查会去 git fetch github,
# 内网机器上 github 常被劫持成假 IP(198.18.x.x), fetch 挂住不动,
# 整个脚本就卡在这儿。带上这个开关它就不去 fetch 了。
timeout 120 flutter --no-version-check --version 2>&1 | head -3
# Linux 桌面支持默认是关的, 不开编不出 Linux 客户端
timeout 120 flutter --no-version-check config --enable-linux-desktop >/dev/null 2>&1 || true
timeout 120 flutter --no-version-check config --no-analytics >/dev/null 2>&1 || true
if timeout 60 flutter --no-version-check config --list 2>/dev/null | grep -q 'enable-linux-desktop: true'; then
  ok "flutter 的 linux 桌面支持已打开"
else
  bad "flutter 的 linux 桌面支持没打开"
fi

# ---------------------------------------------------------------- 环境变量
step "6. 写 build.env (面板靠它找到工具链)"
mkdir -p /etc/rustdesk-panel
cat > /etc/rustdesk-panel/build.env <<EOF
# 由 install-build-env.sh 生成: 构建工具链的 PATH 与 pkg-config 路径。
# 面板派发的构建任务继承这里, 缺了这两行就会报「找不到 cargo」「找不到 gstreamer」。
PATH=/root/.cargo/bin:${FLUTTER_DIR}/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
PKG_CONFIG_PATH=/usr/local/lib/pkgconfig:/usr/local/lib64/pkgconfig
RUSTDESK_BUILD_SRC=/opt/rustdesk-src
RUSTDESK_BUILDER_ROOT=/var/lib/rustdesk-builder
CARGO_HOME=/root/.cargo
RUSTUP_HOME=/root/.rustup
EOF
chmod 644 /etc/rustdesk-panel/build.env
ok "已写 /etc/rustdesk-panel/build.env"

# 面板单元里要有 EnvironmentFile 才读得到
UNIT=/etc/systemd/system/rustdesk-panel.service
if [[ -f "$UNIT" ]] && ! grep -q 'EnvironmentFile' "$UNIT"; then
  sed -i '/^ExecStart=\/usr\/bin\/python3/i EnvironmentFile=-/etc/rustdesk-panel/build.env' "$UNIT"
  ok "已给面板单元加上 EnvironmentFile"
fi
systemctl daemon-reload
systemctl restart rustdesk-panel 2>/dev/null || true
sleep 3

# ---------------------------------------------------------------- 自检
step "7. 自检"
export PKG_CONFIG_PATH=/usr/local/lib/pkgconfig:/usr/local/lib64/pkgconfig
echo "--- 工具 ---"
for c in cargo rustc flutter flutter_rust_bridge_codegen cmake ninja clang; do
  printf '  %-28s %s\n' "$c" "$(command -v $c || echo 缺)"
done
echo "--- 原生库 (pkg-config) ---"
for p in gtk+-3.0 dbus-1 gstreamer-1.0 gstreamer-base-1.0 xdamage xrandr xfixes \
         xcursor xi xtst xinerama alsa uuid openssl libyuv aom opus; do
  printf '  %-20s %s\n' "$p" "$(pkg-config --exists $p 2>/dev/null && echo OK || echo 缺)"
done
echo "--- 静态库 PIC (链共享库必须是 PIC) ---"
printf '  %-20s %s\n' "libyuv.a" "$(readelf -r /usr/local/lib/libyuv.a 2>/dev/null | grep -c 'R_X86_64_32 ') (应为 0)"
printf '  %-20s %s\n' "libaom.a" "$(readelf -r /usr/local/lib64/libaom.a 2>/dev/null | grep -c 'R_X86_64_32 ') (应为 0)"
echo "--- 面板进程环境 ---"
PID=$(systemctl show -p MainPID --value rustdesk-panel 2>/dev/null)
if [[ -n "$PID" && "$PID" != "0" ]]; then
  tr '\0' '\n' < /proc/$PID/environ 2>/dev/null | grep -E '^(PATH|PKG_CONFIG_PATH)='
fi
echo "--- 源码树 ---"
if [[ -f /opt/rustdesk-src/src/server_preset.rs ]]; then
  ok "远界版本"
else
  bad "不是远界版本 (缺 src/server_preset.rs)"
fi

echo
echo "===== build env 结束 $(date '+%F %T') ====="
echo "上面还有 [OK] 就说明环境好了, 面板「构建」页可以选 Linux x64 了"
