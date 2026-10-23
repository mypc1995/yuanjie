#!/bin/bash
# 部署带构建功能的面板 + 构建服务
set -euo pipefail

PANEL_DIR=/opt/rustdesk-panel
BUILDER_DIR=/opt/rustdesk-builder
UNIT=/etc/systemd/system/rustdesk-panel.service
BUILD_ROOT=/var/lib/rustdesk-builder

echo "=== 1. 建目录 ==="
mkdir -p "$PANEL_DIR/templates" "$PANEL_DIR/static" "$BUILDER_DIR" \
         "$BUILD_ROOT/jobs" "$BUILD_ROOT/artifacts" /var/log/rustdesk-builder
echo "  OK"

echo "=== 2. 补 agent_token (老配置可能没有) ==="
CONF=/etc/rustdesk-panel/panel.conf
if ! grep -q '^agent_token=' "$CONF" 2>/dev/null; then
  TOK=$(head -c 24 /dev/urandom | base64 | tr -dc 'A-Za-z0-9' | head -c 20)
  echo "agent_token=$TOK" >> "$CONF"
  chmod 600 "$CONF"
  echo "  已生成 agent_token"
else
  echo "  已存在"
fi

echo "=== 3. 装构建脚本 ==="
if [ -d /tmp/builder ]; then
  cp -r /tmp/builder/. "$BUILDER_DIR/"
  chmod 755 "$BUILDER_DIR"/*.py
  echo "  build_job.py / agent.py 已装"
fi

echo "=== 4. 更新 systemd 单元 ==="
if ! grep -q RUSTDESK_BUILDER_ROOT "$UNIT" 2>/dev/null; then
  sed -i "/^Environment=RUSTDESK_PANEL_SECRET=/a Environment=RUSTDESK_BUILDER_ROOT=$BUILD_ROOT\nEnvironment=RUSTDESK_BUILD_SCRIPT=$BUILDER_DIR/build_job.py" "$UNIT"
  echo "  已加上 builder 环境变量"
fi
systemctl daemon-reload
systemctl restart rustdesk-panel
sleep 3
systemctl is-active rustdesk-panel

echo "=== 5. 验证 ==="
for p in / /build /client /branding; do
  code=$(curl -s -o /dev/null -w "%{http_code}" "http://127.0.0.1:21120$p")
  printf "  %-12s %s\n" "$p" "$code"
done
echo "  agent_token: $(grep '^agent_token=' "$CONF" | cut -d= -f2)"
