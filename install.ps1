#Requires -Version 5.1
# spelunk installer for Windows — https://spelunk.cloud
#
# Standard install:
#   irm https://raw.githubusercontent.com/spelunk-cloud/spelunk/refs/heads/main/install.ps1 | iex
#
# Dry-run (preview without installing):
#   & ([scriptblock]::Create((irm https://raw.githubusercontent.com/spelunk-cloud/spelunk/refs/heads/main/install.ps1))) -DryRun
[CmdletBinding()]
param(
    [switch]$DryRun
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$REPO = 'spelunk-cloud/spelunk'

# ── Arch detection ─────────────────────────────────────────────────────────────
$arch = [System.Runtime.InteropServices.RuntimeInformation]::ProcessArchitecture
switch ($arch) {
    'X64'   { $TARGET = 'x86_64-pc-windows-msvc' }
    'Arm64' {
        Write-Error "spelunk does not yet ship a prebuilt binary for Windows ARM64. Please build from source: https://github.com/$REPO"
        exit 1
    }
    default {
        Write-Error "Unsupported architecture: $arch. Please build from source: https://github.com/$REPO"
        exit 1
    }
}

# ── Resolve latest version tag ─────────────────────────────────────────────────
$apiUrl = "https://api.github.com/repos/$REPO/releases/latest"
try {
    $release = Invoke-RestMethod -Uri $apiUrl -UseBasicParsing -Headers @{ 'User-Agent' = 'spelunk-installer' }
    $VERSION = $release.tag_name
} catch {
    Write-Error "Could not determine latest spelunk version: $_"
    exit 1
}

if (-not $VERSION) {
    Write-Error 'Could not determine latest spelunk version.'
    exit 1
}

$ZIPNAME     = "spelunk-${VERSION}-${TARGET}.zip"
$DOWNLOAD_URL = "https://github.com/$REPO/releases/download/$VERSION/$ZIPNAME"

# ── Install directory ──────────────────────────────────────────────────────────
# Default: %LOCALAPPDATA%\Programs\spelunk — writable without elevation, on a
# per-user PATH that we can extend ourselves.
$INSTALL_DIR = Join-Path $env:LOCALAPPDATA 'Programs\spelunk'

# ── Summary ────────────────────────────────────────────────────────────────────
Write-Host ''
Write-Host '  spelunk installer'
Write-Host '  ─────────────────────────────────────────'
Write-Host "  Arch     : $arch"
Write-Host "  Version  : $VERSION"
Write-Host "  Target   : $TARGET"
Write-Host "  Download : $DOWNLOAD_URL"
Write-Host "  Install  : $INSTALL_DIR\spelunk.exe"
Write-Host "             $INSTALL_DIR\spelunk-server.exe"
Write-Host ''

if ($DryRun) {
    Write-Host '  Dry-run mode — nothing was installed.'
    Write-Host ''
    exit 0
}

# ── Download and extract ───────────────────────────────────────────────────────
$TMP_DIR = Join-Path $env:TEMP ("spelunk-install-{0}" -f [System.IO.Path]::GetRandomFileName())
New-Item -ItemType Directory -Path $TMP_DIR | Out-Null

try {
    $zipPath = Join-Path $TMP_DIR $ZIPNAME
    Write-Host "Downloading $ZIPNAME ..."
    Invoke-WebRequest -Uri $DOWNLOAD_URL -OutFile $zipPath -UseBasicParsing

    Write-Host 'Extracting ...'
    Expand-Archive -Path $zipPath -DestinationPath $TMP_DIR -Force

    # ── Install binaries ───────────────────────────────────────────────────────
    if (-not (Test-Path $INSTALL_DIR)) {
        New-Item -ItemType Directory -Path $INSTALL_DIR | Out-Null
    }

    foreach ($bin in @('spelunk.exe', 'spelunk-server.exe')) {
        $src = Join-Path $TMP_DIR $bin
        if (Test-Path $src) {
            Copy-Item -Path $src -Destination (Join-Path $INSTALL_DIR $bin) -Force
            Write-Host "Installed $bin -> $INSTALL_DIR\$bin"
        }
    }
} finally {
    Remove-Item -Recurse -Force $TMP_DIR -ErrorAction SilentlyContinue
}

# ── PATH guidance ──────────────────────────────────────────────────────────────
$userPath = [System.Environment]::GetEnvironmentVariable('PATH', 'User')
if ($userPath -notlike "*$INSTALL_DIR*") {
    Write-Host ''
    Write-Host "  Note: $INSTALL_DIR is not in your PATH."

    # Offer to add it permanently to the user-level PATH in the registry.
    $newPath = $INSTALL_DIR + ';' + $userPath
    [System.Environment]::SetEnvironmentVariable('PATH', $newPath, 'User')
    Write-Host "  Added $INSTALL_DIR to your user PATH (takes effect in new terminals)."
    Write-Host ''
    Write-Host '  To use spelunk in this session, run:'
    Write-Host "    `$env:PATH = `"$INSTALL_DIR;`$env:PATH`""
    Write-Host ''
}

# ── Verify ─────────────────────────────────────────────────────────────────────
Write-Host ''
$spelunkExe = Join-Path $INSTALL_DIR 'spelunk.exe'
if (Test-Path $spelunkExe) {
    & $spelunkExe --version
}
Write-Host ''
Write-Host 'spelunk installed successfully. Run `spelunk init` to get started.'
Write-Host ''
