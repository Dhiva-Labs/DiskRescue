# DiskRescue architecture

DiskRescue recovers photos and videos from failing and failed storage
devices and repairs damaged media files. It is a Rust workspace of small
crates with one binary, `diskrescue`.

## Principles

1. **The source is never written to.** Every reader goes through the
   read-only `Source` trait in `diskrescue-core`. Devices are opened
   `O_RDONLY`. Output on the source device is refused.
2. **Image first, work on the image.** A failing device gets one careful,
   resumable pass (the imaging engine). Everything else (filesystem parsing,
   carving, repair) runs against the image, as often as needed, without
   touching the device again.
3. **Device-aware.** The imaging strategy is a profile chosen from the
   device class (HDD, SSD, USB flash, SD card, ...). Flash devices get small
   reads, short timeouts and re-open logic; spinning disks get large reads,
   aggressive skipping and a reverse pass.
4. **Names when possible, carving always.** Filesystem parsers recover
   existing and deleted files with their names and paths. The carver then
   sweeps unallocated space (or everything) for media signatures, so files
   whose metadata is gone still come back.
5. **Validate, then repair.** Every recovered file is checked structurally.
   Damaged files go through format-specific repair (JPEG, PNG, MP4/MOV,
   AVI), optionally using healthy donor files from the same camera.
6. **Resumable and honest.** Imaging writes a ddrescue-compatible mapfile.
   Reports say exactly which bytes were unreadable and which files contain
   holes.

## Crates

| Crate | Role |
|---|---|
| `diskrescue-core` | Shared vocabulary: `Source`, `RangeSet`, `DeviceInfo`/`DeviceKind`, `ImagingProfile`, `FileSystem` and `FileFormat` traits, timestamps, byte/bit readers, hashing. No I/O policy. |
| `diskrescue-device` | Enumerate and classify block devices (Linux sysfs, Windows IOCTLs), open them read-only with direct I/O and per-read timeouts, re-find a device by serial after a USB reset. Optional SMART summary. |
| `diskrescue-imaging` | The ddrescue-style engine: copy / trim / scrape / retry passes, skip logic, status block map, ddrescue mapfile read/write, rate statistics, progress events, and a fault-injecting simulator for tests. |
| `diskrescue-partition` | MBR/EBR/GPT parsing with backup headers, superfloppy detection, lost-partition scan, filesystem probe registry. |
| `diskrescue-fat` | FAT12/16/32 and exFAT: live and deleted entries, LFN, orphan directory scan, allocation bitmap. |
| `diskrescue-ntfs` | NTFS: MFT walk with fixups, data runs, attribute lists, named streams, deleted records, $Bitmap. |
| `diskrescue-ext4` | ext2/3/4: superblock, group descriptors, inodes, extents and block maps, directories, inline data, block bitmap. |
| `diskrescue-jpeg` | JPEG: marker walk, EXIF, embedded thumbnails, Huffman scan validator, repair (EOI, donor header, standard tables, truncation at last good restart interval). |
| `diskrescue-png` | PNG: chunk walk, CRC check, lenient inflate/unfilter, repair (CRC fix, truncated IDAT, missing IEND). |
| `diskrescue-stills` | GIF, BMP, WebP, TIFF walker and the TIFF-based RAWs (CR2, NEF, ARW, DNG, PEF, ORF, RW2), RAF, PSD. |
| `diskrescue-isobmff` | ISO BMFF (MP4/MOV/3GP/M4V/HEIC/HEIF/AVIF/CR3): box walker, carving measure, metadata, H.264/HEVC/AAC parsers, moov reconstruction from a donor, truncation repair, writer. |
| `diskrescue-video` | AVI (with index rebuild), Matroska/WebM, MPEG-TS/M2TS, MPEG-PS. |
| `diskrescue-carve` | Signature scanner over a `Source`: chunked reads, multi-pattern search, alignment filter, nested-file suppression, parallel measurement, ordered output. |
| `diskrescue-recover` | The pipeline: open source, (image), partitions, filesystems, extract, carve, dedup, validate, repair, organise output, write report. |
| `diskrescue-fixtures` | Builders for synthetic test data: media files, filesystem images via mkfs tools, damaged variants, simulated failing devices. |
| `diskrescue` | CLI (`list`, `image`, `scan`, `extract`, `carve`, `repair`, `recover`, `verify`) with progress display and a block-map TUI for imaging. |

Dependency direction: everything depends on `core`; `recover` depends on
all parsers; the binary depends on `recover`, `device`, `imaging` and
`partition`. Parser crates never depend on each other.

## Data flow

```
device ──(device crate: open, classify)──► Source
   │
   ▼ imaging engine (profile per DeviceKind, mapfile)
image file + mapfile ──► FileSource.with_unreadable(bad ranges)
   │
   ├─► partition table ──► per-volume SubSource ──► FileSystem parser
   │        │                                          │
   │        │                                          ├─► walk(): live + deleted entries
   │        │                                          ├─► read_file(): data with hole report
   │        │                                          └─► unallocated(): free space RangeSet
   │        │
   │        └─► carver on unallocated (or whole) space ──► FileFormat.measure()
   │
   ▼
recovered files ──► FileFormat.validate() ──► FileFormat.repair() (donors)
   │
   ▼
output tree (photos/, raw/, videos/, ... by date) + report.json + report.txt
```

## Key contracts

* `Source::read_at` returns short reads at the end and a `ReadError`
  naming the first failing offset. Device sources classify errors into
  `Io`, `Timeout`, `DeviceGone`.
* `Source::unreadable()` exposes the bad-block map of a rescued image so
  parsers can flag files with holes and carvers can lower confidence.
* `FileSystem::walk` reports deleted entries with an `EntryConfidence`
  that says whether the data location is known, guessed, or gone.
* `FileFormat::measure` must be cheap on false positives and must stream;
  `nested` ranges suppress signature hits inside embedded previews.
* Imaging mapfiles use GNU ddrescue's format so users can continue in
  either tool.

## Safety rules enforced in code

* Refuse to open a source for writing anywhere in the tree.
* Refuse an output directory that lives on the source device.
* Warn (and require `--allow-mounted`) when the source is mounted.
* Never run filesystem repair tools; never modify the source filesystem.
* Direct I/O by default on devices so cached data never masks errors.
* Read errors that say nothing about the medium (permission denied, out of
  memory) stop the run with progress saved; only real media errors and
  timeouts mark sectors bad.
* The release profile keeps `panic = "unwind"`: third-party decoders run
  under `catch_unwind` so a decoder bug on corrupt data cannot end a run.

## Testing

Every crate has unit tests and integration tests; images are generated at
test time with the system's own tools (mkfs.vfat, mkfs.exfat, mkntfs and
ntfs-3g, mkfs.ext4 and debugfs, ffmpeg, Pillow) and checked against them
(fsck, ntfscat, ntfsundelete, dumpe2fs, ffprobe). `diskrescue-fixtures`
builds complete scenarios with a ground-truth manifest (BLAKE3 of every
file), and `crates/diskrescue-recover/tests` runs the whole pipeline on them:
camera cards, phone cards, NTFS and ext4 disks, wiped filesystems,
destroyed boot sectors, bad sectors, damaged media, and a simulated dying pen
drive imaged and recovered end to end.
