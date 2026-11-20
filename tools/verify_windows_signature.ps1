<#
.SYNOPSIS
    Verify the Authenticode signature of a Dial release dialc.exe against the
    per-release self-signed certificate shipped alongside it.

.DESCRIPTION
    This is the verification counterpart of tools/sign_windows_release.sh.
    Each release ships:
      - dialc.exe                 signed with a one-time RSA-2048 key
      - dialc-code-sign.cer       the PUBLIC certificate only (trust anchor)
      - BUILD_INFO.txt            contains the full version on the "Version:" line

    Verification is deliberately self-contained and does NOT rely on the Windows
    trusted-root store (a self-signed cert is never "Valid" there). Instead:
      1. Get-AuthenticodeSignature checks cryptographic integrity (any tampering
         yields HashMismatch / NotSigned / NotSupportedFileFormat).
      2. The signer certificate's RAW BYTES must equal the shipped .cer byte for
         byte -- this binds the signature to the exact shipped public key.
      3. The certificate CN must embed the same version as BUILD_INFO.txt, so a
         v1 certificate can never sign a v2 package.
      4. The current time must be before NotAfter, so an expired certificate
         (and its signature) is rejected.

    Any failure returns a non-empty error string from Test-DialSignature and the
    caller MUST refuse to install (fail-fast).

    All output is ASCII English on purpose (Windows console codepage safety).

.PARAMETER Exe
    Path to dialc.exe.

.PARAMETER Cer
    Path to dialc-code-sign.cer.

.PARAMETER ExpectedVersion
    Full version string (e.g. 1.10.0). Overrides BUILD_INFO parsing if given.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File verify_windows_signature.ps1 `
        -Exe .\Dial\dialc.exe -Cer .\Dial\dialc-code-sign.cer
#>
param(
    [Parameter(Mandatory=$true)][string] $Exe,
    [string] $Cer = "",
    [string] $ExpectedVersion = ""
)

# ---------------------------------------------------------------------------
# Test-DialSignature: returns $null on success, or a short ASCII error code on
# failure. Kept dependency-free so install-dial.ps1 can inline the same logic.
# ---------------------------------------------------------------------------
function Test-DialSignature {
    param(
        [string] $ExePath,
        [string] $CerPath,
        [string] $ExpectedVer
    )
    if (-not (Test-Path -LiteralPath $ExePath -PathType Leaf)) { return "exe-missing" }
    if (-not (Test-Path -LiteralPath $CerPath -PathType Leaf)) { return "cer-missing" }

    $anchor = $null
    try {
        $anchor = [System.Security.Cryptography.X509Certificates.X509Certificate2]::new($CerPath)
    } catch { return "cer-unreadable" }

    # 1) integrity via the OS Authenticode verifier (trust result ignored)
    $sig = $null
    try { $sig = Get-AuthenticodeSignature -LiteralPath $ExePath } catch { return "sig-check-failed" }
    $badStatus = @('HashMismatch','NotSigned','NotSupportedFileFormat','Incompatible','UnknownError')
    # UnknownError with a signer is common for self-signed (untrusted root); only
    # treat it as fatal when there is no usable signer certificate.
    if ($sig.Status -in @('HashMismatch','NotSigned','NotSupportedFileFormat','Incompatible')) {
        return "integrity:$($sig.Status)"
    }
    $signer = $sig.SignerCertificate
    if (-not $signer) { return "no-signer:$( $sig.Status )" }

    # 2) trust anchor = shipped certificate, compared byte-for-byte
    $anchorB64 = [Convert]::ToBase64String($anchor.RawData)
    $signerB64 = [Convert]::ToBase64String($signer.RawData)
    if ($anchorB64 -cne $signerB64) { return "cert-mismatch" }

    # 3) version binding: CN=dialc-sign-v<version> must equal expected version
    if ($ExpectedVer) {
        $m = [regex]::Match($anchor.Subject, 'CN\s*=\s*dialc-sign-v(\d+\.\d+\.\d+)')
        if (-not $m.Success) { return "cn-malformed" }
        if ($m.Groups[1].Value -ne $ExpectedVer) { return "version-mismatch:$($m.Groups[1].Value)!=$ExpectedVer" }
    }

    # 4) validity window: reject expired cert (NotBefore tolerated for clock skew)
    $now = Get-Date
    if ($now -gt $anchor.NotAfter) { return "cert-expired" }

    return $null
}

# ---------------------------------------------------------------------------
# Resolve expected version from BUILD_INFO.txt when not supplied.
# ---------------------------------------------------------------------------
function Get-ExpectedVersion {
    param([string] $ScriptDir)
    $candidates = @(
        (Join-Path $ScriptDir "BUILD_INFO.txt"),
        (Join-Path $ScriptDir "Dial\BUILD_INFO.txt")
    )
    foreach ($f in $candidates) {
        if (Test-Path -LiteralPath $f) {
            foreach ($line in (Get-Content -LiteralPath $f)) {
                # Match both "Version: 1.10.0" and the Chinese "版本: 1.10.0"
                $m = [regex]::Match($line, '(?:Version|版本)\s*[:：]\s*(\d+\.\d+\.\d+)')
                if ($m.Success) { return $m.Groups[1].Value }
            }
        }
    }
    return ""
}

# ---------------------------------------------------------------------------
# Standalone mode: resolve defaults relative to this script's directory.
# ---------------------------------------------------------------------------
$ScriptDir = if ($MyInvocation.MyCommand.Path) { Split-Path -Parent $MyInvocation.MyCommand.Path } else { (Get-Location).Path }
if (-not $Cer) { $Cer = Join-Path $ScriptDir "dialc-code-sign.cer" }
if (-not $ExpectedVersion) { $ExpectedVersion = Get-ExpectedVersion -ScriptDir $ScriptDir }

$err = Test-DialSignature -ExePath $Exe -CerPath $Cer -ExpectedVer $ExpectedVersion
if ($err) {
    Write-Host "  [X]  Signature verification FAILED: $err"
    exit 1
}
Write-Host "  [OK] Signature verified (cert bound, version $ExpectedVersion, in date)"
exit 0
