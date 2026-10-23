<#
.SYNOPSIS
  组装 Windows 构建机的**离线环境包**。

  在有网的机器上跑这个脚本, 把构建所需的全部东西打成一个目录:
    - Rust 工具链 (官方 MSI, 离线机按用户安装, 不需要管理员)
    - Flutter SDK
    - vcpkg 预置的静态库 (installed/x64-windows-static) —— 离线机不用编译
    - cargo vendor (全部 Rust 依赖源码, 离线机不连 crates.io)
    - 预编译的 cargo-expand / flutter_rust_bridge_codegen
    - 独立 CMake (VS 没装 C++ CMake 组件时 Flutter 会退回用它)
    - libclang.dll (bindgen 和 ffigen 都要)
    - Flutter 工具链补丁 (symlink->junction, 放宽 VS 检测)
    - pub cache (flutter pub get 不用连 pub.dev)
    - RustDesk 源码 + 已生成好的 bridge 代码
    - agent 与构建脚本

  产物可以直接拷进纯内网环境。

.EXAMPLE
  pwsh -File prepare-offline-bundle.ps1 -OutDir D:\Software\RustDesk\.offline-bundle
#>
param(
  [string] $OutDir      = "D:\Software\RustDesk\.offline-bundle",
  [string] $RepoRoot   = "D:\Software\RustDesk",
  [string] $RustVersion = "1.99.1",
  [string] $FlutterVersion = "3.24.5",
  [string] $CargoRegistry = "$env:USERPROFILE\.cargo"
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest
$ProgressPreference = "SilentlyContinue"

function Say([string]$m) { Write-Host "  $m" -ForegroundColor Green }
function Warn([string]$m) { Write-Host "  ! $m" -ForegroundColor Yellow }
function Die([string]$m) { Write-Host "  x $m" -ForegroundColor Red; exit 1 }

$curl = "$env:SystemRoot\System32\curl.exe"
$tar  = "$env:SystemRoot\System32\tar.exe"

# 输出目录不能放在仓库里: 后面 robocopy 拷源码时会把输出目录一起拷进
# 自己的 source 目录, 变成无限递归 (踩过一次, 5GB 里 2.5GB 是自我复制)。
$repoFull = [System.IO.Path]::GetFullPath($RepoRoot).TrimEnd('\')
$outFull  = [System.IO.Path]::GetFullPath($OutDir).TrimEnd('\')
if ($outFull.StartsWith($repoFull + '\', [System.StringComparison]::OrdinalIgnoreCase)) {
  Write-Host "  x 输出目录不能放在仓库内 (会自我递归): $OutDir" -ForegroundColor Red
  Write-Host "    请改到仓库外, 例如 D:\Software\rustdesk-offline-bundle" -ForegroundColor Red
  exit 1
}

New-Item -ItemType Directory -Path $OutDir -Force | Out-Null
foreach ($d in "toolchain\rust", "toolchain\flutter", "toolchain\vcpkg", "toolchain\cargo-vendor", "toolchain\cargo-bins", "toolchain\llvm\bin", "toolchain\cmake", "toolchain\pub-cache", "source", "agent", "patches") {
  New-Item -ItemType Directory -Path (Join-Path $OutDir $d) -Force | Out-Null
}

# ---------------------------------------------------------------- Rust
# 用官方 MSI 而不是 standalone tarball:
#   tarball 里的 bin/rustc.exe 是指向 ../rustc/bin/rustc.exe 的**符号链接**,
#   内网机器没有 SeCreateSymbolicLink 权限就根本解不出来 (实测踩过)。
#   MSI 可以 ALLUSERS=2 + MSIINSTALLPERUSER=1 做按用户安装, 不需要管理员,
#   也不含 rustup, 离线机上直接就有 rustc / cargo / rustfmt / clippy。
Write-Host "`n==> Rust 工具链 $RustVersion" -ForegroundColor Cyan
$rustDir = Join-Path $OutDir "toolchain\rust"
$rustMsi = Join-Path $rustDir "rust-$RustVersion-x86_64-pc-windows-msvc.msi"
if (-not (Test-Path $rustMsi)) {
  $u = "https://static.rust-lang.org/dist/rust-$RustVersion-x86_64-pc-windows-msvc.msi"
  Say "下载 $u"
  & $curl -fsSL --retry 3 -o $rustMsi $u
  if ($LASTEXITCODE -ne 0) { Die "Rust MSI 下载失败" }
}
Say "Rust MSI $([math]::Round((Get-Item $rustMsi).Length/1MB,1)) MB"

# ---------------------------------------------------------------- Flutter
Write-Host "`n==> Flutter SDK $FlutterVersion" -ForegroundColor Cyan
$flutterDir = Join-Path $OutDir "toolchain\flutter"
$flutterZip = Join-Path $flutterDir "flutter_windows_$FlutterVersion-stable.zip"
if (-not (Test-Path $flutterZip)) {
  $meta = Invoke-RestMethod "https://storage.googleapis.com/flutter_infra_release/releases/releases_windows.json"
  $rel = $meta.releases | Where-Object { $_.version -eq $FlutterVersion -and $_.channel -eq "stable" } | Select-Object -First 1
  if (-not $rel) { Die "找不到 Flutter $FlutterVersion" }
  $u = "$($meta.base_url)/$($rel.archive)"
  Say "下载 $u"
  & $curl -fsSL --retry 3 -o $flutterZip $u
}
Say "Flutter 包 $([math]::Round((Get-Item $flutterZip).Length/1MB,1)) MB"

# ---------------------------------------------------------------- vcpkg 预置库
Write-Host "`n==> vcpkg 预置 installed 目录" -ForegroundColor Cyan
# 直接把编好的 installed 树带过去 —— cargo 只需要里面的头文件和 .lib,
# 通过 VCPKG_INSTALLED_ROOT 指过去即可, 离线机根本不用跑 vcpkg install。
$vcpkgDir = Join-Path $OutDir "toolchain\vcpkg"
$instSrc = Join-Path $RepoRoot "vcpkg_installed\x64-windows-static"
$instDst = Join-Path $vcpkgDir "installed\x64-windows-static"
if (-not (Test-Path $instSrc)) { Die "找不到 $instSrc, 先把 vcpkg install 跑完" }
if (Test-Path $instDst) { Remove-Item $instDst -Recurse -Force }
New-Item -ItemType Directory -Path (Split-Path $instDst -Parent) -Force | Out-Null
Copy-Item $instSrc $instDst -Recurse -Force
if (Test-Path "C:\vcpkg\vcpkg.exe") { Copy-Item "C:\vcpkg\vcpkg.exe" (Join-Path $vcpkgDir "vcpkg.exe") -Force }
Say "installed 树 $([math]::Round((Get-ChildItem $instDst -Recurse -File | Measure-Object -Property Length -Sum).Sum/1MB,1)) MB"

# ---------------------------------------------------------------- 独立 CMake
# Flutter 找不到 VS 自带的 cmake 组件时会退回用它 (见 patches/fix-flutter-toolchain.ps1)
Write-Host "`n==> 独立 CMake" -ForegroundColor Cyan
$cmakeHome = Join-Path $OutDir "toolchain\cmake"
$cmakeSrc = "D:\Software\RustDesk\.dl\cmake-4.4.4-windows-x86_64"
if (Test-Path "$cmakeSrc\bin\cmake.exe") {
  if (Test-Path $cmakeHome) { Remove-Item $cmakeHome -Recurse -Force }
  New-Item -ItemType Directory -Path $cmakeHome -Force | Out-Null
  Copy-Item "$cmakeSrc\bin" (Join-Path $cmakeHome "bin") -Recurse -Force
  Copy-Item "$cmakeSrc\share" (Join-Path $cmakeHome "share") -Recurse -Force -ErrorAction SilentlyContinue
  Say "cmake $([math]::Round((Get-ChildItem $cmakeHome -Recurse -File | Measure-Object -Property Length -Sum).Sum/1MB,1)) MB"
} else {
  Warn "找不到独立 CMake, 离线机若没装 VS 的 C++ CMake 组件会构建失败"
}

# ---------------------------------------------------------------- libclang
# kcp-sys 和 scrap 的 bindgen 要 libclang.dll, 离线机没有 clang 就编不了。
# ffigen 找 libclang 的规则是 <llvm-path>/bin/libclang.dll, 所以这里按这个布局放。
Write-Host "`n==> libclang 运行时" -ForegroundColor Cyan
$lcDir = Join-Path $OutDir "toolchain\llvm\bin"
$libclang = "D:\Software\llvm\bin\libclang.dll"
if (-not (Test-Path $libclang)) { Die "找不到 $libclang, 先把 PyPI 的 libclang wheel 解出来" }
Copy-Item $libclang (Join-Path $lcDir "libclang.dll") -Force
Say "libclang.dll $([math]::Round((Get-Item $libclang).Length/1MB,1)) MB -> toolchain/llvm/bin/"

# ---------------------------------------------------------------- cargo vendor
Write-Host "`n==> cargo vendor (Rust 依赖源码)" -ForegroundColor Cyan
$vendorDir = Join-Path $OutDir "toolchain\cargo-vendor"
$vendorCfg = Join-Path $OutDir "toolchain\cargo-vendor-config.toml"
$env:PATH = "$CargoRegistry\bin;$env:PATH"
# cargo vendor 的**标准输出**就是源替换配置, 必须跟 vendor 目录一起带走。
# 只写 crates-io 一段是不够的 —— [patch.crates-io] 里那 4 个 git fork
# (webrtc/webrtc-util/webrtc-sctp/tungstenite) 和另外三十多个 git 依赖
# 都要靠这些 stanza 换成离线目录, 少了它们内网机上 cargo 会去 GitHub 拉, 必然失败。
$preVendored = Join-Path $RepoRoot ".vendor"
$vendorCfgSrc = Join-Path $RepoRoot ".deploy\vendor-config.toml"
$havePre = (Test-Path $preVendored) -and ((Get-ChildItem $preVendored -Directory -ErrorAction SilentlyContinue).Count -gt 100)

if ($havePre -and (Test-Path $vendorCfgSrc)) {
  Say "复用已编好的 vendor 目录 + 已保存的源替换配置"
  if (Test-Path $vendorDir) { Remove-Item $vendorDir -Recurse -Force }
  New-Item -ItemType Directory -Path $vendorDir -Force | Out-Null
  robocopy $preVendored $vendorDir /E /NFL /NDL /NJH /NJS /NP | Out-Null
  Copy-Item $vendorCfgSrc $vendorCfg -Force
} else {
  Say "跑 cargo vendor (下载 + 生成源替换配置), 约 2.5GB ..."
  Push-Location $RepoRoot
  try {
    & cargo vendor --locked --versioned-dirs $vendorDir 1> $vendorCfg 2> "$vendorCfg.err"
    if ($LASTEXITCODE -ne 0) { Die "cargo vendor 失败, 看 $vendorCfg.err" }
  } finally { Pop-Location }
}

# 配置里的 directory 指的是**这台机器**的路径, 离线机上不一样。
# 换成占位符, 安装脚本再填真实路径。
$cfgText = Get-Content $vendorCfg -Raw
$cfgText = $cfgText -replace '(?m)^directory\s*=\s*".*"\s*$', 'directory = "__VENDOR_DIR__"'
[System.IO.File]::WriteAllText($vendorCfg, $cfgText, (New-Object System.Text.UTF8Encoding($false)))
if ($cfgText -notmatch '__VENDOR_DIR__') { Die "vendor 配置里没找到 directory 行, 源替换会指错地方" }
$stanzas = ($cfgText -split "`n" | Where-Object { $_ -match '^\[' }).Count
Say "vendor 目录 $([math]::Round((Get-ChildItem $vendorDir -Recurse -File | Measure-Object -Property Length -Sum).Sum/1MB,1)) MB"
Say "源替换配置 $stanzas 段 (含 git 源)"

# ---------------------------------------------------------------- 预编译 codegen 工具
Write-Host "`n==> cargo-expand / flutter_rust_bridge_codegen" -ForegroundColor Cyan
$bins = Join-Path $OutDir "toolchain\cargo-bins"
foreach ($tool in @(@("cargo-expand", "1.0.95"), @("flutter_rust_bridge_codegen", "1.80.1"))) {
  $name = $tool[0]; $ver = $tool[1]
  $exe = Join-Path $bins "$name.exe"
  if (Test-Path $exe) { Say "$name 已有"; continue }
  Say "编译 $name $ver (只编一次, 离线机直接用)"
  $feat = if ($name -eq "flutter_rust_bridge_codegen") { "--features uuid" } else { "" }
  & cargo install $name --version $ver $feat --locked --root (Split-Path $bins -Parent) 2>&1 | Select-Object -Last 3
  if (Test-Path (Join-Path (Split-Path $bins -Parent) "bin\$name.exe")) {
    Copy-Item (Join-Path (Split-Path $bins -Parent) "bin\$name.exe") $exe -Force
  }
}
Get-ChildItem $bins -ErrorAction SilentlyContinue | Select-Object Name, Length | Format-Table -AutoSize

# ---------------------------------------------------------------- pub cache
Write-Host "`n==> Flutter pub cache (离线机不用连 pub.dev)" -ForegroundColor Cyan
$pubCache = "$env:LOCALAPPDATA\Pub\Cache"
if (-not (Test-Path $pubCache)) { $pubCache = "$env:APPDATA\Pub\Cache" }
$flutterBin = "D:\Software\flutter\bin"
if (Test-Path "$flutterBin\flutter.bat") {
  Push-Location "$RepoRoot\flutter"
  try {
    $env:PATH = "$flutterBin;$env:PATH"
    & "$flutterBin\flutter.bat" pub get 2>&1 | Select-Object -Last 6
  } finally { Pop-Location }
} else {
  Warn "没找到 Flutter, 跳过 pub get"
}
if (Test-Path $pubCache) {
  $pubDst = Join-Path $OutDir "toolchain\pub-cache"
  if (Test-Path $pubDst) { Remove-Item $pubDst -Recurse -Force }
  New-Item -ItemType Directory -Path $pubDst -Force | Out-Null
  Say "复制 pub cache ..."
  # 用 robocopy 而不是 Copy-Item: Copy-Item 复制几千个子目录时会中途失败,
  # 结果就是包里少了一大批包, 离线机上 pub get 直接失败 (踩过: 少了
  # flutter_plugin_android_lifecycle 等 224 个包)。
  $rc = Start-Process -FilePath "$env:SystemRoot\System32\robocopy.exe" `
        -ArgumentList "`"$pubCache`" `"$pubDst`" /E /NFL /NDL /NJH /NJS /NP /R:2 /W:1" `
        -Wait -PassThru -NoNewWindow
  if ($rc.ExitCode -ge 8) { Die "复制 pub cache 失败 (robocopy exit=$($rc.ExitCode))" }
  $n = (Get-ChildItem "$pubDst\hosted" -Recurse -Directory -ErrorAction SilentlyContinue | Measure-Object).Count
  Say "pub cache $([math]::Round((Get-ChildItem $pubDst -Recurse -File | Measure-Object -Property Length -Sum).Sum/1MB,1)) MB, $n 个包目录"
  # 抽查一个核心依赖, 少一个都会让离线机 pub get 失败
  if (-not (Get-ChildItem "$pubDst\hosted" -Recurse -Directory -Filter 'flutter_plugin_android_lifecycle-*' -ErrorAction SilentlyContinue)) {
    Die "pub cache 缺 flutter_plugin_android_lifecycle, 拷贝不完整"
  }
}

# ---------------------------------------------------------------- 源码 + bridge
Write-Host "`n==> RustDesk 源码 + bridge 代码" -ForegroundColor Cyan
$srcOut = Join-Path $OutDir "source\rustdesk"
if (Test-Path $srcOut) { Remove-Item $srcOut -Recurse -Force }
New-Item -ItemType Directory -Path $srcOut -Force | Out-Null
# 用**本地这份源码树**而不是重新克隆上游 —— 这个仓库有自己的改动
# (libs/base 拆分、res/vcpkg overlay port 等), 离线构建机要编的必须是它。
Say "复制本地源码 (排除构建产物) ..."
# 这个产品只做电脑端。flutter/android 和 flutter/ios 是移动端平台工程,
# Windows 构建根本用不到, 不进离线包 (省心也省 0.5MB)。
# .offline-bundle 一定要排除: 输出目录若在仓库内, robocopy 会把它拷进
# 自己的 source 目录里, 变成无限递归。
$excludeDirs = @("target", "vcpkg_installed", "installed", ".vendor", ".dl", ".deploy", ".git", "build", ".dart_tool", "android", "ios", ".offline-bundle", ".trash")
robocopy $RepoRoot $srcOut /E /XD @excludeDirs /NFL /NDL /NJH /NJS /NP /XF "*.pdb" "NUL" | Out-Null
Say "文件数 $((Get-ChildItem $srcOut -Recurse -File | Measure-Object).Count)"
if (Test-Path "$srcOut\libs\hbb_common\src\config.rs") { Say "hbb_common 已就位" } else { Die "libs/hbb_common 缺了" }
foreach ($f in "src\bridge_generated.rs", "src\bridge_generated.io.rs", "flutter\lib\generated_bridge.dart") {
  if (Test-Path (Join-Path $srcOut $f)) { Say "已带 $f" } else { Warn "缺 $f (离线机需要自己生成 bridge)" }
}

# ---------------------------------------------------------------- 补丁 + agent
Write-Host "`n==> Flutter 补丁与 agent" -ForegroundColor Cyan
$patchSrc = Join-Path $RepoRoot ".deploy\package\patches"
if (Test-Path $patchSrc) {
  Copy-Item "$patchSrc\fix-flutter-toolchain.ps1" (Join-Path $OutDir "patches\fix-flutter-toolchain.ps1") -Force
  Say "Flutter 工具链补丁已带上"
}
Copy-Item "$RepoRoot\.deploy\builder\agent.py"     (Join-Path $OutDir "agent\agent.py") -Force
Copy-Item "$RepoRoot\.deploy\builder\build_job.py" (Join-Path $OutDir "agent\build_job.py") -Force
Say "agent.py / build_job.py 已带上"

# 安装脚本和说明放在包根目录, 拿到包就能装
Copy-Item "$RepoRoot\.deploy\install-offline.ps1" (Join-Path $OutDir "install-offline.ps1") -Force
if (Test-Path "$RepoRoot\.deploy\package\windows-offline-README.md") {
  Copy-Item "$RepoRoot\.deploy\package\windows-offline-README.md" (Join-Path $OutDir "README.md") -Force
}
Say "install-offline.ps1 / README.md 已放到包根目录"

# ---------------------------------------------------------------- 清单
Write-Host "`n==> 生成 SHA256 清单" -ForegroundColor Cyan
# 只排除**根目录**那一个 manifest.json —— 按文件名排除会把包内其它同名文件
# (插件示例的 web manifest.json) 一起跳过, 那些文件就拿不到校验和了。
$manifestPath = Join-Path $OutDir "manifest.json"
$files = Get-ChildItem $OutDir -Recurse -File | Where-Object { $_.FullName -ne $manifestPath }
$manifest = $files | ForEach-Object {
  [PSCustomObject]@{
    path = $_.FullName.Substring($OutDir.Length + 1).Replace("\", "/")
    size = $_.Length
    sha256 = (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLower()
  }
}
$manifest | ConvertTo-Json -Depth 3 | Set-Content $manifestPath -Encoding UTF8
Say "清单 $(($manifest | Measure-Object).Count) 个文件"

$total = (Get-ChildItem $OutDir -Recurse -File | Measure-Object -Property Length -Sum).Sum
Write-Host ""
Write-Host "==========================================================" -ForegroundColor Cyan
Write-Host " 离线包已生成: $OutDir" -ForegroundColor Cyan
Write-Host " 总体积: $([math]::Round($total/1GB,2)) GB" -ForegroundColor Cyan
Write-Host "==========================================================" -ForegroundColor Cyan
