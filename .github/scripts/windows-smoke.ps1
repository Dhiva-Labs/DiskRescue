# End-to-end check of DiskRescue on Windows against a real (virtual) disk:
# a FAT32 VHDX with photos and videos, some of them deleted, is listed,
# guarded (output on the source is refused), imaged raw, and recovered from
# both the image and the device; every original must come back byte for byte.
# The desktop app is started and photographed.
#
# Needs an elevated PowerShell (GitHub's Windows runners are), Python with
# Pillow and ffmpeg on PATH.
#
#   .github/scripts/windows-smoke.ps1 -Bin target\release
#
# With -Packaged it runs the installed Microsoft Store package instead,
# through its command aliases (so with the package's identity).

param([string]$Bin, [switch]$Packaged, [string]$Work = 'C:\drsmoke')

$ErrorActionPreference = 'Stop'
if ($Packaged) { $Bin = "$env:LOCALAPPDATA\Microsoft\WindowsApps" }
elseif (-not $Bin) { throw 'pass -Bin <folder with the .exe files> or -Packaged' }
$cli = (Resolve-Path (Join-Path $Bin 'diskrescue.exe')).Path
$gui = (Resolve-Path (Join-Path $Bin 'diskrescue-gui.exe')).Path
$work = $Work
$vhd = "$work\card.vhdx"
$letter = 'R'
$failures = [System.Collections.Generic.List[string]]::new()

function Fail([string]$m) {
    Write-Host "FAIL: $m"
    $failures.Add($m)
}

# Run the CLI, show its output, and record a failure for an unexpected exit code.
function Run([string]$what, [int[]]$expect, [string[]]$argv) {
    Write-Host "`n> diskrescue $($argv -join ' ')"
    & $cli @argv | Out-Host
    $code = $LASTEXITCODE
    if ($expect -notcontains $code) {
        Fail "$what exited with $code (expected $($expect -join ' or '))"
    }
}

# (Not named Diskpart: PowerShell names ignore case and functions come before
# programs, so "diskpart" inside it would call itself.)
function Invoke-Diskpart([string]$name, [string]$script) {
    Set-Content -Encoding ascii -Path "$work\$name.txt" -Value $script
    diskpart.exe /s "$work\$name.txt" | Out-Host
    if ($LASTEXITCODE) { throw "diskpart $name failed ($LASTEXITCODE)" }
}

# Every original must have a byte-identical copy somewhere under $out.
function Check-Recovered([string]$out, [string]$label) {
    if (-not (Test-Path $out)) {
        Fail "${label}: no output folder"
        return
    }
    $got = @{}
    Get-ChildItem -Recurse -File $out | ForEach-Object {
        $got[(Get-FileHash -Algorithm SHA256 $_.FullName).Hash] = $_.FullName.Substring($out.Length + 1)
    }
    foreach ($f in Get-ChildItem -File "$work\media") {
        $h = (Get-FileHash -Algorithm SHA256 $f.FullName).Hash
        if ($got.ContainsKey($h)) {
            Write-Host "ok   $label $($f.Name) -> $($got[$h])"
        } else {
            Fail "${label}: $($f.Name) was not recovered byte for byte"
        }
    }
}

# The C runtime is linked in (.cargo/config.toml): no VCRUNTIME140.dll import.
# (The package's aliases are links, not the programs.)
foreach ($exe in Get-ChildItem (Join-Path $Bin 'diskrescue*.exe') | Where-Object { -not $Packaged }) {
    $text = [System.Text.Encoding]::ASCII.GetString([System.IO.File]::ReadAllBytes($exe.FullName))
    if ($text.IndexOf('vcruntime140', [System.StringComparison]::OrdinalIgnoreCase) -ge 0) {
        Fail "$($exe.Name) needs the Visual C++ Redistributable (VCRUNTIME140.dll)"
    }
}

Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force "$work\media" | Out-Null

# ---------------------------------------------------------------- media
Set-Content -Encoding utf8 -Path "$work\photos.py" -Value @'
import random, sys
from PIL import Image, ImageDraw

def photo(name, seed, w, h, fmt, **kw):
    r = random.Random(seed)
    im = Image.new("RGB", (w, h), (r.randrange(256), r.randrange(256), r.randrange(256)))
    d = ImageDraw.Draw(im)
    for _ in range(500):
        x, y = r.randrange(w), r.randrange(h)
        d.ellipse([x, y, x + r.randrange(8, 160), y + r.randrange(8, 160)],
                  fill=(r.randrange(256), r.randrange(256), r.randrange(256)))
    im.save(f"{sys.argv[1]}/{name}", fmt, **kw)

photo("IMG_0001.JPG", 1, 1600, 1200, "JPEG", quality=90)
photo("IMG_0002.JPG", 2, 1200, 1600, "JPEG", quality=85)
photo("SCREEN_01.PNG", 3, 800, 600, "PNG")
'@
python "$work\photos.py" "$work\media"
if ($LASTEXITCODE) { throw 'making the photos failed' }
ffmpeg -hide_banner -loglevel error -f lavfi -i testsrc2=duration=4:size=640x360:rate=25 `
    -f lavfi -i sine=frequency=440:duration=4 -c:v libx264 -pix_fmt yuv420p -c:a aac -shortest `
    "$work\media\VID_0001.MP4"
if ($LASTEXITCODE) { throw 'making VID_0001 failed' }
ffmpeg -hide_banner -loglevel error -f lavfi -i testsrc=duration=3:size=480x270:rate=30 `
    -c:v libx264 -pix_fmt yuv420p "$work\media\VID_0002.MP4"
if ($LASTEXITCODE) { throw 'making VID_0002 failed' }
Get-ChildItem "$work\media" | Format-Table Name, Length | Out-Host

try {
    # ------------------------------------------------------------ the card
    Invoke-Diskpart 'attach' @"
create vdisk file="$vhd" maximum=128 type=fixed
select vdisk file="$vhd"
attach vdisk
convert mbr
create partition primary
format fs=fat32 quick label=CARD
assign letter=$letter
"@
    $dcim = "${letter}:\DCIM\100TEST"
    New-Item -ItemType Directory -Force $dcim | Out-Null
    Copy-Item "$work\media\*" $dcim
    # Written to the disk before two are deleted: a file deleted while its
    # data is still only in Windows' cache never reaches the disk at all.
    Write-VolumeCache -DriveLetter $letter
    Remove-Item "$dcim\IMG_0002.JPG", "$dcim\VID_0002.MP4"
    Write-VolumeCache -DriveLetter $letter
    $n = (Get-Partition -DriveLetter $letter).DiskNumber
    $dev = "\\.\PhysicalDrive$n"
    $size = (Get-Disk -Number $n).Size
    Write-Host "card: $dev, $size bytes"

    # ------------------------------------------------------------ list
    Run 'list' @(0) @('list', '--all')
    $json = & $cli list --all --json | Out-String
    if ($LASTEXITCODE) { Fail "list --json exited with $LASTEXITCODE" }
    Write-Host $json
    $devices = $json | ConvertFrom-Json
    $card = $devices | Where-Object { $_.path -match "PhysicalDrive$n$" }
    if (-not $card) {
        Fail "list does not show $dev"
    } elseif ($card.mountpoints -notcontains "${letter}:\") {
        Fail "list does not show $dev in use as ${letter}:\ (mount points: $($card.mountpoints -join ', '))"
    }
    $sysNumber = (Get-Partition -DriveLetter $env:SystemDrive.TrimEnd(':')).DiskNumber
    $sys = $devices | Where-Object { $_.path -match "PhysicalDrive$sysNumber$" }
    if (-not $sys -or $sys.mountpoints -notcontains "$env:SystemDrive\") {
        Fail "the system disk (PhysicalDrive$sysNumber) is not shown as $env:SystemDrive\"
    }

    # ------------------------------------------------------------ in use
    Run 'image while Windows uses the drive' @(3) @('image', $dev, "$work\inuse.img")
    Run 'image with --allow-mounted' @(0) @('image', $dev, "$work\inuse.img", '--allow-mounted')
    Remove-Item "$work\inuse.img*" -ErrorAction SilentlyContinue

    # ------------------------------------------------------------ safety
    # --allow-mounted, so only the output-on-source guard can refuse.
    Run 'image onto the source drive' @(3) @('image', $dev, "${letter}:\copy.img", '--allow-mounted')
    if (Test-Path "${letter}:\copy.img") { Fail 'image wrote onto the drive it rescues' }
    Run 'recover onto the source drive' @(3) @('recover', $dev, '-o', "${letter}:\out", '--allow-mounted')
    if (Test-Path "${letter}:\out") { Fail 'recover wrote onto the drive it rescues' }

    # Rescue the way a failing card should be handled: Windows lets go of it.
    Set-Disk -Number $n -IsOffline $true

    # ------------------------------------------------------------ rescue
    Run 'scan' @(0) @('scan', $dev)
    Run 'image' @(0) @('image', $dev, "$work\card.img")
    if ((Test-Path "$work\card.img") -and (Get-Item "$work\card.img").Length -ne $size) {
        Fail "card.img is $((Get-Item "$work\card.img").Length) bytes, the disk $size"
    }
    Run 'recover from the image' @(0) @('recover', "$work\card.img", '-o', "$work\out-image")
    Check-Recovered "$work\out-image" 'image '
    Run 'recover from the device' @(0) @('recover', $dev, '-o', "$work\out-device")
    Check-Recovered "$work\out-device" 'device'

    # ------------------------------------------------------------ desktop app
    $p = Start-Process $gui -PassThru -RedirectStandardError "$work\gui-stderr.txt"
    Start-Sleep -Seconds 15
    if ($p.HasExited) {
        Fail "the desktop app exited with $($p.ExitCode): $(Get-Content -Raw "$work\gui-stderr.txt")"
    } else {
        Add-Type -AssemblyName System.Windows.Forms, System.Drawing
        $b = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
        $bmp = New-Object System.Drawing.Bitmap $b.Width, $b.Height
        $g = [System.Drawing.Graphics]::FromImage($bmp)
        $g.CopyFromScreen($b.Location, [System.Drawing.Point]::Empty, $b.Size)
        $bmp.Save("$work\gui.png")
        Stop-Process -Id $p.Id
        Write-Host 'desktop app running, screenshot saved'
    }
} finally {
    if (Get-DiskImage -ImagePath $vhd -ErrorAction SilentlyContinue | Where-Object Attached) {
        Invoke-Diskpart 'detach' @"
select vdisk file="$vhd"
detach vdisk
"@
    }
}

if ($failures.Count) {
    Write-Host "`n$($failures.Count) check(s) failed:"
    $failures | ForEach-Object { Write-Host "  $_" }
    exit 1
}
Write-Host "`nall Windows checks passed"
