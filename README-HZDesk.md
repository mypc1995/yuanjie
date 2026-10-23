# HZDesk —— RustDesk 私有化定制源码

从 `D:\Software\RustDesk` 拷出来的**纯源码**树（不含编译产物、依赖缓存、下载包）。
1516 个文件，24 MB，**已经是一个 git 仓库**，克隆下来就能编。

## 推到 Git

仓库已经 `git init` 并做好了首次提交，直接加远端推就行：

```bash
cd /d/HZDesk
git remote add origin <你的仓库地址>     # Gitee / GitLab / 内网 Git 都行
git branch -M main
git push -u origin main
```

推之前先把提交身份改成你自己的（仓库里现在是占位身份）：

```bash
git config user.name  "你的名字"
git config user.email "你的邮箱"
```

> **注意**：`git config` 不带 `--global` 时只改这个仓库，不影响你机器上别的项目。

## 这个仓库里有什么

| 目录 | 是什么 |
|---|---|
| `src/` | 客户端主程序（Rust） |
| `libs/` | `hbb_common`（与服务端共享）、`base`（客户端专属）、`scrap`、`enigo`、`clipboard` 等 |
| `flutter/` | 界面（Flutter），`lib/` 是 Dart 代码，`windows/` `linux/` `macos/` `android/` `ios/` 是平台工程 |
| `res/` | 图标、vcpkg 端口定义（含本仓库改过的 libyuv overlay port） |
| `.deploy/` | 私有化部署这一摊：管理面板、构建调度、离线包脚本、服务端一键部署脚本 |
| `.cargo/` | 编译选项（静态 CRT 等） |
| `docs/` `examples/` `appimage/` `flatpak/` `fastlane/` | 上游自带的杂项 |
| `.vcpkg-assets/` | vcpkg 端口的下载缓存（重编 vcpkg 依赖时用，**不提交**） |

## 没有提交什么（以及为什么）

| 没进版本库 | 为什么 | 要用的时候去哪儿拿 |
|---|---|---|
| `target/` (13 GB) | 编译产物，机器相关 | 重新编就行 |
| `.vendor/` (2.5 GB) | cargo vendor 出来的依赖源码 | 见下面的「离线编译」 |
| `vcpkg_installed/` `installed/` (1 GB) | 编好的 libvpx/libyuv/aom/opus 等静态库 | 见下面的「离线编译」 |
| `.dl/` (1.5 GB) | 下载缓存（Rust MSI、CMake 等） | 不需要 |
| `flutter/.dart_tool/` `flutter/build/` `**/ephemeral/` | Dart 依赖解析结果、Flutter 构建产物、插件临时目录 | `flutter pub get` / `flutter build` 会重新生成 |
| `src/version.rs` | 由 `build.rs` 生成 | 编译时自动生成 |
| `.deploy/package/server/{amd64,arm64}/` | 服务端官方二进制（下载来的，不是源码） | `D:\Software\rustdesk-deployment\01-server-kylin\server\` |
| `.deploy/*.zip`、`.vcpkg-assets/` | 打好的套件、vcpkg 下载缓存 | 同上 |
| `.deploy/panel-credentials.txt` | **面板密码** —— 绝不能进版本库 | 服务器 `/etc/rustdesk-panel/panel.conf` |

> `flutter/assets/` 里**没有** `logo.png` / `icon.png` 是正常的 —— 那是品牌化构建时
> 由面板注入的，仓库里原本只有 `icon.svg`。

## 和上游的两点不一样

1. **`libs/hbb_common` 已拍平** —— 上游它是 git 子模块，这里把它并进主仓库
   （`libs/hbb_common/.git` 和根目录的 `.gitmodules` 都已移除）。
   这样一次 push 就是完整源码，clone 下来不用再拉子模块。
   代价：以后想同步上游的 hbb_common 改动，得手动 diff 而不是 `git submodule update`。
2. **bridge 生成代码提交进来了**（`src/bridge_generated.rs`、`flutter/lib/generated_bridge.dart` 等）。
   上游靠 CI 重新生成，本仓库不跑 CI，提交进来才能保证 clone 下来直接编。
   要重新生成就用 `flutter_rust_bridge_codegen` 1.80.1（`--llvm-path` 指向 libclang）。

## 怎么编

### 在这台机器上（有网）

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

### 完全离线

用之前做好的离线包（自带全部依赖源码和预置库）：

```
D:\Software\rustdesk-deployment\02-build-agent-windows\rustdesk-offline-bundle\
```

`install-offline.ps1` 会把环境装好并把源码铺到目标目录，之后就在那儿编。

## 私有化部署那一摊在 `.deploy/`

| 路径 | 是什么 |
|---|---|
| `.deploy/panel/` | Web 管理面板（登录、品牌定制、客户端集成、构建任务下发） |
| `.deploy/builder/` | 构建调度：`agent.py`（构建机客户端）+ `build_job.py`（编译执行） |
| `.deploy/package/` | 服务端一键部署包（`install.sh` + 服务端二进制 + 面板 + systemd 配置） |
| `.deploy/package/client/package.ps1` | 手工烘焙品牌（logo / 应用名 / 服务器配置）出安装包 |
| `.deploy/install-offline.ps1` | Windows 构建机离线安装脚本 |
| `.deploy/prepare-offline-bundle.ps1` | 组装离线包 |
| `.deploy/test_*.py` | 面板与构建链路的测试 |

已经打好的一键部署包在 `D:\Software\rustdesk-deployment\`（服务端 + 构建机 + 客户端成品）。

## 这个源码树里我们改过的地方

相对上游 RustDesk 1.5.1：

| 改动 | 在哪 |
|---|---|
| 编译期预置服务器配置（装完即连自建服务器） | `src/server_preset.rs` + `src/server_preset.txt` |
| 界面上的应用名可预置 | `flutter/lib/common/server_preset.dart` |
| libyuv 端口改成优先用本机 clang、没有就用 MSVC 编（离线机不用带 LLVM） | `res/vcpkg/libyuv/portfile.cmake` |
| `libs/base` 拆分（客户端专属代码从 `hbb_common` 里挪出来） | `libs/base/` |

改动都很小，没有动 `libs/hbb_common` 的共享逻辑。

## 注意事项

- `libs/hbb_common` 是**与服务端共享的子模块**，改它要同步改服务端仓库，代价是来回沟通。客户端专属的东西请放 `libs/base`。
- `cargo build` 必须带 `--locked`，`Cargo.lock` 是锁死的。
- `flutter pub get` 在离线环境要加 `--offline`，否则会去连 pub.dev。
