# HZDesk

**基于 RustDesk 开发的私有化远程控制方案** —— 服务端、客户端、编译环境全套自给自足，数据不出内网。

---

## 为什么需要它

用现成的公共远控工具，单位里有两条过不去的坎：

- **数据必须留在自己内网。** 设备列表、连接记录、文件传输全都不该经过别人的服务器。
- **客户端不该让用户去填服务器地址。** 发到同事手上还要教一遍「ID 服务器填什么、Key 从哪拷」，用起来就劝退了。

HZDesk 把这两件事做掉：服务器装在自己机房里，客户端**解压双击就能连**，logo 和名字也换成自己的。

---

## 和原生 RustDesk 有什么不一样

| | 原生 RustDesk | HZDesk |
|---|---|---|
| 服务器 | 官方公共服务器 | **自建**，信令 / 中继 / 面板都装在你自己机器上 |
| 首次使用 | 要手动填服务器地址和 Key | **已内置**，装完即用 |
| 品牌 | 官方 logo、应用名 | **自己的** logo、应用名、图标、主题色 |
| 部署 | 自己研究 | **一条命令**装完服务端（含管理面板） |
| 改客户端 | 自己搭编译环境，踩坑多 | **一键装好离线编译环境**，面板上一键出包 |
| 日常管理 | 没有 | **Web 面板**：品牌、设备黑白名单、密钥、构建任务、日志 |
| 编译依赖 | 要连网拉一堆东西 | **离线包自带**，内网机上装得动 |

---

## 里面包含什么

| 组成 | 说明 | 装在哪 |
|---|---|---|
| **自建服务器** | 信令服务器、中继服务器、Web 管理面板 | 麒麟 / CentOS / Debian 等 Linux 服务器 |
| **定制客户端** | Windows 版，已内置服务器地址和 Key | 直接发给最终用户 |
| **构建机方案** | 想改客户端？给一台内网 Windows 机器，一键装好编译环境 | 内网 Windows 构建机 |

---

## 快速开始

### 第 1 步 · 装服务器

把部署包里的 `01-server-kylin` 传到服务器，然后：

```bash
cd 01-server-kylin
sudo bash install.sh
```

一条命令搞定：装信令/中继服务器、装管理面板、注册开机自启、放行防火墙端口，最后自检并打印
**面板地址、账号密码、客户端 Key 和客户端配置串**。

想指定参数：

```bash
sudo bash install.sh --server-host <服务器IP> --panel-password '你的密码'
```

| 参数 | 说明 |
|---|---|
| `--server-host <ip>` | 对外地址，不填自动探测本机 IP |
| `--panel-user <name>` | 面板账号，默认 `admin` |
| `--panel-password <pw>` | 面板密码，不填随机生成 |
| `--panel-port <port>` | 面板端口，默认 `21120` |
| `--no-firewall` | 不动防火墙 |
| `--uninstall` | 卸载（配置和密钥保留） |

### 第 2 步 · 发客户端

`03-client-windows/rustdesk-client-windows-x64.zip` 解压就能用，**不需要安装，不需要配置**。
服务器地址和 Key 已经烘焙进去了。

### 第 3 步 ·（可选）改自己的品牌

在管理面板「品牌定制」页上传 logo、填应用名、选主题色，然后重新出一遍客户端，新包就带上了。

想自己改代码，就按第 4 步准备构建机。

### 第 4 步 ·（可选）建一台自己的构建机

只有要**重新编客户端**才需要这一步。把 `02-build-agent-windows` 拷到内网 Windows 机器，
改一下 `deploy-config.txt`（面板地址和 token），然后：

```bat
setup.cmd
```

双击就行。装好完整编译环境（全程不联网）+ 注册成面板的构建节点 + 顺带挂上开机自启。

之后在面板「客户端构建」页点「开始构建」，这台机器自动领任务、编译、把产物回传面板，
你在面板上下载就行。

> 这台机器要有 **MSVC 工具集 + Windows SDK**（装 Visual Studio 时选「使用 C++ 的桌面开发」
> 或只装工具集组件都行）。

---

## 管理面板能干什么

打开 `http://<服务器IP>:21120/`，用安装时打印的账号密码登录。

| 页面 | 干什么 |
|---|---|
| 仪表盘 | 服务状态、设备数、服务器公钥 |
| 基础配置 | 服务器地址、端口等 |
| **品牌定制** | 换 logo、应用名、图标、主题色，面板自身立刻生效 |
| **客户端集成** | 服务器配置串、品牌包导出（给构建流程用） |
| 密钥 | 管理设备授权密钥 |
| 黑白名单 | 按 ID 允许/禁止连接 |
| 日志 | 服务端日志 |
| **客户端构建** | 派发编译任务、看实时日志、下载产物 |

---

## 目录结构

```
HZDesk/
├── src/                 客户端主程序
├── libs/                核心库（与服务端共享的部分 + 客户端专属部分）
├── flutter/             界面
├── res/                 图标、依赖端口定义
├── .deploy/             私有化部署这一摊
│   ├── panel/           Web 管理面板
│   ├── builder/         构建调度：构建机客户端 + 编译执行
│   ├── package/         服务端一键部署包（install.sh + 二进制 + 面板）
│   ├── install-offline.ps1        Windows 构建机离线安装
│   ├── prepare-offline-bundle.ps1 组装离线包
│   └── setup.cmd / setup.ps1      构建机一键入口
├── Cargo.toml  Cargo.lock  build.rs  vcpkg.json
├── AGENTS.md            开发约定
└── LICENCE              许可证
```

已经打好的成品在 `D:\Software\rustdesk-deployment\`：服务端部署包 + 构建机离线包 + 编好的客户端。

---

## 开发

### 技术栈

- **核心**：Rust（客户端主程序、连接协议、编解码、采集、输入模拟）
- **界面**：Flutter / Dart
- **服务端**：官方 rustdesk-server 预编译二进制 + systemd
- **管理面板**：Python + Flask（依赖已 vendor 进去，离线可装）
- **构建调度**：Python（构建机 agent 与面板之间只走标准库 HTTP）

### 在开发机上编客户端（Windows）

```powershell
$env:VCPKG_ROOT='D:\Software\RustDesk'
$env:VCPKG_INSTALLED_ROOT='D:\Software\RustDesk\vcpkg_installed'
$env:LIBCLANG_PATH='D:\Software\llvm\bin'
$env:FLUTTER_CMAKE_PATH='D:\Software\RustDesk\.dl\cmake-4.4.4-windows-x86_64\bin\cmake.exe'
$env:PATH="C:\vcpkg;D:\Software\nasm\nasm-3.02;D:\Software\llvm\bin;D:\Software\flutter\bin;C:\Users\Agent\.cargo\bin;$env:PATH"

cd D:\HZDesk
cargo build --locked --features flutter --lib --release
cd flutter; flutter build windows --release
```

产物在 `flutter\build\windows\x64\runner\Release\`。

### 完全离线地编

用 `D:\Software\rustdesk-deployment\02-build-agent-windows\rustdesk-offline-bundle\`
里的离线包 —— 依赖源码和预置库都在里面，`install-offline.ps1` 装好环境就能编。

### 改了源码之后

| 想改什么 | 改哪儿 |
|---|---|
| 预置服务器地址（装完即连自建服务器） | `src/server_preset.rs` + `src/server_preset.txt` |
| 预置界面上的应用名 | `flutter/lib/common/server_preset.dart` |
| 品牌图片 | 走管理面板「品牌定制」页，或直接换 `flutter/assets/` 和 `res/` |
| 依赖端口（离线编译相关） | `res/vcpkg/` |

---

## 注意事项

- **面板走的是 HTTP。** 内网够用；真要放到公网，前面挂个 Nginx 加 HTTPS。
- **服务器端口**：客户端要连 21115-21119，别关。面板端口安装脚本只放行本机和本网段。
- **一定要备份服务器密钥**：`/var/lib/rustdesk-server/id_ed25519`。
  客户端配置串是拿它签出来的，**这个文件丢了所有客户端都会连不上**。
  面板配置在 `/etc/rustdesk-panel/panel.conf`。
- **编译依赖要钉死版本**：`cargo build` 必须带 `--locked`；重新生成 bridge 代码要用
  `flutter_rust_bridge_codegen` 1.80.1（`--llvm-path` 指向 libclang）。

---

## 关于本项目

HZDesk **基于 RustDesk 开发**，遵循 **AGPL-3.0** 协议（见 [LICENCE](LICENCE)）。
上游项目：<https://github.com/rustdesk/rustdesk>

本仓库相对上游的主要改动：

1. **`libs/hbb_common` 已并入主仓库**（不再是 git 子模块）—— 一次 push 就是完整源码，
   clone 下来不用再拉子模块。代价：以后同步上游这块要手动 diff。
2. **bridge 生成代码一并提交**（`src/bridge_generated.rs`、`flutter/lib/generated_bridge.dart` 等）——
   上游靠 CI 重新生成，本仓库不跑 CI，提交进来才能保证 clone 下来直接能编。
3. **新增编译期预置服务器配置**（`src/server_preset.rs`）与**界面应用名预置**
   （`flutter/lib/common/server_preset.dart`）—— 客户端编译时就把服务器地址和 Key 烘焙进去。
4. **新增私有化部署体系**（`.deploy/`）：管理面板、构建调度、离线包、服务端一键部署脚本。
5. **libyuv 依赖端口改为优先使用本机 clang，没有则用 MSVC 编译**（`res/vcpkg/libyuv/portfile.cmake`）
   —— 离线构建机不需要额外携带 LLVM。
