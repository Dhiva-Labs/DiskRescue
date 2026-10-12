# Changelog

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
