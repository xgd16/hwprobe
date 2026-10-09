# Fetch the third-party binary assets that are deliberately not committed.
#
# Only one asset qualifies today: the official PawnIO installer, which the DLL
# embeds and `hwprobe_setup_driver()` / `hwprobe-cli setup` run to install the
# kernel driver. It is a signed 3.4 MB executable published by the PawnIO
# project, so it is mirrored on demand rather than vendored into git.
#
# Every download is checked against the SHA-256 recorded next to the asset, and
# the Authenticode signature is verified too. A mismatch is a hard failure -
# never install a kernel driver from an unverified file.
#
# This file is intentionally ASCII-only: Windows PowerShell 5.1 decodes a
# BOM-less .ps1 as the ANSI code page, which mangles non-ASCII string literals
# and can even break parsing.
#
# Usage:
#   powershell -ExecutionPolicy Bypass -File tool\fetch_assets.ps1
#   powershell -ExecutionPolicy Bypass -File tool\fetch_assets.ps1 -Force
#   powershell -ExecutionPolicy Bypass -File tool\fetch_assets.ps1 -VerifyOnly

[CmdletBinding()]
param(
    [switch]$Force,
    [switch]$VerifyOnly
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot
$assetsDir = Join-Path $repoRoot 'crates\hwprobe-core\assets'

# --- the asset table -------------------------------------------------------
# url / sha256 / size are kept in sync with PawnIO_setup.README in the same
# directory. Update both together when bumping the version.
$assets = @(
    @{
        Name       = 'PawnIO_setup.exe'
        Url        = 'https://github.com/namazso/PawnIO.Setup/releases/download/2.2.0/PawnIO_setup.exe'
        Sha256     = '1F519A22E47187F70A1379A48CA604981C4FCF694F4E65B734AAA74A9FBA3032'
        Size       = 3410960
        SignerCN   = 'namazso.eu'
        Thumbprint = 'F380DCC9F706E2756A5047B832FFE719E1BC35F5'
    }
)

function Get-HashOf {
    param([Parameter(Mandatory = $true)][string]$LiteralPath)
    return (Get-FileHash -LiteralPath $LiteralPath -Algorithm SHA256).Hash
}

function Get-AssetState {
    # Returns 'ok', 'missing' or 'mismatch'.
    param([Parameter(Mandatory = $true)][hashtable]$Asset)

    $path = Join-Path $assetsDir $Asset.Name
    if (-not (Test-Path -LiteralPath $path)) { return 'missing' }

    $actual = Get-HashOf -LiteralPath $path
    if ($actual -ne $Asset.Sha256) {
        Write-Warning ("{0}: SHA-256 mismatch (expected {1}, got {2})" -f $Asset.Name, $Asset.Sha256, $actual)
        return 'mismatch'
    }
    return 'ok'
}

if (-not (Test-Path -LiteralPath $assetsDir)) {
    throw "asset directory not found: $assetsDir"
}

$failed = $false

foreach ($asset in $assets) {
    $path = Join-Path $assetsDir $asset.Name
    $state = Get-AssetState -Asset $asset

    if ($VerifyOnly) {
        switch ($state) {
            'ok'       { Write-Host ("OK        {0}" -f $asset.Name) -ForegroundColor Green }
            'missing'  { Write-Host ("MISSING   {0}" -f $asset.Name) -ForegroundColor Yellow; $failed = $true }
            'mismatch' { Write-Host ("MISMATCH  {0}" -f $asset.Name) -ForegroundColor Red;    $failed = $true }
        }
        continue
    }

    if ($state -eq 'ok' -and -not $Force) {
        Write-Host ("present   {0} (SHA-256 ok, skipped; -Force to re-download)" -f $asset.Name) -ForegroundColor DarkGray
        continue
    }

    if ($state -eq 'mismatch') {
        Write-Warning ("{0} exists but hashes wrong; downloading again." -f $asset.Name)
    }

    Write-Host ("downloading {0}" -f $asset.Name) -ForegroundColor Cyan
    Write-Host ("            {0}" -f $asset.Url) -ForegroundColor DarkGray

    # Download beside the target so a failed or truncated transfer can never
    # leave a half-written file where the build expects a verified one.
    $temp = "$path.download"
    if (Test-Path -LiteralPath $temp) { Remove-Item -LiteralPath $temp -Force }

    try {
        $progress = $ProgressPreference
        $ProgressPreference = 'SilentlyContinue'   # the default bar makes this crawl
        try {
            Invoke-WebRequest -Uri $asset.Url -OutFile $temp -UseBasicParsing
        }
        finally {
            $ProgressPreference = $progress
        }

        $actual = Get-HashOf -LiteralPath $temp
        if ($actual -ne $asset.Sha256) {
            throw ("SHA-256 mismatch: expected {0}, got {1}" -f $asset.Sha256, $actual)
        }

        $size = (Get-Item -LiteralPath $temp).Length
        if ($size -ne $asset.Size) {
            throw ("size mismatch: expected {0} bytes, got {1} bytes" -f $asset.Size, $size)
        }

        if ($asset.SignerCN) {
            $sig = Get-AuthenticodeSignature -LiteralPath $temp
            if ($sig.Status -ne 'Valid') {
                throw ("Authenticode signature is not valid (Status={0})" -f $sig.Status)
            }
            $subject = $sig.SignerCertificate.Subject
            if ($subject -notmatch [regex]::Escape($asset.SignerCN)) {
                throw ("signer is not {0}: {1}" -f $asset.SignerCN, $subject)
            }
            if ($asset.Thumbprint -and $sig.SignerCertificate.Thumbprint -ne $asset.Thumbprint) {
                throw ("thumbprint mismatch: expected {0}, got {1}" -f $asset.Thumbprint, $sig.SignerCertificate.Thumbprint)
            }
            Write-Host ("            signature valid, publisher {0}" -f $asset.SignerCN) -ForegroundColor DarkGray
        }

        Move-Item -LiteralPath $temp -Destination $path -Force
        Write-Host ("done      {0}" -f $asset.Name) -ForegroundColor Green
    }
    catch {
        if (Test-Path -LiteralPath $temp) {
            Remove-Item -LiteralPath $temp -Force -ErrorAction SilentlyContinue
        }
        Write-Error ("{0}: fetch failed - {1}" -f $asset.Name, $_.Exception.Message)
        $failed = $true
    }
}

Write-Host ''
if ($failed) {
    Write-Host 'Some assets are not ready.' -ForegroundColor Yellow
    Write-Host 'The DLL still builds without them, but hwprobe_setup_driver() will fail and' -ForegroundColor Yellow
    Write-Host 'driver-backed sensors (CPU temperature/power, board fans/voltage) degrade to NO_DRIVER.' -ForegroundColor Yellow
    exit 1
}

Write-Host 'All third-party binary assets are present and verified.' -ForegroundColor Green
