# 补齐离线包: 源码 + agent + 补丁 + 清单 (工具链部分已在包里, 不重复拷)
$ErrorActionPreference = 'Stop'
$OutDir = 'D:\Software\rustdesk-offline-bundle'
$RepoRoot = 'D:\Software\RustDesk'

function Say([string]$m) { Write-Host "  $m" -ForegroundColor Green }

Write-Host "`n==> 1. 拷贝源码" -ForegroundColor Cyan
$srcOut = Join-Path $OutDir "source\rustdesk"
if (Test-Path $srcOut) { [System.IO.Directory]::Delete($srcOut, $true) }
New-Item -ItemType Directory -Path $srcOut -Force | Out-Null

# 排除: 构建产物 / 依赖缓存 / 各种离线包目录 / 移动端平台工程
$excludeDirs = @(
  'target', 'vcpkg_installed', 'installed', '.vendor', '.dl', '.deploy', '.git',
  'build', '.dart_tool', 'android', 'ios',
  '.offline-bundle', '.offline-bundle-bad', 'OfflineTest'
)
Write-Host "  排除: $($excludeDirs -join ', ')"
robocopy $RepoRoot $srcOut /E /XD @excludeDirs /NFL /NDL /NJH /NJS /NP /XF '*.pdb' | Out-Null
$n = (Get-ChildItem $srcOut -Recurse -File | Measure-Object).Count
Say "文件数 $n"
$bad = Get-ChildItem $srcOut -Recurse -Directory -ErrorAction SilentlyContinue | Where-Object { $_.Name -like '.offline*' -or $_.Name -eq 'target' -or $_.Name -eq '.vendor' }
if ($bad) { Say "警告: 仍有被排除的目录: $($bad.Name -join ', ')" } else { Say "没有混入任何构建产物/离线包" }

Write-Host "`n==> 2. agent / 补丁 / 说明" -ForegroundColor Cyan
Copy-Item "$RepoRoot\.deploy\builder\agent.py"     (Join-Path $OutDir "agent\agent.py") -Force
Copy-Item "$RepoRoot\.deploy\builder\build_job.py" (Join-Path $OutDir "agent\build_job.py") -Force
Copy-Item "$RepoRoot\.deploy\package\patches\fix-flutter-toolchain.ps1" (Join-Path $OutDir "patches\fix-flutter-toolchain.ps1") -Force
Copy-Item "$RepoRoot\.deploy\install-offline.ps1" (Join-Path $OutDir "install-offline.ps1") -Force
Copy-Item "$RepoRoot\.deploy\package\windows-offline-README.md" (Join-Path $OutDir "README.md") -Force
Say "agent / patches / install-offline.ps1 / README.md 就位"

Write-Host "`n==> 3. 关键文件核对" -ForegroundColor Cyan
foreach ($p in @(
  "$srcOut\src\flutter_ffi.rs",
  "$srcOut\src\bridge_generated.rs",
  "$srcOut\src\bridge_generated.io.rs",
  "$srcOut\flutter\lib\generated_bridge.dart",
  "$srcOut\libs\hbb_common\src\config.rs",
  "$srcOut\libs\base\Cargo.toml",
  "$srcOut\res\vcpkg\libyuv\portfile.cmake",
  "$srcOut\Cargo.lock",
  "$OutDir\toolchain\vcpkg\installed\x64-windows-static\include\vpx\vpx_encoder.h",
  "$OutDir\toolchain\rust\rust-1.99.1-x86_64-pc-windows-msvc.tar.gz",
  "$OutDir\toolchain\flutter\flutter_windows_3.24.5-stable.zip",
  "$OutDir\toolchain\llvm\bin\libclang.dll",
  "$OutDir\toolchain\cmake\bin\cmake.exe",
  "$OutDir\toolchain\cargo-bins\cargo-expand.exe",
  "$OutDir\toolchain\cargo-bins\flutter_rust_bridge_codegen.exe"
)) {
  '{0,-6} {1}' -f $(if (Test-Path $p) { 'OK' } else { '缺' }), $p.Replace($OutDir, '')
}

Write-Host "`n==> 4. SHA256 清单" -ForegroundColor Cyan
$files = Get-ChildItem $OutDir -Recurse -File | Where-Object { $_.Name -ne 'manifest.json' }
Say "计算 $($files.Count) 个文件的哈希 (耗时较久) ..."
$manifest = $files | ForEach-Object {
  [PSCustomObject]@{
    path = $_.FullName.Substring($OutDir.Length + 1).Replace('\', '/')
    size = $_.Length
    sha256 = (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLower()
  }
}
$manifest | ConvertTo-Json -Depth 3 | Set-Content (Join-Path $OutDir 'manifest.json') -Encoding UTF8
$total = ($files | Measure-Object -Property size -Sum).Sum
Write-Host ""
Write-Host "==========================================================" -ForegroundColor Cyan
Write-Host " 离线包完成: $OutDir" -ForegroundColor Cyan
Write-Host " 文件数 : $($files.Count)" -ForegroundColor Cyan
Write-Host " 总体积 : $([math]::Round($total/1GB,2)) GB" -ForegroundColor Cyan
Write-Host "==========================================================" -ForegroundColor Cyan
