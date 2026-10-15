# Changelog

## 0.2.1 (2026-10-15)

- Windows on ARM: native ARM64 builds, as a zip on the releases page and as a
  Microsoft Store package next to the x64 one. CI builds and tests them on
  Windows 11 on ARM (the test suite, the virtual-disk rescue and the
  installed Store package), like x64. Nothing changes on Linux.

## 0.2.0 (2026-10-15)

Windows support.

- Windows builds: the drive list shows drive letters, so the drive Windows
  runs from is marked as the system disk and a drive in use is refused
  unless `--allow-mounted` is given; messages say how to take a drive out of
  use on Windows. The `.exe` files link the C runtime statically and need no
  Visual C++ Redistributable.
- The desktop app offers "Restart as administrator" (the UAC prompt) when a
  drive needs administrator rights on Windows; the desktop and terminal apps
  give Windows advice (Disk Management, a terminal opened as administrator)
  instead of Linux's (unmount, sudo).
- Microsoft Store package: the desktop app plus the `diskrescue` and
  `diskrescue-tui` commands. Releases also attach a Windows zip.
- Every change is tested on Windows: the test suite, and a rescue of a FAT32
  virtual disk through `\\.\PhysicalDriveN` (listing, in-use and
  output-on-source guards, raw image, byte-exact recovery of existing and
  deleted photos and videos, the desktop app), also from the installed
  Store package.
- Fix (Windows): recovering straight from a drive zero-filled the last
  partial sector of every file; reads of a Windows disk are now always whole
  sectors.
- Fix: a repaired copy could be written outside the output folder when the
  file's recorded path was rooted or drive-relative (Windows) or contained
  `..`; only plain folder names are kept now.
- Fix: loading the progress map of a badly damaged drive with 4 KiB sectors
  took seconds (quadratic joining of sub-sector blocks); it is now linear.

## 0.1.0 (2026-10-13)

First version.

- Device enumeration and classification (HDD, SATA/NVMe SSD, USB flash, USB
  HDD/SSD, SD/MMC, optical) from sysfs, read-only opening with direct I/O,
  per-read timeouts, re-attach by fingerprint after USB re-enumeration.
- Resumable imaging engine with GNU ddrescue compatible mapfiles and
  per-device-type strategies; full-screen block-map view.
- MBR/EBR/GPT parsing with backups; lost-partition scan.
- FAT12/16/32 and exFAT, NTFS and ext2/3/4 readers with deleted-file
  recovery; FAT boot-sector rebuild; NTFS volume rebuild from the MFT.
- Carving of 30+ photo, RAW, video and audio formats with exact extents.
- JPEG, PNG, MP4/MOV (moov rebuild) and AVI repair.
- `diskrescue` CLI: list, info, image, scan, recover, carve, repair, verify,
  map.
- `diskrescue-tui`: interactive terminal app (drive list, rescue wizard with
  safety checks, live block map, recovery progress, results browser, repairs).
- `diskrescue-gui`: desktop app (drive cards, three-step rescue wizard,
  administrator copy through pkexec, live block map, thumbnail results,
  drag-and-drop repairs).
- Repaired photos with grey-filled areas carry a marker so later checks report
  them as partly recovered.
