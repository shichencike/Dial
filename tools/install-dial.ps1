<#
.SYNOPSIS
    Dial 一键安装脚本（Windows / PowerShell）——ARCHITECTURE.md §3.2 阶段 5。

.DESCRIPTION
    承担两件事（顺序不能反）：

    1. 初始化 Dial 环境：建 %USERPROFILE%\.dial\ 目录树、把 dialc 挂进 PATH、
       写工具链配置；
    2. 自动安装已下载的包：初始化完成后重新扫描找到的 .dialpkg
       （当前目录及其子目录，或脚本参数指定的目录），逐个执行安装命令。

    为什么脚本要「自动重新扫描」而不是让用户逐个点名：一键安装的语义是
    「我把这个目录交给你，你把该装的都装上」。用户不该需要记住包名、
    也不该需要为每个包敲一条命令——包就在那里，脚本看得见。

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File install-dial.ps1
    powershell -ExecutionPolicy Bypass -File install-dial.ps1 -ScanDirs C:\pkgs

    .NOTES
    环境变量：
      DIALC_BIN   指定要挂进 PATH 的 dialc 可执行文件（默认：脚本同目录的 .\dialc.exe）
      DIALC_HOME  指定 .dial 的位置（默认：%USERPROFILE%\.dial）
      DIALC_SRC   指定 Dial 发行包的根目录（含 stdlib\ 与 runtime\）（默认：脚本所在目录）
#>

param(
    [string[]] $ScanDirs = @(),
    [string]   $DialcBin  = $env:DIALC_BIN,
    [string]   $DialcHome = $env:DIALC_HOME,
    [string]   $DialcSrc  = $env:DIALC_SRC
)

$ErrorActionPreference = 'Stop'

function Say  { param([string]$m) Write-Host "  $m" }
function Ok   { param([string]$m) Write-Host "  [OK] $m" }
function Warn { param([string]$m) Write-Host "  [!]  $m" }
function Die  { param([string]$m) Write-Host "  [X]  $m" -ForegroundColor Red; exit 1 }

Write-Host "== Dial 一键安装 =="

# ---------------------------------------------------------------------------
# 第 1 步：定位 dialc
# ---------------------------------------------------------------------------
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
if (-not $DialcHome) { $DialcHome = Join-Path $env:USERPROFILE ".dial" }
if (-not $DialcBin)  { $DialcBin  = Join-Path $ScriptDir "dialc.exe" }
if (-not $DialcSrc)  { $DialcSrc  = $ScriptDir }

if (-not (Test-Path $DialcBin)) {
    $cmd = Get-Command dialc -ErrorAction SilentlyContinue
    if ($cmd) {
        $DialcBin = $cmd.Source
    } else {
        Die "找不到 dialc 可执行文件。`n  请把 dialc.exe 放在本脚本同目录下，或用环境变量指定：`n      `$env:DIALC_BIN = 'C:\path\to\dialc.exe'"
    }
}
Ok "编译器: $DialcBin"

# ---------------------------------------------------------------------------
# 第 2 步：初始化环境（建目录树 + 挂 PATH + 写配置）
# ---------------------------------------------------------------------------
Write-Host ""
Write-Host "-- 初始化环境 --"

foreach ($d in @("bin", "packages", "cache\downloads")) {
    $p = Join-Path $DialcHome $d
    New-Item -ItemType Directory -Force -Path $p | Out-Null
}
Ok "目录树: $DialcHome\{bin,packages,cache\downloads}"

Copy-Item -Force $DialcBin (Join-Path $DialcHome "bin\dialc.exe")
Ok "编译器已安装到 $DialcHome\bin\dialc.exe"

# **标准库与运行时也要装**：编译器按「可执行文件所在位置」向上找 stdlib\ 与 runtime\
# （find_project_base：<exe_dir>\..\..\ 即工程根）。装到 %USERPROFILE%\.dial\bin\dialc.exe 后，
# 那个位置正好是 %USERPROFILE%\.dial\ —— 所以把 stdlib\ 与 runtime\ 放这里即可。
# 少了这一步，用户在自己项目里编译会报「找不到 stdlib/ 与 runtime/ 目录」。
if ((Test-Path (Join-Path $DialcSrc "stdlib")) -and (Test-Path (Join-Path $DialcSrc "runtime"))) {
    foreach ($d in @("stdlib", "runtime")) {
        $t = Join-Path $DialcHome $d
        if (Test-Path $t) { Remove-Item -Recurse -Force $t }
        Copy-Item -Recurse -Force (Join-Path $DialcSrc $d) $t
    }
    Ok "标准库与运行时已安装到 $DialcHome\{stdlib,runtime}"
} else {
    Warn "在 $DialcSrc 下没找到 stdlib\ 与 runtime\。`n  请用完整发行包运行本脚本（或用 DIALC_SRC 指定发行包根目录）。`n  否则编译时会报「找不到 stdlib/ 与 runtime/ 目录」。"
}

# PATH 持久化：写进用户级环境变量
$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
$binDir   = Join-Path $DialcHome "bin"
if ($userPath -notlike "*$binDir*") {
    [Environment]::SetEnvironmentVariable("Path", "$userPath;$binDir", "User")
    Ok "PATH 已写入用户环境变量（重开终端后生效）"
} else {
    Say "本次会话请先执行： `$env:Path = `"`$env:Path;$binDir`""
}

$cfg = @(
    "# Dial 环境配置（由 install-dial.ps1 生成）"
    "dialc_path   = $(Join-Path $DialcHome 'bin\dialc.exe')"
    "installed_at = $((Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ'))"
    "uname        = $env:OS / $([System.Environment]::OSVersion.VersionString)"
)
Set-Content -Path (Join-Path $DialcHome "config.txt") -Value $cfg -Encoding UTF8
Ok "工具链配置: $DialcHome\config.txt"

# ---------------------------------------------------------------------------
# 第 3 步：重新扫描 .dialpkg 并逐个安装
# ---------------------------------------------------------------------------
Write-Host ""
Write-Host "-- 扫描 .dialpkg --"

if ($ScanDirs.Count -eq 0) { $ScanDirs = @(".") }

$pkgs = @()
foreach ($d in $ScanDirs) {
    if (-not (Test-Path $d)) { Warn "扫描目录不存在: $d"; continue }
    $pkgs += Get-ChildItem -Path $d -Recurse -File -Filter *.dialpkg -ErrorAction SilentlyContinue |
             Sort-Object FullName
}

if ($pkgs.Count -eq 0) {
    Say "未找到 .dialpkg（跳过包安装）"
} else {
    Write-Host "  找到 $($pkgs.Count) 个包："
    foreach ($p in $pkgs) { Write-Host "    - $($p.FullName)" }
    Write-Host ""
    $installed = 0; $failed = 0
    foreach ($p in $pkgs) {
        Write-Host "  -> 安装 $($p.FullName)"
        & (Join-Path $DialcHome "bin\dialc.exe") install $p.FullName
        if ($LASTEXITCODE -eq 0) { $installed++ } else { $failed++; Warn "安装失败: $($p.FullName)" }
        Write-Host ""
    }
    Ok "包安装完成：成功 $installed 个，失败 $failed 个"
}

Write-Host ""
Write-Host "== 完成 =="
Say "用法：dialc build 你的程序.dial -o 程序.exe"
Say "      import ""包名""; 即可调用已安装的包"
