# Installs a prebuilt coder.exe from GitHub releases: detects arch, downloads the
# matching asset and its SHA256SUMS.txt, verifies the checksum, installs it, and
# checks for podman.
#
# Usage: irm <raw-url-to-this-script> | iex
# Override the version with: $env:CODER_VERSION = "0.1.0"; irm ... | iex

$ErrorActionPreference = "Stop"

$Repo = "DevBytAmir/coder"
$InstallDir = if ($env:CODER_INSTALL_DIR) { $env:CODER_INSTALL_DIR } else { "$env:LOCALAPPDATA\coder\bin" }

switch ([System.Runtime.InteropServices.RuntimeInformation]::ProcessArchitecture) {
    "X64" { $Arch = "x86_64" }
    "Arm64" { $Arch = "arm64" }
    default { throw "unsupported architecture: $_" }
}
$Asset = "coder-windows-$Arch.exe"

if ($env:CODER_VERSION) {
    $Tag = "v$($env:CODER_VERSION)"
} else {
    Write-Host "looking up the latest release..."
    $Release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest"
    $Tag = $Release.tag_name
    if (-not $Tag) { throw "could not determine the latest release tag" }
}

$BaseUrl = "https://github.com/$Repo/releases/download/$Tag"
$WorkDir = Join-Path $env:TEMP "coder-install-$(Get-Random)"
New-Item -ItemType Directory -Path $WorkDir | Out-Null

try {
    Write-Host "downloading $Asset ($Tag)..."
    $AssetPath = Join-Path $WorkDir $Asset
    Invoke-WebRequest -Uri "$BaseUrl/$Asset" -OutFile $AssetPath
    $SumsPath = Join-Path $WorkDir "SHA256SUMS.txt"
    Invoke-WebRequest -Uri "$BaseUrl/SHA256SUMS.txt" -OutFile $SumsPath

    Write-Host "verifying checksum..."
    $Line = Select-String -Path $SumsPath -Pattern " $Asset$" | Select-Object -First 1
    if (-not $Line) { throw "$Asset is not listed in SHA256SUMS.txt" }
    $Expected = ($Line.Line -split "\s+")[0]
    $Actual = (Get-FileHash -Path $AssetPath -Algorithm SHA256).Hash.ToLower()
    if ($Expected -ne $Actual) {
        throw "checksum mismatch for $Asset`: expected $Expected, got $Actual"
    }

    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    Copy-Item -Path $AssetPath -Destination (Join-Path $InstallDir "coder.exe") -Force
    Write-Host "installed coder to $InstallDir\coder.exe"
} finally {
    Remove-Item -Path $WorkDir -Recurse -Force -ErrorAction SilentlyContinue
}

if (-not ($env:Path -split ";" -contains $InstallDir)) {
    Write-Host ""
    Write-Host "warning: $InstallDir is not on your PATH. Add it with:"
    Write-Host "  [Environment]::SetEnvironmentVariable('Path', `"`$env:Path;$InstallDir`", 'User')"
}

Write-Host ""
if (Get-Command podman -ErrorAction SilentlyContinue) {
    Write-Host "podman: found ($(podman --version))"
} else {
    Write-Host "podman: not found. coder needs it to run sandboxed agents."
    Write-Host "  Install Podman Desktop: https://podman-desktop.io/"
}

Write-Host ""
Write-Host "Next steps:"
Write-Host "  coder build --fetch   # or --source-dir <path> if you have a checkout"
Write-Host "  coder init"
