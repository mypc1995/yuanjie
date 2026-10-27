#!/usr/bin/env bash
# 在麒麟/统信服务器上装「客户端构建环境」—— 让面板能直接在这台机器上编客户端。
#
# 用法:
#   sudo bash install-build-env.sh                        # 在线装(默认)
#   sudo bash install-build-env.sh --download <目录>       # 在有网的机器上: 装好并收集成离线包
#   sudo bash install-build-env.sh --offline  <目录>       # 在离线机器上: 从本地目录装, 全程不联网
#
# 离线包里有什么(--download 生成的):
#   rustup/    Rust 工具链          cargo/     cargo 可执行文件
#   flutter/   Flutter SDK          src/       libyuv / libaom 源码
#   rpms/      系统开发包           vendor/    cargo 依赖 + Dart 包(可选, 让编译也不联网)
#
# 幂等: 重复跑不会重装已有的东西。
set -uo pipefail

LOG=/var/log/rustdesk-builder/build-env.log
mkdir -p /var/log/rustdesk-builder
exec >>"$LOG" 2>&1
echo "===== build env 开始 $(date '+%F %T') ====="

FLUTTER_VERSION=3.24.5
FLUTTER_DIR=/opt/flutter
LIBYUV_REV=d98915a654d3564e4802a0004add46221c4e4348
AOM_VER=v3.13.3
RUSTUP_URL=https://sh.rustup.rs
FLUTTER_URL="https://storage.googleapis.com/flutter_infra_release/releases/stable/linux/flutter_linux_${FLUTTER_VERSION}-stable.tar.xz"
LIBYUV_URL="https://chromium.googlesource.com/libyuv/libyuv/+archive/${LIBYUV_REV}.tar.gz"
AOM_URL="https://aomedia.googlesource.com/aom/+archive/${AOM_VER}.tar.gz"

MODE=online
CACHE_DIR=""
VENDOR_SRC=""
ONLY_VENDOR=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --download) MODE=download; CACHE_DIR="$2"; shift 2 ;;
    --offline) MODE=offline; CACHE_DIR="$2"; shift 2 ;;
    --vendor-src) VENDOR_SRC="$2"; shift 2 ;;
    --only-vendor) ONLY_VENDOR=1; shift ;;
    -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
    *) echo "未知参数: $1" >&2; exit 1 ;;
  esac
done

step() { echo; echo "----- $* -----"; }
ok()   { echo "  [OK] $*"; }
bad()  { echo "  [!]  $*"; }

if [[ "$MODE" != "online" && -z "$CACHE_DIR" ]]; then
  echo "--download / --offline 后面要跟目录" >&2; exit 1
fi
# 下载暂存目录。有缓存目录就用它(顺便把源码包留在离线包里),
# 没有就退回 /tmp —— 注意服务器的 /tmp 只有 3.3G tmpfs。
SCRATCH="${CACHE_DIR:-/tmp}/src"
mkdir -p "$CACHE_DIR" 2>/dev/null
[[ -n "$CACHE_DIR" ]] && mkdir -p "$SCRATCH"
export PKG_CONFIG_PATH=/usr/local/lib/pkgconfig:/usr/local/lib64/pkgconfig

# ---------------------------------------------------------------- 开发包清单
DEVPKGS=(
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

# ---------------------------------------------------------------- 开发包
install_dev_packages() {
  step "1. 系统开发包"
  if [[ "$MODE" == "offline" ]]; then
    local dir="$CACHE_DIR/rpms"
    if [[ ! -d "$dir" ]] || ! ls "$dir"/*.rpm >/dev/null 2>&1; then
      bad "离线包里没有 rpms/ 目录, 跳过系统包"
      return 1
    fi
    # 做成一个本地 yum 源, 而不是直接 rpm -Uvh / yum install 本地文件。
    # 直接装本地文件时 dnf 只会拿包名去解依赖, 碰到版本对不上
    # (比如 python3 p21 和 p22) 直接报 "cannot install both" 就失败了。
    # 做成源之后, dnf 能同时看到「本机已装的」和「源里有的」, 自己选能装的。
    if command -v createrepo_c >/dev/null 2>&1; then
      createrepo_c --quiet "$dir" >/dev/null 2>&1 && ok "本地源索引已生成"
    else
      bad "没有 createrepo_c, 本地源索引没生成(离线机得自带这个包)"
    fi
    cat > /etc/yum.repos.d/rustdesk-local.repo <<EOF
[rustdesk-local]
name=远界 离线构建依赖
baseurl=file://$dir
enabled=1
gpgcheck=0
repo_gpgcheck=0
EOF
    dnf install -y --disablerepo='*' --enablerepo=rustdesk-local \
      --setopt=install_weak_deps=False "${DEVPKGS[@]}" >/tmp/buildenv-rpm.log 2>&1
    if [[ $? -ne 0 ]]; then
      bad "本地源安装有报错, 看看 /tmp/buildenv-rpm.log"
      tail -10 /tmp/buildenv-rpm.log
      return 1
    fi
    ok "系统开发包装完"
    return 0
  fi

  # 在线: 先探源里存不存在, 再装。
  # 教训: 一批里混进源里没有的包, yum 整批都不装(而且管道会吞掉退出码)。
  local avail=""
  for p in "${DEVPKGS[@]}"; do
    rpm -q "$p" >/dev/null 2>&1 && continue
    if yum info "$p" >/dev/null 2>&1; then
      avail="$avail $p"
    else
      bad "源里没有, 跳过: $p"
    fi
  done
  if [[ -n "${avail// /}" ]]; then
    echo "  安装:${avail}"
    if ! yum install -y $avail >/tmp/buildenv-yum.log 2>&1; then
      bad "整批安装失败, 改成逐个装"
      for p in $avail; do
        yum install -y "$p" >/dev/null 2>&1 || bad "装不上: $p"
      done
    fi
  else
    ok "开发包都齐了"
  fi
}

# ---------------------------------------------------------------- libyuv / libaom
# 这两个是源码编的, 必须带 -fPIC: 客户端核心是 librustdesk.so(共享库),
# 链非 PIC 的静态库会报 "relocation R_X86_64_32 ... can not be used when
# making a shared object"。
build_native_libs() {
  step "2. libyuv / libaom (带 -fPIC)"
  local srcdir="$SCRATCH"
  mkdir -p "$srcdir"

  # 源码包先备齐。download 模式下即使本机已经编好过, 也要把源码收进离线包。
  if [[ "$MODE" != "offline" ]]; then
    [[ -f "$srcdir/libyuv.tar.gz" ]] || \
      curl -fsSL --retry 3 -o "$srcdir/libyuv.tar.gz" "$LIBYUV_URL" || bad "libyuv 源码下载失败"
    [[ -f "$srcdir/aom.tar.gz" ]] || \
      curl -fsSL --retry 3 -o "$srcdir/aom.tar.gz" "$AOM_URL" || bad "aom 源码下载失败"
  fi

  # libyuv
  if pkg-config --exists libyuv 2>/dev/null && \
     [ "$(readelf -r /usr/local/lib/libyuv.a 2>/dev/null | grep -c 'R_X86_64_32 ')" = "0" ]; then
    ok "libyuv 已就绪(且是 PIC)"
  else
    rm -rf $SCRATCH/libyuv-src $SCRATCH/libyuv-build
    mkdir -p $SCRATCH/libyuv-src $SCRATCH/libyuv-build
    local ltar="$srcdir/libyuv.tar.gz"
    [[ -f "$ltar" ]] || { bad "没有 libyuv 源码包"; return 1; }
    tar -xzf "$ltar" -C $SCRATCH/libyuv-src
    cd $SCRATCH/libyuv-build || return 1
    cmake $SCRATCH/libyuv-src -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=/usr/local \
          -DCMAKE_POSITION_INDEPENDENT_CODE=ON >/tmp/libyuv-cmake.log 2>&1 || {
      bad "libyuv cmake 失败"; tail -5 /tmp/libyuv-cmake.log; return 1; }
    make -j"$(nproc)" >/tmp/libyuv-make.log 2>&1 || {
      bad "libyuv make 失败"; grep -E "error:" /tmp/libyuv-make.log | head -5; return 1; }
    make install >>/tmp/libyuv-make.log 2>&1 || { bad "libyuv install 失败"; return 1; }
    ldconfig
    ok "libyuv 装好了"
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

  # libaom
  if pkg-config --exists aom 2>/dev/null && \
     [ "$(readelf -r /usr/local/lib64/libaom.a 2>/dev/null | grep -c 'R_X86_64_32 ')" = "0" ]; then
    ok "aom 已就绪(且是 PIC)"
  else
    local ok_aom=0
    local atar="$srcdir/aom.tar.gz"
    if [[ -f "$atar" ]]; then
      rm -rf $SCRATCH/aom-src $SCRATCH/aom-build
      mkdir -p $SCRATCH/aom-src $SCRATCH/aom-build
      tar -xzf "$atar" -C $SCRATCH/aom-src
      cd $SCRATCH/aom-build || return 1
      # 麒麟只有 GCC 7.3, aom 3.13 的 AVX2 intrinsics 编不过, 所以关掉 SIMD。
      # 这个参数别改, 改了就编不过。
      cmake $SCRATCH/aom-src -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=/usr/local \
            -DENABLE_DOCS=0 -DENABLE_EXAMPLES=0 -DENABLE_TESTS=0 -DENABLE_TOOLS=0 \
            -DCONFIG_AV1_HIGHBITDEPTH=1 \
            -DCMAKE_POSITION_INDEPENDENT_CODE=ON -DBUILD_SHARED_LIBS=ON \
            -DAOM_TARGET_CPU=generic >/tmp/aom-cmake.log 2>&1 || {
        bad "aom cmake 失败"; tail -5 /tmp/aom-cmake.log; ok_aom=0; } || ok_aom=1
      if [[ "$ok_aom" = "1" ]]; then
        make -j"$(nproc)" >/tmp/aom-make.log 2>&1 && \
          make install >>/tmp/aom-make.log 2>&1 && ldconfig && ok "aom 装好了" || {
          bad "aom 编译/安装失败"; grep -E "error:" /tmp/aom-make.log | head -5; ok_aom=0; }
      fi
    else
      bad "没有 aom 源码包"
    fi
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
}

# ---------------------------------------------------------------- Rust
install_rust() {
  step "3. Rust 工具链"
  if [[ "$MODE" == "offline" ]]; then
    if [[ -d "$CACHE_DIR/rustup" && -d "$CACHE_DIR/cargo" ]]; then
      mkdir -p /root/.rustup /root/.cargo
      cp -a "$CACHE_DIR/rustup/." /root/.rustup/
      cp -a "$CACHE_DIR/cargo/." /root/.cargo/
      ok "Rust 工具链已从离线包解出"
    else
      bad "离线包里没有 rustup/ cargo/, 装不了 Rust"
    fi
  elif [[ ! -x /root/.cargo/bin/cargo ]]; then
    curl -fsSL --proto '=https' --tlsv1.2 "$RUSTUP_URL" -o /tmp/rustup.sh \
      && sh /tmp/rustup.sh -y --default-toolchain stable --profile minimal --no-modify-path \
      && ok "rustup 装好了"
  else
    ok "rustc 已就绪"
  fi
  export PATH="/root/.cargo/bin:$PATH"
  rustc --version 2>&1 | head -1
  cargo --version 2>&1 | head -1
}

# ---------------------------------------------------------------- Flutter
install_flutter() {
  step "4. Flutter SDK $FLUTTER_VERSION"
  if [[ "$MODE" == "offline" ]]; then
    if [[ -d "$CACHE_DIR/flutter/bin" ]]; then
      # 原来有就挪开, 别直接 rm —— 万一拷贝出问题, Flutter 就没了
      local backup=""
      if [[ -d /opt/flutter ]]; then
        backup="/opt/flutter.bak-$(date '+%Y%m%d%H%M%S')"
        mv /opt/flutter "$backup" 2>/dev/null
        ok "原来的 /opt/flutter 已挪到 $backup"
      fi
      cp -a "$CACHE_DIR/flutter" /opt/flutter
      ok "Flutter SDK 已从离线包解出"
      # 立刻验一下能不能用。不行就把原来那份挪回来。
      if ! timeout 120 flutter --no-version-check --version >/dev/null 2>&1; then
        bad "解出来的 Flutter 用不了, 把原来那份挪回来"
        [[ -n "$backup" ]] && mv "$backup" /opt/flutter
        return 1
      fi
      ok "Flutter 可用"
    else
      bad "离线包里没有 flutter/, 装不了 Flutter"
    fi
  elif [[ ! -x "$FLUTTER_DIR/bin/flutter" ]]; then
    echo "  下载 $FLUTTER_URL"
    # 下载到暂存目录, 不放 /tmp —— 那个是 3.3G 的 tmpfs, 662M 的包进去就满了
    if curl -fL --retry 3 -o "$SCRATCH/flutter.tar.xz" "$FLUTTER_URL"; then
      if [[ -d /opt/flutter ]]; then
        mv /opt/flutter "/opt/flutter.bak-$(date '+%Y%m%d%H%M%S')" 2>/dev/null
      fi
      tar -xf "$SCRATCH/flutter.tar.xz" -C /opt
      rm -f "$SCRATCH/flutter.tar.xz"
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
  timeout 120 flutter --no-version-check --version 2>&1 | head -2
  timeout 120 flutter --no-version-check config --enable-linux-desktop >/dev/null 2>&1 || true
  timeout 120 flutter --no-version-check config --no-analytics >/dev/null 2>&1 || true
  if timeout 60 flutter --no-version-check config --list 2>/dev/null | grep -q 'enable-linux-desktop: true'; then
    ok "flutter 的 linux 桌面支持已打开"
  else
    bad "flutter 的 linux 桌面支持没打开"
  fi
}

# ---------------------------------------------------------------- vendor
# 让 cargo / flutter 编译时也不联网: 用本地 vendor 目录替掉 crates.io 和 pub.dev
setup_vendor() {
  step "5. 本地依赖 (cargo vendor / pub-cache)"
  local vd="$CACHE_DIR/vendor"
  if [[ ! -d "$vd/cargo-vendor" ]]; then
    echo "  离线包里没有 vendor/, 编译时 cargo 仍会联网"
    return 0
  fi
  local src=/opt/rustdesk-src
  if [[ ! -d "$src" ]]; then
    bad "没有源码树 $src, 跳过 vendor 配置"
    return 0
  fi
  mkdir -p "$src/.cargo"
  local cfg="$src/.cargo/config.toml"
  # 追加, 不覆盖 —— 原文件里有 target rustflags 和 [net] 这些设置,
  # 覆盖掉会把编译配置弄丢。
  if grep -q 'source.crates-io' "$cfg" 2>/dev/null; then
    ok "$cfg 已经有 source 配置, 不动它"
  else
    [[ -f "$cfg" ]] && cp -f "$cfg" "$cfg.bak"
    sed "s|__VENDOR_DIR__|$vd/cargo-vendor|" "$vd/cargo-vendor-config.toml" >> "$cfg"
    ok "已把 vendor 配置追加到 $cfg (原文件备份为 .bak)"
  fi
  # pub-cache 放哪都行, 用环境变量指过去
  export PUB_CACHE="$vd/pub-cache"
  ok "PUB_CACHE=$PUB_CACHE"
}

# ---------------------------------------------------------------- 环境变量
write_env() {
  step "6. 写 build.env (面板靠它找到工具链)"
  mkdir -p /etc/rustdesk-panel
  local vendor_env=""
  [[ -n "${PUB_CACHE:-}" ]] && vendor_env="PUB_CACHE=$PUB_CACHE"$'\n'
  cat > /etc/rustdesk-panel/build.env <<EOF
# 由 install-build-env.sh 生成: 构建工具链的 PATH 与 pkg-config 路径。
# 面板派发的构建任务继承这里, 缺了这两行就会报「找不到 cargo」「找不到 gstreamer」。
PATH=/root/.cargo/bin:${FLUTTER_DIR}/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
PKG_CONFIG_PATH=/usr/local/lib/pkgconfig:/usr/local/lib64/pkgconfig
${vendor_env}RUSTDESK_BUILD_SRC=/opt/rustdesk-src
RUSTDESK_BUILDER_ROOT=/var/lib/rustdesk-builder
CARGO_HOME=/root/.cargo
RUSTUP_HOME=/root/.rustup
EOF
  chmod 644 /etc/rustdesk-panel/build.env
  ok "已写 /etc/rustdesk-panel/build.env"

  UNIT=/etc/systemd/system/rustdesk-panel.service
  if [[ -f "$UNIT" ]] && ! grep -q 'EnvironmentFile' "$UNIT"; then
    sed -i '/^ExecStart=\/usr\/bin\/python3/i EnvironmentFile=-/etc/rustdesk-panel/build.env' "$UNIT"
    ok "已给面板单元加上 EnvironmentFile"
  fi
  systemctl daemon-reload
  systemctl restart rustdesk-panel 2>/dev/null || true
  sleep 3
}

# ---------------------------------------------------------------- 收集成离线包
collect_cache() {
  step "收集离线包 -> $CACHE_DIR"
  mkdir -p "$CACHE_DIR/src" "$CACHE_DIR/rpms" "$CACHE_DIR/vendor"

  # Rust
  if [[ -d /root/.rustup ]]; then
    mkdir -p "$CACHE_DIR/rustup"
    cp -a /root/.rustup/. "$CACHE_DIR/rustup/"
    ok "rustup/ 已收"
  fi
  if [[ -d /root/.cargo ]]; then
    mkdir -p "$CACHE_DIR/cargo"
    # 只收工具本身(bin + env)。registry 和 git 那些是下载下来的 crate 缓存,
    # 3GB 多, 离线包不需要 —— 真正要的是 vendor 目录(在 vendor/ 里)。
    cp -a /root/.cargo/bin "$CACHE_DIR/cargo/" 2>/dev/null
    cp -a /root/.cargo/env "$CACHE_DIR/cargo/" 2>/dev/null
    cp -f /root/.cargo/config.toml "$CACHE_DIR/cargo/" 2>/dev/null
    ok "cargo/ 已收 (只收工具, 不收 crate 缓存)"
  fi

  # Flutter
  if [[ -d "$FLUTTER_DIR" ]]; then
    mkdir -p "$CACHE_DIR/flutter"
    cp -a "$FLUTTER_DIR/." "$CACHE_DIR/flutter/"
    # .git 必须留着(426MB)。Flutter 的启动脚本要靠它算版本号,
    # 没有 .git 它就会去 git fetch —— 内网机器上 github 常被劫持成假 IP,
    # fetch 挂住不动, 整个 flutter 命令就卡死了。
    ok "flutter/ 已收 (含 .git, 去掉它 Flutter 会卡在版本检查上)"
  fi

  # 源码包(在线模式下 build_native_libs 已经下到 $CACHE_DIR/src 了)
  [[ -f "$CACHE_DIR/src/libyuv.tar.gz" ]] && ok "src/libyuv.tar.gz 已在"
  [[ -f "$CACHE_DIR/src/aom.tar.gz" ]] && ok "src/aom.tar.gz 已在"

  # RPM: 抓完整依赖闭包。
  # 注意别按「下载机上已装」过滤 —— 下载机有的包离线机未必有, 滤掉就装不上了。
  # 要让离线机自己判断哪些还需要。
  # 直接下到缓存目录, 不走 /tmp —— 服务器的 /tmp 是 3.3G 的 tmpfs,
  # 671M 的包在里面再复制一份就把它撑满了, 撑满之后连文件都写不进去
  # (表现是上传的文件变成 0 字节)。
  echo "  抓系统开发包..."
  dnf download --resolve --alldeps --setopt=install_weak_deps=False \
    --destdir="$CACHE_DIR/rpms" "${DEVPKGS[@]}" >/tmp/rpmdl.log 2>&1
  local kept
  kept=$(ls "$CACHE_DIR/rpms"/*.rpm 2>/dev/null | wc -l)
  ok "rpms/ 已收 ($kept 个, $(du -sh "$CACHE_DIR/rpms" | cut -f1))"

  # 把 rpms/ 做成真正的 yum 仓库。离线机上 dnf 靠它解依赖 ——
  # 直接给 rpm 文件的话, dnf 只会拿包名去解, 碰到版本对不上(比如
  # python3 p21 和 p22) 直接报 "cannot install both" 就失败了。
  # 做成源之后, dnf 能同时看到「本机已装的」和「源里有的」, 自己选能装的。
  if command -v createrepo_c >/dev/null 2>&1; then
    createrepo_c --quiet "$CACHE_DIR/rpms" >/dev/null 2>&1 && ok "rpms/ 已做成 yum 仓库"
  else
    bad "没有 createrepo_c, 源索引没生成"
  fi

  # vendor: 从已有的离线包拷(跨平台通用), 或者从 --vendor-src 指定的地方
  local vsrc="$VENDOR_SRC"
  if [[ -z "$vsrc" ]]; then
    for cand in \
      "/opt/rustdesk-build-agent/toolchain" \
      "$HOME/rustdesk-offline-bundle/toolchain" \
      "/mnt/rustdesk-deployment/02-build-agent-windows/rustdesk-offline-bundle/toolchain" \
      "/opt/rustdesk-builder/toolchain"; do
      [[ -d "$cand/cargo-vendor" ]] && vsrc="$cand" && break
    done
  fi
  if [[ -n "$vsrc" && -d "$vsrc/cargo-vendor" ]]; then
    mkdir -p "$CACHE_DIR/vendor/cargo-vendor"
    cp -a "$vsrc/cargo-vendor/." "$CACHE_DIR/vendor/cargo-vendor/"
    ok "vendor/cargo-vendor 已收 (来自 $vsrc)"
    if [[ -d "$vsrc/pub-cache" ]]; then
      mkdir -p "$CACHE_DIR/vendor/pub-cache"
      cp -a "$vsrc/pub-cache/." "$CACHE_DIR/vendor/pub-cache/"
      ok "vendor/pub-cache 已收"
    fi
    if [[ -f "$vsrc/cargo-vendor-config.toml" ]]; then
      cp -f "$vsrc/cargo-vendor-config.toml" "$CACHE_DIR/vendor/cargo-vendor-config.toml"
    fi
  else
    bad "没找到 cargo-vendor 来源, 离线包里的编译仍会联网"
    bad "  可以用 --vendor-src <路径> 指定"
  fi

  cat > "$CACHE_DIR/MANIFEST.txt" <<EOF
远界 构建环境离线包
生成时间: $(date '+%F %T')
来源机器: $(hostname)

用法(离线机器上):
  sudo bash install-build-env.sh --offline <本目录>

内容:
  rustup/    Rust 工具链
  cargo/     cargo 可执行文件
  flutter/   Flutter SDK $FLUTTER_VERSION
  src/       libyuv / libaom 源码
  rpms/      系统开发包
  vendor/    cargo 依赖 + Dart 包(有的话编译也不联网)
EOF
  ok "MANIFEST.txt 已写"
  echo
  echo "  离线包大小: $(du -sh "$CACHE_DIR" 2>/dev/null | cut -f1)"
  echo "  打包: tar -czf build-env-linux.tar.gz -C $(dirname "$CACHE_DIR") $(basename "$CACHE_DIR")"
}

# ---------------------------------------------------------------- 自检
verify() {
  step "7. 自检"
  export PKG_CONFIG_PATH=/usr/local/lib/pkgconfig:/usr/local/lib64/pkgconfig
  echo "--- 工具 ---"
  for c in cargo rustc flutter flutter_rust_bridge_codegen cmake ninja clang; do
    printf '  %-28s %s\n' "$c" "$(command -v $c || echo 缺)"
  done
  echo "--- 原生库 (pkg-config) ---"
  for p in gtk+-3.0 dbus-1 gstreamer-1.0 gstreamer-base-1.0 xdamage xrandr xfixes \
           xcursor xi xtst xinerama alsa uuid openssl libyuv aom opus; do
    printf '  %-20s %s\n' "$p" "$(pkg-config --exists "$p" 2>/dev/null && echo OK || echo 缺)"
  done
  echo "--- 静态库 PIC (链共享库必须是 PIC) ---"
  printf '  %-20s %s\n' "libyuv.a" "$(readelf -r /usr/local/lib/libyuv.a 2>/dev/null | grep -c 'R_X86_64_32 ') (应为 0)"
  printf '  %-20s %s\n' "libaom.a" "$(readelf -r /usr/local/lib64/libaom.a 2>/dev/null | grep -c 'R_X86_64_32 ') (应为 0)"
  echo "--- 面板进程环境 ---"
  PID=$(systemctl show -p MainPID --value rustdesk-panel 2>/dev/null)
  if [[ -n "$PID" && "$PID" != "0" ]]; then
    tr '\0' '\n' < /proc/$PID/environ 2>/dev/null | grep -E '^(PATH|PKG_CONFIG_PATH|PUB_CACHE)='
  fi
  echo "--- 源码树 ---"
  if [[ -f /opt/rustdesk-src/src/server_preset.rs ]]; then
    ok "远界版本"
  else
    bad "不是远界版本 (缺 src/server_preset.rs)"
  fi
}

# ---------------------------------------------------------------- 主流程
if [[ "$MODE" == "download" ]]; then
  # 有网机器: 正常装一遍, 然后收集成离线包
  install_dev_packages
  build_native_libs
  install_rust
  install_flutter
  if [[ "$ONLY_VENDOR" -eq 0 ]]; then
    write_env
    verify
  fi
  collect_cache
elif [[ "$MODE" == "offline" ]]; then
  # 离线机器: 全程不联网
  echo "离线模式, 缓存目录: $CACHE_DIR"
  [[ -d "$CACHE_DIR" ]] || { echo "缓存目录不存在: $CACHE_DIR" >&2; exit 1; }
  install_dev_packages
  build_native_libs
  install_rust
  install_flutter
  setup_vendor
  write_env
  verify
else
  install_dev_packages
  build_native_libs
  install_rust
  install_flutter
  write_env
  verify
fi

echo
echo "===== build env 结束 $(date '+%F %T') ====="
echo "上面还有 [OK] 就说明环境好了, 面板「构建」页可以选 Linux x64 了"
