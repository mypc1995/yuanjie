<#
.SYNOPSIS
  给 Flutter 工具链打两个补丁, 让 Windows 构建在**没有管理员权限、没开开发人员模式**的机器上也能跑。

  背景: 内网构建机通常不允许改系统设置, 而 Flutter 3.24.5 的 Windows 构建有两个硬性依赖:
    1. 插件目录用符号链接 (symlink)。没有 symlink 权限时直接报
       "Building with plugins requires symlink support. Please enable Developer Mode"。
       -> 改成失败时退化成 **目录联接 (mklink /J)**, 不需要管理员。
    2. 要求 Visual Studio 注册了 "使用 C++ 的桌面开发" 工作负载, 并且带 VS 自带的 CMake 组件。
       内网机器上装不了这个工作负载, 但只要有 MSVC 工具链 + Windows SDK 就能编。
       -> 放宽 isUsable 判断, cmakePath 找不到 VS 自带的 cmake 时退回独立 CMake
         (FLUTTER_CMAKE_PATH 或 PATH)。

  用法:
    powershell -ExecutionPolicy Bypass -File fix-flutter-toolchain.ps1 -FlutterHome D:\flutter
#>
param(
  [string] $FlutterHome = $env:FLUTTER_HOME
)

$ErrorActionPreference = "Stop"

if ([string]::IsNullOrEmpty($FlutterHome)) {
  Write-Host "  x 请用 -FlutterHome 指定 Flutter 目录" -ForegroundColor Red
  exit 1
}

$plugins = Join-Path $FlutterHome "packages\flutter_tools\lib\src\flutter_plugins.dart"
$vs      = Join-Path $FlutterHome "packages\flutter_tools\lib\src\windows\visual_studio.dart"

if (-not (Test-Path $plugins)) { Write-Host "  x 找不到 $plugins" -ForegroundColor Red; exit 1 }
if (-not (Test-Path $vs))      { Write-Host "  x 找不到 $vs" -ForegroundColor Red; exit 1 }

# Flutter 的 .dart 源文件是 CRLF, 而这个脚本文件可能是 LF。
# 直接拿 here-string 去 Contains() 会因为换行符不同而永远匹配不上 ——
# 统一先把内容归一化成 LF, 打完补丁再按原来的换行符写回去。
function Read-Normalized([string]$path) {
  $raw = [System.IO.File]::ReadAllText($path)
  return [PSCustomObject]@{
    Text  = $raw.Replace("`r`n", "`n")
    CRLF  = $raw.Contains("`r`n")
    Raw   = $raw
  }
}

function Write-Normalized([string]$path, $doc) {
  $text = $doc.Text
  if ($doc.CRLF) { $text = $text.Replace("`n", "`r`n") }
  [System.IO.File]::WriteAllText($path, $text, (New-Object System.Text.UTF8Encoding($false)))
}

# 锚点字面量也要归一化 —— 这个脚本本身被存成 CRLF 时, here-string 就是 CRLF,
# 照样匹配不上归一化后的 LF 文本。
function Norm([string]$s) { return $s.Replace("`r`n", "`n") }

# ---------------------------------------------------------------- 补丁 1: symlink -> junction
$doc = Read-Normalized $plugins
$txt = $doc.Text
if ($txt -match 'mklink') {
  Write-Host "  [1/2] symlink->junction 补丁: 已存在, 跳过" -ForegroundColor Green
} else {
  $old = Norm @"
    try {
      link.createSync(path);
    } on FileSystemException catch (e) {
      handleSymlinkException(
"@
  $new = Norm @"
    try {
      link.createSync(path);
    } on FileSystemException catch (e) {
      // Windows 上没有 symlink 权限时 (未开开发人员模式, 或进程不是管理员),
      // 退化成目录联接 (junction): mklink /J 不需要管理员权限。
      // 离线内网构建机往往不允许改系统设置, 这个回退是必须的。
      if (globals.platform.isWindows && e.osError?.errorCode == 1314) {
        final io.ProcessResult result = io.Process.runSync(
          'cmd',
          <String>['/c', 'mklink', '/J', link.path, path],
        );
        if (result.exitCode == 0) {
          continue;
        }
      }
      handleSymlinkException(
"@
  if (-not $txt.Contains($old)) { Write-Host "  x 补丁 1 的锚点没找到, Flutter 版本可能不同" -ForegroundColor Red; exit 1 }
  $txt = $txt.Replace($old, $new)
  if ($txt -notmatch "import 'dart:io' as io;") {
    $txt = $txt.Replace(
      "import 'package:meta/meta.dart';",
      "import 'dart:io' as io; // flutter_ignore: dart_io_import.`nimport 'package:meta/meta.dart';")
  }
  $doc.Text = $txt
  Write-Normalized $plugins $doc
  Write-Host "  [1/2] symlink->junction 补丁: 已应用" -ForegroundColor Green
}

# ---------------------------------------------------------------- 补丁 2: VS 检测放宽
$doc2 = Read-Normalized $vs
$txt2 = $doc2.Text
if ($txt2 -match 'FLUTTER_CMAKE_PATH') {
  Write-Host "  [2/2] Visual Studio 检测补丁: 已存在, 跳过" -ForegroundColor Green
} else {
  $oldCmake = Norm @"
    return _fileSystem.path.joinAll(<String>[
      details.installationPath!,
      'Common7',
      'IDE',
      'CommonExtensions',
      'Microsoft',
      'CMake',
      'CMake',
      'bin',
      'cmake.exe',
    ]);
  }
"@
  $newCmake = Norm @"
    final String bundled = _fileSystem.path.joinAll(<String>[
      details.installationPath!,
      'Common7',
      'IDE',
      'CommonExtensions',
      'Microsoft',
      'CMake',
      'CMake',
      'bin',
      'cmake.exe',
    ]);
    if (_fileSystem.file(bundled).existsSync()) {
      return bundled;
    }
    // VS 自带的 CMake 组件没装 (只装了 MSVC 工具集 + Windows SDK 的常见情况),
    // 退回独立的 CMake: 先看 FLUTTER_CMAKE_PATH, 再在 PATH 里找。
    final String? override = _platform.environment['FLUTTER_CMAKE_PATH'];
    if (override != null && override.isNotEmpty && _fileSystem.file(override).existsSync()) {
      return override;
    }
    for (final String dir in _platform.environment['PATH']!.split(';')) {
      if (dir.isEmpty) {
        continue;
      }
      final String candidate = _fileSystem.path.join(dir, 'cmake.exe');
      if (_fileSystem.file(candidate).existsSync()) {
        return candidate;
      }
    }
    return null;
  }
"@
  $oldUsable = Norm @"
  bool get isUsable {
    if (!meetsRequirements) {
      return false;
    }
"@
  $newUsable = Norm @"
  bool get isUsable {
    // 不再要求工作负载/组件注册齐全。装全 "Desktop development with C++"
    // 工作负载往往需要管理员改系统设置, 离线内网机器上做不到。
    // 只要有 MSVC 工具链和 Windows SDK 就能编, 所以这里放宽。
    if (!(isComplete ?? true)) {
      return false;
    }
"@
  if (-not $txt2.Contains($oldCmake)) { Write-Host "  x 补丁 2a 的锚点没找到" -ForegroundColor Red; exit 1 }
  if (-not $txt2.Contains($oldUsable)) { Write-Host "  x 补丁 2b 的锚点没找到" -ForegroundColor Red; exit 1 }
  $doc2.Text = $txt2.Replace($oldCmake, $newCmake).Replace($oldUsable, $newUsable)
  Write-Normalized $vs $doc2
  Write-Host "  [2/2] Visual Studio 检测补丁: 已应用" -ForegroundColor Green
}

# ---------------------------------------------------------------- 强制重建 snapshot
$stamp = Join-Path $FlutterHome "bin\cache\flutter_tools.stamp"
if (Test-Path $stamp) {
  # stamp 是按 git revision 判定的, 改了 .dart 源码不会自动重建 snapshot, 必须删掉
  [System.IO.File]::Delete($stamp)
  Write-Host "  已清除 flutter_tools.stamp, 下次 flutter 会重建 snapshot" -ForegroundColor Green
}

Write-Host ""
Write-Host "  补丁完成。还需要设置 FLUTTER_CMAKE_PATH 指向独立的 cmake.exe" -ForegroundColor Cyan
