<#
.SYNOPSIS
    远界 部署包归档 + 校验和。

.DESCRIPTION
    把部署包根目录打成 rustdesk-deployment.tar, 再生成 SHA256SUMS.txt。
    SHA256SUMS 的格式跟 sha256sum 一致 ( "<哈希>  <相对路径>" ),
    在 Linux 上可以直接 sha256sum -c 校验。

    归档不含 tar 自身和 SHA256SUMS.txt —— 这两个是产物, 不是内容。

.PARAMETER OutRoot
    部署包根目录, 默认 D:\Software\rustdesk-deployment。

.PARAMETER SkipArchive
    只重算校验和, 不重新打 tar。
#>
[CmdletBinding()]
param(
    [string] $OutRoot = 'D:\Software\rustdesk-deployment',
    [switch] $SkipArchive
)

$ErrorActionPreference = 'Stop'
$OutRoot = (Resolve-Path $OutRoot).Path
# 用 gzip 而不是裸 tar: 这套东西 5.6GB 里大半是可压缩的文本,
# 打成 .tar.gz 大约 2GB, 搬去别的环境方便得多(裸 tar 7.8GB)。
$tar = Join-Path $OutRoot 'rustdesk-deployment.tar.gz'
$sums = Join-Path $OutRoot 'SHA256SUMS.txt'

if (-not $SkipArchive) {
    Write-Host "==> 打包 $tar" -ForegroundColor Cyan
    if (Test-Path $tar) { [System.IO.File]::Delete($tar) }
    # 排除所有归档文件 —— 之前只排除了 tar 自己, 结果目录里那个
    # rustdesk-deployment.tar.xz 被打进来了, 包凭空大了 2GB 还夹着一份旧副本。
    & tar.exe -czf $tar --exclude='./rustdesk-deployment.tar' --exclude='./rustdesk-deployment.tar.gz' `
        --exclude='./rustdesk-deployment.tar.xz' --exclude='./SHA256SUMS.txt' -C $OutRoot .
    if ($LASTEXITCODE -ne 0) { throw "打包失败 ($LASTEXITCODE)" }
    Write-Host ("  {0:N1} MB" -f ((Get-Item $tar).Length / 1MB))
}

Write-Host "==> 算 SHA256" -ForegroundColor Cyan
# 只排除「输出根目录下的归档产物」和校验和文件本身, 不能按扩展名一刀切 ——
# 离线包里也有 .tar.gz 之类的测试样本, 那些也要进校验和。
# 新生成的 .tar.gz 要算进去(它在打包之后才存在, 正好能校验)。
$rootArchives = @('SHA256SUMS.txt', 'rustdesk-deployment.tar', 'rustdesk-deployment.tar.xz')
$files = Get-ChildItem $OutRoot -Recurse -File -Force |
    Where-Object {
        $rel = $_.FullName.Substring($OutRoot.Length + 1)
        -not ($rel -notmatch '\\' -and $rootArchives -contains $_.Name)
    } | Sort-Object FullName

$lines = New-Object System.Collections.Generic.List[string]
$i = 0
foreach ($f in $files) {
    $rel = $f.FullName.Substring($OutRoot.Length + 1) -replace '\\', '/'
    $h = (Get-FileHash $f.FullName -Algorithm SHA256).Hash.ToLower()
    $lines.Add("$h  $rel")
    $i++
    if ($i % 20000 -eq 0) { Write-Host "  ...$i / $($files.Count)" }
}
[System.IO.File]::WriteAllLines($sums, $lines, (New-Object System.Text.UTF8Encoding($false)))
Write-Host ("  {0} 条 -> {1}" -f $lines.Count, $sums)

# 抽查三个
Write-Host "==> 抽查" -ForegroundColor Cyan
# 注意: 数组字面量里逗号的优先级高于减号, 减法必须加括号,
# 否则会解析成「数组减数组」, 抛 op_Subtraction。
$picks = @(0, [int]($lines.Count / 2), ($lines.Count - 1))
foreach ($pick in $picks) {
    $line = $lines[$pick]
    $parts = $line -split '  ', 2
    $real = (Get-FileHash -LiteralPath (Join-Path $OutRoot $parts[1]) -Algorithm SHA256).Hash.ToLower()
    if ($real -eq $parts[0]) { Write-Host "  OK  $($parts[1])" -ForegroundColor Green }
    else { Write-Host "  BAD $($parts[1])" -ForegroundColor Red }
}
