<#
  远界 build machine - one click setup.

  1. install the offline build environment (Rust / Flutter / prebuilt libs / vendored deps)
  2. register this machine as a build node of the admin panel
  3. register a scheduled task so the agent survives reboot

  Usage:
    powershell -ExecutionPolicy Bypass -File setup.ps1
    powershell -ExecutionPolicy Bypass -File setup.ps1 -Panel http://1.2.3.4:21120 -Token abc -Src C:\rustdesk-src
#>
param(
  [string] $Panel = "",
  [string] $Token = "",
  [string] $Src   = "",
  [switch] $NoTask
)

$ErrorActionPreference = "Continue"
$Here = $PSScriptRoot
$Bundle = Join-Path $Here "rustdesk-offline-bundle"
$ConfigFile = Join-Path $Here "deploy-config.txt"

function Say([string]$m) { Write-Host "  $m" -ForegroundColor Green }
function Warn([string]$m) { Write-Host "  ! $m" -ForegroundColor Yellow }
function Die([string]$m) { Write-Host "  x $m" -ForegroundColor Red; exit 1 }

Write-Host "`n=== 远界 build machine setup ===" -ForegroundColor Cyan

# ---------------------------------------------------------------- 读配置
if (Test-Path $ConfigFile) {
  foreach ($line in Get-Content $ConfigFile) {
    $line = $line.Trim()
    if (-not $line -or $line.StartsWith("#")) { continue }
    $kv = $line -split "=", 2
    if ($kv.Count -ne 2) { continue }
    $k = $kv[0].Trim(); $v = $kv[1].Trim()
    switch ($k) {
      "panel" { if (-not $Panel) { $Panel = $v } }
      "token" { if (-not $Token) { $Token = $v } }
      "src"   { if (-not $Src)   { $Src   = $v } }
    }
  }
}
if (-not $Panel) { Die "panel address not set (edit deploy-config.txt or pass -Panel)" }
if (-not $Token) { Die "agent token not set (edit deploy-config.txt or pass -Token)" }
if (-not $Src)   { $Src = Join-Path $env:USERPROFILE "rustdesk-src\rustdesk" }
# deploy-config.txt 里可以写 %USERPROFILE%
$Src = $Src.Replace('%USERPROFILE%', $env:USERPROFILE).Replace('%USERPROFILE', $env:USERPROFILE)
if (-not (Test-Path $Src)) {
  Warn "source tree not found: $Src"
  Warn "the environment installer puts it under <RepoHome>\rustdesk (default: $env:USERPROFILE\rustdesk-src\rustdesk)"
  Warn "fix 'src=' in deploy-config.txt before building"
}

Write-Host "  panel : $Panel"
Write-Host "  token : $Token"
Write-Host "  src   : $Src"

# ---------------------------------------------------------------- 装环境
Write-Host "`n==> [1/3] Installing build environment (offline)" -ForegroundColor Cyan
Write-Host "  This takes ~10 minutes on a cold machine."
& powershell -ExecutionPolicy Bypass -File (Join-Path $Bundle "install-offline.ps1")
if ($LASTEXITCODE -ne 0) { Die "environment install failed (exit $LASTEXITCODE)" }

# ---------------------------------------------------------------- 挂上面板
Write-Host "`n==> [2/3] Registering build node" -ForegroundColor Cyan
$agent = Join-Path $Bundle "agent\agent.py"
if (-not (Test-Path $agent)) { Die "agent.py not found: $agent" }
$python = (Get-Command python -ErrorAction SilentlyContinue).Source
if (-not $python) { $python = (Get-Command python3 -ErrorAction SilentlyContinue).Source }
if (-not $python) { Die "python not found in PATH" }

$argList = "`"$agent`" --panel $Panel --token $Token --src `"$Src`""
$p = Start-Process -FilePath $python -ArgumentList $argList -WindowStyle Hidden -PassThru
Say "agent started (pid $($p.Id))"

# ---------------------------------------------------------------- 开机自启
if (-not $NoTask) {
  Write-Host "`n==> [3/3] Registering scheduled task" -ForegroundColor Cyan
  try {
    $action = New-ScheduledTaskAction -Execute $python -Argument $argList
    $trigger = New-ScheduledTaskTrigger -AtLogOn
    $settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries `
                                     -DontStopIfGoingOnBatteries `
                                     -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1)
    Register-ScheduledTask -TaskName "远界BuildAgent" -Action $action -Trigger $trigger `
                           -Settings $settings -RunLevel Highest -Force | Out-Null
    Say "scheduled task '远界BuildAgent' registered (starts at every logon)"
  } catch {
    Warn "could not register scheduled task: $($_.Exception.Message)"
    Warn "run this by hand if you need it to survive reboot:"
    Warn "  schtasks /create /tn 远界BuildAgent /sc ONLOGON /rl HIGHEST /f /tr `"$python $argList`""
  }
} else {
  Write-Host "`n==> [3/3] Skipping scheduled task (-NoTask)" -ForegroundColor Cyan
}

Write-Host "`n==========================================================" -ForegroundColor Green
Write-Host " Build machine is ready" -ForegroundColor Green
Write-Host "==========================================================" -ForegroundColor Green
Write-Host " Panel   : $Panel"
Write-Host " Source  : $Src"
Write-Host ""
Write-Host " Open the panel, go to 'Client Build', click Start Build."
Write-Host "=========================================================="
