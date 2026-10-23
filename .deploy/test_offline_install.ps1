# 在本机用沙箱目录当「假离线机」跑一遍 install-offline.ps1, 验证安装逻辑。
# 跑之前快照用户环境变量, 跑完还原 —— 脚本会写 PATH / VCPKG_ROOT 等, 不能污染当前环境。
$ErrorActionPreference = 'Continue'
$bundle = 'D:\Software\rustdesk-offline-bundle'
$sandbox = 'D:\OfflineTest'

$keys = @('Path','CARGO_HOME','RUSTUP_HOME','VCPKG_ROOT','VCPKG_INSTALLED_ROOT','LIBCLANG_PATH','PUB_CACHE','FLUTTER_CMAKE_PATH')
$snap = @{}
foreach ($k in $keys) { $snap[$k] = [Environment]::GetEnvironmentVariable($k, 'User') }

Write-Host "=== 跑之前的环境 ===" -ForegroundColor Cyan
foreach ($k in $keys) { "  $k = $($snap[$k])" }

Write-Host "`n=== 沙箱安装 ===" -ForegroundColor Cyan
& pwsh -NoProfile -ExecutionPolicy Bypass -File "$bundle\install-offline.ps1" `
    -BundleRoot $bundle `
    -RustHome   "$sandbox\rust" `
    -FlutterHome "$sandbox\flutter" `
    -VcpkgHome  "$sandbox\vcpkg" `
    -RepoHome   "$sandbox\src" 2>&1 | Select-Object -Last 80
$rc = $LASTEXITCODE
Write-Host "install EXIT: $rc"

Write-Host "`n=== 还原环境变量 ===" -ForegroundColor Cyan
foreach ($k in $keys) {
  [Environment]::SetEnvironmentVariable($k, $snap[$k], 'User')
  "  还原 $k"
}

Write-Host "`n=== 沙箱产物核对 ===" -ForegroundColor Cyan
$bad = 0
foreach ($p in @(
  "$sandbox\rust\bin\rustc.exe",
  "$sandbox\rust\bin\cargo.exe",
  "$sandbox\rust\lib\rustlib\x86_64-pc-windows-msvc",
  "$sandbox\flutter\bin\flutter.bat",
  "$sandbox\vcpkg\installed\x64-windows-static\include\vpx\vpx_encoder.h",
  "$sandbox\src\rustdesk\src\bridge_generated.rs",
  "$sandbox\src\rustdesk\.cargo\config.toml",
  "$sandbox\src\rustdesk\src\server_preset.txt"
)) {
  $ok = Test-Path $p
  if (-not $ok) { $bad++ }
  '{0,-6} {1}' -f $(if ($ok) { 'OK' } else { '缺' }), $p
}

Write-Host "`n=== Flutter 补丁状态 ===" -ForegroundColor Cyan
$stamp = "$sandbox\flutter\bin\cache\flutter_tools.stamp"
# 补丁脚本会**删掉** stamp, 让 flutter 用打过补丁的 .dart 重建 snapshot。
# 所以这里期望它不存在, 存在反而说明补丁没生效。
if (Test-Path $stamp) {
  Write-Host "  ! stamp 还在 —— 补丁脚本应该删掉它才会重建带补丁的 snapshot"
  $bad++
} else {
  Write-Host "  OK stamp 已清 -> 下次 flutter 会重建带补丁的 snapshot"
}
$plug = "$sandbox\flutter\packages\flutter_tools\lib\src\flutter_plugins.dart"
if ((Test-Path $plug) -and ((Get-Content $plug -Raw) -match 'mklink')) {
  Write-Host "  OK flutter_plugins.dart 已带 symlink->junction 回退"
} else {
  Write-Host "  ! flutter_plugins.dart 里没有 mklink 回退 —— 补丁没生效"
  $bad++
}
$vsd = "$sandbox\flutter\packages\flutter_tools\lib\src\windows\visual_studio.dart"
if ((Test-Path $vsd) -and ((Get-Content $vsd -Raw) -match 'FLUTTER_CMAKE_PATH')) {
  Write-Host "  OK visual_studio.dart 已带 VS 检测放宽 + FLUTTER_CMAKE_PATH 回退"
} else {
  Write-Host "  ! visual_studio.dart 补丁没生效"
  $bad++
}

Write-Host "`n=== 沙箱里 rustc / cargo 能不能真跑 ===" -ForegroundColor Cyan
$env:PATH = "$sandbox\rust\bin;$env:PATH"
& "$sandbox\rust\bin\rustc.exe" --version 2>&1 | ForEach-Object { "  rustc: $_" }
& "$sandbox\rust\bin\cargo.exe" --version 2>&1 | ForEach-Object { "  cargo: $_" }

Write-Host "`n=== MSI 是否注册成功 (按用户装) ===" -ForegroundColor Cyan
Get-ItemProperty 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\*','HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall\*' -ErrorAction SilentlyContinue |
  Where-Object { $_.DisplayName -like 'Rust 1.99*' } |
  Select-Object DisplayName, DisplayVersion, InstallLocation | Format-List

Write-Host "`n=== Flutter 补丁是否生效 (应无 mklink 报错) ===" -ForegroundColor Cyan
Write-Host "  (上面已单独核对 flutter_plugins.dart / visual_studio.dart / stamp)"
Write-Host "`n=== cargo 源替换配置 ==="
Get-Content "$sandbox\src\rustdesk\.cargo\config.toml" -ErrorAction SilentlyContinue

if ($bad -gt 0) { Write-Host "`n有 $bad 个产物缺失" -ForegroundColor Red; exit 1 }
exit $rc
