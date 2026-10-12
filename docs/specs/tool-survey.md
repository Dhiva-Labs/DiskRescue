# Tool survey: existing data-recovery software and real-world recovery practice

Status: research spec for DiskRescue. Lines tagged `UNVERIFIED` could not be confirmed
against a primary source during research and must be fact-checked before they are
used in README text or in code comments. Fact-check pass 2026-10-12: every tagged line
and the on-disk/wire numbers were checked against primary sources (ddrescue 1.30 manual,
Linux kernel source, OpenSuperClone/PhotoRec/zune-jpeg/dd_rescue source, Garfinkel 2007
PDF, man pages, Debian copyright files, crates.io); corrections are applied in place and
the sources are listed in section 7. Tags that remain are listed in 1.7.

Scope of this document: (a) what each existing tool does, how it does it, its licence
and where it stops; (b) a feature matrix for photo/video recovery; (c) field practice
that DiskRescue must encode as defaults and warnings; (d) concrete numbers (timeouts,
block sizes, fragmentation rates); (e) prioritised feature list for v1 vs later;
(f) exact warning strings.

---

## 1 Overview and scope

### 1.1 Why a survey first

DiskRescue overlaps five tool families that today are separate programs with separate
on-disk artefacts, and a user in the field currently has to chain them by hand:

| Family | Representative tools | Artefact handed to the next step |
|---|---|---|
| Device imagers | GNU ddrescue, OpenSuperClone (ex HDDSuperClone), dd_rescue, safecopy, Disk Drill byte-to-byte, R-Studio imaging, DMDE cloning, PC-3000 (hardware) | raw image + map/log of bad areas |
| Filesystem-level recovery (names, dates, deleted files) | TestDisk, DMDE, R-Studio, UFS Explorer, Recuva, ntfsundelete, extundelete, ext4magic, fatcat | files with original names |
| Carvers (no filesystem needed) | PhotoRec, Foremost, Scalpel, bulk_extractor, R-Studio "known file types", Disk Drill deep scan | files named by offset/sequence |
| Media repair | untrunc, recover_mp4 (now Restore.Media), JPEG-Repair Toolkit, JPEGsnoop, Stellar Repair for Video, EaseUS repair | playable/viewable file |
| Visualisation/reporting | ddrescueview, OSCViewer, GUIs of the commercial tools | mapfile picture, file list with preview |

DiskRescue's value is doing all five in one binary with one shared state (the mapfile),
with device-aware imaging defaults, and with media repair that does not need a human
to find a "reference file".

### 1.2 Per-tool survey

Licence abbreviations: GPL2+ = GNU GPL v2 or later; prop. = proprietary/closed source.

#### GNU ddrescue (Antonio Diaz Diaz)
- Does: copies a block device to an image "trying to rescue the good parts first in
  case of read errors"; never writes zeros for bad sectors into an existing output;
  resumable via mapfile; merges multiple damaged copies into one output.
- Approach: five phases (copying in up to 4 passes with skipping, trimming, sweeping,
  scraping, retrying), sector-level marks `?`,`*`,`/`,`-`,`+` kept in a text mapfile.
  Interface-agnostic: uses only the kernel block layer (`read(2)`, optionally
  `O_DIRECT`). Manual states explicitly that "a bad drive can block ddrescue for a
  long time until the kernel gives up" (it has no own read timeout).
- Licence: GPL-2.0-or-later. C++. Current manual copyright 2004-2026; example mapfile
  in the manual is from version 1.30.
- Strengths: the de-facto mapfile format (DiskRescue must read/write it bit-compatibly);
  minimal head movement design; `--test-mode` fault injection from a mapfile;
  `ddrescuelog` for mapfile set operations.
- Limitations: no SCSI/ATA passthrough, so each failing read costs the full kernel
  timeout and retry budget (see 2.4); no device-type profiles; no filesystem awareness
  (cannot prioritise allocated clusters or metadata); no reporting beyond the screen.

#### ddrescueview (Martin Bittermann)
- Does: GUI viewer for ddrescue mapfiles: colour block grid, zoom to sector level,
  block inspector, auto re-read while the rescue runs, domain-mapfile overlay, decimal
  and binary units.
- Licence: GPL-3.0. Object Pascal / Lazarus. Version 0.4.5 (25 February 2022, "restores
  compatibility with ddrescue 1.24 and later"); Windows, GNU/Linux, macOS.
- Relevance: defines the visual vocabulary users expect (green finished, red bad, etc.).
  DiskRescue's HTML/terminal map should follow the same colour semantics so screenshots
  are mutually readable.

#### HDDSuperClone / OpenSuperClone (Scott Dwyer; fork maintained by ISpillMyDrink)
- Does: "sector-by-sector clones or images of disks" with "a multi-phase approach";
  logs each step and can be stopped/resumed; a "Virtual Disk Mode" that exposes the
  source as a virtual block device so a filesystem tool (DMDE, R-Studio, UFS Explorer)
  can pull only the files that matter while reads are imaged on demand.
- Approach: Phase 1 forward copy with error-based skipping; Phase 2 backward copy with
  error-based skipping; Phase 3 forward copy skipping on read *rate*; Phase 4 forward
  copy without skipping; then Trimming (one sector forward until error, then backward),
  Dividing (only when trimming disabled), Scraping (one sector forward over non-scraped),
  Retrying (bad sectors one at a time). Direct modes talk to the controller without the
  OS driver: Direct AHCI (SATA, drive must be hidden from the OS, controller in AHCI
  mode), Direct IDE, Direct USB, Direct USB ATA. Also SCSI and ATA passthrough modes.
  Virtual driver modes 1-5 (1 = all phases starting at phase 4; 2 = phase 4 only;
  3 = like 2 but grows read size up to cluster size on sequential reads; 4 = read only
  from destination; 5 = return zeros).
- Hardware: USB relay control for automatic power-cycling; v2.6.0 (listed as
  work-in-progress in the README as of 2026-10) adds "Asclepius relay and current
  sensing board". v2.5.0 raised max retry runs from 8 to 1000.
- Licence: GPLv2 (HDDSuperClone source released by its author; OpenSuperClone GPLv2).
- Strengths: the only open tool with controller-level timeouts and power-cycle
  recovery; targeted (file-driven) imaging. HDDSuperClone's skip-threshold note: a
  drive with slow-response firmware glitches "will trigger runaway skipping", mitigated
  by setting the threshold to 60000 ms. OpenSuperClone defaults (verified in
  `src/opensuperclone/opensuperclone.h` and `common.h`, 2026-10-12): skip threshold
  1000 ms (`DEFAULT_SKIP_TIMEOUT`, GUI label "Skip Threshold (ms) phase-1-2"); rate skip
  50 kB/s for phase 3 (`DEFAULT_RATE_SKIP` 50000 B/s); skip size 0x1000 LBAs = 2 MiB at
  512 B (`DEFAULT_SKIP_SIZE`), max skip 0x4000000 LBAs = 32 GiB; cluster 128 sectors
  (64 KiB, `DEFAULT_CLUSTER_SIZE`); the per-read "soft reset" timeout is 8 s
  (`SOFT_RESET_TIMER`), busy-wait 20 s (`BUSY_WAIT_TIMER`), and the optional per-phase
  timers are 0.25 s (phases 1-2), 0.35 s (phase 3), 0.5 s (phase 4), 0.8 s
  (trim/divide/scrape) and 8 s (retry); retry runs up to 1000 since 2.5.0.
- Limitations: Linux only, needs root and (for direct modes) drives hidden from the
  kernel; own log format, not the ddrescue mapfile (see 2.3), but it can import and
  export ddrescue mapfiles ("File->Import ddrescue Log (Map) File" / "Export" in the
  wiki page Basics of Operation; `read_ddrescue_log_ccc` / `write_ddrescue_logfile_ccc`
  in `src/opensuperclone/opensuperclone.c`); no file carving or repair.

#### dd_rescue (Kurt Garloff)
- Does: dd-like copier that "does not abort on errors"; two block sizes (large "soft"
  and small "hard", falling back to the small one on errors and promoting again after a
  clean period); reverse copying from the end; sparse output; `O_DIRECT` to expose
  medium errors immediately; plugins (hash, lzo, crypt).
- Licence: GNU GPL. Name clash is why Debian's GNU ddrescue package is `gddrescue`.
- Options (from the `getopt` table in `dd_rescue.c`): `-b softbs` / `-B hardbs` (defaults
  16 KiB / 4 KiB buffered, 1 MiB / 512 B with `-d`), `-d` / `-D` `O_DIRECT` on input /
  output, `-r` reverse, `-l logfile` (errors and summary), `-o bbfile` (one bad-block
  number per line, in units of `hardbs`, consumable by `mke2fs -l`), `-e maxerr`,
  `-s ipos` / `-S opos`, `-A` always write (zero-fill on error), `-a` sparse.
- Limitations: no multi-phase algorithm, no mapfile-driven resume comparable to GNU
  ddrescue (only the `-o bbfile` bad-block list), so it
  "spends more time at error areas" in the sense criticised by the ddrescue manual.

#### safecopy
- Does: extracts "as much data as possible from a problematic (i.e. damaged sectors)
  source"; floppy/HDD/CD/DVD/tape; marks unreadable regions in the output with a
  recognisable string (`-M`) so files containing holes can be found later; include (`-I`)
  and exclude (`-X`) block lists compatible with e2fsck badblocks lists; `-Z` forces
  head repositioning before retries.
- Presets (man page, verbatim): `--stage1` = `-f 10% -r 10% -R 1 -Z 0 -L 2 -M BaDbLoCk
  -o stage1.badblocks` (fast, no retries, avoid bad areas); `--stage2` = `-f 128* -r 1*
  -R 1 -Z 0 -L 2 -I stage1.badblocks -o stage2.badblocks` (find exact ends of bad
  areas); `--stage3` = `-f 1* -r 1* -R 4 -Z 1 -L 2 -I stage2.badblocks -o
  stage3.badblocks` (maximum retries, head realignment, low-level access). `-R`
  defaults to 3; `-o file` writes the badblocks list.
- Licence: GPLv2 or higher. Last release 1.7 (March 2012): effectively unmaintained.
- Relevance: the "mark the hole with a string" idea is useful for DiskRescue's
  validation stage (a JPEG whose bytes contain the hole marker is known-damaged, not
  merely suspicious).

#### PhotoRec / TestDisk (Christophe Grenier, CGSecurity)
- Does: TestDisk = partition table and boot sector recovery (FAT/NTFS/ext boot sector
  rebuild, MFT mirror, undelete for FAT/NTFS/exFAT/ext2). PhotoRec = signature carver
  for "more than 480 file extensions (about 300 file families)" on FAT, NTFS, exFAT,
  ext2/3/4, HFS+ and raw.
- Approach: reads the superblock/VBR to learn the block size (or infers it from the first
  files found), then "reads the media block by block (or cluster by cluster)" checking
  each block against the signature database; a found file is extended until the next
  header or until the format's own end-of-file structure says stop; recovered JPEGs are
  verified with libjpeg(-turbo) and discarded when invalid unless "Keep corrupted files"
  is set. Fragmentation: after a successful recovery it re-checks earlier blocks where a
  signature was found but recovery failed, which rescues *some* fragmented files;
  the `bruteforce` (Paranoid = "Yes (Brute force)") option decodes JPEG "line by line"
  to find the foreign block and tries appending following blocks: "very slow and not very
  reliable", and "only really works for jpg files". Expert mode forces block size and
  offset; "Free" (unallocated only) vs "Whole" (entire partition) space selection.
  Read-only by design.
- Licence: GPL-2.0-or-later. C. Windows/Linux/macOS/DOS, CLI (ncurses) + qphotorec GUI.
- Strengths: enormous signature DB, battle-tested, ships everywhere.
- Limitations: output names are sequence numbers (`f1234567.jpg`), no original names
  or dates (TestDisk's undelete gives names but only for simple cases); no imaging of
  failing media (users are told to image with ddrescue first); fragmented video is
  essentially unsupported; JPEG repair absent; HEIC is recovered (`src/file_mov.c`
  matches `ftyp` brand `heic` and sets extension `.heic`; listed on the "File Formats
  Recovered By PhotoRec" wiki page) but it lives inside the `mov` family of the
  file-type list, so there is no separate HEIC toggle, which is what the 7.2-WIP forum
  thread was about; Canon CR2 is recovered under the TIFF family, which confuses users.

#### Foremost
- Does: header/footer (and header/max-size) carving from a config file of signatures.
  Originally USAF Office of Special Investigations.
- Licence: public domain.
- Limitations: contiguous carving only, "minimal structural validation", slow, old.

#### Scalpel
- Does: "complete rewrite of the Foremost 0.69 file carver" with large performance
  gains; v2 added multithreading; GPU header/footer matching was offered in 2.0 but the
  2.02 README says it is "deprecated and removed". Scalpel3 (arXiv
  2608.20363, 2026) adds a "massively threaded architecture for contiguous and fragmented
  recovery" with pluggable per-format validators/reassembly strategies and ONNX models.
- Licence: GPL (2.02 README: "released under the GPL"; 1.60 and 2.02 on GitHub
  `nolaforensix`). Scalpel3 availability UNVERIFIED: the arXiv abstract (submitted
  2026-06-17, revised 2026-08-24) names no repository or release.
- Limitations: as Foremost until Scalpel3; Scalpel3 is a research platform not a
  consumer tool.

#### bulk_extractor (Simson Garfinkel et al.)
- Does: "find evidence button": scans bulk data for features (emails, URLs, card
  numbers) and also carves JPEG etc.; multithreaded; recursive decompression.
- Licence (LICENSE.md in `simsong/bulk_extractor`): original NPS-era code is US
  Government work not subject to copyright (17 U.S.C. 105); post-2015 project code is
  GPL-3.0-or-later ("supersedes the prior statement that post-NPS modifications were
  under the MIT License"); bundled components under CPL 1.0 (SleuthKit headers), MIT
  (base64, utfcpp) and others. Not MIT, not public domain as a whole.
- Relevance: its EXIF/JPEG scanner and scalable scanning architecture; not a recovery
  tool for end users.

#### untrunc (Federico Ponchio; many forks, e.g. anthwlock/untrunc)
- Does: "rebuilds the index and recreates the moov atom" for truncated MP4/M4V/MOV/3GP
  files, "using a complete video as an example" (a working file from the same device
  and settings).
- Approach: parse the reference's `moov` to learn per-track codec and sample
  patterns, then walk the broken file's `mdat`, classifying each chunk by codec
  heuristics (H.264 NAL types, AAC frame lengths) and emitting `stco/stsz/stts/stsc`
  tables accordingly.
- Licence: GPL-2.0. C++ with libavcodec.
- Limitations: needs a reference; fails on unknown codecs or when audio/video chunk
  interleaving differs from the reference; no FS or imaging role.

#### recover_mp4 / recover_mp4_to_h264 (Dmitry Vasilyev, "slydiman")
- Does: recovers AVC/H.264 and HEVC/H.265 video and AAC/ADPCM/MP3/PCM audio from
  "unfinalized MP4/MOV files without (or empty) header" caused by "damaging camcorder
  during recording"; `--analyze` a good file first, then run the printed command to
  extract raw `.h264`/`.aac` streams, which are muxed with ffmpeg.
- Status/licence: legacy v1.92 freeware (download discontinued, unsupported); engine
  v3.0 now only inside the web service Restore.Media (proprietary). Reported to fail
  when the data area "is filled with empty [00] bytes".
- Relevance: proves that moov-less recovery is a stream-parsing problem (find NAL/ADTS
  frames, derive timing from the reference), which DiskRescue can do with a built-in
  "reference-free" fallback using SPS/PPS and typical GOP timing.

#### JPEG-Repair Toolkit (DiskTuna, Joep van Steen)
- Does: fixes corrupt JPEG headers using a reference ("donor") file from the same
  camera/settings; repairs invalid markers; "bad sector" damage; MCU-level manual patch
  with visual feedback; an "MCU clone" tool that "re-align[s] it by inserting stuff bytes
  (zeros) which leave a grey horizontal line" and then clones MCUs from the row above to
  cover the grey line. Companion JpegDigger carves JPEGs.
- Licence: proprietary, paid, Windows.
- Relevance: the canonical description of real-world JPEG corruption classes
  (see 4.5) and of repair steps DiskRescue should automate.

#### JPEGsnoop (Calvin Hass)
- Does: JPEG decoder and analysis tool: dumps all markers, quantisation/Huffman tables,
  detects edits by comparing quantisation tables with a camera signature database, shows
  where decoding breaks.
- Licence: GPL-2.0. Windows (MFC). Relevance: diagnostic reference for DiskRescue's
  JPEG validator; its signature DB idea (quant tables -> camera) helps pick donors.

#### R-Studio (R-Tools Technology)
- Does: full-stack commercial recovery: NTFS/FAT/exFAT/HFS+/APFS/ext2-4/XFS/UFS parsing
  with deleted files, "Scan for Known File Types (raw file recovery)" with user-defined
  signatures, RAID reconstruction, disk imaging with "multi-pass imaging" and "runtime
  imaging" (clones sectors as they are read during analysis), file previewer, DeepSpar
  hardware integration.
- Licence: proprietary; Windows, macOS, Linux; consumer editions from USD 49.99 (FAT
  edition; NTFS 59.99, full 79.99), Technician edition USD 899 (review/price listings
  2026).
- Limitations: closed; generic signature carving with no media repair; imaging via OS
  block layer unless DeepSpar hardware present.

#### DMDE (Dmitry Sidorov)
- Does: disk editor + recovery: FAT12/16/32, exFAT, NTFS, ReFS, ext2/3/4, HFS+/HFSX,
  APFS, btrfs; quick/full scan rebuilding directory trees; signature ("raw") search;
  cloning with I/O-error handling, bad-sector skipping and multiple passes via a log;
  RAID; runs on Windows, Linux, macOS and DOS.
- Licence: proprietary; Free Edition recovers up to 4000 files from the current panel
  per operation (unlimited repetitions); paid editions remove limits.
- Relevance: the closest commercial analogue of DiskRescue's "image + FS parse + carve"
  pipeline, often paired with OpenSuperClone's virtual disk mode.

#### UFS Explorer (SysDev Laboratories)
- Does: Standard/Professional Recovery: broad FS support including RAW-state volumes
  and lost partitions, RAID, virtual disks. UFS Explorer Video Recovery: frame-level
  reconstruction of interrupted recordings from RIFF/AVI and QuickTime structures plus
  over 130 proprietary CCTV/dashcam stream formats (Hikvision WFS, Dahua DHFS, ...),
  exporting original codec data (MJPEG, H.264, H.265, MPEG variants) by transmuxing to
  AVI without re-encoding.
- Licence: proprietary; Video Recovery licensed to organisations only ("Personal use
  licenses are not provided"; commercial base rate EUR 8,000/year, 1-5 year terms);
  Windows 7+ x86-64 only for Video Recovery.
- Relevance: the benchmark for fragmented video reassembly; shows the market gap for an
  open equivalent.

#### Recuva (Piriform/CCleaner)
- Does: Windows-only consumer undelete (NTFS/FAT/exFAT) with a slower "deep scan"
  signature mode (deep scan is in the free tier too).
- Licence: proprietary freeware with a paid Professional tier (USD 19.95-29.95 per
  2026 review listings; the official page refused automated fetches).
- Limitations: no imaging, no repair, deep scan weaker than PhotoRec, Windows only.

#### Disk Drill (CleverFiles)
- Does: consumer recovery for macOS/Windows with "byte-to-byte backup" for damaged
  disks: first pass skips problematic blocks, later passes retry them; block size
  starts at 64 KB and halves down to the sector size (5 passes for 4096-byte sectors,
  8 for 512-byte), 5-8 extra attempts; saves a bad-block map (macOS) so later scans skip
  known-bad blocks; resumable ("Append"); outputs plain `.dd` (Windows default),
  `.iso`, `.dmg` (macOS default); recommends 5-10 % extra space on the destination and
  USB 2.0 connections for damaged drives; supports RAW photo formats (CR2, NEF, DNG).
- Licence: proprietary; free tier limited to previews/small recovery amounts.

#### EaseUS Data Recovery Wizard
- Does: consumer recovery (Windows/macOS) with a scan that lists videos and a
  "file repair" step that repairs "corrupt MP4 & MOV videos and JPEG/JPG photos during
  recovery". Licence: proprietary subscription/lifetime.
- Limitations: closed; repair method undocumented; no failing-disk imaging controls.

#### Stellar Data Recovery / Stellar Repair for Video
- Does: Repair for Video repairs MP4/MOV/AVI etc.; for severely corrupted files it
  requires a "Sample File" that is "a working file created from the same device and of
  the same format"; sample has no size limit. Stellar Data Recovery includes photo/video
  repair modules in higher tiers.
- Licence: proprietary.

#### PC-3000 (ACE Lab) -- hardware tier, noted only
- Does: vendor-specific firmware access (service area, head maps, disabling heads,
  reallocation lists), hardware read timeouts and power control, PC-3000 Flash and
  SSD editions for NAND chip-off and controller diagnostic modes; the standard tool in
  labs alongside DeepSpar and Rusolut VNR.
- Cost: new PC-3000 Express about EUR 8,000 (2025 price list, promotional), plus
  mandatory technical support EUR 1,700-2,000 per year.
- Relevance: defines the "go to a lab" boundary; DiskRescue must recognise the cases
  only this tier can handle (4.3, 4.4) and say so.

#### extundelete
- Does: ext3/ext4 undelete by searching "the file system's journal for an old copy of
  that inode" and matching names through deleted directory entries; options
  `--restore-all`, `--restore-file`, `--restore-files`, `--restore-directory`, `--after`,
  `--before`, `--inode`. Requires unmounted (or read-only mounted) FS and e2fsprogs
  1.39+ (1.41+ for ext4). Does not restore hard/soft links or xattrs. Last release 0.2.4
  (January 2013). Licence: GPL-2 (Debian copyright file for 0.2.4-3: upstream files
  2009-2012 Nic Case, `License: GPL-2`).

#### ext4magic
- Does: same journal-based approach, "much more effective" than extundelete in Debian's
  description: recovers name, owner, mode and old atime/mtime. `-r` recover only files
  whose data blocks are all unallocated; `-R` recover all matched inodes incl. partially
  overwritten and directory structure; `-m` partially deleted FS; `-M` whole-FS
  recovery after `rm -rf`; `-j` external journal copy; `-a`/`-b` epoch-second window
  (default: last 24 h); `-l` list; `-f` filename filter; `-d` destination (default
  `RECOVERDIR`). Needs the journal: man page, without a usable journal "it will not be
  possible to recover any file at any time". Licence: GPL-2+ (Debian copyright file).

#### ntfsundelete (ntfs-3g / ntfsprogs)
- Does: scan/undelete/copy modes over an NTFS volume; prints inode, name, size and
  a "percentage recoverable" per deleted MFT record; `-m` glob, `-p` min percentage,
  `-t` time window, `-i` inode list, `-u` undelete, `-o`/`-d` output, `--parent` shows
  the original directory, `-b` fill byte for unrecoverable runs, `-T` truncate to real
  size, `-f` force. Cannot recover compressed or encrypted files (shown as 0 %). Recovery
  fails when the MFT record or the clusters were reused. Name may be lost because
  ntfs-3g clears the name on delete (a patch recovers it when the record is intact).
  Licence: GPL (ntfs-3g).

#### fatcat (Gregwar)
- Does: FAT12/16/32 explore/extract/repair/forensics: read deleted files (`-r`) and
  extract directories incl. deleted entries (`-x`), orphan search over allocated
  clusters (`-o`), compare the two FATs (`-2`), merge FATs preferring non-zero entries
  (`-m`), repair chaining of unallocated-but-readable directories (`-f`), backup/restore
  FATs. Deleted entries "retain cluster references; the tool reads them contiguously".
  Licence: MIT. C++.

### 1.3 Feature matrix for photo/video recovery

Legend: Y = yes; P = partial; N = no; H = hardware/lab tier; ? = UNVERIFIED.

| Feature | ddrescue | OpenSuperClone | dd_rescue | safecopy | PhotoRec/TestDisk | Foremost/Scalpel | bulk_extractor | untrunc | recover_mp4 | JPEG-Repair | R-Studio | DMDE | UFS Explorer | Recuva | Disk Drill | EaseUS | Stellar | PC-3000 | extundelete/ext4magic | ntfsundelete | fatcat |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| Device-aware imaging (timeouts, passthrough, power cycle) | N | Y | N | P | N | N | N | N | N | N | P (DeepSpar hw) | N | N | N | P | N | N | H | N | N | N |
| Mapfile / resume | Y (mapfile) | Y (own log) | P | P (badblocks lists) | N | N | N | N | N | N | Y (own) | Y (own) | Y (own) | N | Y (own) | N | N | H | N | N | N |
| FS-based recovery with names | N | N (via virtual disk + 3rd party) | N | N | P (TestDisk undelete) | N | N | N | N | N | Y | Y | Y | Y | Y | Y | Y | H | Y (ext3/4) | Y (NTFS) | Y (FAT) |
| Signature carving | N | N | N | N | Y | Y | Y | N | N | P (JpegDigger) | Y | Y | Y | P | Y | Y | Y | H | N | N | P (orphans) |
| Fragmented-file handling | n/a | n/a | n/a | n/a | P (JPEG brute force) | N (Scalpel3: research) | N | N | N | N | P ? | P ? | Y (video, frame level) | N | N ? | N ? | N ? | H | n/a | n/a | n/a |
| JPEG repair | N | N | N | N | N | N | N | N | N | Y | N | N | N | N | N | P ? | P | N | N | N | N |
| MP4 moov rebuild | N | N | N | N | N | N | N | Y (needs reference) | Y (needs reference) | N | N | N | Y | N | N | P ? | Y (sample file) | N | N | N | N |
| HEIC/HEIF support | n/a | n/a | n/a | n/a | ? | N | N | N | N | N | Y ? | Y ? | Y ? | ? | Y ? | Y ? | Y ? | n/a | n/a | n/a | n/a |
| Camera RAW (CR2/NEF/ARW/DNG) | n/a | n/a | n/a | n/a | Y (CR2 via TIFF family) | P (config) | N | N | N | N | Y | Y | Y | P | Y | Y | Y | n/a | n/a | n/a | n/a |
| Report (HTML/JSON/log) | P (mapfile only) | P (log) | P | P | P (report.xml) | P (audit.txt) | Y (feature files) | N | N | N | Y (GUI) | Y (GUI) | Y (GUI) | P | Y (GUI) | Y (GUI) | Y (GUI) | H | N | P | N |
| Open source | Y | Y | Y | Y | Y | Y | Y | Y | N | N | N | N | N | N | N | N | N | N | Y | Y | Y |
| Runs on Linux | Y | Y | Y | Y | Y | Y | Y | Y | Y (legacy) | N | Y | Y | P (Standard: Y; Video: Win only) | N | N | N | N | Win | Y | Y | Y |

Observations that drive priorities (section 1.5):
1. No open tool combines device-aware imaging with FS parsing and carving.
2. No open tool rebuilds MP4 `moov` without a reference file.
3. Fragmented video reassembly is commercial-only (UFS Explorer, EUR 8k/yr).
4. JPEG repair is commercial-only (JPEG-Repair) or manual.
5. Mapfile-driven validation ("this JPEG contains sectors marked `-`") exists nowhere.

### 1.4 Real-world practice DiskRescue must encode

Order of operations (field consensus, matches the ddrescue manual, Gillware, Rossmann,
DiskTuna and CGSecurity guidance):

1. Stop using the device. Every mount, every "repair" prompt, every retry burns the
   drive's remaining life (HDD) or risks firmware lock-up (flash).
2. Do not mount. "Never try to rescue a r/w mounted partition... It is best that the
   device or partition to be rescued is not mounted at all, not even read-only."
   (ddrescue manual, "Using ddrescue safely").
3. Never write to the source. Never run `chkdsk`, `fsck`, Windows "Scan and fix", or
   accept "You need to format the disk". "Never try to repair a file system on a drive
   with I/O errors; you will probably lose even more data." (ddrescue manual). On RAW
   volumes, "ignore Windows prompts to format the disk or run CHKDSK" (Gillware).
4. Image first, with a mapfile, to a healthy destination with 5-10 % spare space;
   image the most valuable region first if the drive is dying (ddrescue manual
   example: `-i0 -s50MiB`, then key areas, then the rest).
5. Work only on the image. All FS parsing, carving and repair read the image.
6. Clone with the right sector/cluster size: 512 B logical sectors for most HDD/USB,
   4096 for 4Kn drives and many NVMe namespaces; cluster (read) size 64 KiB default,
   smaller on slow drives ("Try smaller values for slow drives", ddrescue manual).
7. Use direct I/O (`O_DIRECT`) so the kernel cache cannot hide read failures and so
   read-ahead cannot touch bad areas.
8. Stop and go to a lab when: clicking/knocking HDD (head crash in progress), device
   enumerates with wrong capacity (controller dead), SMART-reported pending sectors
   climbing during imaging, or imaging rate collapses after errors (drive stuck in
   internal error recovery). Every retry here reduces what a lab can get.

Why SSD TRIM makes deleted-file recovery usually impossible but existing-file recovery
still works: TRIM is "just another command ... only TRIM is 'deallocate'". With DRAT
the SSD returns deterministic (usually zero) data for trimmed LBAs; with DZAT it
"guarantees it will return zeros, every time, immediately after TRIM". "As soon as a
block is TRIM'd, most SSDs stop giving you the data from that block -- even if it
hasn't been physically erased yet." Deleting files on ext4 (`discard` or periodic
`fstrim`), NTFS on Windows 7+ and APFS/HFS+ on macOS issues TRIM, so the LBAs of a deleted
photo read back as zeros within seconds; carving finds nothing. Allocated files are
not trimmed, so a failing SSD with read errors or a corrupted FS still yields its
existing files through normal reads, and the FS parsers are the right tool. TRIM is
commonly not passed through by USB-SATA bridges without UASP/UNMAP support, by SD cards
and most USB flash sticks (no TRIM on the USB mass-storage path), which is why deleted
photos on cards are still recoverable while deleted photos on an internal SSD are not.
UNVERIFIED: the exact set of bridges/OS versions that pass UNMAP; treat "deleted files
on an SSD/NVMe: assume gone unless the volume was mounted with discard disabled" as the
rule.

Why SD cards and USB sticks "die at the controller": consumer flash is a NAND die plus
a small controller running an FTL. Controller firmware corruption or power damage leaves
the NAND intact but unaddressable. Recognisable signatures:
- Device enumerates but reports a tiny capacity: thumb drive "detecting as 8MB"
  (Recovery Force), SD cards stuck at 30.6 MB / "cannot get LBA access further than
  30MB" (HDDGURU forum: "typical fault of the controller requiring physical recovery").
- Generic identity strings ("USB DISK", "Generic Flash Disk", VID/PID of the controller
  vendor rather than the brand), 0-byte capacity, or "no media"/"needs to be
  initialized".
- Card is read-only and every write fails; reads return constant data.
Recovery path is chip-off (desolder NAND, raw dump, ECC/scrambler/XOR and FTL
translation reconstruction) or, for monolith microSD, grinding to test pads. No
software path exists; DiskRescue must detect these signatures and print the lab warning
instead of imaging garbage.

Why mounted sources are dangerous: a mounted FS replays the journal (ext4/NTFS), updates
atimes, rewrites the dirty bit and free-space bitmaps (exFAT), triggers TRIM, and on
Windows prompts chkdsk -- all writes to a device whose next write may be its last, and
all changes to metadata the recovery relies on.

When to give up and go to a lab: see 4.3 symptom table rows marked LAB.

### 1.5 Prioritised feature list

v1 (must ship):
1. Device-aware imaging engine: `O_DIRECT` reads, per-device profiles (HDD, SATA SSD,
   NVMe, USB stick, SD/MMC), ddrescue-compatible mapfile read/write/resume, 5-phase
   algorithm (copy fwd/back with skipping, trim, sweep, scrape, retry), per-read
   timeouts measured in user space with `/sys/block/<dev>/device/timeout` advice, read
   rate and error rate monitoring, "abort if rate collapses" guard, reverse mode.
2. Source safety: refuse mounted sources, refuse output on the source device, refuse
   to open source `O_RDWR`, detect non-root and explain, detect wrong-capacity
   controller failure, print the SSD TRIM note.
3. Partition + FS parsers (MBR/GPT, FAT12/16/32, exFAT, NTFS, ext4) with names,
   timestamps, deleted entries, plus backup-structure fallback (FAT2, exFAT backup boot,
   NTFS `$MFTMirr`, ext4 backup superblocks/group descriptors).
4. Carver for JPEG, PNG, HEIC/HEIF (ISOBMFF `ftyp heic/mif1`), MP4/MOV/3GP, AVI,
   CR2/NEF/ARW/DNG (TIFF family) with format-aware end detection, cluster-aligned scan,
   and mapfile-aware validation (file touches bad sectors => "damaged" tag, not silent).
5. Repair: JPEG (header donor from same-camera good file found on the same image;
   truncation detection; marker resync), MP4/MOV moov rebuild with and without a
   reference (SPS/PPS-driven NAL walk), PNG chunk CRC/IEND fix, AVI index rebuild.
6. Report: JSON + HTML with mapfile picture (ddrescueview colours), per-file status
   (intact / repaired / damaged / unrecoverable), source identity, SMART-free health
   summary (read rate curve, error map).

Later:
7. Bifragment gap carving for JPEG and MP4 (Garfinkel 2007 algorithm) and
   two-stream interleave reassembly for cameras writing two files at once.
8. SCSI/ATA passthrough (`SG_IO`) with controller-level timeouts, `READ VERIFY`
   and `READ FPDMA` fine control; USB relay power cycling.
9. Virtual-disk mode (NBD export of the partially imaged source, on-demand imaging).
10. HEVC/H.265 and AV1 stream walkers; proprietary dashcam/CCTV containers.
11. Windows port of the imaging engine (`\\.\PhysicalDriveN`, `FILE_FLAG_NO_BUFFERING`,
    `IOCTL_STORAGE_QUERY_PROPERTY`).
12. PhotoRec-scale signature DB for non-media formats.

### 1.6 Exact wording of warnings

Print to stderr, prefixed by the severity word, each wrapped to 78 columns. The text in
backticks is the literal string. `{dev}`, `{mnt}`, `{out}` are substituted.

- Mounted source (abort unless `--i-know-the-source-is-mounted`):
  `ERROR: {dev} is mounted at {mnt}. Reading a mounted device gives an inconsistent image and the mount itself can write to the failing media. Unmount it (umount {mnt}) or power the device off and on without letting the OS mount it, then retry. DiskRescue will not continue.`
- Output on source (always abort):
  `ERROR: the output {out} lives on the source device {dev}. Writing the image or recovered files onto the device being rescued overwrites the data you are trying to save. Choose a destination on a different physical disk.`
- Non-root:
  `WARNING: not running as root. Raw block devices are normally readable only by root, so DiskRescue may be unable to open {dev}, read its sector size, or use direct I/O. Re-run with sudo (recommended) or run against an image file produced by someone with root.`
- SSD TRIM note (printed once when the device is detected as SSD/NVMe and a deleted-file
  or carve operation is requested):
  `NOTE: {dev} is a solid-state device. Most SSDs discard the contents of deleted files immediately (TRIM / deterministic read after TRIM), so carving for deleted photos usually returns nothing. Files that still exist in the file system are unaffected and will be recovered normally.`
- Failing device safety (printed at the start of every imaging run):
  `WARNING: {dev} shows read errors or slow reads. Every read attempt can make a failing device worse. DiskRescue will never write to it, will image good areas first and retry bad areas last, and will stop if the device stops responding. Do not run chkdsk, fsck, "Scan and fix", or accept any "format this disk" prompt. If the drive clicks, knocks, or is not detected with its full capacity, stop now and contact a data recovery laboratory.`
- Wrong-capacity / dead-controller detection:
  `ERROR: {dev} reports {size} ({model}). A flash device that enumerates with a tiny or zero capacity or a generic name has usually lost its controller or firmware, not its data. Software cannot read the NAND through a dead controller. Do not reformat or "initialize" it; a laboratory can read the memory chips directly (chip-off).`
- Read-only destination too small:
  `ERROR: destination has {free} free but the image needs {need} (source size plus 5% for the mapfile and report). Free space or choose another destination.`

### 1.7 Fact-check status (2026-10-12)

Resolved from primary sources (tags removed, text corrected where needed): OpenSuperClone
defaults, log layout and ddrescue import/export; dd_rescue `-o bbfile`; safecopy preset
strings and marker default; PhotoRec HEIC, `b` prefix, 500 files per directory and the
sector-number file names; Scalpel GPU status; bulk_extractor licence; Recuva tiering;
extundelete and ext4magic licences; ATA IDENTIFY words 169/69; NVMe DLFEAT byte 33;
`eh_timeout`; `O_DIRECT` vs read-ahead; sysfs location of `max_retries`; NVMe metadata
format without root; Garfinkel 2007 per-type table (AVI is 20 %, not 22 %) and gap
tables; zune-jpeg partial decode; h264-reader currency; ntfscp destination rules;
`mkntfs -C`; all crate versions.

Still UNVERIFIED (no primary source found): the exact set of USB bridges/OS versions
that pass UNMAP (1.4); the 8 MB / 30.6 MB dead-controller capacities (forum reports only,
2.6); the benefit of the HDD cool-down pause (3.2); a fragmentation-rate study for
consumer camera cards (4.2); Scalpel3 code availability (1.2).

---

## 2 Exact on-disk / wire details

### 2.1 GNU ddrescue mapfile format (must be bit-compatible)

Text file; `#` at beginning of line or after whitespace starts a comment to end of
line. Three parts: heading comments, status line, block list.

Heading comments (informational, DiskRescue writes equivalents with its own name):
```
# Mapfile. Created by GNU ddrescue version 1.30
# Command line: ddrescue -d -c18 /dev/fd0 fdimage mapfile
# Start time:   2015-07-21 09:37:44
# Current time: 2015-07-21 09:38:19
# Copying non-tried blocks... Pass 1 (forwards)
```

Status line = first non-comment line: `<current_pos> <current_status> <current_pass>`.
`current_pos` is the position being tried (start of block in a forward pass, end of block
in a backward pass). `current_pass` is a positive decimal integer (must be decimal even
when other numbers are hex).

| Status char | Meaning |
|---|---|
| `?` | copying non-tried blocks |
| `*` | trimming non-trimmed blocks |
| `%` | sweeping non-tried blocks |
| `/` | scraping non-scraped blocks |
| `-` | retrying bad sectors |
| `F` | filling the blocks specified |
| `G` | generating approximate mapfile |
| `+` | finished |

Block lines: `<pos> <size> <status>`; blocks "must be contiguous and non-overlapping".
Integers may be decimal, hexadecimal (`0x...`) or octal with C++ syntax.

| Block status | Meaning |
|---|---|
| `?` | non-tried block |
| `*` | failed block non-trimmed |
| `/` | failed block non-scraped |
| `-` | failed block bad-sector(s) |
| `+` | finished block |

Example (from the manual):
```
# current_pos  current_status  current_pass
0x00120000     ?               1
#      pos        size  status
0x00000000  0x00117000  +
0x00117000  0x00000200  -
0x00117200  0x00001000  /
0x00118200  0x00007E00  *
0x00120000  0x00048000  ?
```
Before 1.20 the file was called "logfile"; same format. A `.bak` copy is written
periodically. Save interval defaults to 30 s - 5 min depending on size
(`--mapfile-interval`). Sub-sector blocks found when loading are joined to their sector
with the "less processed" state. Output for bad sectors: untouched in the output file;
zeros if the output is a regular file created by ddrescue.

### 2.2 GNU ddrescue defaults and exit status (manual, version 1.30 era)

| Option | Default |
|---|---|
| `-b, --sector-size` | 512 (not auto-detected; manual: "usually 512 for hard discs and 3.5" floppies, 1024 for 5.25" floppies, and 2048 for cdroms", so pass `-b 2048` for CD-ROM) |
| `-c, --cluster-size` | 64 KiB / sector_size (= 128 sectors at 512 B) |
| `-K, --skip-size=[initial][,max]` | initial = infile_size / 32768, minimum 64 KiB; max = 1 % of infile size; doubled on each error/slow read, reset to initial on good data; valid 64 KiB - 1 EiB; `0` disables skipping |
| `-r, --retry-passes` | 0 (`-1` = infinite; direction reversed each pass) |
| `-T, --timeout` | infinity (time since last successful read) |
| `-a, --min-read-rate` | unset; applies to the first two copying passes |
| `-l, --max-slow-reads` | infinity (needs `-a`) |
| `-e, --max-bad-areas` | infinity (`+n` = new ones this run) |
| `-E, --max-error-rate` | infinity, bytes/s of failed reads |
| `--pause-on-error`, `--pause-on-pass` | 0 |
| `--delay-slow` | 30 (s) |
| `-d, --idirect` | off (`O_DIRECT` on input; sector size must be right or an unaligned read error exits 1) |
| `-D, --odirect` | off |
| `-R, --reverse` | off |
| `-n, --no-scrape`, `-N, --no-sweep`, `--no-trim` | off (manual: `--no-trim` "not recommended because trimming has a high probability of finding good data"; `-N` "may be useful when the drive has a dead head") |
| `-O, --reopen-on-error` | off (close/reopen after each error; manual warns slowdown is usually the kernel retrying) |
| `-J, --check-on-error` | off (re-read last good sector; exit 2 if inconsistent: for drives that "return garbage data after finding errors") |
| `-H, --test-mode=file` | fault injection: blocks not `+` in `file` are treated as read errors |
| `-C, --complete-only` | limit domain to mapfile blocks (devices of undefined size) |
| `-m, --domain-mapfile` | restrict to blocks finished in another mapfile |
| `-P, --data-preview` | 3 lines |

Exit status: 0 normal; 1 environmental problems (file not found, invalid options, I/O
errors); 2 corrupt or invalid input file; 3 internal consistency error.

Phase summary (manual, section 4): copying (up to 4 passes: pass 1 forward skipping
errors and slow areas; pass 2 backward delimiting skipped blocks; passes 3-4 re-read
areas skipped for slowness, in the direction they were skipped; passes 2-4 "skip the
rest of the block after finding the first error in the block"), trimming (forward one
sector from the leading edge until bad, backward from trailing edge until bad; rest
becomes non-scraped), sweeping (copy non-tried blocks skipped due to errors, skipping
disabled), scraping (one sector forward through non-scraped), retrying (bad sectors,
each once per pass, direction alternates). "Each sector is tried at most two times:
the first in the copying or sweeping phases as part of a large block read, the second
during trimming, scraping, or retrying as a single sector read" (with `-r 0`; each retry
pass adds one more single-sector attempt).

### 2.3 OpenSuperClone / HDDSuperClone (for interoperability notes)

- Phases: 1 forward copy, skip on error; 2 backward copy, skip on error; 3 forward copy,
  skip on read rate; 4 forward copy, no skipping; Trimming; Dividing (only when trimming
  disabled); Scraping; Retrying. Mapping to ddrescue marks: phases 1-4 = copying
  passes; trimming = trimming; scraping = scraping; retrying = retrying.
- Progress log (`write_logfile_ccc` in `src/opensuperclone/opensuperclone.c`): text,
  `#` comment lines. Header `# Disk progress log file created by OpenSuperClone version
  X`, `# <date>`, `# command= ...`; a settings block between `# startconfig` and
  `# endconfig` (lines `# key \tvalue`: `softtimer`, `busytimer`, `skiptimer`,
  `rateskip`, `clustersize`, `blocksize`, `sectorsize`, `retries`, `size`, `minskip`,
  `maxskip`, `reverse`, `model`, `serial`, ...); optional analyze / SMART / identify
  blocks; then `# current position \t \tstatus` followed by one line
  `0x%06llx \t \t0x%llx`, and `# position \tsize \tstatus \tinfo \terr/status/time`
  followed by one line per block `0x%06llx \t0x%06llx \t0x%llx \t0x%llx \t0x%llx`.
  Position and size are in LBAs (sectors), not bytes. Status codes
  (`opensuperclone.h`): `NONTRIED 0x00`, `NONTRIMMED 0x10`, `NONDIVIDED 0x20`,
  `NONSCRAPED 0x30`, `BAD 0x40`, `FINISHED 0x7f`, plus the `BAD_HEAD 0x80` bit. Its
  ddrescue export (`write_ddrescue_logfile_ccc`) writes byte positions and a
  two-field status line (`# current_pos  current_status`, no `current_pass`), so
  DiskRescue's mapfile parser must accept a missing pass number (treat as 1), as it must
  for pre-1.20 logfiles. DiskRescue will not parse the native log in v1.
- Direct modes require the drive hidden from the kernel (e.g. unbinding the AHCI port)
  and the controller in AHCI mode.

### 2.4 Linux kernel timeouts and retries on the block path (why ddrescue "hangs")

From `drivers/scsi/sd.h` (mainline, 2026): `SD_TIMEOUT (30 * HZ)` = 30 s per command,
"user modifiable via sysfs but initially set to SD_TIMEOUT"
(`/sys/block/sdX/device/timeout`); `SD_MAX_RETRIES 5`. The `max_retries` attribute is
on the scsi_disk class device, not the scsi_device: `/sys/class/scsi_disk/<H:C:T:L>/max_retries`
= `/sys/block/sdX/device/scsi_disk/<H:C:T:L>/max_retries` (`sd.c` `max_retries_store`
accepts -1 = `SCSI_CMD_RETRIES_NO_LIMIT` (unlimited) up to 5). A single unreadable
64 KiB cluster can therefore cost up to (1 + 5) x 30 s = 180 s before `read(2)` returns
`EIO` when the command hangs; a drive that reports MEDIUM ERROR with valid sense data is
failed immediately without mid-layer retries (`scsi_io_completion_action` falls to its
`default: action = ACTION_FAIL` branch; libata `ata_eh_worth_retry` returns 0 for
`AC_ERR_MEDIA`), so its cost is the drive's own internal recovery time. On top of the
command timeout, the SCSI error handler issues its own recovery commands (TEST UNIT
READY / REQUEST SENSE) bounded by `/sys/block/sdX/device/eh_timeout`
(`SCSI_DEFAULT_EH_TIMEOUT` = 10 s, `include/scsi/scsi.h`) and libata adds device and
link resets. USB mass storage
sits on top of the same SCSI path, and the bridge chip may reset the device on timeout
(re-enumeration, device node may change: ddrescue's advice to use `--ask` and check
model/serial applies).

Drive-internal error recovery: desktop HDDs may spend tens of seconds to minutes in
internal retries per sector; enterprise drives expose TLER/ERC/CCTL with a 7 s default
("Windows times out commands after 7 seconds, so drives generally try to respond before
that happens"). TLER is set with `smartctl -l scterc,70,70` (not available on the dev
box) or ATA `SCT ERROR RECOVERY CONTROL`; consumer drives often ignore it.

Practical consequences for DiskRescue's defaults (profile table in 3.2):
- Lower `/sys/block/sdX/device/timeout` to 7-10 s and
  `/sys/class/scsi_disk/<H:C:T:L>/max_retries` to 0 when root (restore on exit); print
  the commands when not root.
- Use `O_DIRECT` and never `readahead`; set `/sys/block/sdX/queue/read_ahead_kb` to 0
  when root (restore on exit). Read-ahead exists only in the buffered read path
  (`filemap_read` -> `page_cache_sync_ra`, `mm/filemap.c` / `mm/readahead.c`); an
  `O_DIRECT` read of a block device goes through `blkdev_direct_IO` (`block/fops.c`) and
  never triggers it. Setting `read_ahead_kb` to 0 is still cheap insurance against
  other processes (udev probes, desktop indexers) touching the device via the page cache.
- Treat any read slower than the profile's `slow_read_ms` as a "slow read" and skip,
  exactly like ddrescue's `--min-read-rate` and OpenSuperClone phase 3.

### 2.5 SSD TRIM detection (ATA IDENTIFY DEVICE)

From ATA/ACS: word 169 bit 0 = DATA SET MANAGEMENT (TRIM) supported; word 69 bit 14 =
"Deterministic read after TRIM supported" (DRAT); word 69 bit 5 = "read zeroes after
TRIM supported" (RZAT/DZAT). `hdparm -I` prints `Data Set Management TRIM supported`,
`Deterministic read data after TRIM`, `Deterministic read ZEROs after TRIM`.
Confirmed against the Linux kernel `include/linux/ata.h`: `ATA_ID_DATA_SET_MGMT = 169`
(bit 0, tested by `ata_id_has_trim`, which additionally requires ATA major version >= 7)
and `ATA_ID_ADDITIONAL_SUPP = 69` with mask `0x4020` (bit 14 DRAT + bit 5 RZAT) tested by
`ata_id_has_zero_after_trim`; hdparm `identify.c` labels word 69 bit 14 "Deterministic
read data after TRIM" and bit 5 "Deterministic read ZEROs after TRIM", and prints "Data
Set Management TRIM supported" only when word 169 bit 0 is set and word 169 != 0xFFFF
(treat 0xFFFF as invalid). Reading IDENTIFY needs `SG_IO`/`HDIO_DRIVE_CMD` (root). NVMe:
Identify Namespace DLFEAT is byte 33 (`struct nvme_id_ns`, `include/linux/nvme.h`:
nsze/ncap/nuse bytes 0-23, nsfeat 24, nlbaf 25, flbas 26, mc 27, dpc 28, dps 29, nmic 30,
rescap 31, fpi 32, dlfeat 33); bits 2:0 = read behaviour of deallocated blocks (0 = not
reported, 1 = all bytes 0x00, 2 = all bytes 0xFF), bit 3 = Write Zeroes may deallocate,
bit 4 = guard field CRC for deallocated blocks (libnvme `enum nvme_id_ns_dlfeat`).
Without root,
DiskRescue falls back to `/sys/block/<dev>/queue/rotational` (0 = SSD) and
`/sys/block/<dev>/queue/discard_granularity` (> 0 = discard supported) to decide
whether to print the TRIM note.

### 2.6 Device-type identification from sysfs (no root needed)

| Attribute | Use |
|---|---|
| `/sys/block/<dev>/queue/rotational` | 1 = HDD, 0 = SSD/flash |
| `/sys/block/<dev>/queue/logical_block_size`, `physical_block_size` | sector sizes for `-b` and alignment |
| `/sys/block/<dev>/size` | capacity in 512 B units; compare with `device/model` for wrong-capacity detection |
| `/sys/block/<dev>/removable` | 1 for USB sticks/card readers |
| `/sys/block/<dev>/device/vendor`, `model`, `rev`, `serial` (via udev `ID_SERIAL`) | identity in the report; "USB DISK"/"Generic" + tiny size => controller-failure heuristic |
| `/sys/block/<dev>/device/timeout`, `max_retries` | kernel timeout tuning (write needs root) |
| `/sys/block/mmcblk0/device/type` (`SD`, `MMC`), `cid`, `csd` | SD/MMC detection; CSD gives card size and class |
| `/sys/block/nvme0n1/device/model`, `/sys/class/nvme/nvme0/` | NVMe |
| `/sys/block/<dev>/device/../../driver` path containing `usb-storage` or `uas` | USB attachment and whether UASP is in use |
| `/proc/mounts`, `/proc/self/mountinfo` | mounted-source refusal; also check holders (`/sys/block/<dev>/holders/`) for dm/md/LVM |

Wrong-capacity heuristic: flash device (`removable=1` or `mmcblk`) with
`size * 512 <= 64 MiB` and model matching `/USB ?DISK|Generic|Flash Disk|^$/i`, or size
exactly 0, => controller failure warning (1.6). UNVERIFIED thresholds; 8 MB and 30.6 MB
are the commonly reported values.

### 2.7 PhotoRec output conventions (for migration/compat)

Output directories `recup_dir.1`, `recup_dir.2`, ... each holding up to 500 files
(`MAX_FILES_PER_DIR 500`, `DEFAULT_RECUP_DIR "recup_dir"` in `src/photorec.h`);
filenames are `%c%07lu.<ext>` (`set_filename` in `src/photorec.c`): prefix `f`, then
the start sector relative to the partition start,
`(location.start - part_offset) / sector_size`, zero-padded to 7 digits (e.g.
`f0012345.jpg`); `t` prefix for EXIF thumbnails extracted from JPEGs (`file_jpg.c`);
`b` prefix is used for files written while "Keep corrupted files" is on (`broken` =
`STATUS_EXT2_ON/OFF_SAVE_EVERYTHING`), not for brute-forced files; `report.xml` (DFXML)
lists each carved file with byte runs and is written only when built with DFXML enabled
(configure default is enabled; `--disable-dfxml` turns it off).
DiskRescue should offer `--photorec-names` for users with existing scripts.

### 2.8 safecopy badblocks list format

Plain text, one block number per line, in units of the `-b` block size; compatible
with `e2fsck -l`/`badblocks` output; `-o file` writes it; `-I file` = incremental mode
("assume the target file already exists and has holes specified in the badblockfile",
i.e. only listed blocks are re-read), `-X file` excludes listed blocks (overrides `-I`).
No marker string by default (unreadable data is left as a hole); `-M string` enables
marking, and `--stage1` passes `-M BaDbLoCk`, so `BaDbLoCk` is the marker to look for
in stage-1 outputs.

---

## 3 Algorithms (pseudocode)

### 3.1 Triage before imaging

```
fn triage(dev) -> Plan:
    id = read_sysfs_identity(dev)            # 2.6
    if is_mounted(dev) or has_holders(dev):  abort(MOUNTED_SOURCE)
    if output_on(dev):                        abort(OUTPUT_ON_SOURCE)
    if id.size_bytes == 0 or (id.is_flash and id.size_bytes <= 64 MiB and id.model_generic):
        abort(DEAD_CONTROLLER)               # 1.6 wrong-capacity text
    profile = match id:
        rotational && !removable      => HDD
        !rotational && nvme           => NVME
        !rotational && sata           => SATA_SSD
        mmcblk                        => SD_MMC
        removable && usb-storage/uas  => USB_FLASH   # may also be a USB HDD: check rotational
    if profile in {NVME, SATA_SSD}: print(TRIM_NOTE) when deleted/carve requested
    probe = timed_read(dev, 0, 64 KiB) ; timed_read(dev, size/2, 64 KiB) ; timed_read(dev, size-64KiB, 64 KiB)
    if any probe errored or took > profile.slow_read_ms: print(FAILING_DEVICE_WARNING)
    return Plan{profile, sector=id.logical_block_size, cluster=profile.cluster, ...}
```

### 3.2 Device profiles (defaults; all overridable)

| Profile | cluster | initial skip | max skip | slow_read_ms | per-read timeout (user space) | retry passes | reverse pass | notes |
|---|---|---|---|---|---|---|---|---|
| HDD | 64 KiB (ddrescue default) | 64 KiB or size/32768 | 1 % of size | 1000 | 10 s, then mark slow + skip | 0 in first run, 3 in `--retry` run | yes (pass 2) | minimise head movement; sweep after trim; pause 2 s after 10 consecutive errors (drive cool-down) UNVERIFIED benefit |
| SATA SSD | 1 MiB | 1 MiB | 1 % | 200 | 5 s | 1 | yes | no mechanical cost to seeks; errors are page/block-level, usually 4-16 KiB granular: scrape at 4 KiB not 512 B |
| NVMe | 1 MiB | 1 MiB | 1 % | 100 | 5 s | 1 | no | use `logical_block_size` (often 4096); controller timeouts are short; large queue depth irrelevant for sequential rescue |
| USB flash | 256 KiB | 256 KiB | 1 % | 500 | 8 s | 2 | yes | bridge resets re-enumerate the device: re-open by serial, not by path; skip aggressively on timeouts |
| SD/MMC | 128 KiB | 128 KiB | 1 % | 500 | 8 s | 2 | yes | card readers often hang hard: cap total stall time; prefer `mmcblk` (native host) over USB reader |

Rationale: ddrescue's 64 KiB cluster is tuned for HDD head movement; flash has no seek
penalty so larger clusters cost nothing on good areas, while error granularity is the
NAND page/erase block, so trimming to 512 B wastes time. Slow-read thresholds mirror
HDDSuperClone phase 3 (rate-based skipping) and ddrescue `--min-read-rate`.

### 3.3 Imaging loop (ddrescue-compatible state machine)

```
state Map { blocks: Vec<(pos, size, Status)>, current_pos, current_status, current_pass }
Status in {NonTried '?', NonTrimmed '*', NonScraped '/', Bad '-', Done '+'}

fn image(src, dst, map, p: Profile):
    load_or_init(map)
    # Phase 1: copying, up to 4 passes
    for pass in 1..=4:
        dir = if pass is odd { Forward } else { Backward }
        for blk in map.blocks_with('?') in dir order:
            pos = blk.start (or end for Backward)
            skip = p.initial_skip
            while pos within blk:
                n = min(p.cluster, remaining)
                t0 = now(); r = pread_direct(src, pos, n) with watchdog(p.timeout)
                match r:
                  Ok(data) and elapsed(t0) <= p.slow_read_ms:
                      write(dst, pos, data); map.mark(pos, n, '+'); skip = p.initial_skip
                  Ok(data) (slow):
                      write(dst, pos, data); map.mark(pos, n, '+')
                      if pass <= 2: pos = advance(pos, skip, dir); skip = min(skip*2, p.max_skip)   # skip slow area
                  Err(EIO) | Err(Timeout):
                      map.mark(failed_sector_range, '*')
                      if pass == 1: pos = advance(pos, skip, dir); skip = min(skip*2, p.max_skip)
                      else: break   # passes 2-4: skip rest of block after first error
                  Err(ENXIO|ENODEV): reopen_by_serial(src) or abort(DEVICE_GONE)
                save_map_if_due()
        if no '?' remain: break
    # Phase 2: trimming
    for blk in map.blocks_with('*'):
        if prev(blk) is '+': read forward 1 sector at a time until error -> mark '+'/'-'
        if next(blk) is '+': read backward 1 sector at a time until error -> mark '+'/'-'
        rest -> '/' unless adjacent to '?'
    # Phase 3: sweeping -- copy '?' left behind by error skipping, skipping disabled, trim each
    # Phase 4: scraping -- each '/' block, one sector forward; failures -> '-'
    # Phase 5: retrying -- for pass in 1..=p.retry_passes: every '-' sector once, direction alternates
    map.current_status = '+' ; save_map()
```
Guards evaluated after every read: `--max-error-rate`, `--max-bad-areas`, `--timeout`
(time since last successful read), rate-collapse guard (average rate over 60 s below 1 %
of the first-minute average on an HDD => pause and print the lab warning; continue only
with `--keep-going`).

Watchdog implementation without passthrough: issue `pread` on a worker thread; if the
profile timeout elapses, the main thread marks the block slow/failed in the map and
continues *scheduling* but must wait for the kernel call to return before issuing the
next read to the same device (there is no way to cancel a blocked `read(2)`); meanwhile
it flushes the map and reports "device stalled for N s" so the user can decide.

### 3.4 Prioritised imaging (what OpenSuperClone's virtual disk mode achieves)

```
fn image_prioritised(src, dst, map, fs_hint):
    # 1. metadata first: partition tables, boot sectors + backups, FAT tables, exFAT bitmap,
    #    $MFT + $MFTMirr, ext4 superblock + GDT + inode tables, directory clusters
    for region in metadata_regions(fs_hint): image_region(region)
    # 2. allocated clusters of media files (from parsed directory tree), largest-first
    for f in files_sorted_by(priority = is_media * size): image_region(f.extents)
    # 3. everything else ('?' remaining)
    image(src, dst, map, profile)
```
This is the single biggest practical gain over plain ddrescue on a dying HDD: the user
gets their photos before the drive finishes dying.

### 3.5 Bifragment gap carving (Garfinkel 2007, for v2)

```
fn bifragment_carve(image, header_pos, footer_pos, validate):
    # object starts at header_pos, ends at footer_pos, but validate(image[header..footer]) failed
    for gap_len in 1.. (in sectors, bounded by max_gap e.g. 64 KiB and by time budget):
        for gap_start in (header_pos+1 .. footer_pos-gap_len) step sector:
            candidate = image[header_pos..gap_start] ++ image[gap_start+gap_len..footer_pos]
            if validate(candidate): return candidate
    return None
```
"Most bifragmented files were split by 1, 2, 4, 8 512-byte sectors", "believed to be
caused by a single-cluster file within the fragmented file" (Pal & Memon slides); the
paper's own Tables 4/5/8 show the common gaps as 4096, 8192, 16,384 and 32,768 bytes,
i.e. 1-8 file-system clusters (8 blocks by far the most common), plus a cluster of tiny
(2-59 byte) gaps that are carver artefacts. So iterate `gap_len` over cluster-size
multiples (1, 2, 4, 8 clusters) first, then sector multiples, and stop early. Validators must be *fast*: JPEG = Huffman decode of the scan with
restart-marker checks; MP4 = chunk-offset table consistency (`stco`/`co64` entries land
on sample boundaries) or, for moov-less files, NAL/ADTS frame continuity.

### 3.6 Symptom to action decision table (encoded in `diskrescue doctor`)

```
if clicking/knocking (user-reported flag) -> LAB, do not image
if size==0 or tiny-capacity flash -> LAB (chip-off), do not image
if read errors + rotational -> HDD profile, prioritised imaging, retry passes 0 first
if read errors + SSD -> SSD profile, existing files via FS parsers, TRIM note
if "RAW" / "need to format" / unknown FS -> image, then boot-sector backup search, then FS parse, then carve
if 0-byte files after camera crash -> FS parse finds entry with size 0; carve data area near the entry's time neighbours; MP4 moov rebuild
if "moov atom not found" -> MP4 repair module (reference-free first, reference if found on same card)
if half-grey JPEG -> truncated: report as damaged, attempt tail recovery from following clusters (bifragment) then accept partial
```

---

## 4 Edge cases and failure modes seen on real failing media

### 4.1 Symptom vs cause table

| Symptom | Most likely cause | Recoverable by software? | DiskRescue action |
|---|---|---|---|
| Clicking / knocking / repeated spin-up on HDD | head crash or head failure, drive retries servo | No; each power-on grinds platters | LAB. Refuse to image unless `--i-accept-platter-damage` |
| Slow reads (KB/s), no errors yet, HDD | drive in internal ECC retry on weak sectors; pending sector count growing | Yes, if imaged now with skipping | HDD profile, rate-based skipping, prioritised imaging |
| I/O errors (EIO) in clusters, HDD | media defects / weak heads | Partially | multi-phase imaging; expect bad areas to grow |
| Device disappears mid-read, reappears (USB) | bridge or controller reset on timeout | Partially | reopen by serial, lower timeout, cap stall budget |
| "RAW" filesystem on Windows / "need to format" | boot sector or `$MFT`/FAT/exFAT boot region unreadable or corrupted (often one bad sector), or wrong partition table | Usually yes | image; read backup boot sector (NTFS last sector of volume, FAT sector 6, exFAT sector 12), `$MFTMirr`; parse; carve fallback |
| Windows ran chkdsk and files became `found.000/*.chk` | chkdsk rewrote metadata | Partially | treat `.chk` as carve input; also carve free space |
| 0-byte files after camera crash | directory entry created at record start with size 0, size written on close; data clusters written but FAT chain/size never finalised | Yes | FAT: follow starting cluster if set, else carve the free area in time order; MP4 moov rebuild |
| "moov atom not found" / player refuses MP4 | camera died before writing `moov` (usually at end for non-faststart recordings) | Yes | moov rebuild (reference-free; use another file from the same card as reference if present) |
| Video plays for N seconds then stops | truncated `mdat` or `moov` tables longer than data | Yes | rebuild tables to actual data length |
| Half-grey JPEG (top half fine, lower part grey) | truncated scan data: file length shorter than image needs (bad sector, interrupted write, carver stopped at wrong footer) | Partial | report damaged; try to extend from following clusters; keep partial |
| JPEG with colour shift / shear from a certain row | missing or inserted bytes in scan data (bad sector filled with zeros, fragmentation gap), MCU misalignment | Partial | locate first bad MCU row, resync at next RSTn marker, blank bad band (JPEG-Repair "stuff bytes" approach); lab/manual for more |
| JPEG opens as thumbnail only / "invalid header" | header (SOI..SOS) overwritten; EXIF thumbnail intact | Yes with donor | header donor from same camera model + same resolution/quality (quant tables) found on the same image |
| Photos recoverable but every one has wrong name/date | carving without FS | n/a | prefer FS parse; if carving, name by EXIF DateTimeOriginal |
| SD card read-only, all writes fail | controller entered read-only safe mode (wear-out or firmware) | Yes, read it now | image immediately, warn that the card is end-of-life |
| Card/USB stick shows 8 MB / 30.6 MB / 0 bytes / "USB DISK" | controller or firmware dead; NAND intact | No | LAB (chip-off) warning |
| SSD not detected or shows 0 GB / wrong model string (e.g. "SATAFIRM S11", "BAD_CTX") | SSD firmware panic / corrupted FTL | No | LAB (firmware-level tools) |
| Deleted photos on SSD, carve finds only zeros | TRIM/DRAT | No | TRIM note; stop carving free space |
| Deleted photos on SD card, carve finds them but many are corrupt | partially overwritten by newer recordings; fragmentation | Partial | validate + damaged tags; bifragment search |
| Drive imaged, FS parses, but files read as zeros in the hole regions | sectors marked `-` in mapfile inside the file extents | n/a | mark file "damaged (N bytes unread)" and list the extents; re-run retry pass targeted at those sectors only |
| Camera formatted the card ("card error, format?") and user said yes | quick format rewrote FAT/exFAT boot + tables, data intact | Yes | FS parse finds nothing; carve whole partition; exFAT: old bitmap gone, so carve cluster-aligned |
| Smartphone internal storage | eMMC/UFS with FBE/FDE encryption | No (without the key) | out of scope; print explanation |

### 4.2 Fragmentation facts (concrete numbers)

Garfinkel 2007 corpus (drives bought on the secondary market 1998-2006, FAT/NTFS/UFS;
Pal & Memon 2009 slides summarising it): "6% of the all files were fragmented";
"~50% of the drives had no fragmented files" (145 of 324 drives at f = 0 %; 42 drives
0 < f < 1 %; 107 drives 1-10 %; 30 drives 10-100 %). Fragment-count distribution:
FAT 1,286,459 contiguous vs 25,154 two-fragment files; NTFS 521,663 vs 22,984
two-fragment, with NTFS showing a much longer tail (590 NTFS files with >1000 fragments
vs 2 on FAT). "Most interesting file types (.doc, .jpg, .pst) files are fragmented
18-57% of the time while 'boring' (.bmp, .ini, .chm) files are commonly fragmented
< 10%"; "Temporary files were fragmented 66% of the time"; "Most bifragmented files were
split by 1, 2, 4, 8 512-byte sectors". Per-type figures from the primary paper (Table 3
"Percent frag.", verified against the PDF at simson.net): JPEG 16 % (108,539 files on
129 drives), AVI 20 % (998 files on 94 drives; Wikipedia's "22 %" is wrong), DOC 17 %,
PST 58 %, MPEG 17 %, MDB 27 %, LOG 31 %, PNF 41 %, TMP 66 %, GIF 8 %, PNG 5 %, BMP 8 %,
INI 10 %. Corpus: "over 300" used drives, 324 file systems with more than five files
(FAT 219, NTFS 51, UFS 5). Gap sizes for all bifragmented files (Table 4): 4096 B /
8 sectors (4272 files), 8192 B (1535), 16,384 B (1344), 2 B (921), 32,768 B (817),
12,288 B (441); for JPEG (Table 5): 2 B (99), 4096 B (88), 16,384 B (40), 8192 B (38),
651,264 B (38), 59 B (23), 32,768 B (16).

Caveats for DiskRescue's domain: that corpus is pre-2007 system disks, not camera cards.
On camera media fragmentation arises from (a) deleting some clips then recording into the
holes, (b) cameras writing two files simultaneously (GoPro `.MP4` + `.LRV`/`.THM`,
dashcam front/rear channels, raw+JPEG pairs), (c) FAT32's 4 GiB file limit causing
chaptered files, (d) wear-levelling (irrelevant at LBA level: the FTL hides it).
UNVERIFIED: no published fragmentation rate for consumer camera cards was found; the
test suite in section 5 therefore generates its own fragmented fixtures at 0 %, 10 %,
25 % and 50 % of files.

### 4.3 Timeout and retry numbers to remember

| Item | Value | Source |
|---|---|---|
| Linux SCSI disk command timeout | 30 s (`SD_TIMEOUT`), sysfs tunable | kernel `drivers/scsi/sd.h` |
| Linux SCSI disk retries | 5 (`SD_MAX_RETRIES`), sysfs `max_retries` -1..5 | kernel `drivers/scsi/sd.h`, `sd.c` |
| Worst case per failed cluster via kernel | about 6 x 30 s = 180 s plus EH resets (hanging commands only; MEDIUM ERROR with sense is not retried by the mid-layer) | derived from `sd.h`, `scsi_lib.c`, `libata-eh.c` |
| Enterprise HDD TLER/ERC default | 7 s read / 7 s write | WD/kernel list discussion |
| Windows storage stack command timeout | 7 s (reason drives target < 7 s) | LKML TLER thread |
| ddrescue own timeout | none (`--timeout` default infinity) | ddrescue manual |
| ddrescue cluster | 64 KiB | ddrescue manual |
| ddrescue initial skip | max(64 KiB, size/32768); max 1 % of size | ddrescue manual |
| HDDSuperClone skip-threshold "disable" value | 60000 ms | HDDSuperClone notes |
| OpenSuperClone defaults | skip threshold 1000 ms (phases 1-2), rate skip 50 kB/s (phase 3), skip size 2 MiB, soft-reset (read) timer 8 s, busy-wait 20 s | `src/opensuperclone/opensuperclone.h`, `common.h` |
| SCSI error-handler command timeout | 10 s (`eh_timeout`, `SCSI_DEFAULT_EH_TIMEOUT`) | kernel `include/scsi/scsi.h` |
| Disk Drill multipass block sizes | 64 KB halving to 512 B; 5-8 attempts | CleverFiles docs |
| safecopy stage1/2/3 | `-f 10% -r 10% -R 1 -Z 0 -L 2 -M BaDbLoCk` / `-f 128* -r 1* -R 1 -Z 0 -L 2` / `-f 1* -r 1* -R 4 -Z 1 -L 2` (each with `-o stageN.badblocks`, stages 2-3 with `-I` of the previous list) | safecopy man page |
| ddrescue mapfile save interval | 30 s - 5 min by size | ddrescue manual |
| Destination headroom | 5-10 % above source size | CleverFiles docs; DiskRescue uses 5 % |

### 4.4 Device behaviours that break naive imagers

- HDD returns garbage instead of EIO after an error (manual's reason for `-J`):
  verify with a re-read of the last good sector; if inconsistent, pause and advise a
  power cycle.
- Drive stops responding entirely after N errors (firmware hang): the only remedy is a
  power cycle; without a relay, DiskRescue must save the map and tell the user to
  unplug/replug and rerun (resume is free with the map).
- USB bridge re-enumerates as a new `/dev/sdX` letter: track by serial/WWN, never by
  path; refuse to continue if the serial differs.
- SD card in a cheap USB reader hangs the whole USB bus; prefer native `mmcblk` hosts;
  cap the total stall budget (e.g. 10 min) then stop with advice.
- Drive reports a smaller size after reset (HPA/DCO or firmware fallback): use the
  mapfile's recorded size (`-C`-like behaviour) and warn.
- 4Kn or 512e: logical 512 with physical 4096 means an unreadable physical sector shows
  as 8 consecutive 512 B failures; scrape at physical size to save time.
- NVMe namespaces with metadata/PI formats: reads of unformatted LBAs return errors
  that look like media errors; detect `nvme` + an LBA format with metadata without
  root via `/sys/block/nvme0n1/metadata_bytes` (per-LBA metadata size,
  `drivers/nvme/host/sysfs.c`; 0 = no metadata) and `/sys/block/nvme0n1/integrity/format`
  (`none` or e.g. `T10-DIF-TYPE1-CRC`), next to `queue/logical_block_size`.

### 4.5 Media-level corruption classes (what repair modules will meet)

JPEG (DiskTuna classification): corrupt header needing a donor; invalid/garbled markers;
truncation (grey bottom); bad-sector bands (zero-filled runs inside scan -> colour shift
and shear until next RST marker); EXIF-only remnants where only the thumbnail survived;
carved files that overran into the next file (trailing garbage after EOI is harmless;
missing EOI is not).

MP4/MOV: moov missing (unfinalised); moov present but `stco/co64` offsets wrong (file
moved/concatenated by a bad carver); `mdat` truncated; two recordings interleaved
(fragmentation); `mdat` size field 0 or 1 (64-bit `largesize`) misparsed by naive
carvers; fragmented MP4 (`moof`) from phones (Android CameraX, iOS) where there is no
single moov; HEVC (`hvc1`/`hev1`) vs AVC sample entry; audio-only AAC chunks every N
video frames (used to classify chunks during rebuild).

AVI: missing `idx1` (dashcams): rebuild from `00dc`/`01wb` chunk walk; `movi` list
size wrong after truncation.

PNG: truncated at a chunk boundary (missing `IEND`), CRC mismatch after bit flips,
IDAT inflate failure at the hole offset.

HEIC: ISOBMFF with `meta`/`iloc`/`iinf`; item offsets absolute: carved copy must be
rewritten or offsets rebased; tiles (`grid`) mean a hole kills one tile, not the whole
image.

---

## 5 Test fixture strategy (only mkfs.vfat, mkfs.exfat, mkntfs + ntfscp, mkfs.ext4 -d, debugfs, ffmpeg, Pillow)

All fixtures are regular files; no loop devices, no root. A `BlockSource` trait
abstracts the device, so a `FaultyImage { data, map }` test double replaces `/dev/sdX`
and injects errors, slowness and stalls exactly like ddrescue's `--test-mode`.

### 5.1 Media samples (deterministic, small)

```sh
# JPEGs with EXIF, two "cameras" (different quant tables) via Pillow quality
python3 - <<'EOF'
from PIL import Image
import random
random.seed(1)
for i in range(20):
    im = Image.effect_noise((640, 480), 64).convert("RGB")
    im.save(f"fx/camA_{i:02}.jpg", quality=85, exif=b"Exif\0\0" + b"MM\0*\0\0\0\x08\0\0")  # minimal TIFF header
    im.save(f"fx/camB_{i:02}.jpg", quality=60, progressive=False, subsampling=2)
im.save("fx/p1.png"); im.save("fx/t1.tif")   # PNG, TIFF (stand-in for RAW family tests)
EOF
# MP4 (moov at end = camera style) and faststart (moov first = reference style), H.264 + AAC
ffmpeg -y -f lavfi -i testsrc2=size=320x240:rate=30 -f lavfi -i sine=frequency=440 \
   -t 5 -c:v libx264 -g 30 -c:a aac fx/ref.mp4
ffmpeg -y -i fx/ref.mp4 -c copy -movflags +faststart fx/ref_faststart.mp4
ffmpeg -y -i fx/ref.mp4 -c:v libx265 -c:a aac fx/ref_hevc.mp4
ffmpeg -y -i fx/ref.mp4 -c copy fx/ref.mov
ffmpeg -y -i fx/ref.mp4 -c:v mjpeg -q:v 5 -c:a pcm_s16le fx/dashcam.avi
# fragmented MP4 (phone style, no single moov)
ffmpeg -y -i fx/ref.mp4 -c copy -movflags frag_keyframe+empty_moov fx/frag.mp4
```

### 5.2 Corruption generators (Python, byte surgery)

- "moov atom not found": `head -c $((size - moov_size)) fx/ref.mp4 > fx/nomoov.mp4`
  where `moov_size` is read by a tiny box walker (moov is the last top-level box of a
  non-faststart ffmpeg output). Also `fx/nomoov_zeros.mp4` = same with the last 1 MiB of
  `mdat` zero-filled (the recover_mp4 failure case).
- Truncated JPEG (half grey): `head -c 50%`. Bad-sector band: overwrite 512 bytes at
  40 % of the scan with zeros. Header loss: overwrite bytes 0..0x400 with zeros, keep the
  rest (donor test uses `camA_00.jpg` as donor for `camA_07.jpg`).
- PNG: truncate before `IEND`; flip one bit in an IDAT chunk.
- AVI: truncate before `idx1`.
- 0-byte camera crash: see 5.4.

### 5.3 Filesystem images without root

| FS | Create and populate | Deleted entries | Fragmentation |
|---|---|---|---|
| ext4 | `mkfs.ext4 -q -F -d fx/tree -b 4096 img/ext4.img 64M` | `debugfs -w -R "rm /photos/camA_03.jpg" img/ext4.img`; also `debugfs -w -R "write fx/x.jpg /photos/new.jpg"` to overwrite freed blocks | `debugfs -w` sequences: write A (large), write B, `rm` B, write C larger than B's hole, `rm` A, write D > hole: D becomes bifragmented. Verify with `debugfs -R "stat /photos/D"` extents |
| FAT12/16/32 | `mkfs.vfat -F 32 -S 512 -s 8 -C img/fat32.img 65536` creates an empty image; populate with `tests/fatput.py` (test-only writer: allocate contiguous clusters, append 8.3 + LFN root entries, update both FATs and FSInfo) | set first byte of the directory entry to `0xE5`; optionally zero the FAT chain (what a real delete does on FAT32: chain cleared, first cluster high word kept in entry on some drivers) | `fatput.py --fragment` allocates every other cluster run to a dummy file first, then the test file, so the chain alternates |
| exFAT | `mkfs.exfat -c 32K img/exfat.img` (file size preset via `truncate -s 64M`); populate with `tests/exfatput.py` (writes File + Stream Extension + File Name entry set with correct `SetChecksum`, sets bitmap bits, `NoFatChain` flag for contiguous files) | clear bit 7 (InUse) of the entry type bytes (0x85 -> 0x05, 0xC0 -> 0x40, 0xC1 -> 0x41); clear bitmap bits | write with `NoFatChain=0` and a real FAT chain alternating clusters |
| NTFS | `mkntfs -F -Q -q -s 512 -c 4096 img/ntfs.img` (`-F` because the target is not a block device; the size comes from the file (`truncate -s 64M` first) or from the optional `number-of-sectors` last argument; `-Q` quick format; do not pass `-C`, which is `--enable-compression` and would make `ntfscp` write compressed `$DATA` runs), then `ntfscp img/ntfs.img fx/camA_00.jpg /camA_00.jpg` per file (`ntfscp(8)`: destination is a file name or an existing directory; ntfsprogs has no mkdir for an unmounted image, so without a FUSE mount (`ntfs-3g img/ntfs.img mnt` works unprivileged where `fusermount3` is installed) fixtures are root-directory only) | clear bit 0 of the flags word at offset 0x16 of the file's `FILE` record (found by scanning `$MFT` for the `FILE` signature and the `$FILE_NAME` attribute), and clear the bit in `$Bitmap`; the test also writes a second file to reuse clusters | fill the volume with alternating small files via `ntfscp`, then copy a large file: NTFS allocates the gaps, giving many data runs (verify by parsing the `$DATA` run list) |
| Partition table | Python writes MBR (type 0x0C/0x07/0x83 entries) and GPT (header CRC32, entry array CRC32, backup header at last LBA) around the FS images; variants: GPT primary header zeroed (backup must be used), MBR with 0x55AA missing, protective MBR only | | |

Where the dev box lacks a writer (FAT, exFAT), the test-only Python writers are
acceptable because the parsers under test are independently checked against images made
by `mkfs.*` (empty images must parse to "no files", and boot-sector fields must match
what `mkfs` wrote).

### 5.4 Camera-crash fixture (0-byte file with data on disk)

On the FAT32 image: write `MOVI0001.MP4` via `fatput.py` with `--size 0 --first-cluster N`
while the clusters N.. are filled with the bytes of `fx/nomoov.mp4` and the FAT chain is
either absent (0x00000000, the usual case) or partially linked. Expected: FS parser lists
the entry with size 0; the "orphan data after entry" heuristic (fatcat `-o` style) or the
carver finds the data; moov rebuild yields a playable file whose duration equals
`ffprobe` on the reference minus the dropped tail.

### 5.5 Failing-device simulator

`FaultyImage` wraps an image file plus a ddrescue mapfile that describes which ranges
return `EIO`, which are "slow" (sleep N ms), which stall (block until a cancel flag) and
which return garbage (for the `-J` consistency logic). Fixture maps are generated for:
HDD-like (a few large bad areas with ragged edges), SSD-like (4 KiB granular scattered
errors), USB-like (random 1 s stalls then a device-gone error once per 10 MiB), dead
controller (size 8 MiB, every read zeros). The imaging engine's output mapfile is diffed
against the expected map: every readable sector must be `+`, every injected bad sector
`-`, and the order of reads (logged) must show skipping and reverse passes.

### 5.6 Compatibility tests for the mapfile

Parse the manual's example mapfile and a set of synthetic ones (decimal/hex/octal
mixes, comments after whitespace, `%` status, pass numbers > 1); round-trip must
preserve block order and statuses; `ddrescuelog`-style set operations (`-D`, `-x`, `-y`)
are reimplemented and tested against hand-computed results.

### 5.7 Carver tests

Concatenate fixtures with cluster-aligned padding and random data into a raw image
(no FS); include a bifragmented JPEG (gap 1, 2, 4, 8 sectors of another JPEG's middle),
an MP4 with `mdat` interrupted by 64 KiB of an AVI, a HEIC-style ISOBMFF skeleton
(`ftyp heic` + `meta` + `mdat`) built by Python, and PhotoRec-style expectations:
contiguous files must come out byte-identical; fragmented ones must be flagged
"damaged" in v1 and recovered in v2.

### 5.8 Repair tests

- JPEG: `nomoov`-style truncation -> repair output must decode with Pillow, height
  reported, damaged rows counted; donor header test: output pixels must equal the
  original within PSNR > 40 dB for the first 90 % rows (Pillow compare).
- MP4: rebuilt file must pass `ffprobe -v error -show_streams` and decode fully with
  `ffmpeg -v error -i out.mp4 -f null -`; frame count equals the number of H.264 access
  units found in `mdat` (ground truth from `ffprobe -count_frames` on the reference).
- AVI: `idx1` rebuilt; ffmpeg decodes all frames.
- PNG: IEND added; CRC recomputed; Pillow loads.

---

## 6 Recommended Rust crates (versions queried from crates.io on 2026-10-12)

| Need | Crate | Version | Notes |
|---|---|---|---|
| CLI | `clap` | 4.6.7 | derive API |
| Errors | `thiserror` / `anyhow` | 2.0.21 / 1.0.104 | library vs binary |
| Logging | `tracing` | 0.1.44 | with `tracing-subscriber` |
| Progress UI | `indicatif` | 0.18.6 | multi-bar for phases |
| Parallelism | `rayon` | 1.12.0 | carving/validation on the image |
| Raw syscalls (`O_DIRECT`, `pread`, `ioctl BLKGETSIZE64/BLKSSZGET`) | `nix` | 0.31.3 | plus `libc` 0.2.190 for missing ioctls |
| Memory-mapped image reads | `memmap2` | 0.9.11 | for FS parsing on the image, not for the device |
| SCSI generic passthrough (later) | `sg` | 0.3.0 | last release 2018: likely write own `SG_IO` wrapper over `nix::ioctl` instead |
| Zero-copy struct views | `zerocopy` | 0.8.62 | `FromBytes` for on-disk structs |
| Endian helpers | `byteorder` | 1.5.0 | or `zerocopy` byteorder types |
| GPT | `gpt` | 4.1.0 | pure Rust, R/W; DiskRescue needs read + CRC check + backup header: consider own parser (small) |
| GPT alt | `gptman` | 3.1.1 | active (2026-04) |
| MBR | `mbrman` | 0.6.1 | |
| FAT | `fatfs` | 0.3.6 | R/W, not designed for damaged volumes: own parser recommended, use `fatfs` only in tests |
| exFAT | `exfat` | 0.1.0 | read-only, 2023, minimal: own parser recommended |
| NTFS | `ntfs` | 0.4.0 | read-only, no_std, by Colin Finck; good base; need own MFT scan for deleted records |
| ext4 | `ext4-view` | 1.0.0 | read-only, no_std, active (2026-09); need own journal/inode-table scan for deleted files |
| ISOBMFF boxes | `mp4-atom` | 0.16.2 | active (2026-10), encoder+decoder: useful for writing the rebuilt `moov` |
| MP4 alt | `mp4` | 0.14.0 | 2023, reader/writer |
| MP4 parse (Mozilla) | `mp4parse` | 0.17.0 | strict, rejects broken files: good as a validator |
| JPEG decode (validation) | `zune-jpeg` | 0.5.15 | fast, safe; in default (non-strict) mode 0.5.15 decodes a truncated baseline scan without error and fills the missing rows with grey (128); it exposes no decoded-row count (docs.rs 0.5.15 has no such method), so count rows by scanning the output for the first all-128 row, or use `DecoderOptions::set_strict_mode(true)` to get an error instead. Git main (0.5.16-rc2, 2026-10-11) adds `set_incremental_mode`, `decoded_scanlines()`, `decode_next_imcu_row()` and `DecodeErrors::ExhaustedData` (`is_recoverable_eof()`) for scan EOF: switch when released |
| JPEG decode alt | `jpeg-decoder` | 0.3.2 | pure Rust, used by `image` |
| Image umbrella | `image` | 0.25.10 | PNG/TIFF/BMP decoders for validation |
| PNG | `png` | 0.18.1 | chunk-level API for repair |
| EXIF | `kamadak-exif` | 0.6.1 | TIFF/JPEG/HEIF/PNG/WebP; camera model, DateTimeOriginal for naming |
| CRC32 | `crc32fast` | 1.5.2 | PNG chunks, GPT |
| RIFF/AVI | `riff` | 2.0.0 | 2023; AVI index rebuild likely hand-written |
| Signatures | `infer` | 0.22.0 | magic DB; DiskRescue needs its own cluster-aligned scanner anyway |
| Serialisation (report JSON) | `serde` / `serde_json` | 1.0.229 / 1.0.154 | |
| Windows (later) | `windows-sys` | 0.61.2 | `CreateFileW` on `\\.\PhysicalDriveN`, `DeviceIoControl` |

Not pure Rust and therefore avoided in v1: `ffmpeg-next` (libav bindings) and any
libjpeg binding. H.264 SPS/PPS parsing for moov rebuild is small enough to hand-write
(Exp-Golomb reader); `h264-reader` 0.9.0 (released 2026-09-14, MIT/Apache-2.0, dholroyd,
>2.2 M downloads) is actively maintained and parses NAL units, SPS and PPS: reuse it
rather than hand-writing.

---

## 7 Sources

GNU ddrescue manual (local copy fetched 2026-10-12): https://www.gnu.org/software/ddrescue/manual/ddrescue_manual.html
ddrescue man page (Debian): https://manpages.debian.org/testing/gddrescue/ddrescue.1.en.html
ddrescue history/licence: https://en.wikipedia.org/wiki/Ddrescue
ddrescueview: https://sourceforge.net/projects/ddrescueview/
OpenSuperClone README: https://github.com/ISpillMyDrink/OpenSuperClone
OpenSuperClone wiki (Recovery Phases, Direct Modes, Virtual Disk Mode): https://github.com/ISpillMyDrink/OpenSuperClone/wiki
HDDSuperClone open-sourcing and skip-threshold notes: https://www.linuxadictos.com/en/hddsuperclone-becomes-open-source-and-its-code-is-already-available.html ; https://forum.hddguru.com/viewtopic.php?p=247527
dd_rescue README: https://slackware.uk/slackwarearm/armedslack-11.0/armedslack-extra/source/ddrescue/dd_rescue/README.dd_rescue ; https://github.com/vaab/dd_rescue
safecopy: https://safecopy.sourceforge.net/
PhotoRec: https://www.cgsecurity.org/wiki/PhotoRec ; https://www.cgsecurity.org/wiki/PhotoRec_Step_By_Step ; https://cgsecurity.org/wiki/Digital_Photos_Recovery_Using_PhotoRec
PhotoRec brute force / fragmentation (forum): https://forum.cgsecurity.org/phpBB3/viewtopic.php?p=5242 ; https://forum.cgsecurity.org/phpBB3/viewtopic.php?p=27916
PhotoRec HEIC thread: https://forum.cgsecurity.org/phpBB3/viewtopic.php?p=35613
PhotoRec CR2: https://forum.cgsecurity.org/phpBB3/viewtopic.php?p=4271
Foremost licence: https://packages.altlinux.org/en/sisyphus/srpms/foremost/
Scalpel 1.60 / 2.02: https://github.com/nolaforensix/scalpel-1.60 ; https://github.com/nolaforensix/scalpel-2.02
Scalpel3 paper: https://arxiv.org/abs/2608.20363
bulk_extractor: https://forensics.wiki/bulk_extractor/ ; licence: https://github.com/simsong/bulk_extractor/blob/main/LICENSE.md
untrunc: https://vcg.isti.cnr.it/~ponchio/untrunc.php ; Gentoo ebuild (GPL-2): https://ftp.fau.de/gentoo-portage/media-video/untrunc/untrunc-0_pre20260820.ebuild
recover_mp4: https://slydiman.me/eng/mmedia/recover_mp4.htm ; https://www.videohelp.com/software/recover-mp4-to-h264
JPEG-Repair Toolkit: https://www.disktuna.com/jpeg-repair-toolkit/ ; RAW partition article: https://www.disktuna.com/raw-partition-ntfs-fat/
JPEGsnoop: https://www.sourceforge.net/projects/jpegsnoop
R-Studio: https://www.r-studio.com/
DMDE: https://www.dmde.com ; https://dmde.com/manual/editions.html
UFS Explorer Video Recovery: https://www.ufsexplorer.com/ufs-explorer-video-recovery/ ; Standard Recovery: https://www.ufsexplorer.com/ufs-explorer-standard-recovery/
Recuva review: https://www.digitalcameraworld.com/reviews/piriform-recuva-review ; https://www.handyrecovery.com/recuva-alternatives/
Disk Drill byte-to-byte backup: https://www.cleverfiles.com/help/dmg-iso-backup.html
EaseUS video repair: https://www.easeus.com/shopping/data-recovery-pro.html
Stellar Repair for Video sample file: https://www.stellarinfo.com/support/kb/index.php/article/corrupted-video-repair-using-sample-file ; https://stellarinfo.com/support/kb/index.php/article/sample-file-size-to-repair-damaged-video-file
PC-3000 pricing discussion: https://forum.hddguru.com/viewtopic.php?p=318136
extundelete: https://extundelete.sourceforge.net/
ext4magic man page: https://manpages.org/ext4magic/8 ; Debian description: https://mynixos.com/nixpkgs/package/ext4magic
ntfsundelete man page: https://man.archlinux.org/man/extra/ntfsprogs/ntfsundelete.8.en ; name-recovery patch: https://ognproject.evlt.uma.es/gitea/opengnsys/ntfs-3g/commit/6e52a39608857d9bb3c7402b30c21a1ba948ee71
fatcat: https://github.com/Gregwar/fatcat
Garfinkel 2007 "Carving contiguous and fragmented files with fast object validation", Digital Investigation 4S (2007) S2-S12, primary PDF: https://simson.net/clips/academic/2007.DFRWS.pdf (Tables 2-8); summarised in Pal & Memon slides: https://engineering.purdue.edu/dcsl/reading/2010/jevin-carving.pdf ; https://en.wikipedia.org/wiki/File_carving (its AVI figure, 22 %, disagrees with the paper's Table 3, 20 %)
Sequential hypothesis testing carving (Pal, Sencar, Memon 2008): https://dfrws.org/sites/default/files/session-files/2008_USA_paper-detecting_file_fragmentation_point_using_sequential_hypothesis_testing.pdf
TRIM/DRAT/DZAT: https://blog.elcomsoft.com/2025/06/what-trim-drat-and-dzat-really-mean-for-ssd-forensics/ ; https://rossmanngroup.com/services/ssd-data-recovery/trim-dzat-physics
Flash controller failure and chip-off: https://www.gillware.com/flash-drive-data-recovery/chip-off-recovery/ ; https://recoveryforce.com/articles/lexar-camera-card-and-thumb-drive-recovery ; https://www.rossmanngroup.com/services/flash-data-recovery/monolithic-microsd-extraction
SD card 30 MB controller fault (HDDGURU): https://forum.hddguru.com/viewtopic.php?p=201203
RAW filesystem causes and "do not format": https://www.gillware.com/hard-drive-data-recovery/raw-drive-data-recovery/ ; https://www.reclaime.com/raw-filesystem-faq/how-to-fix-raw-filesystem-issue-in-place.aspx
Linux SCSI timeouts: kernel source https://raw.githubusercontent.com/torvalds/linux/master/drivers/scsi/sd.h ; `max_retries` sysfs attribute and class: https://raw.githubusercontent.com/torvalds/linux/master/drivers/scsi/sd.c ; `SCSI_DEFAULT_EH_TIMEOUT`: https://raw.githubusercontent.com/torvalds/linux/master/include/scsi/scsi.h ; sense-key retry policy: https://raw.githubusercontent.com/torvalds/linux/master/drivers/scsi/scsi_lib.c ; libata `ata_eh_worth_retry`: https://raw.githubusercontent.com/torvalds/linux/master/drivers/ata/libata-eh.c ; storvsc timeout discussion https://lkml.iu.edu/hypermail/linux/kernel/1306.0/01118.html
ATA IDENTIFY TRIM/DRAT/RZAT bits: https://raw.githubusercontent.com/torvalds/linux/master/include/linux/ata.h (`ata_id_has_trim`, `ata_id_has_zero_after_trim`) ; hdparm identify.c (word 69 bit 14 / bit 5 labels): https://sourceforge.net/projects/hdparm/
NVMe Identify Namespace layout (DLFEAT byte 33): https://raw.githubusercontent.com/torvalds/linux/master/include/linux/nvme.h ; libnvme `enum nvme_id_ns_dlfeat`: https://raw.githubusercontent.com/linux-nvme/libnvme/master/src/nvme/types.h ; NVMe `metadata_bytes` sysfs attribute: https://raw.githubusercontent.com/torvalds/linux/master/drivers/nvme/host/sysfs.c
OpenSuperClone source (defaults, log writer, ddrescue export): https://github.com/ISpillMyDrink/OpenSuperClone/blob/main/src/opensuperclone/opensuperclone.h ; https://github.com/ISpillMyDrink/OpenSuperClone/blob/main/src/opensuperclone/common.h ; https://github.com/ISpillMyDrink/OpenSuperClone/blob/main/src/opensuperclone/opensuperclone.c ; wiki Basics of Operation (ddrescue import/export menu): https://github.com/ISpillMyDrink/OpenSuperClone/wiki/Basics-of-Operation
dd_rescue option table: https://raw.githubusercontent.com/vaab/dd_rescue/master/dd_rescue.c (`getopt` string and `printhelp`)
safecopy man page (stage presets, -M, -I/-X, -R default): https://manpages.debian.org/testing/safecopy/safecopy.1.en.html
PhotoRec source (naming, 500 files/dir, HEIC brand, DFXML): https://raw.githubusercontent.com/cgsecurity/testdisk/master/src/photorec.h ; https://raw.githubusercontent.com/cgsecurity/testdisk/master/src/photorec.c ; https://raw.githubusercontent.com/cgsecurity/testdisk/master/src/file_mov.c ; formats list: https://www.cgsecurity.org/wiki/File_Formats_Recovered_By_PhotoRec
Scalpel 2.02 README (GPL, GPU removed): https://github.com/nolaforensix/scalpel-2.02
extundelete licence: https://sources.debian.org/src/extundelete/0.2.4-3/debian/copyright/ ; ext4magic licence: https://metadata.ftp-master.debian.org/changelogs/main/e/ext4magic/unstable_copyright
ntfscp / mkntfs man pages (ntfs-3g, local): `man ntfscp`, `man mkntfs`; mkfs.fat `-C`/BLOCK-COUNT and mke2fs `-d`: local man pages
zune-jpeg 0.5.15 API: https://docs.rs/zune-jpeg/0.5.15/zune_jpeg/struct.JpegDecoder.html ; git main: https://github.com/etemesi254/zune-image (crates/zune-jpeg/src/mcu.rs, decoder.rs)
h264-reader: https://crates.io/api/v1/crates/h264-reader
R-Studio prices: https://www.techradar.com/reviews/r-studio ; https://www.capterra.com/p/33908/R-Studio/pricing/
Recuva tiering (official site blocks fetches): https://www.techjockey.com/detail/recuva ; https://softwarehow.com/?p=30895
TLER / 7 s: https://lkml.indiana.edu/0411.0/1802.html ; https://lore.kernel.org/all/4BA93C3F.2070208@gmail.com/T/
Crate versions: crates.io API (`https://crates.io/api/v1/crates/<name>`), queried 2026-10-12.
