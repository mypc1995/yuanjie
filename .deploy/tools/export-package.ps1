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

.PARAMETER NoDisclaimer
    导出包里不带免责声明(面板登录页的声明块和样式)。
    一般是给不想在登录页显示这段文字的环境用的。
#>
[CmdletBinding()]
param(
    [string] $Out = 'D:\远界部署包',
    [string] $Source = 'D:\Software\rustdesk-deployment',
    [switch] $NoSanitize,
    [switch] $NoDisclaimer
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

# ---------------------------------------------------------------- 去掉免责声明
if ($NoDisclaimer) {
    Write-Host "`n==> 去掉免责声明" -ForegroundColor Yellow
    $login = Join-Path $Out '01-server-kylin\panel\templates\login.html'
    $css = Join-Path $Out '01-server-kylin\panel\static\style.css'
    if (Test-Path $login) {
        $t = [System.IO.File]::ReadAllText($login)
        $t = [regex]::Replace($t, '(?s)\r?\n\s*<div class="disclaimer">.*?</div>\r?\n', "`n")
        [System.IO.File]::WriteAllText($login, $t, (New-Object System.Text.UTF8Encoding($false)))
        Write-Host "  登录页的免责声明已去掉"
    }
    if (Test-Path $css) {
        $t = [System.IO.File]::ReadAllText($css)
        $t = [regex]::Replace($t, '(?s)\r?\n/\* 免责声明.*?\r?\n\.disclaimer\s*\{.*?\}\r?\n', "`n")
        $t = [regex]::Replace($t, '(?s)\r?\n\.disclaimer strong\s*\{[^}]*\}', '')
        [System.IO.File]::WriteAllText($css, $t, (New-Object System.Text.UTF8Encoding($false)))
        Write-Host "  免责声明样式已去掉"
    }
    # 顶层 README 里如果有声明段, 一起去掉
    $top = Join-Path $Out 'README.md'
    if (Test-Path $top) {
        $t = [System.IO.File]::ReadAllText($top)
        if ($t -match '免责|不纵容') {
            $t = [regex]::Replace($t, '(?s)\r?\n>?\s*#{1,3}\s*[^\r\n]*免责[^\r\n]*\r?\n.*?(?=\r?\n\r?\n|\z)', "`n")
            $t = [regex]::Replace($t, '(?s)\r?\n>[^\r\n]*(不纵容|自行承担)[^\r\n]*(\r?\n>[^\r\n]*)*', "`n")
            [System.IO.File]::WriteAllText($top, $t, (New-Object System.Text.UTF8Encoding($false)))
            Write-Host "  顶层 README 的声明段已去掉"
        }
    }
}

# ---------------------------------------------------------------- 脱敏
# 要清掉哪些值, 先从源包里抓出来, 后面拿它们做精确复查。
# 用具体值查, 不用 192.168.x.x 这种宽泛的写法 —— 离线包里
# 上游第三方 crate 的 README 和测试数据里有大量 RFC1918 的示例地址,
# 那些是别人的文档, 而且 vendor 目录碰不得(动了会破坏 cargo 校验)。
$orig = New-Object System.Collections.Generic.List[string]
if (-not $NoSanitize) {
    Write-Host "`n==> 脱敏 (环境相关的值)" -ForegroundColor Yellow

    $cjSrc = Join-Path $Source '03-client-windows\branding\client-config.json'
    if (Test-Path $cjSrc) {
        try {
            $o = Get-Content $cjSrc -Raw | ConvertFrom-Json
            foreach ($k in @('host', 'relay', 'api', 'key')) {
                $v = $o.$k
                if ($v) { $orig.Add([string]$v) }
            }
        } catch { Write-Host "  读原始 client-config.json 失败: $_" -ForegroundColor Yellow }
    }
    $b64Src = Join-Path $Source '03-client-windows\branding\server-config.b64'
    if (Test-Path $b64Src) {
        $v = (Get-Content $b64Src -Raw).Trim()
        if ($v) { $orig.Add($v) }
    }
    $cfgSrc = Join-Path $Source '02-build-agent-windows\deploy-config.txt'
    if (Test-Path $cfgSrc) {
        foreach ($line in (Get-Content $cfgSrc)) {
            if ($line -match '^(panel|token)=(.*)$' -and $Matches[2]) { $orig.Add($Matches[2]) }
        }
    }
    Write-Host "  捕获到 $($orig.Count) 个需要清掉的值"

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
        $o = @{ host = ''; relay = ''; api = ''; key = '' } | ConvertTo-Json
        [System.IO.File]::WriteAllText($cj, $o, (New-Object System.Text.UTF8Encoding($false)))
        Write-Host "  branding/client-config.json: 已清空"
    }
    $b64 = Join-Path $bd 'server-config.b64'
    if (Test-Path $b64) {
        [System.IO.File]::WriteAllText($b64, '', (New-Object System.Text.UTF8Encoding($false)))
        Write-Host "  branding/server-config.b64: 已清空"
    }

    # 4b. 品牌包的说明文件里也印着地址和 Key
    $brd = Join-Path $bd 'README.txt'
    if (Test-Path $brd) {
        $t = [System.IO.File]::ReadAllText($brd)
        $t = $t -replace '192\.168\.\d+\.\d+', '<服务器IP>'
        $t = $t -replace '(?m)^(Key\s*:\s*).*$', '$1'
        [System.IO.File]::WriteAllText($brd, $t, (New-Object System.Text.UTF8Encoding($false)))
        Write-Host "  branding/README.txt: 地址和 Key 已清空"
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
# 用脱敏时抓到的原始值做精确复查, 不用 192.168.x.x 这种宽泛的写法 ——
# 离线包里上游第三方 crate 的 README 和测试数据里有大量 RFC1918 示例地址,
# 那是别人的文档, 跟源服务器无关, 而且 vendor 目录不能碰(动了 cargo 校验会挂)。
$patterns = $orig.ToArray()
if ($patterns.Count -eq 0) {
    Write-Host "  没抓到原始值, 跳过精确复查" -ForegroundColor Yellow
} else {
    $textExt = @('.txt', '.md', '.json', '.py', '.ps1', '.cmd', '.bat', '.sh', '.conf',
        '.cfg', '.toml', '.yml', '.yaml', '.env', '.b64', '.ini', '.properties', '.xml', '.html')
    $files = Get-ChildItem $Out -Recurse -File -Force |
        Where-Object { $textExt -contains $_.Extension.ToLower() }
    Write-Host "  拿 $($patterns.Count) 个原始值扫 $($files.Count) 个文本文件"
    $hits = Select-String -Path $files.FullName -Pattern $patterns -List -ErrorAction SilentlyContinue
    if ($hits) {
        Write-Host "  !! 仍有命中:" -ForegroundColor Red
        $hits | ForEach-Object { Write-Host "    $($_.Path.Substring($Out.Length + 1))" -ForegroundColor Red }
    } else {
        Write-Host "  干净" -ForegroundColor Green
    }
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
