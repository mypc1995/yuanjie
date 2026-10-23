set -e

PANEL_DIR=/opt/rustdesk-panel
CONF_DIR=/etc/rustdesk-panel

echo "=== 1. 铺文件 ==="
mkdir -p $PANEL_DIR/templates $PANEL_DIR/static $CONF_DIR
cp /tmp/panel/panel.py $PANEL_DIR/panel.py
cp /tmp/panel/templates/*.html $PANEL_DIR/templates/
cp /tmp/panel/static/style.css $PANEL_DIR/static/
chmod 755 $PANEL_DIR/panel.py
echo "  OK"

echo "=== 2. 面板配置（账号密码）==="
if [ ! -f $CONF_DIR/panel.conf ]; then
  cat > $CONF_DIR/panel.conf <<EOF
user=__PANEL_USER__
password=__PANEL_PASSWORD__
EOF
  chmod 600 $CONF_DIR/panel.conf
  echo "  已创建（首次）"
else
  echo "  已存在，保持不变"
fi

echo "=== 3. systemd 单元 ==="
cp /tmp/panel/rustdesk-panel.service /etc/systemd/system/rustdesk-panel.service
sed -i 's/__PANEL_SECRET__/__PANEL_SECRET_VALUE__/' /etc/systemd/system/rustdesk-panel.service
systemctl daemon-reload
echo "  OK"

echo "=== 4. 防火墙：只放行内网 21120 ==="
firewall-cmd --permanent --remove-port=21120/tcp >/dev/null 2>&1 || true
firewall-cmd --permanent --add-rich-rule='rule family="ipv4" source address="192.168.100.0/24" port port="21120" protocol="tcp" accept' >/dev/null 2>&1 || true
firewall-cmd --permanent --add-rich-rule='rule family="ipv4" source address="127.0.0.1" port port="21120" protocol="tcp" accept' >/dev/null 2>&1 || true
firewall-cmd --reload >/dev/null 2>&1 || true
echo "  OK"

echo "=== 5. 启动面板 ==="
systemctl enable --now rustdesk-panel
sleep 4
systemctl is-active rustdesk-panel
echo "--- 面板日志 ---"
tail -15 /var/log/rustdesk-server/panel.log 2>/dev/null || true
tail -15 /var/log/rustdesk-server/panel.error 2>/dev/null || true
echo "--- 本机自测 ---"
curl -sS -o /dev/null -w "  未认证应返回 401，实际: %{http_code}\n" http://127.0.0.1:21120/ || true
curl -sS -u "__PANEL_USER__:__PANEL_PASSWORD__" -o /dev/null -w "  带认证应返回 200，实际: %{http_code}\n" http://127.0.0.1:21120/ || true
