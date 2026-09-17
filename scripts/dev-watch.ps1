# Dev loop: rebuild the debug binary on every source change and restart a
# second Zeron instance that lives next to the installed app.
#
#   powershell -ExecutionPolicy Bypass -File scripts\dev-watch.ps1
#
# The dev instance uses its own data dir and IPC port and skips the
# single-instance guard, so the installed Zeron keeps running untouched.
# Stop with Ctrl+C; the dev instance is closed with the watcher.

$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent $PSScriptRoot
$env:ZERON_DATA_DIR = Join-Path $env:USERPROFILE ".zeron-dev"
$env:ZERON_IPC_PORT = "27655"
$env:ZERON_ALLOW_SECOND_INSTANCE = "1"
$env:ZERON_TOAST_APP_ID = "powershell"   # dev toasts must not hijack the installed identity

$Restart = Join-Path $PSScriptRoot "dev-restart.ps1"
# cargo-watch runs its command through cmd.exe, whose PATH may lack cargo.
$CargoBin = Join-Path (Join-Path $env:USERPROFILE ".cargo") "bin"
$env:Path = "$CargoBin;" + $env:Path
$Cargo = Join-Path $CargoBin "cargo.exe"
if (-not (Get-Command cargo-watch -ErrorAction SilentlyContinue)) {
    throw "cargo-watch missing: cargo install cargo-watch --locked"
}

Write-Host "Dev loop: data=$env:ZERON_DATA_DIR port=$env:ZERON_IPC_PORT"
Set-Location $Root
try {
    cargo watch --why --ignore "target/**" --ignore "**/*.md" --ignore "scripts/**" `
        -s "`"$Cargo`" build -p zeron -j 6 && powershell -NoProfile -ExecutionPolicy Bypass -File `"$Restart`""
} finally {
    & $Restart -StopOnly
}
