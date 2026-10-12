# DiskRescue

**Get your photos and videos back from a dying drive, and repair the files
that came back broken.**

DiskRescue is a single-binary recovery tool for hard disks, SSDs, USB pen
drives and SD cards that are failing, have failed, or were wiped. It images
the device the way a recovery lab would (reading the healthy areas first and
coming back for the damaged ones), recovers files by name from FAT, exFAT,
NTFS and ext4, carves photos and videos out of raw space when the filesystem
is gone, and repairs damaged JPEG, PNG, MP4/MOV and AVI files.

> Status: under construction. See `PLAN.md` for the build plan and
> `docs/ARCHITECTURE.md` for the design.

## What it does

- **Device-aware imaging.** The read strategy adapts to the device: small
  reads, short timeouts and re-open logic for pen drives and SD cards whose
  controllers lock up; large sequential reads, exponential skipping and a
  reverse pass for spinning disks; page-sized probing for SSDs. Progress is
  kept in a GNU ddrescue compatible mapfile, so a rescue can be stopped and
  resumed, or handed over to ddrescue and back.
- **Recovery by name.** Existing and deleted files, with paths and
  timestamps, from FAT12/16/32, exFAT, NTFS and ext2/3/4, including volumes
  whose boot sector or partition table is damaged.
- **Carving.** When the filesystem is unusable, a signature scanner sweeps
  the image for JPEG, HEIC, PNG, GIF, WebP, TIFF, camera RAW (CR2, CR3,
  NEF, ARW, DNG, ORF, RW2, RAF, PEF), MP4/MOV/3GP, AVI, MKV/WebM, MTS/M2TS
  and MPEG-PS, measuring each file's real extent instead of guessing.
- **Repair.** Truncated or header-damaged JPEGs, PNGs with bad CRCs or cut
  IDAT streams, MP4/MOV files whose `moov` atom was lost when the camera
  died (rebuilt from a healthy reference clip from the same device), and
  AVIs without an index.
- **Honest reporting.** Every recovered file is validated; the report says
  which files contain unreadable sectors, which were repaired and how, and
  which are guesses.

## Safety

DiskRescue never writes to the source device. It refuses to put the output
on the device it is recovering from, warns when the source is mounted, and
reads with direct I/O so cached data cannot hide errors.

## Licence

MIT. See `LICENSE`.
