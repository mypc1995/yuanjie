# 服务端（麒麟 / Linux）

一键部署自建服务器 + 管理面板。**全程离线**，包里自带所有程序。

```bash
sudo bash install.sh
```

## 装完有什么

| 组件 | 端口 | 用途 |
|---|---|---|
| 信令服务器 | 21116 | 客户端上线、互相找对方 |
| 中继服务器 | 21117 | 直连不通时中转 |
| 控制台 | 21115 | 调试用，日常不用管 |
| 网页服务 | 21118 | 设备列表、密钥同步 |
| 管理面板 | 21120 | 浏览器里管理 |

另外 21115-21119 的 UDP 是打洞用的。

## 装完会在哪

| 东西 | 路径 |
|---|---|
| 程序 | `/usr/bin/{hbbs,hbbr,rustdesk-utils}` |
| 服务器数据 + 密钥 | `/var/lib/rustdesk-server/` |
| 管理面板 | `/opt/rustdesk-panel/` |
| 面板配置（账号密码、token） | `/etc/rustdesk-panel/panel.conf` |
| 客户端配置串 | `/etc/rustdesk-panel/客户端配置.txt` |
| 构建调度 | `/opt/rustdesk-builder/` |
| 日志 | `/var/log/rustdesk-server/` |

## 日常维护

```bash
systemctl status rustdesk-hbbs rustdesk-hbbr rustdesk-panel   # 看状态
systemctl restart rustdesk-hbbs                                # 重启单个服务
journalctl -u rustdesk-hbbs -f                                 # 看实时日志
journalctl -u rustdesk-panel -n 100                            # 看面板日志
```

## 备份什么

| 要备份 | 路径 | 为什么 |
|---|---|---|
| **服务器密钥** | `/var/lib/rustdesk-server/id_ed25519` | **最重要**。客户端配置串是拿它签出来的，丢了所有客户端都连不上 |
| 面板配置 | `/etc/rustdesk-panel/panel.conf` | 面板账号密码、agent token |
| 设备数据 | `/var/lib/rustdesk-server/db_v2.sqlite3` | 已连接设备、密钥等 |

换服务器重装时，把 `id_ed25519` 一起带走，客户端配置串才继续有效。

## 升级

把新的 `01-server-kylin` 目录传上去，再跑一遍 `sudo bash install.sh`。
脚本会覆盖程序和面板、保留配置和密钥、重启服务。

## 卸载

```bash
sudo bash install.sh --uninstall
```

服务、程序、面板都会删掉，**配置和密钥保留**。

## 参数

| 参数 | 说明 |
|---|---|
| `--server-host <ip>` | 对外地址，不填自动探测本机 IP |
| `--panel-user <name>` | 面板账号，默认 `admin` |
| `--panel-password <pw>` | 面板密码，不填随机生成 |
| `--panel-port <port>` | 面板端口，默认 `21120` |
| `--no-firewall` | 不动防火墙 |
| `--uninstall` | 卸载（配置和密钥保留） |

## 面板在 Linux 上也能编客户端吗

能，但**前提是服务器上有源码树**（`/opt/rustdesk-src`）。

- 有源码树 → 面板直接编 Linux 客户端
- 没有 → 只能派发任务给 Windows 构建机，编 Linux 目标会失败

Windows 客户端**只能**在 Windows 上编，没有交叉编译这条路。
