# Music Player Lite: one-command setup / update for Windows 10 / 11
# (Linux and macOS: setup.sh). Safe to run again after every `git pull`.
#   1. checks Rust, the Visual Studio C++ build tools, ffmpeg and yt-dlp and
#      installs what is missing with winget (asks first; -Yes skips asking),
#   2. builds the app (cargo build --release),
#   3. installs it to %LOCALAPPDATA%\Programs\Music Player Lite and adds a
#      "Music Player Lite" Start Menu shortcut (skip it with -NoShortcut).
# Run from PowerShell in the repository folder:
#   powershell -ExecutionPolicy Bypass -File .\setup.ps1 [-Check] [-Yes] [-NoShortcut] [-Uninstall]
param(
    [switch]$Check,
    [switch]$Yes,
    [switch]$NoShortcut,
    [switch]$Uninstall
)
$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent $MyInvocation.MyCommand.Path
Set-Location $Root

$AppName = 'Music Player Lite'
$MinRust = [version]'1.92.0'
$InstallDir = Join-Path $env:LOCALAPPDATA "Programs\$AppName"
$Shortcut = Join-Path ([Environment]::GetFolderPath('Programs')) "$AppName.lnk"

function Have($tool) { [bool](Get-Command $tool -ErrorAction SilentlyContinue) }
function Run($exe, [string[]]$arguments) {
    & $exe @arguments
    if ($LASTEXITCODE -ne 0) { throw "'$exe $($arguments -join ' ')' failed (exit $LASTEXITCODE)" }
}
function Refresh-Path {
    $machine = [Environment]::GetEnvironmentVariable('Path', 'Machine')
    $user = [Environment]::GetEnvironmentVariable('Path', 'User')
    $env:Path = "$machine;$user;$(Join-Path $env:USERPROFILE '.cargo\bin')"
}

if ($Uninstall) {
    Remove-Item -Force -ErrorAction SilentlyContinue $Shortcut
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $InstallDir
    Write-Host "Uninstalled. Your music and library database were not touched."
    exit 0
}

# --- Prerequisites -------------------------------------------------------------
Refresh-Path
$RustOk = $false
if ((Have 'cargo') -and (Have 'rustc')) {
    $v = ((& rustc --version) -split ' ')[1] -replace '-.*$', ''
    try { $RustOk = ([version]$v -ge $MinRust) } catch { $RustOk = $false }
}
$VsWhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
$HasVc = $false
if (Test-Path $VsWhere) {
    $HasVc = [bool](& $VsWhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath)
}

# name, test, winget arguments
$Missing = @()
if (-not $HasVc) {
    $Missing += , @('Visual Studio C++ build tools', @('install', '-e', '--id', 'Microsoft.VisualStudio.2022.BuildTools', '--override', '--wait --passive --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended'))
}
if (-not $RustOk) {
    if (Have 'rustup') {
        Write-Host '==> Updating Rust (rustup update stable)'
        if (-not $Check) { Run 'rustup' @('update', 'stable'); Run 'rustup' @('default', 'stable') }
    } else {
        $Missing += , @('Rust (rustup)', @('install', '-e', '--id', 'Rustlang.Rustup'))
    }
}
if (-not (Have 'ffmpeg')) { $Missing += , @('ffmpeg', @('install', '-e', '--id', 'Gyan.FFmpeg')) }
if (-not (Have 'yt-dlp')) { $Missing += , @('yt-dlp', @('install', '-e', '--id', 'yt-dlp.yt-dlp')) }

if ($Missing.Count -gt 0) {
    Write-Host 'Missing:' -ForegroundColor Yellow
    foreach ($m in $Missing) { Write-Host ("  {0}:  winget {1}" -f $m[0], ($m[1] -join ' ')) }
    if ($Check) { exit 1 }
    if (-not (Have 'winget')) {
        Write-Host 'winget is not available. Install "App Installer" from the Microsoft Store, or install the items above by hand.'
        exit 1
    }
    $answer = 'y'
    if (-not $Yes) { $answer = Read-Host 'Install them now with winget? [Y/n]' }
    if ($answer -and $answer -notmatch '^[yY]') { exit 1 }
    foreach ($m in $Missing) {
        Write-Host "==> Installing $($m[0])"
        & winget @($m[1] + @('--accept-package-agreements', '--accept-source-agreements'))
    }
    Refresh-Path
    if (Have 'rustup') { Run 'rustup' @('default', 'stable') }
    if (-not (Have 'cargo')) {
        Write-Host 'Rust was installed but is not on PATH yet. Close and reopen PowerShell, then run setup.ps1 again.'
        exit 1
    }
} else {
    Write-Host '==> Prerequisites OK'
}
if ($Check) { exit 0 }

# --- Build -------------------------------------------------------------------
Write-Host "==> Building $AppName (cargo build --release; the first build takes a few minutes)"
Run 'cargo' @('build', '--release', '--locked')
$TargetDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $Root 'target' }
$Built = Join-Path $TargetDir 'release\geonix-lite.exe'
if (-not (Test-Path $Built)) { throw "build finished but $Built is missing" }

# --- Install -------------------------------------------------------------------
Write-Host "==> Installing to $InstallDir"
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
$Exe = Join-Path $InstallDir 'geonix-lite.exe'
Copy-Item -Force $Built $Exe
Copy-Item -Force (Join-Path $Root 'launch.ps1') (Join-Path $InstallDir 'launch.ps1')

if (-not $NoShortcut) {
    $Shell = New-Object -ComObject WScript.Shell
    $Link = $Shell.CreateShortcut($Shortcut)
    $Link.TargetPath = $Exe
    $Link.WorkingDirectory = $InstallDir
    $Link.Description = $AppName
    $Link.Save()
    Write-Host "Start Menu shortcut: $Shortcut"
}

Write-Host ''
Write-Host "Done. Start $AppName from the Start Menu, or with:"
Write-Host '  powershell -ExecutionPolicy Bypass -File .\launch.ps1'
