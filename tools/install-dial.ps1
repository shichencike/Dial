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

    Network resilience: every download tries a domestic mirror (ghproxy.net) first
    and automatically falls back to official GitHub. Downloads resume on failure
    (curl.exe -C -). If all sources are unreachable the installer only warns and
    does not abort -- the compiler itself still works (stdlib falls back to building
    from source at link time).

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File install-dial.ps1
    powershell -ExecutionPolicy Bypass -File install-dial.ps1 -ScanDirs C:\pkgs

.NOTES
    Environment variables:
      DIALC_BIN       Path to the dialc executable (default: .\dialc.exe next to this script)
      DIALC_HOME      Location of .dial (default: %USERPROFILE%\.dial)
      DIALC_SRC       Root of the Dial release package (default: script directory)
      DIAL_VERSION    Version of companion components (default: 0.98.1)
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

# ---------------------------------------------------------------------------
# Constants & config
# ---------------------------------------------------------------------------
# Official stdlib is published independently (tag stdlib-v*), decoupled from the
# compiler. Bump this tag to upgrade the stdlib (matches Dial Releases stdlib-v*).
$StdlibRelease = if ($env:STDLIB_RELEASE) { $env:STDLIB_RELEASE } else { "stdlib-v1.3.0" }

# Companion component (VS Code extension) version.
# Default is only used for bootstrap when no local dialc exists; once we locate a
# real dialc we read its actual version so the companion components always match
# the compiler (kills the version mismatch that made the extension 404 every time).
$DialVersion = if ($env:DIALC_VERSION) { $env:DIALC_VERSION } else { "0.99.1" }
$DialTag = "v" + $DialVersion
$VsixFile = "dial-$DialVersion.vsix"

# Recompute the tag / vsix-file variables after $DialVersion changes.
function Update-VersionVars {
    $script:DialTag  = "v" + $script:DialVersion
    $script:VsixFile = "dial-" + $script:DialVersion + ".vsix"
}

# Read the real version from an installed dialc and override the default/env value.
function Resolve-DialVersion {
    param([string] $Bin)
    if (-not (Test-Path $Bin)) { return }
    try {
        $out = & $Bin --version 2>$null
        if (-not $out) { $out = & $Bin version 2>$null }
        if ($out) {
            $m = [regex]::Match($out, '\d+\.\d+\.\d+')
            if ($m.Success) { $script:DialVersion = $m.Value; Update-VersionVars }
        }
    } catch { }
}

# Resolve the VS Code extension asset name from the release's asset list, so it
# works even when the extension's own semver doesn't match the compiler version.
function Resolve-VsixAsset {
    param([string] $Tag)
    try {
        $api  = "https://api.github.com/repos/shichencike/Dial/releases/tags/$Tag"
        $json = curl.exe -fSL --connect-timeout 15 --max-time 60 -H "Accept: application/vnd.github+json" $api 2>$null
        if ($json) {
            $m = [regex]::Match($json, '"name":\s*"([^"]+\.vsix)"')
            if ($m.Success) { return $m.Groups[1].Value }
        }
    } catch { }
    return ""
}

# Domestic mirror prefix. First candidate source; auto-falls back to official.
# Empty it (DIAL_MIRROR_PREFIX="") to use official GitHub only.
$MirrorPrefix = if ($env:DIAL_MIRROR_PREFIX) { $env:DIAL_MIRROR_PREFIX } else { "https://ghproxy.net/" }

function Say  { param([string]$m) Write-Host "  $m" }
function Ok   { param([string]$m) Write-Host "  [OK] $m" }
function Warn { param([string]$m) Write-Host "  [!]  $m" }
function Die  { param([string]$m) Write-Host "  [X]  $m" -ForegroundColor Red; exit 1 }

# ---------------------------------------------------------------------------
# Download helper: mirror-first, official-fallback, resume.
#   Returns $true on success (file exists and is non-empty), $false otherwise.
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
        # Prefer curl.exe: supports -C - resume and avoids PowerShell codepage
        # issues with non-ASCII URLs.
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
                # Invoke-WebRequest has no resume; attempt a fresh download.
                Invoke-WebRequest -Uri $url -OutFile $OutFile -ErrorAction Stop
                if ((Test-Path $OutFile -PathType Leaf) -and ((Get-Item $OutFile).Length -gt 0)) {
                    return $true
                }
            } catch {
                # try the next candidate
            }
        }
    }
    return $false
}

Write-Host "== Dial one-click install =="

# ---------------------------------------------------------------------------
# Bootstrap (for `irm ... | iex` one-liner)
#   When there is no local release to install from, download the Windows release
#   zip for the current version (mirror-first, official-fallback, resume) and run
#   the bundled installer inside it.
#   The one-liner URL points at raw.githubusercontent.com/<branch>/tools/... which
#   returns the file directly with NO releases/latest redirect; the zip is fetched
#   by exact version tag, again with no latest indirection.
# ---------------------------------------------------------------------------
$ScriptDir = if ($MyInvocation.MyCommand.Path) { Split-Path -Parent $MyInvocation.MyCommand.Path } else { "" }
if (-not $DialcHome) { $DialcHome = Join-Path $env:USERPROFILE ".dial" }
if (-not $DialcBin)  { $DialcBin  = if ($ScriptDir) { Join-Path $ScriptDir "dialc.exe" } else { "dialc.exe" } }
if (-not $DialcSrc)  { $DialcSrc  = $ScriptDir }

function Invoke-Bootstrap {
    param([string[]] $BootstrapArgs = @())
    Write-Host "== Bootstrapping Dial release =="
    $slug = "windows-x86_64"
    $zip = "dialc-$DialTag-$slug.zip"
    $work = New-Item -ItemType Directory -Force -Path (Join-Path $env:TEMP ("dialboot-" + [guid]::NewGuid().ToString("N")))
    $target = Join-Path $work.FullName $zip
    Say "  platform: $slug   version: $DialVersion"
    $official = "https://github.com/shichencike/Dial/releases/download/$DialTag/$zip"
    if (-not (Invoke-DialDownload -OutFile $target -OfficialUrl $official)) {
        Die "Failed to download the Dial release (mirror and official both failed). Download manually: $official then run install-dial.ps1 from inside the zip."
    }
    Expand-Archive -Path $target -DestinationPath $work.FullName -Force
    $inner = Get-ChildItem -Path $work.FullName -Recurse -File -Filter install-dial.ps1 | Select-Object -First 1
    if (-not $inner) { Die "install-dial.ps1 not found inside the release zip." }
    & powershell -ExecutionPolicy Bypass -File $inner.FullName @BootstrapArgs
    exit $LASTEXITCODE
}

# Bootstrap only if we are NOT already inside a local release.
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
# Override the companion-component version with the real compiler version.
Resolve-DialVersion -Bin $DialcBin
Ok "Dial version: $DialVersion ($DialTag)"

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

# LLVM-C.dll must sit next to dialc.exe, or the runtime loader fails (Windows uses
# the official MSVC-built LLVM 18 C ABI; the DLL must travel with the binary).
$llvmDll = Join-Path $DialcSrc "LLVM-C.dll"
if (Test-Path $llvmDll) {
    Copy-Item -Force $llvmDll (Join-Path $DialcHome "bin\LLVM-C.dll")
    Ok "LLVM-C.dll installed next to dialc.exe (runtime dependency)"
} else {
    Warn "LLVM-C.dll not found under $DialcSrc.`n  dialc.exe needs it in the same folder at runtime.`n  Copy LLVM-C.dll from the LLVM 18 release into $DialcHome\bin\."
}

# stdlib/ and runtime/ must also be installed: the compiler walks up from the
# executable location to find stdlib\ and runtime\. Installed at %USERPROFILE%\.dial\
# that location is exactly %USERPROFILE%\.dial\, so place them there.
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

# Persist PATH at the user level.
$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
$binDir   = Join-Path $DialcHome "bin"
if ($userPath -notlike "*$binDir*") {
    [Environment]::SetEnvironmentVariable("Path", "$userPath;$binDir", "User")
    Ok "PATH written to user environment (restart terminal to take effect)"
} else {
    Say "For this session, run first: `$env:Path = `"`$env:Path;$binDir`""
}

$cfg = @(
    "# Dial environment config (generated by install-dial.ps1)"
    "dialc_path   = $(Join-Path $DialcHome 'bin\dialc.exe')"
    "version      = $DialVersion"
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
$StdlibOfficial = "https://github.com/shichencike/Dial/releases/download/$StdlibRelease/stdlib.dialpkg"
$StdlibTmp  = Join-Path $env:TEMP ("stdlib-" + [guid]::NewGuid().ToString("N") + ".dialpkg")
if (Invoke-DialDownload -OutFile $StdlibTmp -OfficialUrl $StdlibOfficial -OverrideUrl $env:STDLIB_URL) {
    & (Join-Path $DialcHome "bin\dialc.exe") install $StdlibTmp
    if ($LASTEXITCODE -eq 0) { Ok "Official stdlib installed ($StdlibRelease)" }
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
# Resolve the actual asset name from the release (so a version mismatch in the
# extension's own semver doesn't cause a 404). Fall back to the version-derived
# name if the API is unreachable.
$resolved = Resolve-VsixAsset -Tag $DialTag
if ($resolved) { $VsixFile = $resolved } else {
    Write-Host "    (could not list release assets; falling back to version-derived name: $VsixFile)"
}
$VsixOfficial = "https://github.com/shichencike/Dial/releases/download/$DialTag/$VsixFile"

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
    $vsixMsg = if ($env:DIAL_VSIX_URL) { $env:DIAL_VSIX_URL } else { $VsixOfficial }
    Warn "VS Code CLI not found (code / code-insiders / codium); skipping extension install.`n  After installing VS Code, run manually:`n    $vsixMsg`n    code --install-extension <downloaded .vsix>"
} else {
    $vsixTmp = Join-Path $env:TEMP ("dialplugin-" + [guid]::NewGuid().ToString("N") + ".vsix")
    if (Invoke-DialDownload -OutFile $vsixTmp -OfficialUrl $VsixOfficial -OverrideUrl $env:DIAL_VSIX_URL) {
        # No --force: if the user already has a newer version, keep it (don't downgrade).
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
