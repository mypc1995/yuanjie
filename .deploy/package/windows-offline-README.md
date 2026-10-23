# RustDesk 客户端构建机 —— 离线环境包

给**纯内网**的 Windows 构建机用的。包里带齐了编译 RustDesk 客户端需要的一切，
**安装过程不联网**。

```
rustdesk-offline-bundle/
├── install-offline.ps1          ← 一键安装（主入口）
├── README.md
├── manifest.json                每个文件的 SHA256
├── toolchain/
│   ├── rust/                    rust-1.99.1 官方 MSI（按用户安装，不需要管理员）
│   ├── flutter/                 Flutter 3.24.5 SDK
│   ├── cargo-vendor/            全部 Rust 依赖源码（cargo vendor 产物）
│   ├── cargo-vendor-config.toml 源替换配置（crates-io + 36 个 git 源，缺了离线必挂）
│   ├── cargo-bins/              cargo-expand / flutter_rust_bridge_codegen
│   ├── vcpkg/installed/         预编译的静态库（libvpx/libyuv/aom/opus…）
│   ├── cmake/                   独立 CMake（VS 没装 C++ CMake 组件时用）
│   ├── llvm/bin/libclang.dll    bindgen / ffigen 要用
│   └── pub-cache/               Dart 包缓存（flutter pub get 不用连 pub.dev）
├── source/rustdesk/             RustDesk 源码 + 已生成好的 bridge 代码
├── agent/                       agent.py / build_job.py
└── patches/
    └── fix-flutter-toolchain.ps1  给 Flutter 工具链打的两个补丁
```

## 唯一的前置条件：Visual Studio C++ 工具链

构建机必须已经有：

- **MSVC 工具集**（`Microsoft.VisualStudio.Component.VC.Tools.x86.x64`）—— VS 2019/2022 都行
- **Windows 10/11 SDK**
- **CMake** —— 包里带了独立的，`install-offline.ps1` 会把 `FLUTTER_CMAKE_PATH` 指过去

> **不需要**装完整的「使用 C++ 的桌面开发」工作负载。Flutter 3.24.5 原本要求
> VS 注册了该工作负载且带 VS 自带的 CMake 组件，内网机器上装不了（要管理员改系统设置）。
> 包里的 `patches/fix-flutter-toolchain.ps1` 已经放宽了这个检查。
>
> 如果连 MSVC 都没有，VS 官方提供离线布局：
> `vs_layout.exe --layout <目录> --add Microsoft.VisualStudio.Workload.NativeDesktop`，
> 把整个目录拷到内网机器后用 `setup.exe modify --installPath <路径> --noWeb` 安装。

## 安装

把整个目录拷到构建机（U 盘 / 内网共享 / scp 都行），然后：

```powershell
powershell -ExecutionPolicy Bypass -File install-offline.ps1
```

默认装到 `~/rust`（Rust）、`~/flutter`（Flutter）、`~/vcpkg`（预置库）、`~/rustdesk-src`（源码）。
可以用参数改：

```powershell
powershell -ExecutionPolicy Bypass -File install-offline.ps1 `
    -RustHome D:\tools\rust -FlutterHome D:\tools\flutter `
    -VcpkgHome D:\tools\vcpkg -RepoHome D:\work\rustdesk-src
```

脚本会做 6 件事：装 Rust → 装 Flutter（并打补丁）→ 铺 vcpkg 预置库 →
拷源码并**离线解析 Dart 依赖** → 写 cargo 离线源替换 → 写环境变量，
最后**自检 16 项**。

> 第 5 步里的 `flutter pub get --offline` 不能省：`flutter build` 会隐式跑
> `pub get`，默认去 pub.dev 解析版本，内网机上必然卡住。`--offline` 让它只用
> 包里的 `pub-cache`，顺带把 `.dart_tool/package_config.json` 生成好，
> 之后的 build 就完全不碰网络了。

### Rust 是怎么装的

用的是官方 `rust-1.99.1-x86_64-pc-windows-msvc.msi`，**按用户安装**：

```
msiexec /i rust-1.99.1-x86_64-pc-windows-msvc.msi /qn ALLUSERS=2 MSIINSTALLPERUSER=1 INSTALLDIR=<RustHome>
```

- **不需要管理员权限**，内网机器上装得动
- **不需要 rustup**——MSI 里直接就是 `rustc` / `cargo` / `rustfmt` / `clippy`，
  装完立刻能用，不存在「注册工具链」这一步
- 装完 `RustHome\bin` 会插到用户 PATH **最前面**，机器上万一已有 rustup 的
  shim 也不会抢走 `rustc`

> 为什么不用官方的 standalone tarball：它里面的 `bin/rustc.exe` 是指向
> `../rustc/bin/rustc.exe` 的**符号链接**，内网机器没有创建符号链接的权限，
> 解压出来是坏的。MSI 没有这个问题。

## 把构建机挂到管理面板

```powershell
python agent\agent.py --panel http://<服务器IP>:21120 --token <agent_token>
```

`agent_token` 在服务器 `/etc/rustdesk-panel/panel.conf` 里（`agent_token=` 那一行），
装服务端时由 `install.sh` 生成。

挂上之后，你在面板「客户端构建」页点「开始构建」，选 Windows x64，
任务会派发到这台机器，编好后产物自动回传面板，你在面板上下载即可。

> **agent 要常驻，不能挂在会退出的命令行窗口上。**
> 窗口一关（或 SSH 一断）agent 就没了，面板会一直停在「等待构建机接单」。
> 推荐两种挂法：
>
> ```powershell
> # 1) 脱离进程, 关窗口不停 (最简单)
> Start-Process -WindowStyle Hidden -FilePath python `
>   -ArgumentList "agent\agent.py --panel http://<服务器IP>:21120 --token <agent_token>"
> ```
>
> ```
> 2) 计划任务 (开机自启 + 异常自动重启)
> schtasks /create /tn RustDeskBuildAgent /sc ONSTART /rl HIGHEST /tr ^
>   "python D:\tools\rustdesk-offline-bundle\agent\agent.py --panel http://<服务器IP>:21120 --token <agent_token>"
> ```
>
> 装成 Windows 服务（NSSM / sc.exe）也可以，效果一样。

## 手工编一次（不经过面板）

```powershell
cd $env:USERPROFILE\rustdesk-src\flutter
..\flutter\bin\flutter.bat build windows --release
```

## 两个补丁做了什么

`patches/fix-flutter-toolchain.ps1` 改的是 Flutter 工具链的 Dart 源码：

1. **symlink → 目录联接**
   `flutter build windows` 要在 `windows/flutter/ephemeral/.plugin_symlinks/<插件名>`
   建符号链接指向插件根目录。没有 symlink 权限时（未开开发人员模式 / 非管理员）
   报 `ERROR_PRIVILEGE_NOT_HELD (1314)`。补丁让它失败时退化成 `mklink /J`
   （目录联接**不需要管理员**）。

2. **放宽 Visual Studio 检测**
   `cmakePath` 找不到 VS 自带的 cmake 时，退回 `FLUTTER_CMAKE_PATH` 指定的独立 CMake。

> 改完 Flutter 的 .dart 源码后，**必须删掉 `bin/cache/flutter_tools.stamp`** 才会重建
> snapshot —— stamp 是按 git revision 判定的，改源码不触发重建。补丁脚本已经代劳。

## 已知限制

- Flutter 首次运行会尝试 `git fetch` 检查更新。**离线时这步会失败但不影响构建**，
  只是会打一条警告。若要彻底消掉，可以在 `flutter/` 目录里把 `.git` 删掉。
- 包里带的是 **Flutter 3.24.5**（项目 CI 指定的版本）。换版本需要重新生成 bridge。
- `cargo-vendor` 是完整依赖图（含 macOS/Linux/Android 平台的 crate），
  离线机只编 Windows，用不到的那些是冗余但无害。
