<#
.SYNOPSIS
    导出「远界」部署包到独立目录, 供搬到其他环境部署。

.DESCRIPTION
    把三样东西收进一个文件夹:
      01-server-kylin/          服务端 + 面板 + 构建脚本 + 客户端源码树
      02-build-agent-windows/   Windows 构建机离线包(能断网编 Windows 客户端)
      03-client-windows/        已打好品牌的 Windows 客户端

    导出时会做一次脱敏: 环境相关的值(面板地址、agent token、服务器
    地址与 Key)在源服务器上是写死的, 换环境后这些值全都得改, 带着走
    只会误导人, 所以清成占位符。装的时候按提示填。

.PARAMETER Out
    导出目录, 默认 D:\远界部署包。

.PARAMETER Source
    部署包源目录, 默认 D:\Software\rustdesk-deployment。

.PARAMETER NoSanitize
    不做脱敏(要把当前环境的值原样带过去时才用)。
#>
[CmdletBinding()]
param(
    [string] $Out = 'D:\远界部署包',
    [string] $Source = 'D:\Software\rustdesk-deployment',
    [switch] $NoSanitize
)

$ErrorActionPreference = 'Stop'
$Source = (Resolve-Path $Source).Path
# 仓库根 = 脚本所在 .deploy/tools/ 往上两级
$Repo = Split-Path (Split-Path (Split-Path $PSCommandPath -Parent) -Parent) -Parent

Write-Host "源目录: $Source" -ForegroundColor Cyan
Write-Host "导出到: $Out" -ForegroundColor Cyan

if (Test-Path $Out) {
    $answer = Read-Host "目录已存在, 要覆盖吗? (y/N)"
    if ($answer -ne 'y') { exit 1 }
    [System.IO.Directory]::Delete($Out, $true)
}
[System.IO.Directory]::CreateDirectory($Out) | Out-Null

# ---------------------------------------------------------------- 复制
$parts = @('01-server-kylin', '02-build-agent-windows', '03-client-windows')
foreach ($p in $parts) {
    $src = Join-Path $Source $p
    if (-not (Test-Path $src)) { throw "缺 $p" }
    Write-Host "`n==> 复制 $p" -ForegroundColor Yellow
    & robocopy.exe $src (Join-Path $Out $p) /E /COPY:DAT /DCOPY:DAT /R:1 /W:1 `
        /XD '__pycache__' /XF '*.pyc' 'NUL' /NFL /NDL /NJH /NJS /NP | Out-Null
    $rc = $LASTEXITCODE
    if ($rc -ge 8) { throw "robocopy 失败 ($rc): $p" }
    $n = (Get-ChildItem (Join-Path $Out $p) -Recurse -File -Force | Measure-Object).Count
    Write-Host "  完成, $n 个文件"
}

# ---------------------------------------------------------------- 脱敏
if (-not $NoSanitize) {
    Write-Host "`n==> 脱敏 (环境相关的值)" -ForegroundColor Yellow

    # 1. 构建机配置: 面板地址和 token 换环境后必然变
    $cfg = Join-Path $Out '02-build-agent-windows\deploy-config.txt'
    if (Test-Path $cfg) {
        $t = [System.IO.File]::ReadAllText($cfg)
        $t = $t -replace '(?m)^panel=.*$', 'panel=http://<服务器IP>:21120'
        $t = $t -replace '(?m)^token=.*$', 'token='
        $t = $t -replace '192\.168\.\d+\.\d+', '<服务器IP>'
        [System.IO.File]::WriteAllText($cfg, $t, (New-Object System.Text.UTF8Encoding($true)))
        Write-Host "  deploy-config.txt: 面板地址/token 已清空"
    }

    # 2. agent.py 的用法示例里写死了源服务器的地址
    $agent = Join-Path $Out '02-build-agent-windows\rustdesk-offline-bundle\agent\agent.py'
    if (Test-Path $agent) {
        $t = [System.IO.File]::ReadAllText($agent)
        $t = $t -replace '192\.168\.\d+\.\d+', '<服务器IP>'
        [System.IO.File]::WriteAllText($agent, $t, (New-Object System.Text.UTF8Encoding($false)))
        Write-Host "  agent.py: 示例地址已替换"
    }

    # 3. 离线包里的 README 示例
    $rd = Join-Path $Out '02-build-agent-windows\rustdesk-offline-bundle\source\rustdesk\README.md'
    if (Test-Path $rd) {
        $t = [System.IO.File]::ReadAllText($rd)
        $t = $t -replace '192\.168\.\d+\.\d+', '<服务器IP>'
        [System.IO.File]::WriteAllText($rd, $t, (New-Object System.Text.UTF8Encoding($false)))
        Write-Host "  离线包 README: 示例地址已替换"
    }

    # 4. 品牌包里的服务器配置: 新环境的地址和 Key 都不同
    $bd = Join-Path $Out '03-client-windows\branding'
    $cj = Join-Path $bd 'client-config.json'
    if (Test-Path $cj) {
        $o = @{
            host = ''; relay = ''; api = ''; key = ''
        } | ConvertTo-Json
        [System.IO.File]::WriteAllText($cj, $o, (New-Object System.Text.UTF8Encoding($false)))
        Write-Host "  branding/client-config.json: 已清空"
    }
    $b64 = Join-Path $bd 'server-config.b64'
    if (Test-Path $b64) {
        [System.IO.File]::WriteAllText($b64, '', (New-Object System.Text.UTF8Encoding($false)))
        Write-Host "  branding/server-config.b64: 已清空"
    }

    # 5. brand-bundle.zip 里也压了一份同样的配置, 重新打一遍(用清空后的)
    $bb = Join-Path $Out '03-client-windows\brand-bundle.zip'
    if (Test-Path $bb) {
        [System.IO.File]::Delete($bb)
        # 显式列文件, 不加 "./" 前缀 —— 跟面板导出的那个 zip 保持一致
        $names = Get-ChildItem $bd -File | Select-Object -ExpandProperty Name
        & tar.exe -a -c -f $bb -C $bd @names | Out-Null
        Write-Host "  brand-bundle.zip: 已按清空后的配置重打 ($($names.Count) 个文件)"
    }
}

# ---------------------------------------------------------------- 顶层说明
$readmeSrc = Join-Path $Repo '.deploy\package\EXPORT-README.md'
if (Test-Path $readmeSrc) {
    Copy-Item $readmeSrc (Join-Path $Out 'README.md') -Force
    Write-Host "  顶层 README.md"
}

# ---------------------------------------------------------------- 复查
Write-Host "`n==> 复查有没有漏掉的敏感值" -ForegroundColor Yellow
$patterns = '192\.168\.\d+\.\d+|0FQR0MPNYTa5rEsYHbKt|fMJgSUtmpgELPap'
# 只查文本类文件: 整包有 12 万多个文件, 逐个读二进制太慢
$textExt = @('.txt', '.md', '.json', '.py', '.ps1', '.cmd', '.bat', '.sh', '.conf',
    '.cfg', '.toml', '.yml', '.yaml', '.env', '.b64', '.ini', '.properties', '.xml', '.html')
$files = Get-ChildItem $Out -Recurse -File -Force |
    Where-Object { $textExt -contains $_.Extension.ToLower() }
Write-Host "  扫描 $($files.Count) 个文本文件"
$hits = Select-String -Path $files.FullName -Pattern $patterns -List -ErrorAction SilentlyContinue
if ($hits) {
    Write-Host "  !! 仍有命中:" -ForegroundColor Red
    $hits | ForEach-Object { Write-Host "    $($_.Path.Substring($Out.Length + 1))" -ForegroundColor Red }
} else {
    Write-Host "  干净" -ForegroundColor Green
}

# ---------------------------------------------------------------- 校验和
Write-Host "`n==> 生成 SHA256SUMS.txt" -ForegroundColor Yellow
$all = Get-ChildItem $Out -Recurse -File -Force |
    Where-Object { $_.Name -ne 'SHA256SUMS.txt' } | Sort-Object FullName
$lines = New-Object System.Collections.Generic.List[string]
$i = 0
foreach ($f in $all) {
    $rel = $f.FullName.Substring($Out.Length + 1) -replace '\\', '/'
    $lines.Add("$((Get-FileHash -LiteralPath $f.FullName -Algorithm SHA256).Hash.ToLower())  $rel")
    $i++
    if ($i % 20000 -eq 0) { Write-Host "  ...$i / $($all.Count)" }
}
[System.IO.File]::WriteAllLines((Join-Path $Out 'SHA256SUMS.txt'), $lines,
    (New-Object System.Text.UTF8Encoding($false)))
Write-Host "  $($lines.Count) 条"

$size = (Get-ChildItem $Out -Recurse -File -Force | Measure-Object Length -Sum).Sum
Write-Host ("`n导出完成: {0}  ({1:N1} GB, {2} 个文件)" -f $Out, ($size / 1GB),
    (Get-ChildItem $Out -Recurse -File -Force | Measure-Object).Count) -ForegroundColor Green
