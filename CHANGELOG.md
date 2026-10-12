# Changelog

## 0.1.0 (unreleased)

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
