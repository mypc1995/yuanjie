# 远界 私有化部署包

一套自包含的私有化部署包。**全程离线**，不需要连外网、不需要账号、不需要 Docker。

包含三样东西，按下面的顺序做一遍就完事：

| 目录 | 是什么 | 装在哪 |
|---|---|---|
| `01-server-kylin/` | 自建服务器 + 管理面板 | 麒麟 / CentOS / Debian 等 systemd 服务器 |
| `02-build-agent-windows/` | Windows 编译机离线环境包 | 纯内网的 Windows 构建机 |
| `03-client-windows/` | 已经编好的客户端 | 直接发给最终用户 |

---

## 第 1 步：装服务器（麒麟）

把 `01-server-kylin` 整个目录传到服务器，然后：

```bash
cd 01-server-kylin
sudo bash install.sh
```

就这一条命令。脚本会自己做完这些事：

- 装信令服务器和中继服务器（自动判断 x86_64 / ARM64）
- 装管理面板（自带界面，账号密码装的时候定）
- 注册成开机自启的服务
- 放行防火墙端口
- 自检：三个服务在跑吗、端口听了吗、面板通不通、公钥生成了没

装完屏幕上会直接打印这几样：

```
信令服务器 (ID Server) : <服务器IP>:21116
中继服务器 (Relay)     : <服务器IP>:21117
管理面板               : http://<服务器IP>:21120/
面板账号               : admin
面板密码               : ********
客户端 Key             : ********
客户端配置串 (贴到客户端即可连上): **********************
```

**把最后那串「客户端配置串」记下来**，客户端要用。

也可以指定参数：

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

---

## 第 2 步：挂 Windows 编译机（可选）

只有想在**自己的 Windows 机器上重新编客户端**（比如改了 logo、想换应用名）才需要这一步。

把 `02-build-agent-windows` 整个目录拷到内网 Windows 机器，改一下
`deploy-config.txt`（面板地址和 token 已经在里面了，改成你自己的），然后：

```bat
setup.cmd
```

**双击就行。** 这一条命令会：

1. 装完整的离线编译环境（Rust / 编译依赖 / 预置库 / 打包好的依赖源码），**全程不联网**
2. 把这台机器注册成面板的构建节点，agent 立刻开始干活
3. 顺带注册一个计划任务，重启机器后 agent 自动起来

装完在面板「客户端构建」页点「开始构建」，这台机器就会自动领任务、编译、
把产物回传面板，你在面板上下载就行。**Windows 客户端只能这么编**，没有交叉编译。

> 这台机器还需要有 **MSVC 工具集 + Windows SDK**（装 Visual Studio 时选「使用 C++ 的桌面开发」，
> 或者只装工具集组件也行）。包里的补丁已经放宽了 Flutter 对 VS 工作负载的硬性要求。

想手动来也行：

```powershell
powershell -ExecutionPolicy Bypass -File rustdesk-offline-bundle\install-offline.ps1
python rustdesk-offline-bundle\agent\agent.py --panel http://<服务器IP>:21120 --token <agent_token>
```

> **agent 要常驻**，别关窗口。`setup.cmd` 已经帮你注册了计划任务，不用管这事。

---

## 第 3 步：发客户端

`03-client-windows/远界-client-windows-x64.zip` 解压就能用，不需要安装。

**这个包已经内置了第 1 步装好的服务器地址和 Key**，用户解压双击就能连上，
不需要自己填任何服务器信息。

如果想换 logo 或应用名，在面板「品牌定制」页上传，然后重新走一遍第 2 步的构建，
新包会带上新的品牌。

---

## 目录结构

```
rustdesk-deployment/
├── README.md                    ← 本文件
├── SHA256SUMS.txt               ← 全包校验和 (传到内网后可以先对一下)
├── 01-server-kylin/
│   ├── install.sh               ← 一键部署 (sudo bash install.sh)
│   ├── server/                  ← x86_64 / ARM64 两套服务端程序 + 服务配置
│   ├── panel/                   ← 管理面板
│   └── builder/                 ← 面板的构建调度脚本
├── 02-build-agent-windows/
│   ├── setup.cmd                 ← 一键安装 (双击就行)
│   ├── setup.ps1                 ← setup.cmd 干的活 (装环境 + 挂面板 + 开机自启)
│   ├── deploy-config.txt         ← 面板地址 / token / 源码目录
│   └── rustdesk-offline-bundle/  ← 离线包本体
│       ├── install-offline.ps1   ← 只想装环境的话, 单独跑它
│       ├── README.md             ← 详细说明
│       └── manifest.json         ← 每个文件的校验和
└── 03-client-windows/
    ├── 远界-client-windows-x64.zip  ← 编好的客户端, 解压即用
    ├── brand-bundle.zip                 ← 品牌包 (logo / 应用名 / 配置串)
    └── branding/                        ← 品牌包解开的样子
```

---

## 部署完之后

| 要做的事 | 在哪 |
|---|---|
| 改 logo、应用名、主题色 | 面板「品牌定制」页 |
| 看服务器地址、生成配置串 | 面板「客户端集成」页 |
| 下发编译任务、下载客户端 | 面板「客户端构建」页 |
| 加设备黑白名单 | 面板「黑白名单」页 |
| 看服务端日志 | 面板「日志」页 |

面板地址 `http://<服务器IP>:21120/`，账号密码装的时候定的。

---

## 注意事项

- **面板走的是 HTTP**，内网环境够用。真要放到公网，前面挂个 Nginx 加 HTTPS，
  然后改面板的 `RUSTDESK_PANEL_SECRET` 和防火墙规则。
- **服务器端口**：21115-21119（TCP）+ 21116（UDP）是客户端要连的，别关。
  面板 21120 脚本只放行了本机和本网段。
- **备份**：服务器的密钥在 `/var/lib/rustdesk-server/id_ed25519`，客户端配置串是拿它
  签出来的。**这个文件丢了，客户端就都连不上了**，换机器重装时要一起带走。
  面板配置在 `/etc/rustdesk-panel/panel.conf`。
- **离线包 5.5 GB**，拷贝到内网机器建议用移动硬盘或内网共享，别走公网中转。
