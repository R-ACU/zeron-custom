# Restart the dev Zeron instance (called by dev-watch.ps1 after each build).
# Kills only the process started from target\debug\zeron.exe, never the
# installed app. -StopOnly closes it without starting a new one.
param([switch]$StopOnly)

$Root = Split-Path -Parent $PSScriptRoot
$Built = Join-Path $Root "target\debug\zeron.exe"
# The dev instance runs from a COPY, so cargo can always replace the built
# exe while the dev window is open (a running exe cannot be overwritten).
$RunDir = Join-Path (Join-Path $env:USERPROFILE ".zeron-dev") "bin"
$Exe = Join-Path $RunDir "zeron.exe"
$PidFile = Join-Path $env:TEMP "zeron-dev.pid"

if (Test-Path $PidFile) {
    $old = Get-Content $PidFile -ErrorAction SilentlyContinue
    if ($old) {
        $p = Get-Process -Id $old -ErrorAction SilentlyContinue
        if ($p -and $p.Path -ieq $Exe) {
            Stop-Process -Id $old -Force -ErrorAction SilentlyContinue
            Start-Sleep -Milliseconds 800
        }
    }
    Remove-Item $PidFile -Force -ErrorAction SilentlyContinue
}
if ($StopOnly) { return }
if (-not (Test-Path $Built)) { Write-Host "no debug exe yet"; return }
New-Item -ItemType Directory -Force -Path $RunDir | Out-Null
Copy-Item $Built $Exe -Force
foreach ($f in @("zeron.png", "zeron.ico")) { $src = Join-Path $Root "dist\$f"; if (Test-Path $src) { Copy-Item $src (Join-Path $RunDir $f) -Force } }

$proc = Start-Process -FilePath $Exe -WorkingDirectory $Root -PassThru
Set-Content -Path $PidFile -Value $proc.Id
Write-Host ("dev instance restarted, pid {0}" -f $proc.Id)
