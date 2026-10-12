# DiskRescue — File-Carving Engine Specification

Status: design spec, implementation-ready. Lines tagged `UNVERIFIED` could not be confirmed
against a primary source during research and must be checked before the behaviour they describe
is relied upon in tests.

---

## 1. Overview and scope

### 1.1 What the carver is for

The carver is the last-resort recovery path. It runs on a **block image** (the output of the
imaging engine, or a mapfile-backed view of it), never on the source device, and finds photo,
video and audio files by content alone, with no help from any filesystem metadata. It is used when:

- the partition table / filesystem is unparseable (reformatted card, zeroed MBR/GPT);
- the filesystem parser found the directory but the allocation chains are broken;
- the user wants *deleted* media whose directory entries are gone (unallocated-only mode);
- the filesystem parser recovered files, and the user wants "everything else" (dedup against the
  filesystem-recovered set).

The carver's contract:

1. Input: a `Source` (positioned reads, known length, optional list of unreadable ranges from the
   ddrescue-style mapfile, optional allocation bitmap from the filesystem module).
2. Output: carved files written under `<out>/carved/…`, plus a JSON report (section 3.8) with one
   record per carved object and one record per rejected candidate when `--report-rejects` is on.
3. Every byte of the image is attributed to at most one carved object (section 3.3).
4. Memory is bounded independent of image size and of the largest file (section 3.5).
5. Output order is deterministic: ascending by image offset (section 3.5).

### 1.2 Prior art the design is built on

**PhotoRec** (Grenier, CGSecurity). Signature table where each format is a `file_hint_t`
(`extension`, `description`, `max_filesize`, `recover`, `enable_by_default`,
`register_header_check`). A header check registers (magic bytes, offset) pairs and a callback that
fills a `file_recovery_t` (`calculated_file_size`, `data_check`, `file_check`, `file_rename`,
`offset_ok`, `offset_error`, `min_filesize`, `blocksize`, `extra`). Three hooks:

- `header_check(buffer)` – cheap structural check on the first buffer; may set
  `calculated_file_size` directly when the header states the size (BMP, RIFF, PSD, RAF).
- `data_check(buffer)` – called as more blocks are read; walks the structure (JPEG markers, PNG
  chunks, MP4 atoms, MP3 frames, EBML elements, TS packets) and returns `DC_CONTINUE` /
  `DC_STOP` / `DC_ERROR` (`DC_SCAN` = 0 also exists). This is PhotoRec's "end detection for
  formats without an end marker".
- `file_check(file)` – post-pass validation on the already-written file (e.g. JPEG decode via
  libjpeg, TIFF IFD walk to compute the true end); can truncate the file or reject it.

Block size: "During pass 0, PhotoRec searches the first 10 files to determine the blocksize". The
algorithm (`find_blocksize` in `photorec.c`) starts at `128*512` = 64 KiB and halves while any
*validated* file start (`file_stat != NULL`) has a different `start % blocksize` than the first
one, with a floor at the sector size; `update_blocksize` then re-aligns the search-space ranges to
that block size and offset. Thumbnails: `header_check_jpg` rejects a standalone JPEG as a likely
thumbnail when `file_size <= 16384 && buffer[3]==0xe0 && width<200 && height<200`; and for a real
JPEG, `jpg_check_app1` extracts the EXIF thumbnail offset so the embedded SOI inside APP1 is not
treated as a new file. Search scope: "whole partition (useful if the filesystem is corrupted) or
unallocated space only (available for ext2/ext3/ext4, FAT12/FAT16/FAT32 and NTFS)". Expert mode
lets the user "force the file system block size and the offset"; other options: Paranoid
(`yes` = verify and reject invalid, `no`, `bruteforce` = "recover more fragmented JPEG files,
very CPU intensive"), Keep corrupted files, Low memory. Max file sizes: JPEG `50*1024*1024`,
FLAC `1500*1024*1024`, most others `PHOTOREC_MAX_FILE_SIZE = (1<<41)-1`.

**Foremost / Scalpel** (Richard & Roussev 2005). Pure header/footer carving driven by a config
file (extension, case-sensitivity, max size, header string, optional footer, REVERSE/NEXT rules).
Scalpel makes exactly two sequential passes in 10 MB chunks: pass 1 indexes every header and
footer position; pass 2 replays "work queues" (`STARTCARVE`, `STARTSTOPCARVE`, `CONTINUECARVE`,
`STOPCARVE`) per chunk so each chunk is read once and written straight from the read buffer.
Foremost 0.69 instead re-reads from each header up to the max carve size; measured read
amplification *k* "was between 1 (for carving an empty disk image) and 45". Scalpel `-r` keeps only
the first of overlapping headers; `-b` (Foremost compatibility) truncates at max size when no
footer is found. **False positives**: on an 8 GB drive all of Scalpel, Foremost and WinHex carved
exactly 4,817 "GIF" files of which "approximately 400 … could not be viewed and were likely not
actually GIF files (e.g., they were binary garbage with appropriate 'GIF' headers and footers)";
FTK 1.50b carved 2,442 "GIF" files of exactly 783 bytes, none viewable. Header/footer carving has
no notion of structure, so (a) any footer belonging to a *later* file is paired with an earlier
header, (b) files without footers are carved to max size, (c) garbage between header and footer is
never noticed.

**Garfinkel 2007** ("Carving contiguous and fragmented files with fast object validation",
DFRWS 2007). Corpus: Sleuth Kit identified active file systems on 449 of the drive images; 324
drives contained more than five files; 2,204,139 files with names, 2,143,553 with data, 892 GB.
Fragmentation statistics (Table 1–3, quoted):

| Statistic | Value |
|---|---|
| Files fragmented overall | 125,659 of 2,004,183 = **6%** |
| Drives with no fragmented file at all | 145 of 324 (≈ half) |
| Drives with > 10% of files fragmented | 30 |
| JPEG files in corpus / fragmented | 108,539 / **16%** (2,999 two-frag, 400 three-frag, 13,973 > 3) |
| AVI | 998 files, **20%** fragmented |
| MPEG | 168 files, **17%** |
| DOC | 17%; PST 58%; TMP 66%; LOG 31%; PNG 5%; BMP 8%; GIF 8%; WAV 9% |
| Worst drive (#1028, 14 GB) | 43% of 2,517 JPEGs fragmented |
| FAT (219 FS): contiguous 1,286,459; 2 frags 25,154; 3 frags 4,932; 4: 2,473 | |
| NTFS (51 FS): contiguous 521,663; 2 frags 22,984; 3: 6,474 | |

Gap sizes between the two pieces of **bifragmented** files (Table 4/5/8): the most common gaps are
4,096 B (4,272 files), 8,192 B (1,535), 16,384 B (1,344), 2 B (921), 32,768 B (817); for JPEG
specifically 2 B (99), 4,096 B (88), 16,384 B (40), 8,192 B (38), 651,264 B (38). Expressed in
allocation blocks: 8 blocks (4,327 files), 32 (1,519), 16 (1,431), 0 (697), 64 (649), 24 (470),
40 (398), 48 (328), 96 (296), 80 (277). "The gaps tended to represent 1, 2, 4 or 8 512-byte
sectors" clusters already allocated to another file. Design consequences: (i) contiguous carving
alone recovers ≥ 84% of JPEGs; (ii) a bifragment search window of 64–128 clusters covers almost all
observed gaps; (iii) three-plus fragment files exist but are a minority of fragmented files and are
out of scope for v1.

Garfinkel's validation ladder: header/footer → container structures → decompression → semantic
→ manual, each cheaper than the next; "all of the contiguous JPEGs in a disk image file can
frequently be found as quickly as the file can be loaded into memory". Validator return values:
`V_OK`, `V_ERR`, `V_EOF` (ran out of data before a decision — important for truncated files),
`object_length`. Validator flags that this spec adopts (section 3.2): `err_is_prefix`,
`appended_data_ignored`, `no_zblocks`, `plaintext_container`, `length_function`,
`offset_function`. Bifragment Gap Carving: with `s1` and `e2` known (header sector, footer sector),
try every gap size `g` from 1 to `e2 - s1` and for every `g` every consistent `(e1, s2)`; O(n²)
per object, O(n⁴) for all objects of a type in an image. Warning that drives the JPEG design:
"the JPEG decompressor will frequently decompress corrupt data for many sectors before detecting an
error" — in the DFRWS 2006 Mars image the true fragment ended at sector 31,752 but libjpeg erred at
31,761, decoding 9 sectors of garbage "as valid data"; the 6 contiguous JPEGs of the challenge were
carved in 6 s with no false positives.

**SmartCarving / sequential hypothesis testing** (Pal, Sencar, Memon, DFRWS 2008; also US patent
application 2010/0257146). Three phases: preprocessing (decrypt/decompress, remove blocks belonging
to allocated files), collation (classify each block by type: JPEG entropy data, text, zeros, …),
reassembly. Fragmentation point detection: starting from the header block, test successive block
pairs with a Wald sequential probability ratio test. `H0`: blocks `b1..bn` belong in sequence to the
fragment; `H1`: they do not. Statistic `Λ(Y) = Π_i Pr(Y_i|H1) / Pr(Y_i|H0)` where `Y_i` is a
match metric between adjacent blocks (for JPEG: decoder behaviour and pixel-boundary continuity at
the block transition). Thresholds `τ+ = (1 − P_fe) / P_fa` and `τ− = P_fe / (1 − P_fa)` where
`P_fa` = tolerated false-addition rate and `P_fe` = tolerated false-elimination rate. Decide `H1`
(fragmentation point found) when `Λ > τ+`, `H0` (keep merging) when `Λ < τ−`, otherwise read
another block. Detected fragmentation points then constrain the greedy Parallel Unique Path (PUP)
reassembler. Reported result: on the DFRWS 2006 and 2007 challenge images SmartCarving "was able
to recover all but one fragmented JPEG file" (forensics.wiki summary). The exact per-image counts
and the match-metric distributions used in the paper could not be retrieved (PDF 403). UNVERIFIED

### 1.3 Throughput expectations

| Stage | Expected rate | Basis |
|---|---|---|
| Sequential read, SATA SSD image | ~500 MB/s → 1 TB in 2,000 s ≈ **33 min** | arithmetic |
| Sequential read, 7200 rpm HDD image | ~150 MB/s → 1 TB in ≈ 1 h 51 min | arithmetic |
| Sequential read, NVMe image | ~3 GB/s → 1 TB in ≈ 5.5 min (CPU-bound instead) | arithmetic |
| `memchr`/`memchr2`/`memchr3` (AVX2) | third-party benchmark reports 5.9 GB/s (ASCII) – 8.7 GB/s (UTF-8) on Intel and 6.4 – 13.2 GB/s on ARM for large haystacks; in-cache figures are higher | memchr_vs_stringzilla repo |
| `memchr::memmem::Finder` (single needle, rare-byte prefilter) | same order as memchr on data where the chosen rare byte is actually rare; degrades toward ~1 GB/s on data where it is common | UNVERIFIED |
| `aho-corasick` with Teddy packed prefilter (≤ ~100 short patterns, x86_64/aarch64) | "quite a bit faster than Aho-Corasick (sometimes by an order of magnitude)"; expect multiple GB/s | docs.rs/aho-corasick packed module; absolute figure UNVERIFIED |
| `aho-corasick` DFA fallback (no prefilter) | ~0.5–1.5 GB/s per core | UNVERIFIED |
| Baseline JPEG Huffman decode (validation) | ~100–300 MB/s per core | UNVERIFIED |
| BLAKE3 (AVX2, multithreaded for large inputs) | several GB/s per core | UNVERIFIED |

Conclusion: the signature scan is I/O-bound on every realistic source. CPU cost is dominated by
validators (JPEG decode, MP4 box walking across large mdat skips, TS packet runs), which is why
measurement is parallelised (section 3.5) and the scanner is a single thread.

---

## 2. Exact on-disk details

### 2.1 Signature table

Conventions: offsets are relative to the candidate file start. `??` = any byte. Min size is the
smallest object the validator will accept; max cap is the hard ceiling after which an object is
truncated with status `capped`. Priority: higher wins in the overlap resolver (section 3.3);
containers that embed other formats outrank their embeddees. End-detection methods are defined in
section 2.2.

| # | Name | Ext(s) | Magic (hex) | Off | Min | Max cap | End detection | Pri |
|---|---|---|---|---|---|---|---|---|
| 1 | JPEG / JFIF / EXIF | jpg (jpeg, mpo) | `FF D8 FF` + marker byte ∈ {C0..CF\C4,C8,CC, C4, CC, DB, DD, E0..EF, FE} | 0 | 1 KiB | 256 MiB | marker walk → entropy scan → `FF D9`; decode validation; MPO index | 50 |
| 2 | PNG | png | `89 50 4E 47 0D 0A 1A 0A` | 0 | 67 B | 1 GiB | chunk walk to `IEND` (+CRC) | 50 |
| 3 | GIF | gif | `47 49 46 38 37 61` / `47 49 46 38 39 61` | 0 | 35 B | 256 MiB | block walk to `3B` | 50 |
| 4 | BMP | bmp | `42 4D` + sanity (DIB size ∈ {12,40,52,56,64,108,124}, reserved=0, offset<size) | 0 | 65 B | 1 GiB | header size field | 30 |
| 5 | WebP | webp | `52 49 46 46 ?? ?? ?? ?? 57 45 42 50` | 0 | 20 B | 4 GiB | RIFF size + 8 | 50 |
| 6 | TIFF | tif (tiff) | `49 49 2A 00` / `4D 4D 00 2A` | 0 | 8 B | 4 GiB | IFD walk, max(strip/tile/JPEG end) | 55 |
| 7 | Canon CR2 | cr2 | `49 49 2A 00 10 00 00 00 43 52 02 00` | 0 | 64 KiB | 4 GiB | IFD walk incl. IFD3 raw strips | 70 |
| 8 | Nikon NEF | nef | `4D 4D 00 2A 00 00 00 08` (also `49 49 2A 00`) + Make `NIKON` | 0 | 64 KiB | 4 GiB | IFD walk incl. SubIFDs | 70 |
| 9 | Sony ARW/SR2 | arw | `49 49 2A 00 08 00 00 00` + Make `SONY` | 0 | 64 KiB | 4 GiB | IFD walk incl. SubIFDs | 70 |
| 10 | Adobe DNG | dng | TIFF magic + tag `0xC612` DNGVersion present in IFD0 | 0 | 8 KiB | 4 GiB | IFD walk incl. SubIFDs | 72 |
| 11 | Pentax PEF | pef | `4D 4D 00 2A` + Make `PENTAX` (or `RICOH IMAGING`) UNVERIFIED | 0 | 64 KiB | 4 GiB | IFD walk | 70 |
| 12 | Olympus ORF | orf | `49 49 52 4F 08 00 00 00` / `49 49 52 53 08 00 00 00` / `4D 4D 4F 52 00 00 00 08` | 0 | 64 KiB | 4 GiB | IFD walk (TIFF LE/BE with swapped magic) | 70 |
| 13 | Panasonic RW2 | rw2 | `49 49 55 00 18 00 00 00` | 0 | 64 KiB | 4 GiB | IFD walk; raw strip has no byte count → cap/next-header | 70 |
| 14 | Fuji RAF | raf | `46 55 4A 49 46 49 4C 4D 43 43 44 2D 52 41 57 20` ("FUJIFILMCCD-RAW ") | 0 | 108 B | 4 GiB | header offsets: max(jpg, cfa_header, cfa end) | 70 |
| 15 | HEIF/HEIC/AVIF | heic (heif, avif) | `?? ?? ?? ?? 66 74 79 70` + major brand ∈ {heic, heix, hevc, hevx, heim, heis, hevm, hevs, mif1, msf1, avif, avis, avio} | 0 (magic at 4) | 256 B | 1 GiB | ISOBMFF box walk | 60 |
| 16 | Canon CR3 | cr3 | `00 00 00 18 66 74 79 70 63 72 78 20` ("ftypcrx ") | 0 | 64 KiB | 4 GiB | ISOBMFF box walk (+CTBO cross-check) | 70 |
| 17 | Photoshop PSD/PSB | psd (psb) | `38 42 50 53 00 01` (PSB: `00 02`) | 0 | 26 B | 4 GiB (PSB 64 GiB) | section lengths + image data computed | 55 |
| 18 | MP4/MOV/3GP/M4V/M4A | mp4 (mov, 3gp, 3g2, m4v, m4a, m4b) | `?? ?? ?? ?? 66 74 79 70` + major brand (see 2.2.8) ; also `?? ?? ?? ?? 6D 6F 6F 76` ("moov"), `?? ?? ?? ?? 6D 64 61 74` ("mdat"), `?? ?? ?? ?? 77 69 64 65` ("wide"), `?? ?? ?? ?? 73 6B 69 70` ("skip"), `?? ?? ?? ?? 66 72 65 65` ("free") for brand-less QuickTime | 0 (magic at 4) | 64 B | 64 GiB | ISOBMFF box walk; `mdat` size 0 or missing `moov` → unknown length | 60 |
| 19 | AVI | avi | `52 49 46 46 ?? ?? ?? ?? 41 56 49 20` | 0 | 56 B | 64 GiB | RIFF size + 8, then chained `RIFF…AVIX` (OpenDML) | 62 |
| 20 | Matroska / WebM | mkv (webm, mka) | `1A 45 DF A3` | 0 | 64 B | 64 GiB | EBML header + Segment size; unknown-size → level-1 element walk | 60 |
| 21 | MPEG-TS | ts | `47` at 0, 188, 376, … (≥ 8 packets) | 0 | 1,504 B | 64 GiB | sync-run walk | 20 |
| 22 | MTS / M2TS (BDAV) | mts (m2ts) | `?? ?? ?? ?? 47` at 4, 196, 388, … (≥ 8 packets) | 0 | 1,536 B | 64 GiB | sync-run walk (192-byte stride) | 25 |
| 23 | MPEG-PS / VOB | mpg (vob) | `00 00 01 BA` + pack header check (byte 4 & 0xC4 == 0x44 MPEG-2, or byte 4 & 0xF1 == 0x21 MPEG-1) | 0 | 2 KiB | 64 GiB | pack/PES walk to `00 00 01 B9` or invalid start code | 25 |
| 24 | MPEG video ES | mpg | `00 00 01 B3` (sequence header) | 0 | 4 KiB | 64 GiB | start-code walk | 15 |
| 25 | MP3 (ID3v2) | mp3 | `49 44 33` + version ∈ {02,03,04} + `00` + flags + 4 syncsafe bytes (each < 0x80) | 0 | 1 KiB | 512 MiB | ID3v2 size, then frame walk | 40 |
| 26 | MP3 (bare frames) | mp3 | `FF` + (b1 & 0xE6) ∈ {E2,E3?…} — i.e. b1 & 0xE0 == 0xE0, version ≠ 01, layer ≠ 00, bitrate idx ∉ {0,15}, samplerate idx ≠ 3, + ≥ 3 consecutive consistent frames | 0 | 1 KiB | 512 MiB | frame walk (+Xing/Info frame count, ID3v1/APE tail) | 10 |
| 27 | WAV | wav | `52 49 46 46 ?? ?? ?? ?? 57 41 56 45` ; RF64: `52 46 36 34 ?? ?? ?? ?? 57 41 56 45` | 0 | 44 B | 4 GiB (RF64 64 GiB) | RIFF size + 8; `data` size 0xFFFFFFFF → unknown | 40 |
| 28 | FLAC | flac | `66 4C 61 43 00` (`fLaC` + STREAMINFO block byte 0x00 or 0x80) | 0 | 42 B | 2 GiB | metadata walk, then frame-sync walk with CRC-8 / size bound from STREAMINFO | 40 |

Notes on the table:

- Rows 7–13 share the TIFF magic with row 6. The scanner emits one candidate at the offset; the
  TIFF family validator decides the concrete format (section 2.2.5). The table lists them
  separately only for the report's `format` field and priorities.
- Row 18 `moov`/`mdat`/`wide`/`free`/`skip` first-box signatures are for QuickTime files with no
  `ftyp` (old cameras, some screen recorders). They produce many false candidates on random data
  (4 ASCII bytes preceded by a plausible size); the validator requires the box size to be ≥ 8 and
  a second valid box to follow.
- Row 21/22 and 23/24/26 have very short magics; they are only accepted at sector-aligned offsets
  (section 3.1) and only after the run/consistency checks succeed.
- Audio formats are "in passing": enabled by default but with `priority` low enough that any
  photo/video container claiming the same bytes wins.

### 2.2 Per-format structure needed for end detection

#### 2.2.1 JPEG (ITU-T T.81, JFIF 1.02, EXIF 2.32, CIPA DC-007 MPO)

Marker = `FF xx`. `xx ∈ {00}` is a stuffed byte in entropy data; `FF FF` is fill (skip).
Stand-alone markers (no length): `D0..D7` RSTn, `D8` SOI, `D9` EOI, `01` TEM. All other markers
are followed by a 16-bit big-endian length `L` that includes the two length bytes.

| Marker | Name | Payload layout used by the validator |
|---|---|---|
| `FF D8` | SOI | — |
| `FF E0` | APP0 | `L`, "JFIF\0" (file offset 6..11 when first), ver(2), units(1), Xdensity(2), Ydensity(2), Xthumb(1), Ythumb(1), RGB thumb |
| `FF E1` | APP1 | `L`, "Exif\0\0" (offset 6..11), TIFF header at offset 12: `II*\0`/`MM\0*`, IFD0 offset u32. IFD1 holds tag `0x0201` JPEGInterchangeFormat (thumbnail JPEG offset relative to TIFF header, i.e. file offset = 12 + value) and `0x0202` JPEGInterchangeFormatLength. Also APP1 "http://ns.adobe.com/xap/1.0/\0" = XMP. |
| `FF E2` | APP2 | "ICC_PROFILE\0" or "MPF\0" (MPO: MP index IFD; offsets in MP entries are relative to the byte after "MPF\0"; `MPEntry` = 16 bytes: attr u32, size u32, offset u32, dep1 u16, dep2 u16) |
| `FF C4` | DHT | tables; `Tc` (class 0/1) high nibble, `Th` (0..3) low nibble; 16 count bytes; sum(counts) symbols ≤ 256 |
| `FF DB` | DQT | `Pq` (0/1) high nibble, `Tq` (0..3) low; 64 or 128 bytes |
| `FF DD` | DRI | `L=4`, restart interval u16 |
| `FF C0/C1/C2` | SOF0/1/2 | `P`(1)=8 (12 for C1/C2 allowed), `Y`(2), `X`(2), `Nf`(1) ∈ {1,3,4}, then `Nf` × (Ci, Hi<<4\|Vi, Tqi). X,Y ∈ 1..65535. |
| `FF DA` | SOS | `Ns`(1), `Ns` × (Cs, Td<<4\|Ta), Ss, Se, Ah<<4\|Al; followed by entropy data |
| `FF D9` | EOI | — |

End detection (`measure`):

```
pos = 2                              // after SOI
loop: read marker at pos; require 0xFF; skip 0xFF fill
  m = byte; if m == 0xD9: END, len = pos+2
  if m in RST/TEM/SOI: FAIL (not valid here)
  L = be16; if L < 2: FAIL; validate payload per table; pos += 2 + L
  if m == 0xDA: scan entropy: find next 0xFF not followed by 0x00 or D0..D7 (or 0xFF fill)
       → pos at that marker; progressive files loop back (more DHT/SOS/DRI); EOI ends.
```
After EOI: allow a trailer only when identified (Samsung `SEFT`/`SEFH` trailer, Google
"MotionPhoto"/"MicroVideo" XMP → an MP4 `ftyp` immediately follows; keep both as one object of
format `jpg` with `metadata.trailer = "motion_photo"` and also record the nested MP4 range). Any
other bytes after EOI are not part of the file (`appended_data_ignored = true`).

Validation levels: (1) marker walk succeeded, SOF seen before SOS, dimensions sane; (2) full
baseline/progressive Huffman decode with **own** decoder that reports the byte offset of the
first invalid code / unexpected marker (`offset_error`) and the MCU row at which it occurred; (3)
boundary discontinuity test on decoded MCU rows at cluster boundaries (section 3.9). Reject a
standalone candidate as "probably a thumbnail" (PhotoRec rule) when `len ≤ 16 KiB`, first marker is
APP0, and `X < 200 && Y < 200` — unless `--keep-thumbnails`.

Nested objects recorded by the JPEG validator: EXIF thumbnail (IFD1 0x0201/0x0202), MPO
secondary images, embedded ICC/XMP (not files), motion-photo MP4 trailer.

#### 2.2.2 PNG (ISO/IEC 15948)

Signature 8 bytes. Chunk = `length` u32 BE (data length, ≤ 2^31−1), `type` 4 ASCII bytes
(each byte must be alphabetic; bit 5 of byte 0 = ancillary), `data`, `crc` u32 BE = CRC-32
(ISO 3309, as zlib) over `type + data`. First chunk must be `IHDR` with length 13: width u32,
height u32 (both 1..2^31−1), bit depth ∈ {1,2,4,8,16}, colour type ∈ {0,2,3,4,6}, compression 0,
filter 0, interlace ∈ {0,1}. End = offset of `IEND` + 12 (`IEND` has length 0). Validation: all
CRCs match (`crc32fast`); optional inflate of concatenated `IDAT` to check adler32 and decoded
size = rows × (1 + rowbytes). APNG files are plain PNG. Nested: none.

#### 2.2.3 GIF (GIF89a spec)

Header 6 bytes. Logical Screen Descriptor 7 bytes at 6: width u16 LE, height u16 LE, packed
(bit 7 = Global Color Table flag, bits 0–2 = N → table size 3 × 2^(N+1)), bg index, aspect.
Then blocks: `0x21` extension: label(1), sub-blocks (len byte, data) until a `0x00` terminator;
`0x2C` image descriptor: left, top, width, height (u16 LE ×4), packed (bit 7 = Local Color Table,
bits 0–2 size), optional LCT, LZW min code size (1 byte, 2..8), sub-blocks until `0x00`;
`0x3B` trailer = END (len = pos + 1). Any other block byte → FAIL at that offset
(`err_is_prefix`).

#### 2.2.4 BMP (Windows BITMAPFILEHEADER)

| Off | Size | Field | Check |
|---|---|---|---|
| 0 | 2 | `BM` | |
| 2 | 4 | file size LE | = object length; ≥ 65 |
| 6 | 4 | reserved | = 0 |
| 10 | 4 | pixel data offset LE | 14 + DIB size ≤ offset < file size |
| 14 | 4 | DIB header size LE | ∈ {12, 40, 52, 56, 64, 108, 124} |
| 18 | 4/2 | width (LE32, or LE16 if DIB=12) | 1..65535 |
| 22 | 4/2 | height | ≠ 0, abs ≤ 65535 |

Validation: `file size ≥ offset + rowstride × |height|` where rowstride = ((bpp × width + 31) / 32) × 4
unless compression ∈ {1 RLE8, 2 RLE4, 4 JPEG, 5 PNG}. Weak 2-byte magic → sector alignment required.

#### 2.2.5 TIFF family (TIFF 6.0; CR2, NEF, ARW, DNG, PEF, ORF, RW2)

Header: bytes 0–1 byte order (`II` LE / `MM` BE), 2–3 magic 42, 4–7 IFD0 offset (≥ 8). IFD:
count u16, then `count` × 12-byte entries (tag u16, type u16, count u32, value-or-offset u32),
then next-IFD offset u32 (0 = last). Type sizes: 1 BYTE=1, 2 ASCII=1, 3 SHORT=2, 4 LONG=4,
5 RATIONAL=8, 6 SBYTE=1, 7 UNDEFINED=1, 8 SSHORT=2, 9 SLONG=4, 10 SRATIONAL=8, 11 FLOAT=4,
12 DOUBLE=8, 13 IFD=4. If `count × size > 4` the value field is an offset.

Object end = max over all visited IFDs (IFD chain, SubIFDs tag `0x014A`, ExifIFD `0x8769`,
GPS `0x8825`, Interop `0xA005`, CR2 IFD3 at header offset 12, Panasonic/Olympus maker IFDs) of:

| Tag | Name | Contribution |
|---|---|---|
| any | entry value area | `offset + count × typesize` when stored out-of-line |
| `0x0111` + `0x0117` | StripOffsets + StripByteCounts | max_i (off_i + cnt_i) |
| `0x0144` + `0x0145` | TileOffsets + TileByteCounts | max_i (off_i + cnt_i) |
| `0x0201` + `0x0202` | JPEGInterchangeFormat + Length | off + len (nested JPEG) |
| `0xC612` | DNGVersion | presence ⇒ format = dng |
| `0x010F` | Make | ASCII; selects nef/arw/pef/orf/cr2 |
| `0x0118` (Panasonic IFD) | RawDataOffset UNVERIFIED | start of RW2 raw; no byte count → unknown length |

Format decision (first match): bytes 8..11 == `43 52 02 00` ("CR", major 2) ⇒ `cr2` (raw IFD3
offset at bytes 12..15); `0xC612` present ⇒ `dng`; magic `IIRO`/`IIRS`/`MMOR` ⇒ `orf`;
magic `IIU\0` ⇒ `rw2`; Make starts with `NIKON` ⇒ `nef`; Make starts with `SONY` ⇒ `arw`;
Make starts with `PENTAX` or `RICOH` ⇒ `pef`; else `tif`. RW2/ORF validators are the TIFF walker
with the 4 magic bytes swapped and (ORF) IFD offset at 4 as usual.

Nested: the IFD0 JPEG preview in CR2 (full-size JPEG in StripOffsets of IFD0), IFD1 thumbnail,
NEF/ARW/PEF/DNG preview JPEGs in SubIFDs or `0x0201`. All reported as `nested[]` and extracted
on `--extract-nested` or when the parent is damaged (section 3.3 rule R5).

#### 2.2.6 Fuji RAF

| Off | Size | Field |
|---|---|---|
| 0 | 16 | `FUJIFILMCCD-RAW ` |
| 16 | 4 | format version ASCII (e.g. `0201`) |
| 20 | 8 | camera ID |
| 28 | 32 | camera model string, NUL padded |
| 60 | 4 | directory version |
| 64 | 20 | unknown |
| 84 | 4 | JPEG offset (BE) |
| 88 | 4 | JPEG length (BE) |
| 92 | 4 | CFA header offset (BE) |
| 96 | 4 | CFA header length (BE) |
| 100 | 4 | CFA (raw) offset (BE) |
| 104 | 4 | CFA (raw) length (BE) |

len = max(jpg_off + jpg_len, cfa_hdr_off + cfa_hdr_len, cfa_off + cfa_len); every offset must be
≥ 108 and every (off + len) ≤ cap. Nested: the embedded JPEG at `jpg_off`. Newer RAF files may
carry a second (RAF container) section after the CFA with sizes in the CFA header; treated as
covered by `cfa_off + cfa_len`. UNVERIFIED

#### 2.2.7 ISOBMFF box grammar (ISO/IEC 14496-12) — MP4, MOV, 3GP, M4A, HEIF, AVIF, CR3

Box header: `size` u32 BE, `type` 4 bytes. `size == 1` ⇒ `largesize` u64 BE follows at +8
(header 16 bytes). `size == 0` ⇒ box extends to end of file (only legal for the last box,
normally `mdat`). `type == 'uuid'` ⇒ 16-byte extended type follows the header. Minimum box size
is 8 (16 with largesize). `ftyp`: major_brand(4), minor_version(4), compatible_brands(4 × n).

Top-level walk:

```
pos = start; seen_ftyp = false; seen_moov = false; seen_mdat = false; n = 0
loop:
  if pos + 8 > cap: END(capped)
  (size, type) = read; if size==1: size = u64 at pos+8 (require ≥ 16)
  if size == 0: UNKNOWN_LENGTH (mdat to EOF) — return len=pos, status=unknown_length
  if type not in ALLOWED_TOP: END(len = pos)            // next file or garbage
  if type == 'ftyp' and n > 0: END(len = pos)            // a second ftyp is the next file
  if size < 8 or pos + size > cap: END(len = pos, status = truncated)
  if type == 'moov': seen_moov = true; parse mvhd (creation_time) for date organisation
  pos += size; n += 1
  stop when seen_moov && seen_mdat && next type ∉ {free, skip, wide, uuid, meta, moof, mfra, sidx}
ALLOWED_TOP = {ftyp, moov, mdat, free, skip, wide, uuid, meta, moof, mfra, sidx, styp, ssix, prft,
               pdin, junk, pnot, PICT, prfl, idat, jP\x20\x20, jp2h, jp2c, ftyp}
```

`mvhd` (inside `moov`): version byte at +8; v0: creation_time u32 at +12 (seconds since
1904-01-01 00:00 UTC), v1: u64 at +12. Used for YYYY/MM organisation when no EXIF.

Status rules: `moov` present and `mdat` present and all boxes closed ⇒ `valid`. `mdat` present,
no `moov` (camera died while recording; moov is written at stop) ⇒ `truncated_no_moov`, len = end
of last complete box, flagged for the moov-rebuild repair module. `size == 0` on `mdat` ⇒
`unknown_length`: the resolver extends it to the next aligned strong header or the cap
(section 3.3 rule R3).

Brand → extension: `isom, iso2, iso4, iso5, iso6, mp41, mp42, mmp4, avc1, dash, MSNV, XAVC`
⇒ mp4; `qt  ` ⇒ mov; `M4V `, `M4VH`, `M4VP` ⇒ m4v; `M4A `, `M4B `, `M4P ` ⇒ m4a;
`3gp4..3gp9, 3ge6, 3gg6, 3gr6` ⇒ 3gp; `3g2a, 3g2b, 3g2c` ⇒ 3g2; `heic, heix, hevc, hevx, heim,
heis, hevm, hevs, mif1, msf1` ⇒ heic (`.heif` when major is `mif1`/`msf1` and no `heic` in
compatible brands); `avif, avis, avio` ⇒ avif; `crx ` ⇒ cr3; `jp2 ` ⇒ jp2 (not carved by default).
Unknown brand with valid box structure ⇒ mp4 with `notes: "unknown brand"`.

HEIF specifics: `meta` is a FullBox (4-byte version/flags after the header), contains `hdlr`
(handler `pict`), `pitm`, `iinf`, `iloc` (item locations: offsets may reference `mdat` or
`idat`), `iprp`. The validator checks that every `iloc` extent lies inside the measured object;
an extent beyond the end ⇒ `truncated`. Nested: `Exif` item (ID with `infe` item_type `Exif`)
gives the capture date; a JPEG-coded thumbnail item is possible (item_type `jpeg`).

CR3 specifics: `ftyp` major `crx ` minor 1; `moov` contains a `uuid` box with extended type
`85c0b687-820f-11e0-8111-f4ce462b6a48` holding `CNCV` (compressor version string), `CCTP`,
`CTBO` (table of offset/size pairs for the main regions, used as a cross-check of the computed
length — record layout UNVERIFIED), `CMT1`..`CMT4` (TIFF-structured EXIF/makernote blocks → date),
`THMB` (160×120 JPEG thumbnail). A second top-level `uuid` box `eaf42b5e-1c98-4b88-b9fb-b7dc406e4d16`
holds `PRVW` (≈1620×1080 JPEG preview). `mdat` holds track 1 = full-size JPEG, track 2 = small
CRX raw, track 3 = full CRX raw, track 4 = CTMD timed metadata. Nested: THMB, PRVW, track-1 JPEG.

#### 2.2.8 RIFF (AVI, WAV, WebP)

`RIFF` (or `RF64`), `size` u32 LE = bytes after this field, form type at 8 (`AVI `, `WAVE`,
`WEBP`). Object len = 8 + size, rounded up to even. AVI OpenDML (files > 1 GiB): after the first
RIFF chunk, further chunks `RIFF <size> AVIX` follow back-to-back; len accumulates while the 12
bytes at the running end are `RIFF ???? AVIX`. Each RIFF-AVI must contain `LIST hdrl` (with `avih`,
56 bytes: µs/frame, max bytes/s, padding, flags, total frames, initial frames, streams, suggested
buffer, width, height, reserved[4]) and `LIST movi`. MJPEG AVIs contain thousands of JPEG frames
(`00dc` chunks starting `FF D8`) ⇒ nested JPEGs; the resolver must suppress them (rule R2).
WAV: `fmt ` chunk (16/18/40 bytes: format tag, channels, sample rate, byte rate, block align,
bits) then `data` chunk; `data` size `0xFFFFFFFF` or RIFF size `0xFFFFFFFF`/0 ⇒ recorder never
finalised ⇒ `unknown_length` (walk chunks until invalid, then cap). RF64: `ds64` chunk right
after the form type carries 64-bit RIFF/data sizes. WebP: first chunk `VP8 `, `VP8L` or `VP8X`;
len from RIFF size; ≤ 4 GiB by construction.

#### 2.2.9 Matroska / WebM (EBML RFC 8794, Matroska spec)

Variable-size integers (vint): leading-zero count of the first byte gives length `n` (1..8); value
= first byte with the marker bit cleared, followed by `n−1` bytes, big-endian. Element = ID (vint
*with* marker bit kept, 1..4 bytes) + size (vint) + data. "Unknown size" = all value bits set
(e.g. `01 FF FF FF FF FF FF FF`).

| Element | ID (hex) | Level |
|---|---|---|
| EBML header | `1A 45 DF A3` | 0 |
| DocType | `42 82` | 1 (inside EBML) — "matroska" ⇒ mkv, "webm" ⇒ webm; audio-only Tracks ⇒ mka |
| Segment | `18 53 80 67` | 0 |
| SeekHead | `11 4D 9B 74` | 1 |
| Info | `15 49 A9 66` | 1 — contains DateUTC `44 61` (i64 ns since 2001-01-01) |
| Tracks | `16 54 AE 6B` | 1 |
| Cluster | `1F 43 B6 75` | 1 |
| Cues | `1C 53 BB 6B` | 1 |
| Attachments | `19 41 A4 69` | 1 (cover art JPEG/PNG ⇒ nested) |
| Chapters | `10 43 A7 70` | 1 |
| Tags | `12 54 C3 67` | 1 |
| Void | `EC` | any |
| CRC-32 | `BF` | any |
| Cluster/Timestamp | `E7` | 2 |
| SimpleBlock | `A3` | 2 |
| BlockGroup | `A0` | 2 |

len = (EBML header end) + Segment header size + Segment data size when Segment size is known.
When Segment size is unknown (live/unfinalised recording; typical for a camera that lost power):
walk level-1 elements; a Cluster with unknown size is walked at level 2 until an ID that is not a
level-2 cluster child and not a level-1 ID appears; then END(len = pos, status `unknown_length`).

#### 2.2.10 MPEG-TS / M2TS

TS packet 188 bytes: sync `0x47`; byte 1: TEI(1) PUSI(1) priority(1) PID high 5; byte 2 PID low 8;
byte 3: TSC(2) AFC(2, ≠ 00) CC(4). M2TS/MTS (BDAV, AVCHD camcorders): each packet is prefixed by
a 4-byte TP_extra_header (2-bit copy permission + 30-bit arrival timestamp), so sync bytes sit at
`4 + 192k`. Probe: require sync at `k × stride` for k = 0..7 (8 packets) and at least one packet
with PID ∉ {0x1FFF} and AFC ≠ 0. Measure: count packets while sync holds; also track CC
continuity per PID (CC gaps > 2 within the run ⇒ confidence penalty but not a stop, since
recorders drop packets). END when sync fails; `len = packets × stride`. Extension: `.m2ts`/`.mts`
when stride = 192 (PhotoRec additionally looks for `HDMV`/`HDPR` descriptor at 0xD7 or `TSHV` at
0x18B to label camcorder streams), else `.ts`. A sync-run has no footer, so this is the weakest
end-detection method; status is never better than `structurally_valid`.

#### 2.2.11 MPEG-PS / VOB / MPEG video ES

Start codes `00 00 01 xx`. Pack header `BA`: MPEG-2 if byte 4 & 0xC4 == 0x44 → 14 bytes + stuffing
(low 3 bits of byte 13); MPEG-1 if byte 4 & 0xF1 == 0x21 → 12 bytes. System header `BB`, PES
`BD` (private 1), `BE` (padding), `BF` (private 2 / DVD NAV), `C0..DF` audio, `E0..EF` video:
each has a u16 BE length at +4 (length of the rest). Program end `B9`. Walk: pack → zero or more
PES → next pack; END on `B9` (+4) or on a start code outside the set (len = pos). DVD VOB: every
pack is exactly 2048 bytes and starts with `BA` then usually `BB`/`BF`; label `.vob` when ≥ 90% of
packs are 2048-byte aligned and private-stream-2 NAV packets appear, else `.mpg`. Video ES
(`B3` sequence header, 12 bytes: width 12 bits, height 12 bits, aspect 4, frame rate 4, bit rate
18, marker, vbv 10, flags) is walked by start codes `00 00 01 {00 picture, B2 user, B3, B5 ext,
B8 GOP}` and `01..AF` slices; END at `B7` sequence end or invalid code. Both are run-walks
(`structurally_valid` at best).

#### 2.2.12 MP3 (ISO/IEC 11172-3, 13818-3; ID3v2.3/2.4; ID3v1; APEv2)

ID3v2 header: `ID3`, version (2/3/4), revision, flags (bit 4 = footer present in v2.4),
size = 4 syncsafe bytes: `((b6&0x7F)<<21)|((b7&0x7F)<<14)|((b8&0x7F)<<7)|(b9&0x7F)`; tag length
= 10 + size (+ 10 if footer). Frame header 32 bits: sync 11 bits (`FF Ex`/`FF Fx`), version 2 bits
(00 = 2.5, 01 = reserved, 10 = 2, 11 = 1), layer 2 bits (00 reserved, 01 = III, 10 = II, 11 = I),
protection 1, bitrate index 4 (0 free, 15 invalid), sample-rate index 2 (3 invalid), padding 1,
private 1, channel mode 2, mode ext 2, copyright 1, original 1, emphasis 2 (10 invalid).

Sample rates (Hz): V1 {44100, 48000, 32000}; V2 {22050, 24000, 16000}; V2.5 {11025, 12000, 8000}.
Bitrates (kbit/s), index 1..14: V1 L1 {32,64,96,128,160,192,224,256,288,320,352,384,416,448};
V1 L2 {32,48,56,64,80,96,112,128,160,192,224,256,320,384}; V1 L3 {32,40,48,56,64,80,96,112,128,
160,192,224,256,320}; V2/V2.5 L1 {32,48,56,64,80,96,112,128,144,160,176,192,224,256};
V2/V2.5 L2&L3 {8,16,24,32,40,48,56,64,80,96,112,128,144,160}.
Frame length: L1 = (12 × bitrate / samplerate + padding) × 4; L2 and V1-L3 = 144 × bitrate /
samplerate + padding; V2/V2.5-L3 = 72 × bitrate / samplerate + padding (bitrate in bit/s).

Probe (bare frames): ≥ 3 consecutive frames with identical version/layer/sample-rate and each
next header at exactly the computed length. Measure: walk frames; tolerate a `Xing`/`Info` (at
+36 for V1 stereo, +21 V1 mono, +21 V2 stereo, +13 V2 mono) frame; END when the next 4 bytes are
not a consistent header; then absorb trailing `TAG` (ID3v1, 128 bytes), `APETAGEX` (APEv2 footer:
32 bytes; tag size u32 LE at +12; items precede it) or `LYRICS200`. Xing frame-count × frame
length gives an expected size used for the confidence score.

#### 2.2.13 FLAC

`fLaC` then metadata blocks: byte 0 = last-block flag (bit 7) | type (0 STREAMINFO, 1 PADDING,
2 APPLICATION, 3 SEEKTABLE, 4 VORBIS_COMMENT, 5 CUESHEET, 6 PICTURE, 127 invalid); 3-byte BE
length. STREAMINFO (34 bytes, must be first): min/max block size u16, min/max frame size u24,
then packed: sample rate 20 bits, channels−1 3 bits, bps−1 5 bits, total samples 36 bits, MD5 16.
PICTURE blocks embed JPEG/PNG cover art ⇒ nested. After the last metadata block come frames with
14-bit sync `11111111111110` (`FF F8` / `FF F9`), header protected by CRC-8 (poly 0x07, init 0),
frame footer CRC-16 (poly 0x8005). Measure: walk frame headers validating CRC-8; END when no
valid sync+CRC-8 within `max_frame_size` (or 64 KiB if unknown) bytes. Upper bound for the cap:
`ceil(total_samples / min_block_size) × max_frame_size` when all three are non-zero.

#### 2.2.14 Photoshop PSD/PSB

| Off | Size | Field | Check |
|---|---|---|---|
| 0 | 4 | `8BPS` | |
| 4 | 2 | version | 1 = PSD, 2 = PSB |
| 6 | 6 | reserved | all 0 |
| 12 | 2 | channels | 1..56 |
| 14 | 4 | height | 1..30000 (PSB 300000) |
| 18 | 4 | width | same |
| 22 | 2 | depth | ∈ {1, 8, 16, 32} |
| 24 | 2 | colour mode | 0..9 |
| 26 | 4 | colour-mode data length | |
| … | 4 | image resources length | resource `0x040C` = JPEG thumbnail ⇒ nested |
| … | 4 (8 PSB) | layer & mask length | |
| … | 2 | compression | 0 raw, 1 RLE, 2 ZIP, 3 ZIP-pred |

Image data length: raw = channels × height × rowbytes; RLE = sum of the per-row byte counts
(height × channels u16 BE, u32 BE for PSB) + the table itself; ZIP ⇒ cannot be computed ⇒
`unknown_length` (walk by inflate stream end — optional). len = sum of all sections.

### 2.3 Containment matrix (who may legitimately contain whom)

| Container | May contain (reported as `nested`) |
|---|---|
| JPEG | JPEG (EXIF thumbnail, MPO images), MP4 (motion-photo trailer) |
| TIFF/CR2/NEF/ARW/DNG/PEF/ORF/RW2 | JPEG (previews/thumbnails), TIFF (SubIFDs are not separate files) |
| RAF | JPEG |
| CR3 | JPEG (THMB, PRVW, track 1), TIFF (CMT1..4) |
| HEIF/AVIF | JPEG (thumbnail items), TIFF (Exif item) |
| MP4/MOV/3GP/M4A | JPEG/PNG (`covr` metadata), TIFF (Exif in `uuid`), MP4 (never — a nested `ftyp` is a new file) |
| AVI | JPEG (MJPEG frames) |
| MKV/WebM | JPEG/PNG (Attachments) |
| PSD | JPEG (resource 0x040C) |
| MP3 | JPEG/PNG (ID3v2 `APIC`/`PIC` frames) |
| FLAC | JPEG/PNG (PICTURE block) |
| WAV | none |
| TS/M2TS, MPEG-PS | none (elementary streams are not carved separately) |

Anything not in the matrix that is found inside a measured object is treated as a *conflict*
(section 3.3).

---

## 3. Algorithms

### 3.1 High-throughput scanner

Chunked sequential read with overlap, single scanning thread.

```
CHUNK = 64 MiB            // tunable; 4 buffers in a pool ⇒ 256 MiB resident
OVERLAP = 64 KiB          // ≥ longest probe window (probe reads ≤ 64 KiB past a header)
align = ctx.cluster_size.unwrap_or(512); if --align=N given, use N (1 disables)

fn scan(src: &Source, ranges: &RangeSet /* whole image or unallocated */) {
  for range in ranges:                      // each range is cluster-aligned, see 3.4
    let mut base = range.start;
    while base < range.end:
      buf = pool.acquire();                  // blocks when 4 chunks are in flight (back-pressure)
      n = src.read_at(base, &mut buf[..min(CHUNK + OVERLAP, range.end - base)]);
      // src zero-fills unreadable sectors (mapfile '-' / '/' / '*' blocks) and reports them
      bad = src.bad_ranges_within(base, base + n);
      hits = multi_search(&buf[..n], base);   // returns (offset, sig_id) ascending
      for (off, sig) in hits:
        if off >= base + CHUNK && off < base + CHUNK + OVERLAP: continue   // owned by next chunk
        if (off - partition_origin) % align != 0 && !sig.allow_unaligned: continue
        if sig.needs_sector_align && (off % 512) != 0: continue            // TS, PS, MP3-bare, BMP
        window = &buf[off - base .. min(n, off - base + 64 KiB)]            // ≤ 64 KiB, may be short
        if let Some(cand) = FORMATS[sig.format].probe(window, &ProbeCtx{off, bad, align}):
          tx.send(Job{seq: chunk_seq, cand, buf: buf.clone_ref()})         // Arc'd buffer
      base += CHUNK;                       // next chunk re-reads the 64 KiB overlap
      chunk_seq += 1
}
```

`multi_search` dispatch (section 1.3 numbers justify the split):

1. **Fixed-magic set** (all signatures with ≥ 4 literal leading bytes and no leading `0x00`):
   one `aho_corasick::AhoCorasick` built with `MatchKind::Standard`, `find_overlapping_iter` so a
   TIFF magic and the longer CR2 magic both report (the probe collapses them). ~30 patterns ⇒ the
   Teddy packed prefilter applies on x86_64/aarch64. Patterns: `FF D8 FF`, PNG 8 bytes, `GIF87a`,
   `GIF89a`, `RIFF`, `II*\0`, `MM\0*`, `IIRO`, `IIRS`, `MMOR`, `IIU\0`, `FUJIFILMCCD-RAW `,
   `8BPS`, `ftyp` (hit offset − 4), `moov`/`mdat`/`wide`/`free`/`skip` (hit − 4),
   `1A 45 DF A3`, `ID3`, `fLaC`.
2. **MPEG start codes**: `memchr::memmem::Finder::new(b"\x00\x00\x01")` (its rare-byte
   heuristic picks `0x01`), then test byte +3 ∈ {BA, B3}. Searching `0x00` with `memchr` would
   stall on zero-filled regions; `memmem` does not.
3. **TS sync**: `memchr(0x47)` restricted to offsets `≡ 0 (mod 512)` is wasteful; instead test
   `buf[k*512]` and `buf[k*512 + 4]` directly for each sector (2 loads per sector) and run the
   8-packet probe on a hit.
4. **MP3 bare sync**: `memchr(0xFF)` only on sectors that are not already covered by a hit from
   (1) and only when the candidate passes the 3-frame probe. On JPEG-dense data `0xFF` occurs
   every ~256 bytes (stuffing), so this step is skipped inside ranges already attributed to a
   measured object (the scanner keeps a "skip-until" cursor fed back from the resolver).
5. **Zero-sector skip**: before (1)–(4), sectors that are all-zero (checked 64 bytes at a time with
   `u64` compares) are marked in a bitmap that the resolver and the bifragment search consult
   (`zblock`); nothing else is done for them.

Alignment rationale: FAT/exFAT/NTFS/ext4 allocate file data at cluster boundaries, so a header at
an unaligned offset is either embedded inside another file (nested) or inside an NTFS resident
attribute (files < ~700 bytes, never a photo). Scanning aligned-only cuts probe calls by
`align/1` and removes most false positives for 2–4 byte magics. `--align=1` ("deep") is offered
for ReiserFS/btrfs-inline/compressed/unknown layouts, and the nested-harvest path (R5) covers
embedded previews without a deep scan.

Block-size inference when the filesystem module gave none (PhotoRec's method, restated): collect
the start offsets of the first 10 candidates whose `measure` returned `valid`; `bs = 64 KiB`;
`off = start[0] % bs`; while any `start[i] % bs != off` and `bs > 512`: `bs >>= 1; off = start[0] % bs`.
Use `(bs, off)` as `(align, partition_origin)` for the rest of the scan and re-scan the first 10
files' region if `bs` changed.

### 3.2 Validator trait

```rust
pub struct Signature { pub bytes: &'static [u8], pub mask: Option<&'static [u8]>,
                       pub offset: u32 /* magic offset within file */, pub needs_sector_align: bool }

pub struct Candidate { pub format: FormatId, pub start: u64,
                       pub min_len: u64,          // what probe already proved (header length)
                       pub hint: ProbeHint }      // e.g. TIFF subtype, brand, stride 188/192

pub struct Extent {
    pub len: u64,                 // bytes attributed to this object, from start
    pub status: Status,           // Valid | StructurallyValid | Truncated | UnknownLength | Capped | Invalid
    pub confidence: u8,           // 0..=100, section 3.8
    pub err_offset: Option<u64>,  // first byte the validator could not accept (absolute)
    pub nested: Vec<Nested>,      // {format, start, len, role: Thumbnail|Preview|CoverArt|Frame|Trailer}
    pub meta: Meta,               // dims, codec, capture_time (EXIF 0x9003 / mvhd / DateUTC), camera make/model
    pub flags: Flags,             // err_is_prefix, appended_data_ignored, no_zblocks, plaintext_container
    pub fragments: Vec<Range>,    // >1 entry only after bifragment recovery (3.9)
}

pub trait Source: Send + Sync {
    fn len(&self) -> u64;
    fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<usize>;   // pread; zero-fills bad sectors
    fn bad_ranges_within(&self, start: u64, end: u64) -> Vec<Range>;     // from mapfile
    fn is_zero_sector(&self, off: u64) -> Option<bool>;                  // from scanner bitmap
}

pub trait Format: Send + Sync {
    fn id(&self) -> FormatId;
    fn signatures(&self) -> &'static [Signature];
    fn priority(&self) -> u8;
    fn max_len(&self) -> u64;
    fn may_contain(&self, inner: FormatId) -> bool;       // containment matrix 2.3
    /// Cheap: ≤ 64 KiB window, no I/O. Rejects most false positives.
    fn probe(&self, window: &[u8], ctx: &ProbeCtx) -> Option<Candidate>;
    /// Streaming: may read any amount via `src`, must stay within `budget` bytes of reads
    /// and must be cancellable via `ctx.cancel`.
    fn measure(&self, src: &dyn Source, cand: &Candidate, ctx: &MeasureCtx) -> Extent;
}
```

Streaming access: `measure` never maps the whole object. It uses a `Cursor` over `Source` with a
1 MiB read-ahead buffer (per worker, reused) and `skip(n)` that seeks without reading (box walkers
skip `mdat` in O(1)). Validators that must touch every byte (JPEG decode, PNG CRC, TS run) stream
in 1 MiB pieces. A `budget` (default `min(max_len, 8 × cluster-aligned run length)`) bounds the I/O
of any single measurement.

Measurement outcomes map to Garfinkel's return values: `Valid` ≙ `V_OK` with length;
`Truncated` ≙ `V_EOF` (ran into cap, bad sectors, or the next header before an end marker);
`Invalid` ≙ `V_ERR` with `err_offset`.

### 3.3 Overlap / nesting resolver

Input: all measured `(cand, extent)` sorted by `start`, then by priority desc. Output: a
non-overlapping list of `Object`s plus per-object `nested[]`. Sweep line with an active set:

```
active = []                         // objects whose end > current start, sorted by end
for (c, e) in sorted:
  evict from active those with end <= c.start
  if active.is_empty(): push(c,e); continue
  outer = active.max_by(priority, then confidence)      // the enclosing/overlapping object
  match relation(outer, c):
    SameStart =>                                   // R1: same offset, two formats (TIFF vs CR2)
        keep higher priority; alias the other into outer.meta.aliases; drop c
    Inside if outer.format.may_contain(c.format) && outer.status in {Valid, StructurallyValid} =>
        outer.nested.push(c as Nested); drop c      // R2: legitimate embedded object
    Inside if outer.status in {UnknownLength, Capped, Truncated} && c.start % align == 0 =>
        outer.len = c.start - outer.start           // R3: a new aligned header ends a weak-length object
        outer.status = Truncated (or keep Valid if outer.flags.appended_data_ignored); push(c,e)
    Inside if c.confidence > outer.confidence + 20 && c.start % align == 0 =>
        split: outer.len = c.start - outer.start; outer.status = Truncated; push(c,e)   // R4
    Inside =>
        outer.nested.push(c as Nested{role: Conflict}); drop c     // R2': keep bytes with outer
    PartialOverlap =>                                // c.start < outer.end < c.end
        if c.confidence > outer.confidence: truncate outer to c.start (if outer.flags.err_is_prefix) else drop outer; push(c,e)
        else: drop c (or truncate c's start? never — a carved object must start at its header)
```

R5 (damaged parent harvest): after the sweep, for every object with status ∈ {Truncated, Invalid,
Capped} and `nested` non-empty, each nested JPEG/PNG whose own `measure` returns `Valid` is
*promoted* to an output object (name `carved_<parentoffset>_nested<N>.jpg`, `report.parent`
set). With `--extract-nested` promotion happens for all parents. Motion-photo trailers are never
promoted unless `--split-motion-photos`.

Every byte is attributed once: the resolver maintains a `RangeSet<u64>` of attributed ranges; an
object is emitted only if its range is disjoint from the set (after the truncations above) and is
then inserted. Bifragment objects (3.9) insert both fragments.

### 3.4 Unallocated-only mode

The filesystem module exposes `fn allocation(&self) -> AllocMap { cluster_size, origin,
bitmap }` for FAT12/16/32 (FAT entries == 0), exFAT (allocation bitmap file), NTFS (`$Bitmap`),
ext4 (block bitmaps per group). The carver converts it to a `RangeSet` of **byte ranges that are
free**, merges adjacent clusters, and (option) drops runs shorter than `min_run = 4 clusters`
(nothing photo-sized fits). Two sub-modes:

- `--unallocated=strict` (default): each free run is scanned independently; an object may not
  cross into an allocated cluster (`measure` sees the run end as EOF ⇒ `Truncated`).
- `--unallocated=skip` (PhotoRec-like, PhotoRec's exact behaviour here is UNVERIFIED): the free
  runs are presented to `measure` as one logical stream (`Source` adapter that maps logical
  offsets to physical runs). This reassembles deleted files whose clusters were interleaved with
  files written later. The report lists every physical fragment. Deleted-but-still-chained
  files recovered by the filesystem module are first subtracted from the free set so the two
  paths do not duplicate work.

Whole-space mode additionally subtracts nothing but takes the allocation map, if any, as a
confidence feature (`in_allocated_space` ⇒ the carved object is probably a live file the
filesystem path already recovered ⇒ dedup, 3.6).

### 3.5 Parallelism and memory model

```
[scanner thread] --(bounded channel, cap 4 chunks)--> [rayon pool: measure]
                                                          |
                                                 results keyed (chunk_seq, start)
                                                          v
                                              [reorder buffer] -> [resolver] -> [writer thread]
```

- One scanner thread (I/O-bound). Chunk buffers come from a pool of 4 × (64 MiB + 64 KiB)
  `Arc<[u8]>`; the channel capacity equals the pool size, so at most 256 MiB of chunk data is
  resident. The scanner blocks when the pool is empty (back-pressure from slow validators).
- `rayon` pool of `num_cpus` workers runs `measure`. Each job holds an `Arc` to its chunk so
  probes can read the header window without I/O; anything beyond the chunk goes through `Source`
  (`pread`, OS page cache; `memmap2` is optional and off by default because a mapped bad sector in a
  live image can SIGBUS).
- Each worker owns one 1 MiB cursor buffer plus a bounded decoder scratch (JPEG: one MCU row
  × components, ≤ 65535 × 4 × 16 B ≈ 4 MiB worst case; PNG: 2 rows). No validator allocates
  proportional to file size.
- Results carry `(chunk_seq, start)`. A reorder buffer releases results to the resolver in
  ascending `start` once every job from chunks `≤ chunk_seq` has completed and no active object from
  an earlier chunk is still open (needed for R3/R4 lookahead). Because an object can extend many
  chunks past its header (a 20 GB MP4), the resolver finalises object *O* only when the scanner
  has passed `O.end` (or the source end); until then *O* is "open" and later candidates inside it
  are queued. Memory for the queue is bounded by the number of candidates per open range (a 20 GB
  mdat yields few candidates because the scanner skips attributed ranges once the resolver has
  tentatively claimed them; the skip-until cursor is advisory and corrected if the object is later
  truncated).
- Two-pass output (Scalpel's approach): pass 1 = scan + measure + resolve, producing the object
  list in memory (≈ 200 B/object; spilled to a temp file at > 1 M objects). Pass 2 = the writer
  walks objects in ascending offset and copies `[start, start+len)` with 4 MiB reads, hashing with
  BLAKE3 as it goes, into `<tmp>/carved_<hex>.<ext>.part`, then renames into the final folder.
  This guarantees output order, keeps pass-2 reads sequential, and means a carve is never left
  half-written if the process dies. `--single-pass` writes during measurement for sources where
  re-reading is expensive (a live mapfile-backed device would be, but the carver refuses live
  devices anyway: `Source` must be an image).
- Cancellation: a global `AtomicBool`; validators poll it every 1 MiB.

### 3.6 Deduplication against filesystem-recovered files

The filesystem module writes `fs_index.jsonl` with `{path, len, blake3, blake3_head64k,
image_ranges[]}` per recovered file. The carver:

1. Loads `image_ranges` into a `RangeSet` → any carved object whose range is *identical* to or
   *contained in* a filesystem-recovered file's ranges is marked `duplicate_of = path` and not
   written (its report record is kept). This catches ~all duplicates with zero hashing.
2. For the rest, after writing, compares `(len, blake3)` and `blake3_head64k`: equal full hash ⇒
   `duplicate_of`; equal head hash and carved `len < fs len` ⇒ `prefix_of` (a truncated carve of a
   file the filesystem path got whole) ⇒ delete the carve unless `--keep-duplicates`.
3. Carved objects are also deduplicated among themselves by full BLAKE3 (same photo stored twice
   on the card, or found both in a parent and promoted via R5).

BLAKE3 is computed in the writer's copy loop (`blake3::Hasher::update_rayon` for pieces ≥ 128 KiB
when the pool is idle; plain `update` otherwise).

### 3.7 Output naming and organisation

```
<out>/carved/<YYYY>/<MM>/carved_<offset:012x>.<ext>
<out>/carved/undated/carved_<offset:012x>.<ext>
<out>/carved/<YYYY>/<MM>/carved_<offset:012x>_nested<N>.<ext>       // promoted nested objects
<out>/carved/<YYYY>/<MM>/carved_<offset:012x>_frag.<ext>            // reassembled bifragment
```

`<offset>` is the absolute byte offset of the header in the image, 12 lowercase hex digits
(covers 2^48 B = 256 TiB), so names sort by offset and are unique per image. Date source, in
order: EXIF `DateTimeOriginal` (0x9003) → EXIF `DateTime` (0x0132) → `mvhd`/`tkhd` creation_time
(skip the sentinel 0 and dates before 1990-01-01 or after now + 1 day) → Matroska `DateUTC` →
ID3 `TDRC`/`TYER` → none ⇒ `undated`. `--flat` disables the folders; `--by-format` adds a
`<ext>/` level. Extension is the lowercase canonical one from the table; `.jpeg` is never used.
Windows: identical layout; names contain no reserved characters.

### 3.8 Confidence score and report

Confidence (0–100) = clamp(sum of):

| Feature | Points |
|---|---|
| signature strength: 2–3 bytes 5, 4–7 bytes 10, ≥ 8 bytes or brand-checked 15 | |
| structure fully walked to a real end marker (EOI/IEND/0x3B/box list closed with moov+mdat/RIFF size/EBML Segment size) | +35 |
| structure walked but run-based end (TS/PS/MP3/FLAC frames, unknown-size EBML) | +15 |
| decode-level validation passed (JPEG full decode, PNG CRC+inflate, MP3 ≥ 32 frames, box tables consistent with mdat extent, iloc extents inside) | +25 |
| capture metadata present and sane (EXIF/mvhd/DateUTC in [1990, now]) | +10 |
| header at cluster-aligned offset (or inside parent for nested) | +5 |
| no unreadable sectors inside the object | +5 |
| penalties: `Truncated` −30, `Capped` −40, `UnknownLength` −20, zero sector inside a `no_zblocks` format −20, CC gaps (TS) −5, dims > 50 Mpx or < 16 px −10, ends exactly at cap −10 | |

Status thresholds: ≥ 80 `valid`; 50–79 `probable`; 25–49 `partial`; < 25 rejected (not written
unless `--keep-corrupted`, PhotoRec's "Keep corrupted files").

Report: `<out>/carve-report.json`

```json
{
  "$schema": "diskrescue-carve-report/1",
  "source": { "image": "/path/sdb.img", "mapfile": "/path/sdb.map", "size": 1000204886016,
              "partition_origin": 1048576, "cluster_size": 32768, "mode": "unallocated-strict",
              "align": 32768, "unreadable_bytes": 4718592 },
  "run": { "started": "2026-10-12T09:00:00Z", "finished": "...", "tool_version": "0.1.0",
           "bytes_scanned": 912345678901, "candidates": 184221, "measured": 184221,
           "objects_written": 10432, "rejected": 173000, "duplicates": 789 },
  "objects": [
    { "id": 1,
      "offset": 2147483648, "length": 4312996, "fragments": [[2147483648, 4312996]],
      "format": "jpg", "aliases": [], "path": "carved/2024/07/carved_000080000000.jpg",
      "blake3": "…64 hex…",
      "validation": { "status": "valid", "confidence": 92, "end_method": "eoi",
                      "decode": "ok", "err_offset": null, "unreadable_inside": 0 },
      "metadata": { "width": 6000, "height": 4000, "capture_time": "2024-07-14T10:22:31",
                    "make": "Canon", "model": "EOS R6", "codec": null, "duration_s": null },
      "nested": [ { "format": "jpg", "offset": 2147483648, "rel_offset": 1382, "length": 11210,
                    "role": "thumbnail", "promoted": false } ],
      "repair": { "applied": false, "module": null, "notes": null },
      "duplicate_of": null, "parent": null,
      "notes": ["trailer: none"] },
    { "id": 2, "offset": 2152267776, "length": 0, "format": "mp4", "path": null,
      "validation": { "status": "rejected", "confidence": 12, "end_method": "box-walk",
                      "err_offset": 2152267800 }, "notes": ["second box invalid"] }
  ]
}
```

Field semantics: `fragments` lists physical ranges in file order; `end_method` ∈ {eoi, iend,
trailer, size-field, box-walk, ebml-size, ebml-walk, riff-size, ifd-walk, sync-run, frame-walk,
next-header, cap}; `repair.applied` is set by the repair modules which append to the same record
(they are a later pipeline stage and key on `id`).

### 3.9 Fragmented JPEG recovery (bifragment gap carving) — stretch goal

Trigger: the JPEG validator returns `Invalid` with `err_offset = X` where X lies after SOS, and
`X` (rounded down) falls within `D = 16 clusters` after a cluster boundary — Garfinkel's
observation that libjpeg-style decoders consume up to several sectors of garbage before erroring
(9 sectors in the Mars example) means the true fragmentation point `e1` is a cluster boundary in
`[X − D, X]`, not `X` itself. Our own decoder also emits, per MCU row, the boundary-discontinuity
metric so `e1` can usually be pinned to the last cluster boundary before the first discontinuous
MCU row.

```
fn bifragment(src, obj /* JPEG, decoder snapshots at each cluster boundary */, C = cluster, W = 64 clusters (default), D = 16):
  e1_set = cluster boundaries b in [X - D*C, X] with b > sos_end, ordered nearest-X-first
  for e1 in e1_set:
    st = snapshot(e1)                       // bit-reader pos, DC predictors, MCU index, RST counter, EOB run
    for s2 in (e1 + C) ..= (e1 + W*C) step C:
      if attributed(s2) || is_zero_sector(s2) || strong_header_at(s2) || (mode==unalloc && allocated(s2)): continue
      dec = st.clone(); dec.input = src[s2..]
      // SHT over MCU rows (Pal et al.): Y_i = boundary metric of row i after the splice
      Λ = 1.0
      for row in 0..R_MAX (8):
        match dec.decode_row():
          Err(_) => { Λ = ∞; break }                    // Huffman/marker error ⇒ H1
          Ok(y)  => Λ *= p1(y) / p0(y)
        if Λ > τ+ { break /* reject s2 */ }
        if Λ < τ− { break /* accept s2 */ }
      if Λ < τ−:
        continue decoding to EOI; if EOI reached without error: return Fragments[(S, e1-S), (s2, end-s2)]
        if a second error occurs at X2: (optional, depth-2) recurse with e1' in [X2-D*C, X2], window W; else record partial
  return None   // leave object as Truncated at e1_best (largest e1 whose rows were all continuous)
```

`p0`, `p1` are histograms of the boundary metric (mean absolute difference between the last pixel
row of the MCU row before the splice and the first row after, per component, normalised by the
image's own mean row-to-row difference) estimated offline from the fixture corpus: `p0` from true
consecutive rows, `p1` from rows across random splices. Defaults `P_fa = P_fe = 0.01` ⇒
`τ+ = 99`, `τ− ≈ 0.0101`. Restart markers: if `DRI` is present, the splice candidate must land such
that the next `RSTn` number continues the sequence — a strong, cheap filter applied before decoding.

Complexity: `|e1_set| ≤ D`, `|s2 set| ≤ W`, each trial decodes ≤ `R_MAX` MCU rows of cost `r`
(≤ a few µs–100 µs for 6000-px-wide images), plus one full decode of the tail on acceptance:
`O(D · W · r + L)`. With `D = 16, W = 64` that is ≤ 1,024 trials per fragmented JPEG, i.e.
sub-second; `W = 4096` (16 MiB at 4 KiB clusters, covers the 651,264-byte gaps seen in the corpus)
is ≤ 65,536 trials ≈ seconds per file. Contrast Garfinkel's unconstrained search `O(n²)` per object
and `O(n⁴)` per image. Trials are embarrassingly parallel across `s2` (rayon). Because the first
fragment's decoder state is snapshotted, no re-decoding of the prefix occurs. Gap coverage: the
corpus shows 8-, 16-, 32- and 64-block gaps dominate, so `W = 64` catches the bulk; `--frag-window`
raises it. Enabled with `--fragmented=jpeg` (PhotoRec's "bruteforce" equivalent); never on by
default because it multiplies validator CPU time on heavily damaged images.

---

## 4. Edge cases and failure modes seen on real failing media

1. **Unreadable sectors inside an object.** The image has zero-filled holes where the mapfile
   says `-`/`*`/`/`. A JPEG with a hole decodes as a grey/garbage band then usually errors
   (`err_offset` inside or after the hole). Policy: do not treat a hole as a fragmentation point;
   measure through it; status `Truncated`/`partial` with `unreadable_inside = n`; the JPEG repair
   module can re-sync at the next RST marker. For box formats a hole inside `moov` is fatal for
   the box walk ⇒ status `truncated_no_moov` path; a hole inside `mdat` is harmless to the walk.
2. **Zero-filled and 0xFF-filled regions** (TRIM'd SSD LBAs read as zeros; erased NAND in pen
   drives reads 0xFF or 0x00; unused SD areas 0x00). `memchr(0xFF)` on a 0xFF-filled region
   produces a hit per byte — the scanner must early-exit a sector when its first 16 bytes are all
   0xFF (treat like zero sectors). MP3 sync `FF Fx` matches `FF FF FF FF…` ⇒ the 3-frame
   consistency check (bitrate idx 15 is invalid) rejects it.
3. **Pen drives / SD cards with controller "wrap-around" (fake capacity)**: the same data appears
   at `offset` and `offset + real_capacity`. Dedup (3.6) by BLAKE3 collapses them; report notes
   `duplicate_of`.
4. **4 KiB vs 512 B sectors, partition origin.** Cluster alignment must be computed from the
   partition start, not the image start (`partition_origin`); FAT32 data area start adds the
   reserved + FAT sectors (data region offset is in the filesystem module's `AllocMap.origin`).
   Without a partition table the PhotoRec block-size inference (3.1) recovers `(bs, off)`.
5. **Camera-written MP4/MOV cut by power loss**: `mdat` size field is a placeholder (0 or the
   pre-allocated size) and no `moov` exists. Status `truncated_no_moov`; length = distance to the
   next aligned header or cap. Many cameras pre-allocate `mdat` with a size and write `moov`
   *after* it (`ftyp mdat … moov`), so a healthy file needs the walk to seek over multi-GB `mdat` —
   `Cursor::skip` must be O(1).
6. **MOV with `moov` first and `mdat` size == 1 (64-bit)**, and `wide` + `mdat` pairs: handled by
   the grammar; `wide` is an 8-byte placeholder preceding `mdat`.
7. **Fragmented MP4 (`moof`/`mdat` pairs, e.g. phone screen recorders, DASH):** no single `moov`
   describes the data; `moov` has empty `trak` tables. The walk accepts repeated `moof`+`mdat`;
   status `Valid` if `mfra` or a clean end is seen, else `UnknownLength`.
8. **Live-recorded MKV/WebM (OBS, dashcams)**: unknown-size Segment and Clusters; no Cues; the
   element walk ends at the first non-cluster ID.
9. **AVCHD/MTS**: 192-byte packets, files > 2 GiB split by the camera into `00000.MTS`,
   `00001.MTS` that are continuous streams — a sync-run walk joins them into one object if they
   are contiguous on disk; the report cannot know the split points (note in `notes`).
10. **JPEG variants**: progressive (multiple SOS), arithmetic coding (SOF9/SOF10 — walk ok, decode
    validation skipped ⇒ max `structurally_valid`), 12-bit, CMYK/Adobe APP14, JPEG with EXIF *and*
    JFIF, JPEG with restart markers (needed for 3.9), MPO (two full JPEGs back-to-back inside one
    file — the second SOI is *aligned* only by chance; the MPF index tells the validator to absorb
    it), Samsung/Google trailers, Lossless JPEG (SOF3) inside DNG/CR2 raw strips — these embedded
    SOF3 streams start with `FF D8 FF C3` and must be attributed to the parent (R2).
11. **RAW previews are full-resolution JPEGs** (CR2 IFD0, NEF/ARW SubIFD, CR3 track 1). If the
    user asks only for "photos", promoting them (R5) when the raw is damaged is the single most
    valuable action on a failing card; when the raw is intact, promotion is off by default to avoid
    tripling the output.
12. **Thumbnail storms**: Windows `Thumbs.db`/`thumbcache_*.db` and browser caches contain
    thousands of tiny JPEGs at unaligned offsets. Aligned scanning plus the < 200×200 / ≤ 16 KiB
    rule keeps them out; `--align=1 --keep-thumbnails` lets a forensic user have them.
13. **Interleaved deleted files on FAT/exFAT**: the classic gap pattern (1–8 clusters of a newer
    file in the middle of an older JPEG). Bifragment search (3.9) targets exactly this; in
    `--unallocated=skip` mode the newer file's clusters are allocated and are skipped automatically,
    which recovers the old JPEG *without* 3.9 when the allocation map is intact.
14. **NTFS compressed/sparse/resident data**: compressed runs are LZNT1-encoded ⇒ headers are not
    visible to the carver; resident files are < ~700 B. Both are the filesystem module's job; the
    carver documents the limitation in `notes`.
15. **ext4 extents / inline data**: ext4 keeps most photo files contiguous (delayed allocation);
    deleted inodes have their extent tree zeroed by the kernel, so unallocated carving is the only
    path for deleted ext4 media — the allocation bitmap, however, remains valid.
16. **exFAT with "NoFatChain" contiguous files and a damaged allocation bitmap**: the carver runs
    whole-space with sector alignment; the bitmap is ignored when the filesystem module marks it
    inconsistent.
17. **Encrypted / compressed volumes (BitLocker, LUKS, APFS-encrypted, NTFS EFS, zip archives)**:
    nothing is carvable; the entropy sidecar (per-sector Shannon estimate from the zero/byte
    histograms the scanner already computes) lets the report say "N GB look encrypted/compressed".
18. **Same-byte-length false positives at scale**: a 2-byte `BM` or a 4-byte `00 00 01 BA` fires
    ~ once per 64 KiB / 4 GiB of random data respectively; at sector alignment the rates drop by 512×
    and the structural probes reject the rest. Scalpel's 783-byte GIF story is the cautionary tale:
    never emit on header/footer alone.
19. **Files larger than the cap**: 64 GiB caps exist to stop a run-walk (TS/PS) from eating a whole
    disk when the stream is corrupt; a genuinely larger file is reported `Capped` and the report
    says where it was cut.
20. **Repeated identical headers (burst mode)**: 100 photos with identical EXIF except timestamps
    are not duplicates; BLAKE3 over content keeps them apart.
21. **Little-endian/big-endian TIFF mixtures in NEF (MM) vs ARW (II)**: a single walker
    parameterised on byte order; never assume `II`.
22. **Bad-sector retries are not the carver's business**: the image is fixed; the carver must be
    rerunnable after the imager fills more sectors (idempotent output: same offsets ⇒ same names,
    `--resume` skips objects whose `(offset, len, blake3)` already exist in a previous report).

---

## 5. Test fixture strategy (only mkfs.vfat, mkfs.exfat, mkntfs + ntfscp, mkfs.ext4 -d, debugfs, ffmpeg, Pillow)

All fixtures are generated by `tests/fixtures/gen.py` into `target/fixtures/` (gitignored) and
cached by content hash; unit tests consume the raw images, integration tests the filesystem images.

### 5.1 Media samples

```bash
# Photos (Pillow). EXIF with DateTimeOriginal and an embedded thumbnail written by hand (piexif is
# not available): build the APP1 TIFF blob in Python, include IFD1 with tags 0x0201/0x0202.
python3 - <<'EOF'
from PIL import Image; import struct, io, os, random
os.makedirs("target/fixtures/media", exist_ok=True)
def thumb_exif(dt="2024:07:14 10:22:31"):
    th = Image.new("RGB",(160,120),(10,200,30)); b=io.BytesIO(); th.save(b,"JPEG",quality=50); th=b.getvalue()
    # TIFF LE: IFD0 {0x0132 DateTime, 0x8769 ExifIFD, next->IFD1}, ExifIFD {0x9003}, IFD1 {0x0201,0x0202}
    dtb = dt.encode()+b"\0"
    ifd0_off=8; ifd0 = struct.pack("<H",3)
    exif_off = ifd0_off+2+3*12+4
    dt_off  = exif_off+2+1*12+4
    ifd1_off = dt_off+len(dtb)+len(dtb)
    thumb_off = ifd1_off+2+2*12+4
    ifd0 += struct.pack("<HHII",0x0132,2,len(dtb),dt_off)
    ifd0 += struct.pack("<HHII",0x8769,4,1,exif_off)
    ifd0 += struct.pack("<HHII",0x0100,4,1,6000)          # dummy ImageWidth to keep 3 entries sorted-ish
    ifd0 += struct.pack("<I",ifd1_off)
    exif = struct.pack("<H",1)+struct.pack("<HHII",0x9003,2,len(dtb),dt_off+len(dtb))+struct.pack("<I",0)
    ifd1 = struct.pack("<H",2)+struct.pack("<HHII",0x0201,4,1,thumb_off)+struct.pack("<HHII",0x0202,4,1,len(th))+struct.pack("<I",0)
    tiff = b"II*\0"+struct.pack("<I",8)+ifd0+exif+dtb+dtb+ifd1+th
    return b"Exif\0\0"+tiff
for i,(w,h) in enumerate([(640,480),(3000,2000),(6000,4000)]):
    im = Image.effect_noise((w,h),64).convert("RGB")          # high-entropy ⇒ realistic size
    im.save(f"target/fixtures/media/photo{i}.jpg","JPEG",quality=90,exif=thumb_exif())
    im.save(f"target/fixtures/media/photo{i}_prog.jpg","JPEG",quality=85,progressive=True)
    im.save(f"target/fixtures/media/photo{i}_rst.jpg","JPEG",quality=85,restart_marker_blocks=1)  # Pillow ≥ 10.2? UNVERIFIED
    im.save(f"target/fixtures/media/photo{i}.png","PNG")
    im.convert("P").save(f"target/fixtures/media/photo{i}.gif","GIF")
    im.save(f"target/fixtures/media/photo{i}.bmp","BMP")
    im.save(f"target/fixtures/media/photo{i}.tif","TIFF")
    try: im.save(f"target/fixtures/media/photo{i}.webp","WEBP")   # needs Pillow built with libwebp; skip if not UNVERIFIED
    except Exception: pass
Image.new("RGB",(100,100)).save("target/fixtures/media/tiny_thumb.jpg","JPEG")   # must be rejected as thumbnail
EOF

# Videos / audio (ffmpeg with libx264, libx265, aac, native mpeg2video/mjpeg/flac/pcm/ac3).
S="-f lavfi -i testsrc2=size=1280x720:rate=30:duration=4 -f lavfi -i sine=frequency=440:duration=4"
ffmpeg -y $S -c:v libx264 -c:a aac -movflags +faststart video_faststart.mp4      # moov first
ffmpeg -y $S -c:v libx264 -c:a aac video_moovlast.mp4                            # moov last (camera-like)
ffmpeg -y $S -c:v libx264 -c:a aac -f mov video.mov
ffmpeg -y $S -c:v libx264 -c:a aac -f 3gp -s 352x288 video.3gp
ffmpeg -y $S -c:v libx264 -c:a aac -f ipod video.m4v                             # brand M4V
ffmpeg -y $S -vn -c:a aac -f ipod audio.m4a                                      # brand M4A
ffmpeg -y $S -c:v libx265 -c:a aac -tag:v hvc1 video_hevc.mp4
ffmpeg -y $S -c:v libx264 -c:a aac -movflags frag_keyframe+empty_moov video_fragmented.mp4
ffmpeg -y $S -c:v libx264 -c:a aac -f matroska video.mkv
ffmpeg -y $S -c:v libvpx-vp9 -c:a libopus video.webm   # only if libvpx/libopus present UNVERIFIED; else -c:v libx264 -f webm fails
ffmpeg -y $S -c:v mjpeg -q:v 3 -c:a pcm_s16le video_mjpeg.avi                    # JPEG-in-AVI nesting
ffmpeg -y $S -c:v libx264 -c:a aac -f mpegts video.ts                            # 188-byte
ffmpeg -y $S -c:v libx264 -c:a aac -f mpegts -mpegts_m2ts_mode 1 video.m2ts     # 192-byte UNVERIFIED flag name
ffmpeg -y $S -c:v mpeg2video -c:a ac3 -f vob video.vob                           # 2048-byte packs
ffmpeg -y $S -c:v mpeg2video -c:a mp2 -f mpeg video.mpg
ffmpeg -y $S -vn -c:a pcm_s16le audio.wav
ffmpeg -y $S -vn -c:a flac audio.flac
ffmpeg -y $S -vn -c:a libmp3lame -id3v2_version 3 audio.mp3   # libmp3lame presence UNVERIFIED; fallback: skip MP3 fixture
ffmpeg -y $S -vn -c:a libmp3lame -write_id3v2 0 -write_xing 1 audio_bare.mp3
# Large AVI > 1 GiB to exercise OpenDML AVIX chaining (only in the "slow" test tier):
ffmpeg -y -f lavfi -i testsrc2=size=1920x1080:rate=30:duration=600 -c:v mjpeg -q:v 2 big_opendml.avi
```

Formats ffmpeg/Pillow cannot produce (HEIC/AVIF/CR2/NEF/ARW/DNG/PEF/ORF/RW2/RAF/CR3/PSD) get
**structurally valid synthetic files** from `gen.py`: a minimal TIFF writer parameterised by
byte order, Make string, DNGVersion tag, CR2 `CR\x02\x00` header and IFD3 raw strip, ORF/RW2 magic
swaps and a dummy raw strip; a RAF with the 108-byte header pointing at a Pillow JPEG and a dummy
CFA block; an ISOBMFF writer emitting `ftyp`(brand) + `meta`(hdlr/pitm/iinf/iloc) + `mdat` for
HEIC/AVIF and `ftyp crx ` + `moov`(uuid 85c0b687…, THMB=JPEG) + `uuid`(PRVW) + `mdat` for CR3;
PSD with raw and RLE image data. These test the walkers and length computation, not decodability.
An optional `FIXTURE_REAL_RAWS=1` tier downloads CC0 samples from raw.pixls.us for manual runs
(never in CI).

### 5.2 Raw carving images (no filesystem)

`gen.py raw` builds `*.img` + `*.truth.json` (expected objects with offset/len/format/status):

- `contig.img`: media files back-to-back at 4 KiB alignment separated by random-length fills of
  (a) zeros, (b) 0xFF, (c) `os.urandom`, (d) text. Truth = every file `valid`.
- `unaligned.img`: same but at random byte offsets ⇒ with default `--align=4096` nothing but the
  aligned ones is found; with `--align=1` all are found.
- `nested.img`: a CR2-like TIFF containing a JPEG; MJPEG AVI; MP3 with APIC cover; MP4 with
  `covr` — truth asserts the inner JPEGs are `nested`, not output.
- `bifrag.img`: `photo2.jpg` split at a 4 KiB boundary with gaps of 1, 2, 4, 8, 16, 32, 64 clusters
  filled with (i) zeros, (ii) random, (iii) another JPEG's middle, (iv) a complete small JPEG
  (the "newer file" case). Truth lists both fragments; also a variant where the split is *not* on
  a cluster boundary (must stay `Truncated`).
- `truncated.img`: every format cut at 60% ⇒ `Truncated`, with `err_offset` ≤ cut + 64 KiB.
- `holes.img`: `contig.img` with 4 KiB ranges zeroed plus a synthetic ddrescue mapfile marking
  them `-` ⇒ `unreadable_inside` counts match.
- `weak_sigs.img`: 64 MiB random data ⇒ zero objects (false-positive budget: 0 at default
  alignment; ≤ 2 with `--align=1` for MP3/PS, reported as `rejected`).
- `fake_capacity.img`: `contig.img` repeated twice ⇒ second copy `duplicate_of`.
- `motion_photo.img`: JPEG + `ftyp` immediately after EOI with XMP `GCamera:MicroVideo="1"` ⇒
  one object with trailer.

### 5.3 Filesystem images (unallocated mode, dedup)

```bash
# ext4: build populated, delete two files with debugfs (data blocks stay, bitmap frees them)
mkdir -p tree && cp target/fixtures/media/{photo1.jpg,video_moovlast.mp4,audio.flac} tree/
truncate -s 256M ext4.img && mkfs.ext4 -q -F -b 4096 -d tree ext4.img
debugfs -w -R "rm /photo1.jpg" ext4.img
debugfs -w -R "rm /video_moovlast.mp4" ext4.img
# truth: filesystem module recovers audio.flac; carver in --unallocated=strict finds the jpg and mp4

# NTFS: populate with ntfscp (no mount); deletion without mount is not possible with the listed
# tools, so the NTFS test covers whole-space + dedup only.
truncate -s 256M ntfs.img && mkntfs -F -Q -c 4096 ntfs.img
for f in tree/*; do ntfscp ntfs.img "$f" /; done

# FAT32 / exFAT: mkfs creates empty volumes; populate by writing clusters + directory entries
# directly from gen.py (small FAT32/exFAT writer: root dir entry, FAT chain, data clusters).
# Deleted-file case = first byte of the 8.3 name set to 0xE5 and FAT chain zeroed, data untouched.
truncate -s 256M fat32.img && mkfs.vfat -F 32 -s 8 fat32.img && python3 gen.py fat32-populate fat32.img tree/
truncate -s 256M exfat.img && mkfs.exfat -c 32K exfat.img && python3 gen.py exfat-populate exfat.img tree/
# Interleaving fixture for 3.9 on FAT32: write A (jpg) then B (small file) then delete A, and
# arrange cluster allocation so B occupies clusters 8..9 of A's original run.
```

Expectations per image: (1) `--unallocated=strict` finds exactly the deleted files; (2) whole-space
finds all and marks live ones `duplicate_of` the filesystem path; (3) on the FAT32 interleaving
image, `--fragmented=jpeg` reproduces the original JPEG bit-exactly (BLAKE3 equal to `tree/`).

### 5.4 Unit-level checks

- Signature table round trip: every `Signature.bytes` appears exactly once in the aho-corasick set
  or in a memchr/memmem path; a property test feeds 1 GiB of `/dev/urandom`-like data (seeded
  `rand_xoshiro`) and asserts the probe false-positive rate ≤ 1 per 256 MiB at 512-byte alignment.
- Each validator: `measure(valid)` == real file length; `measure(valid + 1 MiB garbage)` == real
  length (`appended_data_ignored`); `measure(valid[..k])` for k in 10 random cuts ⇒ `Truncated` with
  `len ≤ k`; CRC/decoder tests on bit-flipped copies.
- Resolver: table-driven cases for R1–R5 and partial overlaps.
- Writer: kill the process mid-pass-2 (test harness with `fork`) ⇒ only `.part` files are
  incomplete; `--resume` finishes with identical names and hashes.
- Throughput smoke test (ignored by default): scan `weak_sigs.img` at ≥ 1 GB/s from page cache on
  the dev box; measure-pool scaling with 1, 2, 4, 8 workers on `contig.img`.

---

## 6. Recommended Rust crates

| Crate | Version | Use | Notes |
|---|---|---|---|
| `memchr` | 2.8.3 | memchr/memchr2/memchr3, `memmem::Finder` for `00 00 01` | SIMD on x86_64 (SSE2/AVX2 runtime detect), aarch64, wasm32 |
| `aho-corasick` | 1.1.5 | fixed-magic multi-pattern search, `find_overlapping_iter`; Teddy prefilter auto | keep pattern count < ~100 for Teddy |
| `rayon` | 1.12.0 | measurement pool, bifragment trials, BLAKE3 `update_rayon` | |
| `crossbeam-channel` | 0.5.17 | bounded scanner→pool channel, results channel | |
| `blake3` | 1.8.7 | content hashes for dedup and report | `rayon` feature |
| `memmap2` | 0.9.11 | optional mmap `Source` for image files on healthy storage | off by default (SIGBUS on I/O error) |
| `zune-jpeg` | 0.5.16-rc2 (pin a stable 0.4.x if rc is unacceptable) | fast decode for `decode` validation level *only when* the own Huffman walker says the stream is complete; it does not expose the byte offset of a decode error, hence the in-house decoder for `err_offset` | own baseline+progressive Huffman scanner is required for 3.9 regardless |
| `png` | 0.17.x UNVERIFIED | optional inflate/adler check | CRC done with `crc32fast` |
| `crc32fast` | 1.4.x UNVERIFIED | PNG chunk CRC | |
| `kamadak-exif` | 0.6.1 | EXIF parse for capture date/make/model from JPEG/TIFF/HEIF Exif items | pure Rust |
| `symphonia` | 0.5.x UNVERIFIED | optional MP3/FLAC frame decode for `decode` level | feature-gated |
| `serde` / `serde_json` | 1.0.x UNVERIFIED | report | |
| `jiff` | 0.2.x UNVERIFIED | date math for 1904/2001 epochs and YYYY/MM folders | or `time` 0.3.x |
| `rangemap` | 1.5.x UNVERIFIED | `RangeSet<u64>` for allocation, attribution, dedup | small; could be in-house BTreeMap |
| `tempfile` | 3.x UNVERIFIED | `.part` outputs, spill file | |
| `rand_xoshiro` | 0.7.x UNVERIFIED | deterministic fixture noise in tests | dev-dependency |

No crate is used for ISOBMFF/EBML/RIFF/TIFF/TS/PS walking: the walkers are a few hundred lines
each, must be streaming and lenient, and must expose `err_offset` — existing parsers are strict and
in-memory. `mp4parse`/`matroska` crates are explicitly not used for the same reason.

---

## 7. Sources

- Garfinkel, S. L. "Carving contiguous and fragmented files with fast object validation",
  Digital Investigation 4S (2007) S2–S12. https://simson.net/clips/academic/2007.DFRWS.pdf
  (fragmentation tables 1–8, validator flags, bifragment gap carving, Mars example).
- Richard, G. G. III; Roussev, V. "Scalpel: A Frugal, High Performance File Carver", DFRWS 2005.
  https://cct.lsu.edu/~golden/Papers/scalpel2005.pdf (two-pass work queues, Foremost read
  amplification k = 1..45, GIF false-positive counts).
- Pal, A.; Sencar, H. T.; Memon, N. "Detecting file fragmentation point using sequential
  hypothesis testing", Digital Investigation 5 (2008) — DFRWS 2008 best paper.
  https://dfrws.org/presentation/detecting-file-fragmentation-point-using-sequential-hypothesis-testing/
  (abstract); formulas confirmed from US patent application US 2010/0257146 A1
  https://patents.google.com/patent/US20100257146A1/en ; results summary
  https://forensics.wiki/file_carving_smartcarving/ .
- PhotoRec documentation: https://www.cgsecurity.org/wiki/PhotoRec_Step_By_Step (free vs whole
  space, pass 0 block size from first 10 files, expert mode, paranoid/bruteforce, keep corrupted,
  low memory).
- PhotoRec/TestDisk sources (github.com/cgsecurity/testdisk, `src/`): `filegen.h`
  (`file_hint_t`, `file_recovery_t`, `DC_*`, `PHOTOREC_MAX_FILE_SIZE`), `photorec.c`
  (`find_blocksize`, `update_blocksize`), `file_jpg.c` (`header_check_jpg`, `data_check_jpg`,
  `file_check_jpg`, thumbnail rule, MPO), `file_tiff_le.c`/`file_tiff.c` (CR2/DNG/SONY/NIKON
  detection, IFD end computation), `file_mov.c` (brand list, atom walk), `file_riff.c` (AVI/WAVE/
  WEBP, AVIX chaining, even padding), `file_mkv.c` (EBML vint, DocType), `file_mp3.c` (sync bytes,
  tables, syncsafe size), `file_m2ts.c` (188/192 strides, HDMV/HDPR/TSHV labels), `file_mpg.c`
  (start codes BA/B3/B5/BB), `file_png.c`, `file_gif.c`, `file_bmp.c` (DIB sizes, min 65),
  `file_psd.c`, `file_flac.c` (1.5 GiB cap), `file_raf.c` (108-byte header), `file_rw2.c` (`IIU\0`),
  `file_orf.c` (`IIRO`/`IIRS`), `file_list.c`.
- ISO/IEC 14496-12 (ISOBMFF box grammar) and the MP4 registration authority brand list
  https://mp4ra.org/registered-types/brands (heic/heix/hevc/hevx/heim/heis/hevm/hevs/mif1/msf1/
  avif/avis/avio/isom/iso2/mp41/mp42/M4V/M4A/M4B/M4P/qt/3gp*/XAVC/MSNV/dash/jp2).
- Canon CR3 structure: https://github.com/lclevy/canon_cr3 (ftyp `crx `, uuid
  85c0b687-820f-11e0-8111-f4ce462b6a48 with CNCV/CMT1–4/THMB, uuid eaf42b5e-1c98-4b88-b9fb-b7dc406e4d16
  with PRVW, four-track mdat layout).
- ITU-T T.81 (JPEG), JFIF 1.02, CIPA DC-008 (EXIF) / DC-007 (MPO); ISO/IEC 15948 (PNG);
  GIF89a specification; TIFF 6.0; EBML RFC 8794 and the Matroska element registry; ISO/IEC
  13818-1 (TS/PS); ISO/IEC 11172-3 / 13818-3 (MPEG audio); FLAC format specification; Adobe
  Photoshop File Formats Specification. (Standard references; field layouts in section 2 are
  transcribed from these, not re-fetched during this research pass.)
- `memchr` crate: https://docs.rs/memchr (2.8.3; SIMD backends); third-party throughput figures:
  https://github.com/ashvardanian/memchr_vs_stringzilla .
- `aho-corasick` crate packed/Teddy docs: https://docs.rs/aho-corasick/latest/aho_corasick/packed/index.html (1.1.5).
- crates.io API for versions: blake3 1.8.7, rayon 1.12.0, crossbeam-channel 0.5.17, memmap2 0.9.11,
  zune-jpeg 0.5.16-rc2, kamadak-exif 0.6.1 (all checked 2026-10-12).
- forensics.wiki "File carving": https://forensics.wiki/file_carving/ (Garfinkel percentages
  restated: 58% PST, 17% JPEG, 16% Word).
