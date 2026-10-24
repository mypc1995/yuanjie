#!/usr/bin/env bash
# 远界 自建服务器 —— 一键离线部署
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
#   --uninstall           卸载(保留配置和密钥)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

SERVER_HOST=""
PANEL_USER="admin"
PANEL_PASSWORD=""
PANEL_PORT="21120"
DO_FIREWALL=1
UNINSTALL=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --server-host) SERVER_HOST="$2"; shift 2 ;;
    --panel-user) PANEL_USER="$2"; shift 2 ;;
    --panel-password) PANEL_PASSWORD="$2"; shift 2 ;;
    --panel-port) PANEL_PORT="$2"; shift 2 ;;
    --no-firewall) DO_FIREWALL=0; shift ;;
    --uninstall) UNINSTALL=1; shift ;;
    -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
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
  echo "==> 卸载 远界 服务"
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
if [[ -d /opt/rustdesk-src ]]; then
  log "检测到源码树 /opt/rustdesk-src, 面板可直接构建 Linux 客户端"
else
  warn "没有 /opt/rustdesk-src, 面板只能派发任务给 Windows agent (Linux 目标会失败)"
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
echo "  构建 agent token       : $(grep '^agent_token=' "$PANEL_CONF_DIR/panel.conf" 2>/dev/null | cut -d= -f2)"
echo
echo " 下一步: 在面板「客户端集成」页生成配置串，或下载品牌包"
echo "=========================================================="
exit $FAIL
