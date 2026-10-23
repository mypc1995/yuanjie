# RustDesk 自建服务器 —— 离线部署包

内网 / 离线环境用的 RustDesk 自建服务器 + 管理面板。
**安装过程完全不联网**，所有依赖都在包里。

```
rustdesk-server-suite/
├── install.sh              一键部署（主入口）
├── server/
│   ├── amd64/              x86_64 二进制 (hbbs / hbbr / rustdesk-utils)
│   ├── arm64/              aarch64 二进制（飞腾 / 鲲鹏 / 龙芯）
│   └── systemd/            服务单元模板
├── panel/                  Web 管理面板
│   ├── panel.py
│   ├── templates/  static/
│   ├── vendor/             Flask 等纯 Python 依赖（已 vendor，零安装）
│   └── rustdesk-panel.service
└── client/
    └── package.ps1         Windows 客户端品牌化打包脚本
```

## 一、部署服务端

```bash
tar -xzf rustdesk-server-suite.tar.gz
cd rustdesk-server-suite
sudo bash install.sh --server-host <服务器IP>
```

参数：

| 参数 | 说明 | 默认 |
|---|---|---|
| `--server-host <ip>` | 对外地址（写进 hbbs 的 `-r`，客户端拿它当中继地址） | 自动探测本机 IP |
| `--panel-user <name>` | 面板账号 | `admin` |
| `--panel-password <pw>` | 面板密码 | 随机生成并打印 |
| `--panel-port <port>` | 面板端口 | `21120` |
| `--no-firewall` | 不动 firewalld | — |
| `--uninstall` | 卸载（保留配置和密钥） | — |

装完：

| 服务 | 端口 | 说明 |
|---|---|---|
| hbbs | 21116/tcp+udp | ID 注册 / NAT 打洞 |
| hbbs | 21115/tcp | NAT 类型探测 + loopback console |
| hbbs | 21118/tcp | WebSocket 信令 |
| hbbr | 21117/tcp | 中继 |
| hbbr | 21119/tcp | WebSocket 中继 |
| 面板 | 21120/tcp | Web 管理界面（**只放行内网网段和 127.0.0.1**） |

密钥在 `/var/lib/rustdesk-server/id_ed25519.pub`，就是客户端要填的 **Key**。
客户端的 Key 留空会回落到官方 RustDesk 公钥，自建服务器会直接拒绝连接。

## 二、管理面板

打开 `http://<服务器>:21120/`，用安装时打印的账号密码登录。

| 页面 | 能干什么 |
|---|---|
| 仪表盘 | 服务状态、端口、运行时值（走 loopback console 实测）、启停/重启/自启 |
| 基础配置 | 改 `.env`：端口、绑定地址、relay 地址、强制中继、UDP 缓冲、带宽 QoS |
| **品牌定制** | **上传 logo / 图标、改应用名和主题色 —— 面板自己立刻生效** |
| **客户端集成** | 填服务器地址和 Key → 生成配置串、**导出品牌包**（给打包脚本） |
| 密钥 | 查看公钥、轮换密钥对 |
| 黑白名单 | 限速黑名单 / 拒绝黑名单 |
| 日志 | hbbs / hbbr 日志尾部 |

### 品牌定制的两条生效路径

**A. 运行时拉取（改完即生效，不用重编客户端）**

客户端启动时访问（无需登录，只读）：

```
GET /api/branding          -> {"app_name": "...", "primary_color": "#...", ...}
GET /api/branding/logo.png -> logo 图片
GET /api/client-config     -> 客户端配置串
```

**B. 打包时烘焙（离线环境没有服务器时用这个）**

在「客户端集成」页下载品牌包（zip），解压成 `branding/`，然后：

```powershell
pwsh -File client/package.ps1 -BrandingDir branding
```

脚本会把 logo/icon 替换进 `flutter/assets/`、`flutter/windows/runner/resources/app_icon.ico`、
`res/icon.ico`，并把服务器配置串预置进 `flutter/lib/common/server_preset.dart`，
然后跑 `cargo build --locked --features flutter --lib --release` + `flutter build windows --release`。

### 客户端配置串的格式

`base64url(json)` 之后**把整个字符串反转** —— 与客户端 `ServerConfig.encode()` 完全一致
（`flutter/lib/common.dart`）。面板生成的就是这个格式，客户端可直接导入。

字段：`host`（ID Server，可带端口）、`relay`、`api`、`key`。

## 三、离线注意事项

* 面板的 Python 依赖已经 vendor 在 `panel/vendor/`，**不碰系统 site-packages**，
  也不需要联网 pip install。面板只要求机器上有 `python3`（麒麟自带 3.7.9）。
* 面板当前用 Flask 自带的 WSGI 服务器，适合内网管理面板。
  要上 HTTPS 的话，前面挂个 nginx 反代即可。
* 客户端打包脚本里的 `cargo build` / `flutter build` 需要预先准备好的
  Rust 工具链、vcpkg 依赖和 Flutter SDK —— 这些在**构建机**上准备，不在部署机上。

## 四、常用运维命令

```bash
systemctl status rustdesk-hbbs rustdesk-hbbr rustdesk-panel
journalctl -u rustdesk-hbbs -f
tail -f /var/log/rustdesk-server/hbbs.log

# hbbs 的 loopback console (21115)
printf 'aur' | nc 127.0.0.1 21115     # 查/切强制中继
printf 'rs'  | nc 127.0.0.1 21115     # 查中继地址

# hbbr 的 loopback console (21117)
printf 'tb'  | nc 127.0.0.1 21117     # 查总带宽
printf 'ba 1.2.3.4' | nc 127.0.0.1 21117   # 加限速黑名单
printf 'Ba 1.2.3.4' | nc 127.0.0.1 21117   # 加拒绝黑名单
```

配置和密钥都在 `/var/lib/rustdesk-server/`，备份这个目录即可。
