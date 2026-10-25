#!/usr/bin/env bash
# RustDesk 自建服务器 —— 一键离线部署
#
# 本包完全自包含: 服务端二进制、面板、面板的 Python 依赖都在里面,
# 安装过程不联网、不下载任何东西。
#
# 用法:
#   sudo bash install.sh
#   sudo bash install.sh --server-host <服务器IP> --panel-password 'MyPass'
#
# 选项:
#   --server-host <ip>    对外地址(中继服务器地址), 默认自动探测本机第一个非回环 IP
#   --panel-user <name>   面板账号, 默认 admin
#   --panel-password <pw> 面板密码, 默认随机生成并打印
#   --panel-port <port>   面板端口, 默认 21120
#   --no-firewall         不动 firewalld
#   --update-src          重新解压客户端源码树 (默认已存在就跳过)
#   --uninstall           卸载(保留配置和密钥)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

SERVER_HOST=""
PANEL_USER="admin"
PANEL_PASSWORD=""
PANEL_PORT="21120"
DO_FIREWALL=1
UPDATE_SRC=0
UNINSTALL=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --server-host) SERVER_HOST="$2"; shift 2 ;;
    --panel-user) PANEL_USER="$2"; shift 2 ;;
    --panel-password) PANEL_PASSWORD="$2"; shift 2 ;;
    --panel-port) PANEL_PORT="$2"; shift 2 ;;
    --no-firewall) DO_FIREWALL=0; shift ;;
    --update-src) UPDATE_SRC=1; shift ;;
    --uninstall) UNINSTALL=1; shift ;;
    -h|--help) sed -n '2,21p' "$0"; exit 0 ;;
    *) echo "未知参数: $1" >&2; exit 1 ;;
  esac
done

BIN_DIR=/usr/bin
SERVER_DIR=/var/lib/rustdesk-server
LOG_DIR=/var/log/rustdesk-server
PANEL_DIR=/opt/rustdesk-panel
PANEL_DATA=/var/lib/rustdesk-panel
PANEL_CONF_DIR=/etc/rustdesk-panel
UNIT_DIR=/etc/systemd/system
SVC_USER=rustdesk

log()  { echo -e "  \033[32m✓\033[0m $*"; }
warn() { echo -e "  \033[33m!\033[0m $*"; }
die()  { echo -e "  \033[31m✗\033[0m $*" >&2; exit 1; }

# ---------------------------------------------------------------- 卸载
if [[ $UNINSTALL -eq 1 ]]; then
  echo "==> 卸载 RustDesk 服务"
  systemctl disable --now rustdesk-hbbs rustdesk-hbbr rustdesk-panel 2>/dev/null || true
  rm -f "$UNIT_DIR/rustdesk-hbbs.service" "$UNIT_DIR/rustdesk-hbbr.service" "$UNIT_DIR/rustdesk-panel.service"
  rm -f "$BIN_DIR/hbbs" "$BIN_DIR/hbbr" "$BIN_DIR/rustdesk-utils"
  rm -rf "$PANEL_DIR"
  systemctl daemon-reload
  log "已卸载。配置与密钥保留在 $SERVER_DIR 和 $PANEL_DATA"
  exit 0
fi

# ---------------------------------------------------------------- 环境检查
echo "==> 检查环境"
[[ $EUID -eq 0 ]] || die "请用 root 运行 (sudo bash install.sh)"
command -v systemctl >/dev/null || die "找不到 systemctl，这不是 systemd 系统"
command -v python3 >/dev/null || die "找不到 python3"
PY_VER=$(python3 -c 'import sys; print("%d.%d" % sys.version_info[:2])')
log "python3 $PY_VER"

ARCH=$(uname -m)
case "$ARCH" in
  x86_64|amd64)  ARCH_DIR=amd64 ;;
  aarch64|arm64) ARCH_DIR=arm64 ;;
  *) die "不支持的架构: $ARCH (支持 x86_64 / aarch64)" ;;
esac
log "架构 $ARCH -> $ARCH_DIR"

[[ -d "$SCRIPT_DIR/server/$ARCH_DIR" ]] || die "包里没有 $ARCH_DIR 的二进制"
[[ -d "$SCRIPT_DIR/panel" ]] || die "包里缺少 panel 目录"

if [[ -z "$SERVER_HOST" ]]; then
  SERVER_HOST=$(ip -4 route get 1.1.1.1 2>/dev/null | awk '{print $7; exit}')
  [[ -n "$SERVER_HOST" ]] || SERVER_HOST=$(hostname -I 2>/dev/null | awk '{print $1}')
  [[ -n "$SERVER_HOST" ]] || SERVER_HOST="127.0.0.1"
fi
log "对外地址 $SERVER_HOST"

if [[ -z "$PANEL_PASSWORD" ]]; then
  PANEL_PASSWORD=$(head -c 18 /dev/urandom | base64 | tr -dc 'A-Za-z0-9' | head -c 16)
  GEN_PASS=1
fi

# ---------------------------------------------------------------- 服务端
echo "==> 安装服务端 (hbbs / hbbr)"
install -m 755 "$SCRIPT_DIR/server/$ARCH_DIR/hbbs" "$BIN_DIR/hbbs"
install -m 755 "$SCRIPT_DIR/server/$ARCH_DIR/hbbr" "$BIN_DIR/hbbr"
install -m 755 "$SCRIPT_DIR/server/$ARCH_DIR/rustdesk-utils" "$BIN_DIR/rustdesk-utils"
log "二进制已装到 $BIN_DIR"

getent group "$SVC_USER" >/dev/null || groupadd -r "$SVC_USER"
id "$SVC_USER" >/dev/null 2>&1 || useradd -r -g "$SVC_USER" -d "$SERVER_DIR" -s /sbin/nologin "$SVC_USER"
mkdir -p "$SERVER_DIR" "$LOG_DIR"
chown -R "$SVC_USER:$SVC_USER" "$SERVER_DIR"
chmod 750 "$SERVER_DIR"
chmod 755 "$LOG_DIR"
log "用户 $SVC_USER / 目录就绪"

sed -e "s|@@SERVER_HOST@@|$SERVER_HOST|g" \
    "$SCRIPT_DIR/server/systemd/rustdesk-hbbs.service" > "$UNIT_DIR/rustdesk-hbbs.service"
sed -e "s|@@SERVER_HOST@@|$SERVER_HOST|g" \
    "$SCRIPT_DIR/server/systemd/rustdesk-hbbr.service" > "$UNIT_DIR/rustdesk-hbbr.service"
chmod 644 "$UNIT_DIR/rustdesk-hbbs.service" "$UNIT_DIR/rustdesk-hbbr.service"
log "systemd 单元已安装"

# ---------------------------------------------------------------- 面板
echo "==> 安装管理面板"
mkdir -p "$PANEL_DIR" "$PANEL_DATA" "$PANEL_CONF_DIR"
cp -r "$SCRIPT_DIR/panel/." "$PANEL_DIR/"
chmod 755 "$PANEL_DIR/panel.py"

if [[ ! -f "$PANEL_CONF_DIR/panel.conf" ]]; then
  PANEL_TOKEN=$(head -c 24 /dev/urandom | base64 | tr -dc 'A-Za-z0-9' | head -c 20)
  cat > "$PANEL_CONF_DIR/panel.conf" <<EOF
user=$PANEL_USER
password=$PANEL_PASSWORD
agent_token=$PANEL_TOKEN
EOF
  chmod 600 "$PANEL_CONF_DIR/panel.conf"
  log "面板账号已创建 (密码 600 权限保存在 $PANEL_CONF_DIR/panel.conf)"
else
  warn "面板配置已存在，保持原样: $PANEL_CONF_DIR/panel.conf"
  GEN_PASS=0
  # 旧配置里可能没有 agent_token, 补一个
  if ! grep -q '^agent_token=' "$PANEL_CONF_DIR/panel.conf"; then
    PANEL_TOKEN=$(head -c 24 /dev/urandom | base64 | tr -dc 'A-Za-z0-9' | head -c 20)
    echo "agent_token=$PANEL_TOKEN" >> "$PANEL_CONF_DIR/panel.conf"
    log "已补充 agent_token"
  fi
fi

PANEL_SECRET=$(head -c 32 /dev/urandom | base64 | tr -dc 'A-Za-z0-9')
sed -e "s|@@PANEL_PORT@@|$PANEL_PORT|g" \
    -e "s|@@PANEL_SECRET@@|$PANEL_SECRET|g" \
    "$SCRIPT_DIR/panel/rustdesk-panel.service" > "$UNIT_DIR/rustdesk-panel.service"
chmod 644 "$UNIT_DIR/rustdesk-panel.service"
log "面板 systemd 单元已安装"

# ---------------------------------------------------------------- 构建服务
echo "==> 安装客户端构建服务"
BUILDER_DIR=/opt/rustdesk-builder
mkdir -p "$BUILDER_DIR" /var/lib/rustdesk-builder/jobs /var/lib/rustdesk-builder/artifacts
if [[ -d "$SCRIPT_DIR/builder" ]]; then
  cp -r "$SCRIPT_DIR/builder/." "$BUILDER_DIR/"
  chmod 755 "$BUILDER_DIR"/*.py 2>/dev/null || true
  log "构建脚本已装到 $BUILDER_DIR"
else
  warn "包里没有 builder 目录, 跳过构建服务"
fi

# ---------------------------------------------------------------- 客户端源码树
# 面板上的「构建」页选 linux 目标时, 是在这台服务器上直接编译的,
# 所以源码树必须在这儿。没有它, Linux 目标一定失败。
echo "==> 准备客户端源码树"
SRC_DIR=/opt/rustdesk-src
SRC_TARBALL="$(ls "$SCRIPT_DIR"/src/*.tar.gz 2>/dev/null | head -1 || true)"
if [[ -n "$SRC_TARBALL" ]]; then
  if [[ -f "$SRC_DIR/src/server_preset.rs" && "$UPDATE_SRC" -eq 0 ]]; then
    log "源码树已就位: $SRC_DIR (要换版本请加 --update-src)"
  else
    mkdir -p "$SRC_DIR"
    tar -xzf "$SRC_TARBALL" -C "$SRC_DIR"
    # 源码树是在 Windows 上打的包, 权限位不可用, 统一收拾一下
    chmod -R u+rwX "$SRC_DIR" 2>/dev/null || true
    log "源码树已解到 $SRC_DIR"
  fi
else
  warn "包里没有 src/ 源码树, 服务器端构建 Linux 客户端不可用"
fi
if [[ -f "$SRC_DIR/src/server_preset.rs" ]]; then
  log "源码树是远界版本 (含品牌与服务器预置)"
else
  warn "源码树缺少 src/server_preset.rs, 编出来的客户端不带品牌、也不锁服务器"
fi

# ---------------------------------------------------------------- 构建工具链
echo "==> 检查构建工具链"
CARGO_BIN=""
for d in /root/.cargo/bin "$HOME/.cargo/bin" /usr/local/cargo/bin; do
  [[ -x "$d/cargo" ]] && CARGO_BIN="$d" && break
done
FLUTTER_BIN=""
for d in /opt/flutter/bin /usr/local/flutter/bin "$HOME/flutter/bin"; do
  [[ -x "$d/flutter" ]] && FLUTTER_BIN="$d" && break
done
if [[ -z "$CARGO_BIN" || -z "$FLUTTER_BIN" ]]; then
  warn "构建工具链不全 (cargo: ${CARGO_BIN:-缺}, flutter: ${FLUTTER_BIN:-缺})"
  warn "服务器端一键出 Linux 客户端需要 Rust 工具链和 Flutter SDK"
else
  log "cargo   = $CARGO_BIN"
  log "flutter = $FLUTTER_BIN"
fi

# 面板是 systemd 起的, 构建任务继承面板进程的环境变量。
# 少了 PATH, 面板派出去的构建只会报一句「找不到 cargo」。
cat > "$PANEL_CONF_DIR/build.env" <<EOF
# 由 install.sh 生成: 构建工具链的 PATH 与 pkg-config 路径。
# 面板派发的构建任务继承这里, 缺了这两行就会报「找不到 cargo」「找不到 gstreamer」。
PATH=${CARGO_BIN}:${FLUTTER_BIN}:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
PKG_CONFIG_PATH=/usr/local/lib/pkgconfig:/usr/local/lib64/pkgconfig
RUSTDESK_BUILD_SRC=$SRC_DIR
RUSTDESK_BUILDER_ROOT=/var/lib/rustdesk-builder
CARGO_HOME=/root/.cargo
RUSTUP_HOME=/root/.rustup
EOF
chmod 644 "$PANEL_CONF_DIR/build.env"
log "构建环境已写入 $PANEL_CONF_DIR/build.env"

# Flutter 的 Linux 桌面支持默认是关的, 不开就编不出 Linux 客户端。
if [[ -n "$FLUTTER_BIN" ]]; then
  export PATH="$FLUTTER_BIN:$CARGO_BIN:$PATH"
  flutter config --enable-linux-desktop >/dev/null 2>&1 || true
  log "已开启 flutter 的 linux 桌面支持"
fi

# 缺这些开发库, cargo 会停在「找不到 gstreamer-1.0.pc」这类地方。
MISSING_PKGS=""
export PKG_CONFIG_PATH=/usr/local/lib/pkgconfig:/usr/local/lib64/pkgconfig
for p in gtk+-3.0 dbus-1 gstreamer-1.0 gstreamer-base-1.0 libXdamage libXrandr \
         libXfixes libXcursor alsa uuid openssl libyuv; do
  pkg-config --exists "$p" 2>/dev/null || MISSING_PKGS="$MISSING_PKGS $p"
done
if [[ -n "$MISSING_PKGS" ]]; then
  warn "缺这些开发库, 服务器端构建会失败:$MISSING_PKGS"
  warn "对应的是 gtk3-devel / gstreamer1-devel 这类 -devel 包, 离线机请提前装好"
else
  log "构建依赖齐全"
fi

# ---------------------------------------------------------------- 防火墙
if [[ $DO_FIREWALL -eq 1 ]] && command -v firewall-cmd >/dev/null; then
  echo "==> 放行防火墙端口"
  firewall-cmd --permanent --add-port=21115-21119/tcp >/dev/null 2>&1 || true
  firewall-cmd --permanent --add-port=21116/udp >/dev/null 2>&1 || true
  # 面板只放行本机和本网段
  firewall-cmd --permanent --remove-port=$PANEL_PORT/tcp >/dev/null 2>&1 || true
  firewall-cmd --permanent --add-rich-rule="rule family=\"ipv4\" source address=\"127.0.0.1\" port port=\"$PANEL_PORT\" protocol=\"tcp\" accept" >/dev/null 2>&1 || true
  SUBNET=$(ip -4 route get 1.1.1.1 2>/dev/null | awk '{print $7; exit}' | cut -d. -f1-3)
  if [[ -n "$SUBNET" ]]; then
    firewall-cmd --permanent --add-rich-rule="rule family=\"ipv4\" source address=\"$SUBNET.0/24\" port port=\"$PANEL_PORT\" protocol=\"tcp\" accept" >/dev/null 2>&1 || true
  fi
  firewall-cmd --reload >/dev/null 2>&1 || true
  log "21115-21119/tcp, 21116/udp, $PANEL_PORT/tcp(仅内网)"
else
  warn "跳过防火墙配置"
fi

# ---------------------------------------------------------------- 启动
echo "==> 启动服务"
systemctl daemon-reload
# 必须显式 restart: enable --now 对已在运行的单元是 no-op,
# 升级安装时新代码不会生效 (旧面板进程还活着, 新路由会 404)。
systemctl enable rustdesk-hbbs rustdesk-hbbr rustdesk-panel
systemctl restart rustdesk-hbbs rustdesk-hbbr rustdesk-panel
sleep 5

echo
echo "==> 自检"
FAIL=0
for u in rustdesk-hbbs rustdesk-hbbr rustdesk-panel; do
  if systemctl is-active --quiet "$u"; then
    log "$u active"
  else
    warn "$u 未运行，请查: journalctl -u $u -n 50"
    FAIL=1
  fi
done

echo "--- 监听端口 ---"
PORTS=$(ss -lntu 2>/dev/null | grep -oE ':2111[5-9]' | sort -u | tr '\n' ' ')
if [[ -n "$PORTS" ]]; then log "已监听: $PORTS"; else warn "没抓到 21115-21119 的监听"; FAIL=1; fi

echo "--- 面板 HTTP ---"
CODE=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$PANEL_PORT/login" 2>/dev/null || echo 000)
if [[ "$CODE" == "200" ]]; then log "面板响应正常 (登录页 200)"; else warn "面板返回 $CODE"; FAIL=1; fi

echo "--- 服务器公钥 ---"
PUBKEY=$(cat "$SERVER_DIR/id_ed25519.pub" 2>/dev/null || echo "")
if [[ -n "$PUBKEY" ]]; then log "公钥: $PUBKEY"; else warn "公钥还没生成，看 journalctl -u rustdesk-hbbs"; fi

# ---------------------------------------------------------------- 客户端配置串
# 跟客户端 ServerConfig.encode() 同格式: base64url(json) 之后整体反转。
# 这样装完服务端, 直接把这一串贴进客户端就能连上, 不用手填四个字段。
CLIENT_CFG=""
if [[ -n "$PUBKEY" ]]; then
  CLIENT_CFG=$(python3 - "$SERVER_HOST" "$PUBKEY" <<'PYEOF'
import base64, json, sys
host, key = sys.argv[1], sys.argv[2]
cfg = {"host": "%s:21116" % host, "relay": "%s:21117" % host,
       "api": "%s:21118" % host, "key": key}
raw = json.dumps(cfg, separators=(",", ":"))
sys.stdout.write(base64.urlsafe_b64encode(raw.encode()).decode()[::-1])
PYEOF
)
fi

AGENT_TOKEN=$(grep '^agent_token=' "$PANEL_CONF_DIR/panel.conf" 2>/dev/null | cut -d= -f2)

if [[ -n "$CLIENT_CFG" ]]; then
  OUT="$PANEL_CONF_DIR/客户端配置.txt"
  {
    echo "# RustDesk 客户端配置 (贴到客户端的「ID 服务器」栏, 或面板「客户端集成」页)"
    echo "ID Server : $SERVER_HOST:21116"
    echo "Relay     : $SERVER_HOST:21117"
    echo "API       : $SERVER_HOST:21118"
    echo "Key       : $PUBKEY"
    echo
    echo "配置串    : $CLIENT_CFG"
  } > "$OUT"
  chmod 600 "$OUT"
  log "客户端配置串已写到 $OUT"
fi

echo
echo "=========================================================="
echo " 部署完成"
echo "=========================================================="
echo "  信令服务器 (ID Server) : $SERVER_HOST:21116"
echo "  中继服务器 (Relay)     : $SERVER_HOST:21117"
echo "  管理面板               : http://$SERVER_HOST:$PANEL_PORT/"
echo "  面板账号               : $PANEL_USER"
if [[ "${GEN_PASS:-0}" == "1" ]]; then
  echo "  面板密码               : $PANEL_PASSWORD"
  echo "  (已存到 $PANEL_CONF_DIR/panel.conf，权限 600)"
else
  echo "  面板密码               : 见 $PANEL_CONF_DIR/panel.conf"
fi
echo "  客户端 Key             : $PUBKEY"
echo "  构建 agent token       : $AGENT_TOKEN"
echo
if [[ -n "$CLIENT_CFG" ]]; then
  echo "  客户端配置串 (贴到客户端即可连上):"
  echo "  $CLIENT_CFG"
  echo
fi
echo " 下一步:"
echo "  1) 打开面板 http://$SERVER_HOST:$PANEL_PORT/ 上传 logo / 改应用名"
echo "  2) 在「客户端集成」页下载品牌包, 或直接用上面的配置串"
echo "  3) 要在 Windows 上自己编客户端, 在那台机器上执行:"
echo "     python agent\\agent.py --panel http://$SERVER_HOST:$PANEL_PORT --token $AGENT_TOKEN"
echo "=========================================================="
exit $FAIL
