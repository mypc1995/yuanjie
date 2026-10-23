# 在本机沙箱里当「Windows 构建机」跑 agent, 用于端到端联调。
# 用法: pwsh -File run_agent_sandbox.ps1 -Panel http://<服务器IP>:21120 -Token <agent_token>
param(
  [string] $Panel = $env:RUSTDESK_PANEL_URL,
  [string] $Token = "<AGENT_TOKEN>",
  [string] $Sandbox = "D:\OfflineTest",
  [string] $Bundle = "D:\Software\rustdesk-offline-bundle"
)

$ErrorActionPreference = 'Continue'

# 沙箱里装出来的工具链
$rustBin   = "$Sandbox\rust\bin"
$flutterBin= "$Sandbox\flutter\bin"
$cargoBins = "$Bundle\toolchain\cargo-bins"
$vcpkgHome = "$Sandbox\vcpkg"

# 顺序很重要: RustHome\bin 必须压过机器上可能存在的 rustup shim
$env:PATH = "$rustBin;$flutterBin;$cargoBins;$env:PATH"
$env:CARGO_HOME = "$env:USERPROFILE\.cargo"
$env:LIBCLANG_PATH = "$Bundle\toolchain\llvm\bin"
$env:VCPKG_ROOT = $vcpkgHome
$env:VCPKG_INSTALLED_ROOT = "$vcpkgHome\installed"
$env:PUB_CACHE = "$Bundle\toolchain\pub-cache"
$env:FLUTTER_CMAKE_PATH = "$Bundle\toolchain\cmake\bin\cmake.exe"
$env:RUSTDESK_SRC_DIR = "$Sandbox\src\rustdesk"
$env:RUSTDESK_BUILDER_ROOT = "$Sandbox\builder"

Write-Host "PATH 前缀 : $rustBin ; $flutterBin ; $cargoBins"
Write-Host "源码目录 : $env:RUSTDESK_SRC_DIR"
Write-Host "面板     : $Panel"

& python "$Bundle\agent\agent.py" --panel $Panel --token $Token --src $env:RUSTDESK_SRC_DIR
