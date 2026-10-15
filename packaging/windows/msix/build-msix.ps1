# Packs the Microsoft Store package (.msix) from a Windows release build
# (cargo build --release -p diskrescue -p diskrescue-tui -p diskrescue-gui).
# It is not signed: the Store signs it. The identity defaults to DiskRescue's
# Partner Center reservation; CI passes a test identity. -Arch is the
# architecture of the programs in -Bin (x64 or arm64). Needs the Windows SDK
# (makeappx, makepri).
#
#   packaging\windows\msix\build-msix.ps1 [-Arch x64] [-Bin target\release] [-Out dist\DiskRescue-<version>-<arch>.msix]
param(
    [string]$Name = 'Dhiva-Labs.DiskRescue',
    [string]$Publisher = 'CN=0F69015A-AA5B-431D-970A-165099309E53',
    [ValidateSet('x64', 'arm64')][string]$Arch = 'x64',
    [string]$Bin,
    [string]$Out
)
$ErrorActionPreference = 'Stop'

# Runs a program. (Windows PowerShell 5.1 turns a program's stderr output into
# errors, which 'Stop' would make fatal, so the exit code decides instead.)
function Run([string]$what, [scriptblock]$command) {
    $ErrorActionPreference = 'Continue'
    & $command 2>&1 | ForEach-Object { "$_" }
    if ($LASTEXITCODE) { throw "$what failed" }
}

$root = Resolve-Path (Join-Path $PSScriptRoot '..\..\..')
$version = (Select-String -Path (Join-Path $root 'Cargo.toml') -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
if (-not $Bin) { $Bin = Join-Path $root 'target\release' }
if (-not $Out) { $Out = Join-Path $root "dist\DiskRescue-$version-$Arch.msix" }

# The SDK's x64 tools, or its ARM64 ones on a machine that only has those.
$sdk = Get-ChildItem 'C:\Program Files (x86)\Windows Kits\10\bin' -Directory |
    Where-Object Name -match '^10\.' | Sort-Object { [version]$_.Name } -Descending |
    ForEach-Object { (Join-Path $_.FullName 'x64'), (Join-Path $_.FullName 'arm64') } |
    Where-Object { Test-Path (Join-Path $_ 'makeappx.exe') } | Select-Object -First 1
if (-not $sdk) { throw 'makeappx.exe not found: install the Windows SDK' }

$stage = Join-Path ([IO.Path]::GetTempPath()) "diskrescue-msix-$PID"
if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
New-Item -ItemType Directory $stage | Out-Null
try {
    foreach ($exe in 'diskrescue.exe', 'diskrescue-tui.exe', 'diskrescue-gui.exe') {
        Copy-Item (Join-Path $Bin $exe) $stage
    }
    Copy-Item (Join-Path $root 'LICENSE') (Join-Path $stage 'LICENSE.txt')
    Copy-Item -Recurse (Join-Path $PSScriptRoot 'Assets') $stage
    $manifest = (Get-Content (Join-Path $PSScriptRoot 'AppxManifest.xml.in') -Raw).
        Replace('@NAME@', $Name).Replace('@PUBLISHER@', $Publisher).Replace('@VERSION@', "$version.0").
        Replace('@ARCH@', $Arch)
    [IO.File]::WriteAllText((Join-Path $stage 'AppxManifest.xml'), $manifest)

    # Resource index, so Windows picks the right image size for each place.
    Push-Location $stage
    try {
        Run 'makepri createconfig' { & "$sdk\makepri.exe" createconfig /cf priconfig.xml /dq en-US /o }
        Run 'makepri new' { & "$sdk\makepri.exe" new /pr . /cf priconfig.xml /of resources.pri /o }
        Remove-Item priconfig.xml
    } finally { Pop-Location }

    New-Item -ItemType Directory -Force (Split-Path $Out) | Out-Null
    Run 'makeappx pack' { & "$sdk\makeappx.exe" pack /d $stage /p $Out /o }
} finally {
    Remove-Item -Recurse -Force $stage -ErrorAction SilentlyContinue
}
Write-Host "Package: $Out"
