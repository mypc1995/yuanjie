<#
.SYNOPSIS
  Windows 构建机 —— **纯离线** 环境安装。

  把 prepare-offline-bundle.ps1 生成的整个目录拷到内网机器上, 然后:
      powershell -ExecutionPolicy Bypass -File install-offline.ps1

  整个过程不联网: Rust / Flutter / vcpkg 预置库 / cargo 依赖 / codegen 工具
  全部来自本目录。

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File install-offline.ps1
  powershell -ExecutionPolicy Bypass -File install-offline.ps1 -RustHome D:\tools\rust -FlutterHome D:\tools\flutter
#>
param(
  [string] $BundleRoot = $PSScriptRoot,
  [string] $RustHome   = "$env:USERPROFILE\.rust",
  [string] $FlutterHome = "$env:USERPROFILE\flutter",
  [string] $VcpkgHome = "$env:USERPROFILE\vcpkg",
  [string] $RepoHome  = "$env:USERPROFILE\rustdesk-src",
  [string] $RustVersion = "1.99.1"
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest
$ProgressPreference = "SilentlyContinue"
$tar = "$env:SystemRoot\System32\tar.exe"

function Say([string]$m) { Write-Host "  $m" -ForegroundColor Green }
function Warn([string]$m) { Write-Host "  ! $m" -ForegroundColor Yellow }
function Die([string]$m) { Write-Host "  x $m" -ForegroundColor Red; exit 1 }

Write-Host "`n=== RustDesk Windows 构建机 —— 离线安装 ===" -ForegroundColor Cyan
Write-Host "包目录: $BundleRoot`n"

# ------------------------------------------------------------ 1. Rust
# 用官方 MSI 做**按用户安装** (ALLUSERS=2 + MSIINSTALLPERUSER=1):
#   - 不需要管理员权限, 内网机器上装得动
#   - 没有符号链接问题 (standalone tarball 的 bin/rustc.exe 是指向
#     ../rustc/bin/rustc.exe 的 symlink, 无权限根本解不出来)
#   - 自带 rustc / cargo / rustfmt / clippy, 不含 rustup, 流程更简单
Write-Host "==> 1/6 Rust 工具链" -ForegroundColor Cyan

# 同一个 MSI 重复安装时, Windows Installer 会当成「维护已有产品」,
# **原来的 InstallLocation 不变**, 传进去的 INSTALLDIR 被忽略。
# 所以先查一下这台机器上是不是已经装过了, 装过就直接复用, 别装两遍。
function Get-InstalledRustHome {
  $paths = @(
    'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\*',
    'HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall\*'
  )
  foreach ($k in $paths) {
    # 有些卸载项没有 DisplayName/InstallLocation, Set-StrictMode 下直接读会抛错,
    # 所以先看属性存不存在。
    foreach ($it in (Get-ItemProperty $k -ErrorAction SilentlyContinue)) {
      $dn = $it.PSObject.Properties['DisplayName']
      if (-not $dn -or $dn.Value -notlike 'Rust * (MSVC 64-bit)') { continue }
      $locProp = $it.PSObject.Properties['InstallLocation']
      if (-not $locProp) { continue }
      $loc = $locProp.Value
      if ($loc -and (Test-Path (Join-Path $loc "bin\rustc.exe"))) {
        return $loc.TrimEnd('\')
      }
    }
  }
  return $null
}

$msi = Get-ChildItem "$BundleRoot\toolchain\rust" -Filter "rust-$RustVersion-x86_64-pc-windows-msvc.msi" -ErrorAction SilentlyContinue |
       Select-Object -First 1
if (-not $msi) { Die "包里没有 Rust $RustVersion 的 MSI 安装包 (toolchain\rust\rust-$RustVersion-x86_64-pc-windows-msvc.msi)" }

$rustcExe = Join-Path $RustHome "bin\rustc.exe"
$existing = Get-InstalledRustHome
if ($existing -and (Test-Path (Join-Path $existing "bin\rustc.exe"))) {
  if ($existing -ne $RustHome.TrimEnd('\')) {
    Say "这台机器已经装过 Rust ($existing), 直接复用"
    $RustHome = $existing
    $rustcExe = Join-Path $RustHome "bin\rustc.exe"
  } else {
    Say "已装过, 跳过"
  }
} else {
  New-Item -ItemType Directory -Path $RustHome -Force | Out-Null
  Say "按用户安装 Rust $RustVersion (MSI, 不需要管理员) ..."
  $msiLog = Join-Path $env:TEMP "rust-$RustVersion-msi.log"
  $argLine = "/i `"$($msi.FullName)`" /qn ALLUSERS=2 MSIINSTALLPERUSER=1 INSTALLDIR=`"$RustHome`" /norestart /L*v `"$msiLog`""
  $proc = Start-Process -FilePath "$env:SystemRoot\System32\msiexec.exe" -ArgumentList $argLine -Wait -PassThru
  $code = $proc.ExitCode
  if ($code -eq 1618) {
    # 1618 = 另一个安装正在进行, 等一会儿重试一次
    Warn "另一个安装正在进行 (1618), 30 秒后重试 ..."
    Start-Sleep -Seconds 30
    $proc = Start-Process -FilePath "$env:SystemRoot\System32\msiexec.exe" -ArgumentList $argLine -Wait -PassThru
    $code = $proc.ExitCode
  }
  switch ($code) {
    0       { Say "MSI 安装完成 (exit 0)" }
    3010    { Say "MSI 安装完成 (exit 3010, 需要重启, 但不影响构建)" }
    1638    { Say "已安装同版本 (exit 1638), 继续" }
    default {
      Warn "MSI 安装失败, exit code = $code"
      if (Test-Path $msiLog) {
        Warn "日志尾部:"
        Get-Content $msiLog -Tail 15 -ErrorAction SilentlyContinue | ForEach-Object { Warn "  $_" }
        Warn "完整日志: $msiLog"
      }
      Die "Rust 工具链安装失败"
    }
  }
  # MSI 可能把工具链装到了别的地方 (重复安装时保持原位置), 再查一次注册表
  if (-not (Test-Path $rustcExe)) {
    $existing = Get-InstalledRustHome
    if ($existing) {
      Say "工具链实际装在 $existing, 改用它"
      $RustHome = $existing
      $rustcExe = Join-Path $RustHome "bin\rustc.exe"
    }
  }
}
if (-not (Test-Path $rustcExe)) { Die "装完找不到 rustc.exe ($rustcExe)" }

$env:CARGO_HOME = "$env:USERPROFILE\.cargo"
# PATH 里把 RustHome 放最前面: 机器上若已有 rustup shim, 要让本包的 rustc 优先
$env:PATH = "$RustHome\bin;$env:USERPROFILE\.cargo\bin;$env:PATH"
# bindgen (kcp-sys / scrap) 要 libclang
$env:LIBCLANG_PATH = "$BundleRoot\toolchain\llvm\bin"
Say "rustc: $(& rustc --version)"

# ------------------------------------------------------------ 2. Flutter
Write-Host "`n==> 2/6 Flutter SDK" -ForegroundColor Cyan
$flutterZip = Get-ChildItem "$BundleRoot\toolchain\flutter" -Filter "*.zip" | Select-Object -First 1
if (-not $flutterZip) { Die "包里没有 Flutter SDK" }
if (-not (Test-Path (Join-Path $FlutterHome "bin\flutter.bat"))) {
  Say "解压 Flutter (约 1.6GB, 稍慢) ..."
  $tmp = Join-Path $env:TEMP "flutter-extract"
  if (Test-Path $tmp) { Remove-Item $tmp -Recurse -Force }
  New-Item -ItemType Directory -Path $tmp -Force | Out-Null
  & $tar -xf $flutterZip.FullName -C $tmp
  $inner = Get-ChildItem $tmp -Directory | Select-Object -First 1
  if (Test-Path $FlutterHome) { Remove-Item $FlutterHome -Recurse -Force }
  Move-Item $inner.FullName $FlutterHome
  Remove-Item $tmp -Recurse -Force
}
$env:PATH = "$FlutterHome\bin;$RustHome\bin;$env:USERPROFILE\.cargo\bin;$env:PATH"
& git config --global --add safe.directory $FlutterHome 2>&1 | Out-Null
# pub cache 放本地, flutter pub get 不用连 pub.dev
$pubCacheLocal = "$BundleRoot\toolchain\pub-cache"
if (Test-Path $pubCacheLocal) {
  $env:PUB_CACHE = $pubCacheLocal
  [Environment]::SetEnvironmentVariable("PUB_CACHE", $pubCacheLocal, "User")
  Say "pub cache: $pubCacheLocal"
}
# 独立 CMake —— Flutter 找不到 VS 自带的 cmake 时会退回用它
$cmakeHome = "$BundleRoot\toolchain\cmake"
if (Test-Path "$cmakeHome\bin\cmake.exe") {
  $env:FLUTTER_CMAKE_PATH = "$cmakeHome\bin\cmake.exe"
  [Environment]::SetEnvironmentVariable("FLUTTER_CMAKE_PATH", $env:FLUTTER_CMAKE_PATH, "User")
  Say "独立 CMake: $env:FLUTTER_CMAKE_PATH"
}
# 给 Flutter 工具链打补丁: symlink->junction, 以及放宽 VS 检测
$patchScript = "$BundleRoot\patches\fix-flutter-toolchain.ps1"
if (Test-Path $patchScript) {
  Say "给 Flutter 工具链打补丁 ..."
  & powershell -ExecutionPolicy Bypass -File $patchScript -FlutterHome $FlutterHome 2>&1 |
    ForEach-Object { Say "  $_" }
}
Say "flutter: $((& flutter --version 2>&1 | Select-Object -First 1))"

# ------------------------------------------------------------ 3. vcpkg 预置库
Write-Host "`n==> 3/6 vcpkg 预置库" -ForegroundColor Cyan
$instSrc = "$BundleRoot\toolchain\vcpkg\installed"
if (-not (Test-Path $instSrc)) { Die "包里没有 vcpkg installed 目录" }
$instDst = Join-Path $VcpkgHome "installed"
if (-not (Test-Path (Join-Path $instDst "x64-windows-static\include"))) {
  New-Item -ItemType Directory -Path $VcpkgHome -Force | Out-Null
  Say "复制预置的静态库和头文件 ..."
  Copy-Item $instSrc $instDst -Recurse -Force
}
Copy-Item "$BundleRoot\toolchain\vcpkg\vcpkg.exe" (Join-Path $VcpkgHome "vcpkg.exe") -Force -ErrorAction SilentlyContinue
Say "x64-windows-static: $((Get-ChildItem "$instDst\x64-windows-static" -Recurse -File | Measure-Object).Count) 个文件"

# ------------------------------------------------------------ 4. cargo 依赖
Write-Host "`n==> 4/6 cargo 依赖 (vendor)" -ForegroundColor Cyan
$vendorDir = "$BundleRoot\toolchain\cargo-vendor"
if (-not (Test-Path $vendorDir)) { Die "包里没有 cargo-vendor 目录" }
Say "vendor $([math]::Round((Get-ChildItem $vendorDir -Recurse -File | Measure-Object -Property Length -Sum).Sum/1MB,1)) MB"

# ------------------------------------------------------------ 5. 源码
Write-Host "`n==> 5/6 RustDesk 源码" -ForegroundColor Cyan
$srcOut = Join-Path $RepoHome "rustdesk"
if (-not (Test-Path (Join-Path $srcOut "src"))) {
  Say "复制源码 (robocopy) ..."
  New-Item -ItemType Directory -Path $RepoHome -Force | Out-Null
  # 用 robocopy 而不是 Copy-Item:
  #   - Copy-Item 碰到 Windows 保留设备名 (NUL) 会直接报错中断整个安装
  #   - robocopy 对几千个文件快一个数量级
  $rc = Start-Process -FilePath "$env:SystemRoot\System32\robocopy.exe" `
        -ArgumentList "`"$BundleRoot\source\rustdesk`" `"$srcOut`" /E /NFL /NDL /NJH /NJS /NP /XF NUL" `
        -Wait -PassThru -NoNewWindow
  if ($rc.ExitCode -ge 8) { Die "复制源码失败 (robocopy exit=$($rc.ExitCode))" }
  Say "源码文件数 $((Get-ChildItem $srcOut -Recurse -File | Measure-Object).Count)"
}

# 离线解析 Dart 依赖 —— 必须显式做。
# `flutter build` 会隐式跑 pub get, 默认去 pub.dev 解析版本, 内网机上必挂。
# --offline 让它只用包里的 pub-cache, 顺带把 .dart_tool/package_config.json 生成好,
# 之后的 build 就不会再碰网络。
if (Test-Path (Join-Path $srcOut "flutter\pubspec.yaml")) {
  Say "离线解析 Dart 依赖 (pub get --offline) ..."
  if (Test-Path $pubCacheLocal) { $env:PUB_CACHE = $pubCacheLocal }
  Push-Location (Join-Path $srcOut "flutter")
  try {
    & "$FlutterHome\bin\flutter.bat" pub get --offline 2>&1 | Select-Object -Last 4 |
      ForEach-Object { Say "  $_" }
    if ($LASTEXITCODE -ne 0) { Die "pub get --offline 失败 —— 包里的 pub-cache 不全, 补齐 toolchain\pub-cache 再装" }
  } finally { Pop-Location }
  if (-not (Test-Path (Join-Path $srcOut "flutter\.dart_tool\package_config.json"))) {
    Die "pub get 没生成 package_config.json"
  }
}
# cargo 离线: 用 vendor 目录替换 crates.io
# 源替换配置由 cargo vendor 生成, 包里带了一份(带 __VENDOR_DIR__ 占位符)。
# 注意: 不只是 crates-io 一段 —— [patch.crates-io] 的 4 个 git fork 和另外
# 三十多个 git 依赖都在里面, 少了这些 stanza 内网机上 cargo 会去 GitHub 拉。
$cargoCfgDir = Join-Path $srcOut ".cargo"
New-Item -ItemType Directory -Path $cargoCfgDir -Force | Out-Null
$cargoCfgFile = Join-Path $cargoCfgDir "config.toml"
$vendorCfg = "$BundleRoot\toolchain\cargo-vendor-config.toml"
$vendorDirPosix = $vendorDir -replace '\\', '/'
if (Test-Path $vendorCfg) {
  $cfgText = (Get-Content $vendorCfg -Raw).Replace('__VENDOR_DIR__', $vendorDirPosix)
  # 不用 Set-Content -Encoding UTF8: Windows PowerShell 5.1 会写 BOM, cargo 读 config.toml 会报错
  [System.IO.File]::WriteAllText($cargoCfgFile, $cfgText, (New-Object System.Text.UTF8Encoding($false)))
  $stanzas = ($cfgText -split "`n" | Where-Object { $_ -match '^\[' }).Count
  Say "cargo 源替换已写好 (.cargo/config.toml, $stanzas 段)"
} else {
  Warn "包里没有 cargo-vendor-config.toml, 只写 crates-io 一段 —— git 依赖离线时会失败"
  @"
[source.crates-io]
replace-with = "vendored-sources"

[source.vendored-sources]
directory = "$vendorDirPosix"
"@ | Set-Content $cargoCfgFile -Encoding UTF8
}

# ------------------------------------------------------------ 6. 环境变量
Write-Host "`n==> 6/6 环境变量" -ForegroundColor Cyan
$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
# 我们自己的路径**插到最前面**:
#   - RustHome 必须压过系统里可能存在的 rustup shim (否则 cargo 会调到别的 rustc)
#   - cargo-bins 放前面, cargo-expand / flutter_rust_bridge_codegen 才能当子命令用
$ours = @(
  "$RustHome\bin",
  "$FlutterHome\bin",
  "$BundleRoot\toolchain\cargo-bins",
  "$env:USERPROFILE\.cargo\bin"
)
$parts = @()
foreach ($p in $ours) { $parts += $p }
if ($userPath) {
  foreach ($p in ($userPath -split ';')) {
    $t = $p.Trim()
    if ($t -and ($ours -notcontains $t)) { $parts += $t }
  }
}
$newPath = ($parts -join ';')
[Environment]::SetEnvironmentVariable("Path", $newPath, "User")
[Environment]::SetEnvironmentVariable("CARGO_HOME", "$env:USERPROFILE\.cargo", "User")
[Environment]::SetEnvironmentVariable("VCPKG_ROOT", $VcpkgHome, "User")
[Environment]::SetEnvironmentVariable("VCPKG_INSTALLED_ROOT", $instDst, "User")
[Environment]::SetEnvironmentVariable("LIBCLANG_PATH", "$BundleRoot\toolchain\llvm\bin", "User")
if (Test-Path $pubCacheLocal) { [Environment]::SetEnvironmentVariable("PUB_CACHE", $pubCacheLocal, "User") }
if (Test-Path "$cmakeHome\bin\cmake.exe") { [Environment]::SetEnvironmentVariable("FLUTTER_CMAKE_PATH", "$cmakeHome\bin\cmake.exe", "User") }
$env:PATH = "$newPath;$env:PATH"
Say "PATH / CARGO_HOME / VCPKG_ROOT / VCPKG_INSTALLED_ROOT / LIBCLANG_PATH 已写入用户环境变量"

# ------------------------------------------------------------ 自检
Write-Host "`n==> 自检" -ForegroundColor Cyan
$fail = 0
function Check([string]$label, [bool]$ok, [string]$detail = "") {
  if ($ok) { Say "$label OK" } else { Warn "$label 失败 $detail"; $script:fail++ }
}
Check "rustc"        ((Test-Path $rustcExe))
$rustcVer = ""
try { $rustcVer = (& rustc --version 2>&1 | Out-String).Trim() } catch { $rustcVer = "" }
Check "rustc 可执行"  ([bool]$rustcVer) $rustcVer
Check "cargo"        ((Test-Path "$RustHome\bin\cargo.exe"))
$cargoVer = ""
try { $cargoVer = (& cargo --version 2>&1 | Out-String).Trim() } catch { $cargoVer = "" }
Check "cargo 可执行"  ([bool]$cargoVer) $cargoVer
Check "rust 标准库"   ((Test-Path "$RustHome\lib\rustlib\x86_64-pc-windows-msvc"))
Check "libclang (bindgen 要)" ((Test-Path "$BundleRoot\toolchain\llvm\bin\libclang.dll"))
Check "flutter" ((Test-Path "$FlutterHome\bin\flutter.bat"))
Check "独立 cmake" ((Test-Path "$cmakeHome\bin\cmake.exe"))
Check "vcpkg 头文件 vpx"  ((Test-Path "$instDst\x64-windows-static\include\vpx\vpx_encoder.h"))
Check "vcpkg 头文件 libyuv" ((Test-Path "$instDst\x64-windows-static\include\libyuv.h"))
Check "vcpkg 头文件 aom"   ((Test-Path "$instDst\x64-windows-static\include\aom\aom.h"))
Check "vcpkg 头文件 opus"  ((Test-Path "$instDst\x64-windows-static\include\opus\opus.h"))
Check "cargo-expand" ((Test-Path "$BundleRoot\toolchain\cargo-bins\cargo-expand.exe"))
Check "flutter_rust_bridge_codegen" ((Test-Path "$BundleRoot\toolchain\cargo-bins\flutter_rust_bridge_codegen.exe"))
Check "源码" ((Test-Path "$srcOut\src\flutter_ffi.rs"))
Check "bridge 代码" ((Test-Path "$srcOut\src\bridge_generated.rs"))

Write-Host ""
if ($fail -gt 0) {
  Write-Host "  有 $fail 项没过, 请看上面的提示" -ForegroundColor Yellow
  exit 1
}
Write-Host "==========================================================" -ForegroundColor Green
Write-Host " 离线环境安装完成" -ForegroundColor Green
Write-Host "==========================================================" -ForegroundColor Green
Write-Host " 源码目录 : $srcOut"
Write-Host " 工具链   : $RustHome"
Write-Host " Flutter  : $FlutterHome"
Write-Host " 依赖     : $instDst"
Write-Host ""
Write-Host " 下一步: 把 agent 挂到面板上"
Write-Host "   python `"$BundleRoot\agent\agent.py`" --panel http://<服务器>:21120 --token <agent_token>"
Write-Host "=========================================================="
