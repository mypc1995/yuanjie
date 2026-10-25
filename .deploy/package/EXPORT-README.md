# 远界 · 部署包

一整套能直接装起来的远界：自建服务器、管理面板、客户端源码，以及在**断网内网**里
编 Windows 客户端的离线构建环境。

搬到一个新环境时，只有服务器地址、agent token、服务器 Key 这几样会变，
其余部分原样可用。

## 这个包里有什么

| 目录 | 是什么 | 体积 |
|---|---|---|
| `01-server-kylin/` | 服务端 + 管理面板 + 客户端源码树 | 30 MB |
| `02-build-agent-windows/` | Windows 构建机离线包，断网也能编客户端 | 5.6 GB |
| `03-client-windows/` | 已打好品牌的 Windows 客户端 | 31 MB |

`SHA256SUMS.txt` 是全包校验和，拷完先对一遍：

```bash
sha256sum -c SHA256SUMS.txt
```

---

## 一、装服务器（新环境每台都要做一次）

前提：麒麟 / 统信 V10，**x86_64**，root 权限。

```bash
# 1. 把 01-server-kylin 拷到服务器并解压
tar -xzf 01-server-kylin.tar.gz      # 或者直接拷目录
cd 01-server-kylin

# 2. 一键部署
sudo bash install.sh --server-host <这台服务器的IP>
```

装完脚本会打印：面板地址、面板账号密码、客户端 Key、客户端配置串。
**把这几个记下来**，后面填配置要用。

如果这台机器上要装构建环境（Rust 工具链、Flutter SDK、编译用的开发库），
`install.sh` 会检查并把缺的东西列出来。开发库在麒麟上大致是这些：

```bash
yum install -y gtk3-devel dbus-devel libXdamage-devel libXrandr-devel \
    libXfixes-devel libXcursor-devel libXi-devel libXtst-devel \
    libXinerama-devel alsa-lib-devel uuid-devel openssl-devel \
    gstreamer1-devel gstreamer1-plugins-base-devel
```

> 面板上的「构建」页选 **Linux x64**，会直接在这台服务器上把客户端编出来。
> 这一步需要联网（要下载依赖包）。纯离线环境请走下面的 Windows 构建机。

---

## 二、装 Windows 构建机（只有要在新环境编 Windows 客户端才需要）

在一台 Windows x64 机器上：

1. 拷 `02-build-agent-windows/` 过去
2. 打开 `deploy-config.txt`，填上面记下来的面板地址和 agent token：

   ```
   panel=http://<面板IP>:21120
   token=<agent_token>
   ```

   agent token 在服务器的 `/etc/rustdesk-panel/panel.conf` 里，叫 `agent_token`。

3. 双击 `setup.cmd`。它会装好整套离线编译环境、把这台机器登记成面板的构建节点。
4. 面板「构建」页选 **Windows x64** → 开始构建 → 这台机器自动领单 → 面板上下载产物。

---

## 三、换品牌 / 换服务器地址

新环境的地址、Key 跟源服务器不一样，所以原来那个客户端连不上新的。改法：

1. 面板「品牌定制」页：改应用名、安装名、logo
2. 面板「客户端集成」页：填新服务器的地址和 Key → 存
3. 面板「构建」页：选目标 → 开始构建 → 下载新包

**安装名要用纯字母数字**（比如 `YuanJie`）——它会被写进服务名、安装目录和提权脚本，
中文会装不上。应用名（界面里显示的）可以随便写中文。

---

## 四、已打好品牌的那个客户端

`03-client-windows/远界-client-windows-x64.zip` 里是**源服务器**环境编出来的客户端，
里面烧死了源服务器的地址和 Key。搬到新环境后它连不上新服务器，当参考件看就行，
新环境的客户端请按上一节重新编。

`03-client-windows/branding/` 里的服务器配置已经清空了，
格式跟面板「客户端集成」页导出的品牌包一致，可以对着填。

---

## 五、端口

| 端口 | 用途 |
|---|---|
| 21116 | 信令（ID Server） |
| 21117 | 中继（Relay） |
| 21118 | 信令 WebSocket |
| 21115 | NAT 探测 |
| 21119 | 中继 WebSocket |
| 21120 | 管理面板（**只放行内网网段和 127.0.0.1**） |

客户端要填的只有三个：ID Server、Relay、Key。`install.sh` 打完的
`/etc/rustdesk-panel/客户端配置.txt` 里是一整条配置串，贴进客户端就能连上。
