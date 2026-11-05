<#
.SYNOPSIS
    Dial one-click installer for Windows (PowerShell).

.DESCRIPTION
    Performs three steps, in this exact order:

    1. Initialize the Dial environment: create %USERPROFILE%\.dial\ directory tree,
       put dialc into PATH, and write a toolchain config file.
    2. Auto-install any downloaded packages: after init, re-scan for .dialpkg files
       (current directory and its sub-directories, or directories passed as arguments)
       and install each via `dialc install`.
    3. Fetch companion components: the official stdlib (stdlib.dialpkg) and the
       VS Code extension (.vsix) are downloaded and installed silently.

    All user-facing messages are in English on purpose: the Windows console (cmd /
    PowerShell) often runs a non-UTF-8 codepage, and Chinese text would render as
    mojibake. Keeping output ASCII/English guarantees a readable installer.

    Zero hardcoded versions: at runtime the installer queries the GitHub API for the
    latest Release's real asset list, then downloads via
    releases/latest/download/<asset-name>. The asset names carry version numbers
    (dialc-v1.3-windows-x86_64.zip / dial-1.3.0.vsix), so they must be resolved
    from the asset list rather than guessed. stdlib.dialpkg is published on the
    same v* Release page as the zips and the extension, so it is reachable through
    the same latest redirect.

    Network resilience: every download tries a domestic mirror (ghproxy.net) first
    and automatically falls back to official GitHub. Downloads resume on failure
    (curl.exe -C -). If all sources are unreachable the installer only warns and
    does not abort -- the compiler itself still works.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File install-dial.ps1
    powershell -ExecutionPolicy Bypass -File install-dial.ps1 -ScanDirs C:\pkgs

.NOTES
    Environment variables:
      DIAL_VERSION    Optional version pin (e.g. 1.3.0). When set, the installer
                      downloads that exact tag instead of following the latest
                      Release. Leave unset (default) to always follow latest.
      STDLIB_RELEASE  Optional stdlib tag pin (legacy stdlib-v* channel); when set,
                      stdlib.dialpkg is fetched from that tag instead of latest.
      DIALC_BIN       Path to the dialc executable (default: .\dialc.exe next to this script)
      DIALC_HOME      Location of .dial (default: %USERPROFILE%\.dial)
      DIALC_SRC       Root of the Dial release package (default: script directory)
      STDLIB_URL      Override the stdlib download URL (mirror / self-hosted)
      DIAL_VSIX_URL   Override the VS Code extension download URL
      DIAL_MIRROR_PREFIX  Mirror prefix (default: https://ghproxy.net/); set empty to use official only
#>

param(
    [string[]] $ScanDirs = @(),
    [string]   $DialcBin  = $env:DIALC_BIN,
    [string]   $DialcHome = $env:DIALC_HOME,
    [string]   $DialcSrc  = $env:DIALC_SRC
)

$ErrorActionPreference = 'Stop'
# Windows PowerShell 5.1 defaults to TLS 1.0; api.github.com requires TLS 1.2+.
try { [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12 } catch { }

# ---------------------------------------------------------------------------
# Constants & config (no hardcoded versions / tags)
# ---------------------------------------------------------------------------
# Optional "pin" overrides. When both are empty, every download follows the
# latest Release through releases/latest/download/.
$PinnedVersion = if ($env:DIAL_VERSION) { $env:DIAL_VERSION } elseif ($env:DIALC_VERSION) { $env:DIALC_VERSION } else { "" }
$StdlibRelease = if ($env:STDLIB_RELEASE) { $env:STDLIB_RELEASE } else { "" }
$DialVersion = $PinnedVersion     # display only; no longer drives download URLs
$DialTag = ""
$Pinned = $false
if ($PinnedVersion) { $DialTag = "v" + $PinnedVersion; $Pinned = $true }

$Repo = "shichencike/Dial"
$ReleaseLatest = "https://github.com/$Repo/releases/latest/download"
$ReleaseTagged = "https://github.com/$Repo/releases/download"

$script:Assets = @()
$script:LatestTag = ""
$script:AssetsLoaded = $false

# ---------------------------------------------------------------------------
# Latest Release asset resolution (the core of "zero hardcoded versions")
#   GitHub's releases/latest/download/<name> needs the exact asset name, and our
#   names embed versions. Resolve them from the API at runtime; never guess.
# ---------------------------------------------------------------------------
function Get-ReleaseAssets {
    if ($script:AssetsLoaded) { return $true }
    $api = if ($DialTag) {
        "https://api.github.com/repos/$Repo/releases/tags/$DialTag"
    } else {
        "https://api.github.com/repos/$Repo/releases/latest"
    }
    $rel = $null
    try {
        $rel = Invoke-RestMethod -Uri $api `
            -Headers @{ "Accept" = "application/vnd.github+json"; "User-Agent" = "dial-installer" } `
            -TimeoutSec 60 -ErrorAction Stop
    } catch {
        # Fall back to curl.exe (some environments have different TLS/proxy setup).
        try {
            $raw = curl.exe -fSL --connect-timeout 15 --max-time 60 `
                -H "Accept: application/vnd.github+json" $api 2>$null
            if ($raw) { $rel = ($raw -join "`n") | ConvertFrom-Json }
        } catch { }
    }
    if (-not $rel) { return $false }
    $script:LatestTag = [string]$rel.tag_name
    $script:Assets = @($rel.assets | ForEach-Object { [string]$_.name })
    $script:AssetsLoaded = $true
    return $true
}

# Select-Asset <regex>: first matching asset name from the cached list.
function Select-Asset {
    param([string] $Pattern)
    return ($script:Assets | Where-Object { $_ -match $Pattern } | Select-Object -First 1)
}

# Get-AssetUrl <name>: build a URL that redirects to the latest Release (or the
# exact tag when a version is pinned).
function Get-AssetUrl {
    param([string] $Name)
    if ($DialTag) { return "$ReleaseTagged/$DialTag/$Name" }
    return "$ReleaseLatest/$Name"
}

# Read the real version from an installed dialc for display / config.txt only.
# Download URLs never depend on it -- they come from the latest Release assets.
function Resolve-DialVersion {
    param([string] $Bin)
    if ($Pinned) { return }
    if (-not (Test-Path $Bin)) { return }
    try {
        $out = & $Bin --version 2>$null
        if (-not $out) { $out = & $Bin version 2>$null }
        if ($out) {
            $m = [regex]::Match(($out -join " "), '\d+\.\d+\.\d+')
            if ($m.Success) { $script:DialVersion = $m.Value }
        }
    } catch { }
}

# Domestic mirror prefix. First candidate source; auto-falls back to official.
$MirrorPrefix = if ($env:DIAL_MIRROR_PREFIX) { $env:DIAL_MIRROR_PREFIX } else { "https://ghproxy.net/" }

function Say  { param([string]$m) Write-Host "  $m" }
function Ok   { param([string]$m) Write-Host "  [OK] $m" }
function Warn { param([string]$m) Write-Host "  [!]  $m" }
function Die  { param([string]$m) Write-Host "  [X]  $m" -ForegroundColor Red; exit 1 }

# ---------------------------------------------------------------------------
# Download helper: mirror-first, official-fallback, resume.
# ---------------------------------------------------------------------------
function Invoke-DialDownload {
    param(
        [string] $OutFile,
        [string] $OfficialUrl,
        [string] $OverrideUrl = ""
    )
    $candidates = @()
    if ($OverrideUrl) {
        $candidates = @($OverrideUrl)
    } elseif ($MirrorPrefix) {
        $candidates = @("${MirrorPrefix}${OfficialUrl}", $OfficialUrl)
    } else {
        $candidates = @($OfficialUrl)
    }
    foreach ($url in $candidates) {
        Write-Host "  source: $url"
        if (Get-Command curl.exe -ErrorAction SilentlyContinue) {
            $errFile = Join-Path $env:TEMP ("dialdl-" + [guid]::NewGuid().ToString("N"))
            curl.exe -fSL --retry 3 -C - --proto "=https" --connect-timeout 15 --max-time 600 -o $OutFile $url 2>$errFile
            if (($LASTEXITCODE -eq 0) -and (Test-Path $OutFile -PathType Leaf) -and ((Get-Item $OutFile).Length -gt 0)) {
                Remove-Item -Force $errFile -ErrorAction SilentlyContinue
                return $true
            }
            $lastErr = (Get-Content $errFile -ErrorAction SilentlyContinue | Select-Object -Last 3) -join " "
            Remove-Item -Force $errFile -ErrorAction SilentlyContinue
            if ($lastErr) { Write-Host "    last error: $lastErr" }
        } else {
            try {
                Invoke-WebRequest -Uri $url -OutFile $OutFile -ErrorAction Stop
                if ((Test-Path $OutFile -PathType Leaf) -and ((Get-Item $OutFile).Length -gt 0)) {
                    return $true
                }
            } catch { }
        }
    }
    return $false
}

Write-Host "== Dial one-click install =="

# ---------------------------------------------------------------------------
# Bootstrap (for `irm ... | iex` one-liner)
#   The one-liner URL points at raw.githubusercontent.com/<branch>/tools/... which
#   returns the file directly. The release zip is then fetched through
#   releases/latest/download/<resolved-asset> so it follows the newest Release.
# ---------------------------------------------------------------------------
$ScriptDir = if ($MyInvocation.MyCommand.Path) { Split-Path -Parent $MyInvocation.MyCommand.Path } else { "" }
if (-not $DialcHome) { $DialcHome = Join-Path $env:USERPROFILE ".dial" }
if (-not $DialcBin)  { $DialcBin  = if ($ScriptDir) { Join-Path $ScriptDir "dialc.exe" } else { "dialc.exe" } }
if (-not $DialcSrc)  { $DialcSrc  = $ScriptDir }

function Invoke-Bootstrap {
    param([string[]] $BootstrapArgs = @())
    Write-Host "== Bootstrapping Dial release =="
    $slug = "windows-x86_64"
    if (-not (Get-ReleaseAssets)) {
        Die "Cannot query the latest GitHub Release assets (API unreachable). Check your network, or set DIAL_VERSION=1.3.0 to pin a version."
    }
    $zip = Select-Asset -Pattern ("-" + [regex]::Escape($slug) + "\.zip$")
    if (-not $zip) { Die "No $slug release zip found in the latest Release ($($script:LatestTag)). Assets: $($script:Assets -join ', ')" }
    Say "  platform: $slug   following: $($script:LatestTag)   package: $zip"
    $work = New-Item -ItemType Directory -Force -Path (Join-Path $env:TEMP ("dialboot-" + [guid]::NewGuid().ToString("N")))
    $target = Join-Path $work.FullName $zip
    $official = Get-AssetUrl -Name $zip
    if (-not (Invoke-DialDownload -OutFile $target -OfficialUrl $official)) {
        Die "Failed to download the Dial release (mirror and official both failed). Download manually: $official then run install-dial.ps1 from inside the zip."
    }
    Expand-Archive -Path $target -DestinationPath $work.FullName -Force
    $inner = Get-ChildItem -Path $work.FullName -Recurse -File -Filter install-dial.ps1 | Select-Object -First 1
    if (-not $inner) { Die "install-dial.ps1 not found inside the release zip." }
    & powershell -ExecutionPolicy Bypass -File $inner.FullName @BootstrapArgs
    exit $LASTEXITCODE
}

$inLocalRelease = $false
if ($DialcBin -and (Test-Path $DialcBin)) {
    $inLocalRelease = $true
} else {
    $cmd = Get-Command dialc -ErrorAction SilentlyContinue
    if ($cmd) {
        $inLocalRelease = $true
    } elseif ($ScriptDir -and (Test-Path (Join-Path $ScriptDir "dialc.exe"))) {
        $inLocalRelease = $true
    } elseif (Test-Path (Join-Path (Get-Location) "dialc.exe")) {
        $inLocalRelease = $true
    }
}
if (-not $inLocalRelease) { Invoke-Bootstrap @args }

# ---------------------------------------------------------------------------
# Step 1: locate dialc
# ---------------------------------------------------------------------------
if (-not (Test-Path $DialcBin)) {
    $cmd = Get-Command dialc -ErrorAction SilentlyContinue
    if ($cmd) {
        $DialcBin = $cmd.Source
    } else {
        Die "Cannot find the dialc executable.`n  Place dialc.exe next to this script, or set it via:`n      `$env:DIALC_BIN = 'C:\path\to\dialc.exe'"
    }
}
Ok "Compiler: $DialcBin"
Resolve-DialVersion -Bin $DialcBin
$shownVersion = if ($DialVersion) { $DialVersion } elseif ($script:LatestTag) { $script:LatestTag } else { "follow latest Release" }
Ok "Dial version: $shownVersion"

# Pre-fetch the latest Release asset list once; failure only warns (companion
# components degrade gracefully).
if (-not (Get-ReleaseAssets)) {
    Warn "Cannot query the latest GitHub Release assets (API unreachable); stdlib and VS Code extension auto-install may be skipped."
}

# ---------------------------------------------------------------------------
# Step 2: initialize environment (directory tree + PATH + config)
# ---------------------------------------------------------------------------
Write-Host ""
Write-Host "-- Initialize environment --"

foreach ($d in @("bin", "packages", "cache\downloads")) {
    $p = Join-Path $DialcHome $d
    New-Item -ItemType Directory -Force -Path $p | Out-Null
}
Ok "Directory tree: $DialcHome\{bin,packages,cache\downloads}"

Copy-Item -Force $DialcBin (Join-Path $DialcHome "bin\dialc.exe")
Ok "Compiler installed to $DialcHome\bin\dialc.exe"

# LLVM-C.dll must sit next to dialc.exe, or the runtime loader fails.
$llvmDll = Join-Path $DialcSrc "LLVM-C.dll"
if (Test-Path $llvmDll) {
    Copy-Item -Force $llvmDll (Join-Path $DialcHome "bin\LLVM-C.dll")
    Ok "LLVM-C.dll installed next to dialc.exe (runtime dependency)"
} else {
    Warn "LLVM-C.dll not found under $DialcSrc.`n  dialc.exe needs it in the same folder at runtime.`n  Copy LLVM-C.dll from the LLVM 18 release into $DialcHome\bin\."
}

if ((Test-Path (Join-Path $DialcSrc "stdlib")) -and (Test-Path (Join-Path $DialcSrc "runtime"))) {
    foreach ($d in @("stdlib", "runtime")) {
        $t = Join-Path $DialcHome $d
        if (Test-Path $t) { Remove-Item -Recurse -Force $t }
        Copy-Item -Recurse -Force (Join-Path $DialcSrc $d) $t
    }
    Ok "Standard library and runtime installed to $DialcHome\{stdlib,runtime}"
} else {
    Warn "stdlib\ and runtime\ not found under $DialcSrc.`n  Run this script from the full release package (or set DIALC_SRC).`n  Otherwise compilation fails with 'cannot find stdlib/ and runtime/ directories'."
}

$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
$binDir   = Join-Path $DialcHome "bin"
if ($userPath -notlike "*$binDir*") {
    [Environment]::SetEnvironmentVariable("Path", "$userPath;$binDir", "User")
    Ok "PATH written to user environment (restart terminal to take effect)"
} else {
    Say "For this session, run first: `$env:Path = `"`$env:Path;$binDir`""
}

$cfgVersion = if ($DialVersion) { $DialVersion } elseif ($script:LatestTag) { $script:LatestTag } else { "latest" }
$cfg = @(
    "# Dial environment config (generated by install-dial.ps1)"
    "dialc_path   = $(Join-Path $DialcHome 'bin\dialc.exe')"
    "version      = $cfgVersion"
    "installed_at = $((Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ'))"
    "uname        = $env:OS / $([System.Environment]::OSVersion.VersionString)"
)
Set-Content -Path (Join-Path $DialcHome "config.txt") -Value $cfg -Encoding UTF8
Ok "Toolchain config: $DialcHome\config.txt"

# ---------------------------------------------------------------------------
# Step 2.5: fetch and install the official stdlib package
# ---------------------------------------------------------------------------
Write-Host ""
Write-Host "-- Install official stdlib (stdlib.dialpkg) --"
if ($StdlibRelease) {
    $StdlibOfficial = "$ReleaseTagged/$StdlibRelease/stdlib.dialpkg"
    $StdlibLabel = "$StdlibRelease (pinned)"
} else {
    $StdlibOfficial = "$ReleaseLatest/stdlib.dialpkg"
    $StdlibLabel = "following latest Release"
}
if ($script:AssetsLoaded -and -not ($script:Assets -contains "stdlib.dialpkg")) {
    Warn "The latest Release ($($script:LatestTag)) has no stdlib.dialpkg attached (older release). Set STDLIB_RELEASE=stdlib-v1.0.0 for the legacy channel, or upgrade to a newer Release."
}
$StdlibTmp = Join-Path $env:TEMP ("stdlib-" + [guid]::NewGuid().ToString("N") + ".dialpkg")
if (Invoke-DialDownload -OutFile $StdlibTmp -OfficialUrl $StdlibOfficial -OverrideUrl $env:STDLIB_URL) {
    & (Join-Path $DialcHome "bin\dialc.exe") install $StdlibTmp
    if ($LASTEXITCODE -eq 0) { Ok "Official stdlib installed ($StdlibLabel)" }
    else { Warn "dialc install stdlib.dialpkg failed (compiler still works; install manually later)" }
} else {
    Warn "Failed to download the stdlib (offline / all sources unreachable); link will fall back to building stdlib from source."
}
if (Test-Path $StdlibTmp) { Remove-Item -Force $StdlibTmp -ErrorAction SilentlyContinue }

# ---------------------------------------------------------------------------
# Step 2.7: fetch and install the VS Code extension (.vsix)
# ---------------------------------------------------------------------------
Write-Host ""
Write-Host "-- Install VS Code extension --"
$VsixFile = Select-Asset -Pattern '\.vsix$'
if (-not $VsixFile -and $Pinned) { $VsixFile = "dial-$DialVersion.vsix" }

$codeCli = $null
foreach ($c in @("code", "code-insiders", "codium")) {
    $cmd = Get-Command $c -ErrorAction SilentlyContinue
    if ($cmd) { $codeCli = $cmd.Source; break }
}
if (-not $codeCli) {
    $defaultCli = Join-Path $env:LOCALAPPDATA "Programs\Microsoft VS Code\bin\code.cmd"
    if (Test-Path $defaultCli -PathType Leaf) { $codeCli = $defaultCli }
}

if (-not $VsixFile) {
    Warn "Cannot resolve the VS Code extension asset name (GitHub API unreachable); skipping.`n  Download the .vsix manually from https://github.com/$Repo/releases/latest or set DIAL_VSIX_URL."
} elseif (-not $codeCli) {
    $vsixMsg = if ($env:DIAL_VSIX_URL) { $env:DIAL_VSIX_URL } else { (Get-AssetUrl -Name $VsixFile) }
    Warn "VS Code CLI not found (code / code-insiders / codium); skipping extension install.`n  After installing VS Code, run manually:`n    $vsixMsg`n    code --install-extension <downloaded .vsix>"
} else {
    $VsixOfficial = Get-AssetUrl -Name $VsixFile
    $vsixTmp = Join-Path $env:TEMP ("dialplugin-" + [guid]::NewGuid().ToString("N") + ".vsix")
    if (Invoke-DialDownload -OutFile $vsixTmp -OfficialUrl $VsixOfficial -OverrideUrl $env:DIAL_VSIX_URL) {
        & $codeCli --install-extension $vsixTmp
        if ($LASTEXITCODE -eq 0) { Ok "VS Code extension installed ($VsixFile)" }
        else { Warn "VS Code extension install failed; run manually later:`n    & `"$codeCli`" --install-extension $VsixFile" }
    } else {
        Warn "Failed to download the VS Code extension (offline); install manually later."
    }
    if (Test-Path $vsixTmp) { Remove-Item -Force $vsixTmp -ErrorAction SilentlyContinue }
}

# ---------------------------------------------------------------------------
# Step 3: re-scan .dialpkg and install each
# ---------------------------------------------------------------------------
Write-Host ""
Write-Host "-- Scan .dialpkg --"

if ($ScanDirs.Count -eq 0) { $ScanDirs = @(".") }

$pkgs = @()
foreach ($d in $ScanDirs) {
    if (-not (Test-Path $d)) { Warn "Scan directory does not exist: $d"; continue }
    $pkgs += Get-ChildItem -Path $d -Recurse -File -Filter *.dialpkg -ErrorAction SilentlyContinue |
             Sort-Object FullName
}

if ($pkgs.Count -eq 0) {
    Say "No .dialpkg found (skipping package install)"
} else {
    Write-Host "  Found $($pkgs.Count) package(s):"
    foreach ($p in $pkgs) { Write-Host "    - $($p.FullName)" }
    Write-Host ""
    $installed = 0; $failed = 0
    foreach ($p in $pkgs) {
        Write-Host "  -> install $($p.FullName)"
        & (Join-Path $DialcHome "bin\dialc.exe") install $p.FullName
        if ($LASTEXITCODE -eq 0) { $installed++ } else { $failed++; Warn "Install failed: $($p.FullName)" }
        Write-Host ""
    }
    Ok "Package install done: $installed succeeded, $failed failed"
}

Write-Host ""
Write-Host "== Done =="
Say "Usage: dialc build your-program.dial -o program.exe"
Say "       import ""package-name"";  to use an installed package"
