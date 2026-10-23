# 客户端（Windows x64）

已经编好的客户端，解压即用。

```
rustdesk-client-windows-x64.zip
```

## 怎么用

1. 解压
2. 把整个 `Release` 目录（zip 解开后那一层）发给用户，或者让用户自己解压
3. 双击 `rustdesk.exe`

**不需要安装，不需要配置。** 这个包已经内置了服务器地址和 Key，
启动后就能连上自建服务器。

## 里面有什么

| 文件 | 是什么 |
|---|---|
| `rustdesk.exe` | 主程序 |
| `librustdesk.dll` | 核心（连接、编解码、采集） |
| `flutter_windows.dll` | 界面 |
| `data/` | 界面资源和字体 |
| 各个 `*_plugin.dll` | 剪贴板、多窗口、文件选择等插件 |

## 换成自己的 logo

包里的 logo 和应用名是打包时烘焙进去的，改了要重新编：

1. 面板「品牌定制」页 → 上传 logo / 填应用名 / 选主题色
2. 走一遍「客户端构建」页 → 开始构建 → 下载新包

如果手上没有 Windows 编译机，也可以在麒麟服务器上编 Linux 版客户端
（服务器上要有源码树，面板会自动识别）。

## brand-bundle.zip / branding/

面板导出的品牌包，里面是：

| 文件 | 是什么 |
|---|---|
| `branding.json` | 应用名、主题色、用了哪些图片 |
| `client-config.json` | 服务器地址（ID / Relay / API）和 Key |
| `server-config.b64` | 客户端配置串，可以贴给用户手动导入 |
| `logo.png` / `logo_light.png` / `logo_dark.png` / `icon.png` | 品牌图片 |
| `README.txt` | 手工烘焙这些资源的说明 |

**注意**：包里现在是占位图和示例应用名（68 字节的小图 + 「测试远控」），
正式发布前在面板上换成真的。

## 手工改配置

不想重编的话，用户可以在客户端「设置 → ID 服务器」里贴 `server-config.b64`
那一串，或者在面板「客户端集成」页拿到配置串再贴。
