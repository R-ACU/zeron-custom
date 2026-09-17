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
    $PngSource = Join-Path $Root "dist\zeron.png"
} else {
    $IconSource = Join-Path $Here "zeron.ico"
    $PngSource = Join-Path $Here "zeron.png"
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
# The toast icon: the notification platform takes png/jpg/gif, an ico only
# sometimes, so ship both next to the exe (crates\ui\src\notify.rs picks).
if (Test-Path $PngSource) { Copy-Item $PngSource (Join-Path $Dest "zeron.png") -Force }
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

# Shortcuts. The Start Menu one carries the AppUserModelID toasts are posted
# under; WScript.Shell cannot write that property, it lives in the .lnk's
# IPropertyStore (System.AppUserModel.ID), so a small interop helper stamps it
# afterwards. Zeron also registers the AUMID itself at startup (HKCU
# Software\Classes\AppUserModelId\Zeron.Desktop), so a failure here is cosmetic.
$Aumid = "Zeron.Desktop"
$stampType = $null
try {
    $stampType = Add-Type -PassThru -ErrorAction Stop -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

public static class ZeronShortcutId
{
    [ComImport, Guid("886d8eeb-8cf2-4446-8d02-cdba1dbdcf99"),
     InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    private interface IPropertyStore
    {
        int GetCount(out uint cProps);
        int GetAt(uint iProp, out PROPERTYKEY pkey);
        int GetValue(ref PROPERTYKEY key, out PROPVARIANT pv);
        int SetValue(ref PROPERTYKEY key, ref PROPVARIANT pv);
        int Commit();
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct PROPERTYKEY { public Guid fmtid; public uint pid; }

    // vt + 3 reserved words, then the 16-byte union.
    [StructLayout(LayoutKind.Sequential)]
    private struct PROPVARIANT
    {
        public ushort vt;
        public ushort r1, r2, r3;
        public IntPtr value;
        public IntPtr unused;
    }

    [DllImport("shell32.dll", CharSet = CharSet.Unicode, PreserveSig = false)]
    private static extern void SHGetPropertyStoreFromParsingName(
        string path, IntPtr bindContext, int flags, ref Guid riid,
        [MarshalAs(UnmanagedType.Interface)] out IPropertyStore store);

    public static void Stamp(string shortcutPath, string appId)
    {
        Guid iid = typeof(IPropertyStore).GUID;
        IPropertyStore store;
        // GPS_READWRITE = 2
        SHGetPropertyStoreFromParsingName(shortcutPath, IntPtr.Zero, 2, ref iid, out store);
        PROPERTYKEY key = new PROPERTYKEY();
        key.fmtid = new Guid("9F4C2855-9F79-4B39-A8D0-E1D42DE1D5F3");
        key.pid = 5;
        PROPVARIANT pv = new PROPVARIANT();
        pv.vt = 31; // VT_LPWSTR
        pv.value = Marshal.StringToCoTaskMemUni(appId);
        try
        {
            Marshal.ThrowExceptionForHR(store.SetValue(ref key, ref pv));
            Marshal.ThrowExceptionForHR(store.Commit());
        }
        finally
        {
            Marshal.FreeCoTaskMem(pv.value);
            Marshal.ReleaseComObject(store);
        }
    }
}
'@
} catch {
    Write-Host "Could not build the shortcut AppUserModelID helper: $($_.Exception.Message)"
}

function Set-ShortcutAppId([string]$Path) {
    if (-not $stampType) { return }
    try {
        [ZeronShortcutId]::Stamp($Path, $Aumid)
        Write-Host "Stamped $Path with AppUserModelID $Aumid."
    } catch {
        Write-Host "Could not stamp $Path with the AppUserModelID: $($_.Exception.Message)"
    }
}

$shell = New-Object -ComObject WScript.Shell
$startMenu = Join-Path ([Environment]::GetFolderPath("Programs")) "Zeron.lnk"
$lnk = $shell.CreateShortcut($startMenu)
$lnk.TargetPath = $Exe
$lnk.WorkingDirectory = $Dest
$lnk.IconLocation = (Join-Path $Dest "zeron.ico")
$lnk.Description = "Zeron: control plane for coding agents"
$lnk.Save()
Set-ShortcutAppId $startMenu
if (-not $NoDesktopShortcut) {
    $desktop = Join-Path ([Environment]::GetFolderPath("Desktop")) "Zeron.lnk"
    $lnk = $shell.CreateShortcut($desktop)
    $lnk.TargetPath = $Exe
    $lnk.WorkingDirectory = $Dest
    $lnk.IconLocation = (Join-Path $Dest "zeron.ico")
    $lnk.Description = "Zeron: control plane for coding agents"
    $lnk.Save()
    Set-ShortcutAppId $desktop
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
