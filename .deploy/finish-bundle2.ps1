$ErrorActionPreference = 'Continue'
$OutDir = 'D:\Software\rustdesk-offline-bundle'
$RepoRoot = 'D:\Software\RustDesk'
function Say([string]$m) { Write-Host "  $m" -ForegroundColor Green }

Write-Host "==> 1. 补 cargo-expand / flutter_rust_bridge_codegen" -ForegroundColor Cyan
$bins = Join-Path $OutDir "toolchain\cargo-bins"
New-Item -ItemType Directory -Path $bins -Force | Out-Null
foreach ($n in 'cargo-expand.exe', 'flutter_rust_bridge_codegen.exe') {
  $src = Join-Path "$env:USERPROFILE\.cargo\bin" $n
  if (Test-Path $src) { Copy-Item -LiteralPath $src -Destination (Join-Path $bins $n) -Force; Say "$n 已拷入" }
  else { Say "$n 找不到: $src" }
}
Get-ChildItem $bins | Select-Object Name, @{n='MB';e={[math]::Round($_.Length/1MB,1)}} | Format-Table -AutoSize

Write-Host "`n==> 2. 仓库里那个 NUL 文件 (Windows 重定向误建)" -ForegroundColor Cyan
Get-ChildItem $RepoRoot -Recurse -File -Filter 'NUL' -ErrorAction SilentlyContinue |
  Select-Object -First 5 FullName, Length | Format-Table -AutoSize

Write-Host "`n==> 3. SHA256 清单 (跳过算不出的文件)" -ForegroundColor Cyan
# 只排除**根目录**那一个 manifest.json。
# 按文件名排除会把包内其它同名文件 (插件示例的 web manifest.json, 有 80 多个)
# 一起跳过, 那些文件就拿不到校验和了。
$manifestPath = Join-Path $OutDir 'manifest.json'
$files = Get-ChildItem $OutDir -Recurse -File -ErrorAction SilentlyContinue | Where-Object { $_.FullName -ne $manifestPath }
Say "计算 $($files.Count) 个文件的哈希 ..."
$manifest = @()
$skipped = 0
$done = 0
foreach ($f in $files) {
  try {
    $h = (Get-FileHash -LiteralPath $f.FullName -Algorithm SHA256 -ErrorAction Stop).Hash.ToLower()
    $manifest += [PSCustomObject]@{
      path = $f.FullName.Substring($OutDir.Length + 1).Replace('\', '/')
      size = $f.Length
      sha256 = $h
    }
    $done++
  } catch {
    $skipped++
  }
  if ($done % 10000 -eq 0 -and $done -gt 0) { Say "  $done / $($files.Count)" }
}
$manifest | ConvertTo-Json -Depth 3 | Set-Content $manifestPath -Encoding UTF8
Say "清单完成: $($manifest.Count) 个文件 (跳过 $skipped 个算不出的)"

$total = ($files | Measure-Object -Property Length -Sum).Sum
Write-Host ""
Write-Host "==========================================================" -ForegroundColor Cyan
Write-Host " 离线包完成: $OutDir" -ForegroundColor Cyan
Write-Host " 文件数 : $($files.Count)" -ForegroundColor Cyan
Write-Host " 总体积 : $([math]::Round($total/1GB,2)) GB" -ForegroundColor Cyan
Write-Host "==========================================================" -ForegroundColor Cyan
