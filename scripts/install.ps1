# Install Zeron for the current user (no admin needed).
#   - copies zeron.exe to %LOCALAPPDATA%\Programs\Zeron
#   - adds that folder to the user PATH
#   - creates Start Menu and Desktop shortcuts
#   - registers the zeron:// URL scheme (HKCU)
#   - optionally installs the background engine as a logon task (-Daemon)
#
# Usage: powershell -ExecutionPolicy Bypass -File install.ps1 [-Daemon] [-NoDesktopShortcut]
# Run from the extracted package folder (next to zeron.exe) or from the repo
# (then it picks target\release\zeron.exe).

param(
    [switch]$Daemon,
    [switch]$NoDesktopShortcut
)

$ErrorActionPreference = "Stop"
$Here = $PSScriptRoot
$Source = Join-Path $Here "zeron.exe"
if (-not (Test-Path $Source)) {
    $Root = Split-Path -Parent $Here
    $Source = Join-Path $Root "target\release\zeron.exe"
    $IconSource = Join-Path $Root "dist\zeron.ico"
} else {
    $IconSource = Join-Path $Here "zeron.ico"
}
if (-not (Test-Path $Source)) { throw "zeron.exe not found next to install.ps1 or in target\release; build first." }

$Dest = Join-Path $env:LOCALAPPDATA "Programs\Zeron"
New-Item -ItemType Directory -Force -Path $Dest | Out-Null
$Exe = Join-Path $Dest "zeron.exe"

# A running instance keeps the exe locked: ask the engine to stop first.
if (Test-Path $Exe) {
    $running = Get-Process -Name zeron -ErrorAction SilentlyContinue
    if ($running) {
        Write-Host "Stopping running Zeron instances..."
        try { & $Exe daemon stop 2>$null | Out-Null } catch {}
        $running | Stop-Process -Force -ErrorAction SilentlyContinue
        Start-Sleep -Seconds 1
    }
}
Copy-Item $Source $Exe -Force
if (Test-Path $IconSource) { Copy-Item $IconSource (Join-Path $Dest "zeron.ico") -Force }
foreach ($name in @("LICENSE", "THIRD_PARTY_NOTICES.md")) {
    $f = Join-Path $Here $name
    if (Test-Path $f) { Copy-Item $f (Join-Path $Dest $name) -Force }
}
$lic = Join-Path $Here "licenses"
if (Test-Path $lic) { Copy-Item $lic (Join-Path $Dest "licenses") -Recurse -Force }

# User PATH
$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
if (-not ($userPath -split ";" | Where-Object { $_ -ieq $Dest })) {
    [Environment]::SetEnvironmentVariable("Path", (($userPath.TrimEnd(";")) + ";" + $Dest), "User")
    Write-Host "Added $Dest to the user PATH (new terminals pick it up)."
}

# Shortcuts (Start Menu one carries the AppUserModelID used for toast notifications)
$shell = New-Object -ComObject WScript.Shell
$startMenu = Join-Path ([Environment]::GetFolderPath("Programs")) "Zeron.lnk"
$lnk = $shell.CreateShortcut($startMenu)
$lnk.TargetPath = $Exe
$lnk.WorkingDirectory = $Dest
$lnk.IconLocation = (Join-Path $Dest "zeron.ico")
$lnk.Description = "Zeron: control plane for coding agents"
$lnk.Save()
if (-not $NoDesktopShortcut) {
    $desktop = Join-Path ([Environment]::GetFolderPath("Desktop")) "Zeron.lnk"
    $lnk = $shell.CreateShortcut($desktop)
    $lnk.TargetPath = $Exe
    $lnk.WorkingDirectory = $Dest
    $lnk.IconLocation = (Join-Path $Dest "zeron.ico")
    $lnk.Description = "Zeron: control plane for coding agents"
    $lnk.Save()
}

# zeron:// URL scheme (per user, no elevation)
$cls = "HKCU:\Software\Classes\zeron"
New-Item -Path $cls -Force | Out-Null
Set-ItemProperty -Path $cls -Name "(Default)" -Value "URL:Zeron Protocol"
Set-ItemProperty -Path $cls -Name "URL Protocol" -Value ""
New-Item -Path "$cls\DefaultIcon" -Force | Out-Null
Set-ItemProperty -Path "$cls\DefaultIcon" -Name "(Default)" -Value "`"$Exe`",0"
New-Item -Path "$cls\shell\open\command" -Force | Out-Null
Set-ItemProperty -Path "$cls\shell\open\command" -Name "(Default)" -Value "`"$Exe`" --open-url `"%1`""

# WebView2 runtime (needed for the built-in browser tab; Windows 11 ships it)
$wv = Get-ItemProperty "HKLM:\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}" -ErrorAction SilentlyContinue
if (-not $wv) { $wv = Get-ItemProperty "HKCU:\SOFTWARE\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}" -ErrorAction SilentlyContinue }
if ($wv) {
    Write-Host "WebView2 runtime $($wv.pv) found."
} else {
    Write-Host "WebView2 runtime not found. The browser tab needs it: https://go.microsoft.com/fwlink/p/?LinkId=2124703"
}

if ($Daemon) {
    & $Exe daemon install
}

Write-Host "Installed $Exe"
Write-Host "Start Zeron from the Desktop or Start Menu shortcut, or run 'zeron' in a new terminal."
