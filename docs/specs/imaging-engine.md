# DiskRescue Imaging Engine Specification

Status: draft for implementation. Facts tagged `UNVERIFIED` could not be confirmed
against a primary source during research and must be checked before relying on them.
Everything else was read from the sources listed in section 7 (ddrescue 1.28 source
and manual, Linux kernel source/docs, local kernel headers, smartmontools source,
libnvme headers, OpenSuperClone manual, udev rules, man-pages 6.17).

---

## 1. Overview and scope

### 1.1 Purpose

The imaging engine copies a failing block device (HDD, SATA/NVMe SSD, USB flash,
SD/MMC card) or a partial image into a destination image file, as completely as
possible, with as little additional damage to the source as possible, and in a
way that can be interrupted and resumed indefinitely. All later DiskRescue stages
(partition/filesystem parsing, carving, repair) operate on the **image + map**,
never on the source device.

### 1.2 Hard requirements

| # | Requirement |
|---|-------------|
| R1 | Never write to the source device. Open source `O_RDONLY` (Linux) / `GENERIC_READ` (Windows); never issue `BLKRRPART`, `BLKFLSBUF`, TRIM, SMART self-tests or any write-class passthrough command to the source. |
| R2 | Map file is byte-for-byte compatible with GNU ddrescue 1.28 (`ddrescue`/`ddrescuelog`/`ddrescueview` must read it; ddrescue must be able to continue a DiskRescue rescue and vice versa). DiskRescue-only state goes in a sidecar file. |
| R3 | Resumable after crash, power loss, device disappearance, or SIGKILL, losing at most `save_interval` seconds of progress. |
| R4 | Per-read timeout that cannot be defeated by a `read()` that blocks inside the kernel. |
| R5 | Device-type-aware strategy (HDD / SSD / USB-flash / SD) selected automatically from sysfs with manual override. |
| R6 | Survives the device dropping off the bus and reappearing under a different `/dev` name; re-identifies the device by serial/model/size before continuing. |
| R7 | Rate statistics, phase progress and ETA exposed through a `Progress` struct for CLI/TUI. |
| R8 | Works unprivileged on regular files (for tests and for re-imaging partial images); privileged operations (sysfs tuning, SG_IO, NVMe admin) are optional and degrade gracefully. |

### 1.3 Terminology (ddrescue vocabulary, kept verbatim so the map file is interoperable)

| Term | Meaning |
|------|---------|
| sector / `hardbs` | Hardware block; the smallest unit the device can read. Default 512; read from `BLKSSZGET`. |
| cluster / `softbs` | `cluster_size_sectors * hardbs`; the size of one read in the copying phase. ddrescue default: 64 KiB / hardbs sectors. |
| block | `(pos, size)` byte range in the input. |
| rescue domain | The set of blocks the engine is allowed to touch (whole input by default; restricted by `--input-position/--size/--domain-mapfile`). |
| status of a block | one of `non_tried ?`, `non_trimmed *`, `non_scraped /`, `bad_sector -`, `finished +`. |
| phases | copying (up to 5 passes) → trimming (1 pass) → scraping (1 pass) → retrying (N passes). |

### 1.4 Out of scope for this document

Partition/filesystem parsing, carving, file repair, GUI. Hardware power-relay
control is described only as an interface hook.

---

## 2. Exact on-disk / wire details

### 2.1 The ddrescue mapfile (logfile) format

Source: ddrescue 1.28 `doc/ddrescue.texi` "Mapfile structure", `mapfile.cc`
(`read_mapfile`, `write_mapfile`, `my_fgets`), `main_common.cc` (`write_file_header`).

#### 2.1.1 Grammar

```
mapfile      := { blank_or_comment } status_line { blank_or_comment | block_line }
comment      := "#" { any char except LF } LF
blank        := { whitespace } LF
status_line  := ws* integer ws+ status_char [ ws+ pass ] [ ws* "#" ... ] LF
block_line   := ws* integer ws+ integer ws+ block_status [ ws* "#" ... ] LF
integer      := C++ integer literal: decimal | "0x"/"0X" hex | leading "0" octal
pass         := decimal integer >= 1              (sscanf "%d")
status_char  := "?" | "*" | "/" | "-" | "F" | "G" | "+"
block_status := "?" | "*" | "/" | "-" | "+"
```

Exact parser behaviour (`mapfile.cc`):

- Lines are read by `my_fgets`: leading whitespace is skipped; a `#` at the start of a
  line **or after whitespace** starts a comment to end of line; blank lines are skipped.
  Lines longer than 127 bytes are truncated to 127 (parse what fits).
- The **first non-comment line** is the status line, parsed with
  `sscanf(line, "%lli %c %d")`. Accepted if 2 or 3 fields matched, `current_pos >= 0`,
  status char valid, and `current_pass >= 1`. If the pass is absent it defaults to 1.
  Otherwise: "Bad or missing status line", exit status 2.
- Every following non-comment line is a block line parsed with
  `sscanf(line, "%lli %lli %c")`. Accepted if 3 fields, `pos >= 0`, status valid, and
  `size > 0` (or `size == 0 && pos == 0`, the empty-mapfile case).
- **Contiguity rule**: each block must start exactly at `prev.end()` (`pos == prev.pos + prev.size`).
  Otherwise "Blocks are not contiguous", exit 2 — unless loose mode (`-L`) is on, in which
  case blocks may be unordered and may overlap *only if all overlapping blocks have the
  same status*; gaps are filled with the default status (non-tried for domain/test mapfiles).
- Positions and sizes are **bytes**, not sectors.
- `%lli` means a value `> 2^63-1` is rejected; a block size is clamped so that
  `pos + size <= LLONG_MAX` (`Block::fix_size`).

#### 2.1.2 Status characters

Status line (`current_status`):

| Char | Meaning |
|------|---------|
| `?` | copying non-tried blocks |
| `*` | trimming non-trimmed blocks |
| `/` | scraping non-scraped blocks |
| `-` | retrying bad sectors |
| `F` | filling the blocks specified (fill mode) |
| `G` | generating approximate mapfile (generate mode) |
| `+` | finished |

Block lines:

| Char | Name | `processed_state` rank | Meaning |
|------|------|-----------------------|---------|
| `?` | non_tried | 0 | never read |
| `*` | non_trimmed | 1 | a large (cluster) read failed; edges not yet probed sector-wise |
| `/` | non_scraped | 2 | edges trimmed; interior not yet read sector-wise |
| `-` | bad_sector | 3 | a single-sector read failed |
| `+` | finished | 4 | successfully read |

`current_pos` semantics: "the position being tried in the input file (the beginning of
the block being tried in a forward pass or the end of the block in a backward pass)".
`current_pass`: pass number inside the current phase; lets copying resume at pass N
instead of pass 1, and lets retrying resume in the same direction (odd = forward,
even = backward when bidirectional).

#### 2.1.3 Exact writer output (what DiskRescue must emit)

```
# Mapfile. Created by GNU ddrescue version 1.28            <- "# %s. Created by %s version %s"
# Command line: ddrescue -d -c18 /dev/fd0 fdimage mapfile
# Start time:   2015-07-21 09:37:44                        <- "%Y-%m-%d %H:%M:%S" local time
# Current time: 2015-07-21 09:38:19                        <- written on every save
# Copying non-tried blocks... Pass 1 (forwards)            <- current_msg (screen status), optional
# current_pos  current_status  current_pass
0x00120000     ?               1
#      pos        size  status
0x00000000  0x00117000  +
0x00117000  0x00000200  -
0x00117200  0x00001000  /
0x00118200  0x00007E00  *
0x00120000  0x00048000  ?
```

Format strings (verbatim from `mapfile.cc`):

```
"# current_pos  current_status  current_pass\n"
"0x%08llX     %c               %d\n"
"#      pos        size  status\n"
"0x%08llX  0x%08llX  %c\n"        (one per block)
```

`%08llX` = uppercase hex, zero-padded to at least 8 digits, wider when needed.
DiskRescue writes `# Mapfile. Created by DiskRescue version X.Y.Z` (the program name
is informational; ddrescue does not parse it). All other lines identical.

A block of unknown size (input size unknown) is stored as `size = LLONG_MAX - pos`
and written as such; readers must accept `pos + size == 0x7FFFFFFFFFFFFFFF`.

#### 2.1.4 Invariants, ordering and merge rules

1. Blocks are sorted by `pos`, contiguous, non-overlapping, and together cover
   `[0, input_size)` (or `[0, LLONG_MAX)` when the size is unknown).
2. Adjacent blocks of equal status **may** exist in a file produced mid-run (ddrescue
   only compacts at exit and at `f` in command mode: `compact_sblock_vector`), so a
   reader must merge them on load and a writer should merge before the final save.
3. **Extend to input size** on load (`extend_sblock_vector(insize)`):
   - empty list → one `non_tried` block `[0, insize)`;
   - first block `pos > 0` → prepend `non_tried [0, pos)`;
   - last block ends `< insize` → append `non_tried [end, insize)`;
   - last block starts `>= insize`: if it starts exactly at `insize` and is not
     finished, drop it; else fatal "Last block in mapfile begins past end of input
     file" (hint `-C`);
   - last block ends `> insize`: if not finished, shrink it; else fatal "Rescued data
     in mapfile goes past end of input file".
   With `-C/--complete-only` the domain is instead truncated to the mapfile extent.
4. **Sub-sector joining** (`join_subsectors(hardbs)`): for every boundary not on a
   `hardbs` multiple, if either neighbour is `finished` leave it; otherwise move the
   partial sector into the neighbour with the **less processed** state (lower rank in
   the table above), or join both blocks keeping the less-processed status. Done over
   the whole mapfile, not only the domain. Reason: a successful sector read must never
   be lost because the mapfile was written by a tool with a different sector size.
5. **Status change** (`change_chunk_status(b, st, domain)`) is the only mutation:
   split the block(s) overlapping `b`, set the status of exactly `[b.pos, b.end)`,
   then merge with equal-status neighbours. It returns the delta of `bad_areas`
   (a bad area = maximal run of `bad_sector` blocks; the counter is maintained
   incrementally by looking at the statuses of the two neighbours before and after).
6. `--domain-mapfile FILE`: the rescue domain = union of the blocks marked `+`
   (finished) in FILE (loose parsing allowed). Several `-m` options intersect.
   Blocks of the working mapfile are split at domain borders on load.
7. Numbers may be decimal/hex/octal when hand-edited; `current_pass` must be decimal.

#### 2.1.5 Save policy (what ddrescue does; DiskRescue default identical)

Source: `mapbook.cc::update_mapfile`.

```
interval  = user_save_interval if set, else 30 + min(270, nblocks / 38)   // 30 s .. 5 min
sync_int  = user_sync_interval if set, else 300 s  (min 5 s, must be >= interval)
save when (now - last_save >= interval) or forced (phase end, exit, signal)
mf_sync   = forced or (now - last_sync >= sync_int)
if out_fd >= 0: fsync(out_fd)                       // data before map, always
if previous save was fsynced: remove(map.bak); rename(map, map.bak)
write map to `map` ("w" = truncate + rewrite)        // NOT atomic in ddrescue
if mf_sync: fsync(map)
timestamps updated AFTER the write so writing time is excluded from intervals
on write error: prompt user (retry / E = emergency save to ./ddrescue.map or $HOME/ddrescue.map / Q)
```

DiskRescue tightens this (section 3.8): write to `map.tmp`, `fsync`, `rename` over
`map`, `fsync(dir)`; keep `map.bak` as ddrescue does so ddrescueview users see it.
`--mapfile-interval=0` means "save after every read" (allowed, slow).

#### 2.1.6 DiskRescue sidecar `map.dr.json` (not read by ddrescue)

Stored next to the mapfile; absence is fine (pure ddrescue resume).

| Field | Type | Purpose |
|-------|------|---------|
| `format` | int (1) | sidecar version |
| `device.fingerprint` | object | `{model, serial, wwn, size_bytes, logical_bs, physical_bs, bus, first_sector_sha256}` for re-identification (3.6) |
| `device.kind` | enum | `hdd`, `ssd_sata`, `ssd_nvme`, `usb_flash`, `sd_mmc`, `optical`, `file`, `unknown` |
| `strategy` | object | the resolved parameter set (3.9) actually used |
| `phase_log` | array | `{t, phase, pass, direction, finished, bad_areas}` per pass boundary |
| `errors` | array (capped) | last 10 000 `{pos, size, errno, sense:[key,asc,ascq] or null, latency_ms}` |
| `slow_map` | array | blocks skipped for slowness in passes 1–2 with the measured rate |
| `stats` | object | totals (read errors, slow reads, re-attach count, time per phase) |

### 2.2 Linux kernel interfaces used by the engine

#### 2.2.1 Block-device ioctls (`/usr/include/linux/fs.h`)

| ioctl | Number | Arg | Use |
|-------|--------|-----|-----|
| `BLKSSZGET` | `_IO(0x12,104)` = `0x1268` | `int*` | logical sector size → `hardbs`; required for O_DIRECT alignment |
| `BLKPBSZGET` | `_IO(0x12,123)` = `0x127B` | `uint*` | physical sector size (4Kn/512e detection; cluster alignment) |
| `BLKGETSIZE64` | `_IOR(0x12,114,size_t)` = `0x80081272` (64-bit) | `u64*` | input size in bytes |
| `BLKIOMIN` | `_IO(0x12,120)` = `0x1278` | `uint*` | minimum I/O size |
| `BLKIOOPT` | `_IO(0x12,121)` = `0x1279` | `uint*` | optimal I/O size |
| `BLKALIGNOFF` | `_IO(0x12,122)` = `0x127A` | `int*` | alignment offset |
| `BLKRRPART` | `_IO(0x12,95)` = `0x125F` | — | **never on source** (R1) |
| `BLKFLSBUF` | `_IO(0x12,97)` = `0x1261` | — | **never on source** |

Fallback when ioctls fail (regular file): `fstat().st_size`, `hardbs` = 512 unless
overridden by `--sector-size`.

Sysfs equivalents (readable without root): `/sys/block/<dev>/queue/logical_block_size`,
`physical_block_size`, `max_sectors_kb` (largest single request the kernel will issue;
observed 128 on NVMe here, OpenSuperClone notes 512 for SATA HDD and smaller for USB),
`max_hw_sectors_kb`, `/sys/block/<dev>/size` (in 512-byte units), `removable`,
`device/model`, `device/vendor`, `device/serial` (NVMe), `device/state`.

#### 2.2.2 `O_DIRECT` alignment (open(2), man-pages 6.17)

- Alignment restrictions vary by filesystem/kernel; misaligned I/O "can either fail with
  EINVAL or fall back to buffered I/O".
- Block devices since Linux 2.6.0: **file offset, length and user buffer address must
  all be multiples of the logical block size** (`BLKSSZGET`).
- Since Linux 6.1: `statx(2)` with `STATX_DIOALIGN` (`0x00002000`) returns
  `stx_dio_mem_align` and `stx_dio_offset_align` (plus `stx_dio_read_offset_align` in
  newer headers). Use it when available; fall back to `BLKSSZGET`.
- Buffers: allocate with `posix_memalign`/`std::alloc::alloc` with
  `align = max(page_size, hardbs)` (ddrescue aligns to page size when it is a multiple
  of the sector size).
- Reading a block that is not sector aligned under O_DIRECT (`copy_block` in ddrescue):
  read `[pos - pre, end + post)` where `pre = pos % hardbs`, `post = (hardbs - end % hardbs) % hardbs`,
  then `memmove` the wanted part. This is "the only case where ddrescue reads outside
  the rescue domain".
- Never mix `O_DIRECT` and buffered reads of overlapping ranges on the same file.
- `O_DIRECT` I/O must not be in flight across `fork(2)` on `MAP_PRIVATE` memory;
  DiskRescue never forks after opening the source.
- Why O_DIRECT on the source: the page cache would (a) serve stale data after a
  partially failed read, (b) merge/split requests so the error granularity is lost
  ("positions and sizes in mapfile always multiples of the sector size" symptom),
  (c) read-ahead into bad areas the engine is trying to avoid. ddrescue `-d`.
- Why O_DIRECT on the destination is optional (`-D`): some OSes do not report write
  errors on cached writes; with a regular image file DiskRescue uses buffered writes +
  periodic `fsync` (or `--synchronous` = fsync after every write).

#### 2.2.3 errno values from `read(2)`/`pread(2)` on block devices

Values from `asm-generic/errno-base.h` / `errno.h`; meaning from read(2) and observed
driver behaviour.

| errno | # | Meaning for the engine | Action |
|-------|---|------------------------|--------|
| `EINTR` | 4 | interrupted by signal before any data | retry the same read immediately (ddrescue `readblock` loops on EINTR) |
| `EIO` | 5 | "low-level I/O error while reading from a disk": medium error, aborted command, timeout after kernel EH gave up | mark block per phase rules; count as read error |
| `ENXIO` | 6 | "No such device or address": device node exists but no driver/device behind it (typical right after a USB device vanished, when reopening by stale path) | treat as *device gone*; enter re-attach wait |
| `ENODEV` | 19 | "No such device": gendisk marked dead/driver unbound (`sd` removed) | *device gone*; re-attach wait |
| `EAGAIN` | 11 | only with `O_NONBLOCK` (not used) | treat as transient; retry once |
| `EINVAL` | 22 | O_DIRECT misalignment, or object unsuitable | fatal: "Unaligned read error. Is sector size correct?" (ddrescue message) |
| `ENOMEDIUM` | 123 | removable reader with no card | re-attach wait (card reader stays, medium disappears) |
| `ETIMEDOUT` | 110 | rare; some transports | treat like EIO *and* raise the slow/hang counter |
| `EREMOTEIO` | 121 | USB/transport error | treat like EIO; after N in a row suspect bridge, trigger reopen |
| `EBADMSG` | 74 / `EILSEQ` 84 | integrity (DIF/PI) failure | treat like EIO, log separately |
| `ENOENT` | 2 (on reopen) | device node removed (udev) | *device gone*; re-attach wait |
| `EACCES`/`EPERM`/`EBADF`/`EBUSY`/`EISDIR`/`ENOMEM`/`ENOSYS`/`ESPIPE` | — | ddrescue treats these as fatal "Fatal error reading the input file" | fatal unless caused by reopen race |
| short read with `errno == 0` | — | EOF (device shrank or image is partial) | truncate the map (`truncate_vector`) if no finished data lies beyond; else fatal "EOF found below the size calculated from mapfile" |

ddrescue's exact fatal list in `copy_block` (`copied_size <= 0`):
`EACCES EBADF EBUSY EISDIR ENOBUFS ENODEV ENOENT ENOMEM ENOSYS ENXIO EPERM ESPIPE`
plus `EINVAL` (unaligned). DiskRescue intercepts `ENODEV/ENXIO/ENOENT/ENOMEDIUM`
before that list to implement re-attach (R6).

After every read error ddrescue also `stat()`s the input path and aborts with
"Input file disappeared" if it fails; DiskRescue does the same check but routes the
failure into the re-attach state instead of exiting.

Important kernel behaviour for error granularity: with buffered I/O the kernel may
report `EIO` for the whole request; with `O_DIRECT` a partially successful request
returns the number of bytes read before the first failing sector **only if** the
driver splits the request; on most SATA/USB paths the whole request fails. That is
why ddrescue never trusts partial reads to locate the bad sector and instead
*trims* and *scrapes* sector by sector.

#### 2.2.4 SCSI/ATA per-device sysfs knobs (`drivers/scsi/scsi_sysfs.c`, `sd.c`)

All under `/sys/block/sdX/device/` (symlink to `/sys/bus/scsi/devices/H:C:T:L/`).

| Attribute | Mode | Semantics (from kernel source) | Engine use |
|-----------|------|--------------------------------|-----------|
| `timeout` | `0644` | seconds; `rq_timeout / HZ`; store rejects `<= 0`. Default `SD_TIMEOUT = 30 s` (sd.h). Per-request timeout before `scsi_timeout()` fires. | lower to 5–10 s on failing HDD/USB so a hung read returns `EIO` sooner (3.5); requires root |
| `eh_timeout` | `0644`, store needs `CAP_SYS_ADMIN` | seconds used by the error handler for its own recovery commands (TUR/START UNIT). | lower to 2–5 s |
| `state` | rw | one of `created running cancel deleted quiesce offline transport-offline blocked`. Writing `offline` then `running` is a userspace "soft reset" of the sdev; EH itself sets `offline` when all resets fail (`scsi_eh_offline_sdevs`). | detect `offline`/`transport-offline` ⇒ device unusable; attempt `running`; else re-attach |
| `delete` | w | write `1` → `scsi_remove_device` | last resort before host rescan |
| `rescan` | w | `scsi_rescan_device` (re-reads capacity/INQUIRY) | after `running` |
| `/sys/class/scsi_host/hostN/scan` | w | write `"- - -"` to re-probe | after `delete` to get the device back |
| `/sys/class/scsi_disk/H:C:T:L/max_retries` | rw | `-1` (no limit) .. `SD_MAX_RETRIES = 5`; default 5 | set to `0` or `1` on failing media so the kernel does not retry a bad sector 5× (each retry is another head hit) |
| `queue/max_sectors_kb` | rw | cap on one request; passthrough buffer limit | choose `cluster <= max_sectors_kb` |

Kernel timeout → EH flow (Documentation/scsi/scsi_eh.rst): `scsi_timeout()` →
optional LLDD `eh_timed_out()` → `scsi_abort_command()` (async abort, may retry
`scmd->allowed + 1` times) → EH thread: TUR check → START UNIT → device reset →
bus reset → host reset → offline the sdev. On libata (SATA) the device/host reset is a
SATA link soft/hard reset (COMRESET). Each step can take tens of seconds; a single
hung read can therefore block for `timeout × (retries+1) + EH time` ≈ minutes with
defaults. This is the "bad drive can block ddrescue for a long time until the kernel
gives up" sentence from the ddrescue manual and the reason R4 exists.

#### 2.2.5 SG_IO pass-through (`/usr/include/scsi/sg.h`, `scsi/scsi.h`)

`ioctl(fd, SG_IO = 0x2285, struct sg_io_hdr*)` on `/dev/sdX` (needs `CAP_SYS_RAWIO`
or root for READ commands on a block device). Key fields:

| Field | Type | Value for a read |
|-------|------|------------------|
| `interface_id` | int | `'S'` |
| `dxfer_direction` | int | `SG_DXFER_FROM_DEV = -3` |
| `cmd_len` | u8 | 10 or 16 |
| `mx_sb_len` | u8 | 32 (sense buffer length) |
| `dxfer_len` | uint | bytes to read |
| `dxferp` | void* | aligned buffer |
| `cmdp` | u8* | CDB |
| `sbp` | u8* | sense buffer |
| `timeout` | uint | **milliseconds**; `MAX_UINT` = no timeout |
| out: `status`, `masked_status`, `host_status`, `driver_status`, `sb_len_wr`, `resid`, `info` | | `info & SG_INFO_CHECK (0x1)` ⇒ something abnormal |

CDBs (SBC; `scsi.h` defines `READ_10 = 0x28`; READ(16) = `0x88` UNVERIFIED against
local header, taken from SBC-3):

```
READ(10): [0]=0x28 [1]=0 [2..5]=LBA (BE32) [6]=0 [7..8]=transfer length in blocks (BE16) [9]=0
READ(16): [0]=0x88 [1]=0 [2..9]=LBA (BE64)        [10..13]=transfer length (BE32)   [14]=0 [15]=0
```

Sense decoding (SPC): fixed format `sbp[0] & 0x7F ∈ {0x70,0x71}` → key = `sbp[2] & 0x0F`,
ASC = `sbp[12]`, ASCQ = `sbp[13]`, and if `sbp[0] & 0x80` (VALID) the Information
field `sbp[3..6]` (BE32) holds the LBA of the first unrecovered block for a READ
(UNVERIFIED: SBC wording of the Information field for MEDIUM ERROR). Descriptor
format `{0x72,0x73}` → key = `sbp[1] & 0x0F`, ASC = `sbp[2]`, ASCQ = `sbp[3]`.

Sense keys (`scsi.h`): `NO_SENSE 0x0`, `RECOVERED_ERROR 0x1`, `NOT_READY 0x2`,
`MEDIUM_ERROR 0x3`, `HARDWARE_ERROR 0x4`, `ILLEGAL_REQUEST 0x5`, `UNIT_ATTENTION 0x6`,
`ABORTED_COMMAND 0xB`. Common ASC/ASCQ: `0x11/0x00` unrecovered read error,
`0x04/0x00..0x03` not ready/becoming ready, `0x29/0x00` power-on/reset occurred
(UNIT ATTENTION after a bridge reset), `0x3A/0x00` medium not present (UNVERIFIED:
ASC list from SPC tables, not re-read during research).

Why SG_IO matters: the **timeout is per command in milliseconds** and the result
carries the sense data, so the engine learns *which* LBA failed and *why* (medium vs
hardware vs aborted vs not-ready) instead of an opaque `EIO`. This is what
HDDSuperClone/OpenSuperClone "SCSI passthrough mode" (their default, and "the ONLY
mode that can work with a USB attached device") does. Caveats: the kernel still runs
its EH on a timed-out SG_IO command (abort → resets), and a USB bridge "may not send
the proper response when there are errors or faults" (OpenSuperClone manual), so the
sense must be treated as a hint, not ground truth.

#### 2.2.6 SMART via SAT ATA PASS-THROUGH (smartmontools `ata.h`, `scsiata.cpp`)

ATA register values: `ATA_SMART_CMD = 0xB0`, feature `ATA_SMART_READ_VALUES = 0xD0`,
`ATA_SMART_READ_THRESHOLDS = 0xD1`, `ATA_IDENTIFY_DEVICE = 0xEC`; SMART commands
require `LBA_MID = 0x4F`, `LBA_HIGH = 0xC2` as input.

ATA PASS-THROUGH(16) CDB (opcode `0x85`; (12) is `0xA1`):

```
[0]=0x85
[1]=(protocol<<1)|extend         protocol 4 = PIO Data-In (UNVERIFIED: SAT numeric value)
[2]=(ck_cond<<5)|(t_dir<<3)|(byte_block<<2)|t_length   t_dir=1 from device, byte_block=1, t_length=2 (sector count)
[3]=features(15:8) [4]=features(7:0)      -> 0x00, 0xD0
[5]=count(15:8)    [6]=count(7:0)         -> 0x00, 0x01
[7]=lba_low(15:8)  [8]=lba_low(7:0)       -> 0, 0
[9]=lba_mid(15:8)  [10]=lba_mid(7:0)      -> 0, 0x4F
[11]=lba_high(15:8)[12]=lba_high(7:0)     -> 0, 0xC2
[13]=device (0xA0 or 0x40)  [14]=command 0xB0  [15]=0
```

Returned 512-byte `ata_smart_values`: `u16 revnumber` @0, then 30 ×
`ata_smart_attribute` (12 bytes each) @2..361:

| Offset | Size | Field |
|--------|------|-------|
| 0 | 1 | `id` (0 = unused slot) |
| 1 | 2 | `flags` (LE) |
| 3 | 1 | `current` (normalized) |
| 4 | 1 | `worst` |
| 5 | 6 | `raw` (vendor-defined; usually LE48 count) |
| 11 | 1 | reserved |

Byte 511 of the page is a checksum (sum of all 512 bytes == 0 mod 256; UNVERIFIED).

Attributes the engine reads (Wikipedia S.M.A.R.T. table; semantics are conventions,
"vendors are in no way required to adhere"):

| ID | Name | Engine meaning |
|----|------|----------------|
| 5 (0x05) | Reallocated Sectors Count | raw = sectors already remapped. Rising **during** the rescue = the drive is remapping on reads (head/platter degradation); large value ⇒ expect clustered bad areas. |
| 196 (0xC4) | Reallocation Event Count | remap attempts (success + failure) |
| 197 (0xC5) | Current Pending Sector Count | "unstable sectors waiting to be remapped because of unrecoverable read errors"; the sectors the retry phase will hit. If this drops during the rescue the drive is remapping = good sign for retries. |
| 198 (0xC6) | (Offline) Uncorrectable Sector Count | total uncorrectable errors; rising ⇒ surface/mechanical defects; raw growth rate per GB read is the "is it getting worse" signal that triggers cooling pauses / strategy downgrade. |
| 199 (0xC7) | UDMA CRC Error Count | cable/bridge errors, **not** media: if this rises, reads failing with ABORTED COMMAND are a cable/USB-bridge problem, not bad sectors. |

The engine reads SMART only (a) at start, (b) every `smart_interval` (default 10 min),
(c) after each phase; never `SMART EXECUTE OFFLINE IMMEDIATE` (self-test) on a failing
drive. On a drive that is barely responding, even IDENTIFY can hang; SMART reads are
issued through the same timeout-guarded worker as data reads.

#### 2.2.7 NVMe (libnvme `types.h`, `linux/nvme_ioctl.h`, local sysfs)

- Admin passthrough: `ioctl(ctrl_or_ns_fd, NVME_IOCTL_ADMIN_CMD = _IOWR('N', 0x41, struct nvme_admin_cmd), &cmd)`;
  struct has `timeout_ms` field. Get Log Page: `opcode = 0x02`, `nsid = 0xFFFFFFFF`,
  `addr = buf`, `data_len = 512`, `cdw10 = (((512/4) - 1) << 16) | 0x02` (NUMDL, LID)
  (UNVERIFIED: cdw10 bit layout from NVMe base spec memory).
- SMART / Health log page `02h` layout (`struct nvme_smart_log`, 512 bytes):

| Offset | Size | Field |
|--------|------|-------|
| 0 | 1 | `critical_warning` |
| 1 | 2 | composite temperature (Kelvin, LE16) |
| 3 | 1 | `avail_spare` (%) |
| 4 | 1 | `spare_thresh` (%) |
| 5 | 1 | `percent_used` |
| 6 | 1 | endurance group critical warning summary |
| 7 | 25 | reserved |
| 32 | 16 | data units read (×1000 × 512 B) |
| 48 | 16 | data units written |
| 64 | 16 | host read commands |
| 80 | 16 | host write commands |
| 96 | 16 | controller busy time |
| 112 | 16 | power cycles |
| 128 | 16 | power on hours |
| 144 | 16 | unsafe shutdowns |
| 160 | 16 | **media and data integrity errors** |
| 176 | 16 | number of error information log entries |
| 192 | 4 | warning composite temperature time |
| 196 | 4 | critical composite temperature time |
| 200 | 16 | temperature sensors 1–8 (LE16 each) |

- `critical_warning` bits (enum `nvme_smart_crit`): bit 0 spare below threshold,
  bit 1 temperature, bit 2 reliability degraded (media errors), **bit 3 all media in
  read-only mode**, bit 4 volatile memory backup failed, bit 5 PMR read-only.
  Bit 3 set ⇒ the drive is already protecting itself; imaging is still possible and is
  the right move; writes to it are refused (irrelevant for us).
- Sysfs (verified on this machine): `/sys/class/nvme/nvme0/{state,model,serial,firmware_rev,reset_controller,rescan_controller,queue_count}`;
  `state` = `live` when healthy (other values: `new`, `resetting`, `connecting`,
  `deleting`, `dead`; UNVERIFIED list). Writing `1` to `reset_controller` performs a
  controller reset (root) — the NVMe equivalent of "offline/running". `/dev/nvme0` is
  the admin char device; `/dev/nvme0n1` the namespace block device. NVMe block timeout:
  `/sys/block/nvme0n1/queue/io_timeout` (ms; module param `nvme_core.io_timeout`,
  default 30 s; UNVERIFIED exact path/default).

#### 2.2.8 Device identity (udev `60-persistent-storage.rules`, verified on this box)

`/dev/disk/by-id/` symlink names, which the engine uses as *stable* handles:

| Bus | Symlink pattern | Source of serial |
|-----|-----------------|------------------|
| ATA via libata | `ata-<ID_MODEL>_<ID_SERIAL_SHORT>` (`$env{ID_BUS}-$env{ID_SERIAL}`) | `ata_id --export` (IDENTIFY DEVICE words 10–19 serial, 27–46 model) |
| SCSI/SAS | `scsi-<ID_SERIAL>` | `scsi_id` (VPD page 0x80/0x83) |
| USB mass storage | `usb-<ID_USB_SERIAL>` (= `vendor_model_usbserial-0:0`) **and** usually an `ata-`/`scsi-` link if the bridge passes SAT | USB descriptor iSerial; many cheap bridges report a constant or empty serial |
| NVMe | `nvme-<ID_MODEL>_<ID_SERIAL_SHORT>` and `nvme-<ID_WWN>` (`eui.`/`nvme.` prefix) and `_1` namespace variant | `/sys/class/nvme/nvmeN/serial`, `wwid` |
| MMC/SD | `mmc-<ID_NAME>_<ID_SERIAL>` | `/sys/block/mmcblkN/device/{name,serial,cid}` |
| Virtio | `virtio-<serial>` | |

Fingerprint used by the engine (3.6): `(bus, model, serial, wwn?, size_bytes,
logical_bs)` plus `sha256(first 4 KiB)` once it has been read successfully. For
devices with no usable serial (many SD cards via USB readers, some bridges) the
fingerprint falls back to `(model, size, first-4KiB hash, partition-table GUID)`.

#### 2.2.9 Hotplug events (`lib/kobject_uevent.c`, `linux/netlink.h`)

- Socket: `socket(AF_NETLINK, SOCK_RAW | SOCK_CLOEXEC, NETLINK_KOBJECT_UEVENT /* 15 */)`,
  `bind` with `nl_groups = 1` (kernel multicast group 1; udevd re-broadcasts processed
  events on group 2 in libudev format, which we do not need). Unprivileged processes
  may receive group-1 uevents (UNVERIFIED on hardened kernels).
- Message: `"<ACTION>@<DEVPATH>\0"` followed by `KEY=VALUE\0` pairs. Actions:
  `add remove change move online offline bind unbind`. Max message 2048 bytes
  (`UEVENT_BUFFER_SIZE`, UNVERIFIED value). Relevant pairs: `SUBSYSTEM=block`,
  `DEVTYPE=disk|partition`, `DEVNAME=sdb`, `MAJOR`, `MINOR`, `SEQNUM`.
- Engine listens for `remove` of its device's `DEVPATH` (⇒ device gone, immediately,
  without waiting for a read to fail) and `add` of any `SUBSYSTEM=block DEVTYPE=disk`
  (⇒ candidate for re-identification). Fallback when the socket cannot be opened:
  poll `/dev/disk/by-id/` and `/sys/block/` every 500 ms.
- USB device-level sysfs: `/sys/bus/usb/devices/<bus-port>/authorized` (write `0`
  then `1` to deconfigure/reconfigure = software "replug" without power cycle;
  verified attribute, UNVERIFIED that it reliably re-enumerates mass-storage bridges),
  `/sys/bus/usb/drivers/usb/{unbind,bind}` (driver-core generic attributes).

### 2.3 Windows mapping (best effort; all UNVERIFIED unless noted)

| Linux concept | Windows equivalent |
|---------------|--------------------|
| `/dev/sdX` | `\\.\PhysicalDriveN` opened with `CreateFileW(GENERIC_READ, FILE_SHARE_READ\|FILE_SHARE_WRITE, OPEN_EXISTING, FILE_FLAG_NO_BUFFERING \| FILE_FLAG_OVERLAPPED)` |
| `O_DIRECT` alignment | `FILE_FLAG_NO_BUFFERING`: offset/length multiples of sector size, buffer aligned to sector size |
| `pread` with timeout | `ReadFile` with `OVERLAPPED.Offset/OffsetHigh`, wait with `GetOverlappedResultEx(timeout_ms)`; on timeout `CancelIoEx(h, &ov)` then wait for completion (the cancel may itself block until the miniport abandons the request) |
| `BLKGETSIZE64` | `IOCTL_DISK_GET_LENGTH_INFO` |
| `BLKSSZGET/BLKPBSZGET` | `IOCTL_STORAGE_QUERY_PROPERTY` with `StorageAccessAlignmentProperty` |
| serial/model | `IOCTL_STORAGE_QUERY_PROPERTY` with `StorageDeviceProperty` (`STORAGE_DEVICE_DESCRIPTOR.SerialNumberOffset`) |
| SMART ATA | `SMART_RCV_DRIVE_DATA` (`0x0007C088`) with `SENDCMDINPARAMS` |
| NVMe SMART | `IOCTL_STORAGE_QUERY_PROPERTY` with `StorageDeviceProtocolSpecificProperty`, `ProtocolTypeNvme`, `NVMeDataTypeLogPage`, log 0x02 |
| SG_IO | `IOCTL_SCSI_PASS_THROUGH_DIRECT` (`SCSI_PASS_THROUGH_DIRECT.TimeOutValue` in seconds) |
| `EIO` | `ERROR_IO_DEVICE` (1117), `ERROR_CRC` (23), `ERROR_SECTOR_NOT_FOUND` (27), `ERROR_DISK_OPERATION_FAILED` (1127) |
| `ENODEV/ENXIO` | `ERROR_DEVICE_NOT_CONNECTED` (1167), `ERROR_FILE_NOT_FOUND` (2) on reopen, `ERROR_NOT_READY` (21), `ERROR_NO_MEDIA_IN_DRIVE` (1112) |
| `EINTR`/cancel | `ERROR_OPERATION_ABORTED` (995); `ERROR_IO_PENDING` (997) is not an error |
| uevent | `RegisterDeviceNotificationW` + `WM_DEVICECHANGE` (`DBT_DEVICEARRIVAL`/`DBT_DEVICEREMOVECOMPLETE`) or `CM_Register_Notification` |
| sysfs timeout | `HKLM\SYSTEM\CurrentControlSet\Services\disk\TimeOutValue` (seconds, global, needs reboot) — do not touch by default |

Windows additionally auto-mounts and may write to the source (volume dirty bit, system
volume information); the engine warns and advises `diskpart offline`/`automount disable`
(cannot be enforced; UNVERIFIED that offline volumes stop all writes).

---

## 3. Algorithms (pseudocode)

### 3.1 Block-status map

A sorted `Vec<Sblock { pos: u64, size: u64, status: Status }>` exactly like ddrescue:
contiguous, non-overlapping, covering the whole input. A mapfile for a heavily damaged
multi-TB drive has 10^5–10^6 blocks; `Vec` with binary search for lookup and `O(n)`
splice for status changes is what ddrescue uses and is fast enough (ddrescue auto-tunes
save interval by `nblocks/38` precisely because writing the map, not mutating it,
dominates). An interval tree is **not** needed; `BTreeMap<u64 pos, (size,status)>` is an
acceptable alternative with `O(log n)` split/merge, but the sorted vec has better cache
behaviour for the sequential `find_chunk` scans and is what the pseudocode assumes.

```
struct Sblock { pos: u64, size: u64, status: Status }   // end() = pos + size
enum Status { NonTried='?', NonTrimmed='*', NonScraped='/', BadSector='-', Finished='+' }
rank(status) = { NonTried:0, NonTrimmed:1, NonScraped:2, BadSector:3, Finished:4 }

struct BlockMap {
    v: Vec<Sblock>,                 // invariant: sorted, contiguous, v[0].pos == 0
    cursor: usize,                  // index hint for sequential find_chunk (ddrescue index_)
    sizes: [u64; 5],                // total bytes per status, maintained incrementally
    bad_areas: u64,                 // maximal runs of BadSector
    current_pos: u64, current_status: PhaseChar, current_pass: u32,
}

fn find_index(map, pos) -> Option<usize>          // binary search over v by pos..end
fn change_chunk_status(map, b: Block, st: Status, domain: &Domain) -> i64 /*bad_areas delta*/ {
    // 1. split v so that b.pos and b.end are block boundaries
    // 2. for every block fully inside b and inside domain: sizes[old] -= size; sizes[st] += size; status = st
    // 3. merge equal-status neighbours around [b.pos, b.end)
    // 4. return bad-area delta computed from (left.status, old, right.status) -> (left, st, right):
    //    a BadSector run is created when st==Bad and neither neighbour is Bad (+1),
    //    destroyed when old==Bad, st!=Bad and neither neighbour is Bad (-1),
    //    split when old==Bad, st!=Bad and both neighbours Bad (+1),
    //    joined when st==Bad, old!=Bad and both neighbours Bad (-1).
}

// Find the first chunk of status `st` at or after b.pos, inside `domain`, cropping b to it
// and aligning b.end down to `alignment` unless b.end is the chunk end.
// after_finished=true (passes 3 and 4): only accept a chunk whose left neighbour is Finished,
// i.e. the chunks that were *skipped* (their left neighbour is the block that was read before skipping).
fn find_chunk(map, b: &mut Block, st, domain, alignment, after_finished=false) -> bool
fn rfind_chunk(map, b: &mut Block, st, domain, alignment, before_finished=false) -> bool  // mirror image
fn compact(map)                                   // merge equal-status neighbours globally (before final save)
fn join_subsectors(map, hardbs)                   // 2.1.4 rule 4
fn extend_to(map, insize)                         // 2.1.4 rule 3
fn truncate_vector(map, end, force=false) -> bool // drop blocks past `end`; refuse if that drops Finished data unless force
```

Domain: a second, read-only `Vec<Block>` (sorted, non-overlapping; built from
`--input-position/--size` and the intersection of all `--domain-mapfile` finished
blocks). `domain.includes(block)`, `domain < block` (block lies entirely after the
domain), `domain > block`.

### 3.2 Engine state machine

```
enum Phase { Copying{pass:1..=5, forward}, Trimming, Scraping, Retrying{pass, forward}, Finished }

run():
    open_and_fingerprint_source()
    load_or_create_map(); join_subsectors(hardbs); extend_to(insize); split at domain borders
    apply --retrim (NonScraped,BadSector -> NonTrimmed in domain) and --try-again (NonScraped,NonTrimmed -> NonTried)
    initialize_sizes()                      // also counts bad_areas; max_bad_areas += bad_areas if "+n" form
    start_timeout_watchdog_and_hotplug_listener()
    rates.start()
    if sizes[NonTried] > 0            : copy_non_tried()        ; abort on error/limit
    if sizes[NonTrimmed] > 0 && !no_trim   : trim_errors()
    if sizes[NonScraped] > 0 && !no_scrape : scrape_errors()
    if sizes[BadSector] > 0 && retry_passes != 0 : copy_errors()
    fsync(out); compact(map); map.current_status = '+' unless interrupted/error; save_map(force=true)
    exit code: 0 ok | 1 environment/limits | 2 corrupt input/mapfile | 3 internal | 128+sig if signalled
```

All reads go through `copy_and_update()`:

```
fn copy_and_update(b: Block, phase, pass, forward, fail_status: Status) -> Result<(copied, errored)> {
    if first_post_of_pass { if pause_on_pass>0 { sleep; shift timeout clock }; map.current_status/pass = ...; log pass start }
    map.current_pos = forward ? b.pos : b.end
    if limits_exceeded() { return Err(Limit) }           // e_code bits: 1 error-rate, 2 bad-areas, 4 timeout, 8 check-on-error, 16 read-errors, 32 slow-reads
    if interrupted() { return Err(Signal) }
    (copied, errored, err) = source.read_block(b, timeout_for(phase))   // 3.5
    if copied + errored < b.size {                        // EOF: device shrank / partial image
        if complete_only { domain.truncate(b.pos+copied+errored) }
        else if !truncate_vector(b.pos+copied+errored) { fatal("EOF found below the size calculated from mapfile") }
        initialize_sizes()
    }
    if copied > 0 { write_out(b.pos, copied); change_chunk_status(Block{b.pos, copied}, Finished) }
    if errored > 0 {
        error_sum += errored; read_errors += 1; if read_errors > max_read_errors { e_code |= 16 }
        st2 = if errored > hardbs { fail_status } else { BadSector }   // single-sector failure is final
        change_chunk_status(Block{b.pos+copied, errored}, st2)
        record_error(sidecar, b, err)                       // errno / sense / latency
        if err is DeviceGone { return Err(DeviceGone) }     // 3.6 handles it, then the caller retries the same block
        if !source.still_present() { return Err(DeviceGone) }   // ddrescue: stat(iname) fails -> "Input file disappeared"
    }
    Ok((copied, errored))
}
```

Note `fail_status`: copying passes pass `NonTrimmed`; trimming/scraping/retrying
pass `BadSector` for any size (they read single sectors anyway).

### 3.3 Copying phase (passes 1–5)

Source: `rescuebook.cc::copy_non_tried / fcopy_non_tried / rcopy_non_tried`, ddrescue manual "Algorithm".

Purpose of each pass:

| Pass | Direction (default) | Reads | Skips on | Why |
|------|---------------------|-------|----------|-----|
| 1 | forward | every NonTried chunk, cluster-sized | first error: skip `eskip` (doubling); slow read: skip `sskip` | get the good data fast, get out of bad areas fast, delimit them coarsely |
| 2 | backward | NonTried chunks | first error **or** slow read in a chunk: skip the **rest of that chunk** (`b.size = -1` ⇒ to chunk end) | approaching the bad area from the other side delimits it from the other edge; reading backwards also means the drive's own read-ahead does not plough into the bad area |
| 3 | forward | only chunks whose left neighbour is Finished (`after_finished`) = chunks that were skipped | rest of chunk at first error | re-try areas skipped for *slowness* in the same direction they were skipped |
| 4 | backward | chunks whose right neighbour is Finished (`before_finished`) | rest of chunk at first error | same, other direction |
| 5 | forward | everything still NonTried | **no skipping** | sweeping pass; guarantees every sector is tried once at cluster size |

Direction alternates after every pass unless `--unidirectional`; `--reverse` flips all.
`--cpass=` selects passes; `min_read_rate` (slow-skip) is only active in passes 1–2
(ddrescue sets `min_read_rate = -1` after pass 2). `--min-read-rate` is "ignored in all
passes but the first two".

```
fn copy_non_tried() {
    resume = (!cpass_given && map.current_status == '?'); if !resume || map.current_pass > 5 { map.current_pass = 1 }
    first_pass = map.current_pass; forward = !reverse
    for pass in 1..=5 {
        if pass >= first_pass && cpass_bitset has pass {
            r = if forward { fcopy_non_tried(pass, resume && pass==first_pass) } else { rcopy_non_tried(...) }
            if r != NoMoreBlocks { return r }                // error/limit/signal, or 0 == "nothing found at all"
        }
        if pass >= 2 { min_read_rate = -1 }                 // slow-skipping only in passes 1..2
        if !unidirectional { forward = !forward }
    }
}

fn fcopy_non_tried(pass, resume) {
    pos = 0; eskip = skipbs; sskip = 0; after_finished = (pass == 3 || pass == 4)
    if resume && domain.includes(map.current_pos) { b = Block{current_pos,1}; find_chunk(b, NonTried, domain, hardbs); if b.size>0 { pos = b.pos } }
    loop {
        b = Block{pos, softbs}
        found |= find_chunk(b, NonTried, domain, softbs, after_finished)
        if b.size == 0 { break }
        if pos != b.pos { eskip = skipbs; current_slow = false }       // new chunk: reset skip growth
        pos = b.end
        (copied, errored) = copy_and_update(b, Copying, pass, forward=true, NonTrimmed)?
        slow = rates.update()                                          // 3.4; true only in passes 1..2 when below min rate
        if slow { slow_reads += 1; if slow_reads > max_slow_reads { e_code |= 32; return Err } }
        if (errored > 0 || (slow && pass <= 2)) {
            if reopen_on_error { source.reopen()? }                   // close+open: drops kernel per-fd state, some bridges recover
            if pause_on_error > 0 { sleep(pause_on_error) ; shift clocks }
            if skipbs > 0 && pass <= 4 {
                b.pos = pos
                if pass >= 2        { b.size = REST_OF_CHUNK }         // skip rest of this chunk
                else if errored > 0 { b.size = eskip; eskip = if eskip <= max_skipbs/2 { eskip*2 } else { max_skipbs } }
                else /* slow, pass 1 */ {
                    if !prev_slow { sskip = max(skipbs, min(c_rate /*bytes/s*/, max_skipbs)) }   // first slow read: skip ~1 s worth of data
                    else if sskip <= max_skipbs/2 { sskip *= 2 } else { sskip = max_skipbs }
                    b.size = sskip
                }
                find_chunk(b, NonTried, domain, hardbs)                // crop the skip to the NonTried chunk
                if pos == b.pos && b.size > 0 { pos = b.end }          // perform the skip (the skipped bytes stay NonTried)
            }
        } else if errored == 0 && copied > 0 { eskip = skipbs }       // good data: reset skip size
        save_map_if_due()?
    }
    if !found { return Ok(0) }   // nothing of this status in domain at all -> phase done
    return NoMoreBlocks          // -3 in ddrescue: pass completed, continue with next pass
}
// rcopy_non_tried is the mirror: iterate with `end` going down, rfind_chunk, b = Block{end - skip, skip}, pass>=2 => skip to chunk start.
```

Skip-size defaults (`rescuebook.cc` constructor, `main.cc`):

```
min_skipbs    = 64 KiB
skipbs        = round_up(max(insize/100000, 64 KiB), 64 KiB)    // "initial" ; 0 disables skipping
max_skipbs    = if user did not set: max(skipbs, insize/100)    // 1 % of the input
both rounded up to a multiple of hardbs; valid range 64 KiB .. 1 EiB
cluster       = 64 KiB / hardbs sectors (softbs = 64 KiB) unless -c
```

Why doubling: a defect (scratch, head-crash debris, failing head/zone) is usually
contiguous; each read inside it costs seconds and damages the surface; doubling the
skip finds the far edge in `O(log)` attempts, and the skipped-over good data is
collected later by passes 2–5 cheaply. Why the skip is measured from the *end of the
failed block* and independent of the cluster size: so that `-c` tuning does not change
the escape behaviour. Why reset to initial when good data is found: a new bad area
should again be probed finely first.

### 3.4 Rates, slow detection, timeout, limits, ETA

Source: `rescuebook.cc::update_rates`.

```
t0 = start time; t1 = time of last rate update (1 s resolution); ts = time of last successful read; tp = simulated pause
first_size = finished bytes at start; last_size = finished bytes at last update
every call (after each read), if now > t1:
    a_rate     = (finished - first_size) / (now - t0)      // average over the run
    c_rate     = (finished - last_size)  / (now - t1)      // current, since last 1 s tick
    if finished != last_size { last_size = finished; ts = now }
    else if timeout >= 0 && now - ts > timeout && t1 > t0  { e_code |= 4 }       // --timeout: no successful read for `timeout`
    error_rate = error_sum / (now - t1); error_sum = 0
    if max_error_rate >= 0 && error_rate > max_error_rate   { e_code |= 1 }       // --max-error-rate (bytes of failed reads per second)
    t1 = now; prev_slow = current_slow
    current_slow = (now - t0 > delay_slow /*30 s*/) &&
                   ((min_read_rate > 0 && c_rate < min_read_rate) || (min_read_rate == 0 && c_rate < a_rate / 10))
    if !current_slow && reset_slow { slow_reads = 0 }
    return current_slow
max_read_rate: if finished - last_size > max_read_rate within the same second, sleep(1)  // bandwidth cap
clock jumps backwards are absorbed by shifting t0/ts; pauses shift t0/t1/ts so they do not count as "no progress"
```

`--min-read-rate=0` = auto: threshold recalculated every second as `a_rate / 10`.
`--delay-slow` (30 s) exists because `a_rate` is meaningless in the first seconds.
`--timeout` measures time since the **last successful read**, not wall time.
`--max-error-rate` measures bytes of *failed reads per second*, so it can trigger while
the bad-sector total does not change (areas being re-marked). `--max-bad-areas=[+]n`
counts maximal runs of `-` blocks (`+n` = new ones only). `--max-read-errors=n` counts
failed reads. `--max-slow-reads=n` counts slow 1-second ticks in passes 1–2 (needs
`--min-read-rate`; `--reset-slow` makes it "consecutive slow seconds").

DiskRescue additions:

```
sliding window: ring of 30 one-second samples (ddrescue keeps sliding_avg(30) for display)
per-phase ETA:
    Copying : remaining = sizes[NonTried]           ; rate = sliding avg of finished bytes/s (clusters)
    Trimming: remaining = non_trimmed blocks * 2 * hardbs (two edges) ; rate = sector reads/s (successful+failed)
    Scraping: remaining = sizes[NonScraped]         ; rate = sector reads/s
    Retrying: remaining = sizes[BadSector] * remaining passes ; rate = sector reads/s
    eta = remaining / max(rate, tiny) ; reported as "unknown" until 30 s of samples exist
hang stats: p50/p95/max read latency per phase; count of reads over `slow_read_ms`
```

### 3.5 Per-read timeout: worker thread + channel

Problem: a `pread()` into a bad sector can block for `sysfs timeout × (max_retries+1)`
plus the whole SCSI EH escalation (minutes) and is **uninterruptible** (`D` state):
signals do not return `EINTR` from block I/O, `pthread_cancel` cannot cancel it, and
`close()` on the fd from another thread does not abort it. The only userspace-visible
handle is time.

Design:

```
struct Reader {
    req_tx:  Sender<ReadReq>,            // bounded(1)
    resp_rx: Receiver<ReadResp>,
    worker:  Option<JoinHandle<()>>,     // owns the fd and an aligned buffer
    generation: u64,                     // incremented when a worker is abandoned
}
struct ReadReq  { gen: u64, pos: u64, len: usize, buf: AlignedBuf }         // buffer ownership moves to the worker
struct ReadResp { gen: u64, pos: u64, result: Result<usize, Errno>, latency: Duration, buf: AlignedBuf }

fn read_block(&mut self, pos, len, timeout) -> ReadOutcome {
    self.req_tx.send(ReadReq{gen: self.generation, ...})
    match self.resp_rx.recv_timeout(timeout) {
        Ok(resp) if resp.gen == self.generation => classify(resp),          // 3.7
        Ok(stale) => { drop(stale); /* a previous abandoned worker finally returned: ignore, re-arm */ }
        Err(Timeout) => {
            // The worker is stuck in the kernel. We cannot kill it. We *abandon* it:
            self.generation += 1
            self.abandoned.push(self.worker.take())           // keep JoinHandle so the thread is joined at exit if it ever returns
            self.worker = spawn_new_worker(open_source_again())  // new fd: a second open() usually succeeds immediately
            // The abandoned read may still complete later and its data is DISCARDED (we cannot trust ordering),
            // and its request is still queued in the kernel: the device will keep servicing it (seek, retries).
            // Therefore after a timeout the engine (a) marks the block NonTrimmed (copying) or BadSector (sector read),
            // (b) treats it like an error for skipping purposes, (c) waits `post_hang_cooldown` (default 2 s) before the
            // next read so the queued request has a chance to drain, (d) counts it in `hangs`.
            return ReadOutcome::Hang
        }
    }
}
```

Cost of a hung thread: one OS thread (8 MiB virtual stack, ~64 KiB resident), one fd,
one aligned buffer (cluster size), and — the real cost — one outstanding request in the
device queue that the drive is still grinding on. If `hangs` exceeds `max_hangs`
(default 8 per 10 min for HDD, 3 for USB) the engine stops issuing reads and goes to
the *reset ladder* (3.6) because stacking hung requests only makes the drive worse.
Abandoned threads are detached at process exit (`std::process::exit` does not wait for
them); the process may stay in `D`-wait until the kernel completes/aborts the I/O. The
mapfile is saved **before** spawning the replacement worker so a subsequent crash loses
nothing.

Choosing the timeout: default = `max(2 × observed p95 latency, floor)` with
floor per device kind (3.9); hard cap `--timeout-per-read`. A read that returns
successfully after the engine abandoned it is **not** used (data goes to the discard
path) to keep the write path strictly ordered with the map.

Reducing kernel-side blocking (root only, best effort, logged, restored at exit):

```
echo 7  > /sys/block/sdX/device/timeout      # from 30 s: a hung command becomes EIO after 7 s per attempt
echo 2  > /sys/block/sdX/device/eh_timeout
echo 0  > /sys/class/scsi_disk/H:C:T:L/max_retries   # no kernel retries; the engine does its own retry phase
```

Alternative read path (opt-in, root): SG_IO READ(10/16) with `timeout` in ms
(2.2.5). The kernel still runs EH when the command times out, but the engine gets the
timeout result and sense data as soon as EH finishes, and `max_retries` does not apply
to pass-through commands. On NVMe there is no SG_IO; use `NVME_IOCTL_IO_CMD` with
`timeout_ms` (UNVERIFIED that the read opcode `0x02` passthrough is allowed
unprivileged; assume root).

Linux `io_uring`/AIO are **not** used: they still cannot cancel a request the driver
has already issued, and they complicate O_DIRECT buffer ownership. A thread per read
path is simpler and matches the "one outstanding request" discipline that failing
drives need (queue depth 1 is deliberate: NCQ reordering on a failing HDD causes extra
seeks into bad areas and makes latency attribution impossible).

### 3.6 Device loss, re-identification and re-attach

Triggers (any of): `read()` returns `ENODEV/ENXIO/ENOENT/ENOMEDIUM`; `stat(path)` fails
after an error; uevent `remove` for our `DEVPATH`; sysfs `device/state` ∉ {`running`};
NVMe `state` ≠ `live`; `hangs` over threshold.

```
fn handle_device_gone() {
    save_map(force=true)
    abandon_worker(); close fds; drop /dev path (it may be reused by a different disk!)
    announce("device lost at pos X; waiting for re-attach (serial S) ...")
    loop until deadline (--reattach-timeout, default infinite for USB/SD, 10 min for HDD):
        wait for uevent add(block,disk) or 500 ms poll
        for each candidate in /sys/block/* (excluding loop/ram/dm/zram and the destination's device):
            fp = fingerprint(candidate)         // from sysfs only: no reads yet
            if fp.serial == saved.serial && fp.model == saved.model && fp.size == saved.size { break found }
            if saved.serial is empty/generic and fp.model == saved.model && fp.size == saved.size:
                 read 4 KiB at 0 (guarded) and compare sha256 with saved.first_sector_sha256; also compare GPT disk GUID if present
                 if equal { break found }
        if device reappeared under the OLD name but with a different fingerprint: refuse ("a different disk now owns /dev/sdX")
    reopen by /dev/disk/by-id/<link> if present (stable), else by the new /dev name; re-run open_and_fingerprint (compare again)
    reattach_count += 1 ; sleep(settle_delay: 3 s USB, 10 s HDD)   // let the bridge/firmware finish initialising
    if reattach_count within the last 10 min > reattach_storm (5): switch strategy downgrade (smaller cluster, longer pause, no slow-skip)
    resume at map.current_pos in the same phase/pass (the mapfile guarantees nothing is lost)
}
```

Reset ladder (used when the device is present but stuck: repeated hangs, `offline`
state, NOT READY sense). Each step requires root; unavailable steps are skipped;
every step is logged with its outcome; a hook (`--on-reset-hook CMD`) runs user
scripts (e.g. uhubctl, a USB relay, a smart plug) and the engine then waits for
re-attach:

```
1. close + reopen fd                                   (ddrescue --reopen-on-error; clears per-fd state, cheap)
2. write "offline" then "running" to device/state ; write 1 to device/rescan      (SCSI soft reset at sdev level)
3. USB: echo 0 > .../authorized ; sleep 1 ; echo 1 > .../authorized             (software replug) UNVERIFIED efficacy
   USB: unbind/bind the usb driver for the device   (/sys/bus/usb/drivers/usb/{unbind,bind})
   NVMe: echo 1 > /sys/class/nvme/nvmeN/reset_controller
4. echo 1 > device/delete ; echo "- - -" > /sys/class/scsi_host/hostN/scan        (full re-probe)
5. external power cycle via --on-reset-hook; then re-attach wait
```

Never: `hdparm` write-class commands, `BLKRRPART`, ATA soft-reset via SG_IO
(SAT allows it but it is a write-class management command and some bridges wedge).

### 3.7 Error classification

```
fn classify(resp) -> ReadOutcome {
    match resp.result {
        Ok(n) if n == len            => Good{latency}
        Ok(n) /* short, errno 0 */   => Eof{n}
        Err(EINTR)                   => retry immediately (max 3), then Transient
        Err(EIO|ETIMEDOUT|EREMOTEIO|EBADMSG|EILSEQ) => Bad{kind: from sense if SG_IO else Unknown}
        Err(ENODEV|ENXIO|ENOENT|ENOMEDIUM) => DeviceGone
        Err(EINVAL)                  => Fatal("Unaligned read error. Is sector size correct?")
        Err(EACCES|EPERM|EBADF|EBUSY|EISDIR|ENOMEM|ENOSYS|ESPIPE|ENOBUFS) => Fatal(errno)
        Err(other)                   => Bad{kind: Unknown}  // conservative: treat as read error, log errno
    }
    // sense refinement (SG_IO path):
    //   MEDIUM_ERROR(0x3)/ASC 0x11       -> Bad{Medium}      : genuine bad sector; Information field gives exact LBA -> mark only that sector Bad, rest of cluster NonTrimmed
    //   HARDWARE_ERROR(0x4)              -> Bad{Hardware}    : head/servo; raise cooling/pause, count toward reset ladder
    //   ABORTED_COMMAND(0xB)             -> Bad{Aborted}     : often cable/bridge/reset; retry once after 1 s before marking
    //   NOT_READY(0x2)                   -> NotReady         : wait (spin-up / after reset) up to 30 s, then DeviceGone path
    //   UNIT_ATTENTION(0x6)/ASC 0x29     -> Reset occurred   : re-identify (fingerprint) then retry the block once
    //   RECOVERED_ERROR(0x1)             -> Good but flag "weak sector" (count in sidecar)
    //   host_status DID_TIME_OUT/DID_NO_CONNECT/DID_BAD_TARGET -> Hang / DeviceGone respectively
}
```

A block that produces **good data on one read and garbage on another** (USB bridge
returning zeros/stale data: "all reads could be considered good when there is not
actually data being read") is detected by `--check-on-error` behaviour from ddrescue
(`-J`): after every read error re-read the last good sector and compare; if the
device "no longer returns data" or "returns inconsistent data" stop (exit 2) because
every further "successful" read is suspect. DiskRescue enables this automatically for
USB devices and additionally verifies that a cluster is not entirely `0x00`/`0xFF`
immediately after an error (such clusters are re-read once before being accepted).

### 3.8 Mapfile persistence (atomic)

```
fn save_map(force) {
    if !force && now - last_save < interval { return }
    fsync(out_fd)                                            // data must be durable before the map says it is
    write map to <map>.tmp (header 2.1.3; current_pos/status/pass; blocks, uncompacted is fine)
    fsync(<map>.tmp)
    if last_save_was_synced { unlink(<map>.bak); hard-link or copy <map> -> <map>.bak }   // ddrescue-compatible backup
    rename(<map>.tmp, <map>)                                 // atomic replace: a crash leaves either old or new, never a torn file
    fsync(dir)
    write <map>.dr.json the same way (tmp + rename)
    last_save = now (after the write, excluding write time) ; if synced { last_sync = now }
    on ENOSPC/EIO writing the map: retry every 5 s, write an emergency copy to $XDG_RUNTIME_DIR or $HOME/diskrescue-emergency.map, never lose the in-memory map, keep rescuing
}
```

Save interval = ddrescue auto (`30 + min(270, nblocks/38)` s) unless set; **plus**
forced saves at: pass/phase boundaries, before abandoning a worker, before any reset
ladder step, on `SIGINT/SIGTERM/SIGHUP` (handled like ddrescue: set a flag, finish the
current read, save, re-raise the signal), and every `--mapfile-interval`.

Windows: `MoveFileExW(MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)` replaces
`rename`; `FlushFileBuffers` replaces `fsync` (UNVERIFIED directory-fsync equivalent;
none needed on NTFS).

### 3.9 Device-type strategies

Device kind detection (Linux): `/sys/block/X/device/../..` subsystem chain
(`usb` ⇒ usb_flash or usb-bridged HDD/SSD — decide by `queue/rotational` and model
string; `mmc` ⇒ sd_mmc; `nvme` ⇒ ssd_nvme; `ata`/`scsi` + `queue/rotational == 1`
⇒ hdd, `== 0` ⇒ ssd_sata). `removable`, `queue/rotational`, IDENTIFY word 217 =
1 ⇒ SSD (via SAT, optional). User override `--device-kind`.

| Parameter | HDD (spinning) | SATA SSD | NVMe SSD | USB flash / pen drive | SD / MMC card (reader or mmcblk) |
|-----------|----------------|----------|----------|------------------------|----------------------------------|
| cluster (`softbs`) | 64 KiB pass 1–2 (ddrescue default); 128 KiB–1 MiB only if latency p95 < 20 ms; ≤ `max_sectors_kb` | 256 KiB (no seek penalty; large reads amortize NAND page/ECC latency) | 512 KiB–1 MiB, ≤ `max_sectors_kb` (128 KiB observed) | 32–64 KiB (bridges wedge on big transfers; OSC: limit "smaller for a USB connected drive") | 32 KiB (readers often cap at 64 KiB; SD cards time out internally on long reads) |
| initial skip | `max(insize/100000, 64 KiB)` | 1 MiB (block = erase block ~1–8 MiB; a failed read usually means the whole flash block/page group is dead) | 1 MiB | 1 MiB | 512 KiB |
| max skip | 1 % of size (ddrescue) | 1 % | 1 % | 1 % | 1 % |
| skipping on slow | on, `--min-read-rate=0` (auto = a_rate/10) | off (slowness on SSD is ECC retry = soon to fail, but no mechanical cost to persist; keep reading) | off | on, `min_read_rate` = 1 MiB/s (bridge about to lock up) | on, 512 KiB/s |
| per-read timeout | 15 s pass 1–2, 30 s trim/scrape, 45 s retry | 5 s / 10 s / 20 s | 3 s / 10 s / 20 s | 10 s / 20 s / 30 s | 10 s / 20 s / 30 s |
| sysfs `device/timeout` (root) | 7 s | 5 s | n/a (`io_timeout`) | 10 s | n/a |
| `max_retries` (root) | 0 | 1 | n/a | 0 | n/a |
| pause-on-error | 0 by default; auto 1–3 s when SMART 198 raw grows > 10 per 10 min or sense = HARDWARE ERROR | 0 | 0 | 0.5 s (lets the bridge recover) | 0.5 s |
| cooling pause | every 20 min of continuous reading in damaged zones (phase ≠ copying pass 1–2) pause 60 s if SMART temperature (attr 194) rose > 8 °C since start or `hangs` > 3 in 10 min | none | none (thermal throttling is the controller's job) | none | none |
| reverse reads | pass 2 is backward (ddrescue default); if pass 1 found > 1000 errors in the first 1 % of the disk, `--reverse` the whole run (bad area at the start = typical outer-track/head-0 damage: reach the good data first) | irrelevant (no locality cost) | irrelevant | irrelevant | irrelevant |
| trimming | on | **off by default** (`--no-trim`): flash failures are page/block granular; trimming sector-by-sector inside a dead 16 KiB page yields nothing and burns read-disturb budget. Scrape directly with `cluster/8` (OSC "dividing" idea) then sector scrape of what remains | off; divide with `cluster/8` | on (bridges often fail only a few sectors) | on |
| scraping | on | on, after dividing | on, after dividing | on | on |
| retry passes | 2 (`-r2`), alternating direction; more only on explicit request | 3 (ECC may succeed on a later try, no mechanical risk; stop if 0 sectors recovered in a pass) | 3 | 1 | 1 |
| re-attach | wait 10 min | wait 10 min | wait 10 min (controller reset) | wait forever, re-identify by serial; bridge serial often generic ⇒ fallback fingerprint | wait forever |
| check-on-error (`-J`) | off (extra reads cost head hits) | off | off | **on** | **on** |
| queue depth | 1 | 1 (deliberately; see 3.5) | 1 | 1 | 1 |
| SMART polling | 5/197/198/199/194 every 10 min, before/after phases | 5/197/198 + 177/231 wear (vendor) every 10 min | log page 02h every 10 min: `media_errors`, `critical_warning` bit 2/3, `percent_used`, `avail_spare` | n/a (bridges rarely pass SAT; try once) | n/a |
| stop condition suggestions | `--max-slow-reads` with `--reset-slow` for "slow responding" drives that recover after a power cycle (OSC `--exit-on-slow` rationale); `--max-bad-areas=+N` to stop a drive that is deteriorating | `critical_warning` bit 2 set + media_errors rising fast ⇒ warn, continue (nothing mechanical to save) | same | 3 re-attaches in 10 min ⇒ advise a different reader/port/cable | same |

Rationale per device:

**HDD.** Errors are *clustered* (a scratch spans many consecutive tracks, a weak head
affects every Nth stripe because tracks are interleaved across heads, debris from a
head crash grows with every pass over it). Every read in a bad zone costs seconds of
retries and physical wear; the drive's own retry/recalibration ("clicking") is far more
harmful than our reads. Hence: cluster small enough to localize errors but not so small
that the drive spends its life seeking; skip exponentially; come back later from the
other side; do the fine-grained work (trim/scrape) last and only once; keep queue depth
1 so each latency sample is attributable; watch 197/198: if 198 grows steadily the
surface is being damaged *by the rescue*, so pause (cool, let the drive park) and
prefer finishing the copying passes elsewhere before scraping. OpenSuperClone's
"skip out of a bad head in about 7 read errors" heuristic is approximated by the
doubling skip (7 doublings of 64 KiB ≈ 8 MiB; with `insize/100000` initial skip on a
2 TB drive, 20 MiB → 2.5 GiB in 7 steps); DiskRescue records a `head_stride` estimate
in the sidecar when errors recur at a constant stride (candidate for a later
stride-aware skipper, not in v1).

**SATA/NVMe SSD.** No mechanical penalty, so persistence is free in wear terms, but
(a) failed reads are page/block granular (4–16 KiB pages, 1–8 MiB erase blocks), so
sector-level trimming is pointless; (b) *read disturb*: repeatedly reading a weak
block raises error rates in its neighbours, so retries should be bounded and spaced,
not infinite; (c) a controller that has entered **read-only mode** (NVMe critical
warning bit 3, SATA vendor-specific) still serves reads, often slowly, and may stop
responding entirely after a power cycle, so **do not power cycle an SSD that is still
answering**; (d) SSD latency outliers are ECC retries that still return correct data,
so slow-skip would discard recoverable data; (e) a failing SSD controller frequently
drops off the bus and returns as a 0-byte/"SATAFIRM S11"-style device; the fingerprint
match must then fail loudly rather than silently resuming on a different identity.

**USB flash / pen drives.** The failure is usually the controller or the bridge, not
the NAND: lockups after an error (the controller firmware hangs in its own retry loop),
disconnect/reconnect (new `/dev/sdX`, often with an empty or generic serial such as
`General_UDisk` or `0123456789ABCDEF`), and bridges that answer reads with zeros or
stale buffer contents after a fault. Hence: small transfers (bridges and the
usb-storage driver have a 120 KiB-ish default limit and some firmware panics above
64 KiB), short timeouts (a stuck bridge will not come back by waiting), re-open on
every error, aggressive re-attach handling, `check-on-error` on, and a hard cap on how
many times we will re-plug before telling the user to try another port/cable/reader.

**SD / MMC cards.** Same as USB flash when behind a USB reader, plus: readers often
enforce a 64 KiB max transfer; cards with a dying controller report a fake size
(e.g. 30 MB) after a fault — treat a size change on re-attach as a mismatch; via a
native `mmcblk` host the kernel's `mmc` driver has its own timeouts and no `sysfs
timeout`; the card may enter permanent write-protect (irrelevant) or "locked" state
(`NOT READY`).

### 3.10 What other tools do differently (and what DiskRescue adopts)

| Tool | Distinctive behaviour | DiskRescue |
|------|------------------------|------------|
| **GNU ddrescue** | Pure `read()`-based; relies on kernel EH; 5 copy passes + trim + scrape + retry; mapfile; no device identity, no timeouts per read (only `--timeout` since last good read). | Core algorithm and mapfile adopted verbatim. |
| **HDDSuperClone / OpenSuperClone** | SCSI/ATA pass-through (sense/ATA status per read, per-command timeouts, marks exact bad LBA from ATA return data), 4 copy phases (fwd skip-on-error, bwd skip-on-error, fwd rate-skip, fwd no-skip), self-learning head-skip (“about 7 read errors”), `--skip-threshold` 2000 ms read-time skip trigger, `--max-skip-size` reset to prevent runaway skipping, *dividing* passes (cluster/8 or /4 then /16) instead of trimming, **virtual disk** (`/dev/sdv` via a kernel driver: data is read from the source *on demand* when a filesystem tool reads the virtual disk, so only the sectors a file system actually needs are ever touched), USB relay power cycling, "direct IO" IDE/AHCI register access bypassing the OS (pro). Progress log in LBA hex with extra skip/error columns; imports/exports ddrescue maps. | Pass-through read path (opt-in), sense-aware classification, per-read timeouts, latency-based skip trigger (`slow_read_ms` = 2000 ms default in passes 1–2), max-skip reset, dividing instead of trimming for flash, reset/power-cycle hook. Virtual-disk/on-demand imaging is planned as `--domain-from-filesystem` (image only used blocks + metadata first), not a kernel driver. |
| **DMDE** | Windows/Linux; its own "disk image" with bad-sector map; read settings (retries, timeouts, "skip after N errors", block size), can work on a partial image + device simultaneously; file-system reconstruction drives *which* sectors to read. (UNVERIFIED: details from vendor docs not re-read.) | Domain mapfiles + later FS-driven domains. |
| **R-Studio (Technician)** | "Multi-pass imaging": good areas first, bad/slow later; runtime imaging (data read once from the device is cached in the image and served from it afterwards); timeouts "depend more on the OS"; read attempts count; exclusive regions. | Same principle; DiskRescue's later stages always read from the image+map and request the engine to fill missing blocks on demand (`command mode` like ddrescue: `c pos size`). |

ddrescue command mode (adopted as the engine's library API for later stages):
`c pos size` copy on demand (finished areas are not re-read), `s pos size` status in
mapfile format, `u` write mapfile, `f` compact+write+quit, `q` quit without writing.

---

## 4. Edge cases and failure modes seen on real failing media

1. **Kernel blocks for minutes on one sector.** Default sd timeout 30 s × up to 5
   retries + EH resets. Mitigation: worker abandonment (3.5), sysfs tuning, SG_IO.
2. **Drive goes "offline" (`device/state`).** EH exhausted; reads return `EIO`
   instantly for every block ⇒ the engine would mark the whole disk bad at high speed.
   Guard: if ≥ 50 consecutive errors each return in < 5 ms, stop, check `state`,
   run the reset ladder, then *un-mark* the blocks errored during that burst (they are
   recorded in the sidecar `error_burst` and reverted to the status they had before).
3. **Device disappears and a different disk takes its `/dev` name** (hot-plug of the
   destination, docking stations, two identical drives). Fingerprint check (3.6);
   refuse to read if the identity does not match; refuse to write if the *destination*
   fingerprint changed.
4. **USB bridge returns zeros / stale data after an error.** `check-on-error`,
   all-zero/all-FF cluster re-read, bridge-generic serial handling. Advise direct SATA.
5. **Bridge lies about sector size** (512e vs 4Kn through USB, or 4 KiB logical
   emulation): mapfile positions become multiples of 4096; `join_subsectors` on load;
   `--sector-size` override; detection via `BLKSSZGET` vs IDENTIFY word 106
   (via SAT).
6. **Reported capacity wrong** (card controllers in failure mode report 30 MB or 2 TB;
   HDD with a corrupted translator reports 0 or a different size). `-C/--complete-only`
   semantics: trust the mapfile extent; a size *change* on re-attach is a mismatch.
7. **EOF before mapfile end** (partial image as input, device shrank): `truncate_vector`
   unless finished blocks would be lost ⇒ fatal with the ddrescue message.
8. **Slow-responding drive** (reads succeed but at 50 KB/s after an error; often cured
   temporarily by power cycle): `--min-read-rate` + `--max-slow-reads` + `--reset-slow`,
   or `--exit-on-slow` behaviour (exit code 16 in OSC) + power-cycle hook + resume.
9. **Runaway skipping**: skip size reaches max and the rest of the disk is skipped in
   huge steps, leaving almost everything NonTried for the slow backward pass. OSC
   resets to the initial size when the max is reached; ddrescue caps at max. DiskRescue
   follows OSC (reset to initial when max is hit) and counts `skip_resets`.
10. **Errors at a constant stride** (bad head on a multi-head drive): recorded as
    `head_stride`; v1 only reports it; the user may run with `--domain-mapfile` built
    from "every Nth stripe" later.
11. **Thermal runaway**: a drive that gets hotter as it retries; SMART 194/190. Cooling
    pauses; advise a fan.
12. **Destination problems**: `ENOSPC` while writing the image (stop immediately, keep
    map); destination on the same physical disk/USB hub as the source (warn: bandwidth
    and power); sparse output (`--sparse` semantics: zero clusters are not written, so
    the output must be pre-zeroed or new).
13. **Mapfile written by an older/newer tool**: `logfile` naming, `pass` field missing
    (default 1), decimal numbers, comments after whitespace, size `-1` for unknown,
    loose domain files. All accepted per 2.1.1.
14. **Interrupt in the middle of a backward pass**: `current_pos` is the *end* of the
    block; resume uses `rfind_chunk(current_pos - 1)`.
15. **Mixed cached/direct reads of the same device** (user ran `dd` meanwhile): stale
    page cache ⇒ always O_DIRECT on devices; on regular-file inputs use buffered.
16. **O_DIRECT unsupported** (tmpfs, some FUSE, 9p): `open` fails with `EINVAL` ⇒ fall
    back to buffered with a warning (ddrescue warns "direct disc access not available").
17. **Signals**: `SIGINT/TERM/HUP` set a flag; the engine saves the map and re-raises
    (exit 128+sig). `SIGUSR1/2` ignored (ddrescue) — DiskRescue uses `SIGUSR1` = force
    map save, `SIGUSR2` = print status.
18. **Power loss of the host**: map `.tmp` + rename guarantees a consistent map; the
    image may contain blocks the map does not mark finished (harmless: they are re-read)
    but never the reverse, because `fsync(out)` precedes every map write.
19. **Optical media / 2048-byte sectors**: supported through `--sector-size=2048`; no
    special handling in v1.
20. **Same-file / overlapping in/out** (`--same-file`): disallowed in DiskRescue (R1).

---

## 5. Test fixture strategy (no root; only the allowed tools)

The engine is written against a `BlockSource` trait; production implements it with
`pread` on a device; tests implement it with a **fault-injecting file source** driven
by a fault map in ddrescue mapfile syntax — the same idea as ddrescue's
`--test-mode=FILE` (`-H`), where "blocks marked as finished in FILE are read normally;
all other block types are considered read errors without even trying to read them".

```rust
trait BlockSource {
    fn size(&self) -> Option<u64>;
    fn sector_size(&self) -> u32;
    fn read_at(&mut self, pos: u64, buf: &mut [u8]) -> ReadResult;   // may sleep to emulate latency
    fn fingerprint(&self) -> Fingerprint;
    fn still_present(&self) -> bool;
    fn reopen(&mut self) -> io::Result<()>;
}

struct FaultyFile {
    file: File,
    faults: Vec<Fault>,            // sorted, from a mapfile: '+' = good, '-' = EIO, '*' = EIO (large area), '/' = slow(latency_ms), '?' = unspecified
    latency: LatencyModel,         // per-status fixed or random latency; e.g. '-' blocks sleep 600 ms then fail (bad sector), '/' sleep 3 s then succeed (slow area)
    disappear_at: Vec<(read_index, Duration)>,  // read #N returns ENODEV, still_present()=false for Duration, reopen succeeds after
    garbage_after_error: bool,     // next K reads after an EIO return zeros with Ok (USB bridge lie)
    hang_at: Vec<read_index>,      // read blocks forever (thread parks) -> exercises worker abandonment
    reported_size_override: Option<u64>,  // wrong capacity
}
```

Fixture image generation (all unprivileged):

```bash
set -euo pipefail
F=target/fixtures; mkdir -p $F/src

# 1. realistic payload: photos and videos
python3 - <<'EOF'
from PIL import Image; import random, os
os.makedirs("target/fixtures/src/DCIM", exist_ok=True)
for i in range(40):
    im = Image.effect_noise((1600, 1200), 64).convert("RGB")
    im.save(f"target/fixtures/src/DCIM/IMG_{i:04d}.jpg", quality=90)   # ~300-500 KiB each
    if i % 10 == 0: im.save(f"target/fixtures/src/DCIM/IMG_{i:04d}.png")
EOF
ffmpeg -y -f lavfi -i testsrc=size=640x360:rate=25 -f lavfi -i sine=frequency=440 -t 20 \
       -c:v libx264 -pix_fmt yuv420p -c:a aac -movflags +faststart $F/src/DCIM/MOV_0001.mp4
ffmpeg -y -f lavfi -i testsrc=size=640x360:rate=25 -t 10 -c:v libx265 -tag:v hvc1 $F/src/DCIM/MOV_0002.mov
ffmpeg -y -f lavfi -i testsrc=size=320x240:rate=15 -t 5  -c:v mjpeg -c:a pcm_s16le -f avi $F/src/DCIM/MOV_0003.avi

# 2. file-system images (populated without root)
truncate -s 64M $F/ext4.img && mkfs.ext4 -q -F -d $F/src $F/ext4.img
truncate -s 64M $F/fat32.img && mkfs.vfat -F 32 $F/fat32.img          # populate via the project's own FAT writer or mtools if present
truncate -s 64M $F/exfat.img && mkfs.exfat $F/exfat.img
truncate -s 64M $F/ntfs.img && mkntfs -F -Q $F/ntfs.img && ntfscp $F/ntfs.img $F/src/DCIM/IMG_0000.jpg /IMG_0000.jpg

# 3. a "disk" with MBR + the images at known offsets (python writes the MBR; see partition spec)
python3 tools/mkdisk.py $F/disk.img --mbr 1M:$F/fat32.img 70M:$F/ext4.img
# 4. golden hash for verification
sha256sum $F/disk.img > $F/disk.img.sha256
```

Fault maps (plain text, ddrescue syntax, hand-written in `tests/faults/*.map`):

```
# faults: one bad cluster, one 3 MiB bad area with a good island, one slow zone
0x00000000 ? 1
0x00000000  0x00100000  +
0x00100000  0x00000200  -          # single bad sector -> must end as one 512 B '-' block
0x00100200  0x002FFE00  +
0x00400000  0x00100000  -          # 1 MiB bad
0x00500000  0x00001000  +          # 4 KiB good island (scraping must find it)
0x00501000  0x00200000  -
0x00701000  0x00400000  /          # slow (3 s per read): pass 1 must skip it, pass 3 must come back
0x00B01000  0x034FF000  +
```

Test matrix (each test: run engine to completion on `FaultyFile(disk.img, faults)`,
then assert):

| Test | Assertion |
|------|-----------|
| `copy_pass_shape` | after pass 1 only: every `-`-marked region is NonTrimmed or NonTried (skipped), finished bytes ≥ expected; read log shows skip sizes doubling 64 KiB→128→256… and reset after good data |
| `five_passes_direction` | read log positions: pass 1 ascending, pass 2 descending, pass 3 only reads chunks after a finished chunk, pass 5 covers all remaining with no skips |
| `trim_scrape_exact` | final map: `-` blocks equal the fault map's `-` regions exactly (byte-exact), the good island is `+`, no `*` or `/` remain |
| `image_bytes` | `out[pos..]` == `disk.img[pos..]` for every `+` block; `-` blocks are zero in a fresh output; `sha256` of out with fault regions zeroed == expected |
| `mapfile_roundtrip` | our writer's output parses with our parser **and** matches the ddrescue 1.28 writer format byte-for-byte for the ddrescue testsuite maps (`testsuite/mapfile1..6`, `mapfile_blank` embedded as fixtures) |
| `mapfile_compat_inputs` | parse: decimal numbers, missing pass field, comments after whitespace, `logfile` header, size `0x7FFFFFFFFFFFFFFF - pos`, loose domain with gaps/unordered, subsector boundaries (expect `join_subsectors` result == `mapfile6j`) |
| `resume_every_phase` | kill the engine (return `Err(Signal)`) after read #N for N in a sweep; restart from the saved map; final map and image identical to the uninterrupted run; total reads ≤ uninterrupted + one cluster per interruption |
| `atomic_save` | simulate crash between `.tmp` write and `rename` (inject failure in the FS layer) ⇒ old map intact and parseable |
| `worker_abandon` | `hang_at=[k]`: `read_block` returns `Hang` within `timeout + 100 ms`; the block is NonTrimmed; generation increments; the parked thread's late completion is discarded; process exits without joining |
| `device_gone_reattach` | `disappear_at=[(k, 2 s)]`: engine saves map, waits, `reopen` succeeds, fingerprint matches, resumes at `current_pos`; with a fingerprint mismatch injected ⇒ refuses and exits 1 |
| `bridge_lies` | `garbage_after_error=true` + check-on-error ⇒ exit 2 "returns inconsistent data"; the garbage cluster is not marked finished |
| `rates_and_limits` | `--timeout=2s` with a source that stops returning data ⇒ `e_code & 4`; `--max-error-rate` with a burst ⇒ `e_code & 1`; `--max-slow-reads=3 --min-read-rate=1M` ⇒ `e_code & 32` |
| `eof_truncate` | source smaller than mapfile: truncation unless finished data beyond ⇒ fatal message |
| `odirect_alignment` | on a tmp regular file opened with O_DIRECT (where the FS supports it), unaligned cluster at a domain border is read via the pre/post expansion and only the wanted bytes are written |
| `strategy_table` | `resolve_strategy(DeviceKind::UsbFlash)` yields cluster ≤ 64 KiB, check_on_error = true, no_trim = false, etc. (table 3.9 snapshot test) |
| `eta_monotone` | with constant synthetic rate, ETA decreases monotonically and reaches 0 at the end |

Optional, **root-only, not in CI** (documented for manual validation on the dev box):
`losetup` the fixture and run against `/dev/loopN` (exercises `BLKSSZGET/BLKGETSIZE64`
and O_DIRECT on a real block device); `dm-flakey`/`dm-error` targets for EIO
injection with real kernel semantics; `modprobe scsi_debug dev_size_mb=64 opts=2
every_nth=100` for synthetic MEDIUM ERRORs through the full SCSI stack (UNVERIFIED
`opts` bit values); a real USB stick with `authorized` toggling for the re-attach path.

Unit tests for the map: property tests (`proptest`) that random sequences of
`change_chunk_status` keep the invariants (sorted, contiguous, covering, no equal
neighbours after compact, `sizes[]` and `bad_areas` consistent with a recount).

---

## 6. Recommended Rust crates

Versions are `max_stable_version` from crates.io on 2026-10-12 unless tagged.

| Purpose | Crate | Version | Notes |
|---------|-------|---------|-------|
| raw syscalls, ioctl numbers, errno | `libc` | 0.2.190 | `pread64`, `ioctl`, `posix_memalign`, `socket(AF_NETLINK)`; define `BLKSSZGET`… locally from 2.2.1 (libc has them on Linux; verify) |
| safer wrappers (optional) | `rustix` | 1.1.5 | pure-Rust syscall layer; `rustix::fs::{open, pread}`, `rustix::net` for netlink; prefer over `nix` for new code |
| alternative wrappers | `nix` | 0.31.3 | `ioctl_read!` macros, `sys::stat`, `unistd::fsync`; fine if the team already uses it |
| worker/engine channels | `crossbeam-channel` | 0.5.17 | `recv_timeout`, bounded(1) request channel; `std::sync::mpsc` also has `recv_timeout` and is acceptable |
| interval map (alternative to sorted vec) | `rangemap` | 1.8.0 | `RangeMap<u64, Status>` auto-coalesces equal adjacent values; good for the domain and for a simpler map; does not maintain per-status totals or `bad_areas` (keep those outside) |
| interval set with fast queries | `iset` | 0.3.3 | interval tree; only if `BTreeMap`/vec prove too slow (not expected) |
| hashing for fingerprint / verification | `sha2` | 0.11.0 | first-4 KiB hash, image verification |
| fast CRC for optional per-cluster checksums in sidecar | `crc32fast` | 1.5.2 | |
| CLI | `clap` | 4.6.7 | derive API; ddrescue-compatible long options |
| TUI (progress screen) | `ratatui` | 0.30.2 | optional; `indicatif` 0.18.6 for plain progress bars |
| logging | `tracing` | 0.1.44 | structured events → `--log-events/--log-rates/--log-reads` equivalents |
| sidecar JSON | `serde` 1.0.229 + `serde_json` | 1.0.x (UNVERIFIED exact) | |
| errors | `thiserror` 2.0.21 / `anyhow` 1.0.104 | | |
| hotplug (pure Rust) | `netlink-sys` 0.9.0 **or** raw `libc` socket | | `neli` 0.7.4 is an alternative; the libudev-based `udev` crate (0.9.3) needs `libudev-dev` at build time (not installed on this box) — avoid |
| mmap for later stages (not the engine) | `memmap2` | 0.9.11 | never mmap the source device |
| Windows | `windows-sys` | 0.61.2 | `CreateFileW`, `ReadFile`/`OVERLAPPED`, `CancelIoEx`, `DeviceIoControl` |
| property tests | `proptest` | 1.x (UNVERIFIED) | map invariants |
| temp files in tests | `tempfile` | 3.x (UNVERIFIED) | |

Not recommended: `tokio`/`io-uring` for the read path (3.5); any crate that shells out
to `smartctl`/`ddrescue` (not available on the dev box, and the engine must own the
timeouts).

---

## 7. Sources

Primary (read during research):

- GNU ddrescue 1.28 source tarball: `https://ftpmirror.gnu.org/gnu/ddrescue/ddrescue-1.28.tar.lz`
  (files `doc/ddrescue.texi`, `rescuebook.cc`, `rescuebook.h`, `mapfile.cc`, `mapbook.cc`,
  `main.cc`, `main_common.cc`, `io.cc`, `block.h`, `testsuite/check.sh`). Manual online:
  `https://www.gnu.org/software/ddrescue/manual/ddrescue_manual.html` (rate-limited during research; the texinfo in the tarball is the same text).
- OpenSuperClone (HDDSuperClone) manual: `https://raw.githubusercontent.com/ISpillMyDrink/OpenSuperClone/master/doc/opensuperclone.texi`
  (Theory of Operation, Usage, Exit Codes, Progress Log Structure, Important Notes, WARNING).
- Linux kernel: `drivers/scsi/scsi_sysfs.c` (`timeout`, `eh_timeout`, `state`, `delete`, `rescan` attributes),
  `drivers/scsi/sd.c` and `sd.h` (`SD_TIMEOUT`, `SD_MAX_RETRIES`, `max_retries` attribute, `scsi_disk` class),
  `Documentation/scsi/scsi_eh.rst` (timeout → abort → reset escalation),
  `lib/kobject_uevent.c` (uevent format, group 1),
  `Documentation/driver-api/driver-model/driver.rst`,
  `Documentation/ABI/testing/sysfs-bus-usb` (`authorized`) — all via `https://raw.githubusercontent.com/torvalds/linux/master/...`.
- Local kernel UAPI headers (this machine): `/usr/include/linux/fs.h` (BLK* ioctls),
  `/usr/include/scsi/sg.h` (`sg_io_hdr`, `SG_IO`, timeout in ms), `/usr/include/scsi/scsi.h`
  (opcodes, sense keys), `/usr/include/linux/stat.h` (`STATX_DIOALIGN`),
  `/usr/include/linux/nvme_ioctl.h` (`NVME_IOCTL_ADMIN_CMD`, `timeout_ms`),
  `/usr/include/linux/netlink.h` (`NETLINK_KOBJECT_UEVENT`), `/usr/include/asm-generic/errno*.h`.
- man-pages 6.17 (local): `open(2)` O_DIRECT section, `read(2)` ERRORS.
- smartmontools: `include/smartmon/ata.h` (SMART command/feature/register values, `ata_smart_attribute`, `ata_smart_values`),
  `lib/scsiata.cpp` (SAT ATA PASS-THROUGH(16)/(12) CDB layout) — `https://raw.githubusercontent.com/smartmontools/smartmontools/main/...`.
- libnvme: `src/nvme/types.h` (`struct nvme_smart_log`, `enum nvme_smart_crit`) — `https://raw.githubusercontent.com/linux-nvme/libnvme/master/src/nvme/types.h`.
- udev: `/usr/lib/udev/rules.d/60-persistent-storage.rules` (local) for `/dev/disk/by-id` naming.
- Local sysfs inspection of `/sys/block/nvme0n1`, `/sys/class/nvme/nvme0` (attribute names, `state=live`, `max_sectors_kb=128`).
- Wikipedia, "Self-Monitoring, Analysis and Reporting Technology" (attributes 5/196/197/198/199 conventions):
  `https://en.wikipedia.org/wiki/Self-Monitoring,_Analysis_and_Reporting_Technology`
- crates.io API (`https://crates.io/api/v1/crates/<name>`) for crate versions, 2026-10-12.

Secondary (search results, not fully read):

- Red Hat, "Controlling the SCSI Command Timer and Device Status": `https://docs.redhat.com/en/documentation/red_hat_enterprise_linux/5/html/online_storage_reconfiguration_guide/task_controlling-scsi-command-timer-onlining-devices`
- Netdata guides on NVMe critical warning / read-only mode: `https://www.netdata.cloud/guides/smartctl-disk-monitoring/smartctl-nvme-read-only-mode/`
- R-Studio forum on multi-pass/runtime imaging: `https://forum.r-tt.com/viewtopic.php?p=11281`
- Rossmann Group, "hddsuperclone vs ddrescue": `https://rossmanngroup.com/technical-reference/hddsuperclone-vs-ddrescue`
- HDDGuru / FreeBSD forum threads on drives dropping off the bus and reset+rescan loops (anecdotal):
  `https://forums.freebsd.org/goto/post?id=709112`, `https://forum.hddguru.com/viewtopic.php?p=255321`
- man-pages patch discussion for `STATX_DIOALIGN`: `https://lore.kernel.org/all/YtrQ9cWwUkmOUe9r@magnolia/`
