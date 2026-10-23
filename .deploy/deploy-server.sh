set -e
echo "=== 1. 安装二进制 ==="
install -m 755 /tmp/hbbs /usr/bin/hbbs
install -m 755 /tmp/hbbr /usr/bin/hbbr
install -m 755 /tmp/rustdesk-utils /usr/bin/rustdesk-utils
echo "  OK"

echo "=== 2. 建用户/组/目录 ==="
getent group rustdesk >/dev/null || groupadd -r rustdesk
id rustdesk >/dev/null 2>&1 || useradd -r -g rustdesk -d /var/lib/rustdesk-server -s /sbin/nologin rustdesk
mkdir -p /var/lib/rustdesk-server /var/log/rustdesk-server
chown -R rustdesk:rustdesk /var/lib/rustdesk-server
chmod 750 /var/lib/rustdesk-server
chmod 755 /var/log/rustdesk-server
echo "  OK (uid=$(id -u rustdesk))"

echo "=== 3. 安装 systemd 单元 ==="
install -m 644 /tmp/rustdesk-hbbs.service /etc/systemd/system/rustdesk-hbbs.service
install -m 644 /tmp/rustdesk-hbbr.service /etc/systemd/system/rustdesk-hbbr.service
systemctl daemon-reload
echo "  OK"

echo "=== 4. 防火墙放行 ==="
firewall-cmd --permanent --add-port=21115-21119/tcp >/dev/null 2>&1 || true
firewall-cmd --permanent --add-port=21116/udp >/dev/null 2>&1 || true
firewall-cmd --reload >/dev/null 2>&1 || true
firewall-cmd --list-ports 2>/dev/null || true

echo "=== 5. 启动服务 ==="
systemctl enable --now rustdesk-hbbs rustdesk-hbbr
sleep 5
systemctl is-active rustdesk-hbbs rustdesk-hbbr
echo "--- 监听端口 ---"
ss -lntup 2>/dev/null | grep -E ":2111[5-9]" || echo "无监听！"
echo "--- 公钥 ---"
cat /var/lib/rustdesk-server/id_ed25519.pub 2>/dev/null || echo "(尚未生成)"
