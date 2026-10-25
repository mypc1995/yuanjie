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
├── builder/                构建任务执行器
│   ├── build_job.py        编译 + 打包（Linux 目标在服务器上直接跑）
│   └── agent.py            Windows 构建机的领单 agent
├── src/
│   └── rustdesk-src.tar.gz 客户端源码树（面板在服务器上出包时用）
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
| `--update-src` | 重新解压客户端源码树 | 已存在就跳过 |
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

## 三、在面板上一键生成客户端

面板的「构建」页可以出客户端包，选目标按下按钮就行。

| 目标 | 在哪编 | 需要什么 |
|---|---|---|
| **Linux x64** | **就在这台服务器上编** | 服务器上有 Rust 工具链 + Flutter SDK（见下面「装构建环境」） |
| Linux arm64 | 交叉编译 | 另配交叉工具链 |
| Windows x64 | Windows 构建机编 | 一台装了离线构建包的 Windows 机器，跑 `builder/agent.py` 领单 |

Linux 目标走的是服务器本地编译：面板把任务丢给 `builder/build_job.py`，
它会把你填的品牌资源和服务器配置烘焙进源码，再跑 `cargo build` + `flutter build linux`，
产物打成 `rustdesk-client-linux-x64.tar.gz`，在面板上点一下就能下载。

烘焙进客户端的内容：

* 显示名（界面上的名字，可以是中文）和安装名（服务名、安装目录、注册表键，**必须纯 ASCII**）
* 服务器地址、Key —— 装完就能连，不用手填
* logo 和图标

### 装构建环境

服务器要能编 Linux 客户端，需要下面这些。**装好之后重跑一次 `install.sh` 即可**，
它会把工具链路径写进 `/etc/rustdesk-panel/build.env`（面板是 systemd 起的，
构建任务继承这个文件里的 `PATH` 和 `PKG_CONFIG_PATH`）。

```bash
# Rust 工具链
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Flutter SDK（3.24.5，Linux 桌面版）
# 解压到 /opt/flutter 即可

# 编译要用的开发库（麒麟 / 统信按自己的包名装）
yum install -y gtk3-devel dbus-devel libXdamage-devel libXrandr-devel \
    libXfixes-devel libXcursor-devel libXi-devel libXtst-devel \
    libXinerama-devel alsa-lib-devel uuid-devel openssl-devel \
    gstreamer1-devel gstreamer1-plugins-base-devel

# 自编的 libyuv / libaom 装在 /usr/local，pkg-config 要能找到它们
export PKG_CONFIG_PATH=/usr/local/lib/pkgconfig:/usr/local/lib64/pkgconfig
```

> **自己编 libaom / libyuv 的话，一定要带 `-fPIC`**：
> 客户端的 Rust 核心编出来是 `librustdesk.so`（共享库），链静态库时目标文件
> 必须是 PIC。没加的话链接会报
> `relocation R_X86_64_32 against symbol ... can not be used when making a shared object`。
> cmake 加 `-DCMAKE_POSITION_INDEPENDENT_CODE=ON` 即可。

`install.sh` 会检查这些依赖，缺哪个会明确列出来。

## 四、离线注意事项

* 面板的 Python 依赖已经 vendor 在 `panel/vendor/`，**不碰系统 site-packages**，
  也不需要联网 pip install。面板只要求机器上有 `python3`（麒麟自带 3.7.9）。
* 面板当前用 Flask 自带的 WSGI 服务器，适合内网管理面板。
  要上 HTTPS 的话，前面挂个 nginx 反代即可。
* 服务端和面板的安装**不联网**。但服务器上编译 Linux 客户端这一步需要联网：
  cargo 要从 crates.io 拉依赖，Flutter 要从 pub.dev 拉包。
  纯离线环境要编客户端，请改用 Windows 构建机那条路（`02-build-agent-windows` 离线包）。

## 五、常用运维命令

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
