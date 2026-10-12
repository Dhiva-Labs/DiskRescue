# DiskRescue spec: partition tables and filesystems (list existing + deleted files, read data)

Status: implementation spec, written 2026-10-12. Lines tagged `UNVERIFIED` could not be
confirmed against a primary source and must be fact-checked before relying on them.
Everything in section 5 marked "VERIFIED on dev box" was executed on the development
machine as an unprivileged user (uid 1000) on 2026-10-12 with the tool versions listed there.

All multi-byte integers are little-endian unless stated otherwise. Offsets are hex, sizes in
bytes. "Sector" means the logical sector size of the medium (512 or 4096), "LBA" is a sector
index from the start of the *device image*, and "partition-relative" offsets are from the
first byte of the partition (or byte 0 of the image when the filesystem starts at 0).

---

## 1. Overview and scope

### 1.1 Goal

Given a read-only block image (the output of the imaging engine, plus its ddrescue mapfile
describing which byte ranges are good / bad / untried), produce a tree of files with:

* a name and parent path (where recoverable),
* a status: `Live`, `Deleted`, `Orphan` (reachable from metadata but not from any directory),
* an ordered list of byte extents in the image that hold the file's data, each tagged with
  how trustworthy it is (`FromMetadata`, `AssumedContiguous`, `Guessed`),
* a logical size (and for exFAT/NTFS a "valid data length" smaller than the size),
* timestamps and attributes where available.

A separate module (the carver) handles unallocated space; this spec provides it with the
"free space bitmap" of each filesystem so it can prioritise unallocated clusters, and with the
list of clusters already claimed by recovered files so it can avoid double-recovery.

### 1.2 Scope per filesystem (v1)

| Filesystem | Live files | Deleted files with names | Notes |
|---|---|---|---|
| FAT12/16/32 | yes | yes (first byte of short name lost; LFN restores it) | assume contiguous after delete |
| exFAT | yes | yes (full name retained) | NoFatChain files are exact after delete |
| NTFS | yes | yes (MFT record retained until reused; name in $FILE_NAME) | compressed data: documented, may be skipped |
| ext2/3/4 | yes (main target: damaged fs) | names only (dir slack); extents are zeroed by the kernel on unlink | data recovery for deleted files via carving |

Out of scope for v1 (documented only): NTFS encryption (EFS), ext4 fscrypt, ext4 bigalloc,
exFAT TexFAT, Btrfs/XFS/APFS/HFS+. The parser must *detect* and report these, never crash.

### 1.3 Design principles

1. **Never trust a single copy.** Every boot sector / superblock has a documented backup
   (section 2.x "backups"). Validation rules decide which copy wins.
2. **Every read goes through the mapfile.** A metadata block that lies in any region whose
   ddrescue status is not `+` (finished) - i.e. `?` non-tried, `*` failed/non-trimmed, `/`
   failed/non-scraped or `-` bad sector (ddrescue manual, "Mapfile structure") - is reported
   as `Unreadable` and the parser continues with what it has
   (e.g. lists a directory partially). Never abort a volume because one directory cluster is bad.
3. **Bounds-check everything.** Cluster numbers, LCNs, block numbers, record offsets and
   lengths are checked against the volume geometry before use. A corrupt pointer yields a
   diagnostic and a skipped item, never a panic or an out-of-range read.
4. **Deleted != unreadable.** Treat every "not in use" structure with a valid signature as a
   candidate and report it with a confidence score instead of dropping it.
5. **No writes to the source.** All parsers take `&dyn ReadAt` (positional reads) only.

---

## 2. Exact on-disk details

### 2.1 Partition tables

#### 2.1.1 MBR (LBA 0)

| Offset | Size | Field |
|---|---|---|
| 0x000 | 440 | Bootstrap code (ignored) |
| 0x1B8 | 4 | Disk signature (Windows "NT disk signature"; may be 0) |
| 0x1BC | 2 | Copy-protect flag, 0x0000 or 0x5A5A |
| 0x1BE | 16 | Partition entry 0 |
| 0x1CE | 16 | Partition entry 1 |
| 0x1DE | 16 | Partition entry 2 |
| 0x1EE | 16 | Partition entry 3 |
| 0x1FE | 2 | Boot signature: bytes `55 AA` (u16 LE 0xAA55) |

16-byte partition entry:

| Offset | Size | Field |
|---|---|---|
| 0x00 | 1 | Status: 0x80 active/bootable, 0x00 inactive. Anything else (except 0x80..0xFF on some tools) -> treat entry as suspicious |
| 0x01 | 3 | CHS start: byte0 = head; byte1 bits 0-5 = sector (1-based), bits 6-7 = cylinder bits 8-9; byte2 = cylinder bits 0-7 |
| 0x04 | 1 | Partition type (see table) |
| 0x05 | 3 | CHS end (same packing) |
| 0x08 | 4 | LBA of first sector (u32) |
| 0x0C | 4 | Number of sectors (u32) |

An entry is *empty* when all 16 bytes are zero (in practice: type == 0 or sector count == 0).
CHS fields are ignored for anything larger than ~8 GB; use the LBA fields exclusively.

Partition types we care about:

| Type | Meaning | Action |
|---|---|---|
| 0x01 | FAT12 | probe FAT |
| 0x04 | FAT16 < 32 MiB | probe FAT |
| 0x06 | FAT16B | probe FAT |
| 0x0B | FAT32 (CHS) | probe FAT |
| 0x0C | FAT32 (LBA) | probe FAT |
| 0x0E | FAT16 (LBA) | probe FAT |
| 0x07 | NTFS, exFAT, HPFS (ambiguous!) | probe boot-sector signature (2.1.4) |
| 0x05 | Extended (CHS) | walk EBR chain |
| 0x0F | Extended (LBA) | walk EBR chain |
| 0x85 | Linux extended | walk EBR chain |
| 0x82 | Linux swap | skip |
| 0x83 | Linux native | probe ext2/3/4 (also XFS/Btrfs -> report only) |
| 0x8E | Linux LVM | report only (v1) |
| 0xFD | Linux RAID autodetect | report only |
| 0xAF | Apple HFS/HFS+ | report only |
| 0xEE | GPT protective MBR | parse GPT (2.1.3) |
| 0xEF | EFI system partition (FAT) | probe FAT |

Rule: the type byte is a *hint* only. Always run the signature probe of 2.1.4 on the partition
start sector; cameras and phones regularly write 0x0B/0x0C for exFAT and 0x07 for FAT32.

#### 2.1.2 Extended partitions / EBR chain

An extended partition (0x05/0x0F/0x85) spanning sectors [E, E+N) contains a linked list of
Extended Boot Records. Each EBR is a 512-byte sector with the same layout as an MBR
(signature 0xAA55 at 0x1FE) but only two meaningful entries:

* Entry 0: the logical partition. **Start LBA is relative to the EBR's own sector.**
* Entry 1: the next EBR. **Start LBA is relative to the start of the outer extended
  partition E** (not to this EBR). Type is 0x05 or 0x0F. All-zero = end of chain.
* Entries 2 and 3: must be zero (some tools abuse them; ignore).

```
walk_ebr(E, N):
    ebr_lba = E; seen = {}
    loop:
        if ebr_lba in seen or ebr_lba >= E+N: break        # loop / corruption guard
        seen.insert(ebr_lba)
        sec = read_sector(ebr_lba); if sec[0x1FE..] != 55 AA: break
        e0 = entry(sec, 0x1BE); e1 = entry(sec, 0x1CE)
        if e0.sectors != 0: emit(start = ebr_lba + e0.start_lba, len = e0.sectors, type = e0.type)
        if e1.sectors == 0: break
        ebr_lba = E + e1.start_lba
```
Cap the chain at 128 entries.

#### 2.1.3 GPT

Protective MBR at LBA 0: one entry of type 0xEE, start LBA 1, size = min(disk_sectors-1,
0xFFFFFFFF). Do not require the protective MBR to be present or correct: go straight to LBA 1
and also to the last LBA.

GPT header (LBA 1; backup copy at the last LBA of the device):

| Offset | Size | Field | Validation |
|---|---|---|---|
| 0x00 | 8 | Signature `45 46 49 20 50 41 52 54` ("EFI PART") | must match |
| 0x08 | 4 | Revision, 0x00010000 | expect 1.0 |
| 0x0C | 4 | HeaderSize | 92 (0x5C); accept 92..sector size |
| 0x10 | 4 | HeaderCRC32 | CRC-32 (IEEE 802.3, poly 0x04C11DB7 reflected 0xEDB88320, init 0xFFFFFFFF, final xor; same as zlib `crc32`) over HeaderSize bytes with this field zeroed |
| 0x14 | 4 | Reserved (0) | |
| 0x18 | 8 | MyLBA | must equal the LBA the header was read from |
| 0x20 | 8 | AlternateLBA | location of the other header |
| 0x28 | 8 | FirstUsableLBA | |
| 0x30 | 8 | LastUsableLBA | |
| 0x38 | 16 | DiskGUID | |
| 0x48 | 8 | PartitionEntryLBA | primary: usually 2; backup: usually last LBA - 32 (the 32-sector array for 128 x 128-byte entries on 512-byte sectors sits immediately before the backup header, i.e. at `total_sectors - 33`) |
| 0x50 | 4 | NumberOfPartitionEntries | usually 128 (the array must be at least 16384 bytes); cap at 1024 when scanning damaged media |
| 0x54 | 4 | SizeOfPartitionEntry | usually 128; UEFI 2.10 5.3.2 requires `128 * 2^n` (128, 256, 512, ...; confirmed via EDK2 UefiGpt.h / OSDev); accept any multiple of 8 >= 128 defensively (Wikipedia: "implementations must allow for other values") |
| 0x58 | 4 | PartitionEntryArrayCRC32 | CRC-32 over Number*Size bytes of the array |

Partition entry (SizeOfPartitionEntry bytes, first 128 defined):

| Offset | Size | Field |
|---|---|---|
| 0x00 | 16 | PartitionTypeGUID (all-zero = unused entry) |
| 0x10 | 16 | UniquePartitionGUID |
| 0x20 | 8 | StartingLBA |
| 0x28 | 8 | EndingLBA (**inclusive**) |
| 0x30 | 8 | Attributes: bit 0 platform required, bit 1 EFI firmware ignore (no block I/O protocol), bit 2 legacy BIOS bootable, bits 3-47 reserved, bits 48-63 type-specific (Microsoft basic data: bit 60 read-only, bit 61 shadow copy, bit 62 hidden, bit 63 no drive letter / no automount; verified against Wikipedia "GUID Partition Table", which mirrors UEFI 2.10 table 5-6 and Microsoft's documented attribute bits) |
| 0x38 | 72 | PartitionName, UTF-16LE, zero padded, up to 36 code units |

GUID byte order on disk: the first three fields are little-endian, the last two are
big-endian ("mixed endian"). `C12A7328-F81F-11D2-BA4B-00A0C93EC93B` is stored as
`28 73 2A C1 1F F8 D2 11 BA 4B 00 A0 C9 3E C9 3B`.

Type GUIDs (textual form):

| GUID | Meaning | Action |
|---|---|---|
| 00000000-0000-0000-0000-000000000000 | unused | skip |
| C12A7328-F81F-11D2-BA4B-00A0C93EC93B | EFI System Partition | probe FAT |
| EBD0A0A2-B9E5-4433-87C0-68B6B72699C7 | Microsoft basic data | probe NTFS/exFAT/FAT |
| 0FC63DAF-8483-4772-8E79-3D69D8477DE4 | Linux filesystem data | probe ext4 (and others) |
| 0657FD6D-A4AB-43C4-84E5-0933C84B4F4F | Linux swap | skip |
| E3C9E316-0B5C-4DB8-817D-F92DF00215AE | Microsoft reserved | skip |
| DE94BBA4-06D1-4D40-A16A-BFD50179D6AC | Windows recovery | probe NTFS |
| 933AC7E1-2EB4-4F13-B844-0E14E2AEF915 | Linux /home | probe ext4 |
| 4F68BCE3-E8CD-4DB1-96E7-FBCAF984B709 | Linux root (x86-64, discoverable partitions spec) | probe ext4 (and others) |
| E6D6D379-F507-44C2-A23C-238F2A3DF928 | Linux LVM | report only |
| 21686148-6449-6E6F-744E-656564454649 | BIOS boot | skip |
| 48465300-0000-11AA-AA11-00306543ECAC | Apple HFS+ | report only |
| 7C3457EF-0000-11AA-AA11-00306543ECAC | Apple APFS | report only |

All type GUIDs above were checked 2026-10-12 against Wikipedia "GUID Partition Table" (which
mirrors the UEFI spec and the vendor lists) and, for the Linux ones, the UAPI Discoverable
Partitions Specification (SD_GPT_HOME, SD_GPT_SWAP, SD_GPT_ROOT_X86_64, SD_GPT_LINUX_GENERIC,
SD_GPT_ESP). As with MBR, the type GUID is a hint; signature-probe every partition start.

Backup GPT: header at the last LBA of the device, its entry array immediately before it
(PartitionEntryLBA of the backup header; typically last LBA - 32 for 128 entries of 128 bytes
with 512-byte sectors). If the primary header fails validation, use the backup. If both fail
CRC but one has a sane signature and sane LBAs, use it with a warning. If the device was
imaged partially (mapfile shows the end untried) the backup header may simply be unread:
say so rather than "GPT corrupt".

4K-sector devices: LBA 1 is byte 4096, not 512. When the sector size is unknown (image file),
probe both: `EFI PART` at byte 512 or at byte 4096.

#### 2.1.4 No partition table: filesystem starts at byte 0 ("superfloppy")

USB sticks and SD cards formatted by cameras or Windows "format" frequently have no MBR.
Also, after a partition table is destroyed, the filesystem is still intact. Probe byte 0 (and
every partition start) in this order; the first match wins but all matches are reported:

| FS | Test (partition-relative offsets) |
|---|---|
| exFAT | bytes 3..11 == `45 58 46 41 54 20 20 20` ("EXFAT   ") AND bytes 11..64 all zero AND 0x1FE == 55 AA |
| NTFS | bytes 3..11 == `4E 54 46 53 20 20 20 20` ("NTFS    ") AND 0x1FE == 55 AA AND bytes-per-sector at 0x0B in {512,1024,2048,4096} |
| FAT | 0x1FE == 55 AA AND byte 0 in {0xEB, 0xE9} AND bytes-per-sector at 0x0B in {512,1024,2048,4096} AND sectors-per-cluster at 0x0D is a power of two in 1..128 AND reserved sectors at 0x0E != 0 AND number of FATs at 0x10 in {1,2}. Then run the full BPB validation of 2.2.3. (The "FAT12"/"FAT16"/"FAT32" strings at 0x36/0x52 are informational only and must not be used for type detection.) |
| ext2/3/4 | u16 at 0x438 == 0xEF53 (superblock at byte 1024, magic at superblock offset 0x38). Then validate per 2.5.3 |
| MBR/EBR | not exFAT/NTFS/FAT by the above, 0x1FE == 55 AA, and at least one partition entry at 0x1BE.. is sane (type != 0, sectors != 0, start < device size) |

Ambiguity: an MBR also ends in 55 AA and begins with a jump; the FAT test's BPB checks
(reserved sectors, FATs, cluster size) reject MBRs in practice, but a *FAT boot sector whose
BPB is zeroed by damage* can look like an MBR. In that case the backup boot sector (FAT32:
sector 6) decides.

### 2.2 FAT12 / FAT16 / FAT32

Primary source: Microsoft "FAT32 File System Specification" (fatgen103); layout cross-checked
with Wikipedia "Design of the FAT file system" and with images made by mkfs.fat 4.2.

#### 2.2.1 Boot sector / BPB (sector 0 of the volume)

| Offset | Size | Field (MS name) | Valid values |
|---|---|---|---|
| 0x00 | 3 | BS_jmpBoot | `EB xx 90` or `E9 xx xx` |
| 0x03 | 8 | BS_OEMName | any (mkfs.fat: "mkfs.fat") |
| 0x0B | 2 | BPB_BytsPerSec | 512, 1024, 2048, 4096 |
| 0x0D | 1 | BPB_SecPerClus | 1,2,4,8,16,32,64,128 |
| 0x0E | 2 | BPB_RsvdSecCnt | != 0 (FAT12/16: usually 1; FAT32: usually 32) |
| 0x10 | 1 | BPB_NumFATs | 1 or 2 |
| 0x11 | 2 | BPB_RootEntCnt | FAT12/16: != 0, RootEntCnt*32 is a multiple of BytsPerSec (512 typical). FAT32: must be 0 |
| 0x13 | 2 | BPB_TotSec16 | if 0, use TotSec32. FAT32: must be 0 |
| 0x15 | 1 | BPB_Media | 0xF8 fixed, 0xF0 removable; must equal low byte of FAT[0] |
| 0x16 | 2 | BPB_FATSz16 | FAT12/16: != 0. FAT32: must be 0 |
| 0x18 | 2 | BPB_SecPerTrk | ignore |
| 0x1A | 2 | BPB_NumHeads | ignore |
| 0x1C | 4 | BPB_HiddSec | sectors before the volume (should equal partition start LBA; often wrong, ignore) |
| 0x20 | 4 | BPB_TotSec32 | if TotSec16 == 0 this must be != 0 |

FAT12/16 extended BPB:

| Offset | Size | Field |
|---|---|---|
| 0x24 | 1 | BS_DrvNum (0x80 / 0x00) |
| 0x25 | 1 | BS_Reserved1 |
| 0x26 | 1 | BS_BootSig = 0x29 if the next three fields are present |
| 0x27 | 4 | BS_VolID (serial) |
| 0x2B | 11 | BS_VolLab (space padded; "NO NAME    ") |
| 0x36 | 8 | BS_FilSysType "FAT12   " / "FAT16   " / "FAT     " (informational only) |
| 0x3E | 448 | boot code |
| 0x1FE | 2 | 55 AA |

FAT32 extended BPB:

| Offset | Size | Field |
|---|---|---|
| 0x24 | 4 | BPB_FATSz32 (sectors per FAT) |
| 0x28 | 2 | BPB_ExtFlags: bits 0-3 = active FAT number if bit 7 set; bit 7 = 0 means FATs are mirrored (both valid) |
| 0x2A | 2 | BPB_FSVer, must be 0x0000 |
| 0x2C | 4 | BPB_RootClus (first cluster of root dir, usually 2) |
| 0x30 | 2 | BPB_FSInfo (sector of FSInfo, usually 1) |
| 0x32 | 2 | BPB_BkBootSec (backup boot sector, usually 6; 0 = none) |
| 0x34 | 12 | reserved |
| 0x40 | 1 | BS_DrvNum |
| 0x41 | 1 | BS_Reserved1 |
| 0x42 | 1 | BS_BootSig 0x29 |
| 0x43 | 4 | BS_VolID |
| 0x47 | 11 | BS_VolLab |
| 0x52 | 8 | BS_FilSysType "FAT32   " (informational only) |
| 0x5A | 420 | boot code |
| 0x1FE | 2 | 55 AA |

Observed mkfs.fat 4.2 defaults (dev box): 16 MiB -> FAT12, BytsPerSec 512, SecPerClus 16
(0x10 at 0x0D), RsvdSecCnt 16, 2 FATs, RootEntCnt 512; 40 MiB `-F 16` -> SecPerClus 4,
RsvdSecCnt 4; 64 MiB `-F 32` -> SecPerClus 1, RsvdSecCnt 32, FATSz32 0x3F1, RootClus 2,
FSInfo 1, BkBootSec 6.

FSInfo sector (FAT32 only, at sector BPB_FSInfo, backup copy at BkBootSec+1):

| Offset | Size | Field |
|---|---|---|
| 0x000 | 4 | FSI_LeadSig `52 52 61 41` ("RRaA", u32 0x41615252) |
| 0x1E4 | 4 | FSI_StrucSig `72 72 41 61` ("rrAa", u32 0x61417272) |
| 0x1E8 | 4 | FSI_Free_Count (0xFFFFFFFF = unknown; advisory only) |
| 0x1EC | 4 | FSI_Nxt_Free (advisory; hint where last allocation happened - useful for "newest files live here") |
| 0x1FC | 4 | FSI_TrailSig `00 00 55 AA` |

#### 2.2.2 Derived geometry (the Microsoft rule)

```
RootDirSectors = ((RootEntCnt * 32) + (BytsPerSec - 1)) / BytsPerSec      # 0 on FAT32
FATSz          = FATSz16 != 0 ? FATSz16 : FATSz32
TotSec         = TotSec16 != 0 ? TotSec16 : TotSec32
FirstDataSector= RsvdSecCnt + NumFATs * FATSz + RootDirSectors
DataSec        = TotSec - FirstDataSector
CountOfClusters= DataSec / SecPerClus                                      # integer division
type = CountOfClusters < 4085 ? FAT12 : CountOfClusters < 65525 ? FAT16 : FAT32
FirstSectorOfCluster(N) = FirstDataSector + (N - 2) * SecPerClus          # N >= 2
valid cluster numbers: 2 ..= CountOfClusters + 1
```
Cluster count, not the type string, not the partition type byte, decides the FAT width.
Volumes with CountOfClusters within a few units of 4085 or 65525 exist and are formatted
inconsistently by third-party tools; if the FAT type implied by the count disagrees with
`BS_FilSysType` and with `FATSz16 == 0`, try both widths and keep the one whose FAT[0]/FAT[1]
and root directory parse cleanly.

#### 2.2.3 Validating a candidate boot sector

All of: 0x1FE == 55 AA; jmp byte in {EB, E9}; BytsPerSec in {512,1024,2048,4096}; SecPerClus
power of two <= 128; BytsPerSec*SecPerClus <= 32 KiB (64 KiB tolerated with warning);
RsvdSecCnt != 0; NumFATs in {1,2}; Media >= 0xF0 or 0xF8..0xFF; TotSec != 0;
FATSz != 0; FirstDataSector < TotSec; CountOfClusters >= 1; for FAT32: RootEntCnt == 0,
FATSz16 == 0, TotSec16 == 0, FSVer == 0, 2 <= RootClus <= CountOfClusters+1; for FAT12/16:
RootEntCnt != 0. Soft checks (score, don't reject): FAT[0] low byte == Media; FAT[1] has the
top bits set; FAT copies identical; root dir first entry plausible; BS_BootSig == 0x29.

Backups: FAT32 keeps a copy of sectors 0-2 at sectors BkBootSec..BkBootSec+2 (6, 7, 8).
FAT12/16 have **no** backup boot sector; a second copy of the FAT (NumFATs == 2) is the only
redundancy. When sector 0 is unreadable or fails validation, on FAT32 read sector 6; on
FAT12/16 reconstruct the BPB heuristically (2.2.10).

#### 2.2.4 FAT entries

FAT N starts at sector `RsvdSecCnt + N*FATSz`. Entry for cluster C:

| Type | Entry location | Read | Free | Reserved | Bad | End-of-chain (EOC) |
|---|---|---|---|---|---|---|
| FAT12 | byte offset C + C/2 | u16 at that offset; if C is odd: `>> 4`, else `& 0x0FFF` | 0x000 | 0x001 | 0xFF7 | 0xFF8..0xFFF |
| FAT16 | byte offset C*2 | u16 | 0x0000 | 0x0001 | 0xFFF7 | 0xFFF8..0xFFFF |
| FAT32 | byte offset C*4 | u32 `& 0x0FFFFFFF` (top 4 bits reserved, ignore) | 0x0000000 | 0x0000001 | 0x0FFFFFF7 | 0x0FFFFFF8..0x0FFFFFFF |

A chain value `v` is "a next cluster" when `2 <= v <= CountOfClusters+1`; EOC when
`v >= Bad+1`; `Bad` ends the chain with a warning; `0`, `1` or `v > CountOfClusters+1` is
corruption (chain broken: stop, mark the file `Truncated`, and if the file has a known size,
continue with the "assume contiguous" rule for the remainder, flagged `Guessed`).

FAT[0] = 0x?F8 / 0xFFFFFF8 (media byte in low 8 bits), FAT[1] = EOC with dirty flags:
FAT16 bit 15 (FAT32 bit 27) clear = volume dirty (not cleanly unmounted), bit 14 (bit 26)
clear = I/O errors seen at last mount. Report these as "volume was dirty" evidence.

FAT12 note: the FAT12 entry for the last cluster may straddle the end of the FAT; bounds
check `C + C/2 + 1 < FATSz*BytsPerSec`.

Two FATs: read FAT0 unless ExtFlags (FAT32) says a different active FAT; when a FAT sector is
unreadable (mapfile) read the same sector from the other FAT; when the two FATs disagree for
a cluster, prefer the value that keeps the chain inside the volume, note the disagreement.

#### 2.2.5 Root directory

* FAT12/16: fixed region starting at sector `RsvdSecCnt + NumFATs*FATSz`, `RootDirSectors`
  sectors long (RootEntCnt entries).
* FAT32: cluster chain starting at BPB_RootClus. If RootClus is corrupt (0, 1, > max), try
  cluster 2, then scan for a cluster that starts with a volume-label entry (attr 0x08) or
  whose first entries pass the directory-entry heuristic of 2.2.9.

#### 2.2.6 32-byte short directory entry

| Offset | Size | Field |
|---|---|---|
| 0x00 | 11 | DIR_Name: 8 base + 3 ext, upper case, space padded (0x20). Byte 0 special: 0x00 = free and **all following entries are free too** (end of directory), 0xE5 = deleted, 0x05 = the real first byte is 0xE5 (Kanji), 0x2E = `.` / `..` |
| 0x0B | 1 | DIR_Attr: 0x01 READ_ONLY, 0x02 HIDDEN, 0x04 SYSTEM, 0x08 VOLUME_ID, 0x10 DIRECTORY, 0x20 ARCHIVE; 0x0F (= RO|HID|SYS|VOLID) = long-name entry |
| 0x0C | 1 | DIR_NTRes: bit 3 (0x08) base name is lower case, bit 4 (0x10) extension is lower case (Windows NT/Linux "shortname=mixed") |
| 0x0D | 1 | DIR_CrtTimeTenth: 0..199, 10 ms units added to creation time |
| 0x0E | 2 | DIR_CrtTime |
| 0x10 | 2 | DIR_CrtDate |
| 0x12 | 2 | DIR_LstAccDate |
| 0x14 | 2 | DIR_FstClusHI (FAT32 only; must be 0 on FAT12/16 - some drivers store EA index here) |
| 0x16 | 2 | DIR_WrtTime |
| 0x18 | 2 | DIR_WrtDate |
| 0x1A | 2 | DIR_FstClusLO |
| 0x1C | 4 | DIR_FileSize (0 for directories; directories have no size, walk the chain) |

Time: bits 15-11 hours, 10-5 minutes, 4-0 seconds/2. Date: bits 15-9 year-1980, 8-5 month
(1-12), 4-0 day (1-31). Local time, no zone. Validate ranges when scoring candidate entries.

First cluster = (FstClusHI << 16) | FstClusLO. 0 for empty files and for the `..` entry that
points at the root directory. A directory's `.` entry has first cluster = its own cluster,
`..` = parent's cluster (0 = root): use these to re-attach orphan directories (3.2.5).

Volume label: entry with attr 0x08 (and not 0x0F) in the root directory; name is the label.

#### 2.2.7 Long file name (LFN) entries

LFN entries precede their short entry, in **reverse** order (highest sequence first, so the
entry physically just before the short entry is sequence 1).

| Offset | Size | Field |
|---|---|---|
| 0x00 | 1 | LDIR_Ord: sequence 1..20 in bits 0-5; bit 6 (0x40) = last (first physically) entry; 0xE5 = deleted |
| 0x01 | 10 | LDIR_Name1: UCS-2 characters 1-5 |
| 0x0B | 1 | LDIR_Attr = 0x0F |
| 0x0C | 1 | LDIR_Type = 0 |
| 0x0D | 1 | LDIR_Chksum: checksum of the short name's 11 bytes |
| 0x0E | 12 | LDIR_Name2: characters 6-11 |
| 0x1A | 2 | LDIR_FstClusLO = 0 |
| 0x1C | 4 | LDIR_Name3: characters 12-13 |

Each entry holds 13 UCS-2 code units; the name is terminated by 0x0000 and padded with
0xFFFF. Max 255 characters = 20 entries. Checksum:

```
sum = 0
for b in short_name[0..11]:   # bytes as stored, including 0x05 if present, space padded
    sum = ((sum & 1) << 7) + (sum >> 1) + b     # u8 arithmetic, wrap
```
Accept an LFN only if every entry's checksum equals the following short entry's checksum,
the sequence numbers are contiguous and descending, and the 0x40 flag is on the first one.
When the LFN entries are deleted (0xE5) their sequence byte is lost; reassemble by physical
adjacency (consecutive 0x0F entries directly before a 0xE5 short entry whose checksum
matches) - checksum match is the key test.

Case: if there is no LFN, apply DIR_NTRes bits 0x08/0x10 to lower-case base/extension.

#### 2.2.8 Deleted files

What a delete does (Windows and Linux vfat): short entry byte 0 := 0xE5; each LFN entry's
byte 0 := 0xE5; the FAT chain is zeroed (all clusters marked free). Nothing else in the
32-byte entry is touched, so size, first cluster (low word), timestamps survive.
**FAT32 on Windows additionally zeroes DIR_FstClusHI** (confirmed by forensic references,
see Sources: forensicfocus thread, apriorit article). **Linux keeps the high word** (verified in
kernel source 2026-10-12): `vfat_unlink` (fs/fat/namei_vfat.c) calls `fat_remove_entries`
(fs/fat/dir.c), which writes only `de->name[0] = DELETED_FLAG` (0xE5) for each of the
`nr_slots` entries (LFN slots + short entry) and touches no other byte; it then `clear_nlink`s
and `fat_detach`es the inode (`i_pos = 0`), and `__fat_write_inode` (fs/fat/inode.c) returns
early when `i_pos == 0`, so the in-memory truncation done later by `fat_evict_inode` ->
`fat_truncate_blocks` (which frees the FAT chain to 0 via `fat_free_clusters`) is never written
back to the directory entry: size, FstClusHI/LO and timestamps all survive on Linux.

Consequences for recovery:

1. Name: the 0xE5 short entry has lost its first character; the LFN (if present and intact)
   has the full name. If there is no LFN, report the name as `?BCDEFGH.EXT` and, if the
   entry's checksum field in a neighbouring deleted LFN matches a candidate first letter,
   brute-force the first byte (256 candidates, pick printable upper-case letters first).
2. Data: the chain is gone. Assume the file was contiguous: clusters
   `first .. first + ceil(size / cluster_size) - 1`. Tag `AssumedContiguous`. Clip at
   CountOfClusters+1. Flag clusters in this range that the FAT now marks as *allocated* to a
   live file as `Overwritten` (per cluster) - the carver/validator can then decide whether
   the JPEG is salvageable.
3. FAT32 with FstClusHI == 0 and CountOfClusters > 65535: the true first cluster is
   `(h << 16) | lo` for some h in 0..=CountOfClusters>>16. Try each h (usually <= 60
   candidates on a 256 GB card with 32 KiB clusters, 2^16 clusters per h): the candidate whose
   first cluster starts with a known media signature (FF D8 FF, `ftyp`, RIFF, 89 50 4E 47,
   `FUJIFILM`/`II*\0`/`MM\0*`) and whose clusters are free wins. Tag `Guessed`.
4. Deleted directories: entry attr 0x10, byte 0 = 0xE5, FirstClus intact. Read the first
   cluster as a directory (its `.` entry must point at itself: strong validation) and recurse;
   the directory's own chain is gone so only its first cluster is reliable, continue into the
   next cluster only while it still parses as directory entries.

#### 2.2.9 Directory-looking cluster heuristic (orphan directory scan)

A cluster (or 512-byte sector for FAT12/16 root) is a directory if, for the first `k` 32-byte
entries (k = min(8, entries per cluster)): each entry's byte 0 is 0x00 (then stop) or 0xE5 or
a printable byte, attr has no bits outside 0x3F or equals 0x0F, LFN entries have
FstClusLO == 0 and Type == 0, short entries have reserved NTRes bits outside 0x18 clear,
dates in range (month 1-12, day 1-31, year < 2100 and not all zero for live files), first
cluster <= CountOfClusters+1, and (strong signal) the first two entries are `.` and `..`.
Score it; threshold at "at least two valid short entries or `.`+`..`".

#### 2.2.10 Reconstructing a destroyed BPB (FAT12/16, or FAT32 with sector 6 also gone)

1. Find FAT0: scan sectors 1..64 for a sector starting with `F8 FF FF` (FAT12/16,
   `F8 FF FF FF ...` / `F8 FF FF 0F FF FF FF 0F` FAT32). FAT1 is a byte-identical copy some
   sectors later: the distance is FATSz.
2. RsvdSecCnt = sector index of FAT0; NumFATs = 2 if a copy was found.
3. Root directory: the first sector after the FATs that passes 2.2.9 (FAT12/16), or the
   first cluster after the FATs whose first entry is the volume label (FAT32, RootClus = 2).
4. SecPerClus: find two files in the root with known sizes and first clusters; the cluster
   size is the one for which `FirstSectorOfCluster` lands on a sector with a media signature.
   Try 1..128; prefer the value Windows would pick for the volume size (fatgen103 table).
5. TotSec: from the partition size (or the device size for superfloppies).

### 2.3 exFAT

Primary source: Microsoft exFAT specification (learn.microsoft.com). Layout cross-checked
against images made by mkfs.exfat 1.3.2 (exfatprogs) and dump.exfat.

#### 2.3.1 Boot region

Main boot region: sectors 0-11 of the volume. Backup boot region: sectors 12-23, byte-identical
copy. Sector 0 = Main Boot Sector, sectors 1-8 Extended Boot Sectors (each ends in 55 AA at
the last 2 bytes of the sector), 9 OEM parameters, 10 reserved, 11 = Boot Checksum sector
(the 32-bit checksum repeated to fill the sector).

Main Boot Sector:

| Offset | Size | Field | Notes |
|---|---|---|---|
| 0x00 | 3 | JumpBoot | `EB 76 90` |
| 0x03 | 8 | FileSystemName | "EXFAT   " |
| 0x0B | 53 | MustBeZero | all zero (this is where a FAT BPB would be: exFAT deliberately zeroes it) |
| 0x40 | 8 | PartitionOffset | sector offset of the volume on the media (often 0; ignore) |
| 0x48 | 8 | VolumeLength | volume size in sectors |
| 0x50 | 4 | FatOffset | sector offset of the first FAT (>= 24) |
| 0x54 | 4 | FatLength | sectors per FAT |
| 0x58 | 4 | ClusterHeapOffset | sector offset of cluster 2 |
| 0x5C | 4 | ClusterCount | number of clusters (max 2^32 - 11) |
| 0x60 | 4 | FirstClusterOfRootDirectory | >= 2 |
| 0x64 | 4 | VolumeSerialNumber | |
| 0x68 | 2 | FileSystemRevision | 0x0100 = 1.00 (high byte major) |
| 0x6A | 2 | VolumeFlags | bit 0 ActiveFat (0 = first FAT/bitmap; 1 only on TexFAT), bit 1 VolumeDirty, bit 2 MediaFailure, bit 3 ClearToZero |
| 0x6C | 1 | BytesPerSectorShift | 9..12 (bytes per sector = 1 << n) |
| 0x6D | 1 | SectorsPerClusterShift | 0..25-BytesPerSectorShift (cluster bytes <= 32 MiB) |
| 0x6E | 1 | NumberOfFats | 1 (2 only for TexFAT) |
| 0x6F | 1 | DriveSelect | 0x80 |
| 0x70 | 1 | PercentInUse | 0..100 or 0xFF |
| 0x71 | 7 | Reserved | |
| 0x78 | 390 | BootCode | |
| 0x1FE | 2 | BootSignature | 55 AA |
| 0x200.. | | ExcessSpace (for sectors > 512) | |

Observed (mkfs.exfat 1.3.2, 64 MiB image): FatOffset 2048, FatLength 126,
ClusterHeapOffset 4096, ClusterCount 15872, root cluster 5, bitmap at cluster 2 (1984 bytes),
up-case table at cluster 3 (5836 bytes), 512-byte sectors, 8 sectors/cluster.

Boot checksum (sector 11, and sector 23 for the backup): over the first 11 sectors
(`BytesPerSector * 11` bytes), skipping byte indices 106, 107 (VolumeFlags) and 112
(PercentInUse):
```
cs = 0
for i in 0 .. 11*bps:
    if i in {106, 107, 112}: continue
    cs = ((cs & 1) ? 0x80000000 : 0) + (cs >> 1) + sector_bytes[i]     # u32 wrap
```
Sector 11 must contain `cs` as u32 LE repeated `bps/4` times.

Validation of a candidate boot sector: FileSystemName matches; MustBeZero all zero;
BytesPerSectorShift in 9..12; SectorsPerClusterShift + BytesPerSectorShift <= 25;
NumberOfFats in {1,2}; FatOffset >= 24; FatOffset + NumberOfFats*FatLength <= ClusterHeapOffset;
ClusterHeapOffset + (ClusterCount << SectorsPerClusterShift) <= VolumeLength;
FatLength * bps >= (ClusterCount + 2) * 4; root cluster in 2..ClusterCount+1; 55 AA.
Strong check: checksum sector matches. If the main region fails, try the backup at sector 12
(same tests, checksum at sector 23).

#### 2.3.2 FAT and cluster heap

FAT entry for cluster C: u32 at byte `FatOffset*bps + C*4`. Entry 0 = 0xFFFFFFF8 (media),
entry 1 = 0xFFFFFFFF. Values: 2..0xFFFFFFF6 next cluster; 0xFFFFFFF7 bad cluster;
0xFFFFFFFF end of chain; 0 = free *or* "not described by FAT" (NoFatChain files leave their
FAT entries at 0 - the FAT is **not** authoritative for allocation; the bitmap is).
Cluster C's bytes: `(ClusterHeapOffset + ((C-2) << SectorsPerClusterShift)) * bps` (note the
parentheses: shift the cluster index first, then add the heap offset in sectors),
cluster size `bps << SectorsPerClusterShift`.

#### 2.3.3 Directory entries (32 bytes)

Generic template: byte 0 EntryType; bits 0-4 TypeCode, bit 5 TypeImportance (0 critical,
1 benign), bit 6 TypeCategory (0 primary, 1 secondary), **bit 7 InUse**. 0x00 = end of
directory (nothing valid follows); 0x01-0x7F = unused entries (= deleted entries, InUse
cleared); 0x80 invalid; 0x81-0xFF in use. Primary entries at offset 20 (4) FirstCluster and
24 (8) DataLength when they allocate clusters.

| EntryType (in use) | Deleted form | Entry |
|---|---|---|
| 0x81 | 0x01 | Allocation Bitmap: 0x01 BitmapFlags (bit 0: 0 = first bitmap, 1 = second/TexFAT), 0x14 FirstCluster, 0x18 DataLength (bytes; = ceil(ClusterCount/8)) |
| 0x82 | 0x02 | Up-case Table: 0x04 TableChecksum (u32), 0x14 FirstCluster, 0x18 DataLength. Standard table checksum 0xE619D30D |
| 0x83 | 0x03 | Volume Label: 0x01 CharacterCount (0..11), 0x02 VolumeLabel UTF-16LE (22 bytes) |
| 0x85 | 0x05 | File (primary): see below |
| 0xC0 | 0x40 | Stream Extension (secondary, critical): see below |
| 0xC1 | 0x41 | File Name (secondary, critical): 0x01 GeneralSecondaryFlags (0), 0x02 FileName 15 UTF-16LE units |
| 0xA0 | 0x20 | Volume GUID (benign primary) - ignore. mkfs.exfat 1.3.2 writes this entry in the root with InUse already clear (0x20 observed right after the 0x83 label on the dev-box image), so a 0x20 entry in the root is normal, not evidence of deletion |
| 0xA1 | 0x21 | TexFAT Padding - ignore |
| 0xA2 | 0x22 | Windows CE Access Control Table (benign primary) - ignore |
| 0xE0 | 0x60 | Vendor Extension (benign secondary, TypeCode 0) - ignore |
| 0xE1 | 0x61 | Vendor Allocation (benign secondary, TypeCode 1; has FirstCluster/DataLength at 20/24) - ignore |
| 0xC2 | 0x42 | Windows CE Access Control List (critical secondary) - ignore |

(Type codes verified against Linux `fs/exfat/exfat_raw.h`: EXFAT_GUID 0xA0, EXFAT_PADDING 0xA1,
EXFAT_ACLTAB 0xA2, EXFAT_STREAM 0xC0, EXFAT_NAME 0xC1, EXFAT_ACL 0xC2, EXFAT_VENDOR_EXT 0xE0,
EXFAT_VENDOR_ALLOC 0xE1; `EXFAT_DELETE = ~0x80`, `IS_EXFAT_DELETED(x) = x < 0x80`.)

File directory entry (0x85):

| Offset | Size | Field |
|---|---|---|
| 0x00 | 1 | 0x85 |
| 0x01 | 1 | SecondaryCount (2..18: one Stream Extension + 1..17 File Name entries) |
| 0x02 | 2 | SetChecksum (over the whole entry set, see below) |
| 0x04 | 2 | FileAttributes: 0x01 ReadOnly, 0x02 Hidden, 0x04 System, 0x10 Directory, 0x20 Archive |
| 0x06 | 2 | Reserved1 |
| 0x08 | 4 | CreateTimestamp |
| 0x0C | 4 | LastModifiedTimestamp |
| 0x10 | 4 | LastAccessedTimestamp |
| 0x14 | 1 | Create10msIncrement (0..199) |
| 0x15 | 1 | LastModified10msIncrement |
| 0x16 | 1 | CreateUtcOffset (bit 7 = valid; bits 0-6 signed 15-minute units) |
| 0x17 | 1 | LastModifiedUtcOffset |
| 0x18 | 1 | LastAccessedUtcOffset |
| 0x19 | 7 | Reserved2 |

(The learn.microsoft.com page as summarised by our fetch placed FileAttributes/timestamps
2 bytes earlier and put FirstCluster/DataLength into the 0x85 entry; that is a summarisation
error - the 0x85 entry has **no** FirstCluster/DataLength, the table above matches the spec's
Table 26 and the mkfs.exfat/fsck.exfat-validated fixture of section 5.3.)

Timestamp (u32): bits 0-4 seconds/2, 5-10 minute, 11-15 hour, 16-20 day, 21-24 month,
25-31 year-1980. Add 10msIncrement*10 ms. If UtcOffset bit 7 set, the timestamp is local
time at that offset (sign-extend the 7-bit value, units of 15 minutes).

Stream Extension entry (0xC0):

| Offset | Size | Field |
|---|---|---|
| 0x00 | 1 | 0xC0 |
| 0x01 | 1 | GeneralSecondaryFlags: bit 0 AllocationPossible (1 for files/dirs), **bit 1 NoFatChain** (1 = data is contiguous from FirstCluster; FAT entries not maintained) |
| 0x02 | 1 | Reserved1 |
| 0x03 | 1 | NameLength (1..255 UTF-16 units) |
| 0x04 | 2 | NameHash (of the up-cased name) |
| 0x06 | 2 | Reserved2 |
| 0x08 | 8 | **ValidDataLength** (bytes actually written; <= DataLength) |
| 0x10 | 4 | Reserved3 |
| 0x14 | 4 | FirstCluster (0 if DataLength == 0) |
| 0x18 | 8 | DataLength (allocated/logical size; for directories: multiple of cluster size) |

Bytes beyond ValidDataLength and up to DataLength are *allocated but never written*: for a
video that was being recorded when the card failed, ValidDataLength is the honest size
(DataLength may be a preallocation of e.g. 4 GiB). Recover `ValidDataLength` bytes; offer
the remainder as "uninitialised tail" only on request.

Entry set checksum (u16) over all `(SecondaryCount+1)*32` bytes of the set in order, skipping
byte indices 2 and 3:
```
cs = 0
for i in 0 .. 32*(SecondaryCount+1):
    if i == 2 or i == 3: continue
    cs = ((cs & 1) ? 0x8000 : 0) + (cs >> 1) + bytes[i]       # u16 wrap
```
Name hash (u16) over the UTF-16LE bytes of the up-cased name (apply the volume's up-case
table; identity for code points not in the table):
```
h = 0
for b in upcased_name_utf16le_bytes: h = ((h & 1) ? 0x8000 : 0) + (h >> 1) + b
```
Up-case table format (data of the 0x82 entry): a sequence of u16; an entry 0xFFFF followed by
u16 `n` means "the next n code points map to themselves" (run-length identity), otherwise the
u16 is the up-case of the next code point in sequence. Validated by the fixture script in 5.3
(fsck.exfat verifies NameHash).

Deleted entries: the driver clears bit 7 of every entry in the set: 0x85 -> 0x05, 0xC0 -> 0x40,
0xC1 -> 0x41 (Linux: `exfat_remove_entries` applies `EXFAT_DELETE = ~0x80`), and clears the
file's bits in the allocation bitmap. **The Linux exfat driver does not touch the FAT on
delete** (verified 2026-10-12 in `fs/exfat/fatent.c` `exfat_free_cluster`: both the
NoFatChain branch and the FAT-chain branch only call `exfat_clear_bitmap` per cluster, never
`exfat_ent_set`), so on a Linux-deleted FAT-chained file the chain is still walkable. The
Microsoft spec (section 4.1) does not require FAT entries of freed clusters to be zeroed
either (it only defines the valid values for allocated clusters), so whether Windows zeroes the
chain of a FAT-chained file on delete remains UNVERIFIED for Windows: treat a non-zero FAT
entry at FirstCluster as a usable chain and a zero entry as "chain gone, assume contiguous".
**All other bytes are retained**: full name,
size, ValidDataLength, FirstCluster, NoFatChain flag, timestamps and the checksum (which still
verifies if you re-set bit 7 on each entry when computing it - a strong validation for
deleted sets). A deleted set is 0x05 followed by SecondaryCount entries 0x40, 0x41...; the
spec says unused entries have arbitrary content, so verify by checksum before trusting.

Recovery of a deleted file:
* `NoFatChain == 1`: data is exactly clusters `FirstCluster .. FirstCluster +
  ceil(DataLength/cluster) - 1`. Tag `FromMetadata` (exact, unless overwritten: check the
  bitmap, clusters now allocated to someone else are `Overwritten`).
* `NoFatChain == 0`: walk the FAT; if the chain is zeroed (entry 0 at FirstCluster) fall back
  to assume-contiguous (`AssumedContiguous`). In practice cameras write contiguous files.

Allocation bitmap (0x81 entry's data): bit i of byte j (LSB first) = cluster `2 + 8j + i`;
1 = allocated. Length = ceil(ClusterCount/8). Unallocated space for the carver = bits that
are 0. The bitmap itself is cluster-allocated (it occupies its own clusters, bits set).

Directory data: a directory is a file whose DataLength is a multiple of the cluster size;
its stream has AllocationPossible = 1 and may be NoFatChain. The root directory has no 0x85
entry: it is FirstClusterOfRootDirectory with a FAT chain (spec 3.1.13: "The root directory
shall always be described with a cluster chain in the active FAT, as if ... NoFatChain ...
equal to zero"; a fresh root is one cluster). Likewise the Allocation Bitmap (spec 7.1.3) and
Up-case Table (7.2.3) chains are "as the FAT describes" - walk the FAT for all three, never
assume NoFatChain for them. Directories have no `.`/`..` entries. Parent
links do not exist on exFAT: an orphan directory cluster cannot be re-attached by metadata,
only by the fact that something references its first cluster.

Backups: boot region at sector 12 (checksum at 23). The second bitmap/FAT exist only on TexFAT.
Root directory and bitmap have **no** backup; if the root cluster is unreadable, scan all
clusters for directory-looking content (an entry set whose checksum verifies = extremely
strong signal) - section 3.3.2.

### 2.4 NTFS

Primary sources: ntfs-3g `include/ntfs-3g/layout.h` (struct definitions and flag values) and
the linux-ntfs documentation (flatcap.github.io). Observed values from mkntfs 2022.10.3 images.

#### 2.4.1 Boot sector ($Boot, sector 0; backup = last sector of the partition)

| Offset | Size | Field | Notes |
|---|---|---|---|
| 0x00 | 3 | jump | `EB 52 90` |
| 0x03 | 8 | OEM ID | "NTFS    " |
| 0x0B | 2 | bytes per sector | 512..4096 |
| 0x0D | 1 | sectors per cluster | power of two <= 128 as u8 (1,2,4,...,128); values > 128 are **negative-encoded** (Windows 10 1709+ for clusters > 64 KiB, up to 2 MiB): sectors per cluster = `1 << (256 - value)` = `1 << (-(s8)value)`, e.g. 0xF8 = -8 -> 256 sectors (128 KiB), 0xF4 = -12 -> 4096 sectors (2 MiB). Verified 2026-10-12: ntfs-3g `libntfs-3g/bootsect.c` accepts the plain powers of two plus raw values 240..253 (0xF0..0xFD) and decodes `1 << (256 - sectors_per_cluster)`; Linux `fs/ntfs3/super.c` `true_sectors_per_clst` accepts `<= 0x80` as-is and `>= 0xF4` as `1U << (-(s8)v)` and rejects 0x81..0xF3. Accept the union (0xF0..0xFD) and require the decoded value to be a power of two with cluster bytes <= 2 MiB. mkntfs on this box wrote plain 8 |
| 0x0E | 2 | reserved sectors | 0 |
| 0x10 | 3 | zero | must be 0 |
| 0x13 | 2 | unused | 0 |
| 0x15 | 1 | media descriptor | 0xF8 |
| 0x16 | 2 | zero | 0 |
| 0x18 | 2 | sectors per track | ignore |
| 0x1A | 2 | heads | ignore |
| 0x1C | 4 | hidden sectors | ignore |
| 0x20 | 4 | unused | 0 |
| 0x24 | 4 | unused | 0x00800080 ("80 00 80 00") |
| 0x28 | 8 | total sectors | number of sectors in the *volume*; the backup boot sector lives in the sector **after** this count, i.e. the partition is total_sectors + 1 sectors |
| 0x30 | 8 | $MFT LCN | cluster of MFT record 0 |
| 0x38 | 8 | $MFTMirr LCN | cluster of the mirror (copy of the first 4 records; mkntfs puts it mid-volume) |
| 0x40 | 1 | clusters per MFT record (s8) | if positive: record size = value * cluster bytes; if negative: record size = 1 << (-value). Observed: 0xF6 = -10 -> 1024 bytes |
| 0x41 | 3 | padding | |
| 0x44 | 1 | clusters per index record (s8) | same encoding; observed 1 -> 4096 bytes with 4 KiB clusters |
| 0x45 | 3 | padding | |
| 0x48 | 8 | volume serial number | |
| 0x50 | 4 | checksum | unused (0) |
| 0x54 | 426 | boot code | |
| 0x1FE | 2 | 55 AA | |

Validation: OEM ID; bytes per sector in {512,1024,2048,4096}; sectors per cluster valid
(after decoding); reserved == 0; zero fields zero; total sectors * bps <= partition size + 1
sector (allow slack); MFT LCN and MFTMirr LCN < total clusters; record size in {1024..4096}
(mkntfs/Windows use 1024; 4096 is possible on 4Kn drives); index record size 4096 typical;
55 AA. Backup: read the partition's last sector (LBA_end) and apply the same tests; it is a
byte-identical copy. Windows NT 3.51 (NTFS 1.x) placed the backup in the logical middle of
the volume; NT 4 and later put it at the end (flatcap linux-ntfs, files/boot.html). The
lost-partition scan (3.1.1) should therefore also test `volume_start + (total_sectors/2)*bps`
for pre-NT4 volumes, which are practically extinct on recoverable media.

#### 2.4.2 Fixups (update sequence array) - MUST be applied to FILE and INDX records

Every multi-sector record (FILE, INDX, also $LogFile RSTR/RCRD) carries at header offset
0x04 `usa_ofs` (u16) and 0x06 `usa_count` (u16; = record_size/512 + 1). At `usa_ofs` lies the
Update Sequence Number (USN, u16) followed by `usa_count-1` saved words. On disk, the **last
two bytes of every 512-byte sector** of the record have been replaced by the USN; the
original bytes are in the array.

```
apply_fixups(rec: &mut [u8]) -> Result<(), Torn>:
    usa_ofs = u16(rec[4..]); n = u16(rec[6..])
    require usa_ofs + 2*n <= 512 and n == rec.len()/512 + 1          # else reject record
    usn = rec[usa_ofs..usa_ofs+2]
    for s in 1..n:
        at = s*512 - 2
        if rec[at..at+2] != usn: return Err(Torn{sector: s})        # multi-sector write was interrupted
        rec[at..at+2] = rec[usa_ofs + 2*s .. usa_ofs + 2*s + 2]
    Ok
```
A `Torn` record (USN mismatch) is the classic signature of a power loss / failing drive. Policy:
still parse the record (after replacing what can be replaced), but downgrade its confidence
and tag the sector(s) that mismatched; the attribute data inside those sectors may be stale.
Never apply fixups twice; never forget them (a run list whose last 2 bytes sit at offset
510 of a sector is garbage without the fixup).

#### 2.4.3 MFT FILE record header (layout.h `MFT_RECORD`)

| Offset | Size | Field | Notes |
|---|---|---|---|
| 0x00 | 4 | magic | "FILE" = 0x454C4946 LE. Also seen: "BAAD" (0x44414142: chkdsk marked it bad), "HOLE", all 0xFF / all 0 (never used) |
| 0x04 | 2 | usa_ofs | 0x30 on NTFS 3.1 (48 = sizeof header) |
| 0x06 | 2 | usa_count | 3 for 1 KiB records |
| 0x08 | 8 | lsn | $LogFile sequence number |
| 0x10 | 2 | sequence_number | ntfs-3g increments it when the record is **freed** (`ntfs_mft_record_free`: if 0xFFFF -> 1, else if != 0 -> +1); Linux ntfs3 increments it when the record is **reused** (`fs/ntfs3/record.c` `mi_format_new`: "Record is reused. Update its sequence number": seq = old + 1, 0 -> 1, verified 2026-10-12); forensic literature (Carrier, FSFA ch. 11) says Windows increments on deallocation. Windows timing UNVERIFIED against a primary source; the effect is the same either way: a reference whose sequence != the record's sequence is stale, and a freed record's own sequence is either old+1 (ntfs-3g/Windows) or still old (ntfs3) - so when matching a deleted record against a stale $FILE_NAME.parent or $I30 reference accept seq == ref_seq **or** seq == ref_seq + 1 |
| 0x12 | 2 | link_count | hard links = number of directory entries referencing the record. ntfs-3g (`libntfs-3g/dir.c` `ntfs_link_i`) increments it for every $FILE_NAME it adds, including the DOS 8.3 name added by `set_dos_name`, so a file with separate Win32 and DOS names has link_count 2; a single WIN32_AND_DOS name counts 1. Use it only as a plausibility hint, never as a loop bound |
| 0x14 | 2 | attrs_offset | first attribute; 8-byte aligned (0x38 on 3.1) |
| 0x16 | 2 | flags | 0x0001 MFT_RECORD_IN_USE, 0x0002 MFT_RECORD_IS_DIRECTORY, 0x0004 IN_EXTEND ($Extend member), 0x0008 IS_VIEW_INDEX |
| 0x18 | 4 | bytes_in_use | <= bytes_allocated |
| 0x1C | 4 | bytes_allocated | == MFT record size |
| 0x20 | 8 | base_mft_record | 0 for base records; else file reference of the base record (this is an *extension* record holding spilled attributes) |
| 0x28 | 2 | next_attr_instance | |
| 0x2A | 2 | reserved | (NTFS 3.1) |
| 0x2C | 4 | mft_record_number | (NTFS 3.1) self number; use to validate the record landed where expected |
| 0x30 | 2 | USN | then the update sequence array |

File reference (u64): bits 0-47 = MFT record number, bits 48-63 = sequence number.
`MREF(x) = x & 0x0000FFFFFFFFFFFF`, `MSEQNO(x) = x >> 48`.

System records: 0 $MFT, 1 $MFTMirr, 2 $LogFile, 3 $Volume, 4 $AttrDef, 5 `.` (root dir),
6 $Bitmap, 7 $Boot, 8 $BadClus, 9 $Secure, 10 $UpCase, 11 $Extend, 12-15 reserved
(in use, empty), 16+ user files. mkntfs put the first user file at 64 on the test image.

#### 2.4.4 Attribute header

Attributes follow each other from `attrs_offset`, each 8-byte aligned, terminated by
type 0xFFFFFFFF (AT_END). Common header:

| Offset | Size | Field |
|---|---|---|
| 0x00 | 4 | type (see list) |
| 0x04 | 4 | length (whole attribute incl. header, multiple of 8); must be >= 0x18 and <= remaining record bytes |
| 0x08 | 1 | non_resident flag (0 resident, 1 non-resident) |
| 0x09 | 1 | name_length (UTF-16 units) |
| 0x0A | 2 | name_offset (from attribute start) |
| 0x0C | 2 | flags: 0x0001 COMPRESSED (0x00FF compression mask), 0x4000 ENCRYPTED, 0x8000 SPARSE |
| 0x0E | 2 | instance (attribute id) |

Resident (non_resident == 0):

| Offset | Size | Field |
|---|---|---|
| 0x10 | 4 | value_length |
| 0x14 | 2 | value_offset (from attribute start) |
| 0x16 | 1 | resident flags: 0x01 indexed |
| 0x17 | 1 | reserved |
| value_offset | value_length | the value (for $DATA: the file content itself, max ~700 bytes in a 1 KiB record) |

Non-resident (non_resident == 1):

| Offset | Size | Field |
|---|---|---|
| 0x10 | 8 | lowest_vcn (0 for the first/only piece) |
| 0x18 | 8 | highest_vcn (last VCN covered by this piece; = allocated_size/cluster - 1) |
| 0x20 | 2 | mapping_pairs_offset (data runs, from attribute start) |
| 0x22 | 1 | compression_unit (log2 clusters; 4 => 16 clusters; 0 if not compressed) |
| 0x23 | 5 | reserved |
| 0x28 | 8 | allocated_size (bytes, multiple of cluster; only valid when lowest_vcn == 0) |
| 0x30 | 8 | data_size (real size, bytes) |
| 0x38 | 8 | initialized_size (bytes actually written; beyond this read as zero) |
| 0x40 | 8 | compressed_size (only if flags & (COMPRESSED|SPARSE)); data runs then start at 0x48 |

Attribute types: 0x10 $STANDARD_INFORMATION, 0x20 $ATTRIBUTE_LIST, 0x30 $FILE_NAME,
0x40 $OBJECT_ID, 0x50 $SECURITY_DESCRIPTOR, 0x60 $VOLUME_NAME, 0x70 $VOLUME_INFORMATION,
0x80 $DATA, 0x90 $INDEX_ROOT, 0xA0 $INDEX_ALLOCATION, 0xB0 $BITMAP, 0xC0 $REPARSE_POINT,
0xD0 $EA_INFORMATION, 0xE0 $EA, 0xF0 $PROPERTY_SET, 0x100 $LOGGED_UTILITY_STREAM,
0xFFFFFFFF end.

#### 2.4.5 Data runs (mapping pairs)

Byte stream at `mapping_pairs_offset`:
```
lcn = 0; vcn = lowest_vcn
loop:
    h = next byte; if h == 0: break
    len_size = h & 0x0F; off_size = h >> 4
    require 1 <= len_size <= 8 and off_size <= 8
    length = unsigned LE integer of len_size bytes          # in clusters, must be > 0
    if off_size == 0: run = Sparse(vcn, length)            # hole: reads as zeros
    else:
        delta = signed LE integer of off_size bytes (sign-extend from the top bit of the last byte)
        lcn += delta; require 0 <= lcn < total_clusters; run = Real(vcn, lcn, length)
    vcn += length
```
Examples (flatcap): `21 18 34 56 00` = 0x18 clusters at LCN 0x5634.
`11 30 60 21 10 00 01 11 20 E0 00` = 0x30 @0x60, 0x10 @0x160, 0x20 @0x140 (offset -0x20).
`11 30 20 01 60 11 10 30 00` = 0x30 @0x20, 0x60 sparse, 0x10 @0x50.
Observed $MFT:$DATA on the test image: `11 13 04 00` = 19 clusters at LCN 4.

Byte offset of file VCN v: find the run containing v; `lcn_of(v) * cluster_size + (v -
run.vcn) * cluster_size`. Read min(data_size - v*cluster_size, cluster_size) bytes. Beyond
`initialized_size` return zeros without reading.

#### 2.4.6 $STANDARD_INFORMATION (0x10, resident, 48 or 72 bytes)

0x00 creation time, 0x08 last data change, 0x10 last MFT change, 0x18 last access (all
u64 = 100 ns intervals since 1601-01-01 UTC; Unix seconds = (t - 116444736000000000)/10^7),
0x20 file attribute flags (0x01 RO, 0x02 hidden, 0x04 system, 0x20 archive, 0x80 normal,
0x100 temporary, 0x200 sparse, 0x400 reparse point, 0x800 compressed, 0x1000 offline,
0x2000 not indexed, 0x4000 encrypted), 0x24 max versions, 0x28 version, 0x2C class id;
NTFS 3.0+: 0x30 owner id, 0x34 security id, 0x38 quota charged (u64), 0x40 USN (u64).

#### 2.4.7 $FILE_NAME (0x30, resident; one per hard link / name space)

| Offset | Size | Field |
|---|---|---|
| 0x00 | 8 | parent directory file reference (record number + sequence) |
| 0x08 | 8 | creation time |
| 0x10 | 8 | modification time |
| 0x18 | 8 | MFT change time |
| 0x20 | 8 | access time |
| 0x28 | 8 | allocated size (of the unnamed $DATA; stale - updated only on rename) |
| 0x30 | 8 | real size (stale likewise; observed 0 for a 1100-byte file written by ntfscp) |
| 0x38 | 4 | flags (as $STANDARD_INFORMATION) + 0x10000000 directory ($I30 index present), 0x20000000 view index |
| 0x3C | 4 | reparse tag / EA size |
| 0x40 | 1 | name length in UTF-16 units |
| 0x41 | 1 | namespace: 0 POSIX, 1 Win32, 2 DOS (8.3), 3 Win32+DOS (names identical) |
| 0x42 | 2*len | name, UTF-16LE, not terminated |

A file with a long name has two $FILE_NAME attributes: namespace 1 (Win32, e.g.
"Holiday Photo.jpg") and namespace 2 (DOS, "HOLIDA~1.JPG"). Prefer namespace 1 or 3 over 2,
and 0 (POSIX; Linux ntfs-3g writes POSIX by default) is fine too. Never show the DOS name
when a Win32 one exists.

#### 2.4.8 $DATA (0x80)

The unnamed $DATA attribute (name_length == 0) is the file content. Named $DATA attributes
are alternate data streams (e.g. `Zone.Identifier`); report but do not treat as the file.
Resident $DATA: content is in the record. Non-resident: runs. For very fragmented files the
runs do not fit in one record: $DATA is split into several non-resident attribute pieces
(each with its own lowest_vcn/highest_vcn) living in extension records, located via
$ATTRIBUTE_LIST (2.4.9). Only the piece with lowest_vcn == 0 carries valid sizes.

Compressed $DATA (flags & 0x0001): compression_unit = 4 => units of 16 clusters. Each unit
in the run list is either (a) a run of exactly 16 clusters -> stored uncompressed; (b) a
sparse run of 16 -> all zeros; (c) a real run of L < 16 clusters followed by a sparse run of
16-L -> compressed: the L clusters hold LZNT1 data as a sequence of 4 KiB chunks, each with a
2-byte header (bits 0-11 = compressed chunk length - 3, bit 15 = chunk is compressed; header
0 = end), tokens in groups of 8 preceded by a flag byte (bit set = 2-byte back-reference,
clear = literal), back-reference = (offset-1, length-3) packed with a position-dependent
split (`lmask = 0xFFF, dshift = 12; for i = pos-1; i >= 0x10; i >>= 1: lmask >>= 1,
dshift--`). v1: detect, report `Compressed`, implement LZNT1 later (photos are rarely NTFS-
compressed because JPEG does not compress; the flag is set per file by users, rarely on
camera-written volumes).

Sparse $DATA (0x8000): holes read as zeros; nothing else special. Encrypted (0x4000): data is
EFS ciphertext - report, do not recover.

#### 2.4.9 $ATTRIBUTE_LIST (0x20) - large / fragmented / many-named files

Resident or non-resident; value is a sequence of entries, each 8-byte aligned:
0x00 type (u32), 0x04 entry length (u16), 0x06 name length (u8), 0x07 name offset (u8),
0x08 lowest VCN (u64; which piece of a split attribute), 0x10 file reference of the MFT
record holding the attribute (u64), 0x18 attribute id (u16), 0x1A name (UTF-16).
Walk: for every entry, load the referenced record (validating the sequence number and that
its base_mft_record references back to this base record), find the attribute by type/id,
and splice pieces by lowest VCN. $DATA pieces of a 200 GB video on a fragmented disk
routinely span 3-10 extension records.

#### 2.4.10 Directories: $INDEX_ROOT (0x90) and $INDEX_ALLOCATION (0xA0), name "$I30"

$INDEX_ROOT (resident, attribute name "$I30"):
0x00 indexed attribute type (0x30 = $FILE_NAME), 0x04 collation rule (1 = file name),
0x08 index record size in bytes (4096), 0x0C clusters per index record (or sectors when the
record is smaller than a cluster: same s8 encoding idea as the boot sector), then at 0x10 the
index node header: 0x10 entries offset (relative to 0x10), 0x14 index length (bytes used),
0x18 allocated length, 0x1C flags (0x01 = LARGE_INDEX: there is an $INDEX_ALLOCATION).
Entries follow at 0x10 + entries_offset.

Index entry:
0x00 file reference (u64) of the child, 0x08 entry length (u16), 0x0A key length (u16; size
of the $FILE_NAME body that follows), 0x0C flags (u16: 0x01 has sub-node, 0x02 last entry
in node), 0x0E reserved, 0x10 key = a $FILE_NAME attribute body (2.4.7, so the child's
name, parent ref, sizes, times are *all readable without touching the child's record*),
and when flag 0x01 is set, at `entry_length - 8` the VCN (u64) of the child index record.
The "last" entry (flag 0x02) has no key (key length 0) but may have a sub-node VCN.

$INDEX_ALLOCATION (non-resident, name "$I30"): runs over index records (INDX) of the size in
$INDEX_ROOT; $BITMAP (0xB0, name "$I30") says which index records are in use.

INDX record: 0x00 "INDX" (0x58444E49), 0x04 usa_ofs, 0x06 usa_count (**apply fixups**),
0x08 LSN, 0x10 VCN of this record, 0x18 node header (entries offset relative to 0x18, index
length, allocated length, flags 0x01 = not a leaf), 0x28 USN + array, then entries.

Listing a directory = B+tree in-order walk: for each entry in a node, if it has a sub-node,
first recurse into that VCN, then emit the entry. For recovery purposes order does not matter:
simply emit every entry of $INDEX_ROOT and every entry of every INDX record that is in use
*and also* of INDX records marked unused in $BITMAP (they often hold entries of deleted files;
"index slack" also holds deleted entries beyond `index length` inside used records:
UNVERIFIED as a general rule, well known from forensic tools; validate each slack entry by
sanity-checking the embedded $FILE_NAME).

#### 2.4.11 $Bitmap (record 6) and $MFT:$BITMAP

$Bitmap's unnamed $DATA: one bit per cluster, LSB first, bit set = allocated. Length =
ceil(total_clusters/8), padded to 8 bytes. Unallocated space for the carver = clear bits.
$MFT's $BITMAP attribute (0xB0 in record 0): one bit per MFT record, bit set = record in use.
A record whose bit is clear but whose header says IN_USE is inconsistent (chkdsk-style
damage): list it as `Deleted?` with lower confidence; a record with bit set but flags without
IN_USE: `Deleted` (seen when the free is partially written).

#### 2.4.12 Deleted files, orphans, $MFTMirr

Windows delete: clears flag 0x0001, clears the $MFT bitmap bit, (eventually) increments the
sequence number, removes the $I30 index entry in the parent, clears the file's clusters in
$Bitmap. The MFT record content (all attributes, including resident data and the run list)
is untouched until the record is reused. Hence:

* Deleted file = record with magic FILE, flags & 1 == 0, at least one $FILE_NAME, and a
  $DATA. Data via runs, tagged `FromMetadata` but each cluster cross-checked against $Bitmap:
  allocated-now clusters are `Overwritten` candidates.
* Name/path: $FILE_NAME.parent ref -> parent record (if parent is in use and its sequence
  matches, the path is reliable; if the sequence mismatches the parent record was reused:
  path is `<unknown>/name`, keep the name).
* Orphan = in-use record whose parent chain does not reach record 5 (root) with matching
  sequence numbers, or whose name is not found in the parent's $I30. Place under
  `/$Orphan/<record#>/name`.
* Reused record: a *new* file now occupies the record; the old file is gone from the MFT.
  Only carving (or $LogFile/$UsnJrnl analysis, out of scope) can help.

$MFTMirr (record 1; LCN from the boot sector): byte copy of the first 4 MFT records
(Windows: 4; some tools mirror more, size = $MFTMirr's $DATA length). When record 0 is
unreadable, read record 0 from the mirror to get $MFT's runs; then the rest of the MFT is
reachable.

#### 2.4.13 Walking the MFT without any directory structure (the recovery entry point)

```
boot = validate(sector 0) or validate(last sector)
record_size = decode(boot[0x40]); cluster = bps * spc
rec0 = read(boot.mft_lcn * cluster, record_size) or read($MFTMirr) ; apply_fixups
runs = data_runs(unnamed $DATA of rec0)       # the MFT's own location on disk, may be fragmented
mft_bitmap = $BITMAP of rec0 (resident or via runs)
n_records = data_size(rec0 $DATA) / record_size
for i in 0..n_records:
    rec = read_vcn(runs, i * record_size)       # through the run list, not linearly!
    if magic != FILE: continue (count BAAD/zero)
    apply_fixups (tolerate torn)
    if base_mft_record != 0: stash as extension record for later splicing; continue
    emit Candidate{ in_use: flags&1, dir: flags&2, seq, names: all $FILE_NAME, std_info,
                    data: unnamed $DATA (+ pieces via $ATTRIBUTE_LIST), index: $I30 if dir }
build tree: parent refs from $FILE_NAME; root = record 5
```
If $MFT's own run list is unreadable (record 0 and mirror both bad): scan the volume for
sectors starting with "FILE" whose usa_ofs == 0x30 and bytes_allocated == record_size and
mft_record_number is monotonic in 1 KiB steps - the MFT is almost always one or a few long
contiguous runs; rebuild the run list from where "FILE" records are found consecutively.

### 2.5 ext2 / ext3 / ext4

Primary source: kernel documentation "ext4 Data Structures and Algorithms"
(docs.kernel.org/filesystems/ext4) and fs/ext4 source. Observed values from mke2fs 1.47.2.

#### 2.5.1 Superblock (byte 1024 of the partition, 1024 bytes long)

Block size = `1024 << s_log_block_size` (1 KiB .. 64 KiB). Block 0 contains the boot area
+ the superblock when block size is 1 KiB (superblock = block 1, s_first_data_block = 1);
with >= 2 KiB blocks the superblock is inside block 0 at byte 1024 (s_first_data_block = 0).

| Offset | Size | Field | Notes |
|---|---|---|---|
| 0x00 | 4 | s_inodes_count | |
| 0x04 | 4 | s_blocks_count_lo | |
| 0x08 | 4 | s_r_blocks_count_lo | |
| 0x0C | 4 | s_free_blocks_count_lo | |
| 0x10 | 4 | s_free_inodes_count | |
| 0x14 | 4 | s_first_data_block | 1 for 1 KiB blocks, else 0 |
| 0x18 | 4 | s_log_block_size | block = 1024 << n; n <= 6 |
| 0x1C | 4 | s_log_cluster_size | bigalloc only |
| 0x20 | 4 | s_blocks_per_group | typically 8 * block_size (32768 for 4 KiB) |
| 0x24 | 4 | s_clusters_per_group | bigalloc |
| 0x28 | 4 | s_inodes_per_group | |
| 0x2C | 4 | s_mtime | |
| 0x30 | 4 | s_wtime | |
| 0x34 | 2 | s_mnt_count | |
| 0x36 | 2 | s_max_mnt_count | |
| 0x38 | 2 | **s_magic = 0xEF53** | bytes `53 EF` |
| 0x3A | 2 | s_state | 0x0001 cleanly unmounted, 0x0002 errors detected, 0x0004 orphans being recovered |
| 0x3C | 2 | s_errors | |
| 0x3E | 2 | s_minor_rev_level | |
| 0x40 | 4 | s_lastcheck | |
| 0x44 | 4 | s_checkinterval | |
| 0x48 | 4 | s_creator_os | 0 Linux |
| 0x4C | 4 | s_rev_level | 0 = original (fixed 128-byte inodes, first inode 11), 1 = dynamic |
| 0x50 | 2 | s_def_resuid | |
| 0x52 | 2 | s_def_resgid | |
| 0x54 | 4 | s_first_ino | first non-reserved inode (11); rev 0: assume 11 |
| 0x58 | 2 | s_inode_size | 128 (ext2/3), 256 (ext4 default); rev 0: 128 |
| 0x5A | 2 | s_block_group_nr | group number of *this* superblock copy (0 for primary; tells you which backup you found) |
| 0x5C | 4 | s_feature_compat | 0x4 has_journal, 0x20 dir_index, 0x200 sparse_super2, 0x1000 orphan_file (these never affect reading) |
| 0x60 | 4 | s_feature_incompat | 0x2 FILETYPE (dirent has file_type), 0x4 RECOVER (journal needs replay), 0x10 META_BG, 0x40 EXTENTS, 0x80 64BIT, 0x200 FLEX_BG, 0x400 EA_INODE, 0x2000 CSUM_SEED, 0x4000 LARGEDIR, 0x8000 INLINE_DATA, 0x10000 ENCRYPT, 0x20000 CASEFOLD |
| 0x64 | 4 | s_feature_ro_compat | 0x1 SPARSE_SUPER, 0x2 LARGE_FILE, 0x8 HUGE_FILE, 0x10 GDT_CSUM, 0x40 EXTRA_ISIZE, 0x200 BIGALLOC, 0x400 METADATA_CSUM |
| 0x68 | 16 | s_uuid | checksum seed input |
| 0x78 | 16 | s_volume_name | label |
| 0x88 | 64 | s_last_mounted | |
| 0xCE | 2 | s_reserved_gdt_blocks | blocks after the GDT reserved for growth |
| 0xD0 | 16 | s_journal_uuid | |
| 0xE0 | 4 | s_journal_inum | 8 |
| 0xE8 | 4 | s_last_orphan | head of orphan inode list (files deleted while open!) |
| 0xEC | 16 | s_hash_seed | |
| 0xFC | 1 | s_def_hash_version | |
| 0xFE | 2 | s_desc_size | group descriptor size when INCOMPAT_64BIT (64); else 32 |
| 0x108 | 4 | s_mkfs_time | |
| 0x150 | 4 | s_blocks_count_hi | 64BIT |
| 0x15C | 2 | s_min_extra_isize | |
| 0x174 | 1 | s_log_groups_per_flex | FLEX_BG: 2^n groups per flex group (observed 4 -> 16) |
| 0x175 | 1 | s_checksum_type | 1 = crc32c |
| 0x24C | 8 | s_backup_bgs[2] | SPARSE_SUPER2: the only two groups with backups |
| 0x270 | 4 | s_checksum_seed | CSUM_SEED: use instead of crc32c(uuid) |
| 0x3FC | 4 | s_checksum | METADATA_CSUM: crc32c of bytes 0..0x3FC |

Observed (mke2fs 1.47.2 -t ext4, 32 MiB): 4 KiB blocks, 32768 blocks/group, 8192
inodes/group, inode size 256, desc size 64, features incl. 64bit, extent, flex_bg (16),
metadata_csum, metadata_csum_seed, inline_data absent, first inode 11.

Validation of a candidate superblock (at byte 1024 of the partition, or at a backup location):
magic; s_log_block_size <= 6; s_inodes_per_group != 0 and <= 8*block_size (bitmap must fit one
block); s_blocks_per_group in 1..=8*block_size; s_inode_size in {128, 256, 512, 1024, ...,
block_size} and power of two; s_rev_level <= 1; s_first_data_block == (block_size == 1024 ? 1
: 0); s_desc_size in {32, 64} (or 0 when !64BIT); s_blocks_count <= partition size / block_size
(+ small slack); s_inodes_count == s_inodes_per_group * group_count; group_count =
ceil((blocks_count - first_data_block) / blocks_per_group). Strong check: METADATA_CSUM and
s_checksum matches (crc32c, see 3.5.1). Soft: s_wtime/s_mtime plausible.

Backups: with SPARSE_SUPER (always set by modern mke2fs), group g has a superblock + GDT
copy iff g == 0, g == 1, or g is a power of 3, 5 or 7: groups 0, 1, 3, 5, 7, 9, 25, 27, 49,
81, 125, 243, 343, 625, 729, 2187, 2401, 3125, ... Without SPARSE_SUPER every group has one.
Location: the first block of group g = `first_data_block + g * blocks_per_group`; the
superblock copy is at byte offset 0 of that block for block sizes >= 2 KiB (because it is a
whole-block copy; the primary alone sits at byte 1024), and at that block (which *is* byte
1024 + g*blocks_per_group*1024) for 1 KiB blocks. Practical: 4 KiB blocks -> byte
134,217,728 (group 1), 402,653,184 (group 3), ...; 1 KiB blocks -> byte 8,389,632 (group 1).
The copy's s_block_group_nr tells which group it is (use it to compute the partition start
when the partition table is lost: `partition_start = found_offset - g*blocks_per_group*
block_size - (block_size == 1024 ? 1024 : 0)`). The GDT copy follows the superblock copy.
SPARSE_SUPER2: only the two groups in s_backup_bgs.

#### 2.5.2 Block group descriptors

The group descriptor table (GDT) starts in the block after the superblock: block 1 for
block sizes >= 2 KiB, block 2 for 1 KiB blocks (block `first_data_block + 1` in general).
`group_count` descriptors of `s_desc_size` bytes (32 unless INCOMPAT_64BIT).

| Offset | Size | Field |
|---|---|---|
| 0x00 | 4 | bg_block_bitmap_lo |
| 0x04 | 4 | bg_inode_bitmap_lo |
| 0x08 | 4 | bg_inode_table_lo |
| 0x0C | 2 | bg_free_blocks_count_lo |
| 0x0E | 2 | bg_free_inodes_count_lo |
| 0x10 | 2 | bg_used_dirs_count_lo |
| 0x12 | 2 | bg_flags: 0x1 INODE_UNINIT (inode bitmap/table not initialised: no inodes allocated here, table may be garbage), 0x2 BLOCK_UNINIT (block bitmap not initialised: compute from layout), 0x4 INODE_ZEROED (inode table has been zeroed) |
| 0x14 | 4 | bg_exclude_bitmap_lo |
| 0x18 | 2 | bg_block_bitmap_csum_lo |
| 0x1A | 2 | bg_inode_bitmap_csum_lo |
| 0x1C | 2 | bg_itable_unused_lo (inodes at the end of the table never used) |
| 0x1E | 2 | bg_checksum: GDT_CSUM: crc16(~0, uuid, group# le32, descriptor[0..0x1E]) (and bytes 0x20..desc_size when INCOMPAT_64BIT and desc_size > 0x20); METADATA_CSUM: crc32c(seed, group# le32, descriptor with this field zeroed) & 0xFFFF. Verified 2026-10-12 against `fs/ext4/super.c` `ext4_group_desc_csum` and by recomputing dumpe2fs's "csum 0x..." in Python on two dev-box images (gdt_csum image: 0x9285; metadata_csum image: 0xe10d) - see 3.5.1 |
| 0x20 | 4 | bg_block_bitmap_hi (64-byte descriptors only) |
| 0x24 | 4 | bg_inode_bitmap_hi |
| 0x28 | 4 | bg_inode_table_hi |
| 0x2C | 2 | bg_free_blocks_count_hi |
| 0x2E | 2 | bg_free_inodes_count_hi |
| 0x30 | 2 | bg_used_dirs_count_hi |
| 0x32 | 2 | bg_itable_unused_hi |
| 0x34 | 4 | bg_exclude_bitmap_hi |
| 0x38 | 2 | bg_block_bitmap_csum_hi |
| 0x3A | 2 | bg_inode_bitmap_csum_hi |
| 0x3C | 4 | reserved |

Inode table of group g: `bg_inode_table` (lo | hi<<32) block, length
`inodes_per_group * inode_size / block_size` blocks. With FLEX_BG the bitmaps and tables of
16 (2^s_log_groups_per_flex) groups sit consecutively inside the first group of the flex
group; the descriptors still carry the real block numbers, so a reader needs no special
handling. Observed: group 0 inode table at block 37 (after sb, GDT, reserved GDT, 16 block
bitmaps, 16 inode bitmaps).

#### 2.5.3 Inode

Inode number N (1-based; 0 does not exist): `group = (N-1) / inodes_per_group`,
`index = (N-1) % inodes_per_group`, byte = `inode_table(group) * block_size + index *
inode_size`. Reserved: 1 bad blocks, 2 root, 7 reserved GDT, 8 journal, 11 lost+found (by
convention, not reserved), first regular = s_first_ino.

| Offset | Size | Field | Notes |
|---|---|---|---|
| 0x00 | 2 | i_mode | type in bits 12-15: 0x8000 regular, 0x4000 dir, 0xA000 symlink, 0x1000 fifo, 0x2000 chr, 0x6000 blk, 0xC000 sock; low 12 bits perms |
| 0x02 | 2 | i_uid (lo) | |
| 0x04 | 4 | i_size_lo | |
| 0x08 | 4 | i_atime | |
| 0x0C | 4 | i_ctime | |
| 0x10 | 4 | i_mtime | |
| 0x14 | 4 | **i_dtime** | deletion time; != 0 => deleted |
| 0x18 | 2 | i_gid (lo) | |
| 0x1A | 2 | i_links_count | 0 => deleted (or being deleted; orphan list) |
| 0x1C | 4 | i_blocks_lo | 512-byte units (unless HUGE_FILE + EXT4_HUGE_FILE_FL 0x40000: fs blocks) |
| 0x20 | 4 | i_flags | 0x1000 INDEX (htree dir), 0x40000 HUGE_FILE, **0x80000 EXTENTS**, 0x200000 EA_INODE, 0x10000000 INLINE_DATA, 0x800 ENCRYPT |
| 0x24 | 4 | i_osd1 (l_i_version) | |
| 0x28 | 60 | **i_block[15]** | block map, extent tree root, inline data, or symlink target |
| 0x64 | 4 | i_generation | checksum input |
| 0x68 | 4 | i_file_acl_lo | xattr block |
| 0x6C | 4 | i_size_high | size = lo | hi<<32 (ext2 rev0 dirs: must be 0) |
| 0x70 | 4 | i_obso_faddr | |
| 0x74 | 2 | l_i_blocks_high | |
| 0x76 | 2 | l_i_file_acl_high | |
| 0x78 | 2 | l_i_uid_high | |
| 0x7A | 2 | l_i_gid_high | |
| 0x7C | 2 | l_i_checksum_lo | |
| 0x7E | 2 | reserved | |
| 0x80 | 2 | i_extra_isize | bytes valid beyond 128 (32 typical); fields below exist only if within it |
| 0x82 | 2 | i_checksum_hi | |
| 0x84 | 4 | i_ctime_extra | bits 0-1 epoch extension, bits 2-31 nanoseconds |
| 0x88 | 4 | i_mtime_extra | |
| 0x8C | 4 | i_atime_extra | |
| 0x90 | 4 | i_crtime | creation time |
| 0x94 | 4 | i_crtime_extra | |
| 0x98 | 4 | i_version_hi | |
| 0x9C | 4 | i_projid | |
| 0x80+i_extra_isize | .. | in-inode xattrs | magic 0xEA020000 then ext4_xattr_entry list (holds `system.data` for inline files) |

Inode checksum (METADATA_CSUM): crc32c(seed, inode# le32, i_generation le32, inode bytes with
l_i_checksum_lo and i_checksum_hi zeroed); lo 16 bits at 0x7C, hi 16 bits at 0x82 (hi only if
i_extra_isize >= 4). Compare with debugfs `stat` "Inode checksum:" to validate the
implementation (fixture 5.5: inode 13 live = 0xb5f80595, after rm = 0x109ae230).

#### 2.5.4 Extent tree (i_flags & 0x80000)

i_block holds the root node: a 12-byte header + up to 4 entries (eh_max = 4).

ext4_extent_header: 0x00 eh_magic u16 = **0xF30A** (bytes `0A F3`), 0x02 eh_entries,
0x04 eh_max, 0x06 eh_depth (0 = leaf; max 5), 0x08 eh_generation u32.
ext4_extent_idx (depth > 0): 0x00 ei_block u32 (first logical block covered), 0x04
ei_leaf_lo u32, 0x08 ei_leaf_hi u16, 0x0A unused. Child node = one fs block at that number,
same header layout, eh_max = (block_size - 12) / 12 for every block size, **with or without
METADATA_CSUM** (kernel `ext4_ext_space_block`: `(blocksize - sizeof(header)) / sizeof(extent)`,
no adjustment for the tail; because every power-of-two block size leaves `block_size mod 12`
= 4 or 8 bytes of slack, the 4-byte ext4_extent_tail crc32c always fits at
`12 + 12*eh_max`). Readers must take eh_max from the header anyway and only check
`eh_max <= (block_size - 12) / 12`.
ext4_extent (depth == 0): 0x00 ee_block u32 (logical), 0x04 ee_len u16 (**> 32768 means
uninitialised/preallocated: real length = ee_len - 32768, contents must be treated as zeros,
but on a failing device after a crash they may contain the real data - offer both**),
0x06 ee_start_hi u16, 0x08 ee_start_lo u32 (physical block = lo | hi<<32).

Walk: validate magic, eh_entries <= eh_max, eh_depth <= 5; recurse through index nodes
(bounds-check leaf block < blocks_count, detect cycles with a visited set); collect leaf
extents sorted by ee_block; gaps between extents are holes (zeros). Logical byte range of
extent = [ee_block*bs, (ee_block+len)*bs) clipped to i_size.

#### 2.5.5 Block map (no EXTENTS flag: ext2/ext3 and ext4 files created without extents)

i_block[0..11] = direct block numbers (u32 each) of logical blocks 0..11; i_block[12] =
indirect block (a block of block_size/4 u32 pointers to logical blocks 12..12+N-1, N =
block_size/4); i_block[13] = double indirect; i_block[14] = triple indirect. A 0 pointer =
hole. Validate every pointer < blocks_count and not 0 when the file size requires it;
abort the map (file `Truncated`) on the first wild pointer. Also i_blocks (512-byte units)
counts indirect blocks.

#### 2.5.6 Inline data (INCOMPAT_INLINE_DATA, i_flags & 0x10000000)

File content: first 60 bytes in i_block; the rest in the in-inode extended attribute
`system.data` (name index 7, name "data"). Inline directories: i_block[0] (4 bytes) = parent
inode number, followed by dirents (without `.`/`..`), continuing in system.data.

#### 2.5.7 Symlinks

i_mode & 0xF000 == 0xA000. If the target is < 60 bytes and the inode has no blocks
(i_blocks == 0, or EA block only) the target string is in i_block (not NUL-terminated;
length = i_size). Otherwise it is in data block 0 (extent or block map).

#### 2.5.8 Directory entries

A directory's data (via extents/block map) is a sequence of blocks; each block is a sequence
of entries that never cross block boundaries.

ext4_dir_entry_2 (when INCOMPAT_FILETYPE, i.e. practically always):
0x00 inode u32 (0 = unused entry), 0x04 rec_len u16 (distance to the next entry; multiple of
4; the last entry's rec_len reaches the end of the block). Decoding (kernel `ext4.h`
`ext4_rec_len_from_disk`, verified 2026-10-12): `if raw == 65535 or raw == 0: len =
block_size; else len = (raw & 65532) | ((raw & 3) << 16)` - i.e. for block sizes < 64 KiB the
raw value is the length (its low 2 bits are always 0), and a whole-block entry on a 64 KiB
block is stored as 65535 (65536 on larger-than-64 KiB "blocks" stores 0; the low two bits
carry bits 16-17 of the length). The encoder (`ext4_rec_len_to_disk`) never emits raw values
with the low bits set for block sizes <= 64 KiB, so treat `raw & 3 != 0` as corruption there.
0x06 name_len u8, 0x07 file_type u8
(0 unknown, 1 regular, 2 dir, 3 chr, 4 blk, 5 fifo, 6 sock, 7 symlink, 0xDE = checksum tail),
0x08 name (name_len bytes, not terminated).
Old ext4_dir_entry (no FILETYPE): name_len is a u16 at 0x06.
Checksum tail (METADATA_CSUM): a fake 12-byte entry at the end of each block: inode 0,
rec_len 12, name_len 0, file_type 0xDE, then crc32c u32.

Hash-tree directories (i_flags & 0x1000): block 0 is a dx_root: `.` entry (rec_len 12),
`..` entry whose rec_len spans the rest of the block and hides the dx_root_info (0x18:
reserved u32, 0x1C hash_version u8, 0x1D info_length 8, 0x1E indirect_levels u8, 0x1F
flags) and the dx_entry array (0x20 limit u16, 0x22 count u16, 0x24 block u32, then
(hash u32, block u32) pairs). Interior dx_node blocks start with a fake empty dirent (inode 0,
rec_len = block size). **A linear scan of all blocks that simply follows rec_len and skips
inode == 0 entries lists every file in an htree directory**; use that (no hashing needed).

Deleted directory entries: unlink does **not** erase the entry; it extends the *previous*
entry's rec_len to cover it (or, for the first entry in a block, sets inode = 0). The bytes
(inode number, name_len, file_type, name) remain until the slot is reused. Recovery of
*names*: inside each live entry, after `8 + round_up(name_len, 4)` bytes and before the next
entry (`rec_len`), look for plausible hidden entries: inode in 1..inodes_count, name_len > 0,
8 + name_len <= remaining, file_type <= 7, name printable; chain them by their own rec_len.
This gives name -> inode number for deleted files (`Deleted` names); whether the inode still
has data is decided in 2.5.9.

#### 2.5.9 Deleted inode detection and the extent-zeroing fact

A deleted inode has i_links_count == 0 and i_dtime != 0 and its bit clear in the inode
bitmap. The kernel's `ext4_evict_inode` (fs/ext4/inode.c) sets i_size = 0, calls
`ext4_truncate` *before* setting i_dtime and freeing the inode; `ext4_ext_rm_leaf`
(fs/ext4/extents.c) zeroes each removed extent (`ext4_ext_store_pblock(ex, 0); ee_len = 0`,
then memmove/memset the tail) and `ext4_ext_remove_space` leaves the root header with
eh_entries = 0, eh_depth = 0. Index blocks are freed, not zeroed (their contents survive in
free space - a carver for ext4_extent_header magic 0xF30A in unallocated blocks can
reconstruct old leaf nodes, future work). Block-mapped files: the ext4 driver (which also
serves ext3, and ext2 when CONFIG_EXT4_USE_FOR_EXT2 is set - the default on most distros)
zeroes the block pointers on truncate and writes the zeroed inode. The **native ext2 driver
keeps them on disk** (verified 2026-10-12 in `fs/ext2/inode.c` `ext2_evict_inode`): it sets
`i_dtime`, calls `mark_inode_dirty` + `sync_inode_metadata` (writing the inode with dtime set
and i_size/i_block **intact**), and only then does `i_size = 0; ext2_truncate_blocks(inode,
0)`, which zeroes `i_block` in memory (`ext2_free_data`/`ext2_free_branches` do `*p = 0`) but
is never written back because the inode is I_FREEING; `ext2_free_inode` only clears the bitmap
bit. So on a volume last written by the native ext2 driver (embedded devices, old kernels)
a deleted inode has dtime != 0, links 0 and a complete block map - recover it. Indirect
blocks are freed but not zeroed in either driver.

Therefore on ext4: deleted inode = name + size (i_size is 0 after kernel delete! only
timestamps survive; the block count is 0) and **no block list**. Report `Deleted` entries
with name, inode number, dtime, and hand the carver the directory's name list for
"name-by-content-time" matching (a JPEG carved with EXIF DateTimeOriginal within seconds of
the dirent's crtime/mtime ... is a heuristic match, mark `Guessed`).
Exception: images produced with debugfs `rm` (our fixtures) keep extents and i_size; the
parser must still *try* the extent tree of a deleted inode (eh_entries > 0 and sane) and
recover it when present (real-world sources: ext3 with data in journal, fs images modified by
other tools, inodes whose unlink crashed half-way).

Orphan list: s_last_orphan -> inode; each orphan inode's i_dtime field holds the *next*
orphan inode number (overloaded) while the fs is dirty. Files deleted while open at the time
of the crash are on this list with their extents intact - prime recovery candidates.

#### 2.5.10 Unallocated space

Block bitmap of group g (bg_block_bitmap, one block, bit i LSB-first = block
`first_data_block + g*blocks_per_group + i`; 1 = in use). With BLOCK_UNINIT the bitmap block
is not valid: treat the group as "all free except the group's own metadata blocks". Bits
beyond blocks_count in the last group are set to 1 (padding). Unallocated = clear bits, minus
the journal (inode 8: its extents contain copies of recently written metadata/data blocks -
worth carving separately), minus blocks of recovered files.

#### 2.5.11 ext2/ext3 compatibility

ext2/ext3 have the same superblock/descriptor/inode/dirent layouts without the ext4
features: no EXTENTS (use the block map), 32-byte descriptors, 128-byte inodes (no
i_extra_isize: do not read beyond 128 bytes when s_inode_size == 128), rev 0 has a fixed
first inode 11 and inode size 128, no checksums, directories without file_type when
!INCOMPAT_FILETYPE. ext3 = ext2 + has_journal. Reading code: branch on feature flags, never
on the "ext2/ext3/ext4" name.

---

## 3. Algorithms (pseudocode)

Common types used below:

```
enum Trust { FromMetadata, AssumedContiguous, Guessed }
struct Extent { logical: u64, physical: u64, len: u64, trust: Trust, overwritten: bool, unreadable: bool }
enum Status { Live, Deleted, Orphan }
struct Entry { path: Vec<String>, name: String, status: Status, size: u64, valid_size: u64,
               extents: Vec<Extent>, times: Times, attrs: Attrs, confidence: u8, notes: Vec<String> }
trait ReadAt { fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<Readability> }  // mapfile-aware
```
`read_at` returns `Readability::{Good, Bad(ranges), Untried(ranges)}`; callers decide. A
helper `read_exact_or_zero` fills unreadable bytes with zeros and sets `unreadable = true` on
the resulting extent/structure.

### 3.1 Partition discovery

```
discover(img) -> Vec<Volume>:
    vols = []
    for sector_size in [512, 4096]:                     # unknown for image files; both tried
        # 1. GPT first (protective MBR may be damaged)
        for lba in [1, last_lba(sector_size)]:
            if let Some(h) = parse_gpt_header(img, lba, sector_size):
                for e in entries(h): if e.type != 0: vols.push(Volume{start: e.first*ss, len: (e.last-e.first+1)*ss, hint: Gpt(e.type)})
                if vols nonempty: break both loops
    if vols.empty:
        # 2. MBR
        if let Some(mbr) = parse_mbr(img):
            for p in mbr.entries: if p.type in EXTENDED: vols.extend(walk_ebr(..)) else if p.sane(): vols.push(..)
    # 3. always probe byte 0 as a filesystem (superfloppy), even when a table was found
    if let Some(fs) = probe_fs(img, 0): vols.push(Volume{start: 0, len: img.len, hint: Superfloppy(fs)})
    # 4. signature-probe every volume start; demote to Unknown if nothing matches
    for v in vols: v.fs = probe_fs(img, v.start)
    # 5. optional: lost partition scan (3.1.1) when user asks or when nothing was found
    return dedup(vols)
```

#### 3.1.1 Lost-partition scan

Scan the image at a stride (default 1 MiB, since every modern partitioner aligns to 1 MiB;
fall back to 4 KiB then 512 B when nothing is found - older XP-era tools aligned to CHS
cylinder boundaries, sector 63 = byte 32256 being the classic FAT/NTFS start). At each
position `p` run `probe_fs(p)`; additionally test the backup-structure signatures that imply
a volume start elsewhere:

* NTFS backup boot sector at `p`: implies the volume starts at `p - total_sectors*bps`.
* FAT32 backup boot sector at `p` with BkBootSec == 6: volume starts at `p - 6*bps`.
* exFAT backup boot region at `p` (sector 12 copy, checksum sector follows at +11 sectors):
  volume starts at `p - 12*bps`.
* ext4 backup superblock at `p` (magic at p+0x38, s_block_group_nr == g != 0): volume starts
  at `p - g*blocks_per_group*block_size - (block_size == 1024 ? 1024 : 0)`; for the primary
  (g == 0) at `p - 1024`.
* An MFT run ("FILE" with usa_ofs 0x30 at 1 KiB stride) implies NTFS; the boot sector's MFT
  LCN then pins the volume start: `start = mft_byte_pos - mft_lcn*cluster` once a candidate
  boot sector is found by other means.

Each hit produces a candidate `(start, len, fs, evidence)`. Merge candidates whose start
agrees; score = number of independent evidences (boot sector + backup + FAT copies ...). Drop
candidates whose range overlaps a higher-scored one unless the user asks to see all
(a card reformatted from FAT32 to exFAT shows both: the old FAT32 is "lost", its files
may still be carvable).

### 3.2 FAT

```
open_fat(vol):
    bs = read(0,512); if !valid_fat_bpb(bs): bs = read(6*512, 512) (FAT32 only) else reconstruct (2.2.10)
    geo = derive(bs)                                           # 2.2.2
    fat = FatTable::load(vol, geo)                             # both copies, per-sector readability
    root = geo.is_fat32 ? ClusterChain(root_clus) : FixedRegion(..)
    walk_dir(root, path=[], depth=0, visited={})

walk_dir(region, path, depth, visited):
    if depth > 64: return
    lfn = LfnAccumulator::new()
    for (idx, e) in entries(region):                       # 32-byte slices across clusters, in chain order
        b0 = e[0]
        if b0 == 0x00:   end_seen = true; break             # per spec; but see note below
        if e[0x0B] == 0x0F: lfn.push(e, deleted = b0 == 0xE5); continue
        deleted = b0 == 0xE5
        name = lfn.take_if_checksum_matches(e[0..11]) or short_name(e, deleted)
        first = (u16(e,0x14) << 16 | u16(e,0x1A)); size = u32(e,0x1C); attr = e[0x0B]
        if attr & 0x08: volume_label = name; continue
        if name in [".", ".."]: record_parent_hint; continue
        if attr & 0x10:
            if first in visited or !valid_cluster(first): emit_dir_entry(..broken); continue
            visited.insert(first)
            sub = deleted ? AssumeContiguousDir(first) : ClusterChain(first)
            emit Entry{dir, status: deleted?Deleted:Live}
            walk_dir(sub, path+name, depth+1, visited)
        else:
            extents = deleted ? assume_contiguous(first, size) : follow_chain(first, size)
            emit Entry{file, status, size, extents, times(e)}
    # Robustness: do NOT stop at the first 0x00 entry when the directory is deleted/damaged;
    # continue through the region while the heuristic of 2.2.9 says "still directory-like",
    # because a zeroed sector in the middle of a directory (bad block read as zeros) would
    # otherwise hide everything after it.

follow_chain(first, size) -> Vec<Extent>:
    extents = []; c = first; remaining = size (dirs: unbounded); seen = {}
    while valid_cluster(c) and !seen.contains(c) and (remaining > 0 or is_dir):
        seen.insert(c); coalesce_push(extents, Extent{physical: byte_of(c), len: min(csize, remaining), trust: FromMetadata})
        remaining -= csize
        n = fat.get(c)
        if n is Unreadable: n = other_fat.get(c)
        if is_eoc(n): break
        if n == 0 or n == 1 or !valid_cluster(n) or n == Bad:
            # chain broken: continue contiguously for the rest, tagged Guessed
            extents.extend(assume_contiguous(c+1, remaining).tag(Guessed)); break
        c = n
    if remaining > 0 and chain ended: mark Truncated
    return extents

assume_contiguous(first, size) -> Vec<Extent>:
    n = ceil(size / csize); clip so first+n-1 <= max_cluster
    for i in 0..n: overwritten = fat.get(first+i) != 0        # now allocated to someone else
    return [Extent{physical: byte_of(first), len: size, trust: AssumedContiguous, overwritten: any}]  # per-cluster flags kept in a side list
```

#### 3.2.1 Short name decoding

```
base = e[0..8].trim_end(' '); ext = e[8..11].trim_end(' ')
if base[0] == 0x05: base[0] = 0xE5
if deleted: base[0] = '?'                                # unknown first byte
if e[0x0C] & 0x08: base = lower(base); if e[0x0C] & 0x10: ext = lower(ext)
name = ext.empty ? base : base + "." + ext
decode bytes as the volume's OEM code page (default CP437 or CP850; user option; non-ASCII bytes -> U+FFFD or best-effort)
```

#### 3.2.2 Deleted-name first-letter guess

For a deleted short entry without LFN, if a preceding *deleted* LFN entry has a checksum
byte, try first bytes 'A'..'Z', '0'..'9', '_' until `lfn_checksum(candidate_11_bytes)` equals
it; if exactly one matches, use it tagged `Guessed`; else keep '?'.

#### 3.2.3 Free-cluster bitmap for the carver

`bitmap[c-2] = fat.get(c) == 0` for c in 2..=max_cluster, both FATs consulted; clusters
referenced by *deleted* files (assume-contiguous ranges) are reported separately so the
carver can give them priority and the validator can match carved files to names.

#### 3.2.4 Orphan directory scan

For every cluster not reachable from the root walk (and, optionally, every cluster): test
2.2.9. Hits are directories whose parent entry was lost. If the hit's `..` entry gives a
parent cluster that was walked, attach it there (name unknown: `lost+found/dir_<cluster>`);
if its `.` entry points to itself, confidence is high. Then `walk_dir` it (chain via FAT if
allocated, else assume-contiguous one cluster at a time while directory-like).

### 3.3 exFAT

```
open_exfat(vol):
    bs = read(0, 512) if valid else read(12*512, 512)       # backup boot region
    geo = parse(bs); csize = bps << spc_shift
    root = fat_chain(FirstClusterOfRootDirectory)           # root is always FAT-chained
    (bitmap, upcase, label) = parse_root_primaries(root)   # 0x81, 0x82, 0x83 entries
    walk_dir(root, [], 0)

walk_dir(data, path, depth):
    i = 0
    while i + 32 <= data.len:
        t = data[i]
        if t == 0x00: break (but see FAT note: continue if the rest looks like entries after a zeroed sector)
        if t & 0x7F == 0x05:                               # file entry, in use (0x85) or deleted (0x05)
            set = parse_set(data, i); i += 32 * (set.secondary_count + 1); continue_if_bad
            emit(set, deleted = t == 0x05)
            if set.is_dir and valid_cluster(set.first): walk_dir(read_stream(set), path + set.name, depth+1)
        else: i += 32

parse_set(data, i) -> Option<Set>:
    n = data[i+1]; require 1 <= n <= 18 and i + 32*(n+1) <= data.len
    stream = data[i+32..i+64]; require stream[0] & 0x7F == 0x40
    names = (2..=n).map(|k| data[i+32k..]).take_while(|e| e[0] & 0x7F == 0x41)
    copy = set bytes with bit 7 forced on in every entry's byte 0; ok = set_checksum(copy) == u16(data, i+2)
    name = concat(names[2..32]).utf16le()[..stream[3]]
    hash_ok = name_hash(upcase(name)) == u16(stream, 4)
    confidence = ok && hash_ok ? High : ok || hash_ok ? Medium : Low (reject when deleted and both fail)
    Set{ attrs: u16(data,i+4), times.., flags: stream[1], valid: u64(stream,8), first: u32(stream,0x14), size: u64(stream,0x18), name }

read_stream(set) -> Vec<Extent>:
    n = ceil(set.size / csize)
    if set.flags & 0x02 (NoFatChain) or set.deleted_and_fat_zeroed: contiguous(first, n), trust = deleted ? FromMetadata-but-check-bitmap : FromMetadata
    else: follow FAT chain exactly like FAT32 (EOC 0xFFFFFFFF, bad 0xFFFFFFF7), fall back to contiguous when the chain is 0 or wild
    cut the returned byte length at set.valid (ValidDataLength); keep [valid, size) as an optional tail
    for deleted sets mark clusters whose bitmap bit is 1 as overwritten
```

#### 3.3.1 Unallocated space

`free(c) = bitmap bit (c-2) == 0`. Also hand the carver the ranges of deleted sets.

#### 3.3.2 Orphan directory / lost root scan

Scan every cluster start (and every 32-byte offset in it is unnecessary: entry sets are
32-byte aligned within a cluster) for a byte sequence `0x85 n ...` followed 32 bytes later by
`0xC0`, verify the set checksum: a verified set is a directory fragment. Also accept `0x05 n`
+ `0x40` (deleted). Group hits by cluster; each cluster with >= 1 verified set is a directory
cluster; recover its files. If FirstClusterOfRootDirectory was unreadable, the root is the
cluster containing the 0x81/0x82/0x83 entries (mkfs puts it right after the up-case table;
observed cluster 5).

### 3.4 NTFS

#### 3.4.1 Record access

```
mft_read_record(n) -> Option<Record>:
    off = vcn_to_byte(mft_runs, n * rsize) ; raw = read(off, rsize)
    if raw[0..4] != "FILE": return None (log magic)
    fix = apply_fixups(raw)                              # Err(Torn) => keep going with lowered confidence
    hdr = parse_header(raw); require hdr.bytes_allocated == rsize && hdr.attrs_offset < rsize
    if hdr.mft_record_number != n and ntfs31: note("record number mismatch")  # MFT runs corrupt?
    Some(Record{n, hdr, attrs: parse_attrs(raw, hdr.attrs_offset), torn: fix.is_err()})

parse_attrs(raw, off):
    while off + 16 <= rsize:
        t = u32(raw, off); if t == 0xFFFFFFFF: break
        l = u32(raw, off+4); if l < 24 or l % 8 != 0 or off + l > rsize: break (truncated)
        yield Attr{t, nonres: raw[off+8], name: utf16(raw, off+u16(raw,off+0xA), raw[off+9]), flags: u16(raw,off+0xC), id: u16(raw,off+0xE), body: &raw[off..off+l]}
        off += l
```

#### 3.4.2 Attribute data

```
attr_data(rec, attr) -> Data:
    if attr.nonres == 0: Resident(&body[u16(body,0x14) .. +u32(body,0x10)])
    else: NonResident{ lowest_vcn, highest_vcn, alloc, size, init, runs: decode_runs(&body[u16(body,0x20)..]), compressed: flags&1, sparse: flags&0x8000 }

full_attribute(rec, type, name) -> Vec<Piece>:        # handles $ATTRIBUTE_LIST
    pieces = rec.attrs.filter(type, name).map(attr_data)
    if let Some(al) = rec.attr(0x20):
        for e in entries(attr_data(rec, al).bytes()):            # 2.4.9
            if e.type == type && e.name == name && MREF(e.ref) != rec.n:
                ext = mft_read_record(MREF(e.ref)); require ext.hdr.base_mft_record == ref_of(rec) (record# and seq)
                pieces.push(attr_data(ext, ext.attr_by_id(e.id)))
    sort pieces by lowest_vcn; splice run lists; sizes from the piece with lowest_vcn == 0
```

#### 3.4.3 Bootstrapping $MFT

```
open_ntfs(vol):
    boot = parse(read(0,512)) or parse(read(vol.len - 512, 512))
    rec0 = read(boot.mft_lcn * csize, rsize) with fixups; if bad: rec0 = read(boot.mftmirr_lcn * csize, rsize)
    mft_runs = decode_runs(unnamed $DATA of rec0); mft_size = data_size
    if rec0 unreadable: mft_runs = scan_for_mft(vol)                   # 2.4.13 last paragraph
    mft_bitmap = full_attribute(rec0, 0xB0, "")
    cluster_bitmap = full_attribute(record 6, 0x80, "")
```

#### 3.4.4 Enumerating files

```
enumerate():
    n = mft_size / rsize
    for i in 0..n:
        rec = mft_read_record(i) or continue
        if rec.hdr.base_mft_record != 0: continue                    # extension; reached via $ATTRIBUTE_LIST
        in_use = rec.hdr.flags & 1; bit = mft_bitmap[i]
        names = rec.attrs(0x30).map(parse_file_name)                  # may be empty: nameless record => "$Orphan/rec_<i>"
        best = names.max_by(namespace preference Win32=1 or 3 > POSIX=0 > DOS=2)
        std = rec.attr(0x10)
        data = full_attribute(rec, 0x80, "")                          # unnamed stream
        streams = rec.attrs(0x80) with names != "" (ADS; report only)
        status = in_use ? Live : Deleted; if in_use && !bit: note("in use but bitmap clear"); if !in_use && bit: note("bitmap set but not in use")
        extents = data.to_extents(); for e in extents: e.overwritten = status == Deleted && any cluster_bitmap bit set in e
        emit Entry{ name: best.name, parent: best.parent_ref, status, size: data.size, valid_size: data.init, extents, times: std or best, attrs, dir: flags&2, torn: rec.torn, confidence }
    build_paths()

build_paths():
    for e in entries: walk parent refs: p = MREF(e.parent); ps = MSEQNO(e.parent)
        if p == 5: root reached
        parent = entries[p]; if parent is None or parent.seq != ps or !parent.dir: e.status = Orphan if e was Live else keep Deleted with path "<unknown>"; break
        guard cycles (visited set, depth 256)
```

#### 3.4.5 Directory listing from the index (cross-check and recovery of names of reused records)

```
list_dir(rec):
    root = rec.attr(0x90, "$I30"); emit entries(root.body[0x10 + entries_offset .. 0x10 + index_length])
    if root.node_flags & 1:
        alloc = full_attribute(rec, 0xA0, "$I30"); bm = full_attribute(rec, 0xB0, "$I30")
        for v in 0..alloc.size / index_record_size:
            blk = read_vcn(alloc.runs, v * irs, irs); if blk[0..4] != "INDX": continue
            apply_fixups(blk); hdr at 0x18; emit entries(blk[0x18+entries_offset .. 0x18+index_length])
            if !bm[v] or user_wants_slack: emit scan_slack(blk[0x18+index_length .. 0x18+allocated]) with strict $FILE_NAME validation
entries(bytes): walk by entry length (>= 0x10, 8-aligned); stop at flag 0x02; key = $FILE_NAME body => (child ref, name, sizes, times)
```
Use: (1) sanity: a Live record not named in its parent's index is an `Orphan`; (2) names of
files whose MFT record was *reused* appear only in slack: emit `Deleted` entries with
`extents = []` and a note "record reused; carve". The $FILE_NAME in an index entry carries
the file's sizes, so even nameless/reused files have a size for carving.

#### 3.4.6 Unallocated space

`free(c) = cluster_bitmap bit c == 0`; exclude $BadClus:$Bad runs (record 8, named stream
"$Bad": clusters chkdsk marked bad - still worth imaging/carving, just unreliable).

### 3.5 ext4

#### 3.5.1 Checksums (only for validation/scoring; never required to read)

```
seed = CSUM_SEED ? s_checksum_seed : crc32c_raw(~0, uuid)        # crc32c_raw = Castagnoli, init given, NO final xor
sb_csum  = crc32c_raw(~0, sb[0..0x3FC])                          # superblock: seed is ~0, not the uuid seed
inode    = crc32c_raw(seed, le32(inum)) -> crc32c_raw(., le32(gen)) -> crc32c_raw(., inode bytes with csum fields 0)
dirblock = crc32c_raw(seed, le32(inum)) -> (., le32(gen)) -> (., block bytes up to the tail entry)
gdt_csum (METADATA_CSUM) = crc32c_raw(seed, le32(group)) -> (., desc[0..0x1E]) -> (., 00 00) -> (., desc[0x20..desc_size]) & 0xFFFF
gdt_csum (GDT_CSUM only) = crc16(0xFFFF init, reflected poly 0x8005 (table built from 0xA001), no final xor = CRC-16/MODBUS) over uuid, le32(group), desc bytes 0..0x1E, then 0x20..desc_size when 64BIT
```
All five formulas were verified 2026-10-12 against kernel source (`fs/ext4/super.c`
`ext4_group_desc_csum` / `ext4_superblock_csum`, `lib/crc/crc16.c`: "The poly is 0x8005", table
0x0000, 0xC0C1, 0xC181, 0x0140, ..., `crc = (crc >> 8) ^ crc16_table[(crc & 0xff) ^ *p++]`) and
**by recomputation on dev-box images** with a 30-line Python implementation (reflected crc32c
table from 0x82F63B78, `c = T[(c ^ b) & 0xFF] ^ (c >> 8)`, init = given seed, no final xor):
mke2fs 1.47.2 `-O ^metadata_csum,uninit_bg` -> group 0 crc16 0x9285 == dumpe2fs; default
ext4 -> superblock 0x38e644fd == dumpe2fs "Checksum", seed field 0x82ced6f9 == crc32c_raw(~0,
uuid), group 0 0xe10d == dumpe2fs, inodes 2/7/8 == debugfs "Inode checksum"; `-O
^metadata_csum_seed` -> seed = crc32c_raw(~0, uuid) = 0x03bc5831 reproduces group and inode
checksums. The script is reproduced in 5.5 (`ext4csum.py`); use it as the oracle for the
Rust `csum` module.

Rust `crc32c` crate note: `crc32c::crc32c_append(crc, data)` takes and returns the
*finalised* (xor 0xFFFFFFFF) value (the crate's `crc32c(data) == crc32c_append(0, data)` and
its check value for "123456789" is 0xE3069283, the standard CRC-32C); the kernel's raw form
is therefore `raw_out = !crc32c_append(!raw_in, data)`. Verified 2026-10-12 by running
`crc32c` 0.6 and `crc` 3.4.0 on the dev box against the images above:
`!crc32c_append(!0xFFFFFFFF, sb[..0x3FC])` = 0x38e644fd = dumpe2fs "Checksum", and
`Crc::<u16>::new(&CRC_16_MODBUS)` over uuid + le32(0) + desc = 0x9285 = dumpe2fs "csum".
Still pin both with unit tests that reproduce the debugfs "Inode checksum" of the fixture
(5.5) - the Python oracle makes the expected values available without a mounted filesystem.

#### 3.5.2 Opening

```
open_ext(vol):
    sb = read(1024, 1024); if !valid(sb): for g in [1,3,5,7,9,25,27,49,81,...]: try backup at group g (2.5.1 locations) until valid; record which copy was used
    bs = 1024 << sb.log_block_size; groups = ceil((blocks_count - first_data_block) / blocks_per_group)
    desc_size = 64BIT ? sb.desc_size : 32; gdt_block = first_data_block + 1
    gdt = read(gdt_block*bs, groups*desc_size) ; if unreadable: use the GDT copy following the backup superblock in group 1 (or 3, 5 ...)
    for g: inode_table[g] = desc.inode_table_lo | hi<<32 ; validate < blocks_count; block_bitmap[g], inode_bitmap[g], flags
```

#### 3.5.3 Reading an inode's data

```
inode_extents(ino, inode) -> Vec<Extent>:
    if inode.flags & INLINE_DATA: return [Inline(i_block[0..min(60,size)] ++ xattr system.data)]
    if inode.is_symlink && size < 60 && !(flags & EXTENTS && extent header has entries): return [Inline(i_block[..size])]
    if inode.flags & EXTENTS: return extent_tree(i_block, depth_limit 5, visited {})
    else: return block_map(i_block, bs)
extent_tree(node_bytes, depth):
    hdr = header(node_bytes); require hdr.magic == 0xF30A && hdr.entries <= hdr.max && hdr.max <= (len-12)/12
    if hdr.depth == 0: for e in entries: push Extent{logical: e.block*bs, physical: start*bs, len: (len & 0x7FFF)*bs, uninit: len > 0x8000, trust: FromMetadata}
    else: for idx in entries: child = leaf_lo | leaf_hi<<32; require child < blocks_count && !visited; recurse(read_block(child), depth-1)
    sort by logical; clip to i_size; gaps => holes
block_map(i_block):
    direct 0..12; ind(i_block[12], 1); ind(i_block[13], 2); ind(i_block[14], 3); each pointer validated; stop at first invalid when beyond size
```

#### 3.5.4 Enumerating

Two passes, both always run:

1. **Tree walk** from inode 2: read directory blocks (3.5.3), parse ext4_dir_entry_2
   linearly per block (skip inode == 0 entries and the 0xDE tail; htree blocks parse as
   linear automatically), recurse into type-2 entries (or type unknown + inode is dir),
   emit `Live` entries. Also scan dirent slack (2.5.8) for `Deleted` names.
2. **Inode table sweep**: for every group without INODE_UNINIT (and *with* it too when the
   superblock says errors were detected, with lower confidence), for every inode slot:
   `mode != 0 && (links > 0 || dtime != 0)`; classify: `Live` if links > 0 and bitmap bit set
   (if not reached by the tree walk -> `Orphan`, under `lost+found/#ino`); `Deleted` if
   links == 0 or dtime != 0 or bitmap bit clear; for `Deleted` try `inode_extents` anyway
   (debugfs-style deletions keep them; kernel deletions have eh_entries == 0 -> no data).
   Pair deleted inodes with names from pass-1 slack by inode number.

#### 3.5.5 Unallocated space

Per group: `free(b) = block_bitmap bit (b - group_first_block) == 0`, BLOCK_UNINIT => all
free except metadata blocks of that group (sb/GDT copies if a backup group, bitmaps, inode
table - compute from descriptors); exclude blocks beyond blocks_count. Hand the journal's
extents (inode 8) to the carver as a separate high-value region.

### 3.6 Damaged-media read policy (all filesystems)

* Every metadata read checks the mapfile. A bad sector inside a FAT/bitmap/inode table is
  substituted from its redundant copy (FAT1, backup GDT, $MFTMirr) when one exists; else the
  structure is parsed with the bad bytes zeroed and the result tagged `partial`.
* A file whose data extents intersect bad ranges is still emitted, with the bad ranges listed
  per extent so the repair modules know exactly which byte ranges are untrusted.
* The parser never seeks past the image length; a run/extent/cluster beyond the volume is
  clipped and the entry tagged `Truncated`.
* Never recurse into a directory cluster/block/record already visited (cycle guard), never
  deeper than 64 levels, never more than 2^20 entries per directory (bail with a note).

---

## 4. Edge cases and failure modes seen on real failing media

Each item: what it looks like, what the parser must do.

### 4.1 Cross-cutting

| Symptom | Cause | Handling |
|---|---|---|
| Sector 0 all zeros or random | Dead first track (very common on HDDs: head parks there; on SD: controller returned garbage) | Backup boot sectors/superblocks (2.x), lost-partition scan (3.1.1) |
| Partition table lists a partition whose start has no signature | Table from a previous layout, or the start sector is bad | Probe backups; scan forward up to 2 MiB from the stated start for a boot sector (misaligned tools) |
| Whole-image sector size mismatch (512e vs 4Kn) | Image taken through a USB bridge that reports 512 while the FS was made with 4096 (or vice versa) | Boot sectors carry bytes-per-sector; trust the FS value over the device; GPT: try both LBA 1 positions |
| Image shorter than the volume says (`TotSec`, `VolumeLength`, `blocks_count`, `total sectors`) | Partial image (imaging aborted) or the drive now reports a smaller capacity (HPA/DCO, firmware fallback) | Clip extents; mark files beyond the end `Truncated`; never fail the volume |
| Repeating 512-byte pattern (e.g. the same JPEG sector 1000s of times) | Failing controller returning a stale buffer | Detect runs of identical sectors > 64 in metadata regions, treat as unreadable |
| 0xFF-filled regions | Unprogrammed NAND pages exposed by a dead FTL, or a reader's error fill | Treat as unreadable for metadata; FAT entries of 0xFFFF.. look like EOC - do not trust a FAT sector that is entirely 0xFF |
| 0x00-filled regions inside directories | Bad sector read as zeros by the imaging engine | FAT/exFAT: a 0x00 entry normally means "end of directory"; continue if the next sector is directory-like |
| Volume reported as dirty (FAT[1] bits, exFAT VolumeDirty, NTFS $Volume flags 0x01, ext4 s_state & 2) | Power loss during write | Expect torn metadata: NTFS torn records, FAT chains ending in 0, exFAT sets with wrong checksum; parse tolerant |
| Two filesystems overlapping (reformat) | Card reformatted (FAT32 -> exFAT, NTFS quick format) | Report both; the older one's directories are often intact in unallocated clusters (orphan directory scans) |
| Camera "ghost" files: size 0 or ValidDataLength 0, DataLength huge | Recording interrupted | Recover by DataLength if the clusters contain MP4 data; the MP4 repair module will rebuild moov |
| Files of exactly cluster-multiple sizes with trailing garbage | Normal (slack) | Size field truncates; never rely on content end markers for size |

### 4.2 FAT

* **Deleted-file chains overlap**: two deleted files that once occupied the same clusters
  (one written after the other was deleted). Both are listed; the validator decides which
  content matches which name (EXIF/mtime). Mark both `AssumedContiguous` with a shared-cluster
  note.
* **FAT32 high cluster word zeroed on Windows delete** (2.2.8). On 64 GB+ cards every deleted
  file above cluster 65535 has the wrong first cluster; implement the h-guess.
* **Fragmented large videos**: camera firmware appends to the first free cluster; after
  deletions a 4 GB MOV can be split in 2-5 pieces. Deleted fragmented files cannot be
  recovered exactly from metadata; use the assume-contiguous recovery and let the MP4 repair
  module detect the junction (bad `mdat` chunk boundaries) and the carver supply the rest.
* **Directory cluster reused**: a deleted directory's first cluster reallocated to a file:
  2.2.9 fails on it, nothing to list; the names are gone.
* **Both FATs differ**: FAT1 is often stale on cameras (written lazily) and on volumes with
  ExtFlags mirroring disabled. Prefer the active FAT, use the other only for unreadable
  sectors.
* **"Dirty" FAT12/16 on 8-32 MB SD/MMC cards from old cameras**: no backup boot sector at
  all, BPB corruption kills the volume; reconstruct via 2.2.10.
* **Short names with code page bytes**: Japanese cameras write Shift-JIS short names; decode
  as the configured code page, never panic on invalid bytes.
* **Volume label entries with 0xE5** (deleted label) and entries with attr 0x0F but a bad
  checksum (orphan LFN pieces after a short entry was overwritten) - ignore, do not attach.
* **Root directory of FAT12/16 is full** (512 entries): camera firmware stops writing;
  nothing special for us.
* **Cluster count near the thresholds**: a card formatted FAT16 by a camera with 65,500
  clusters that Windows would call FAT32 - keep the width that parses.
* **Entries past the 0x00 terminator**: old data from before a reformat; scanning past the
  terminator (with the 2.2.9 heuristic) recovers them as `Deleted`.

### 4.3 exFAT

* **VolumeDirty + checksum sector stale**: Windows updates VolumeFlags/PercentInUse in place
  and the checksum deliberately skips them; everything else in the boot sector must checksum.
  A mismatch means the boot sector itself was corrupted - use the backup.
* **Bitmap says free but data intact** (deleted files) vs **bitmap says allocated but
  directory says deleted** (dirty unmount): treat `allocated && deleted` as "possibly
  overwritten, check the content".
* **Entry sets straddling cluster boundaries**: sets may span two clusters of a FAT-chained
  directory; always concatenate the whole directory before parsing sets.
* **Secondary entries missing** (SecondaryCount larger than what follows, or the 0xC0 is
  missing): emit the file with name = "" and zero size, confidence Low; happens when a cluster
  of the directory is unreadable.
* **NoFatChain flag cleared but FAT entries all 0**: seen when the driver crashed before
  writing the FAT; treat as contiguous.
* **Up-case table unreadable**: fall back to ASCII upper-casing for NameHash validation
  (lower the weight of the hash check).
* **4 GiB+ preallocation by camcorders** (DataLength = 4 GiB, ValidDataLength small): obey
  ValidDataLength, offer the tail.
* **TexFAT (NumberOfFats == 2, second bitmap 0x81 with flag bit 0 = 1)**: Windows CE only;
  read the first FAT/bitmap and warn.

### 4.4 NTFS

* **Torn FILE/INDX records** (fixup mismatch): the single most common artefact after power
  loss on a failing HDD. Parse anyway with confidence lowered. If the torn sector contains the
  run list, the runs may be garbage: validate every LCN against the volume size and $Bitmap.
* **$MFT itself fragmented**: `11 13 04` on a fresh volume, but production volumes often have
  2-20 runs; a record number -> byte address must always go through the run list. If the
  $MFT run list is lost, rebuild by scanning for "FILE" records (2.4.13).
* **MFT record 0 unreadable and $MFTMirr unreadable**: scan.
* **Record reuse**: a deleted file's record reused by a new file: the old $FILE_NAME is gone;
  only the parent's INDX slack may still carry its name and size.
* **Sequence number mismatches** between $FILE_NAME.parent and the parent record: the parent
  was deleted and reused -> the file is an orphan; still list it under `$Orphan`.
* **Extension records whose base record is bad**: the extension's $DATA pieces are
  recoverable only by carving the run lists; v1: report "fragment of record N".
* **Compressed files**: flag 0x0001 with compression_unit 4; report them as compressed; v1
  may skip decompression (document to the user: "file is NTFS-compressed; not decoded").
* **Sparse runs in normal files** (`off_size == 0`): zeros; common for preallocated video
  files written by capture software - the sparse region is the unwritten tail.
* **initialized_size < data_size**: bytes beyond initialized_size are zeros by definition on a
  healthy volume; on a failing one they may contain the real data (writes landed but the
  metadata update did not): offer "read beyond initialized size" as an option.
* **Hard links** (link_count > 1): one record, several names/parents; emit once per name.
* **Junctions/reparse points, encrypted files ($EFS), $Extend/$UsnJrnl**: list, do not
  recurse into reparse targets, do not recover encrypted content.
* **4 KiB MFT records** (4Kn drives, `clusters_per_mft_record` = -12): usa_count = 9; never
  hard-code 1024.
* **Cluster size > 64 KiB** (Windows 10 1709+, up to 2 MiB; sectors_per_cluster encoded as
  a negative power, verified against ntfs-3g bootsect.c and ntfs3 super.c, see 2.4.1): if the
  u8 at 0x0D is > 128 use `1 << (256 - value)` and require the result to be a power of two
  with cluster bytes <= 2 MiB; record size 1 KiB with 2 MiB clusters still means
  `clusters_per_mft_record` is negative (-10), never a cluster count.
* **$Bitmap says free but record says in use**: chkdsk interrupted; prefer the record.
* **BAAD records**: chkdsk replaced the magic after detecting a bad multi-sector transfer;
  the attributes may still be intact: parse as FILE with confidence Low.

### 4.5 ext4

* **Superblock at 1024 destroyed**: backups at groups 1, 3, 5, 7, 9, 25 ... (2.5.1); the
  copy's s_block_group_nr confirms which one you found. s_state/s_wtime/s_free_* in backups
  are stale: fine for reading.
* **GDT destroyed**: copy after the backup superblock; with FLEX_BG all inode tables of a
  flex group are consecutive after the bitmaps, so a missing descriptor can be inferred from
  neighbours (`inode_table[g] = inode_table[g-1] + inode_blocks_per_group`, for g not the first
  of its flex group).
* **Inode table block unreadable**: lose `block_size / inode_size` (16 with 4K/256) inodes;
  their names still appear in directories: emit `Deleted?` entries with no data ("inode
  unreadable").
* **Directory block unreadable**: files remain reachable through the inode sweep as `Orphan`
  (names lost, but EXIF usually restores a date-based name).
* **Journal (inode 8) not replayed** (INCOMPAT_RECOVER set): the on-disk inode table may be
  stale relative to the journal; the journal blocks contain newer copies of inode table and
  directory blocks. v1: read the on-disk state, note "journal pending"; v2: scan the journal
  for inode-table blocks (descriptor blocks tag fs block numbers) and prefer newer copies.
* **Deleted files**: no extents (2.5.9) - do not promise recovery; names and sizes only;
  carving does the work. ext3 with `data=journal` and ext4 journals do carry recent file data
  blocks and old inode copies with intact extents (extundelete's method): optional future
  module.
* **Uninitialised extents** (ee_len > 32768): after fallocate; treat as zeros but allow the
  user to read the raw blocks.
* **Huge directories with htree**: linear read works; do not implement the hash.
* **Encrypted directories (fscrypt, INCOMPAT_ENCRYPT, inode flag 0x800)**: names and
  contents are ciphertext; report and skip.
* **Casefold (0x20000)**: names are plain UTF-8 anyway.
* **bigalloc**: block bitmap bits are per cluster (2^s_log_cluster_size blocks); v1 refuse
  with a clear message (free-space math would be wrong).
* **64bit feature with 32-byte descriptors claimed** (s_desc_size 0/32 with INCOMPAT_64BIT):
  corrupt superblock; take desc_size from the backup.
* **1 KiB block filesystems** (old SD/USB with ext2/3 from embedded devices): superblock is
  block 1, GDT block 2, group 0 starts at block 1 - every "block number" offset must add
  nothing (block numbers are absolute) but group layout math uses first_data_block = 1.
* **Inodes with i_size < bytes mapped / i_size > bytes mapped**: clip to i_size; if extents
  cover less than i_size the gap is a hole (zeros), not an error.

---

## 5. Test fixture strategy (unprivileged, only the tools on the dev box)

### 5.1 What exists and what works without root (VERIFIED on dev box, 2026-10-12)

Tool versions: mkfs.fat 4.2, fsck.fat 4.2, exfatprogs 1.3.2 (mkfs.exfat, fsck.exfat,
dump.exfat), ntfs-3g 2022.10.3 (mkntfs, ntfscp, ntfsls, ntfsinfo, ntfsundelete, ntfsfix,
ntfs-3g FUSE), e2fsprogs 1.47.2 (mke2fs, debugfs, dumpe2fs, e2fsck), Python 3 with
Pillow 12.1.1, ffmpeg 8.0.1, `unshare`, `fusermount3`, `/dev/fuse` mode 0666.
**Missing**: mtools (mcopy), exfat-fuse/mount.exfat, fuse2fs, fusefat, losetup access as
non-root, ddrescue, testdisk/photorec, exiftool, smartctl.

| Need | Works unprivileged? | How |
|---|---|---|
| Create FAT12/16/32 image | yes | `truncate -s N img && mkfs.vfat -F 12|16|32 -n LABEL img` |
| Put files into FAT image | **no tool** (no mtools, loop mount needs root; `unshare -Urm mount -o loop` fails: loop devices are not available in a user namespace - verified) | `fatput.py` (5.2) - stdlib Python, validated by fsck.vfat |
| Delete a file in a FAT image | no tool | `fatput.py rm` (5.2); emulates Windows (`--win32` zeroes FstClusHI) or Linux |
| Create exFAT image | yes | `mkfs.exfat -L LABEL img` |
| Put/delete files in exFAT | **no tool** (exfatprogs has no copy; no FUSE exfat) | `exfatput.py` (5.3), validated by fsck.exfat + dump.exfat |
| Create NTFS image | yes, **`-F` required** for a regular file ("is not a block device. Refusing" without it) | `mkntfs -F -Q -L LABEL img` |
| Put files into NTFS | yes | `ntfscp img src /dest` (root dir or existing dir; ntfscp cannot mkdir) |
| Make subdirectories / delete files in NTFS | yes via FUSE (**verified**: `ntfs-3g img mnt` as uid 1000 mounts a file image through /dev/fuse + setuid fusermount3; `rm`, `mkdir`, `cp` then `fusermount3 -u mnt`; image passes `ntfsfix -n` and `ntfsundelete -s` lists the deleted file) | 5.4 |
| Delete in NTFS without FUSE (CI without /dev/fuse) | partial | `ntfsdel.py` (5.4) clears in-use + bumps seq + clears $MFT bitmap bit; the $I30 entry remains (realistic "stale index" case) |
| Create ext4/ext2 with content | yes | `mke2fs -q -F -t ext4 -d SRCDIR -L LABEL img` (populates from a directory incl. symlinks, subdirs, ownership) |
| Delete a file in ext4 image | yes | `debugfs -w -R 'rm /path' img` (**keeps extents + i_size**, unlike the kernel); to emulate the kernel add `set_inode_field` calls (5.5) |
| Mount anything as loop | no (needs root) | not used |
| Assemble a partitioned disk image (MBR/GPT) around fs images | yes | `mkdisk.py` (5.6), pure Python |

### 5.2 FAT fixture: `fatput.py` (validated: fsck.vfat -n reports clean after put and rm on FAT12/16/32)

Build script (`tests/fixtures/build_fat.sh`):
```sh
set -e
python3 - <<'PY'
from PIL import Image
im = Image.new('RGB', (640, 480)); px = im.load()
for y in range(480):
    for x in range(640): px[x, y] = (x % 256, y % 256, (x * y) % 256)
im.save('photo1.jpg', quality=90); im.save('photo2.png')
PY
ffmpeg -y -loglevel error -f lavfi -i testsrc=size=320x240:rate=30 -t 2 -c:v libx264 -pix_fmt yuv420p clip.mp4
for t in 12 16 32; do
  case $t in 12) sz=16M;; 16) sz=40M;; 32) sz=64M;; esac
  truncate -s $sz fat$t.img && mkfs.vfat -F $t -n "TEST$t" fat$t.img >/dev/null
  python3 fatput.py fat$t.img put photo1.jpg "Holiday Photo 2024.jpg"
  python3 fatput.py fat$t.img put photo2.png photo2.png
  python3 fatput.py fat$t.img put clip.mp4 "big video clip.mp4"
  python3 fatput.py fat$t.img rm photo2.png                 # Linux-style delete (high word kept)
  python3 fatput.py fat$t.img rm "big video clip.mp4" --win32   # Windows-style (FstClusHI zeroed)
  fsck.vfat -n fat$t.img
done
```
Expected parser output: `Holiday Photo 2024.jpg` Live (LFN, 2 entries); `photo2.png` Deleted
with LFN-recovered name, short name `?HOTO2~1.PNG`, extents assumed contiguous from the
stored first cluster; `big video clip.mp4` Deleted, first cluster high word 0 (identical on
these small volumes, but the test asserts the `--win32` path is exercised by checking the
raw entry). Observed first clusters on the dev box: FAT12 2/17/26, FAT16 2/61/96,
FAT32 3/239/377 (cluster 2 = root dir on FAT32).

Create a damaged variant: `dd if=/dev/zero of=fat32.img bs=512 count=1 conv=notrunc` (sector 0
gone -> backup at sector 6 must be used); `dd if=/dev/zero of=fat16.img bs=512 seek=4 count=1
conv=notrunc` zeroes the first FAT sector of FAT0 (RsvdSecCnt 4) -> FAT1 must be consulted.

`fatput.py` (pure stdlib; root directory only; files are written contiguously first-fit):
```python
#!/usr/bin/env python3
"""fatput.py - put/delete files in a FAT12/16/32 image without mounting or mtools.

usage: fatput.py IMG put  SRC DESTNAME        (root directory only; long names ok)
       fatput.py IMG rm   DESTNAME [--win32]  (mark 0xE5, zero FAT chain; --win32 also zeroes FstClusHI)
       fatput.py IMG ls
Writes files contiguously (first-fit). Pure stdlib. Validate with fsck.vfat -n IMG.
"""
import struct, sys, os, time

def bpb(b):
    d = {}
    d['bps']  = struct.unpack_from('<H', b, 0x0B)[0]
    d['spc']  = b[0x0D]
    d['rsvd'] = struct.unpack_from('<H', b, 0x0E)[0]
    d['nfats']= b[0x10]
    d['rootent'] = struct.unpack_from('<H', b, 0x11)[0]
    tot16 = struct.unpack_from('<H', b, 0x13)[0]
    fatsz16 = struct.unpack_from('<H', b, 0x16)[0]
    tot32 = struct.unpack_from('<I', b, 0x20)[0]
    fatsz32 = struct.unpack_from('<I', b, 0x24)[0]
    d['fatsz'] = fatsz16 if fatsz16 else fatsz32
    d['tot']   = tot16 if tot16 else tot32
    d['rootsecs'] = ((d['rootent'] * 32) + (d['bps'] - 1)) // d['bps']
    d['firstdata'] = d['rsvd'] + d['nfats'] * d['fatsz'] + d['rootsecs']
    d['nclus'] = (d['tot'] - d['firstdata']) // d['spc']
    d['type'] = 12 if d['nclus'] < 4085 else (16 if d['nclus'] < 65525 else 32)
    d['rootclus'] = struct.unpack_from('<I', b, 0x2C)[0] if d['type'] == 32 else 0
    d['eoc'] = {12: 0xFFF, 16: 0xFFFF, 32: 0x0FFFFFFF}[d['type']]
    return d

class Fat:
    def __init__(self, path):
        self.f = open(path, 'r+b')
        self.f.seek(0); self.d = bpb(self.f.read(512))
        d = self.d
        self.f.seek(d['rsvd'] * d['bps']); self.fat = bytearray(self.f.read(d['fatsz'] * d['bps']))
    def get(self, n):
        t = self.d['type']
        if t == 12:
            o = n + n // 2; v = struct.unpack_from('<H', self.fat, o)[0]
            return (v >> 4) if n & 1 else (v & 0xFFF)
        if t == 16: return struct.unpack_from('<H', self.fat, n * 2)[0]
        return struct.unpack_from('<I', self.fat, n * 4)[0] & 0x0FFFFFFF
    def set(self, n, v):
        t = self.d['type']
        if t == 12:
            o = n + n // 2; cur = struct.unpack_from('<H', self.fat, o)[0]
            cur = (cur & 0x000F) | (v << 4) if n & 1 else (cur & 0xF000) | (v & 0xFFF)
            struct.pack_into('<H', self.fat, o, cur)
        elif t == 16: struct.pack_into('<H', self.fat, n * 2, v)
        else:
            old = struct.unpack_from('<I', self.fat, n * 4)[0] & 0xF0000000
            struct.pack_into('<I', self.fat, n * 4, old | (v & 0x0FFFFFFF))
    def flush_fat(self):
        d = self.d
        for i in range(d['nfats']):
            self.f.seek((d['rsvd'] + i * d['fatsz']) * d['bps']); self.f.write(self.fat)
    def clus_off(self, c):
        d = self.d; return (d['firstdata'] + (c - 2) * d['spc']) * d['bps']
    def csize(self): return self.d['spc'] * self.d['bps']
    def find_free_run(self, n):
        run = 0
        for c in range(2, 2 + self.d['nclus']):
            run = run + 1 if self.get(c) == 0 else 0
            if run == n: return c - n + 1
        raise SystemExit('no contiguous free space')
    # root directory as (offset, size) regions
    def root_regions(self):
        d = self.d
        if d['type'] != 32:
            return [(d['rsvd'] * d['bps'] + d['nfats'] * d['fatsz'] * d['bps'], d['rootsecs'] * d['bps'])]
        regs, c = [], d['rootclus']
        while 2 <= c < 0x0FFFFFF8:
            regs.append((self.clus_off(c), self.csize())); c = self.get(c)
        return regs
    def read_root(self):
        return b''.join(self._read(o, n) for o, n in self.root_regions())
    def _read(self, o, n): self.f.seek(o); return self.f.read(n)
    def write_root(self, data):
        pos = 0
        for o, n in self.root_regions():
            self.f.seek(o); self.f.write(data[pos:pos + n]); pos += n

def lfn_checksum(short11):
    s = 0
    for b in short11: s = ((s & 1) << 7) + (s >> 1) + b; s &= 0xFF
    return s

def short_name(name, existing):
    base, _, ext = name.upper().partition('.')
    base = ''.join(ch for ch in base if ch.isalnum() or ch in '_-')[:6] or 'FILE'
    ext = ''.join(ch for ch in ext if ch.isalnum())[:3]
    for i in range(1, 100):
        cand = (base + '~' + str(i)).ljust(8)[:8] + ext.ljust(3)
        if cand.encode() not in existing: return cand.encode('ascii')
    raise SystemExit('no short name')

def dos_time(t=None):
    tm = time.localtime(t)
    return ((tm.tm_hour << 11) | (tm.tm_min << 5) | (tm.tm_sec // 2),
            (((tm.tm_year - 1980) & 0x7F) << 9) | (tm.tm_mon << 5) | tm.tm_mday)

def lfn_entries(name, chk):
    u = name.encode('utf-16-le') + b'\x00\x00'
    u += b'\xff' * ((-len(u)) % 26)
    chunks = [u[i:i + 26] for i in range(0, len(u), 26)]
    out = []
    for idx, ch in reversed(list(enumerate(chunks, 1))):
        seq = idx | (0x40 if idx == len(chunks) else 0)
        out.append(bytes([seq]) + ch[0:10] + b'\x0f\x00' + bytes([chk]) + ch[10:22] + b'\x00\x00' + ch[22:26])
    return b''.join(out)

def put(fs, src, dest):
    data = open(src, 'rb').read()
    root = bytearray(fs.read_root())
    existing = {bytes(root[i:i + 11]) for i in range(0, len(root), 32)}
    sn = short_name(dest, existing)
    n = max(1, -(-len(data) // fs.csize()))
    first = fs.find_free_run(n) if data else 0
    if data:
        for i in range(n): fs.set(first + i, first + i + 1 if i < n - 1 else fs.d['eoc'])
        fs.f.seek(fs.clus_off(first)); fs.f.write(data)
        fs.flush_fat()
    t, dt = dos_time()
    ent = sn + bytes([0x20, 0, 0]) + struct.pack('<HHHHHHHI', t, dt, dt, (first >> 16) & 0xFFFF, t, dt, first & 0xFFFF, len(data))
    entries = lfn_entries(dest, lfn_checksum(sn)) + ent
    need = len(entries) // 32
    # find `need` consecutive free (0x00 or 0xE5) slots
    free = 0
    for i in range(0, len(root), 32):
        free = free + 1 if root[i] in (0x00, 0xE5) else 0
        if free == need:
            s = i - (need - 1) * 32; root[s:s + len(entries)] = entries; fs.write_root(root); return
    raise SystemExit('root directory full')

def entries(root):
    """yield (index, short_entry, long_name) for in-use entries"""
    lfn = {}
    for i in range(0, len(root), 32):
        e = root[i:i + 32]
        if e[0] == 0: break
        if e[11] == 0x0F and e[0] != 0xE5:
            lfn[e[0] & 0x3F] = (e[1:11] + e[14:26] + e[28:32]); continue
        if e[0] == 0xE5: lfn = {}; continue
        name = b''.join(lfn[k] for k in sorted(lfn)).decode('utf-16-le').split('\x00')[0] if lfn else None
        yield i, e, name
        lfn = {}

def rm(fs, dest, win32):
    root = bytearray(fs.read_root())
    for i, e, name in entries(root):
        sn = (e[0:8].decode('latin1').rstrip() + ('.' + e[8:11].decode('latin1').rstrip() if e[8:11].strip() else ''))
        if name == dest or sn.lower() == dest.lower():
            first = struct.unpack_from('<H', e, 0x1A)[0] | (struct.unpack_from('<H', e, 0x14)[0] << 16)
            c = first
            while 2 <= c < fs.d['eoc'] - 7 and c != 0:
                nxt = fs.get(c); fs.set(c, 0); c = nxt
                if nxt == 0: break
            fs.flush_fat()
            j = i
            while j - 32 >= 0 and root[j - 32 + 11] == 0x0F: j -= 32
            for k in range(j, i + 32, 32): root[k] = 0xE5
            if win32: struct.pack_into('<H', root, i + 0x14, 0)
            fs.write_root(root); return
    raise SystemExit('not found')

if __name__ == '__main__':
    fs = Fat(sys.argv[1]); cmd = sys.argv[2]
    if cmd == 'put': put(fs, sys.argv[3], sys.argv[4])
    elif cmd == 'rm': rm(fs, sys.argv[3], '--win32' in sys.argv)
    elif cmd == 'ls':
        print('FAT%d clusters=%d' % (fs.d['type'], fs.d['nclus']))
        for i, e, name in entries(fs.read_root()):
            first = struct.unpack_from('<H', e, 0x1A)[0] | (struct.unpack_from('<H', e, 0x14)[0] << 16)
            print('%-12s %-30s clus=%d size=%d' % (e[:11].decode('latin1'), name, first, struct.unpack_from('<I', e, 0x1C)[0]))
    fs.f.close()
```

### 5.3 exFAT fixture: `exfatput.py` (validated: fsck.exfat -n "clean", dump.exfat free-cluster count drops/rises, NameHash accepted)

```sh
truncate -s 64M exfat.img && mkfs.exfat -L TESTEX exfat.img
python3 exfatput.py exfat.img put photo1.jpg "Holiday Photo 2024 exfat.jpg"   # 2 File Name entries (28 chars)
python3 exfatput.py exfat.img put photo2.png photo2.png
python3 exfatput.py exfat.img put clip.mp4 clip.mp4
python3 exfatput.py exfat.img rm photo2.png
fsck.exfat -n exfat.img && dump.exfat exfat.img | grep -E 'Root Cluster|Free Clusters'
```
Observed: root cluster 5, files at clusters 6, 36, 54 with GeneralSecondaryFlags 0x03
(AllocationPossible|NoFatChain); after rm the set reads 0x05/0x40/0x41 with checksum still
verifying when bit 7 is restored, bitmap bits cleared, FAT untouched (all zero).
Damaged variants: zero sector 0 (backup at sector 12 + checksum at 23 must be used);
zero the root directory cluster (orphan set scan must find the files by set checksum).

```python
#!/usr/bin/env python3
"""exfatput.py - put/delete files in an exFAT image without mounting (exfatprogs has no copy tool).

usage: exfatput.py IMG put SRC DESTNAME     (root directory; contiguous, NoFatChain=1)
       exfatput.py IMG rm  DESTNAME         (clear InUse bit 7 on the entry set, free bitmap bits)
       exfatput.py IMG ls
Pure stdlib. Validate with fsck.exfat -n IMG and dump.exfat IMG.
"""
import struct, sys, time

class Ex:
    def __init__(self, path):
        self.f = open(path, 'r+b'); self.f.seek(0); b = self.f.read(512)
        assert b[3:11] == b'EXFAT   ', 'not exFAT'
        self.fat_off, self.fat_len, self.heap_off, self.nclus, self.rootclus = struct.unpack_from('<IIIII', b, 80)
        self.bps = 1 << b[108]; self.spc = 1 << b[109]; self.csz = self.bps * self.spc
        self.root = self.read_chain(self.rootclus)
        self.bitmap_clus = self.bitmap_len = None; self.upcase = None
        for off in range(0, len(self.root), 32):
            e = self.root[off:off + 32]
            if e[0] == 0x81: self.bitmap_clus, self.bitmap_len = struct.unpack_from('<IQ', e, 20)
            if e[0] == 0x82:
                uc, ul = struct.unpack_from('<IQ', e, 20); self.upcase = self.load_upcase(self.read_clusters(uc, -(-ul // self.csz))[:ul])
        self.bitmap = bytearray(self.read_clusters(self.bitmap_clus, -(-self.bitmap_len // self.csz))[:self.bitmap_len])
    def coff(self, c): return (self.heap_off + (c - 2) * self.spc) * self.bps
    def read_clusters(self, c, n): self.f.seek(self.coff(c)); return self.f.read(n * self.csz)
    def fat(self, c): self.f.seek(self.fat_off * self.bps + c * 4); return struct.unpack('<I', self.f.read(4))[0]
    def read_chain(self, c):  # root dir always uses a FAT chain (NoFatChain is per-file)
        out = b''
        while 2 <= c <= 0xFFFFFFF6:
            out += self.read_clusters(c, 1); c = self.fat(c)
        return out
    def load_upcase(self, raw):
        tbl, i, ch = {}, 0, 0
        vals = struct.unpack('<%dH' % (len(raw) // 2), raw[:len(raw) // 2 * 2])
        while i < len(vals):
            v = vals[i]
            if v == 0xFFFF and i + 1 < len(vals): ch += vals[i + 1]; i += 2; continue  # identity run
            tbl[ch] = v; ch += 1; i += 1
        return tbl
    def up(self, s): return ''.join(chr(self.upcase.get(ord(c), ord(c))) for c in s)
    def bit(self, c): i = c - 2; return (self.bitmap[i >> 3] >> (i & 7)) & 1
    def setbit(self, c, v):
        i = c - 2
        if v: self.bitmap[i >> 3] |= 1 << (i & 7)
        else: self.bitmap[i >> 3] &= ~(1 << (i & 7)) & 0xFF
    def flush_bitmap(self): self.f.seek(self.coff(self.bitmap_clus)); self.f.write(self.bitmap)
    def free_run(self, n):
        run = 0
        for c in range(2, 2 + self.nclus):
            run = run + 1 if not self.bit(c) else 0
            if run == n: return c - n + 1
        raise SystemExit('no contiguous space')
    def write_root(self): self.f.seek(self.coff(self.rootclus)); self.f.write(self.root)  # single-cluster root assumed

def set_checksum(es):
    s = 0
    for i, b in enumerate(es):
        if i in (2, 3): continue
        s = (((s & 1) << 15) | (s >> 1)) + b; s &= 0xFFFF
    return s

def name_hash(upname):
    s = 0
    for b in upname.encode('utf-16-le'):
        s = (((s & 1) << 15) | (s >> 1)) + b; s &= 0xFFFF
    return s

def ts():
    tm = time.localtime()
    return ((tm.tm_year - 1980) << 25) | (tm.tm_mon << 21) | (tm.tm_mday << 16) | (tm.tm_hour << 11) | (tm.tm_min << 5) | (tm.tm_sec // 2)

def put(fs, src, dest):
    data = open(src, 'rb').read(); n = -(-len(data) // fs.csz)
    first = fs.free_run(n) if data else 0
    if data:
        fs.f.seek(fs.coff(first)); fs.f.write(data)
        for c in range(first, first + n): fs.setbit(c, 1)
        fs.flush_bitmap()
    nchunks = -(-len(dest) // 15); t = ts()
    file_e = bytearray(32); file_e[0] = 0x85; file_e[1] = 1 + nchunks
    struct.pack_into('<HIII', file_e, 4, 0x20, t, t, t)
    stream = bytearray(32); stream[0] = 0xC0; stream[1] = 0x03 if data else 0x01  # AllocationPossible | NoFatChain
    stream[3] = len(dest); struct.pack_into('<H', stream, 4, name_hash(fs.up(dest)))
    struct.pack_into('<Q', stream, 8, len(data)); struct.pack_into('<IQ', stream, 20, first, len(data))
    names = b''
    for i in range(nchunks):
        e = bytearray(32); e[0] = 0xC1; e[2:2 + 30] = dest[i * 15:(i + 1) * 15].encode('utf-16-le').ljust(30, b'\x00'); names += e
    es = bytes(file_e) + bytes(stream) + names
    struct.pack_into('<H', file_e, 2, set_checksum(es)); es = bytes(file_e) + es[32:]
    need = len(es) // 32; free = 0
    for off in range(0, len(fs.root), 32):
        free = free + 1 if (fs.root[off] & 0x80) == 0 else 0
        if free == need:
            s = off - (need - 1) * 32; fs.root = fs.root[:s] + es + fs.root[s + len(es):]; fs.write_root(); return
    raise SystemExit('root dir full')

def sets(root):
    off = 0
    while off < len(root):
        e = root[off:off + 32]
        if e[0] == 0: break
        if e[0] in (0x85, 0x05):
            cnt = e[1]; es = root[off:off + 32 * (cnt + 1)]
            name = ''.join(es[i + 2:i + 32].decode('utf-16-le') for i in range(64, len(es), 32) if es[i] & 0x7F == 0x41)
            st = es[32:64]
            yield off, es, name[:st[3]], struct.unpack_from('<I', st, 20)[0], struct.unpack_from('<Q', st, 24)[0], st[1]
            off += 32 * (cnt + 1)
        else: off += 32

def rm(fs, dest):
    for off, es, name, first, size, flags in sets(fs.root):
        if name == dest and es[0] == 0x85:
            r = bytearray(fs.root)
            for i in range(0, len(es), 32): r[off + i] &= 0x7F
            fs.root = bytes(r); fs.write_root()
            if first and flags & 2:
                for c in range(first, first + -(-size // fs.csz)): fs.setbit(c, 0)
                fs.flush_bitmap()
            return
    raise SystemExit('not found')

if __name__ == '__main__':
    fs = Ex(sys.argv[1]); cmd = sys.argv[2]
    if cmd == 'put': put(fs, sys.argv[3], sys.argv[4])
    elif cmd == 'rm': rm(fs, sys.argv[3])
    elif cmd == 'ls':
        for off, es, name, first, size, flags in sets(fs.root):
            print('%s %-30s first=%d size=%d flags=%#x csum_ok=%s' % ('live' if es[0] == 0x85 else 'DEL ', name, first, size, flags, set_checksum(es) == struct.unpack_from('<H', es, 2)[0] if es[0] == 0x85 else '-'))
    fs.f.close()
```

Limitations of the script (fine for fixtures): root directory only, root must fit in its
first cluster (~128 entries), no subdirectory creation (add a `mkdir` that writes an 0x85 set
with attribute 0x10 and a zeroed cluster if a test needs it). exfatprogs' `fsck.exfat` is the
oracle; `dump.exfat` shows the bitmap/upcase/root positions for assertions.

### 5.4 NTFS fixture

```sh
truncate -s 64M ntfs.img && mkntfs -F -Q -L TESTNTFS ntfs.img      # -F mandatory for a regular file
ntfscp ntfs.img photo1.jpg "/Holiday Photo 2024.jpg"              # Win32-namespace long name? ntfscp writes POSIX namespace
ntfscp ntfs.img photo2.png /photo2.png
ntfscp ntfs.img clip.mp4 /clip.mp4
# Realistic deletion + subdirectory, needs /dev/fuse (present on the dev box, works as uid 1000):
mkdir -p mnt && ntfs-3g -o no_def_opts,silent ntfs.img mnt
mkdir mnt/DCIM && cp photo1.jpg mnt/DCIM/IMG_0001.JPG && rm mnt/photo2.png
fusermount3 -u mnt
ntfsfix -n ntfs.img          # "processed successfully"
ntfsundelete -s ntfs.img     # oracle: lists the deleted record with "FN.." and 100% recoverable
ntfsinfo -F /clip.mp4 ntfs.img   # oracle for attribute layout / data runs of a live file
```
Observed: first user file at MFT record 64, record size 1024 (`clusters_per_mft_record` =
0xF6 = -10), 4 KiB clusters (8 sectors), $MFT at LCN 4 (`$DATA` runs `11 13 04 00` = 19
clusters), $MFTMirr at LCN 8191, usa_ofs 0x30 usa_count 3, attrs_offset 0x38.
ntfscp writes the $FILE_NAME with namespace POSIX (0) and a stale FILE_NAME real size of 0 -
a good test that sizes must come from $DATA, not $FILE_NAME.

CI fallback without FUSE: `ntfsdel.py IMG RECNO` (verified: `ntfsundelete -s` then lists
record 64 as deleted; `ntfsls` still shows the name because the $I30 entry is untouched -
the parser must mark such an entry stale by sequence mismatch and still emit the file as
`Deleted`). Note it does not free the file's clusters in $Bitmap, so ntfsundelete reports
0% recoverable for it; add $Bitmap clearing (decode the $DATA runs, clear bits in record 6's
$DATA) if a test needs "clusters free".
```python
#!/usr/bin/env python3
"""ntfsdel.py IMG RECNO - emulate NTFS file deletion without mounting.

Does what ntfs-3g's ntfs_mft_record_free() does to the MFT record: clears
MFT_RECORD_IN_USE (0x0001), bumps the sequence number (0xFFFF wraps to 1) and
clears the record's bit in $MFT:$BITMAP. It does NOT remove the $I30 index entry
from the parent directory (so the parent still names the file: a stale
reference whose sequence number no longer matches - exactly what a recovery
parser must tolerate). It does NOT free the file's clusters in $Bitmap.
Fixups: the flag/sequence words are never in the last 2 bytes of a 512-byte
sector, so no fixup rewrite is needed; the record is otherwise untouched.
"""
import struct, sys

def runs(buf):
    """decode an NTFS data run list -> [(lcn or None, length)]"""
    out, pos, lcn = [], 0, 0
    while buf[pos]:
        h = buf[pos]; ls, os_ = h & 0xF, h >> 4; pos += 1
        ln = int.from_bytes(buf[pos:pos+ls], 'little'); pos += ls
        if os_:
            off = int.from_bytes(buf[pos:pos+os_], 'little', signed=True); pos += os_
            lcn += off; out.append((lcn, ln))
        else: out.append((None, ln))
    return out

def attrs(rec):
    ao = struct.unpack_from('<H', rec, 0x14)[0]
    while True:
        t, l = struct.unpack_from('<II', rec, ao)
        if t == 0xFFFFFFFF or l == 0: return
        yield ao, t, rec[ao:ao+l]; ao += l

img, recno = sys.argv[1], int(sys.argv[2])
f = open(img, 'r+b'); b = f.read(512)
bps = struct.unpack_from('<H', b, 11)[0]; spc = b[13]; csz = bps * spc
mft_lcn = struct.unpack_from('<Q', b, 48)[0]; cpr = struct.unpack_from('<b', b, 64)[0]
rs = (1 << -cpr) if cpr < 0 else cpr * csz

def read_rec(n, mft_runs=None):
    if mft_runs is None: off = mft_lcn * csz + n * rs
    else:
        vcn = n * rs // csz; rem = n * rs % csz
        for lcn, ln in mft_runs:
            if vcn < ln: off = lcn * csz + vcn * csz + rem; break
            vcn -= ln
    f.seek(off); r = bytearray(f.read(rs)); assert r[:4] == b'FILE', 'not a FILE record'
    uo, uc = struct.unpack_from('<HH', r, 4); usn = r[uo:uo+2]
    for s in range(1, uc): assert r[s*512-2:s*512] == usn, 'fixup mismatch (record torn)'
    return off, r

_, r0 = read_rec(0)
mft_runs = bitmap = None
for ao, t, a in attrs(r0):
    if t == 0x80 and a[8] == 1: mft_runs = runs(a[struct.unpack_from('<H', a, 0x20)[0]:])
    if t == 0xB0: bitmap = (ao, a)
off, r = read_rec(recno, mft_runs)
flags, seq = struct.unpack_from('<HH', r, 0x16)[0], struct.unpack_from('<H', r, 0x10)[0]
struct.pack_into('<H', r, 0x16, flags & ~1)
struct.pack_into('<H', r, 0x10, 1 if seq == 0xFFFF else (seq + 1 if seq else 0))
f.seek(off); f.write(r)
ao, a = bitmap
if a[8] == 0:  # resident $BITMAP inside record 0 - patch in place (not in a fixup slot? check)
    vo = struct.unpack_from('<H', a, 0x14)[0]; p = ao + vo + recno // 8
    assert p % 512 < 510, 'bitmap byte sits in a fixup slot; rewrite fixups instead'
    off0, r0 = read_rec(0); r0[p] &= ~(1 << (recno % 8)) & 0xFF; f.seek(off0); f.write(r0)
else:
    rl = runs(a[struct.unpack_from('<H', a, 0x20)[0]:]); byte = recno // 8
    for lcn, ln in rl:
        if byte < ln * csz: f.seek(lcn * csz + byte); v = f.read(1)[0]; f.seek(-1, 1); f.write(bytes([v & ~(1 << (recno % 8)) & 0xFF])); break
        byte -= ln * csz
print('record %d: flags %#x -> %#x, seq %d -> %d, $MFT bitmap bit cleared' % (recno, flags, flags & ~1, seq, struct.unpack_from('<H', r, 0x10)[0]))
```
Damaged variants: zero sector 0 (backup = last sector: `dd if=ntfs.img bs=512 skip=$((size/512-1)) count=1` must validate); corrupt record 64's USN (`printf '\xAA\xBB' | dd of=ntfs.img bs=1 seek=$((4*4096 + 64*1024 + 510)) conv=notrunc`) to exercise torn-record handling; zero record 0 and rely on $MFTMirr at LCN 8191.

### 5.5 ext4 / ext2 fixture (verified)

```sh
mkdir -p src/DCIM && cp photo1.jpg src/DCIM/IMG_0001.JPG && cp photo2.png src/photo2.png && cp clip.mp4 src/clip.mp4
ln -s DCIM/IMG_0001.JPG src/link; python3 -c "open('src/del.txt','w').write('DELETEME\n'*2000)"
truncate -s 32M ext4.img && mke2fs -q -F -t ext4 -d src -L TESTEXT ext4.img      # extents, 64bit, flex_bg, metadata_csum
truncate -s 16M ext2.img && mke2fs -q -F -t ext2 -d src -L TESTEXT2 ext2.img    # block map, 32-byte descriptors (ext2 default inode size is still 256 with 1.47)
debugfs -R 'ls -l /' ext4.img; debugfs -R 'stat /del.txt' ext4.img           # inode 13, EXTENTS (0-4):1581-1585, "Inode checksum: 0x..."
debugfs -w -R 'rm /del.txt' ext4.img                                           # dtime set, links 0, bitmap bit cleared, EXTENTS KEPT
debugfs -R 'lsdel' ext4.img                                                    # oracle: lists inode 13, 5 blocks
# Emulate what the kernel does (2.5.9): size 0, blocks 0, i_block zeroed
for i in $(seq 0 14); do debugfs -w -R "set_inode_field <13> block[$i] 0" ext4.img; done
debugfs -w -R 'set_inode_field <13> size 0' ext4.img; debugfs -w -R 'set_inode_field <13> blocks 0' ext4.img
e2fsck -fn ext4.img                                                            # still clean
dumpe2fs ext4.img | grep -E 'superblock at|Group descriptors at|Inode table at' | head   # backup locations for assertions
```
Verified on the dev box: after `rm`, inode 13 has Links 0, dtime set, extents retained
(debugfs); after the `set_inode_field` loop, i_block is all zero, size 0, e2fsck clean
(the "kernel-like" deleted inode). The `rm`-only image is the "optimistic" deleted inode
the parser must still recover from. Inode checksums printed by debugfs (`0xb5f80595` live,
`0x109ae230` after rm, on the image built on 2026-10-12 - regenerate per build) validate the
crc32c implementation. Name recovery: `del.txt`'s dirent remains in the root directory block
after `rm`, hidden by the previous entry's rec_len (verify with `debugfs -R 'bd <2>'` hexdump).
Damaged variants: zero bytes 1024..2048 (primary superblock) -> backup at group 1
(byte 134217728 for 4 KiB blocks; for the 32 MiB image there is only group 0: make a 256 MiB
sparse image for the backup test, `truncate -s 256M`, mke2fs output stays small on disk);
zero the GDT block; zero one inode table block.

Checksum oracle `ext4csum.py` (pure stdlib; run as `python3 ext4csum.py IMG...`; verified on
2026-10-12 to reproduce dumpe2fs/debugfs values on gdt_csum, metadata_csum and
metadata_csum-without-seed images, see 3.5.1). Its two CRC routines are the reference for
the Rust `csum` module:
```python
#!/usr/bin/env python3
"""ext4csum.py IMG... - recompute ext4 superblock, group-descriptor and inode checksums and
compare with what is on disk and what debugfs prints (crc16 for GDT_CSUM, raw crc32c for
METADATA_CSUM)."""
import struct, sys, subprocess, re

def mk_table(poly):
    t = []
    for i in range(256):
        c = i
        for _ in range(8): c = (c >> 1) ^ (poly if c & 1 else 0)
        t.append(c)
    return t
T32 = mk_table(0x82F63B78)          # CRC-32C (Castagnoli), reflected
T16 = mk_table(0xA001)              # CRC-16 poly 0x8005, reflected (kernel lib/crc/crc16.c)

def crc32c_raw(init, data):         # kernel crc32c(): init as given, NO final xor
    c = init & 0xFFFFFFFF
    for b in data: c = T32[(c ^ b) & 0xFF] ^ (c >> 8)
    return c

def crc16(init, data):              # kernel crc16(): init as given, NO final xor
    c = init & 0xFFFF
    for b in data: c = (c >> 8) ^ T16[(c ^ b) & 0xFF]
    return c

def check(img):
    f = open(img, 'rb'); f.seek(1024); sb = f.read(1024)
    uuid = sb[0x68:0x78]
    incompat, rocompat = struct.unpack_from('<II', sb, 0x60)
    desc_size = struct.unpack_from('<H', sb, 0xFE)[0] if incompat & 0x80 else 32
    bs = 1024 << struct.unpack_from('<I', sb, 0x18)[0]
    fdb = struct.unpack_from('<I', sb, 0x14)[0]
    mcs, gdt, cseed = bool(rocompat & 0x400), bool(rocompat & 0x10), bool(incompat & 0x2000)
    print(img, 'bs', bs, 'desc', desc_size, 'metadata_csum', mcs, 'gdt_csum', gdt, 'csum_seed', cseed)
    seed = None
    if mcs:
        print(' sb csum computed %08x stored %08x' % (crc32c_raw(0xFFFFFFFF, sb[:0x3FC]), struct.unpack_from('<I', sb, 0x3FC)[0]))
        seed = struct.unpack_from('<I', sb, 0x270)[0] if cseed else crc32c_raw(0xFFFFFFFF, uuid)
    f.seek((fdb + 1) * bs); d = f.read(desc_size)
    stored = struct.unpack_from('<H', d, 0x1E)[0]
    if mcs:
        c = crc32c_raw(seed, struct.pack('<I', 0)); c = crc32c_raw(c, d[:0x1E])
        c = crc32c_raw(c, b'\0\0'); c = crc32c_raw(c, d[0x20:desc_size])
        print(' gdt csum computed %04x stored %04x' % (c & 0xFFFF, stored))
    elif gdt:
        c = crc16(0xFFFF, uuid); c = crc16(c, struct.pack('<I', 0)); c = crc16(c, d[:0x1E])
        if incompat & 0x80: c = crc16(c, d[0x20:desc_size])
        print(' gdt crc16 computed %04x stored %04x' % (c, stored))
    if mcs:
        isz = struct.unpack_from('<H', sb, 0x58)[0]
        itab = struct.unpack_from('<I', d, 8)[0] | ((struct.unpack_from('<I', d, 0x28)[0] << 32) if desc_size >= 64 else 0)
        for ino in (2, 7, 8):
            f.seek(itab * bs + (ino - 1) * isz); raw = bytearray(f.read(isz))
            gen = struct.unpack_from('<I', raw, 0x64)[0]; extra = struct.unpack_from('<H', raw, 0x80)[0]
            raw[0x7C:0x7E] = b'\0\0'
            if extra >= 4: raw[0x82:0x84] = b'\0\0'
            c = crc32c_raw(seed, struct.pack('<I', ino)); c = crc32c_raw(c, struct.pack('<I', gen)); c = crc32c_raw(c, bytes(raw))
            out = subprocess.run(['debugfs', '-R', 'stat <%d>' % ino, img], capture_output=True, text=True).stdout
            m = re.search(r'Inode checksum: (0x[0-9a-f]+)', out)
            print(' inode %d csum computed %08x debugfs %s' % (ino, c, m.group(1) if m else '?'))

for img in sys.argv[1:]: check(img)
```

### 5.6 Partitioned disk images (`mkdisk.py`, pure Python)

Wrap the filesystem images into MBR and GPT disks so the partition parser is tested end to
end (sparse files keep them small):
```python
#!/usr/bin/env python3
"""mkdisk.py OUT.img mbr|gpt FS1.img[:type] [FS2.img[:type] ...]  - 1 MiB aligned, 512-byte sectors."""
import struct, sys, uuid, zlib
out, scheme, parts = sys.argv[1], sys.argv[2], sys.argv[3:]
SS = 512; ALIGN = 2048
specs = []; lba = ALIGN
for p in parts:
    path, _, t = p.partition(':'); n = -(-__import__('os').path.getsize(path) // SS); n = -(-n // ALIGN) * ALIGN
    specs.append((path, t or ('07' if scheme == 'mbr' else 'EBD0A0A2-B9E5-4433-87C0-68B6B72699C7'), lba, n)); lba += n
total = lba + ALIGN
f = open(out, 'wb'); f.truncate(total * SS)
for path, _, start, _ in specs:
    f.seek(start * SS); f.write(open(path, 'rb').read())
def mbr(entries):                                   # entries: [(type, start, count)]
    m = bytearray(512); struct.pack_into('<I', m, 0x1B8, 0x12345678)
    for i, (t, s, c) in enumerate(entries[:4]):
        struct.pack_into('<BBBBBBBBII', m, 0x1BE + 16 * i, 0, 0xFE, 0xFF, 0xFF, t, 0xFE, 0xFF, 0xFF, s, c)
    m[0x1FE:0x200] = b'\x55\xAA'; return m
def guid(s): u = uuid.UUID(s); return u.bytes_le
if scheme == 'mbr':
    f.seek(0); f.write(mbr([(int(t, 16), s, n) for _, t, s, n in specs]))
else:
    ents = bytearray(128 * 128)
    for i, (path, t, s, n) in enumerate(specs):
        struct.pack_into('<16s16sQQQ72s', ents, 128 * i, guid(t), uuid.uuid4().bytes_le, s, s + n - 1, 0, ('part%d' % i).encode('utf-16-le'))
    def hdr(my, alt, elba):
        h = bytearray(92); struct.pack_into('<8sIIII', h, 0, b'EFI PART', 0x00010000, 92, 0, 0)
        struct.pack_into('<QQQQ16sQIII', h, 24, my, alt, ALIGN, total - ALIGN - 1, uuid.uuid4().bytes_le, elba, 128, 128, zlib.crc32(ents))
        struct.pack_into('<I', h, 16, zlib.crc32(h)); return h + b'\0' * (SS - 92)
    f.seek(0); f.write(mbr([(0xEE, 1, min(total - 1, 0xFFFFFFFF))]))
    f.seek(1 * SS); f.write(hdr(1, total - 1, 2)); f.seek(2 * SS); f.write(ents)
    f.seek((total - 33) * SS); f.write(ents); f.seek((total - 1) * SS); f.write(hdr(total - 1, 1, total - 33))
f.close()
```
Note the GPT header's `my/alt` and entry-array LBAs are swapped for the backup copy, and the
backup entry array sits at `total-33`. Tests: parse both; zero LBA 1 (primary header) and
expect the backup to be used; zero LBA 0 (protective MBR) and expect no change. Produce an
EBR chain by hand for the extended-partition test: EBR at E with entry 0 = (type, 2048,
n) relative to E and entry 1 = (0x05, E2 - E, n2 + 2048) relative to E (the outer extended
start); second EBR at E2 with entry 1 zero.

### 5.7 Media content and oracles

* Photos: Pillow (`Image.save(... quality=90)` JPEG with JFIF header FF D8 FF E0; PNG
  89 50 4E 47; `im.save('x.tif')` TIFF `II*\0`). Add EXIF via `im.save(..., exif=exif_bytes)`
  (Pillow's `Image.Exif()`), so date-based matching can be tested.
* Videos: ffmpeg `-c:v libx264` MP4 (`ftypisom`), `-c:v libx265` MP4/MOV, `-c:a aac`,
  `-f avi` AVI (`RIFF....AVI `), `-movflags +faststart` to control moov position.
* Oracles for parser assertions: `fsck.vfat -n` (file/cluster counts), `fsck.exfat -n` +
  `dump.exfat`, `ntfsls -l`, `ntfsinfo -F path`, `ntfsundelete -s`, `ntfscat img /path`
  (byte-compare recovered data), `debugfs -R 'ls -l' / 'stat' / 'lsdel' / 'bd <ino>'`,
  `dumpe2fs`. Every recovered Live file is byte-compared with the source file; every Deleted
  file is byte-compared with the source as well (contiguous fixtures make this exact).
* Mapfile integration: generate a ddrescue-style mapfile marking chosen sectors `-` and
  assert the parser reports the right entries as `partial`/`Unreadable` without aborting.

---

## 6. Recommended Rust crates

Versions are the current stable releases on crates.io as of 2026-10-12 (queried via the
crates.io API on the dev box). Core recommendation: **write the parsers in-house** (the
published filesystem crates expose only live files, do not surface deleted entries, fixup
failures or partial reads, and bail on corruption); use the crates below for layout plumbing,
checksums, and as *test oracles*.

| Purpose | Crate | Version | Notes |
|---|---|---|---|
| Zero-copy struct views over bytes (`FromBytes`, `Unaligned`, `little_endian::U32`) | `zerocopy` (+ `zerocopy-derive`) | 0.8.62 | preferred for all on-disk structs; no unsafe in our code |
| Declarative binary parsing where layouts are variable (data runs, dir entries) | `binrw` | 0.15.2 | optional; zerocopy + hand parsing is enough |
| CRC-32 (GPT) | `crc32fast` | 1.5.2 | zlib-compatible; SIMD |
| CRC-32C (ext4 metadata_csum) | `crc32c` | 0.6.8 | hardware accelerated; see 3.5.1 note on raw vs finalised values |
| CRC-16 (ext4 gdt_csum) and generic CRC algorithms | `crc` | 3.4.0 | use `crc::CRC_16_MODBUS` (poly 0x8005, init 0xFFFF, refin/refout, xorout 0) - matches the kernel's `crc16(~0, ...)` exactly (verified against dumpe2fs, 3.5.1); a 30-line table implementation is also fine |
| GUIDs (GPT types, mixed-endian) | `uuid` | 1.28.0 | `Uuid::from_bytes_le` for on-disk order |
| UTF-16 names (FAT LFN, exFAT, NTFS) | `widestring` | 1.2.1 | lossy decoding of unpaired surrogates |
| OEM code pages for FAT short names (CP437/850/932) | `encoding_rs` | 0.8.42 | covers Shift-JIS (932); CP437 is not in encoding_rs - embed a 256-entry table |
| Memory-mapped image access | `memmap2` | 0.9.11 | for images; devices use positional reads (`FileExt::read_at`) |
| Unix device ioctls (BLKGETSIZE64, BLKSSZGET) | `nix` | 0.31.3 | imaging engine side; parsers stay `std::io` only |
| MBR parsing (oracle / cross-check) | `mbrman` | 0.6.1 | pure Rust |
| GPT parsing (oracle / cross-check) | `gpt` | 4.1.0 | pure Rust |
| FAT reading (oracle in tests) | `fatfs` | 0.3.6 | no deleted entries, no_std capable |
| NTFS reading (oracle in tests) | `ntfs` | 0.4.0 | pure Rust, applies fixups, resident/non-resident, index walking; no deleted records |
| ext2/3/4 reading (oracle in tests) | `ext4-view` | 1.0.0 | pure Rust, read-only, no_std; strong for byte-comparison of live files |
| exFAT reading | `exfat` | 0.1.0 | unmaintained (2023), do not depend on it; our own parser is required anyway |

Checksum helpers should be wrapped in one `csum` module with unit tests pinned to values from
the fixtures (GPT header CRC from `mkdisk.py`, ext4 inode checksum from `debugfs stat`, exFAT
boot checksum from sector 11 of a mkfs.exfat image, exFAT set checksum from a live set).

---

## 7. Sources

Primary specifications and source code consulted (fetched 2026-10-12 unless noted):

* Microsoft exFAT file system specification - https://learn.microsoft.com/en-us/windows/win32/fileio/exfat-specification (boot sector, checksums, directory entry types, NameHash, flags, timestamps)
* Microsoft FAT32 File System Specification (fatgen103) - https://download.microsoft.com/download/1/6/1/161ba512-40e2-4cc9-843a-923143f3456c/fatgen103.doc ; PDF mirror read in full via pdftotext on 2026-10-12: https://academy.cba.mit.edu/classes/networking_communications/SD/FAT.pdf (BPB_ExtFlags bits, 3.6 "Backup BPB Structure": sector 6 holds a copy of all three boot sectors incl. FSInfo, 4.2 FAT[0]/FAT[1] and ClnShutBitMask/HrdErrBitMask 0x8000/0x4000 and 0x08000000/0x04000000, EOC/bad values, 4085/65525 thresholds, FSInfo offsets 0/484/488/492/508, DIR_CrtTimeTenth 0..199, DIR_FstClusHI "must be set to 0 on volumes formatted FAT12/FAT16", LAST_LONG_ENTRY 0x40, ChkSum loop)
* Wikipedia, "Design of the FAT file system" - https://en.wikipedia.org/wiki/Design_of_the_FAT_file_system (BPB/EBPB, FSInfo, FAT entry values, directory and LFN entries, checksum, time/date)
* Wikipedia, "Master boot record" - https://en.wikipedia.org/wiki/Master_boot_record
* Wikipedia, "GUID Partition Table" - https://en.wikipedia.org/wiki/GUID_Partition_Table (header/entry layout, EFI/MS basic data/Linux fs/swap GUIDs)
* UEFI Specification 2.10, section 5 "GUID Partition Table (GPT) Disk Layout" - https://uefi.org/specs/UEFI/2.10/05_GUID_Partition_Table_Format.html (HTTP 403 at fetch time on 2026-10-12, twice; layout, attribute bits and all type GUIDs taken from Wikipedia which mirrors it; the `128 * 2^n` SizeOfPartitionEntry rule confirmed from the spec text as reproduced in EDK2 `MdePkg/Include/Uefi/UefiGpt.h` and the OSDev GPT page: "shall be set to a value of 128 x 2^n where n is an integer greater than or equal to zero")
* UAPI Group, "Discoverable Partitions Specification" - https://uapi-group.org/specifications/specs/discoverable_partitions_specification/ (Linux /home, swap, root x86-64, generic data and ESP GUIDs)
* GNU ddrescue manual, "Mapfile structure" - https://www.gnu.org/software/ddrescue/manual/ddrescue_manual.html (block statuses `?` `*` `/` `-` `+`, status line `pos status pass`, `#` comments)
* ntfs-3g `libntfs-3g/bootsect.c` - https://raw.githubusercontent.com/tuxera/ntfs-3g/edge/libntfs-3g/bootsect.c (sectors_per_cluster: powers of two or 240..253 decoded as `1 << (256 - v)`; clusters_per_mft_record negative = `1 << -c`)
* ntfs-3g `libntfs-3g/dir.c` - https://raw.githubusercontent.com/tuxera/ntfs-3g/edge/libntfs-3g/dir.c (`ntfs_link_i` increments link_count per $FILE_NAME incl. DOS names)
* Linux `fs/ntfs3/super.c` and `fs/ntfs3/record.c` - https://raw.githubusercontent.com/torvalds/linux/master/fs/ntfs3/super.c , .../record.c (`true_sectors_per_clst`: <= 0x80 plain, >= 0xF4 negative; record/index size negative encoding; `mi_format_new` bumps the sequence number on reuse)
* Linux `fs/fat/dir.c`, `fs/fat/inode.c`, `fs/fat/namei_vfat.c` - https://raw.githubusercontent.com/torvalds/linux/master/fs/fat/dir.c , .../inode.c , .../namei_vfat.c (`fat_remove_entries` writes only name[0] = 0xE5; `fat_detach` sets i_pos = 0 so the truncated inode is never written back: FstClusHI survives on Linux)
* Linux `fs/exfat/exfat_raw.h` and `fs/exfat/fatent.c` - https://raw.githubusercontent.com/torvalds/linux/master/fs/exfat/exfat_raw.h , .../fatent.c (entry type codes incl. vendor 0xE0/0xE1; `exfat_free_cluster` clears bitmap bits only, never FAT entries)
* Linux `fs/ext4/ext4.h`, `fs/ext4/super.c`, `lib/crc/crc16.c` - https://raw.githubusercontent.com/torvalds/linux/master/fs/ext4/ext4.h , .../fs/ext4/super.c , .../lib/crc/crc16.c (`ext4_rec_len_from_disk`, bg_flags, inode flags; `ext4_group_desc_csum`, `ext4_superblock_csum`; CRC-16 poly 0x8005 reflected table)
* Linux `fs/ext2/inode.c` - https://raw.githubusercontent.com/torvalds/linux/master/fs/ext2/inode.c (`ext2_evict_inode`: dtime + sync_inode_metadata before truncation; `ext2_free_data`/`ext2_free_branches` zero pointers only in memory)
* Empirical checksum verification 2026-10-12 (Python, dev box): ext4 gdt_csum/crc16, superblock/descriptor/inode crc32c with both seed forms, exFAT boot-region checksum (sector 11 == sector 23 region, value repeated bps/4 times) and up-case table checksum 0xE619D30D (5836-byte table) all reproduced bit-exactly from mke2fs 1.47.2 / mkfs.exfat 1.3.2 images.
* ntfs-3g `include/ntfs-3g/layout.h` - https://raw.githubusercontent.com/tuxera/ntfs-3g/edge/include/ntfs-3g/layout.h (boot sector, MFT_RECORD, flags, attribute types, STANDARD_INFORMATION, FILE_NAME namespaces, index flags, MREF/MSEQNO, system files)
* ntfs-3g `libntfs-3g/mft.c` - https://raw.githubusercontent.com/tuxera/ntfs-3g/edge/libntfs-3g/mft.c (`ntfs_mft_record_free`: in-use flag, sequence number wrap, $MFT bitmap; `ntfs_mft_record_layout`: usa_ofs/usa_count/attrs_offset)
* linux-ntfs documentation (flatcap) - https://flatcap.github.io/linux-ntfs/ntfs/concepts/file_record.html , .../concepts/attribute_header.html , .../concepts/data_runs.html , .../concepts/file_reference.html , .../concepts/index_record.html , .../concepts/compression.html , .../attributes/standard_information.html , .../attributes/file_name.html , .../attributes/index_root.html , .../attributes/attribute_list.html , .../files/boot.html , .../files/index.html
* Linux kernel documentation, "ext4 Data Structures and Algorithms" - https://docs.kernel.org/filesystems/ext4/super.html , https://docs.kernel.org/filesystems/ext4/inodes.html , https://docs.kernel.org/filesystems/ext4/directory.html , https://docs.kernel.org/filesystems/ext4/ifork.html , https://docs.kernel.org/filesystems/ext4/blockmap.html , https://docs.kernel.org/filesystems/ext4/overview.html
* Linux kernel source `fs/ext4/extents.c` (`ext4_ext_rm_leaf`, `ext4_ext_remove_space`) - https://raw.githubusercontent.com/torvalds/linux/master/fs/ext4/extents.c
* Linux kernel source `fs/ext4/inode.c` (`ext4_evict_inode`: i_size = 0, truncate, dtime, free) - https://raw.githubusercontent.com/torvalds/linux/master/fs/ext4/inode.c
* FAT32 high-word zeroing on Windows delete: Forensic Focus forum thread "deleted fat files first cluster addr high word gets cleared" - https://www.forensicfocus.com/forums/general/deleted-fat-files-first-cluster-addr-high-word-gets-cleared/ ; Apriorit, "File system image data recovery" - https://www.apriorit.com/dev-blog/220-file-system-image-data-recovery
* crates.io API (versions in section 6) - https://crates.io/api/v1/crates/<name>
* Empirical observations: images produced on the dev box with mkfs.fat 4.2, exfatprogs 1.3.2, ntfs-3g 2022.10.3, e2fsprogs 1.47.2 (section 5), 2026-10-12.

Not consulted (unavailable or not fetched): Brian Carrier, "File System Forensic Analysis"
(the NTFS index-slack statement and the Windows sequence-number timing, both still tagged
UNVERIFIED, come from general forensic knowledge usually attributed to it; secondary web
sources agree that Windows bumps the sequence number on deallocation); the osdev.org FAT page
and uefi.org (HTTP 403). Windows' exFAT FAT-chain behaviour on delete (2.3.3) is the only
other remaining UNVERIFIED item; the Linux side of every "driver behaviour" claim has been
checked against kernel source.
