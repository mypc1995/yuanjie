# 内部说明（不对外）

这份文档记录的是二开过程中的内部决策和取舍，**不适合出现在对外的 README 里**。
放在 `.deploy/` 下是因为这部分内容只跟部署和维护有关。

## 一、和 Server Pro 的功能差距

本项目用**免费版 rustdesk-server**。Server Pro 的企业级管控能力我们**一个都没实现**：

| Server Pro 的能力 | 本项目 | 说明 |
|---|---|---|
| 多用户 / 团队 / 用户组 | 无 | 面板只有单个 admin 账号 |
| 角色权限（admin-roles / control-roles） | 无 | 纯面板侧可实现，工作量大 |
| 访问控制策略（strategies/assign） | 无 | 要自己写策略引擎 |
| 设备组 | 无 | 依赖策略引擎 |
| 审计日志 | 无 | 纯面板侧可实现，工作量小 |
| SSO（OIDC / OAuth / LDAP / SAML） | 无 | 纯面板侧可实现，工作量中等 |
| 地址簿 | 无 | 纯面板侧可实现，工作量小 |
| 设备管控（强制登出 / 锁设备 / 强制 TOTP） | 无 | **受限于免费版 hbbs 没有这些接口** |

已实现的（相对 Pro 也有）：Web 管理面板、品牌定制、客户端定制打包、
黑白名单、密钥管理、日志查看。

Pro 拿不到的那些接口，对应脚本在清理上游文件时已删掉（在 `res/` 下的
`users.py` / `strategies.py` / `control-roles.py` 等）。

**结论**：本项目方向是「私有化 + 品牌化 + 一键出包 + 内网离线构建」，
和 Pro 的「企业级管控」不重叠。真要补 Pro 那套，要么买 Pro 授权换服务端，
要么自己在面板侧重建一整套。

## 二、清理上游文件时删了什么

| 删掉的 | 原因 |
|---|---|
| `docs/` | 上游多语言 README、行为准则、贡献指南 |
| `.github/` | 上游 CI / 发布流水线（fdroid 发布、macOS 签名、上游 Flutter 补丁） |
| `flutter/android` `ios` `macos` | 移动端与 macOS 平台工程 —— 本产品只出 Windows 客户端 |
| `flutter/build_*.sh` `ndk_*.sh` `ios_*.sh` | 移动端构建脚本 |
| `appimage/` `flatpak/` `fastlane/` `examples/` | 上游的 Linux 打包、Android 商店素材、示例 |
| `res/DEBIAN` `fdroid` `PKGBUILD` `rpm*.spec` | 上游的 deb/rpm 打包链 |
| `res/{users,devices,device-groups,strategies,audits,admin-roles,control-roles,user-groups,job,lang,ab}.py` | Server Pro 管理 API 客户端与许可证激活脚本 |
| `Dockerfile` `entrypoint.sh` `build.py` | 上游的 Docker 与 Linux 打包脚本 |
| `res/mac-*` `res/*x128*.png` `res/logo-header.svg` `res/rustdesk-banner.svg` `res/*.desktop` `res/rustdesk.service` | macOS 图标、Linux 桌面图标与打包文件、README 配图 |
| `.vcpkg-assets/` | vcpkg 端口的下载缓存（30MB，不是源码） |

## 三、特意留下没删的

| 留下的 | 为什么 |
|---|---|
| `src/ui/` | 上游已废弃的 Sciter 界面。它还挂在 crate 的**非 flutter 编译路径**上（`cargo build` 不带 `--features flutter` 会编它），删掉要连带改 `src/lib.rs`、`src/keyboard.rs` 和被 flutter 路径共用的 `src/ui_session_interface.rs`，风险大收益小。**本项目只用 Flutter 界面**，编译命令一律带 `--features flutter` |
| `res/vcpkg-triplets/` | 4 个 Android triplet。`vcpkg.json` 的 `overlay-triplets` 指着它，删目录就得同步改 `vcpkg.json`，会动到已验证过的 vcpkg 依赖编译链。才 2KB |
| `res/msi/` | Windows 客户端安装包工程（WiX）。上游品牌名（`RustDesk.wxs` 等）要自己改成远界，但这是唯一能出正规安装包的路径，先留着 |
| `src/lang/` | 客户端界面多语言 |

## 四、注释语言

全仓 14000 多行注释已整体翻成中文（`src/` `libs/` `flutter/lib/`，333 个文件、13276 行）。

保持英文的例外：

- `src/lang/*.rs` 是界面翻译表，里面的中文是**界面内容**不是注释
- 标识符、类型名、文件路径、URL、反引号里的代码片段
- 语言器指令（`ignore:` / `ignore_for_file:` / `@nodoc` 等）—— 翻了会失效

批量翻译用的脚本是 `.deploy/tools/comment_translate.py`：翻译完回填时逐文件校验
「去掉注释后的代码骨架与原文逐字节一致」，不一致就整文件回滚，保证代码一行没动。