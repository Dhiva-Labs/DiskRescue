# Still-image formats: carving, validation and repair

Status: implementation spec for the DiskRescue carver (`carve::image`) and repair modules
(`repair::jpeg`, `repair::png`). Fact-check pass 2026-10-12: all byte-level claims in
sections 2 and 6 were verified against primary sources (T.81 / libjpeg `jstdhuff.c` by
script, ExifTool module sources, PhotoRec, dcraw, lclevy, libopenraw, crate sources and a
fixture run against the real crates); corrections are in place. The few lines still tagged
`UNVERIFIED` are performance estimates or heuristics that only a fixture/benchmark run can
settle, and must not be relied on until measured.

Conventions used in this document:

- `u16be` / `u32be` = big-endian unsigned; `u16le` / `u32le` = little-endian. Offsets are
  decimal unless prefixed with `0x`. Byte sequences are hex, space separated.
- "Extent" = exact number of bytes from the first byte of the magic to the last byte of the
  file as the carver should emit it.
- "Validation level": L0 = magic only, L1 = header structure parsed and self-consistent,
  L2 = entropy/compressed data walked or symbol-decoded without pixel reconstruction,
  L3 = full decode by a library.

---

## 1. Overview and scope

### 1.1 What the image carver must do

For every format below the carver needs four primitives:

1. **find start**: scan the image for a magic at a sector- or cluster-aligned offset (and,
   for JPEG only, also at unaligned offsets, because JPEGs embedded inside other files are
   worth recovering too, see 2.1.11).
2. **find exact end**: compute the extent from the structure, never by searching for a
   trailer blindly (JPEG is the notorious exception where a trailer search is needed, but it
   must be done *after* walking the header segments, see 2.1.6).
3. **validate**: reject false positives with the cheapest check that catches them (L1), then
   optionally go deeper (L2/L3) to grade the output as `good`, `partial`, `damaged`.
4. **repair** (JPEG, PNG; limited for others): turn a `partial`/`damaged` file into something
   a viewer will open, without inventing pixel content.

### 1.2 Priorities

Formats are listed in priority order for a photo/video recovery tool:

| Priority | Format(s) | Why |
|---|---|---|
| P0 | JPEG (JFIF/Exif), MPO | Every phone and camera; most common recovery request |
| P0 | HEIC/HEIF (Apple since 2017), AVIF | Default on iPhone |
| P1 | PNG | Screenshots, graphics |
| P1 | TIFF-based RAW (CR2, NEF, ARW, DNG, PEF, ORF, RW2, ERF, SRW), RAF, CR3 | Camera cards |
| P2 | WebP, GIF, BMP, TIFF, PSD | Less common on cards, common on PCs |

### 1.3 Fragmentation reality check

Carving from a raw image recovers **contiguous** files only. On FAT/exFAT SD cards written
sequentially by a camera, photos are usually contiguous; on NTFS/ext4 system disks and on
cards that were partially deleted and refilled, a significant share of large files is
fragmented. Measured figures (Garfinkel, "Carving contiguous and fragmented files with fast
object validation", DFRWS 2007, Table 1/3, 324-drive corpus): 6 % of all files, **16 % of
JPEGs**, 20 % of AVIs, 17 % of MPEGs were in two or more fragments; roughly half the drives
had no fragmented file at all, while the worst drive had 43 % of its JPEGs fragmented
(Digital Investigation 4S (2007) S2-S12, doi:10.1016/j.diin.2007.06.017; author copy https://simson.net/clips/academic/2007.DFRWS.pdf).
The carver must therefore:

- Prefer the filesystem-parser path (which follows the cluster chain) and only carve the
  unallocated/unreadable remainder.
- Detect "bi-fragment" JPEGs: validator reports the scan stops decoding cleanly at a cluster
  boundary (offset multiple of cluster size) with garbage after it; record this in the report
  as `fragmented_at = offset` so a later "gap carving" pass can try to splice the next
  cluster run that continues the Huffman stream (out of scope for v1 but the validator must
  expose the offset of the last good MCU and its byte offset to make it possible).

### 1.4 Safety invariants

- Never write to the source device or image. All output goes to the output directory.
- Every repair writes a new file; the carved original is kept next to it as `<name>.orig`.
- No repair step may discard bytes that a later step might need: truncation is applied to a
  copy.

---

## 2. Exact on-disk details

### 2.1 JPEG (ITU-T T.81 / ISO 10918-1, JFIF, Exif, MPF)

#### 2.1.1 Marker list

Every marker is `FF xx`. Markers with a segment payload are followed by `Lx = u16be`, the
length **including the two length bytes but excluding the marker**. Markers without a
payload: SOI, EOI, RSTn, TEM.

| Marker | Hex | Payload | Notes |
|---|---|---|---|
| SOI | FF D8 | none | Start of image; must be first two bytes |
| APP0..APP15 | FF E0..FF EF | yes | APP0 = JFIF, APP1 = Exif / XMP, APP2 = ICC / MPF, APP13 = Photoshop IRB, APP14 = Adobe |
| DQT | FF DB | yes | Quantisation table(s) |
| SOF0 | FF C0 | yes | Baseline DCT, Huffman |
| SOF1 | FF C1 | yes | Extended sequential, Huffman |
| SOF2 | FF C2 | yes | Progressive DCT, Huffman |
| SOF3 | FF C3 | yes | Lossless, Huffman (used by CR2/DNG raw payloads) |
| SOF5/6/7 | FF C5..C7 | yes | Differential sequential/progressive/lossless (never seen in cameras) |
| JPG | FF C8 | reserved | Treat as invalid |
| SOF9/10/11 | FF C9..CB | yes | Arithmetic sequential/progressive/lossless |
| SOF13/14/15 | FF CD..CF | yes | Differential arithmetic |
| DHT | FF C4 | yes | Huffman table(s) |
| DAC | FF CC | yes | Arithmetic conditioning |
| SOS | FF DA | yes | Start of scan; entropy data follows the segment |
| DRI | FF DD | yes, Lr = 4 | Restart interval |
| RST0..RST7 | FF D0..FF D7 | none | Only inside entropy data |
| DNL | FF DC | yes, Ld = 4 | Defines number of lines when SOF has Y = 0 (rare) |
| DHP | FF DE | yes | Hierarchical (never seen) |
| EXP | FF DF | yes | Hierarchical expand |
| JPG0..JPG13 | FF F0..FF FD | reserved | Treat as invalid |
| COM | FF FE | yes | Comment |
| TEM | FF 01 | none | Arithmetic temporary; treat as invalid in headers |
| EOI | FF D9 | none | End of image |
| (fill) | FF FF | n/a | Any number of FF bytes may precede a marker; `FF FF D8` is a valid SOI prefix |
| (stuffed) | FF 00 | n/a | Inside entropy data only: literal 0xFF byte |

Reserved range FF 02..FF BF: not markers; treat as invalid.

#### 2.1.2 Segment payload layouts

**SOFn** (`FF Cn`):

| Offset in segment | Size | Field |
|---|---|---|
| 0 | u16be | Lf = 8 + 3*Nf |
| 2 | u8 | P = sample precision (8 for baseline; 12 for extended; 2-16 for lossless) |
| 3 | u16be | Y = number of lines (0 allowed, then a DNL marker defines it) |
| 5 | u16be | X = samples per line (must be > 0) |
| 7 | u8 | Nf = components (1, 3 or 4; baseline allows 1-255 but cameras use 1/3/4) |
| 8 + 3i | u8 | Ci = component id (JFIF uses 1,2,3; Adobe RGB uses 'R','G','B' = 0x52,0x47,0x42) |
| 9 + 3i | u8 | Hi (high nibble, 1-4) / Vi (low nibble, 1-4) sampling factors |
| 10 + 3i | u8 | Tqi = quant table selector (0-3) |

**DQT** (`FF DB`): `Lq`, then repeated until Lq is consumed: `u8` Pq (high nibble: 0 = 8-bit,
1 = 16-bit entries) / Tq (low nibble 0-3), then 64 entries in **zigzag order** (64 or 128
bytes). Lq = 2 + sum(65 or 129).

**DHT** (`FF C4`): `Lh`, then repeated until consumed: `u8` Tc (high nibble: 0 = DC, 1 = AC)
/ Th (low nibble: 0-3; baseline allows only 0-1), `BITS[1..16]` (16 bytes: count of codes of
each length), `HUFFVAL[0..n)` where n = sum(BITS) (n <= 256; DC tables use 12, AC tables 162
with standard tables). Lh = 2 + sum(17 + n).

**SOS** (`FF DA`):

| Offset | Size | Field |
|---|---|---|
| 0 | u16be | Ls = 6 + 2*Ns |
| 2 | u8 | Ns = components in scan (1-4) |
| 3 + 2j | u8 | Csj = component selector (must match a Ci from SOF) |
| 4 + 2j | u8 | Tdj (high nibble, DC table) / Taj (low nibble, AC table) |
| 3 + 2Ns | u8 | Ss = start of spectral selection (0 for baseline) |
| 4 + 2Ns | u8 | Se = end of spectral selection (63 for baseline) |
| 5 + 2Ns | u8 | Ah (high) / Al (low) successive approximation bit positions (0/0 baseline) |

Entropy-coded data begins immediately at `segment_start + Ls` and continues until a marker
other than RSTn (and other than the stuffing FF 00 / fill FF FF) is found.

**DRI** (`FF DD`): `Lr = 4`, `Ri = u16be` = number of MCUs between restart markers. Ri = 0
disables restarts. A DRI may appear between scans and changes Ri for subsequent scans.

**APP0 JFIF**: `"JFIF\0"` (4A 46 49 46 00), `u16be` version (0x0101 or 0x0102), `u8` units
(0 = none, 1 = dpi, 2 = dpcm), `u16be` Xdensity, `u16be` Ydensity, `u8` Xthumbnail, `u8`
Ythumbnail, then 3*Xt*Yt bytes RGB thumbnail. APP0 `"JFXX\0"` = JFIF extension (thumbnail
coded as JPEG (0x10), 1-byte palette (0x11) or RGB (0x13)).

**APP1 Exif**: `"Exif\0\0"` (45 78 69 66 00 00) then a complete TIFF structure (2.1.9).
Length cap: a single segment holds <= 65533 payload bytes; Exif thumbnails are usually
< 20 KB. Some encoders split large metadata over several APP1 segments (Extended XMP uses
APP1 `"http://ns.adobe.com/xmp/extension/\0"` + 32-byte GUID + u32be full length + u32be
offset).

**APP1 XMP**: `"http://ns.adobe.com/xap/1.0/\0"` then UTF-8 XML packet.

**APP2 ICC**: `"ICC_PROFILE\0"`, `u8` sequence number (1-based), `u8` total count, profile
bytes. **APP2 MPF**: `"MPF\0"` (4D 50 46 00) then a TIFF header (2.1.7).

**APP13 Photoshop**: `"Photoshop 3.0\0"`, then 8BIM resource blocks (`"8BIM"`, u16be id, Pascal
string name padded to even, u32be size, data padded to even). Resource 0x0404 = IPTC-NAA,
0x040C = thumbnail (contains a small JPEG: **another SOI/EOI pair inside a header segment**).

**APP14 Adobe**: `"Adobe"` (41 64 6F 62 65), `u16be` version (100), `u16be` flags0, `u16be`
flags1, `u8` transform: 0 = none (RGB or CMYK), 1 = YCbCr, 2 = YCCK. Needed to get colours
right for 3-/4-component images without JFIF.

#### 2.1.3 Segment length semantics and header walk

```
walk_headers(buf, pos):                # pos points at the SOI
    assert buf[pos..pos+2] == FF D8
    pos += 2
    loop:
        while buf[pos] == 0xFF and buf[pos+1] == 0xFF: pos += 1     # fill bytes
        if buf[pos] != 0xFF: return Err(NoMarker(pos))
        m = buf[pos+1]
        if m == 0xD8: return Err(NestedSOI)          # second SOI in header = corrupt or concatenated
        if m == 0xD9: return Err(EoiBeforeScan)
        if m in D0..D7 or m == 0x01: return Err(StrayRst)
        if m == 0xDA:                                 # SOS
            L = u16be(buf[pos+2..]); if L < 8: return Err(BadLen)
            parse_sos(buf[pos+2 .. pos+2+L])
            return Ok(scan_data_start = pos + 2 + L)
        if m in {C0..CF} - {C4, C8, CC} and sof_seen: return Err(TwoSof)
        L = u16be(buf[pos+2..])
        if L < 2: return Err(BadLen)
        if m == 0xDD and L != 4: return Err(BadLen)
        if pos + 2 + L > buf.len(): return Err(Truncated)
        parse_segment(m, buf[pos+4 .. pos+2+L])   # APPn/DQT/DHT/SOF/DRI/COM
        pos += 2 + L
```

Rules the validator applies during the walk (all cheap, all catch real false positives):

- First marker after SOI must carry a length (practically always APPn, DQT or COM; Motion
  JPEG frames may start directly with DQT or SOF0).
- SOF before SOS, exactly one SOF (except hierarchical, ignore).
- SOF: P in {8, 12} for DCT SOFs; X > 0; Nf in 1..=4; each Hi,Vi in 1..=4; Tqi <= 3; a 3- or
  4-component image where every Hi*Vi == 1 is fine; sum(Hi*Vi) over an interleaved scan must
  be <= 10 (T.81 B.2.3).
- SOS: Ns in 1..=4, every Cs matches a SOF Ci, Td/Ta <= 3 (<= 1 for baseline), Ss/Se/Ah/Al
  valid for the SOF type (2.1.5).
- All quant tables and Huffman tables referenced by the scan are defined (except when the
  DHT-less Motion-JPEG convention is detected, 3.6).
- Reject if total header length (SOI..SOS) > 16 MiB (chosen default, not a sourced figure;
  biggest legitimate headers are multi-segment XMP + ICC + MPF, a few MB; make it a
  `--max-jpeg-header` option).

#### 2.1.4 Entropy-coded data, stuffing, fill, restart markers

Inside scan data:

| Sequence | Meaning |
|---|---|
| `FF 00` | one literal 0xFF data byte |
| `FF FF` | fill byte; skip the first FF and re-examine (`FF FF 00` is literal FF after a fill) |
| `FF D0..FF D7` | RSTn; the bit reader is byte-aligned before it; the encoder pads the last byte with 1-bits |
| `FF D9` | EOI (padding 1-bits before it) |
| `FF C4 / DB / DA / DD / DC / Ex / FE` | next segment of a multi-scan (progressive) file, or DNL |
| `FF 01..FF BF` (other), `FF D8` | invalid inside scan data (corruption, or next file) |

Expected RST sequence: RST0, RST1, ..., RST7, RST0, ... every `Ri` MCUs. A gap in the
sequence (RST2 followed by RST5) tells the repair module exactly how many MCU intervals were
lost (3 x Ri MCUs) and lets it keep the geometry correct by inserting empty intervals (3.5).

#### 2.1.5 Baseline vs progressive scan structure

| | Baseline / extended sequential (SOF0/SOF1) | Progressive (SOF2) |
|---|---|---|
| Scans | usually 1 interleaved scan (Ns = Nf). Some encoders emit one non-interleaved scan per component (3 scans), legal. | Many scans (typically 6-10). DC first scan(s) may be interleaved; every AC scan has Ns = 1. |
| SOS fields | Ss = 0, Se = 63, Ah = Al = 0 | DC scan: Ss = 0, Se = 0. AC scan: 1 <= Ss <= Se <= 63. First pass Ah = 0, refinement Ah = Al_prev = Al + 1. Al <= 13 (practically <= 3) |
| Table needs | DC and AC tables for every component | DC scans need DC table only; AC scans need AC table only; refinement scans need the table too (AC refinement decodes RS symbols) |
| Truncation behaviour | bottom of image lost | later scans lost: whole image present at lower quality / blurred chroma, viewers show it fully |
| Repair strategy | re-emit entropy stream up to last good MCU (3.5) | cut at end of last complete scan, append EOI |

MCU geometry (T.81 A.1.1, A.2):

```
Hmax = max(Hi), Vmax = max(Vi)
mcu_w_px = 8*Hmax, mcu_h_px = 8*Vmax
mcus_x = ceil(X / mcu_w_px), mcus_y = ceil(Y / mcu_h_px)
interleaved scan (Ns > 1): MCU = for each scan component j: Hj*Vj blocks (row-major Vj rows of Hj blocks)
                           total MCUs = mcus_x * mcus_y
non-interleaved scan (Ns = 1, component c): MCU = 1 block
                           blocks_x = ceil( ceil(X * Hc / Hmax) / 8 )
                           blocks_y = ceil( ceil(Y * Vc / Vmax) / 8 )
                           total MCUs = blocks_x * blocks_y   (note: NOT mcus_x*Hc*mcus_y*Vc - edge blocks are not padded in non-interleaved scans)
```

Example: 4:2:0 (Y H=2,V=2; Cb,Cr 1,1), 4000x3000: Hmax=Vmax=2, MCU = 16x16 px, 250x188 MCUs =
47000 MCUs, each 6 blocks (4 Y + Cb + Cr). Restart interval `Ri` counts MCUs.

#### 2.1.6 Finding the true EOI

Naive `find(FF D9)` fails in three common situations:

1. **Exif thumbnail in APP1**: the TIFF inside APP1 has IFD1 with `0x0201 JPEGInterchangeFormat`
   / `0x0202 JPEGInterchangeFormatLength` pointing to a complete small JPEG (own SOI and EOI)
   *inside the APP1 segment*. A blind search finds the thumbnail's `FF D9` after ~5-20 KB and
   emits a 20 KB file that opens as a 160x120 thumbnail.
2. **Photoshop APP13 thumbnail** (resource 0x040C): same problem.
3. **MPF (APP2 "MPF\0")**: Samsung (since 2014), Apple (portrait depth, HDR gain map since
   iOS 14/iPhone 12), Sony, Canon, Nikon and all `.mpo` 3D cameras append **additional complete
   JPEG images after the main image's EOI**. A search that stops at the first EOI after the
   scan loses them; a search that ignores structure and takes the last EOI in the buffer
   over-reads into the next file.

Correct procedure:

```
find_jpeg_end(buf, soi):
    hdr = walk_headers(buf, soi)                 # skips every APPn by its length -> thumbnails never seen
    pos = hdr.scan_data_start
    loop:                                        # scan-data walk (progressive: repeats per scan)
        pos = skip_entropy_data(buf, pos)        # stops at first FF xx with xx not in {00, FF, D0..D7}
        m = buf[pos+1]
        if m == 0xD9: main_eoi_end = pos + 2; break
        if m in {C4, DB, DA, DD, DC, E0..EF, FE}:   # more segments (progressive / DNL)
            parse segment as in walk_headers; if SOS then continue the loop; else continue
        else: return Partial(last_good = pos)    # corruption or next file
    end = main_eoi_end
    if hdr.mpf is Some:
        for entry in hdr.mpf.entries:            # see 2.1.7
            if entry.offset == 0: continue       # first image
            img_start = hdr.mpf.tiff_header_offset + entry.offset
            img_end = img_start + entry.size
            if img_start >= main_eoi_end and buf[img_start..img_start+2] == FF D8:
                end = max(end, img_end)
    end = check_trailers(buf, end)               # 2.1.8 (Samsung SEFT, Motion Photo)
    return Ok(end)
```

`skip_entropy_data`: a tight byte loop looking for 0xFF (memchr); on `FF 00` and `FF FF`
continue; on `FF D0..D7` continue; otherwise return position of the FF. For validation at
L2 the same loop is replaced by the Huffman symbol decoder (3.2) which also counts MCUs.

The scan-data walk must be bounded: `max_jpeg_size` (default 256 MiB, a chosen cap, not a
sourced figure; PhotoRec's hard cap is 50 MiB (`file_jpg.c`: `.max_filesize=50*1024*1024`),
which is too small for 100-megapixel bodies and for MPF depth/gain-map files; the Phase One
IQ4 at 151 MP produces JPEGs > 60 MB (UNVERIFIED figure)).

#### 2.1.7 MPF (CIPA DC-007) layout

APP2 payload: `"MPF\0"`, then TIFF header at offset `T` (= segment payload start + 4):
`49 49 2A 00` or `4D 4D 00 2A`, u32 first IFD offset (= 8). All MPF offsets are **relative to
T**. The MP Index IFD (first IFD) tags:

| Tag | Name | Type/count | Content |
|---|---|---|---|
| 0xB000 | MPFVersion | UNDEFINED[4] | `"0100"` |
| 0xB001 | NumberOfImages | LONG | N |
| 0xB002 | MPEntry | UNDEFINED[16*N] | N entries, layout below |
| 0xB003 | ImageUIDList | UNDEFINED[33*N] | optional |
| 0xB004 | TotalFrames | LONG | optional |

MPEntry (16 bytes, byte order of the TIFF header):

| Offset | Size | Field |
|---|---|---|
| 0 | u32 | Individual Image Attribute: bit 31 dependent parent, bit 30 dependent child, bit 29 representative image, bits 26-24 image data format (0 = JPEG), bits 23-0 MP type code: 0x030000 baseline MP primary, 0x010001 large thumbnail (VGA), 0x010002 large thumbnail (full HD), 0x010003/0x010004/0x010005 large thumbnail (4K/8K/16K, later CIPA revisions), 0x020001 multi-frame panorama, 0x020002 multi-frame disparity, 0x020003 multi-angle, 0x000000 undefined (bit masks cross-checked against ExifTool `MPF.pm`: flags mask 0xF8000000, format mask 0x07000000, type mask 0x00FFFFFF) |
| 4 | u32 | Individual Image Size (bytes, SOI..EOI inclusive) |
| 8 | u32 | Individual Image Data Offset, relative to T; **0 for the first image** |
| 12 | u16 | Dependent Image 1 Entry Number |
| 14 | u16 | Dependent Image 2 Entry Number |

Only the first image carries the MP Index IFD; subsequent images carry an MP Attribute IFD
only. The extent of the whole MPO/MPF file = `max(T + offset_i + size_i)`. Apple's HDR gain
map and depth images are typically 1/4 to 1/2 the size of the main image.

Output naming: a file with NumberOfImages > 1 whose later images are `0x020002/0x020003`
(3D) is an `.mpo`; otherwise keep `.jpg` (viewers ignore the trailing images).

#### 2.1.8 Trailers after EOI

- **Samsung "Motion Photo" / SEF trailer**: after the JPEG (and after MPF images) Samsung
  appends an MP4 and then a SEF directory. Layout (reverse-engineered, no public spec; this is
  exactly what ExifTool `Samsung.pm` `ProcessSamsung` parses, all integers **little-endian**):
  the file ends with `<u32le dir_len> "SEFT"` (last 4 bytes `53 45 46 54`; the 6 bytes before
  EOF therefore read `00 00 53 45 46 54` for any dir_len < 64 KiB, which is what ExifTool
  tests). The directory block of `dir_len` bytes immediately precedes those 8 bytes:
  `"SEFH" <u32 version (seen 101)> <u32 count>` then `count` 12-byte entries:
  `u16 (unused) @0, u16 block_type @2, u32 noff @4, u32 size @8`. `noff` is a **negative**
  offset: the block's data starts at `dirPos - noff` where `dirPos` = offset of `"SEFH"`;
  `size` is the block length (`size >= 8`, `size <= noff`). Each data block =
  `u32 @0 (type copy), u32 name_len @4, name[name_len] @8, payload`. Known block types /
  names: 0x0001 `EmbeddedImage` (`DualShot_1`, `SingleShot`), 0x0100 `EmbeddedAudioFile`
  (Sound & Shot), 0x0201 `SurroundShotVideo`, 0x0a01 `Image_UTC_Data` (timestamp), 0x0a30
  `MotionPhoto_Data` (the MP4), 0x0aa1 `MCC_Data`, 0x0a41 `BackupRestore_Data`. Older
  Galaxy "Sound & Shot" files end with `"QDIOBS"` instead of `SEFT` (same block chain). The
  XMP of such files carries `GCamera:MotionPhoto=1` (newer) or `GCamera:MicroVideo=1` +
  `MicroVideoOffset` (older). The carver should treat "XMP says MotionPhoto and the bytes
  after EOI start with `00 00 00 xx 66 74 79 70` (ftyp)" as authoritative and use the MP4
  walker (video spec) to find the end, then accept a SEFT-terminated block right after
  (extent = end of `"SEFT"`). Source: https://raw.githubusercontent.com/exiftool/exiftool/master/lib/Image/ExifTool/Samsung.pm
  (`ProcessSamsung`, `%Image::ExifTool::Samsung::Trailer`).
- **Google Motion Photo (Pixel)**: XMP `GCamera:MicroVideoOffset` (bytes from end of file to
  start of MP4) or `Container:Directory` with `Item:Length` entries (Motion Photo v2); the MP4
  follows EOI directly. Same handling: detect `ftyp` right after EOI.
- **Trailing garbage**: anything else after EOI that is not a recognised structure is **not**
  part of the file. Cluster-slack is the common case.

#### 2.1.9 Exif: TIFF inside APP1

Offset `T` = start of TIFF header = APP1 payload + 6. Header: `II` + `2A 00` + u32le IFD0
offset, or `MM` + `00 2A` + u32be. All offsets are relative to T.

IFD: `u16 count`, then `count` entries of 12 bytes, then `u32 next_ifd` (0 = none).

Entry: `u16 tag, u16 type, u32 count, u32 value_or_offset` (value inline, left-justified in
the 4 bytes, when `count * size(type) <= 4`).

Types: 1 BYTE(1), 2 ASCII(1), 3 SHORT(2), 4 LONG(4), 5 RATIONAL(8), 6 SBYTE(1), 7
UNDEFINED(1), 8 SSHORT(2), 9 SLONG(4), 10 SRATIONAL(8), 11 FLOAT(4), 12 DOUBLE(8), 13 IFD(4).
Unknown type -> skip entry (do not abort; MakerNotes contain junk types).

Tags needed for naming and orientation:

| IFD | Tag | Name | Type | Use |
|---|---|---|---|---|
| IFD0 | 0x010F | Make | ASCII | output subfolder / RAW format disambiguation |
| IFD0 | 0x0110 | Model | ASCII | same |
| IFD0 | 0x0112 | Orientation | SHORT | 1 normal, 2 mirror-H, 3 rotate 180, 4 mirror-V, 5 transpose, 6 rotate 90 CW, 7 transverse, 8 rotate 270 CW; keep as-is in output (we copy bytes), report it |
| IFD0 | 0x0132 | DateTime | ASCII[20] | fallback timestamp |
| IFD0 | 0x8769 | ExifIFD pointer | LONG/IFD | |
| IFD0 | 0x8825 | GPS IFD pointer | LONG/IFD | optional |
| ExifIFD | 0x9003 | DateTimeOriginal | ASCII[20] `"YYYY:MM:DD HH:MM:SS\0"` | **primary naming key** |
| ExifIFD | 0x9004 | DateTimeDigitized | ASCII[20] | fallback |
| ExifIFD | 0x9010/0x9011/0x9012 | OffsetTime / OffsetTimeOriginal / OffsetTimeDigitized | ASCII `"+05:30\0"` | optional tz |
| ExifIFD | 0x9291 | SubSecTimeOriginal | ASCII | tie-breaker for bursts |
| ExifIFD | 0xA420 | ImageUniqueID | ASCII[33] | dedupe |
| IFD1 | 0x0201 / 0x0202 | JPEGInterchangeFormat / Length | LONG | thumbnail; the reason blind EOI search fails |

Naming rule: `YYYYMMDD_HHMMSS[_ss][_Make-Model]_<seq>.jpg`; if DateTimeOriginal missing or
not matching `^\d{4}:\d\d:\d\d \d\d:\d\d:\d\d` (cameras write `"    :  :     :  :  "` when the
clock is unset), fall back to DateTime, then to `carved_<offset-hex>.jpg`.

Robustness: the Exif parser must bound every offset to the APP1 payload, cap IFD count at
e.g. 512 entries, cap the IFD chain at 8, and never follow a pointer twice (cycle guard with a
visited set).

#### 2.1.10 Huffman coding model (what the L2 validator implements)

**Table construction (T.81 Annex C):**

```
// input: BITS[1..=16], HUFFVAL[0..n)
k = 0
for i in 1..=16: for _ in 0..BITS[i]: HUFFSIZE[k] = i; k += 1
HUFFSIZE[k] = 0; LASTK = k                            // LASTK == n

code = 0; si = HUFFSIZE[0]; k = 0
loop:
    while HUFFSIZE[k] == si: HUFFCODE[k] = code; code += 1; k += 1
    if HUFFSIZE[k] == 0: break
    loop: code <<= 1; si += 1; if HUFFSIZE[k] == si: break

// decoding tables (F.2.2.3)
j = 0
for l in 1..=16:
    if BITS[l] == 0: MAXCODE[l] = -1
    else: VALPTR[l] = j; MINCODE[l] = HUFFCODE[j]; j += BITS[l] - 1; MAXCODE[l] = HUFFCODE[j]; j += 1
MAXCODE[17] = i32::MAX   // sentinel
```

Table sanity (reject the DHT if violated): `n <= 256`; Kraft sum
`sum(BITS[i] * 2^(16-i)) <= 2^16`; `code` must never exceed `2^si - 1` while generating (an
all-ones code of length 16 is forbidden so that the all-ones prefix can serve as marker
padding); DC tables must have every HUFFVAL <= 15 (<= 11 for 8-bit); AC tables HUFFVAL are
RS bytes.

**Bit reader (F.2.2.5):**

```
next_bit():
    if bits_left == 0:
        b = read_byte()
        if b == 0xFF:
            b2 = peek_byte()
            if b2 == 0x00: consume; b = 0xFF
            else if b2 == 0xFF: consume FF, retry (fill)  // rare inside scans; accept
            else: marker_hit = (pos-1); return 0           // marker reached: stream ended
        acc = b; bits_left = 8
    bits_left -= 1
    return (acc >> bits_left) & 1

receive(n): v = 0; repeat n times: v = (v << 1) | next_bit(); return v
extend(v, t): if t == 0 { 0 } else if v < (1 << (t-1)) { v - (1 << t) + 1 } else { v }
decode(tbl):
    code = next_bit(); l = 1
    while code > tbl.MAXCODE[l]: code = (code << 1) | next_bit(); l += 1; if l > 16: return Err(BadCode)
    return tbl.HUFFVAL[tbl.VALPTR[l] + code - tbl.MINCODE[l]]
```

The validator treats `marker_hit` as "scan data ended here" and the MCU counter tells whether
that is the expected end (all MCUs decoded, then marker must be EOI/next SOS) or premature.

**Baseline block decode (F.2.2.1, F.2.2.2):**

```
decode_block_baseline(comp):
    t = decode(dc_table[comp])                  // category 0..11
    if t > 11: Err(BadDcCategory)
    diff = extend(receive(t), t)
    pred[comp] += diff; coef[0] = pred[comp]
    // DC range check: |pred| <= 2048 for 8-bit (2^(P+3)); allow slack, flag > 4096 as suspicious
    k = 1
    while k <= 63:
        rs = decode(ac_table[comp]); r = rs >> 4; s = rs & 15
        if s == 0:
            if r == 15: k += 16; continue       // ZRL: 16 zeros
            break                                // EOB
        k += r
        if k > 63: Err(RunPastBlock)             // corruption signature #1
        coef[zz[k]] = extend(receive(s), s)
        k += 1
```

**MCU loop with restarts:**

```
decode_scan(scan):
    reset preds, eobrun = 0; expected_rst = 0; mcus_done = 0
    for mcu in 0..total_mcus:
        if Ri != 0 and mcu != 0 and mcu % Ri == 0:
            align_to_byte()                      // discard remaining bits (must be all 1s; flag otherwise)
            expect marker FF D(expected_rst); if a different RSTn: record gap; resync (3.5)
            expected_rst = (expected_rst + 1) & 7; reset preds; eobrun = 0
        for comp in scan.components: for blk in 0..(interleaved ? Hc*Vc : 1):
            decode_block(comp) ?                 // error => return Partial { mcus_done, byte_offset }
        mcus_done += 1
    align_to_byte(); expect EOI or next marker
```

**Progressive (T.81 G.1.2):**

```
dc_first:  t = decode(dc); diff = extend(receive(t), t); pred += diff; coef[0] = pred << Al
dc_refine: if next_bit(): coef[0] |= 1 << Al

ac_first(Ss, Se, Al):
    if eobrun > 0: eobrun -= 1; return
    k = Ss
    while k <= Se:
        rs = decode(ac); r = rs >> 4; s = rs & 15
        if s == 0:
            if r < 15: eobrun = (1 << r); if r > 0: eobrun += receive(r); eobrun -= 1; return   // EOBn
            k += 16; continue                                                                    // ZRL
        k += r; if k > Se: Err
        coef[zz[k]] = extend(receive(s), s) << Al
        k += 1

ac_refine(Ss, Se, Al):           // mirrors libjpeg decode_mcu_AC_refine
    p1 = 1 << Al; m1 = -1 << Al; k = Ss
    if eobrun == 0:
        while k <= Se:
            rs = decode(ac); r = rs >> 4; s = rs & 15
            if s == 0 and r < 15: eobrun = (1 << r); if r > 0: eobrun += receive(r); break
            val = 0
            if s != 0: if s != 1: Err(BadRefineSize); val = next_bit() ? p1 : m1
            // skip r zero-history coefs, refining nonzero ones on the way
            while k <= Se:
                c = &coef[zz[k]]
                if *c != 0:
                    if next_bit() and (*c & p1) == 0: *c += (*c >= 0) ? p1 : m1
                else:
                    if r == 0: break
                    r -= 1
                k += 1
            if val != 0 and k <= Se: coef[zz[k]] = val
            k += 1
    if eobrun > 0:
        while k <= Se: c = &coef[zz[k]]; if *c != 0 and next_bit() and (*c & p1) == 0: *c += (*c >= 0) ? p1 : m1; k += 1
        eobrun -= 1
```

The L2 validator for progressive files must keep the coefficient array (64 x i16 per block,
i.e. 128 bytes x blocks; for a 50 MP 4:2:0 image that is ~1.2 GB) **or** validate only the
symbol stream per scan without refinement history. Decision: keep coefficient *presence
bits* only (1 bit per coefficient, 8 bytes per block) for the refinement walk, since
`ac_refine` only needs to know whether a coefficient is non-zero. That reduces memory to
~75 MB for 50 MP and is exact.

Zigzag table `zz[0..64]` (natural index of the k-th zigzag coefficient):

```
0, 1, 8,16, 9, 2, 3,10,17,24,32,25,18,11, 4, 5,12,19,26,33,40,48,41,34,27,20,13, 6, 7,14,21,28,
35,42,49,56,57,50,43,36,29,22,15,23,30,37,44,51,58,59,52,45,38,31,39,46,53,60,61,54,47,55,62,63
```

#### 2.1.11 T.81 Annex K.3 standard Huffman tables

These are the tables in T.81 Tables K.3-K.6 as shipped in libjpeg `jstdhuff.c` (`bits[0]` is
unused and 0; `bits[1..=16]` are BITS; values in HUFFVAL order).

```
DC luminance   (Tc=0, Th=0)
  BITS: 00 01 05 01 01 01 01 01 01 00 00 00 00 00 00 00        (sum 12)
  VALS: 00 01 02 03 04 05 06 07 08 09 0A 0B
DC chrominance (Tc=0, Th=1)
  BITS: 00 03 01 01 01 01 01 01 01 01 01 00 00 00 00 00        (sum 12)
  VALS: 00 01 02 03 04 05 06 07 08 09 0A 0B
AC luminance   (Tc=1, Th=0)
  BITS: 00 02 01 03 03 02 04 03 05 05 04 04 00 00 01 7D        (sum 162)
  VALS:
   01 02 03 00 04 11 05 12 21 31 41 06 13 51 61 07
   22 71 14 32 81 91 A1 08 23 42 B1 C1 15 52 D1 F0
   24 33 62 72 82 09 0A 16 17 18 19 1A 25 26 27 28
   29 2A 34 35 36 37 38 39 3A 43 44 45 46 47 48 49
   4A 53 54 55 56 57 58 59 5A 63 64 65 66 67 68 69
   6A 73 74 75 76 77 78 79 7A 83 84 85 86 87 88 89
   8A 92 93 94 95 96 97 98 99 9A A2 A3 A4 A5 A6 A7
   A8 A9 AA B2 B3 B4 B5 B6 B7 B8 B9 BA C2 C3 C4 C5
   C6 C7 C8 C9 CA D2 D3 D4 D5 D6 D7 D8 D9 DA E1 E2
   E3 E4 E5 E6 E7 E8 E9 EA F1 F2 F3 F4 F5 F6 F7 F8
   F9 FA
AC chrominance (Tc=1, Th=1)
  BITS: 00 02 01 02 04 04 03 04 07 05 04 04 00 01 02 77        (sum 162)
  VALS:
   00 01 02 03 11 04 05 21 31 06 12 41 51 07 61 71
   13 22 32 81 08 14 42 91 A1 B1 C1 09 23 33 52 F0
   15 62 72 D1 0A 16 24 34 E1 25 F1 17 18 19 1A 26
   27 28 29 2A 35 36 37 38 39 3A 43 44 45 46 47 48
   49 4A 53 54 55 56 57 58 59 5A 63 64 65 66 67 68
   69 6A 73 74 75 76 77 78 79 7A 82 83 84 85 86 87
   88 89 8A 92 93 94 95 96 97 98 99 9A A2 A3 A4 A5
   A6 A7 A8 A9 AA B2 B3 B4 B5 B6 B7 B8 B9 BA C2 C3
   C4 C5 C6 C7 C8 C9 CA D2 D3 D4 D5 D6 D7 D8 D9 DA
   E2 E3 E4 E5 E6 E7 E8 E9 EA F2 F3 F4 F5 F6 F7 F8
   F9 FA
```

As a ready-to-insert DHT segment (the one every MJPEG decoder injects; 420 bytes total):

```
FF C4 01 A2
  00  <DC lum BITS 16 bytes> <12 vals>
  10  <AC lum BITS 16 bytes> <162 vals>
  01  <DC chr BITS 16 bytes> <12 vals>
  11  <AC chr BITS 16 bytes> <162 vals>
```

Lh = 2 + (17+12) + (17+162) + (17+12) + (17+162) = 418 = 0x01A2.

#### 2.1.12 Carving parameters for JPEG

| Item | Value |
|---|---|
| Magic | `FF D8 FF` at offset 0, followed by `E0..EF`, `DB`, `C0..C3`, `C4`, `FE`, `DD`, `C9..CB` (arithmetic; still carve). PhotoRec registers only the 3-byte `FF D8 FF` (`file_jpg.c` `jpg_header`) and then validates the segment chain; it does not special-case `FF D8 FF FF`. Treat a 4th byte of `FF` as fill (2.1.1) and continue the walk. |
| Minimum size | 134 bytes (SOI + minimal DQT 69 + SOF 19 + DHT 2x(19+...) + SOS 14 + 1 MCU + EOI, in practice); reject < 256 bytes as not worth emitting |
| Maximum plausible | 256 MiB default; raise with `--max-jpeg-size` |
| Exact end | 2.1.6 |
| False positives | `FF D8 FF` occurs at random every 16 MB; the header walk (SOF present, tables defined, lengths chain to SOS) rejects essentially all of them. Also reject SOF X*Y > 2^31 and files whose scan ends after < 1 MCU |
| Validation levels | L1 header walk; L2 Huffman decode of all scans with MCU count; L3 zune-jpeg decode |
| Alignment | search at every sector (512) **and** every byte position inside data that is itself a recognised container (TIFF/RAW, HEIF mdat, PSD, MP4) to extract embedded previews; sector-only for the raw image |

### 2.2 PNG (W3C PNG 3rd edition / ISO 15948)

| Item | Value |
|---|---|
| Signature | `89 50 4E 47 0D 0A 1A 0A` at offset 0 |
| Chunk | `u32be length` (<= 2^31 - 1), `4 x ASCII type` (each byte in A-Z or a-z), `data[length]`, `u32be CRC` over type + data |
| IHDR | 13 bytes: `u32be width`, `u32be height` (each 1..2^31-1), `u8 bit_depth`, `u8 colour_type`, `u8 compression (0)`, `u8 filter (0)`, `u8 interlace (0 or 1)` |
| Colour type / depth | 0 grey: 1,2,4,8,16; 2 RGB: 8,16; 3 indexed: 1,2,4,8 (needs PLTE); 4 grey+alpha: 8,16; 6 RGBA: 8,16 |
| Channels | 0 -> 1, 2 -> 3, 3 -> 1, 4 -> 2, 6 -> 4; bpp_bits = channels * depth; bytes-per-pixel for filtering `bpp = max(1, bpp_bits / 8)` |
| Scanline stride | `1 + ceil(width * bpp_bits / 8)` (the leading byte is the filter type) |
| Order | IHDR first; PLTE before IDAT; IDAT chunks consecutive; IEND last (`00 00 00 00 49 45 4E 44 AE 42 60 82`) |
| CRC-32 | polynomial 0x04C11DB7 (reflected 0xEDB88320), init 0xFFFFFFFF, final XOR 0xFFFFFFFF, over type+data. Same as zlib `crc32`. |
| zlib stream | concatenation of all IDAT data: `CMF (0x78 typical: CM=8, CINFO<=7)`, `FLG` (CMF*256+FLG % 31 == 0), deflate blocks, `u32be Adler-32` of the raw filtered bytes |
| Minimum size | 8 + 25 (IHDR) + 12 + ~10 (IDAT) + 12 (IEND) = 67 bytes |
| Maximum plausible | 512 MiB default (screenshots at 8K are < 100 MB) |
| Exact end | walk chunks; end = after IEND CRC. Stop with `Partial` on bad type chars, length > 2^31-1 or chunk overrunning the buffer |
| False positives | 8-byte signature + IHDR sanity (length 13, valid colour/depth combo, compression/filter = 0, interlace <= 1) makes them negligible |
| Validation | L1 chunk walk + IHDR; L2 CRC of every chunk + inflate of IDAT + Adler check + total inflated size == expected; L3 png crate decode |

Expected inflated size: non-interlaced `height * stride`; interlaced = sum over the 7 passes
of `pass_rows * (1 + ceil(pass_cols * bpp_bits / 8))` where a pass with 0 columns or 0 rows
contributes nothing.

Filtering (per scanline, `x` = current byte, `a` = byte `bpp` to the left, `b` = same byte
previous scanline, `c` = `bpp` left on previous scanline; bytes outside the image are 0):

| Type | Recon(x) |
|---|---|
| 0 None | Filt(x) |
| 1 Sub | Filt(x) + Recon(a) |
| 2 Up | Filt(x) + Recon(b) |
| 3 Average | Filt(x) + floor((Recon(a) + Recon(b)) / 2) |
| 4 Paeth | Filt(x) + Paeth(Recon(a), Recon(b), Recon(c)) |

```
paeth(a,b,c): p = a + b - c; pa = |p-a|; pb = |p-b|; pc = |p-c|
              if pa <= pb && pa <= pc: a else if pb <= pc: b else c
```

All arithmetic mod 256. A filter type byte > 4 is a corruption signature (and the exact row
where damage starts).

Adam7 passes (start col, start row, col step, row step):
1: (0,0,8,8), 2: (4,0,8,8), 3: (0,4,4,8), 4: (2,0,4,4), 5: (0,2,2,4), 6: (1,0,2,2), 7: (0,1,1,2).
Pass dimensions: `cols = ceil((width - start_col) / col_step)` (0 if width <= start_col), same
for rows. Each pass is filtered independently (filter context resets).

APNG: `acTL` (u32be num_frames, u32be num_plays) before IDAT, `fcTL` (u32be seq, width,
height, x_off, y_off, u16be delay_num, delay_den, u8 dispose_op, u8 blend_op), `fdAT` (u32be
seq + IDAT-format data). Carver: no special handling, chunks walk normally; the default image
(IDAT) is always a valid still image, so the repair output for a damaged APNG is the still.

Apple CgBI: iOS-optimised PNGs have a `CgBI` chunk before IHDR, IDAT is a raw deflate stream
(no zlib header, no Adler), pixels BGRA premultiplied. Carve normally (chunk walk works),
mark `variant = cgbi`, do not attempt L2 inflate with the zlib wrapper.

### 2.3 GIF (GIF89a spec)

| Item | Value |
|---|---|
| Magic | `47 49 46 38 37 61` ("GIF87a") or `47 49 46 38 39 61` ("GIF89a") at 0 |
| Logical Screen Descriptor (7 bytes at 6) | `u16le width`, `u16le height`, `u8 packed` (bit 7 GCT flag, bits 6-4 colour resolution-1, bit 3 sort, bits 2-0 GCT size N -> 3 * 2^(N+1) bytes), `u8 bg index`, `u8 aspect` |
| GCT | follows LSD if flag set |
| Blocks | repeated: `0x2C` Image Descriptor (u16le left, top, width, height, u8 packed: bit 7 LCT flag, bit 6 interlace, bit 5 sort, bits 2-0 LCT size) + LCT + `u8 LZW min code size (2..8)` + data sub-blocks; `0x21` Extension: `u8 label` (0xF9 GCE, 0xFE comment, 0x01 plain text, 0xFF application: 11-byte id e.g. `NETSCAPE2.0`) + sub-blocks; `0x3B` trailer |
| Data sub-blocks | `u8 len` (1..255), `len` bytes, ..., `00` terminator |
| Exact end | walk blocks until 0x3B; end = offset of 0x3B + 1 |
| Min / max | 6 + 7 + 1 = 14 bytes (`GIF89a` + LSD + 0x3B) is legal but useless; emit only >= 64 bytes; max 256 MiB (animations) |
| False positives | "GIF8" + "7a"/"9a" + width/height nonzero + first block byte in {0x21, 0x2C, 0x3B}: negligible |
| Validation | L1 block walk; L2 LZW decode of each image (code size progression, clear/EOI codes, pixel count == w*h); L3 gif crate |

Truncated GIF repair: if the walk ends inside an image's sub-blocks, terminate the sub-block
chain (`00`) and append `3B`; viewers show frames decoded so far.

### 2.4 BMP (Windows BITMAPFILEHEADER + DIB header)

| Offset | Size | Field |
|---|---|---|
| 0 | `42 4D` | "BM" |
| 2 | u32le | bfSize = file size (often correct; **may be 0 or wrong**, some writers put the pixel-data size here) |
| 6 | u32le | reserved (0) |
| 10 | u32le | bfOffBits = offset of pixel array (>= 14 + DIB header size) |
| 14 | u32le | DIB header size: 12 (BITMAPCOREHEADER), 40 (INFO), 52 (V2), 56 (V3), 108 (V4), 124 (V5); other values -> reject |
| 18 | i32le | width (BITMAPCOREHEADER, DIB size 12: `u16le` width at 18, `u16le` height at 20, `u16le` planes at 22, `u16le` bpp at 24, no compression field) |
| 22 | i32le | height (negative = top-down; INFO and later headers only) |
| 26 | u16le | planes = 1 |
| 28 | u16le | bpp: 1,4,8,16,24,32 (0 = JPEG/PNG embedded) |
| 30 | u32le | compression: 0 BI_RGB, 1 RLE8, 2 RLE4, 3 BITFIELDS, 4 JPEG, 5 PNG, 6 ALPHABITFIELDS, 11-13 CMYK |
| 34 | u32le | biSizeImage (0 allowed for BI_RGB) |
| 46 | u32le | biClrUsed |

Exact end: `stride = ((width * bpp + 31) / 32) * 4`; for BI_RGB/BITFIELDS `end =
bfOffBits + stride * |height|`; for RLE/JPEG/PNG `end = bfOffBits + biSizeImage` (must be
> 0); accept `bfSize` only if `bfSize >= computed_end` and `bfSize - computed_end < 64 KiB`
(ICC profile / trailing data), else use computed_end. Min size 14 + 12 + 4 = 30; cap 1 GiB.
False positives: "BM" is 2 bytes; require DIB size in the allowed set, planes == 1, bpp in
the allowed set, |width| and |height| <= 65535, bfOffBits <= 64 KiB.

### 2.5 WebP (RIFF container)

| Offset | Size | Field |
|---|---|---|
| 0 | `52 49 46 46` | "RIFF" |
| 4 | u32le | size of everything after this field = file_size - 8 (always even) |
| 8 | `57 45 42 50` | "WEBP" |
| 12 | chunk | first chunk: `VP8 ` (lossy), `VP8L` (lossless) or `VP8X` (extended) |

Chunk: `FourCC`, `u32le size`, payload, 1 pad byte if size odd (pad counted in RIFF size).

- `VP8 ` payload: 3-byte frame tag (bit 0 = 0 key frame, bits 1-3 version, bit 4 show, bits
  5-23 first partition size), then key-frame start code `9D 01 2A`, `u16le` width (low 14
  bits; top 2 bits scale), `u16le` height (same). Validate start code and dimensions > 0.
- `VP8L` payload: `2F` signature byte, then 14 bits width-1, 14 bits height-1, 1 bit alpha,
  3 bits version (must be 0), LSB-first packed in the next 4 bytes.
- `VP8X` payload (10 bytes): `u8 flags` (bit 5 ICC, bit 4 alpha, bit 3 Exif, bit 2 XMP, bit 1
  animation; bits 7-6 and 0 reserved = 0), 3 reserved bytes, `u24le canvas width-1`, `u24le
  canvas height-1`. Followed by `ICCP`, `ANIM`/`ANMF`, `ALPH`, `VP8 `/`VP8L`, `EXIF`, `XMP `.

Exact end: `8 + riff_size`; cross-check by walking chunks: the sum must land exactly on that
end (tolerate a RIFF size larger than the chunk walk by <= 1 pad byte; otherwise prefer the
chunk-walk end and mark `partial`). Min 12 + 8 + 10 = 30; cap 256 MiB (animated). False
positives: RIFF+WEBP+valid first FourCC: negligible. Validation: L2 chunk walk + VP8/VP8L
header check; L3 image-webp decode.

### 2.6 TIFF generic walker (TIFF 6.0 + Exif + DNG)

Header (8 bytes): `49 49 2A 00` / `4D 4D 00 2A` + u32 IFD0 offset. BigTIFF: `49 49 2B 00 08 00 00 00` + u64 offset (entries 20 bytes: u16 tag, u16 type, u64 count, u64 value) - support read-only, rare in cameras.

RAW variants that change the magic word (same IFD structure, walker must accept them):

| Bytes 0-3 | Format | Notes |
|---|---|---|
| `49 49 2A 00` / `4D 4D 00 2A` | TIFF, CR2, NEF, ARW, DNG, PEF, ERF, SRW, 3FR, DCR, KDC, MEF, MOS, NRW, SR2, SRF, IIQ | distinguish by Make and private tags |
| `49 49 52 4F` ("IIRO") | ORF little-endian (magic word 0x4F52) | Olympus |
| `4D 4D 4F 52` ("MMOR") | ORF big-endian | Olympus E-series early |
| `49 49 52 53` ("IIRS") | ORF variant (0x5352) | Olympus, some models |
| `49 49 55 00` ("IIU\0") | RW2 / RWL (Panasonic/Leica), magic word 0x55 | IFD0 offset is 0x18 in RW2 (`49 49 55 00 18 00 00 00`, PRONOM fmt/662); older Panasonic `.RAW` (pre-2008, DMC-FZ8/FZ18/L10 era) use IFD0 offset 8: `49 49 55 00 08 00 00 00` (libopenraw RW2 notes give this 8-byte magic). Accept any IFD0 offset in 8..=64 KiB. |
| `49 49 2A 00` + `43 52 02 00` at 8 | CR2 | "CR", major 2, minor 0; u32le at 12 = offset of raw IFD (IFD3) (lclevy CR2 notes). Only major version 2 has ever been observed; reject other values. |

Walker:

```
tiff_extent(buf, base, byteorder):
    visited = set(); ends = [8]
    queue = [(ifd0_offset, depth 0)]
    while let Some((off, depth)) = queue.pop():
        if off in visited or depth > 6 or off + 2 > cap: continue; visited.insert(off)
        n = u16(off); if n == 0 or n > 4096: continue      # 4096: generous; real IFDs < 200
        ends.push(off + 2 + 12*n + 4)
        for e in 0..n:
            tag, typ, cnt, val = entry(off + 2 + 12*e)
            sz = type_size(typ) * cnt                      # unknown type -> skip
            if sz > 4: ends.push(val + sz)                 # value data lives out-of-line
            match tag:
              0x0111 StripOffsets, 0x0144 TileOffsets: pair with 0x0117 / 0x0145 (same count), ends.push(offset_i + count_i) for each i
              0x0201 JPEGInterchangeFormat: pair with 0x0202, ends.push(off + len)
              0x014A SubIFDs (LONG or IFD, cnt up to 32): queue each
              0x8769 ExifIFD, 0x8825 GPS, 0xA005 Interop: queue
              0x927C MakerNote: ends.push(val + cnt); if Make is Olympus/Pentax/Nikon: parse makernote preview (2.7)
              0xC612 DNGVersion: is_dng = true
              0xC68C OriginalRawFileData (DNG): ends.push(val + cnt)  (can be huge)
              0xC634 DNGPrivateData: if Make == SONY: sr2_private(val)  (2.7 ARW)
              0x0118 (RW2 RawDataOffset, LONG, no length): rw2_raw_offset = val
              0x002E (RW2 JpgFromRaw, UNDEFINED): ends.push(val + cnt)
        next = u32(off + 2 + 12*n); if next != 0: queue.push((next, depth))   # IFD chain (IFD1..)
    return max(ends)
```

Validation: every offset must be < cap (`max_tiff_size`, default 512 MiB; DNG with
OriginalRawFileData up to 1 GiB); StripOffsets count must equal StripByteCounts count; for
IFDs with `0x0100 ImageWidth`/`0x0101 ImageLength` and `Compression 0x0103 == 1` the strip
total must be >= `width * height * bits/8` (catches wrong-endian/garbage IFDs).

Extent = max over all ends. Plain TIFF files: this is exact (the last strip ends the file or
an IFD does). Camera RAWs: the raw strip is normally the last thing, so exact too, **except**
RW2 (2.7) and files where the MakerNote stores a preview beyond all strips.

Min size 8 + 2 + 12 + 4 = 26; tiny TIFFs are junk below ~1 KB. False positives: 4-byte magic
+ IFD0 offset < 64 KiB + entry count 1..512 + tags sorted ascending (TIFF requires ascending
order; most writers comply, treat descending as a strong negative signal, not fatal).

### 2.7 TIFF-based camera RAW specifics

| Format | Detect | Where things are | Extent notes |
|---|---|---|---|
| CR2 (Canon) | `II*\0` + `CR` 02 00 at 8; Make "Canon" | IFD0: full-size JPEG preview via 0x0111/0x0117 (Compression 6); IFD1: 160x120 thumb via 0x0201/0x0202; IFD2: small uncompressed RGB; IFD3 (u32le at offset 12): raw, lossless JPEG (SOF3) via 0x0111/0x0117, 0xC640 RawImageSegmentation ("cr2_slice", 3 x SHORT: slice count-1, slice width, last slice width), 0xC5E0 CR2CFAPattern, 0xC6C5 SRawType, 0xC5D8/0xC5D9 unknown (ExifTool `Exif.pm`; raw dimensions come from the SOF3 inside the strip, not from these tags) | max of strips; raw strip is last |
| NEF / NRW (Nikon) | Make "NIKON CORPORATION" / "NIKON"; mostly `MM` | IFD0: thumbnail (160x120 RGB strips) + `0x014A SubIFDs` -> [preview JPEG (Compression 6, 0x0201/0x0202 or strips), raw (Compression 34713)]; MakerNote `"Nikon\0"` + `02 1x 00 00` (version) + TIFF header (`II`/`MM` + 2A + IFD offset 8) at makernote+10, IFD at makernote+18; all makernote offsets relative to that TIFF header (ExifTool `MakerNotes.pm`: Start = valuePtr+18, Base = start-8); preview in MakerNote tag 0x0011 PreviewIFD (ExifTool `Nikon.pm`, SubIFD with standard 0x0201/0x0202, relative to the makernote TIFF header) | max of SubIFD strips |
| ARW / SR2 / SRF (Sony) | Make "SONY". PhotoRec `file_tiff_le.c` really does `memcmp(make, "SONY", 5) == 0` (NUL-terminated, 4 chars) -> `.sr2`, `memcmp(make, "SONY ", 5) == 0` (trailing space) -> `.arw`, with a `TODO` admitting the correct rule is "sr2 if Sony::FileFormat begins with 1". Authoritative rule (ExifTool `Sony.pm` tag 0xB000 FileFormat, BYTE[4] in the MakerNote): `01 00 00 00` = SR2, `02 00 00 00` = ARW 1.0, `03 0x 00 00` = ARW 2.x, `04..06 ...` = ARW 3-5; prefer it, fall back to the Make-string heuristic | IFD0: preview JPEG (0x0201/0x0202) + SubIFDs -> raw (Compression 32767 = Sony ARW compressed, strips); 0xC634 DNGPrivateData -> SR2Private IFD (tags 0x7200 SR2SubIFDOffset, 0x7201 SR2SubIFDLength, 0x7221 SR2SubIFDKey (LONG, decryption key); ExifTool `Sony.pm` `SR2Private`) | max of strips incl. SR2SubIFDOffset+Length |
| DNG (Adobe) | `0xC612 DNGVersion` BYTE[4] in IFD0 (e.g. 01 04 00 00) | IFD0 may be the preview (NewSubFileType 1) with SubIFDs -> raw (NewSubFileType 0), raw often **tiled** (0x0144/0x0145, Compression 7 lossless JPEG or 34892 lossy JPEG, 52546 JPEG XL in DNG 1.7); 0xC68C OriginalRawFileData embeds the original camera file; 0xC71C PreviewApplicationName etc. | max over tiles/strips/private |
| PEF (Pentax/Ricoh) | Make "PENTAX Corporation" / "PENTAX" / "RICOH IMAGING COMPANY, LTD." | IFD0: raw (Compression 65535 Pentax), strips; IFD1: thumbnail JPEG 0x0201/0x0202; preview JPEG in MakerNote (`AOC\0` then `MM`/`II` on newer bodies, IFD directly after on older ones; ExifTool parses it with `FixBase` because the offset base varies; tags **0x0002 PreviewImageSize** (2 x SHORT), **0x0003 PreviewImageLength**, **0x0004 PreviewImageStart** (`IsOffset => 2` = relative to the start of the file, not the TIFF header or makernote); 0x0005 is PentaxModelID, not a preview tag; ExifTool `Pentax.pm`) | max incl. makernote preview |
| ORF (Olympus / OM System) | magic IIRO/MMOR/IIRS | IFD0: raw strips (Compression 1 but packed 12-bit, length may be less than w*h*2); IFD1: thumbnail; preview JPEG in MakerNote (`OLYMPUS\0` + `II`/`MM` + `03 00`, IFD at makernote+12, offsets relative to makernote start (ExifTool `MakerNotes.pm`: Start = valuePtr+12, Base = start-12); `OM SYSTEM\0` header: IFD at +16, same base rule; old `OLYMP\0` header: IFD at +8, offsets relative to the TIFF header; CameraSettings IFD 0x2020 -> 0x0100 PreviewImageValid, 0x0101 PreviewImageStart, 0x0102 PreviewImageLength, ExifTool `Olympus.pm`) | max incl. makernote preview |
| RW2 / RWL (Panasonic / Leica) | `IIU\0` | IFD0 at 0x18: 0x0002 SensorWidth, 0x0003 SensorHeight (SHORT), 0x002D RawFormat, 0x002E JpgFromRaw (UNDEFINED, whole JPEG incl. Exif, in the value area), 0x0118 RawDataOffset (LONG, **no byte count**), 0x0127 JpgFromRaw2 (newer bodies) | raw data runs from RawDataOffset to EOF. RawFormat values (ExifTool `PanasonicRaw.pm`): 2 = old `.RAW` (FZ8/FZ18), 3 = old `.RAW` (L10), 4 = RW2 for most bodies (incl. G9 pixel-shift off), 5 = GH5s / G9 pixel-shift on, 6 = DC-S1 / S1R pixel-shift off, 7 = S1R pixel-shift on (5 and 7 are lossless). Size estimate for RawFormat 4 (dcraw `panasonic_load_raw`: 14 pixels per 128 bits, i.e. 16 bytes per 14 pixels, `pana_bits` refilling 0x4000-byte blocks with `load_flags = 0x2008`; dcraw's own detection test is `(file_len - data_offset) / (raw_width*8/7) == raw_height`): `size ~= raw_width * raw_height * 8 / 7` bytes where raw_width/raw_height are the **sensor** dims (0x0002/0x0003), allowing up to one extra 0x4000 block of padding. For RawFormat 5/6/7 the stream is a different (v5/v6/v7) compression whose length is not derivable from the header -> fall back to "up to next carved header or next FS cluster boundary + validation". Report `extent_confidence = low` for everything but RawFormat 4 |
| ERF (Epson) | Make "SEIKO EPSON CORP." | plain TIFF, raw in IFD0 strips | exact |
| SRW (Samsung NX) | Make "SAMSUNG" | IFD0 + SubIFDs, raw strips (Compression 32769 Packed RAW, 32770 Samsung SRW Compressed, 32772 Samsung SRW Compressed 2 (NX3000/NXmini); **32773 is PackBits**, not Samsung; ExifTool `Exif.pm`) | max of strips |
| 3FR/FFF (Hasselblad), MEF (Mamiya), MOS (Leaf), IIQ (Phase One), DCR/KDC (Kodak), SR2 | by Make | generic walker | max |

Naming: use the Make/Model from IFD0 plus DateTimeOriginal from the ExifIFD (same tags as
2.1.9) and the extension from the table; unknown Make -> `.tif`.

### 2.8 RAF (Fujifilm)

All multi-byte fields big-endian.

| Offset | Size | Field |
|---|---|---|
| 0 | 16 | `"FUJIFILMCCD-RAW "` (46 55 4A 49 46 49 4C 4D 43 43 44 2D 52 41 57 20; note trailing space) |
| 16 | 4 | format version ASCII, `"0201"` |
| 20 | 8 | camera number ID (ASCII digits) |
| 28 | 32 | camera model string, NUL padded (e.g. `"X-T3"`) |
| 60 | 4 | directory version ASCII (`"0100"`, `"0130"`, `"0159"`...; ExifTool labels this field `FirmwareVersion`) |
| 64 | u32be | 1 for M-RAW (multi-shot) files, 0 otherwise (ExifTool `FujiFilm.pm` comments) |
| 68 | u32be | high word of M-RAW offset (only seen zero) |
| 72 | u32be | M-RAW header offset (0 when not M-RAW) |
| 76 | u32be | M-RAW header length |
| 80 | u32be | unknown (only seen zero) |
| 84 | u32be | JPEG offset (embedded full Exif JPEG with preview) |
| 88 | u32be | JPEG length |
| 92 | u32be | CFA header offset ("metadata container") |
| 96 | u32be | CFA header length |
| 100 | u32be | CFA offset (raw data; a TIFF container `49 49 2A 00` on X-series, bare on old bodies) |
| 104 | u32be | CFA length |
| 108 | u32be | RAFCompression: 0 = uncompressed, 2 = lossless, 3 = lossy (ExifTool `FujiFilm.pm` `RAFHeader` 0x6c, valid only when the first 3 bytes are zero; on some old RAF versions the embedded JPEG itself starts at 108, i.e. `jpeg_off == 108`, so always test `jpeg_off` first) |
| 112 | 8 | unknown (libopenraw: "12 unknown bytes" from 108) |
| 120 | u32be | unknown offset (libopenraw: "unknown offset, purpose unclear"); not needed for extent. UNVERIFIED whether it is a second CFA record |

CFA header record format at CFA header offset: `u32be count`, then `count` records of
`u16be tag, u16be size, data[size]`. Tags: 0x0100 RawImageFullSize (u16 h, u16 w), 0x0110
RawImageCropTopLeft, 0x0111 RawImageCroppedSize, 0x0115 RawImageAspectRatio, 0x0121
RawImageSize, 0x0130 FujiLayout, 0x0131 XTransLayout (36 bytes), 0x2FF0 WB_GRGBLevels, 0xC000
RAFData (ExifTool tag names).

Extent = `max(jpeg_off + jpeg_len, cfah_off + cfah_len, cfa_off + cfa_len)` with every offset
>= 84 and every end <= cap (512 MiB; GFX100 RAFs are ~200 MB). PhotoRec uses exactly this
rule. Validation: the JPEG at `jpeg_off` must start with `FF D8` and pass the JPEG L1 walk;
the CFA at `cfa_off` starts with `49 49 2A 00` on X-Trans bodies.

### 2.9 ISO BMFF stills: HEIF/HEIC, AVIF, CR3 (ISO/IEC 14496-12, 23008-12)

Box: `u32be size`, `4 x type`; `size == 1` -> `u64be largesize` at +8 (header 16 bytes);
`size == 0` -> box extends to end of file (only legal for the last top-level box; the carver
cannot know EOF, so treat size 0 as "extent unknown" and fall back to `meta`/`iloc` evidence).
`uuid` type -> 16-byte UUID follows the header. FullBox = Box + `u8 version` + `u24 flags`.

`ftyp` at offset 0: size (>= 16, <= 256, multiple of 4), `66 74 79 70`, `major_brand[4]`,
`u32be minor_version`, `compatible_brands[4]*`.

| Major brand (bytes 8-11) | Format | Ext |
|---|---|---|
| `heic`, `heix`, `heim`, `heis` | HEIF single image, HEVC | .heic |
| `hevc`, `hevx`, `hevm`, `hevs` | HEIF image sequence | .heics / .heic |
| `mif1`, `msf1` | generic HEIF (check compatible brands / item types for `hvc1`, `av01`, `jpeg`, `jxl `) | .heif / .avif |
| `avif`, `avis` | AVIF | .avif |
| `crx ` (63 72 78 20), minor 1, compatible `crx `, `isom` | Canon CR3 | .cr3 |
| `jxl ` | JPEG XL in ISOBMFF (file starts `00 00 00 0C 4A 58 4C 20 0D 0A 87 0A`) | .jxl |
| `qt  `, `isom`, `mp42`, `3gp*`, `M4V ` ... | video, handled by the video spec | |

Apple HEIC first 24 bytes (typical): `00 00 00 18 66 74 79 70 68 65 69 63 00 00 00 00 6D 69 66 31 68 65 69 63` (ftyp size 24, major `heic`, minor 0, compat `mif1`, `heic`).
CR3 first 24 bytes: `00 00 00 18 66 74 79 70 63 72 78 20 00 00 00 01 63 72 78 20 69 73 6F 6D`.

Top-level box walk: allowed types `ftyp, meta, mdat, moov, free, skip, uuid, idat, jumb,
mdat, moof, mfra, sidx, pdin, pitm` (the last few for video compatibility); stop at an unknown
type (all 4 bytes must be printable ASCII 0x20-0x7E) or when `size < 8` (and != 0/1).
Extent = end of the last valid top-level box. For an unknown-size last box, use the
`iloc`/`mdat` evidence below.

`meta` (FullBox) children: `hdlr` (handler_type `pict`), `pitm` (primary item id), `iinf`
(`infe` entries: v2: u16 item_ID, u16 protection, item_type `hvc1`/`av01`/`grid`/`iden`/
`iovl`/`Exif`/`mime`/`jpeg`), `iref` (`thmb`, `dimg`, `cdsc`, `auxl`), `iprp` -> `ipco`
(`ispe` = u32 width, u32 height; `hvcC`; `irot`; `colr`; `pixi`), `ipma`, `idat`, `iloc`.

`iloc` (FullBox, version 0/1/2):

```
u8  offset_size:4, length_size:4            // each 0, 4 or 8
u8  base_offset_size:4, index_size:4        // index_size only v1/v2 (reserved in v0)
item_count: u16 (v0, v1) | u32 (v2)
for each item:
    item_ID: u16 (v0, v1) | u32 (v2)
    v1/v2: u16 (12 bits reserved, 4 bits construction_method: 0 = file offset, 1 = offset into idat, 2 = item offset)
    u16 data_reference_index (0 = this file)
    base_offset: uint(base_offset_size*8)
    u16 extent_count
    for each extent:
        v1/v2 and index_size > 0: extent_index: uint(index_size*8)
        extent_offset: uint(offset_size*8)
        extent_length: uint(length_size*8)
```

Evidence-based extent for HEIF: `needed_end = max over items with construction_method 0 of
(base_offset + extent_offset + extent_length)`. All of these must lie inside `mdat`
(`mdat_start+8 <= off`, `off+len <= mdat_end`); if the box walk ends before `needed_end` the
file is truncated (`partial`), and the carver can still emit the primary item if its extents
are inside the recovered bytes. Exif item (`infe` type `Exif`, ISO 23008-12 Annex A): payload =
`u32be exif_tiff_header_offset` followed by the Exif block; the TIFF header is at
`payload + 4 + exif_tiff_header_offset`. Apple writes the 6-byte `Exif\0\0` identifier first
and therefore an offset field of 6; other writers use 0. Always honour the offset field.

CR3 specifics (lclevy/canon_cr3 reverse engineering):

| Box | Content |
|---|---|
| `ftyp` | as above |
| `moov` | contains `uuid 85c0b687-820f-11e0-8111-f4ce462b6a48` (Canon metadata): `CNCV` (30-byte version string, e.g. "CanonCR3_001/00.09.00/00.00.00" EOS R/RP/M50, "CanonCR3_001/01.09.00/01.00.00" raw-burst rolls, "CanonCR3_002/..." and "CanonCR3_003/..." for HDR-preview variants, "CanonCRM0001/..." for CRM movies, "CanonHEIF001/..." for Canon HEIF - match the prefix `Canon` only), `CCTP`, `CTBO` (offset table: u32be count (4 or 5), records of 20 bytes {u32 index (1-based), u64 offset, u64 size}; index 1 = XMP uuid box, 2 = PRVW uuid box, 3 = mdat, 4 = zero offset/size, 5 (newer bodies) = `uuid 5766b829-bb6a-47c5-bcfb-8b9f2260d06d` containing `CMTA`), `CMT1` (IFD0 TIFF), `CMT2` (Exif IFD TIFF), `CMT3` (MakerNote TIFF), `CMT4` (GPS TIFF), `THMB` (after the 8-byte box header: u8 version (0 or 1), 3 flag bytes, u16 width (160), u16 height (120), u32 jpeg_size, u16 (=1), u16 (=0), then JPEG at box+24, padded to 4); then `mvhd`, `trak` x 3-5 (track 1 full-size JPEG, 2 SD crx, 3 HD crx, 4 metadata (CTMD), 5 dual-pixel) with `stsd` sample entries of type `CRAW` (43 52 41 57) and `stco/co64` chunk offsets into mdat |
| `uuid be7acfcb-97a9-42e8-9c71-999491e3afac` | XMP packet |
| `uuid eaf42b5e-1c98-4b88-b9fb-b7dc406e4d16` | `PRVW` box: u32 size, `PRVW`, u32 (0), u16 (1), u16 width, u16 height (1620x1080), u16 (1), u32 jpeg_size at +20, JPEG at +24, padded to 4 |
| `mdat` | track payloads in order: JPEG (full size), SD crx, HD crx, CTMD records, dual-pixel |

Note the task brief says `CRAW` is an ftyp compatible brand; it is not - `CRAW` is the
`stsd` sample-entry type inside each `trak`. CR3 detection = major brand `crx `.
Extent = `max(offset + size)` over all CTBO records with non-zero size (on bodies that write
CTBO record 5 the `CMTA` uuid box follows `mdat`, so `CTBO[3]` alone under-reads) and this
must equal the box-walk end; use both, they must agree. The embedded full-size JPEG (track 1, first `stco` entry) is extractable as a plain
JPEG even when the crx data is damaged.

Carving parameters: magic `66 74 79 70` at offset 4 with size at 0 in 16..=256; min size ~1 KB;
cap 512 MiB (CR3 from an R5 ~ 45 MB; HEIC bursts/sequences bigger); false positives: the
brand must be in the known list (or all-printable + at least one known compatible brand).
Validation: L1 box walk; L2 `meta` parse with every `iloc` extent inside `mdat`, `ispe`
present for the primary item; L3 none in pure Rust (HEVC decode needs libde265/libheif; AV1
needs dav1d) - the DiskRescue L3 for HEIC is "extract the `Exif` item + primary item extents
and verify the hvcC NAL array parses", not a pixel decode.

### 2.10 PSD / PSB (Adobe)

| Offset | Size | Field |
|---|---|---|
| 0 | `38 42 50 53` | "8BPS" |
| 4 | u16be | version: 1 = PSD, 2 = PSB |
| 6 | 6 | reserved, must be 0 |
| 12 | u16be | channels 1..56 |
| 14 | u32be | height 1..30000 (PSB 300000) |
| 18 | u32be | width (same limits) |
| 22 | u16be | depth 1, 8, 16, 32 |
| 24 | u16be | colour mode: 0 bitmap, 1 grey, 2 indexed, 3 RGB, 4 CMYK, 7 multichannel, 8 duotone, 9 Lab |
| 26 | u32be len + data | Colour mode data section |
| ... | u32be len + data | Image resources section (`8BIM` blocks; 0x0404 IPTC, 0x040C JPEG thumbnail, 0x0422 Exif, 0x0424 XMP) |
| ... | u32be (PSD) / u64be (PSB) len + data | Layer and mask info |
| ... | u16be compression + data | Image data: 0 raw (channels*height*stride, stride = ceil(width*depth/8)), 1 RLE (PackBits: per-row byte counts u16be (PSD) / u32be (PSB) for channels*height rows, then the rows), 2 ZIP, 3 ZIP with prediction |

Exact end: sum the four length-prefixed sections, then for compression 0 compute, for 1 sum
the row counts table, for 2/3 the deflate stream has no stored length -> inflate to find the
end (L2) or fall back to "next header". Min size 26 + 4 + 4 + 4 + 2; cap 2 GiB (PSD), 4 GiB
PSB. False positives: reserved zeros + field ranges make this solid.

### 2.11 MPO

A JPEG whose first image has an APP2 `MPF\0` MP Index IFD with NumberOfImages >= 2. Carve as
JPEG (2.1.6 already includes all images). Name `.mpo` only when any MPEntry type code is
0x020002/0x020003 or the Make is a 3D camera (Fujifilm FinePix REAL 3D, Nintendo 3DS).

### 2.12 Summary table

| Format | Magic (hex @ offset) | Min | Max cap | End rule | Validation without full decode |
|---|---|---|---|---|---|
| JPEG | `FF D8 FF` @0 | 256 B | 256 MiB | header walk + scan walk + MPF + trailers | L2 Huffman decode, MCU count |
| PNG | `89 50 4E 47 0D 0A 1A 0A` @0 | 67 B | 512 MiB | chunk walk to IEND | CRC + inflate + size |
| GIF | `47 49 46 38 [37|39] 61` @0 | 64 B | 256 MiB | block walk to 0x3B | LZW decode |
| BMP | `42 4D` @0 + DIB size @14 | 30 B | 1 GiB | computed from header | header ranges |
| WebP | `52 49 46 46 .. .. .. .. 57 45 42 50` @0 | 30 B | 256 MiB | RIFF size, chunk walk | VP8/VP8L header |
| TIFF/RAW | `49 49 2A 00`, `4D 4D 00 2A`, `49 49 52 4F`, `4D 4D 4F 52`, `49 49 52 53`, `49 49 55 00` @0 | 1 KiB | 512 MiB-1 GiB | max(IFD data ends) | IFD consistency, strip sizes |
| CR2 | TIFF + `43 52 02 00` @8 | 1 MiB | 512 MiB | same | + IFD3 raw strip SOF3 header |
| RAF | `46 55 4A 49 46 49 4C 4D 43 43 44 2D 52 41 57 20` @0 | 1 MiB | 512 MiB | max of 3 offset+len pairs | JPEG at jpeg_off |
| HEIF/AVIF/CR3 | `66 74 79 70` @4, brand @8 | 1 KiB | 512 MiB | top-level box walk | iloc extents in mdat |
| PSD | `38 42 50 53 00 0[1|2]` @0 | 40 B | 2 GiB | section lengths | field ranges |

---

## 3. Algorithms

### 3.1 Carver driver (all formats)

```
carve_images(image: &RescueImage, map: &Mapfile, out):
    for each readable run (from ddrescue mapfile; skip bad sectors, they read as zero):
        for pos in run.step_by(512):                       # 2.1.12 alignment note
            fmt = match_magic(image[pos..pos+32])           # table 2.12; cheapest first (JPEG)
            if fmt.is_none(): continue
            res = fmt.find_end(image, pos, cap)             # returns Ok(end) | Partial{end, reason} | Reject
            match res:
              Reject -> continue
              Ok(end) | Partial{end,..}:
                  grade = fmt.validate(image[pos..end], level)   # L1 always; L2 if --deep; L3 if --verify
                  if grade == Reject: continue
                  name = fmt.name_from_metadata(...)          # 2.1.9
                  write out/<fmt>/<name>; record {offset, end, grade, reason, mcu_rows_ok, ...}
                  if fmt is JPEG/PNG and grade != Good: repair::try(...) -> out/repaired/<name>
                  pos = end rounded down to 512 (overlapping headers inside the file, e.g.
                        embedded previews, were already extracted by fmt.find_end as children)
```

Rules: a sector-aligned hit that is strictly inside a previously carved file is skipped only
if that file validated at L2 or better (embedded previews in RAW/HEIC are emitted as
children `<name>.preview.jpg` by the parent's parser, not by the outer loop). If the parent
validated only at L1 or is Partial, the inner JPEG **is** carved separately (it may be the only
recoverable thing).

Bad-sector awareness: `find_end` receives the mapfile; if the extent crosses a `-` (bad
sector), `?` (non-tried), `*` (non-trimmed) or `/` (non-scraped) region (ddrescue manual,
"Mapfile structure"; only `+` is finished), the result is at best `Partial{reason: BadSectors(ranges)}`
and the repair module gets the list of zero-filled ranges inside the file (this is far more
useful than guessing: a 4 KiB zero hole inside scan data is precisely located).

### 3.2 JPEG validator (L2)

```
validate_jpeg(buf) -> Report:
    hdr = walk_headers(buf, 0)?                                 # 2.1.3
    build tables (2.1.10); geometry (2.1.5)
    for each scan (sequential: 1; progressive: iterate SOS segments):
        r = decode_scan(scan)                                   # symbol decode only, no IDCT
        record: mcus_done, total_mcus, byte offset of first bad bit, restart gaps, dc_out_of_range count
        if r is Partial: break
    classify:
        all scans complete and EOI found:            Good
        sequential, mcus_done < total:               Truncated{rows_ok = floor(mcus_done / mcus_x) * 8*Vmax, at = byte}
        sequential, bad symbol before data ran out:  CorruptMid{at = byte, mcu = mcus_done}
        progressive, N of M scans complete:          ProgressivePartial{scans_ok = N}
        header error:                                Reject or NeedsHeader (if SOI+scan-like data but SOF missing)
```

Cost: a symbol decoder runs at roughly 150-300 MB/s single-threaded in Rust (UNVERIFIED,
estimate from zune-jpeg's decode rate without IDCT/colour conversion); acceptable for the
`--deep` pass over carved candidates, not for every sector.

Corruption signatures the decoder detects (in order of how early they trigger): invalid
Huffman code (no code of length <= 16 matches: with standard AC tables every 16-bit prefix is
valid except all-ones so this is rare - about 1 in 100 random bytes trigger it for the
luminance AC table UNVERIFIED), run past block (`k > 63`), DC category > 11, DC predictor
drifting outside [-2048, 2047] (very reliable within ~5-20 blocks of random data), RST marker
with wrong index, RST marker at wrong MCU count, non-RST marker before `total_mcus`.

### 3.3 JPEG EOI / extent

See 2.1.6. Implementation note: the scan walk and the validator share code; at L1 the walk
uses `memchr(0xFF)`; at L2 the bit reader's `marker_hit` provides the same offsets.

### 3.4 JPEG truncated-file repair

Input: `Truncated{rows_ok, at}` or any file where the scan walk hits end-of-buffer.

```
repair_truncated(buf, report):
    out = buf[..at_last_complete_byte]
    if sequential and report.mcus_done < total:
        option A (fast, always works): out += padding such that the last byte is not FF; out += FF D9
            -> libjpeg-based viewers show the top rows and grey below ("Premature end of JPEG file")
            -> some viewers (Windows Photos, Android Gallery) refuse; so prefer option B
        option B (coefficient re-emit, lossless for decoded MCUs):
            coeffs = decoded blocks from the validator (store them during decode: for sequential we
                     need them only until written, so stream: decode MCU -> re-encode MCU)
            writer = HuffmanWriter(same DC/AC tables, same Ri)
            for mcu in 0..mcus_done: encode_mcu(coeffs[mcu])        # identical bits as source
            for mcu in mcus_done..total_mcus:
                encode_mcu(empty)                                   # DC diff 0 (category 0 code), AC EOB per block
                # grey = DC 0 after level shift; keep DC predictors coherent (diff 0 => pred unchanged =
                # continues the last colour instead of grey; to get neutral grey emit diff = -pred once)
            writer.flush_with_ones(); out += FF D9
            Requires the tables to contain DC category 0 and AC symbol 0x00 (EOB): standard tables do;
            optimised tables (libjpeg -optimize, phones) almost always do (every real image has EOBs);
            if not, fall back to option A.
    if progressive:
        cut after the last complete scan (the validator knows the byte after the last scan's
        last MCU and the marker position); out = buf[..cut]; out += FF D9
    emit out; report rows_ok / scans_ok
```

Huffman encoding for re-emit (F.1.2):

```
encode_dc(diff): t = bit_length(|diff|); emit(code[t], len[t]); if t > 0: emit((diff < 0 ? diff - 1 : diff) & ((1<<t)-1), t)
encode_ac(block): r = 0
    for k in 1..=63: c = block[zz[k]]
        if c == 0: r += 1; continue
        while r > 15: emit ZRL (code of 0xF0); r -= 16
        s = bit_length(|c|); emit(code[(r<<4)|s]); emit((c < 0 ? c - 1 : c) & ((1<<s)-1), s); r = 0
    if r > 0: emit EOB (code of 0x00)
byte output: after each full byte, if byte == 0xFF emit 00 after it; before RST/EOI pad with 1-bits
```

Because the decoder read the same symbols with the same tables, re-encoding reproduces the
source bits exactly (the mapping symbol -> code is a bijection within a table); the only
differences are the padding bytes, which are reconstructed by the same rule.

### 3.5 JPEG mid-stream corruption

Cases and actions:

| Symptom | Cause | Action |
|---|---|---|
| Decoder errors at byte `at`, `DRI` present | a few bad/zeroed bytes | find next `FF Dn` after `at`; compute n's expected position: `mcu_index = (interval_index+1) * Ri`; emit coefficients for MCUs `mcus_done..mcu_index` as empty (grey) interval(s); continue decoding from the RST; if the RST sequence skips values, insert that many empty intervals (geometry preserved) |
| Error, no DRI, zeros of exactly one cluster (mapfile says bad sector) | bad sector | truncate at last good MCU (3.4 option B); additionally try: skip the zero region and attempt to resynchronise by brute force - for each bit offset in the next 64 bytes after the hole, decode 32 MCUs; accept the offset whose DC predictors stay in range and whose decode continues to EOI with the right total MCU count. If found, the image after the hole is correct except for a horizontal shift and colour cast (DC predictors lost); emit as `<name>.resync.jpg` with a warning. Shift correction (re-aligning rows) requires knowing how many MCUs were lost: try the candidate counts that make the remaining MCU count exact, pick the one that minimises the DC discontinuity at row boundaries UNVERIFIED (heuristic, needs evaluation) |
| Error, no DRI, data continues plausibly (not zeros) | cluster of another file (fragmentation) | truncate at last good MCU; flag `fragmented_at`; store last good MCU count so a gap-carving pass can test candidate continuation clusters by decoding from them with the same predictors |
| Image decodes fully but colours shift from some row on, or image shifted horizontally | bytes inserted/deleted mid-stream without a symbol error (Huffman streams resync by chance) | nothing automatic; report |

### 3.6 JPEG header repair

**Missing or corrupt header (data starts with scan-like bytes, or SOI..SOS damaged):**

```
repair_header(scan_data, donors):
    for donor in donors (same Make/Model from neighbouring files, same extension, sorted by closeness of file size):
        hdr = donor[SOI .. SOS end]              # includes DQT, DHT, SOF, DRI, APPn
        candidate = hdr + scan_data
        r = validate_jpeg(candidate)
        score = r.mcus_done / r.total_mcus, DC range violations, restart marker agreement (if DRI: the RSTn positions in the data must land at Ri-MCU boundaries - this is a very strong signal: a wrong SOF/sampling makes the count wrong immediately)
        keep best
```

What must match between donor and victim: `X`, `Y` (if the victim's scan has RSTs you can
*derive* X from the number of MCUs per restart interval and the total count: `total_mcus =
intervals * Ri` -> `mcus_x * mcus_y`, usually factorable to the camera's known geometry),
`Nf`, `Hi/Vi` per component, `P`, baseline vs progressive (and for progressive the whole scan
script), Huffman tables (**must** match exactly if the camera uses optimised tables; standard
tables are interchangeable), `Ri`. What may differ: DQT (wrong tables decode with wrong
contrast/brightness/blockiness but geometry is intact; try donors with the same quality
setting, e.g. same camera "Fine" vs "Normal"), APPn metadata (Exif will describe the donor
shot - strip or rewrite DateTimeOriginal from the filesystem timestamp and mark the file as
header-transplanted in the report).

**Missing DHT (Motion-JPEG convention):** AVI/MOV MJPEG frames, some webcams and some
security cameras omit DHT; decoders assume the Annex K tables (OpenDML AVI spec). If SOS
references table 0/1 and no DHT was seen, insert the 420-byte segment from 2.1.11 before SOS.
Both Rust decoders apply the Annex K tables automatically **only** when the file carries an
APP0 segment whose payload starts with `AVI1\0` (the OpenDML marker): jpeg-decoder sets
`is_mjpeg` on `AppData::Avi1` and calls `fill_default_mjpeg_tables` (`decoder.rs`); zune-jpeg
0.5.15 sets `is_mjpeg` on `AVI1\0` and fills the tables (`decoder.rs`, `huffman.rs`
`fill_mjpeg_tables`). Neither exposes an API to force it. Verified on fixtures
(2026-10-12): a DHT-stripped JFIF file fails in jpeg-decoder with "scan makes use of unset
dc huffman table" and in zune-jpeg with `HuffmanDecode("No DC table for component Y")`
(strict and non-strict alike); the same bytes with the APP0 rewritten to `AVI1\0` decode in
both. So the repair must either insert the DHT segment (preferred, viewer-portable) or
rewrite/insert an `AVI1` APP0 before handing the file to a crate.

**Bad segment length:** when `walk_headers` fails at segment `S` (length points to a byte that
is not FF, or past EOF), scan forward from `S+4` for the next `FF xx` with `xx` a valid
header marker whose own length lands on another valid marker (two-link chain check). If
found, rewrite `S`'s length to point there (if the segment is APPn/COM) or drop the segment
(if it is DQT/DHT with inconsistent content: a DQT whose entries contain 0 is dead; recover
with donor tables).

**Stray bytes before SOI / double SOI:** carve from the last `FF D8 FF` that precedes a valid
header chain; strip anything before it. `FF D8 FF D8 FF E0`: drop the first SOI.

**Missing EOI only (everything decodes):** append `FF D9`. Common and trivial; many viewers
already tolerate it.

**Grey / half image:** this *is* truncation or a hole (3.4/3.5). A fully grey image with a
valid header = scan data lost (first cluster only recovered) - not repairable; keep the
header's Exif thumbnail (IFD1 0x0201/0x0202) as `<name>.thumb.jpg` since it is often the only
surviving picture.

**Colour shift / wrong colours explanation (for the report text):** (a) DC predictors are
differential per component; after a resync without a restart marker every later block of a
component carries an unknown constant offset -> uniform colour cast per 8x8 region that
changes at the corruption point; (b) wrong Huffman tables -> garbage, not a cast; (c) wrong
DQT -> contrast/brightness error, blocky; (d) missing JFIF/Adobe markers on a 3-component
image whose component ids are not 1,2,3 -> decoder guesses RGB vs YCbCr wrongly (libjpeg
rule: ids 1,2,3 or JFIF present -> YCbCr; ids 'R','G','B' -> RGB; Adobe transform 0 -> RGB,
1 -> YCbCr; 4 components: Adobe transform 2 -> YCCK else CMYK) - fix by inserting an APP14
Adobe segment with the right transform or by renumbering component ids in SOF and SOS;
(e) wrong sampling factors in a donor header -> smeared chroma, image stretched.

**Not repairable (say so in the report):** scan data overwritten (zeros or another file)
beyond what the header + first rows provide; files whose only remaining bytes are the header
(thumbnail salvage only); progressive files missing the DC first scan (nothing decodes);
arithmetic-coded JPEG with corruption (no restart-free resync possible, few decoders);
encrypted or compressed-container content; "repair" of pixel content itself (inpainting the
grey area) is out of scope and must never be presented as recovery.

### 3.7 PNG validation and repair

```
validate_png(buf):
    chunks = walk(buf)                                  # L1
    ihdr = parse(chunks[0]); expect PLTE if colour 3; expect >= 1 IDAT; IEND last
    for c in chunks: if crc32(type+data) != c.crc: bad_crc.push(c)
    z = inflate_stream(concat IDAT data)                # L2, streaming, record bytes_out at error
    expected = expected_inflated_size(ihdr)
    rows_ok = complete rows from bytes_out (non-interlaced: floor(bytes_out / stride); interlaced: per pass)
    filter bytes: every row's first byte <= 4 else CorruptAt(row)
    adler check at the end
```

Repair:

| Problem | Fix |
|---|---|
| Bad CRC on an ancillary chunk | recompute and rewrite CRC (data assumed good) or drop the chunk; prefer rewrite for tEXt/tIME/eXIf, drop for iCCP if it also fails to inflate |
| Bad CRC on IHDR/PLTE/IDAT | if the chunk inflates/decodes consistently (IDAT), rewrite the CRC; if IHDR fields are implausible, donor IHDR (same source: screenshots from the same device share width/height/colour type/depth) |
| Truncated IDAT (inflate hits EOF) | inflate what exists; `rows_ok` complete rows; **emit a new PNG**: unfilter the rows, re-filter (type 0 or 1, no need to optimise), deflate, write IHDR with the original height and append `rows_ok..height` rows filled with 0 (black; or copy the last good row - pick black so the damage is visible), or write height = rows_ok if `--crop-truncated`. The original deflate stream cannot be cut at a row boundary without re-encoding. |
| Truncated interlaced IDAT | passes completed give a full-frame low-resolution image: upsample the last complete pass to full size (nearest) and overlay later passes' complete rows; emit non-interlaced. |
| Missing IEND | append `00 00 00 00 49 45 4E 44 AE 42 60 82` |
| Bad zlib header / Adler | rewrite header to `78 9C`? No: CMF/FLG are only needed for inflate's window check; inflate with `miniz_oxide` raw deflate from byte 2 if the header is bad; ignore the Adler mismatch and report it |
| Chunk length corrupt | find the next chunk by searching for a 4-letter type at `pos + 8 + L'` for candidate `L'` such that the CRC at that position matches (CRC over type+data for that `L'`): one pass with a rolling check is cheap for chunks up to a few MB |
| Missing/garbage IHDR with intact IDAT | donor IHDR + brute force: try the candidate (width, colour type, depth) set from donors; the correct one is the only one where every row's filter byte is <= 4 for the whole inflated stream and the total size matches exactly |

Not repairable: deflate stream damage before the truncation point (the stream is not
restartable; everything after the bad block is lost; a single bad bit yields garbage rows
followed by an inflate error - truncate to the last row whose filter byte and pixel
statistics look sane, UNVERIFIED heuristic).

### 3.8 TIFF/RAW/HEIF/PSD extent and validation

See 2.6-2.10; these formats get extent + L1/L2 structural validation and embedded-preview
extraction (every RAW format above carries a full-size or large JPEG preview that is itself
repairable with 3.4-3.6). Pixel-level raw repair is out of scope; a RAW whose raw strip is
partially missing is emitted as `partial` with its preview extracted.

---

## 4. Edge cases and failure modes seen on real failing media

1. **Zero-filled holes at 4 KiB or 512 B granularity** (ddrescue fills unreadable sectors with
   zeros): inside JPEG scan data a run of `00` bytes decodes as valid-looking symbols for a
   while (with standard tables `00 00` is DC category 0 + a few AC codes), so the error is
   detected tens of MCUs later. Use the mapfile to know the true hole; do not trust the first
   symbol error position when it is after a hole.
2. **Read-retry garbage**: dying HDDs sometimes return the wrong sector (stale data from a
   neighbouring LBA) rather than an error. Signature: a JPEG decodes with one 512 B span
   that belongs to another JPEG (RST sequence jumps, Exif in the middle of a scan). Only the
   RST-interval resync handles it.
3. **Flash/SD wear-levelling remap**: after a controller failure the chip is often read
   directly (chip-off) and the image is in physical page order with ECC/spare bytes between
   pages (e.g. 2048 + 64). Carving must then operate on a de-interleaved image; DiskRescue
   should detect the periodic spare pattern (constant-stride non-data bytes breaking JPEG
   streams at exact multiples of 2112/4224/...) and refuse rather than emit garbage.
4. **FAT32 cluster slack after EOI**: trailing bytes of the previous occupant of the cluster
   (often another JPEG's tail). Never include them; 2.1.8 rule.
5. **Fragmented large files**: video more than photos, but 20+ MB JPEGs from modern bodies on
   nearly-full cards fragment too. Signature in 3.5.
6. **Cameras that write files twice (JPEG+RAW)**: the RAW's embedded JPEG is often identical
   to the sidecar JPEG; dedupe by Exif ImageUniqueID / DateTimeOriginal+SubSec+Model before
   reporting counts, otherwise the user sees 3x the photos they lost.
7. **Progressive JPEGs from phones/web** (most social-media downloads, some Samsung modes): a
   truncation leaves a complete but soft image - grade `partial` but tell the user it is
   "complete at reduced quality", not "half an image".
8. **Apple HEIC with multiple items** (Live Photo still + depth + gain map + thumbnail), all in
   one mdat; a truncated mdat may still contain the full primary item (it is first). Check per
   item extents.
9. **HEIC/CR3 with `mdat` before `moov`/`meta`** (not Apple/Canon, but some encoders): the
   box walk still works; the iloc evidence is simply found later.
10. **Samsung Motion Photo**: 2-3 MB JPEG followed by a 2-4 MB MP4; carving only the JPEG is
    acceptable but the report should mention the video part so the video carver output can be
    matched (same offset range).
11. **Exif with offsets beyond the APP1 segment**: some phones write `0x0201` thumbnail
    offsets that overflow; never follow them outside the segment.
12. **Fill bytes `FF FF FF ...` before markers inside scan data** (some Kodak/old Canon)
    and **`FF` as last byte before `D9`**: handled by the bit reader rules.
13. **DNL (height = 0 in SOF)**: extremely rare (some scanners); the MCU count is unknown until
    DNL after the first scan; treat `Y = 0` as "walk to marker" only and skip L2.
14. **12-bit JPEG** (SOF1 with P = 12, medical/DNG lossy): decode symbols the same way, DC
    categories up to 15; most viewers refuse it; do not convert.
15. **Lossless JPEG (SOF3) in CR2/DNG**: different predictor model (no AC tables, no zigzag);
    the L2 JPEG validator must not be applied to it; only the length-based extent.
16. **PNG from screenshots with hundreds of IDAT chunks of 8 KB or 64 KB**: a chunk walk with
    per-chunk CRC is fine; expected.
17. **BMP with bfSize = 0** (older Windows apps, some scanners) and **negative height**.
18. **TIFF with IFD entries not sorted / duplicate tags** (scanner software); do not reject,
    just lower the confidence.
19. **RAW files where the preview JPEG is larger than 16 MB** (DNG from phones with
    full-size previews, CR3 track 1): the embedded JPEG extraction must use the same cap as
    the top-level JPEG carver.
20. **Bit rot on SSD/flash (single bit flips)**: in JPEG scan data a flipped bit usually
    desynchronises the Huffman stream (blocky garbage until the next RST); in PNG it makes
    inflate fail at that block. There is no error correction; the repair is truncation or
    RST-resync. In headers, a flipped bit in a length field is recoverable by 3.6.
21. **Controller "format on recovery" cards**: entire card reads as zeros/FF; nothing to carve;
    detect "all-zero image" early and tell the user (chip-off needed).

---

## 5. Test fixture strategy

Tools: Python 3 + Pillow, ffmpeg (libx264, libx265, aac), standard shell. Everything runs
without root under `tests/fixtures/gen_images.py`. Fixtures are generated into
`target/fixtures/images/` at test time (deterministic seeds) so no binaries are committed.

### 5.1 Base images

```python
# tests/fixtures/gen_images.py
from PIL import Image, ImageDraw
import numpy as np, struct, zlib, io, os, subprocess, random

OUT = "target/fixtures/images"; os.makedirs(OUT, exist_ok=True)
rng = np.random.default_rng(1234)

def base(w=640, h=480):
    a = rng.integers(0, 255, (h, w, 3), dtype=np.uint8)      # noise = large JPEG, exercises AC codes
    img = Image.fromarray(a)
    d = ImageDraw.Draw(img); d.rectangle([40, 40, 300, 200], fill=(200, 30, 30)); d.text((50, 50), "DiskRescue", fill=(255,255,255))
    return img

def exif_bytes(dt="2024:05:06 07:08:09", make="DiskRescue", model="FixtureCam", orient=6):
    ex = Image.Exif()
    ex[0x010F] = make; ex[0x0110] = model; ex[0x0112] = orient; ex[0x0132] = dt
    ex.get_ifd(0x8769)[0x9003] = dt
    return ex.tobytes()                           # Pillow emits "Exif\0\0"+TIFF ready for APP1

img = base()
img.save(f"{OUT}/baseline.jpg", "JPEG", quality=90, exif=exif_bytes())                  # SOF0, 4:2:0, with Exif
img.save(f"{OUT}/baseline_444_rst.jpg", "JPEG", quality=85, subsampling=0, exif=exif_bytes())  # 4:4:4
img.save(f"{OUT}/progressive.jpg", "JPEG", quality=80, progressive=True, exif=exif_bytes())
img.save(f"{OUT}/optimized.jpg", "JPEG", quality=75, optimize=True)                     # non-standard DHT
img.convert("L").save(f"{OUT}/grey.jpg", "JPEG", quality=70)                            # Nf = 1
img.convert("CMYK").save(f"{OUT}/cmyk.jpg", "JPEG", quality=70)                         # Nf = 4, Adobe APP14
# thumbnail inside APP1 (the EOI-inside-header trap): Pillow does not write IFD1 thumbnails; build one:
thumb = io.BytesIO(); img.resize((160,120)).save(thumb, "JPEG", quality=50)
# -> use exif_with_thumbnail() below (hand-built TIFF with IFD1 0x0201/0x0202) and pass exif=...
img.save(f"{OUT}/interlaced.png", "PNG", interlace=1)
img.save(f"{OUT}/rgb.png", "PNG")
img.convert("P").save(f"{OUT}/palette.png", "PNG")
img.save(f"{OUT}/anim.gif", "GIF", save_all=True, append_images=[img.rotate(90), img.rotate(180)], duration=100, loop=0)
img.save(f"{OUT}/rgb24.bmp", "BMP")
img.save(f"{OUT}/lossy.webp", "WEBP", quality=80)
img.save(f"{OUT}/lossless.webp", "WEBP", lossless=True)
img.save(f"{OUT}/strips.tif", "TIFF", compression=None)          # StripOffsets/StripByteCounts
img.save(f"{OUT}/strips_lzw.tif", "TIFF", compression="tiff_lzw")
```

Restart intervals: Pillow >= 10.2.0 exposes libjpeg's restart markers directly:
`img.save("rst.jpg", "JPEG", quality=90, restart_marker_blocks=8)` (DRI every 8 MCUs) or
`restart_marker_rows=1` (one interval per MCU row) - Pillow handbook, "JPEG saving options"
(https://pillow.readthedocs.io/en/stable/handbook/image-file-formats.html#jpeg-saving);
verified with Pillow 12.1.1 that the output contains `FF DD` and `FF D0..D7`. ffmpeg's mjpeg
encoder has **no** restart-interval option (`ffmpeg -h encoder=mjpeg` on 8.0 lists only
`huffman`, `force_duplicated_matrix` and the generic mpegvideo flags; same in the codecs
manual), so do not use it for DRI fixtures. Our own re-emitter from 3.4 should additionally
regenerate `rst.jpg` from `baseline.jpg` symbols with `Ri = 8` as a round-trip test of the
encoder (output must be byte-identical to Pillow's apart from APPn/COM segments).

Motion-JPEG without DHT: `ffmpeg -y -i baseline.jpg -c:v mjpeg -q:v 4 -frames:v 1 mjpeg.avi`
then extract the frame bytes from the AVI (`00dc` chunk) and strip the DHT segment with
Python (ffmpeg writes DHT; the stripped copy models the OpenDML convention).

### 5.2 Hand-built structures (Python `struct`)

- `exif_with_thumbnail()`: build `Exif\0\0` + `II*\0` + IFD0 (Make, Model, Orientation,
  DateTime, ExifIFD ptr) + ExifIFD (DateTimeOriginal) + IFD1 (0x0201/0x0202 + 0x0103 = 6) +
  the thumbnail JPEG bytes. Verifies the carver does not stop at the thumbnail's EOI.
- `mpf_file()`: two JPEGs (main + a 320x240 one) with an APP2 `MPF\0` MP Index IFD
  (0xB000, 0xB001 = 2, 0xB002 = two 16-byte entries with offsets relative to the TIFF header).
  Also a 3-image version with type codes 0x020002 (expect `.mpo`).
- `png_apng()`: add `acTL`/`fcTL`/`fdAT` chunks around a Pillow PNG's IDAT.
- `png_cgbi()`: prepend a `CgBI` chunk and strip the zlib header (raw deflate) - marks variant.
- `psd_raw()`: `8BPS`, version 1, 3 channels, 640x480, depth 8, mode 3, three empty
  sections, compression 0, raw planar data from the base image; and an RLE (PackBits) variant
  with the row-count table.
- `tiff_raw_family()`: generic TIFF builder taking `(byteorder, magic_word, ifd0_offset,
  extra_header_bytes)` and a list of IFDs with tags and out-of-line data, producing:
  - `cr2`: `II*\0` + `CR\x02\x00` + u32 IFD3 offset; IFD0 strips = JPEG preview, IFD1
    0x0201/0x0202 = thumbnail, IFD2 small RGB strips, IFD3 = "raw" strips (random bytes with
    `FF D8 FF C3` lossless header), 0xC640 CR2Slice.
  - `nef`: `MM`, IFD0 thumb strips + 0x014A SubIFDs -> [preview JPEG via 0x0201/0x0202, raw
    strips with Compression 34713], Make "NIKON CORPORATION".
  - `arw`: `II`, Make "SONY", IFD0 0x0201/0x0202 preview + SubIFD raw strips Compression 32767,
    0xC634 pointing at a small private IFD placed *after* the raw strip (tests max-of-ends).
  - `dng`: 0xC612 = 01 04 00 00, IFD0 preview strips (NewSubFileType 1), SubIFD raw tiled
    (0x0144/0x0145, 4 tiles) + 0xC68C OriginalRawFileData blob placed last.
  - `pef`: Make "PENTAX Corporation", IFD0 raw strips (Compression 65535), IFD1 thumb.
  - `orf`: magic `IIRO` and `MMOR`, IFD0 raw strips, IFD1 thumb, MakerNote `OLYMPUS\0II` with
    a preview beyond all strips (tests makernote extent).
  - `rw2`: `49 49 55 00 18 00 00 00`, IFD0 at 0x18 with 0x0002/0x0003 sensor size, 0x002D
    RawFormat 4, 0x002E JpgFromRaw (a small JPEG), 0x0118 RawDataOffset pointing at a raw blob
    of `ceil(w*h*16/14)` bytes rounded to 0x4000 at EOF.
  - `erf`, `srw`: Make strings only.
- `raf_file()`: header per 2.8 with real offsets to an embedded JPEG (Pillow) at 160, a CFA
  header (count 2: 0x0100 size, 0x0130 layout) and a CFA TIFF blob; also a variant where
  the JPEG is **after** the CFA (max-of-pairs test).
- `heic_file()`: `ftyp(heic, mif1 heic)` + `meta` (hdlr pict, pitm 1, iinf with infe v2
  items: 1 = hvc1, 2 = Exif; iprp/ipco with ispe 640x480 and a minimal hvcC; ipma; iloc v0
  with offset_size 4, length_size 4, base_offset_size 0, item 1 extent -> mdat payload, item 2
  -> Exif payload) + `mdat` containing (a) the Exif item (`u32be 6` + `Exif\0\0` + TIFF built
  by `exif_with_thumbnail`) and (b) an HEVC intra frame from
  `ffmpeg -y -f lavfi -i testsrc=size=640x480:rate=1 -frames:v 1 -c:v libx265 -x265-params keyint=1 -f hevc frame.h265`
  (Annex B bytes copied raw; the carver never decodes HEVC, so this is sufficient for extent
  and iloc tests). AVIF variant: brand `avif`, item type `av01`, payload random bytes.
  CR3 variant: `ftyp(crx , 1, crx isom)` + `moov` containing `uuid 85c0b687...` with `CNCV`,
  `CTBO` (3 records) and `CMT1` (TIFF from the Exif builder) + `uuid eaf42b5e...` with `PRVW`
  + JPEG + `mdat` with the full JPEG first. ffmpeg has **no HEIF muxer** at all as of
  8.0/9.0 (`ffmpeg -muxers` lists `avif` but no `heif`; the Changelog records only
  "Support for HEIF/AVIF still images and tiled still images" = *demuxing* in 7.0 and
  "ffmpeg CLI tiled HEIF support" in 8.1), so real HEIC fixtures must be hand-built as above
  or taken from a real device. AVIF fixtures can be produced with ffmpeg's `avif` muxer
  (`ffmpeg -i x.png -c:v libaom-av1 -still-picture 1 x.avif`, muxer documented in
  ffmpeg-formats "avif").
- `bmp_bad_size()`: Pillow BMP with bfSize patched to 0 and a top-down (negative height)
  variant.
- `webp_vp8x()`: Pillow WebP with `exif=` and an ICC profile -> VP8X path.

### 5.3 Corruption generators (apply to every base fixture)

```python
def truncate(b, frac): return b[: int(len(b) * frac)]
def zero_hole(b, off, n=4096): return b[:off] + b"\0" * n + b[off + n:]
def flip_bits(b, offs): a = bytearray(b); [a.__setitem__(o, a[o] ^ 0x08) for o in offs]; return bytes(a)
def strip_header(b):   # JPEG: drop SOI..SOS
    i = b.find(b"\xff\xda"); L = struct.unpack(">H", b[i+2:i+4])[0]; return b[i + 2 + L:]
def strip_segment(b, marker):  # e.g. 0xC4 for DHT
    p = 2
    while p + 4 <= len(b) and b[p] == 0xFF and b[p+1] != 0xDA:
        L = struct.unpack(">H", b[p+2:p+4])[0]
        if b[p+1] == marker: return b[:p] + b[p+2+L:]
        p += 2 + L
    return b
def bad_length(b, marker, delta): ...          # patch Lx of the first `marker` segment
def prepend_junk(b, n=777): return os.urandom(n) + b
def drop_eoi(b): return b[:-2] if b.endswith(b"\xff\xd9") else b
def splice(b1, b2, off): return b1[:off] + b2[off:]   # fragment from another file
def disk_image(files, cluster=4096):               # lay files at cluster boundaries with slack = previous tails
    img = bytearray(); for f in files: img += f; img += b"\xA5" * (-len(f) % cluster); return bytes(img)
```

Cases per format (each a named fixture + expected carver/validator/repair outcome):

| Fixture | Expect |
|---|---|
| `baseline.jpg` in a 4 KiB-cluster image with 5 other files | extent exact, L2 Good, name from DateTimeOriginal `20240506_070809_...` |
| `exif_thumb.jpg` | extent exact (not stopping at thumbnail EOI) |
| `mpf2.jpg`, `mpo3.mpo` | extent includes all images; `.mpo` only for 3D codes |
| `truncate(baseline, 0.6)` | Truncated{rows_ok ~ 60 %}; repair option B output decodes with png crate? no: with zune-jpeg strict mode **without error**, top 60 % identical to source (pixel compare), rest grey |
| `truncate(progressive, 0.5)` | ProgressivePartial; repaired file decodes fully in strict mode |
| `zero_hole(baseline_444_rst, mid, 4096)` | CorruptMid; RST resync; repaired decodes, only the hole's intervals grey, geometry intact |
| `zero_hole(baseline, mid)` (no DRI) | truncation at last good MCU, `fragmented_at` unset, hole offset reported from the mapfile |
| `strip_header(baseline)` + donor `baseline_sameparams.jpg` | header transplant succeeds; wrong-donor (`baseline_444_rst`) rejected by RST/geometry check |
| `strip_segment(mjpeg, 0xC4)` | DHT inserted; decodes |
| `bad_length(baseline, 0xE1, +37)` | header chain repaired |
| `prepend_junk`, `drop_eoi`, double SOI | handled |
| `optimized.jpg` truncated | option B works (EOB present in tables) |
| `cmyk.jpg`, `grey.jpg` | L2 Good; component rules |
| `truncate(rgb.png, 0.5)` | rows_ok; repaired PNG opens in png crate with original dimensions |
| `truncate(interlaced.png, 0.3)` | low-res full-frame repaired output |
| `flip_bits(rgb.png, [IDAT CRC byte])` | CRC rewrite; `flip_bits(..., [tEXt data])` -> chunk dropped or CRC rewritten per policy |
| all RAW fixtures | extent exact to the byte; embedded preview JPEG extracted and validated; Make/Model/date naming |
| `rw2` | extent within [estimate, estimate + 0x4000), confidence low |
| `raf` (both orderings) | exact |
| `heic`, `avif`, `cr3` | exact; iloc extents checked; Exif item parsed; CR3 full JPEG extracted |
| `truncate(heic, 0.7)` | partial; primary item still extracted if its extents fit |
| `disk_image(all)` carve pass | every fixture recovered once, no duplicates from embedded previews (dedupe), no slack bytes in outputs |

Expected results are asserted by (a) byte-equality of extents against the generator's known
lengths, (b) decoding the repaired outputs with the Rust crates in strict mode, (c) pixel
comparison of the good region against the source decode.

---

## 6. Recommended Rust crates

Versions from crates.io on 2026-10-12 unless tagged.

| Crate | Version | Pure Rust | Use | Behaviour on truncated / corrupt input |
|---|---|---|---|---|
| `zune-jpeg` | 0.5.15 | yes | L3 JPEG decode; DHT-less MJPEG only when APP0 is `AVI1\0` (3.6) | Verified on a 60 %-truncated baseline fixture (2026-10-12): non-strict (default) `decode()` returns `Ok(full-size buffer)` with the missing region filled with 128 (grey); strict (`zune_core::options::DecoderOptions::default().set_strict_mode(true)` passed to `JpegDecoder::new_with_options(ZCursor::new(buf), opts)`) returns `Err(DecodeErrors::ExhaustedData)`. **`decoded_output_bytes()` / `decoded_scanlines()` do not exist in 0.5.15** (they are on the `dev` branch / 0.5.16-rc); to know how many rows are valid use our own L2 validator, or wait for 0.5.16. EOI marker while parsing headers -> `Err(FormatStatic("Premature End of image"))` (`decoder.rs`). Use strict for grading, non-strict for "show what we have". |
| `jpeg-decoder` | 0.3.2 | yes | reference second opinion; fills Annex K tables automatically when APP0 `AVI1` is present (no API to force it) | Verified: `decode()` on the truncated fixture -> `Err("failed to fill whole buffer")`; **never** a partial image. Missing DHT without `AVI1` -> `Err("scan makes use of unset dc huffman table")`; with `AVI1` APP0 -> decodes. |
| `image` | 0.25.10 | yes | one-stop decode for PNG/GIF/BMP/WebP/TIFF (JPEG goes through zune-jpeg) | JPEG: `JpegDecoder::new` builds zune options with `set_strict_mode(matches!(spec, SpecCompliance::Strict))` and the default `SpecCompliance` is lenient (`codecs/jpeg/decoder.rs`), so `image::load_from_memory` on a truncated JPEG returns `Ok` with grey fill (verified). Verified on truncated fixtures: PNG -> `Err(unexpected end of file)`, WebP -> `Err(Corrupt bitstream)`, TIFF -> `Err(failed to fill whole buffer)`, **GIF -> `Ok` (first frame only is decoded by `load_from_memory`, so a GIF truncated in frame 2 passes)**. No partial output for PNG/WebP/TIFF; use the per-format crates below for that. |
| `png` | 0.18.1 | yes | PNG decode and the repair writer (`Encoder`) | Verified: on a 60 %-truncated 256x192 RGB PNG `Reader::next_row` returned 124 complete rows then `Err(DecodingError::IoError(UnexpectedEof))`; rows already returned stay valid (streaming), so the repair module uses `next_row` to collect complete rows. `DecodeOptions` (docs.rs 0.18): `set_ignore_crc` (default false), `set_ignore_adler32` (default true), `set_skip_ancillary_crc_failures` (default true), `set_ignore_text_chunk` (default false), `set_ignore_iccp_chunk` (default false), `set_ignore_checksums`. |
| `miniz_oxide` | 0.9.1 | yes | streaming inflate for PNG/PSD with exact `bytes_out` at failure (`inflate::stream::inflate` with `MZFlush::None`, returns `TINFLStatus::FailedCannotMakeProgress`/`NeedsMoreInput` and the counts) | returns status + consumed/produced counts; partial output retained. |
| `flate2` | 1.1.10 | yes (default backend = miniz_oxide) | convenience inflate | `read` returns bytes up to the error; then `Err(UnexpectedEof)`/`InvalidData`. |
| `crc32fast` | 1.5.2 | yes | PNG chunk CRC (same polynomial as zlib) | n/a |
| `kamadak-exif` | 0.6.1 | yes | Exif from JPEG/TIFF/HEIF/PNG/WebP containers for naming | default: stops at first error -> `Err(Error::InvalidFormat)` on truncation; `Reader::continue_on_error(true)` -> `Err(Error::PartialResult)` from which `distill_partial_result()` yields the fields parsed so far + error list. Use continue-on-error. |
| `tiff` | 0.11.3 | yes | TIFF/DNG strip & tile decode for L3 on plain TIFF; also exposes IFD walking | Verified: `Decoder::new` + `dimensions()` + `strip_count()` succeed on a truncated file (IFD intact); `read_chunk(i)` on the truncated strip -> `Err(TiffError::IoError(UnexpectedEof, "failed to fill whole buffer"))`; `read_image()` errors as a whole. `read_chunk` seeks per strip, so strips that lie entirely before the truncation decode (single-strip fixture only tested the error path). RAW compressions (34713, 32767, 65535) unsupported -> `UnsupportedError`. |
| `image-webp` | 0.2.4 | yes | WebP L3 | Verified: `WebPDecoder::new` parses the (RIFF-size-inconsistent) header fine; `read_image` on the truncated lossy fixture -> `Err(DecodingError::BitStreamError)`; no partial output. |
| `gif` | 0.14.2 | yes | GIF L2/L3 (`Decoder::read_next_frame`) | Verified: 3-frame fixture truncated at 60 % -> `read_next_frame` returns frame 1 `Ok`, then `Err(DecodingError::Io(UnexpectedEof))`; frames before the truncation are returned. |
| `zune-png` | 0.5.2 | yes | faster PNG alternative | **Not lenient**: verified `decode_raw()` on the truncated fixture -> `Err("I/O error Not enough bytes, expected 4 but found 0")` in both default and `set_strict_mode(true)`; no partial rows. Use the `png` crate for row salvage. |
| `rawloader` | 0.37.2 | yes | optional L3 for CR2/NEF/ARW/DNG/PEF/ORF/RW2/RAF/ERF/SRW raw decode (validation that the raw strip decodes) | `Err` on bad data; heavy; feature-gated `raw-verify` |
| `memchr` | 2.8.3 | yes | fast `FF` scanning in entropy data | n/a |
| `libheif-rs` | 3.0.0 | **no** (links libheif) | not used by default; optional feature `heif-verify` for L3 HEIC | n/a |
| `mp4parse` | 0.17.0 | yes | not needed: own box walker is 150 lines and must tolerate damage | n/a |

Policy: our own parsers (sections 2-3) do all extent and L1/L2 work; crates are for L3 and for
writing repaired PNGs. Every crate decode runs with a panic guard (`std::panic::catch_unwind`)
and a size cap, since carved input is adversarial by nature.

---

## 7. Sources

- ITU-T T.81 / ISO/IEC 10918-1 (JPEG): https://www.w3.org/Graphics/JPEG/itu-t81.pdf - markers (B.1), segment layouts (B.2), Huffman table generation (Annex C), decoding procedures (F.2.2), progressive (Annex G), standard tables (Annex K.3).
- libjpeg-turbo `jstdhuff.c` (Annex K tables as shipped; used to cross-check the hex arrays): https://raw.githubusercontent.com/libjpeg-turbo/libjpeg-turbo/main/src/jstdhuff.c
- JFIF 1.02: https://www.w3.org/Graphics/JPEG/jfif3.pdf
- Exif 2.32 / CIPA DC-008: https://www.cipa.jp/std/documents/e/DC-008-Translation-2019-E.pdf
- CIPA DC-007 Multi-Picture Format (MPF): https://www.cipa.jp/std/documents/e/DC-007_E.pdf
- Adobe APP14 / Photoshop file format (PSD, APP13): https://www.adobe.com/devnet-apps/photoshop/fileformatashtml/
- PhotoRec sources (JPEG end detection, MPF, 50 MiB cap; TIFF extent; RAF; PNG; ISO BMFF brands): https://github.com/cgsecurity/testdisk/blob/master/src/file_jpg.c , https://github.com/cgsecurity/testdisk/blob/master/src/file_tiff_le.c , https://github.com/cgsecurity/testdisk/blob/master/src/file_raf.c , https://github.com/cgsecurity/testdisk/blob/master/src/file_png.c , https://github.com/cgsecurity/testdisk/blob/master/src/file_mov.c
- W3C PNG 3rd edition: https://www.w3.org/TR/png-3/ (signature, chunks, IHDR, filters, Adam7, CRC, APNG)
- GIF89a specification: https://www.w3.org/Graphics/GIF/spec-gif89a.txt
- WebP RIFF container: https://developers.google.com/speed/webp/docs/riff_container
- Microsoft BITMAPFILEHEADER / BITMAPINFOHEADER: https://learn.microsoft.com/en-us/windows/win32/api/wingdi/ns-wingdi-bitmapfileheader , https://learn.microsoft.com/en-us/windows/win32/api/wingdi/ns-wingdi-bitmapinfoheader
- TIFF 6.0: https://www.itu.int/itudoc/itu-t/com16/tiff-fx/docs/tiff6.pdf ; DNG 1.7: https://helpx.adobe.com/camera-raw/digital-negative.html
- libopenraw format notes: RAF https://libopenraw.freedesktop.org/formats/raf/ , ORF https://libopenraw.freedesktop.org/formats/orf/ , RW2 https://libopenraw.freedesktop.org/formats/rw2/ , CR3 https://libopenraw.freedesktop.org/formats/cr3/
- ExifTool tag tables: PanasonicRaw https://exiftool.org/TagNames/PanasonicRaw.html , FujiFilm https://exiftool.org/TagNames/FujiFilm.html , Olympus https://exiftool.org/TagNames/Olympus.html , Pentax https://exiftool.org/TagNames/Pentax.html , Sony https://exiftool.org/TagNames/Sony.html , Nikon https://exiftool.org/TagNames/Nikon.html , Canon https://exiftool.org/TagNames/Canon.html
- RW2 header bytes (`49 49 55 00 18 00 00 00`): https://pronom.nationalarchives.gov.uk/fmt/662
- Canon CR3 reverse engineering (lclevy): https://github.com/lclevy/canon_cr3 (readme.md: CNCV strings, CTBO/THMB/PRVW layouts); Canon CR2 header and IFD3 (lclevy): http://lclevy.free.fr/cr2/
- ISO/IEC 14496-12 (ISO BMFF: box, ftyp, meta, iloc) and ISO/IEC 23008-12 (HEIF) - public draft text of 14496-12 is at https://mpeg.chiariglione.org/standards/mpeg-4/iso-base-media-file-format ; Library of Congress HEIF description https://www.loc.gov/preservation/digital/formats/fdd/fdd000525.shtml
- AVIF specification: https://aomediacodec.github.io/av1-avif/
- OpenDML AVI / Motion-JPEG DHT convention: https://learn.microsoft.com/en-us/windows/win32/directshow/mjpeg-decoder (and OpenDML AVI File Format Extensions, section "JPEG DHT")
- Crates (versions re-checked against the crates.io API on 2026-10-12, all `max_stable_version` values in section 6 confirmed): https://docs.rs/zune-jpeg , https://github.com/etemesi254/zune-image/tree/dev/crates/zune-jpeg (decoder.rs / mcu.rs for strict vs lenient EOF behaviour and the `AVI1` MJPEG path), https://github.com/image-rs/jpeg-decoder (decoder.rs, huffman.rs `fill_default_mjpeg_tables`), https://github.com/image-rs/image/blob/main/src/codecs/jpeg/decoder.rs (`SpecCompliance` -> `set_strict_mode`), https://docs.rs/png/latest/png/struct.DecodeOptions.html , https://docs.rs/kamadak-exif/latest/exif/struct.Reader.html (`continue_on_error`, `Error::PartialResult`, `distill_partial_result`), https://docs.rs/tiff/latest/tiff/decoder/struct.Decoder.html (`read_chunk`, `strip_count`, `tile_count`), https://docs.rs/image , https://docs.rs/miniz_oxide , https://docs.rs/flate2 . The "verified" truncation behaviours in section 6 come from a throwaway cargo project (zune-jpeg 0.5.15, jpeg-decoder 0.3.2, image 0.25.10, png 0.18.1, zune-png 0.5.2, tiff 0.11.3, image-webp 0.2.4, gif 0.14.2) run on Pillow 12.1.1 fixtures truncated to 60 %.
- ExifTool module sources used for RAW/MakerNote/trailer facts: https://raw.githubusercontent.com/exiftool/exiftool/master/lib/Image/ExifTool/{MakerNotes,Nikon,Olympus,Pentax,Sony,Samsung,PanasonicRaw,FujiFilm,Exif,MPF}.pm
- dcraw `panasonic_load_raw` / `pana_bits` (RW2 size estimate): https://raw.githubusercontent.com/ncruces/dcraw/master/dcraw.c
- Garfinkel 2007 fragmentation statistics (Digital Investigation 4S, S2-S12, Tables 1 and 3): https://simson.net/clips/academic/2007.DFRWS.pdf , https://doi.org/10.1016/j.diin.2007.06.017
- Pillow JPEG save options (`restart_marker_blocks` / `restart_marker_rows`, added 10.2.0): https://pillow.readthedocs.io/en/stable/handbook/image-file-formats.html#jpeg-saving
- ffmpeg encoder/muxer docs and Changelog (no mjpeg DRI option; no HEIF muxer): https://ffmpeg.org/ffmpeg-codecs.html#mjpeg , https://ffmpeg.org/ffmpeg-formats.html#avif , https://raw.githubusercontent.com/FFmpeg/FFmpeg/master/Changelog
- ddrescue manual, mapfile status characters: https://www.gnu.org/software/ddrescue/manual/ddrescue_manual.html#Mapfile-structure
