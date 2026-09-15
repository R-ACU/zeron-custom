# Windows packaging: build the release binary and produce
#   target\package\zeron-<version>-windows-<arch>.zip
# containing zeron.exe, the icon, the font licenses, THIRD_PARTY_NOTICES.md
# and install.ps1 (per-user install, no admin needed).
#
# Usage: powershell -ExecutionPolicy Bypass -File scripts\package-windows.ps1
# Env:   $env:PROFILE = "debug" for a fast unoptimized package; default release.
#        $env:SKIP_BUILD = "1" to package an already built binary.

$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent $PSScriptRoot
$Profile = if ($env:PROFILE) { $env:PROFILE } else { "release" }
$Arch = if ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture -eq "Arm64") { "aarch64" } else { "x86_64" }
$Version = (Select-String -Path "$Root\Cargo.toml" -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
$OutDir = Join-Path $Root "target\package"
$Stage = Join-Path $OutDir "zeron-$Version-windows-$Arch"
$Zip = "$Stage.zip"

Push-Location $Root
try {
    if ($Profile -eq "release") {
        if ($env:SKIP_BUILD -ne "1") { cargo build --release -p zeron; if ($LASTEXITCODE -ne 0) { throw "cargo build failed" } }
        $Bin = Join-Path $Root "target\release\zeron.exe"
    } else {
        if ($env:SKIP_BUILD -ne "1") { cargo build -p zeron; if ($LASTEXITCODE -ne 0) { throw "cargo build failed" } }
        $Bin = Join-Path $Root "target\debug\zeron.exe"
    }

    if (-not (Test-Path $Bin)) { throw "binary not found: $Bin" }
    if (Test-Path $Stage) { Remove-Item -Recurse -Force $Stage }
    if (Test-Path $Zip) { Remove-Item -Force $Zip }
    New-Item -ItemType Directory -Force -Path (Join-Path $Stage "licenses\fonts") | Out-Null
    Copy-Item $Bin (Join-Path $Stage "zeron.exe")
    Copy-Item (Join-Path $Root "dist\zeron.ico") (Join-Path $Stage "zeron.ico")
    Copy-Item (Join-Path $Root "dist\zeron.png") (Join-Path $Stage "zeron.png")
    Copy-Item (Join-Path $Root "LICENSE") (Join-Path $Stage "LICENSE")
    Copy-Item (Join-Path $Root "THIRD_PARTY_NOTICES.md") (Join-Path $Stage "THIRD_PARTY_NOTICES.md")
    Copy-Item (Join-Path $Root "crates\ui\assets\fonts\licenses\*") (Join-Path $Stage "licenses\fonts\")
    Copy-Item (Join-Path $Root "scripts\install.ps1") (Join-Path $Stage "install.ps1")

    Compress-Archive -Path "$Stage\*" -DestinationPath $Zip -CompressionLevel Optimal
    Write-Host "Packaged $Zip"
} finally {
    Pop-Location
}
