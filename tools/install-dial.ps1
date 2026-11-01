<#
.SYNOPSIS
    Dial 一键安装脚本（Windows / PowerShell）——ARCHITECTURE.md §3.2 阶段 5。

.DESCRIPTION
    承担三件事（顺序不能反）：

    1. 初始化 Dial 环境：建 %USERPROFILE%\.dial\ 目录树、把 dialc 挂进 PATH、
       写工具链配置；
    2. 自动安装已下载的包：初始化完成后重新扫描找到的 .dialpkg
       （当前目录及其子目录，或脚本参数指定的目录），逐个执行安装命令；
    3. 补齐配套件：官方标准库（stdlib.dialpkg）与 VS Code 插件（.vsix）
       静默下载并安装——都是「装上编译器后自然该有」的东西，不该让用户再手动跑一遍。

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
      DIALC_VERSION 指定配套件的版本（默认：0.98.0）。Release 标签（v0.98）与
                  插件文件名（dial-0.98.0.vsix）都由它推导，只改这一处即可。
#>

param(
    [string[]] $ScanDirs = @(),
    [string]   $DialcBin  = $env:DIALC_BIN,
    [string]   $DialcHome = $env:DIALC_HOME,
    [string]   $DialcSrc  = $env:DIALC_SRC
)

$ErrorActionPreference = 'Stop'

# 官方标准库独立发布（阶段 7）：tag 形如 stdlib-v<版本>，与编译器（dialc.exe）解耦。
# 安装脚本在装好编译器后，会自动静默拉取该版本的标准库包并通过 dialc install 安装。
# 升级标准库时只需改这里的标签（与 Dial Releases 里的 stdlib-v* 对应）。
$StdlibRelease = if ($env:STDLIB_RELEASE) { $env:STDLIB_RELEASE } else { "stdlib-v1.0.0" }

# 配套件（VS Code 插件）版本：只此一个来源。Release 标签（v0.98）与插件文件名
# （dial-0.98.0.vsix）都由它推导，避免三者各说各话、下次发版又忘了改其中一个。
$DialVersion = if ($env:DIALC_VERSION) { $env:DIALC_VERSION } else { "0.98.0" }
$DialTag = "v" + (($DialVersion -split '\.')[0..1] -join '.')
$VsixFile = "dial-$DialVersion.vsix"

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

# LLVM-C.dll 必须和 dialc.exe 同目录，否则运行期加载失败（Windows 走官方 MSVC
# 构建的 LLVM 18 的 C 接口，纯 C ABI，跨工具链无冲突，但 DLL 必须随行）。
$llvmDll = Join-Path $DialcSrc "LLVM-C.dll"
if (Test-Path $llvmDll) {
    Copy-Item -Force $llvmDll (Join-Path $DialcHome "bin\LLVM-C.dll")
    Ok "LLVM-C.dll 已随编译器安装到 $DialcHome\bin\（dialc.exe 运行期依赖）"
} else {
    Warn "在 $DialcSrc 下没找到 LLVM-C.dll。`n  dialc.exe 运行期需要它（与 dialc.exe 同目录）。`n  请从 LLVM 18 发行包复制 LLVM-C.dll 到 $DialcHome\bin\。"
}

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
# 第 2.5 步：自动拉取官方标准库包 stdlib.dialpkg 并安装
#   官方标准库独立发布（tag stdlib-v*，与编译器解耦）。装完编译器后静默拉取
#   对应版本并通过 dialc install 安装，使链接期能优先使用预编译的标准库包。
#   失败（如离线）只告警、不阻断安装——链接时会回退到源码编译 stdlib。
# ---------------------------------------------------------------------------
Write-Host ""
Write-Host "-- 安装官方标准库 stdlib.dialpkg --"
$StdlibUrl  = "https://github.com/shichencike/Dial/releases/download/$StdlibRelease/stdlib.dialpkg"
$StdlibTmp  = Join-Path $env:TEMP ("stdlib-" + [guid]::NewGuid().ToString("N") + ".dialpkg")
$downloaded = $false
try {
        if (Get-Command curl.exe -ErrorAction SilentlyContinue) {
            # --connect-timeout / --max-time：**必须有**。否则 GitHub 不可达时 curl 会一直挂着，
            # 「一键安装」就变成「一键卡住」——用户看到的是脚本不动，而不是一句清晰的失败提示。
            curl.exe -fSL --retry 2 --proto "=https" --connect-timeout 15 --max-time 180 -o $StdlibTmp $StdlibUrl 2>$null
            if ($LASTEXITCODE -eq 0) { $downloaded = $true }
        } else {
        Invoke-WebRequest -Uri $StdlibUrl -OutFile $StdlibTmp -ErrorAction Stop
        $downloaded = $true
    }
} catch {
    $downloaded = $false
}
if ($downloaded -and (Test-Path $StdlibTmp -PathType Leaf) -and ((Get-Item $StdlibTmp).Length -gt 0)) {
    & (Join-Path $DialcHome "bin\dialc.exe") install $StdlibTmp
    if ($LASTEXITCODE -eq 0) { Ok "官方标准库已安装（$StdlibRelease）" }
    else { Warn "dialc install stdlib.dialpkg 失败（不影响编译器本体，可稍后手动安装）" }
} else {
    Warn "下载官方标准库失败（可能离线）；链接时将回退到源码编译 stdlib。"
}
if (Test-Path $StdlibTmp) { Remove-Item -Force $StdlibTmp -ErrorAction SilentlyContinue }

# ---------------------------------------------------------------------------
# 第 2.7 步：自动下载并安装 VS Code 插件（.vsix）
#   编译器装好了，编辑器插件也得跟上——否则用户还得自己去 Release 页翻 .vsix、
#   再手动 `code --install-extension`，「一键」就名不副实了。
#   与标准库同理：失败（离线 / 没装 VS Code）只告警、不阻断安装。
# ---------------------------------------------------------------------------
Write-Host ""
Write-Host "-- 安装 VS Code 插件 --"
$VsixUrl = if ($env:DIAL_VSIX_URL) { $env:DIAL_VSIX_URL } else { "https://github.com/shichencike/Dial/releases/download/$DialTag/$VsixFile" }

# 找 VS Code 的命令行入口：先看 PATH，再看默认安装位置
$codeCli = $null
foreach ($c in @("code", "code-insiders", "codium")) {
    $cmd = Get-Command $c -ErrorAction SilentlyContinue
    if ($cmd) { $codeCli = $cmd.Source; break }
}
if (-not $codeCli) {
    $defaultCli = Join-Path $env:LOCALAPPDATA "Programs\Microsoft VS Code\bin\code.cmd"
    if (Test-Path $defaultCli -PathType Leaf) { $codeCli = $defaultCli }
}

if (-not $codeCli) {
    Warn "未找到 VS Code 命令行（code / code-insiders / codium），跳过插件安装。`n  装好 VS Code 后手动执行即可：`n    $VsixUrl`n    code --install-extension <下载到的 .vsix>"
} else {
    $vsixTmp = Join-Path $env:TEMP ("dialplugin-" + [guid]::NewGuid().ToString("N") + ".vsix")
    $got = $false
    try {
        if (Get-Command curl.exe -ErrorAction SilentlyContinue) {
            curl.exe -fSL --retry 2 --proto "=https" --connect-timeout 15 --max-time 60 -o $vsixTmp $VsixUrl 2>$null
            if ($LASTEXITCODE -eq 0) { $got = $true }
        } else {
            Invoke-WebRequest -Uri $VsixUrl -OutFile $vsixTmp -ErrorAction Stop
            $got = $true
        }
    } catch {
        $got = $false
    }
    if ($got -and (Test-Path $vsixTmp -PathType Leaf) -and ((Get-Item $vsixTmp).Length -gt 0)) {
        # 不加 --force：用户若已装了更新版本，不该在「一键安装」里被悄悄降级
        & $codeCli --install-extension $vsixTmp
        if ($LASTEXITCODE -eq 0) { Ok "VS Code 插件已安装（$VsixFile）" }
        else { Warn "VS Code 插件安装失败，可稍后手动执行：`n    & `"$codeCli`" --install-extension $VsixFile" }
    } else {
        Warn "下载 VS Code 插件失败（可能离线）；可稍后手动安装。"
    }
    if (Test-Path $vsixTmp) { Remove-Item -Force $vsixTmp -ErrorAction SilentlyContinue }
}

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
