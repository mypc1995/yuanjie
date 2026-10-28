<#
.SYNOPSIS
    远界 部署包组装脚本 —— 把仓库里的 .deploy/ 组装成可以直接交付的部署包。

.DESCRIPTION
    面板和构建脚本在仓库里各只有一份 (.deploy/panel/ 与 .deploy/builder/),
    这个脚本负责把它们复制进部署包, 顺带把客户端源码树打成 tar.gz 一起带上。

    为什么要有这个脚本:
      以前部署包里的 panel/ 和 builder/ 是手工维护的第二份拷贝,
      改了一处忘了同步, 服务器上跑的面板就比仓库旧 —— 品牌、安装名、
      免责声明这些改动全都发不出去。这个脚本把第二份拷贝变成生成物。

.PARAMETER Repo
    仓库根目录, 默认按脚本自身位置往上两级。

.PARAMETER OutRoot
    部署包输出根目录, 默认 D:\Software\rustdesk-deployment。

.PARAMETER NoDisclaimer
    把免责声明从产物里去掉 —— 面板登录页的声明块和对应的样式。
    仓库里的源文件不动, 所以 GitHub 上的 README 仍然带免责声明,
    以后想加回来重打一次就行。

.EXAMPLE
    pwsh -File .deploy/tools/make-package.ps1

.EXAMPLE
    pwsh -File .deploy/tools/make-package.ps1 -NoDisclaimer -OutRoot D:\Software\rustdesk-deployment-nodisclaimer
#>
[CmdletBinding()]
param(
    [string] $Repo,
    [string] $OutRoot = 'D:\Software\rustdesk-deployment',
    [switch] $SkipSource,
    [switch] $NoDisclaimer
)

$ErrorActionPreference = 'Stop'

if (-not $Repo) {
    # <repo>/.deploy/tools/make-package.ps1 -> <repo>
    $Repo = Split-Path (Split-Path (Split-Path $PSCommandPath -Parent) -Parent) -Parent
}
$Repo = (Resolve-Path $Repo).Path
$Deploy = Join-Path $Repo '.deploy'
$ServerOut = Join-Path $OutRoot '01-server-kylin'

Write-Host "仓库    : $Repo" -ForegroundColor Cyan
Write-Host "输出    : $ServerOut" -ForegroundColor Cyan

function Copy-Tree($src, $dst) {
    if (-not (Test-Path $src)) { throw "源目录不存在: $src" }
    if (Test-Path $dst) { [System.IO.Directory]::Delete($dst, $true) }
    [System.IO.Directory]::CreateDirectory($dst) | Out-Null
    # robocopy 比 Copy-Item 稳, 大目录用它
    & robocopy.exe $src $dst /E /XD __pycache__ '.git' /XF '*.pyc' /NFL /NDL /NJH /NJS /NP | Out-Null
    if ($LASTEXITCODE -ge 8) { throw "robocopy 失败 ($LASTEXITCODE): $src" }
}

# ---------------------------------------------------------------- 1. 面板
Write-Host "`n==> 1/4 复制面板" -ForegroundColor Yellow
Copy-Tree (Join-Path $Deploy 'panel') (Join-Path $ServerOut 'panel')
Write-Host "  panel/  <- .deploy/panel/"

if ($NoDisclaimer) {
    # 只改产物, 仓库里的源文件不动
    $login = Join-Path $ServerOut 'panel\templates\login.html'
    $css = Join-Path $ServerOut 'panel\static\style.css'
    if (Test-Path $login) {
        $t = [System.IO.File]::ReadAllText($login)
        # 去掉整个 <div class="disclaimer">…</div> 块
        $t = [regex]::Replace($t, '(?s)\r?\n\s*<div class="disclaimer">.*?</div>\r?\n', "`n")
        [System.IO.File]::WriteAllText($login, $t, (New-Object System.Text.UTF8Encoding($false)))
        Write-Host "  登录页的免责声明已去掉" -ForegroundColor Yellow
    }
    if (Test-Path $css) {
        $t = [System.IO.File]::ReadAllText($css)
        # 去掉 .disclaimer 和 .disclaimer strong 这两条规则
        $t = [regex]::Replace($t, '(?s)\r?\n/\* 免责声明.*?\r?\n\.disclaimer\s*\{.*?\}\r?\n', "`n")
        $t = [regex]::Replace($t, '(?s)\r?\n\.disclaimer strong\s*\{[^}]*\}', '')
        [System.IO.File]::WriteAllText($css, $t, (New-Object System.Text.UTF8Encoding($false)))
        Write-Host "  免责声明样式已去掉" -ForegroundColor Yellow
    }
}

# ---------------------------------------------------------------- 2. 构建脚本
Write-Host "`n==> 2/4 复制构建脚本" -ForegroundColor Yellow
Copy-Tree (Join-Path $Deploy 'builder') (Join-Path $ServerOut 'builder')
Write-Host "  builder/ <- .deploy/builder/"

# ---------------------------------------------------------------- 3. 服务端二进制与安装脚本
Write-Host "`n==> 3/4 复制服务端二进制与安装脚本" -ForegroundColor Yellow
Copy-Tree (Join-Path $Deploy 'package\server') (Join-Path $ServerOut 'server')
foreach ($f in @('install.sh', 'install-build-env.sh', 'README.md')) {
    $src = Join-Path $Deploy "package\$f"
    if (Test-Path $src) {
        [System.IO.File]::Copy($src, (Join-Path $ServerOut $f), $true)
        Write-Host "  $f"
    }
}

# ---------------------------------------------------------------- 4. 客户端源码树
Write-Host "`n==> 4/4 打客户端源码树" -ForegroundColor Yellow
if ($SkipSource) {
    Write-Host "  跳过 (-SkipSource)"
} else {
    $srcDir = Join-Path $ServerOut 'src'
    [System.IO.Directory]::CreateDirectory($srcDir) | Out-Null
    $tar = Join-Path $srcDir 'rustdesk-src.tar.gz'
    if (Test-Path $tar) { [System.IO.File]::Delete($tar) }
    # 排除构建产物、生成物和部署工具 —— 服务器上只要能编出客户端的那部分源码
    $excludes = @(
        './.deploy', './.git', './target', './flutter/.dart_tool', './flutter/build',
        './flutter/linux/flutter/ephemeral', './flutter/windows/flutter/ephemeral',
        './.vcpkg-assets', './installed', './.dl', './libs/hbb_common/.git'
    )
    $argList = @('-czf', $tar)
    foreach ($e in $excludes) { $argList += @('--exclude', $e) }
    $argList += @('-C', $Repo, '.')
    & tar.exe @argList
    if ($LASTEXITCODE -ne 0) { throw "打源码树失败 ($LASTEXITCODE)" }
    $mb = (Get-Item $tar).Length / 1MB
    Write-Host ("  src/rustdesk-src.tar.gz  {0:N1} MB" -f $mb)
}

# ---------------------------------------------------------------- 校验
Write-Host "`n==> 校验" -ForegroundColor Yellow
$must = @(
    'install.sh', 'install-build-env.sh', 'README.md',
    'panel\panel.py', 'panel\templates\branding.html', 'panel\templates\login.html',
    'panel\static\style.css', 'panel\rustdesk-panel.service',
    'builder\agent.py', 'builder\build_job.py',
    'server\amd64\hbbs', 'server\amd64\hbbr'
)
$missing = @()
foreach ($m in $must) {
    if (-not (Test-Path (Join-Path $ServerOut $m))) { $missing += $m }
}
if ($missing.Count) {
    Write-Host "缺文件:" -ForegroundColor Red
    $missing | ForEach-Object { Write-Host "  $_" -ForegroundColor Red }
    exit 1
}
Write-Host "  必备文件齐全"

# 面板必须带 install_name 支持, 否则「品牌定制」页填的安装名不会生效
$hasInstall = Select-String -Path (Join-Path $ServerOut 'panel\panel.py') -Pattern 'install_name' -Quiet
$hasCss = Select-String -Path (Join-Path $ServerOut 'panel\static\style.css') -Pattern '\.disclaimer' -Quiet
$hasText = Select-String -Path (Join-Path $ServerOut 'panel\templates\login.html') -Pattern 'class="disclaimer"' -Quiet
if (-not $hasInstall) { Write-Host "  ! 面板缺 install_name 支持" -ForegroundColor Yellow }
if ($NoDisclaimer) {
    # 这次就是要没有免责声明, 所以"没有"才是对的
    if ($hasCss -or $hasText) {
        Write-Host "  ! 声明没删干净 (样式或登录页还在)" -ForegroundColor Red
    } else {
        Write-Host "  免责声明已去掉 (登录页和样式都没有了)"
    }
} else {
    if (-not $hasCss) { Write-Host "  ! 面板缺免责声明样式" -ForegroundColor Yellow }
    if (-not $hasText) { Write-Host "  ! 登录页缺免责声明" -ForegroundColor Yellow }
    if ($hasInstall -and $hasCss -and $hasText) { Write-Host "  面板特性齐全 (install_name + 免责声明)" }
}

# 面板代码里不能出现真实内网地址
$hits = Select-String -Path (Join-Path $ServerOut 'panel\panel.py'), (Join-Path $ServerOut 'panel\templates\*.html') `
    -Pattern '192\.168\.\d+\.\d+' -ErrorAction SilentlyContinue
if ($hits) {
    Write-Host "  ! 面板里混进了内网地址:" -ForegroundColor Red
    $hits | ForEach-Object { Write-Host "    $($_.Filename):$($_.LineNumber)" -ForegroundColor Red }
} else {
    Write-Host "  面板里没有内网地址"
}

# 上面几项任何一项不对, 都说明产物里的面板不对(复制那一步没生效, 或者
# 免责声明没删干净)。这种情况不能只是提醒 —— 旧面板发出去, 品牌、安装名、
# 免责声明这些改动全丢失。直接失败, 别让人以为组包成功了。
$panelBad = $false
if (-not $hasInstall) { $panelBad = $true }
if ($NoDisclaimer) {
    if ($hasCss -or $hasText) { $panelBad = $true }
} else {
    if (-not $hasCss -or -not $hasText) { $panelBad = $true }
}
if ($hits) { $panelBad = $true }
if ($panelBad) {
    Write-Host ""
    Write-Host "  产物里的面板不对, 组包没成功。" -ForegroundColor Red
    Write-Host "  常见原因: 同一时间还有别的任务在读写这个目录(比如正在打整包 tar)," -ForegroundColor Red
    Write-Host "  Delete + robocopy 撞上了, 部分文件没写进去。等那个任务结束再重跑。" -ForegroundColor Red
    exit 1
}

Write-Host "`n完成: $ServerOut" -ForegroundColor Green
