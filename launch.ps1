# Start Music Player Lite on Windows (Linux and macOS: launch.sh).
# Uses the installed copy (setup.ps1) or a build in this folder, updates
# yt-dlp at most once a week, and passes all arguments on, e.g.
#   powershell -ExecutionPolicy Bypass -File .\launch.ps1 --headless
# runs only the server (phone page, device sync) without a window.
# Settings come from GEONIX_* environment variables (see README).
$ErrorActionPreference = 'Stop'
$Here = Split-Path -Parent $MyInvocation.MyCommand.Path

$Candidates = @()
if ($env:GEONIX_LITE_BIN) { $Candidates += $env:GEONIX_LITE_BIN }
$Candidates += (Join-Path $Here 'geonix-lite.exe')
$Candidates += (Join-Path $env:LOCALAPPDATA 'Programs\Music Player Lite\geonix-lite.exe')
$TargetDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $Here 'target' }
$Candidates += (Join-Path $TargetDir 'release\geonix-lite.exe')
$Bin = $Candidates | Where-Object { $_ -and (Test-Path $_) } | Select-Object -First 1
if (-not $Bin) {
    Write-Error 'Music Player Lite is not built yet. Run setup.ps1 first.'
    exit 1
}

# YouTube changes often and old yt-dlp versions start failing (HTTP 403):
# update it in the background at most once a week.
$DataDir = if ($env:GEONIX_DATA_DIR) { $env:GEONIX_DATA_DIR } else { Join-Path $env:LOCALAPPDATA 'geonix-lite' }
New-Item -ItemType Directory -Force -Path $DataDir | Out-Null
$Stamp = Join-Path $DataDir '.yt-dlp-updated'
$Fresh = (Test-Path $Stamp) -and ((Get-Item $Stamp).LastWriteTime -gt (Get-Date).AddDays(-7))
if (-not $Fresh -and (Get-Command winget -ErrorAction SilentlyContinue)) {
    $cmd = "winget upgrade -e --id yt-dlp.yt-dlp --silent --accept-package-agreements --accept-source-agreements; Set-Content -Path '$Stamp' -Value (Get-Date)"
    Start-Process -WindowStyle Hidden -FilePath (Join-Path $PSHOME 'powershell.exe') -ArgumentList '-NoProfile', '-Command', $cmd | Out-Null
}

if ($args.Count -gt 0) {
    Start-Process -FilePath $Bin -WorkingDirectory (Split-Path -Parent $Bin) -ArgumentList $args
} else {
    Start-Process -FilePath $Bin -WorkingDirectory (Split-Path -Parent $Bin)
}
