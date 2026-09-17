# Publish a Windows release to the Windows fork: package the zip (via
# scripts\package-windows.ps1) and create the GitHub release the in-app
# updater reads (zeron-update fetches releases/latest from this repo).
#
# Usage: powershell -ExecutionPolicy Bypass -File scripts\release-windows.ps1 [-SkipBuild] [-Notes "..."] [-Draft]
# Requires: gh (authenticated, for the upload) and a bumped
# [workspace.package] version in Cargo.toml (the release tag is v<version>).

param(
    [string]$Repo = "R-ACU/zeron-custom",
    [switch]$SkipBuild,
    [string]$Notes = "Windows build.",
    [switch]$Draft
)

$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent $PSScriptRoot
$Version = (Select-String -Path "$Root\Cargo.toml" -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
$Arch = if ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture -eq "Arm64") { "aarch64" } else { "x86_64" }
$Zip = Join-Path $Root "target\package\zeron-$Version-windows-$Arch.zip"

Push-Location $Root
try {
    # "release not found" is the normal case here, but redirecting a native
    # command's stderr under $ErrorActionPreference = "Stop" turns that line
    # into a terminating NativeCommandError. Only the exit code matters.
    $previous = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    gh release view "v$Version" --repo $Repo *>$null
    $exists = ($LASTEXITCODE -eq 0)
    $ErrorActionPreference = $previous
    if ($exists) {
        throw "release v$Version already exists - bump [workspace.package] version in Cargo.toml first"
    }

    if ($SkipBuild) { $env:SKIP_BUILD = "1" }
    # Calling a .ps1 does not set $LASTEXITCODE (it would still hold the value
    # of the last native command), so the produced zip is the success check.
    # package-windows.ps1 runs with $ErrorActionPreference = "Stop" and throws
    # on its own failures.
    & "$PSScriptRoot\package-windows.ps1"
    if (-not (Test-Path $Zip)) { throw "expected package not found: $Zip" }

    $ghArgs = @("release", "create", "v$Version", "--repo", $Repo, "--title", "Zeron v$Version (Windows)", "--notes", $Notes)
    if ($Draft) { $ghArgs += "--draft" }
    $ghArgs += $Zip
    $url = gh @ghArgs
    if ($LASTEXITCODE -ne 0) { throw "gh release create failed" }
    Write-Host "Released: $url"
} finally {
    Pop-Location
}
