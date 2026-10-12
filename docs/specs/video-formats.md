# Video container formats: carving, validation and repair (moov rebuild)

Status: research spec, written 2026-10-12. Lines tagged `UNVERIFIED` were not confirmed
against a primary source and must be checked before an implementer relies on them.
Everything else was checked against the cited specs/sources, or against bytes produced by
the dev box's ffmpeg 8.0.1 (libx264 core 165, libx265, native aac) and parsed with an
independent Python reference parser (see section 5.6).

---

## 1 Overview and scope

DiskRescue's video module has three jobs, in increasing difficulty:

1. **Carve**: given a raw image (or a filesystem-recovered file of unknown length), find
   the start and the *exact* end of every video file so that the carved output is
   byte-identical to the original when the media is intact.
2. **Validate**: decide whether a carved/recovered file is playable, partially playable, or
   needs repair, and report precisely what is damaged (missing `moov`, truncated `mdat`,
   wrong sizes, unreadable sectors inside media data).
3. **Repair**: make the file playable again. For ISO BMFF this means rebuilding the `moov`
   index from the raw `mdat` payload using a *reference file* recorded by the same device
   (the untrunc / recoverMP4 approach). For AVI it means rebuilding `idx1` and fixing the
   RIFF/avih counters. Matroska, MPEG-TS and MPEG-PS are self-describing streams; "repair"
   is cutting at the last good unit and (for MKV) fixing unknown-size elements.

Formats in scope and what we do with them:

| Family | Extensions | Carve | Validate | Repair |
|---|---|---|---|---|
| ISO BMFF | .mp4 .m4v .mov .3gp .3g2 .heic .heif .avif .crx (CR3) | yes | yes | moov rebuild, table clipping, size fixing |
| AVI (RIFF) | .avi | yes | yes | idx1 rebuild, header counters |
| Matroska/WebM | .mkv .webm .mka | yes | yes | cut at last good cluster, write sizes |
| MPEG-TS / M2TS | .ts .m2ts .mts (AVCHD) | yes | yes | cut to last good packet |
| MPEG-PS / VOB | .mpg .vob .mpeg | yes | yes | cut to last good pack |

Out of scope here: fragmented MP4 repair (noted only), codec-level error concealment,
re-encoding. The tool never decodes video to repair it; it only needs to *parse* enough of
the H.264/HEVC/AAC bitstreams to find sample boundaries and keyframes.

Terminology used throughout: **sample** = one access unit (one video frame or one AAC frame
or one run of PCM frames); **chunk** = a run of consecutive samples of one track stored
contiguously in `mdat`; all ISO BMFF integers are big-endian; all RIFF/AVI integers are
little-endian; EBML integers are big-endian; "4CC" is a 4-byte ASCII tag.

---

## 2 Exact on-disk details

### 2.1 ISO BMFF box header (ISO/IEC 14496-12 §4.2)

| Offset | Size | Field | Notes |
|---|---|---|---|
| 0 | 4 | `size` | u32 BE. Total box size incl. header. `1` = largesize follows. `0` = box extends to end of file (only legal for the last top-level box, in practice only `mdat`). |
| 4 | 4 | `type` | 4CC. `uuid` = extended type follows. |
| 8 | 8 | `largesize` | u64 BE, present only when `size == 1`. |
| 8 or 16 | 16 | `usertype` | present only when `type == 'uuid'`. |

Header length = 8, +8 if `size==1`, +16 if `type=='uuid'`. A **FullBox** adds
`version` (u8) and `flags` (u24) immediately after the header; "payload offset" below is
relative to the end of the FullBox header unless stated.

Minimum valid `size` is 8 (16 for `uuid`); any smaller non-zero size is corruption.

### 2.2 Top-level boxes and brands

| Box | Role | Notes for carving/repair |
|---|---|---|
| `ftyp` | file type | must be first. Payload: `major_brand` 4CC, `minor_version` u32, `compatible_brands[]` 4CC until box end. |
| `free`, `skip` | padding | content ignored; camera firmware often reserves a large `free` after `ftyp` for a `moov` that is written at the end of recording. |
| `wide` | QuickTime | 8-byte placeholder preceding `mdat` so that `mdat`'s 32-bit size can later be widened to 64-bit in place (`wide`+`mdat` header → `mdat` with largesize). Seen in ffmpeg MOV output. |
| `mdat` | media data | may have `size==0` (to EOF) or `size==1` (64-bit). Content is opaque; **located only via `stco/co64`**. |
| `moov` | movie index | may precede or follow `mdat`. |
| `moof` | fragment | fragmented MP4 (CMAF, DASH, some dashcams, GoPro "LRV" is not fragmented). Contains `mfhd`, `traf`(`tfhd`,`tfdt`,`trun`). Each `moof` is followed by its `mdat`; `mvex` in `moov` announces it. **Note only**: carving = walk `moof`/`mdat` pairs; repair = cut after the last complete pair; fragmented files are inherently crash-tolerant. |
| `meta` | metadata | FullBox (version/flags) then `hdlr`, `pitm`, `iloc`, `iinf`, `iprp`… In HEIF the image data is addressed by `iloc`, not `stco`. |
| `mfra`, `sidx`, `styp`, `uuid`, `udta` | misc | tolerate; do not require. |

Brands actually seen (major or compatible) – match exact 4 bytes including spaces:

| 4CC | Hex | Origin |
|---|---|---|
| `isom` | 69 73 6F 6D | generic ISO |
| `iso2`…`iso6` | 69 73 6F 32.. | generic ISO |
| `mp41`, `mp42` | 6D 70 34 31 / 32 | MP4 v1/v2 (phones, ffmpeg) |
| `qt  ` | 71 74 20 20 | QuickTime (Apple, Canon, GoPro, DJI MOV) |
| `3gp4`…`3gp7`, `3g2a` | 33 67 70 34.. / 33 67 32 61 | 3GPP / 3GPP2 |
| `M4V ` | 4D 34 56 20 | Apple video |
| `M4A ` | 4D 34 41 20 | Apple audio |
| `avc1` | 61 76 63 31 | AVC file format |
| `heic`, `heix`, `hevc`, `mif1`, `msf1` | 68 65 69 63 … | HEIF still / sequence |
| `avif`, `avis` | 61 76 69 66 | AVIF |
| `crx ` | 63 72 78 20 | Canon CR3 raw (ISO BMFF container) |
| `dash`, `iso5`+`cmfc` | | fragmented |

Carving magic: bytes 4..8 == `ftyp` with bytes 0..4 == a plausible u32 size (16 ≤ size ≤ 256,
multiple of 4). Secondary magic for files whose head is lost: `moov`, `mdat`, `wide`,
`free` at offset 4 of a sector-aligned position; these only start *fragments*, not files.

### 2.3 `moov` tree

```
moov
 ├─ mvhd
 ├─ trak (one per track)
 │   ├─ tkhd
 │   ├─ edts / elst            (optional)
 │   ├─ tref                   (optional: tmcd ↔ video links)
 │   └─ mdia
 │       ├─ mdhd
 │       ├─ hdlr               (handler_type: vide / soun / meta / tmcd / text / hint)
 │       └─ minf
 │           ├─ vmhd | smhd | nmhd | gmhd (QT)
 │           ├─ hdlr           (QuickTime-only data handler, 'dhlr'/'alis'; ignore)
 │           ├─ dinf / dref / url
 │           └─ stbl
 │               ├─ stsd       (sample descriptions)
 │               ├─ stts       (decode deltas)
 │               ├─ ctts       (composition offsets, optional)
 │               ├─ stss       (sync samples, optional: absent ⇒ all sync)
 │               ├─ stsc       (sample→chunk)
 │               ├─ stsz | stz2
 │               ├─ stco | co64
 │               ├─ sdtp, sgpd, sbgp, stps, cslg   (optional)
 │               └─ stsh, stdp                     (rare)
 ├─ mvex                        (fragmented only)
 ├─ udta, meta                  (optional)
 └─ (Canon) uuid 85c0b687-820f-11e0-8111-f4ce462b6a48 with CNTH/CNDA thumbnail JPEG    UNVERIFIED (uuid value)
```

All multi-byte fields BE. "v" = `version` of the FullBox.

#### mvhd (FullBox)

| Offset (after v/flags) | v0 size | v1 size | Field |
|---|---|---|---|
| 0 | 4 | 8 | creation_time (seconds since 1904-01-01 UTC) |
| 4/8 | 4 | 8 | modification_time |
| 8/16 | 4 | 4 | timescale (ticks/second, typically 1000, 600 (QT), 90000) |
| 12/20 | 4 | 8 | duration (in timescale; 0xFFFFFFFF/all-ones = unknown) |
| 16/28 | 4 | 4 | rate, 16.16 fixed, 0x00010000 |
| 20/32 | 2 | 2 | volume, 8.8 fixed, 0x0100 |
| 22/34 | 2 | 2 | reserved 0 |
| 24/36 | 8 | 8 | reserved 0 |
| 32/44 | 36 | 36 | matrix: 9 × 32-bit, identity = 00010000 0 0 0 00010000 0 0 0 40000000 |
| 68/80 | 24 | 24 | pre_defined 0 |
| 92/104 | 4 | 4 | next_track_ID (must exceed every tkhd.track_ID; 0xFFFFFFFF ⇒ search) |

Box size: v0 = 108, v1 = 120.

#### tkhd (FullBox). flags: 0x1 enabled, 0x2 in_movie, 0x4 in_preview, 0x8 size_is_aspect_ratio

| v0 off | v0 | v1 | Field |
|---|---|---|---|
| 0 | 4 | 8 | creation_time |
| 4 | 4 | 8 | modification_time |
| 8 | 4 | 4 | track_ID (1-based, non-zero) |
| 12 | 4 | 4 | reserved 0 |
| 16 | 4 | 8 | duration (in **movie** timescale, includes edit list) |
| 20 | 8 | 8 | reserved 0 |
| 28 | 2 | 2 | layer (i16, 0) |
| 30 | 2 | 2 | alternate_group (i16) |
| 32 | 2 | 2 | volume 8.8 (0x0100 audio, 0 video) |
| 34 | 2 | 2 | reserved 0 |
| 36 | 36 | 36 | matrix (rotation lives here: 90° = 0 00010000 0 FFFF0000 0 0 <h> 0 40000000) |
| 72 | 4 | 4 | width 16.16 (e.g. 320 → 0x01400000) |
| 76 | 4 | 4 | height 16.16 |

Box size: v0 = 92, v1 = 104. Width/height are 0 for audio.

#### elst (FullBox) in `edts`

| Field | v0 | v1 |
|---|---|---|
| entry_count | u32 | u32 |
| per entry: segment_duration | u32 (movie timescale) | u64 |
| media_time | i32 (media timescale; -1 = empty edit) | i64 |
| media_rate_integer | i16 (1) | i16 |
| media_rate_fraction | i16 (0) | i16 |

Typical phone/ffmpeg file: one entry, `media_time` = AAC priming (1024 samples) on audio
and = first `ctts` offset (reorder delay) on video. Observed ffmpeg: video `media_time=1024`
at timescale 15360 (two 512-tick frames), audio `media_time=1024` at 48000.

#### mdhd (FullBox)

| v0 off | v0 | v1 | Field |
|---|---|---|---|
| 0 | 4 | 8 | creation_time |
| 4 | 4 | 8 | modification_time |
| 8 | 4 | 4 | timescale (video: often fps×512 e.g. 15360, or 30000/600/90000; audio: sample rate) |
| 12 | 4 | 8 | duration (media timescale) |
| 16 | 2 | 2 | language: bit15 pad=0, three 5-bit letters, each `char - 0x60`. `und` = 0x55C4; QuickTime files use 0x7FFF (old Mac language code) |
| 18 | 2 | 2 | pre_defined / quality 0 |

Box size: v0 = 32, v1 = 44.

#### hdlr (FullBox)

| Off | Size | Field |
|---|---|---|
| 0 | 4 | pre_defined (ISO: 0; QuickTime: component type `mhlr`) |
| 4 | 4 | handler_type: `vide`, `soun`, `meta`, `tmcd`, `text`, `sbtl`, `hint`, `auxv` |
| 8 | 12 | reserved (QT: manufacturer, flags, flags mask) |
| 20 | n | name: ISO = UTF-8 NUL-terminated; QuickTime = Pascal string (length byte first). Handle both: if byte[20] == len(rest)-1 treat as Pascal. |

#### vmhd (FullBox, flags=1): graphicsmode u16 (0), opcolor u16[3]. Box size 20.
#### smhd (FullBox): balance 8.8 i16 (0), reserved u16. Box size 16.
#### dinf/dref: dref FullBox: entry_count u32, then entries, normally one `url ` FullBox with flags=1 (self-contained, no string). dref size 28.

#### stsd (FullBox): entry_count u32, then `entry_count` sample entries. Each sample entry is a box:

SampleEntry common prefix (after the 8-byte box header):

| Off | Size | Field |
|---|---|---|
| 0 | 6 | reserved 0 |
| 6 | 2 | data_reference_index (1) |

**VisualSampleEntry** (`avc1`, `avc3`, `hvc1`, `hev1`, `mp4v`, `apcn`/`apch` ProRes, `jpeg`, `mjpa`, `dvh1`, `av01`): after the 8-byte prefix:

| Off | Size | ISO name | QuickTime name |
|---|---|---|---|
| 8 | 2 | pre_defined 0 | version |
| 10 | 2 | reserved 0 | revision level |
| 12 | 4 | pre_defined 0 | vendor (`FFMP`, `appl`, `GoPr`) |
| 16 | 4 | pre_defined 0 | temporal quality |
| 20 | 4 | pre_defined 0 | spatial quality |
| 24 | 2 | width | width |
| 26 | 2 | height | height |
| 28 | 4 | horizresolution 16.16 (0x00480000 = 72 dpi) | |
| 32 | 4 | vertresolution 16.16 | |
| 36 | 4 | reserved 0 | data size |
| 40 | 2 | frame_count (1) | |
| 42 | 32 | compressorname: Pascal string in 32-byte field (len byte + ≤31 chars) | |
| 74 | 2 | depth 0x0018 | |
| 76 | 2 | pre_defined -1 (0xFFFF) | colour table id |
| 78 | … | child boxes: `avcC`, `hvcC`, `esds`(mp4v), `pasp`, `colr`, `btrt`, `fiel`, `clap`, `gama`, `chrm` | |

Total fixed part = 8 (box hdr) + 78 = 86 bytes before children.

**AudioSampleEntry** (`mp4a`, `sowt`, `twos`, `lpcm`, `in24`, `fl32`, `ac-3`, `ec-3`, `alac`, `Opus`, `fLaC`, `samr`): after the 8-byte prefix:

| Off | Size | ISO | QuickTime |
|---|---|---|---|
| 8 | 2 | reserved 0 | **version** (0, 1 or 2) |
| 10 | 2 | reserved 0 | revision |
| 12 | 4 | reserved 0 | vendor |
| 16 | 2 | channelcount (v2 QT: 3) | |
| 18 | 2 | samplesize (16) | |
| 20 | 2 | pre_defined 0 | compression_id (0; v2: -2 = 0xFFFE) |
| 22 | 2 | reserved 0 | packet_size |
| 24 | 4 | samplerate 16.16 (48000 → 0xBB800000); v2 QT: 0x00010000 | |
| 28 | | children (`esds`, `chan`, `btrt`, `wave`(QT), `dOps`, `dfLa`) when version==0 | |

QuickTime **version 1** adds at offset 28 (16 bytes): samples_per_packet u32,
bytes_per_packet u32, bytes_per_frame u32, bytes_per_sample u32; children start at 44.
QuickTime **version 2** adds at offset 28 (36 bytes): sizeOfStructOnly u32 (=72),
audioSampleRate f64 BE, numAudioChannels u32, always7F000000 u32, constBitsPerChannel u32,
formatSpecificFlags u32, constBytesPerAudioPacket u32, constLPCMFramesPerAudioPacket u32;
children start at 64. UNVERIFIED (v2 field order taken from QuickTime File Format spec from memory)
`formatSpecificFlags` (LPCM): 0x1 float, 0x2 big-endian, 0x4 signed int, 0x8 packed,
0x10 aligned high, 0x20 non-interleaved. UNVERIFIED

Observed ffmpeg MOV `sowt` entry (36 bytes + `chan`): channelcount 1, samplesize 16,
samplerate 0xBB800000, then `chan` box (24 bytes: v/flags, layout tag u32, bitmap u32,
number of descriptions u32).

**avcC** (ISO/IEC 14496-15 §5.3.3.1, `AVCDecoderConfigurationRecord`), inside `avc1`/`avc3`:

| Off | Size | Field |
|---|---|---|
| 0 | 1 | configurationVersion = 1 |
| 1 | 1 | AVCProfileIndication (66 Baseline, 77 Main, 100 High, 110 High10, 122 High422, 244 High444) |
| 2 | 1 | profile_compatibility (constraint_set flags) |
| 3 | 1 | AVCLevelIndication (e.g. 13, 31, 41, 51) |
| 4 | 1 | `111111` + lengthSizeMinusOne (2 bits). **NAL length prefix = this+1 bytes; in practice always 3 ⇒ 4-byte lengths.** |
| 5 | 1 | `111` + numOfSequenceParameterSets (5 bits) |
| 6 | var | for each SPS: sequenceParameterSetLength u16, then the SPS NAL (starts with 0x67 / 0x27 etc.) |
| | 1 | numOfPictureParameterSets |
| | var | for each PPS: pictureParameterSetLength u16, PPS NAL (0x68) |
| | | if profile ∈ {100,110,122,144}: `111111`+chroma_format(2), `11111`+bit_depth_luma_minus8(3), `11111`+bit_depth_chroma_minus8(3), numOfSequenceParameterSetExt u8, SPS-ext NALs |

Observed: `01 4d 40 0d ff e1 00 18 67 4d 40 0d …` = v1, Main, compat 0x40, level 1.3,
lengthSize 4, 1 SPS of 24 bytes. `avc3`/`hev1` mean parameter sets may also be in-band
(inside samples) — the rebuild must then accept SPS/PPS NALs inside `mdat`.

**hvcC** (ISO/IEC 14496-15 §8.3.3.1, `HEVCDecoderConfigurationRecord`), inside `hvc1`/`hev1`:

| Off | Size | Field |
|---|---|---|
| 0 | 1 | configurationVersion = 1 |
| 1 | 1 | general_profile_space(2) general_tier_flag(1) general_profile_idc(5) (1 Main, 2 Main10, 3 MainStill) |
| 2 | 4 | general_profile_compatibility_flags (32 bits) |
| 6 | 6 | general_constraint_indicator_flags (48 bits) |
| 12 | 1 | general_level_idc (e.g. 60 = 2.0, 120 = 4.0, 153 = 5.1) |
| 13 | 2 | `1111` + min_spatial_segmentation_idc (12) |
| 15 | 1 | `111111` + parallelismType (2) |
| 16 | 1 | `111111` + chromaFormat (2) |
| 17 | 1 | `11111` + bitDepthLumaMinus8 (3) |
| 18 | 1 | `11111` + bitDepthChromaMinus8 (3) |
| 19 | 2 | avgFrameRate |
| 21 | 1 | constantFrameRate(2) numTemporalLayers(3) temporalIdNested(1) **lengthSizeMinusOne(2)** |
| 22 | 1 | numOfArrays |
| 23 | var | per array: array_completeness(1) reserved(1) NAL_unit_type(6); numNalus u16; per NALU: nalUnitLength u16, NAL bytes |

Arrays normally in order VPS(32), SPS(33), PPS(34), optionally SEI(39).
Observed header: `01 01 60 00 00 00 90 00 00 00 00 00 3c f0 00 fc fd f8 f8 00 00 0f 04`.

**esds** (FullBox; ISO/IEC 14496-1 §7.2.6 descriptors) inside `mp4a` (and `mp4v`):

Each descriptor = tag u8 + "expandable" size: 1–4 bytes, each byte contributes 7 bits, MSB
set means another byte follows (ffmpeg always writes 4 bytes: `80 80 80 xx`).

```
ES_Descriptor        tag 0x03: ES_ID u16, flags u8 (bit7 streamDependence, bit6 URL, bit5 OCR, low 5 priority)
                              [dependsOn_ES_ID u16] [URL len u8 + string] [OCR_ES_ID u16]
  DecoderConfigDescriptor tag 0x04: objectTypeIndication u8, streamType(6)|upStream(1)|reserved(1) u8,
                              bufferSizeDB u24, maxBitrate u32, avgBitrate u32
    DecoderSpecificInfo   tag 0x05: AudioSpecificConfig (below)
  SLConfigDescriptor      tag 0x06: predefined u8 = 0x02
```

objectTypeIndication: 0x40 MPEG-4 Audio (AAC), 0x66/0x67/0x68 MPEG-2 AAC, 0x69 MPEG-2
audio, 0x6B MPEG-1 audio (MP3), 0x20 MPEG-4 Visual, 0x21 H.264, 0x23 HEVC. streamType
byte 0x15 = audio (0x05<<2 | reserved 1).

**AudioSpecificConfig** (ISO/IEC 14496-3 §1.6.2.1), bit-packed:

| Bits | Field |
|---|---|
| 5 | audioObjectType (2 = AAC LC, 5 = SBR, 29 = PS, 23 = LD, 39 = ELD; 31 ⇒ 6 more bits + 32) |
| 4 | samplingFrequencyIndex (0:96000 1:88200 2:64000 3:48000 4:44100 5:32000 6:24000 7:22050 8:16000 9:12000 10:11025 11:8000 12:7350 15: explicit 24-bit follows) |
| 4 | channelConfiguration (1 mono, 2 stereo, 3–7 multichannel, 0 = PCE in stream) |
| GASpecificConfig: 1 | frameLengthFlag (0 ⇒ 1024 samples/frame, 1 ⇒ 960) |
| 1 | dependsOnCoreCoder (+14 bits coreCoderDelay if 1) |
| 1 | extensionFlag |
| opt 11 | syncExtensionType 0x2B7 ⇒ 5 bits extensionAudioObjectType (5 = SBR), 1 bit sbrPresentFlag, [4 bits extSamplingFrequencyIndex] |

Observed ffmpeg: `11 88 56 e5 00` = AOT 2, 48 kHz, mono, 1024, then sync ext 0x2B7, SBR absent.

#### stts (FullBox): entry_count u32; entries { sample_count u32, sample_delta u32 }.
Decode time of sample n = Σ deltas of preceding samples. Video from cameras: one entry
(constant delta, e.g. 512 @ 15360, 1001 @ 30000, 100 @ 2997). Audio AAC: delta 1024
(last frame may be shorter: observed `[141×1024, 1×640]`). PCM in MOV: delta 1 per PCM
frame, one entry.

#### ctts (FullBox): entry_count u32; entries { sample_count u32, sample_offset u32 (v0) / i32 (v1) }.
CT(n) = DT(n) + offset. Version 1 allows negative offsets (then `elst` is not needed to
remove the delay). Only present when B-frames / reordering exist.

#### stsc (FullBox): entry_count u32; entries { first_chunk u32 (1-based), samples_per_chunk u32, sample_description_index u32 (1-based) }.
Run-length: entry i applies to chunks first_chunk[i] .. first_chunk[i+1]-1 (last entry to
the end). Entries must have strictly increasing first_chunk; first entry has first_chunk=1.

#### stsz (FullBox): sample_size u32 (if ≠0 all samples this size and no table), sample_count u32, then sample_count × entry_size u32 when sample_size==0.
#### stz2 (FullBox): reserved u24, field_size u8 ∈ {4,8,16}, sample_count u32, packed sizes (4-bit: high nibble first).
#### stco (FullBox): entry_count u32; chunk_offset u32[] (absolute file offsets to the first byte of each chunk).
#### co64 (FullBox): entry_count u32; chunk_offset u64[].
#### stss (FullBox): entry_count u32; sample_number u32[] (1-based, ascending). Absent ⇒ every sample is a sync sample.
#### stps (QuickTime): same layout as stss, "partial sync" (open-GOP). Drop on rebuild.
#### sdtp (FullBox): one byte per sample (count from stsz): is_leading(2) sample_depends_on(2) sample_is_depended_on(2) sample_has_redundancy(2). Observed x265: 0x20 for IDR, 0x10 P, 0x18 non-ref B. Optional; drop on rebuild (or regenerate: 0x20 for sync, 0x10 otherwise).
#### sgpd / sbgp (note only): grouping_type `roll` with roll_distance i16 (-1) for AAC pre-roll; `rap `, `sync`. sgpd v1 has default_length u32. Drop on rebuild.
#### cslg (composition to decode shift): drop on rebuild.

### 2.4 Sample N → file offset (the arithmetic)

Given one track's `stsc`, `stco/co64`, `stsz/stz2` (sample indices 0-based here, chunk
indices 0-based; stsc stores them 1-based):

```
expand_stsc(stsc, chunk_count) -> samples_per_chunk[c] for c in 0..chunk_count:
    for i in 0..len(stsc):
        first = stsc[i].first_chunk - 1
        last  = (i+1 < len(stsc)) ? stsc[i+1].first_chunk - 2 : chunk_count - 1
        for c in first..=last: samples_per_chunk[c] = stsc[i].samples_per_chunk

build_sample_map(track):
    chunk_count = len(stco)
    spc = expand_stsc(stsc, chunk_count)
    n = 0
    for c in 0..chunk_count:
        off = stco[c]
        for k in 0..spc[c]:
            size = stsz.sample_size != 0 ? stsz.sample_size : stsz.entry[n]
            sample[n] = { offset: off, size, chunk: c, index_in_chunk: k }
            off += size
            n += 1
    assert n == stsz.sample_count   // else tables are inconsistent (clip to min)
```

Decode timestamps: `dts[n] = Σ_{m<n} delta(m)` from the expanded `stts`;
`cts[n] = dts[n] + ctts_offset(n)`; presentation after edit list:
`pts = cts - elst.media_time` (single-entry list, rate 1). Durations:
`mdhd.duration = Σ all stts deltas`; `tkhd.duration = mvhd.duration = round(mdhd.duration ×
mvhd.timescale / mdhd.timescale)` (or Σ elst segment_durations when an edit list exists).

**Interleaving in `mdat`.** Writers emit chunks in increasing offset order, alternating
tracks. Observed on the dev box (ffmpeg default `-min_frag`-less MP4, 30 fps + AAC 48 kHz):
video chunk 1 holds 2 samples then 1 sample per chunk; audio chunks hold 1–5 AAC frames
(stsc with 78 entries for 142 frames), i.e. chunking is driven by DTS interleave
(`max_interleave_delta`), not by a fixed count. Phones/cameras typically write **≈0.5–1 s
of video (15–30 frames) then ≈0.5–1 s of audio (23–47 AAC frames)** per chunk pair; GoPro
writes 1 s. QuickTime MOV with PCM audio: audio chunks of e.g. 0x800/0x400 PCM frames
(observed stsc: 2048, 1024, …). Chunk boundaries are **not** sector-aligned and samples
are **not** padded; a chunk starts immediately after the previous chunk, except when the
writer inserted `free`/`skip` padding inside `mdat` (seen from some Canon/Sony firmware).

### 2.5 H.264 / AVC bitstream (ITU-T H.264 §7.3, §7.4)

In MP4 each sample = one access unit = sequence of **length-prefixed NAL units**
(prefix = `lengthSizeMinusOne+1` bytes, BE, length of the NAL *excluding* the prefix).
No start codes (00 00 01) inside MP4 samples; if you see them, the data is Annex B
(from a .ts/.264 file), not MP4.

NAL header (1 byte): `forbidden_zero_bit(1) nal_ref_idc(2) nal_unit_type(5)`.

| nal_unit_type | Meaning | Rebuild action |
|---|---|---|
| 1 | non-IDR slice (VCL) | part of a picture |
| 2,3,4 | slice data partitions A/B/C | VCL (rare, Extended profile) |
| 5 | IDR slice | VCL, **keyframe** |
| 6 | SEI | prefix of an AU (x264 writes user-data SEI before the first IDR) |
| 7 | SPS | AU prefix (in `avc3` streams) |
| 8 | PPS | AU prefix |
| 9 | AUD | AU delimiter: always starts an AU |
| 10, 11 | end of sequence / stream | |
| 12 | filler data | may be inside AU |
| 14, 15, 20 | SVC/MVC prefix, subset SPS, extension slice | treat as non-VCL prefix |
| 0, 16–18, 21–31 | reserved/unspecified | implausible: reject |

A **picture's first slice** is recognised by `first_mb_in_slice == 0` (H.264 §7.4.1.2.4 also
lists frame_num/pps_id/field/nal_ref_idc/POC/idr_pic_id differences; untrunc uses those
and *ignores* first_mb; we use both: `first_mb_in_slice == 0` is the primary signal, the
7.4.1.2.4 comparisons are the fallback when slices are reordered or lost).

**Exp-Golomb** (`ue(v)`, `se(v)`):

```
read_ue(br):
    zeros = 0
    while br.read_bit() == 0: zeros += 1; if zeros > 31: error
    return (1 << zeros) - 1 + br.read_bits(zeros)
read_se(br):
    k = read_ue(br)
    return (k & 1) ? (k + 1) / 2 : -(k / 2)
```

**Emulation prevention**: before parsing any RBSP (SPS, PPS, slice header), convert the
NAL payload to RBSP by deleting every `0x03` that follows `0x00 0x00` (pattern
`00 00 03` → `00 00`). Only the header bytes need conversion (first ~64 bytes of a slice).

**SPS parsing order** (§7.3.2.1.1) – every field up to `frame_mbs_only_flag` must be
parsed; scaling lists must be *skipped correctly*, not ignored:

```
profile_idc           u(8)
constraint_set flags  u(8)      // constraint_set0..5 + 2 reserved
level_idc             u(8)
seq_parameter_set_id  ue        // 0..31
chroma_format_idc = 1; separate_colour_plane_flag = 0
if profile_idc in {100,110,122,244,44,83,86,118,128,138,139,134,135}:
    chroma_format_idc ue
    if chroma_format_idc == 3: separate_colour_plane_flag u(1)
    bit_depth_luma_minus8 ue; bit_depth_chroma_minus8 ue
    qpprime_y_zero_transform_bypass_flag u(1)
    seq_scaling_matrix_present_flag u(1)
    if set: for i in 0..(chroma_format_idc != 3 ? 8 : 12):
        seq_scaling_list_present_flag[i] u(1)
        if set: scaling_list(i < 6 ? 16 : 64)
log2_max_frame_num_minus4        ue   // → frame_num width = +4 bits
pic_order_cnt_type               ue   // 0, 1 or 2
if == 0: log2_max_pic_order_cnt_lsb_minus4 ue      // → poc_lsb width = +4 bits
elif == 1:
    delta_pic_order_always_zero_flag u(1)
    offset_for_non_ref_pic se; offset_for_top_to_bottom_field se
    num_ref_frames_in_pic_order_cnt_cycle ue; for each: offset_for_ref_frame se
max_num_ref_frames               ue
gaps_in_frame_num_value_allowed_flag u(1)
pic_width_in_mbs_minus1          ue   // width  = (v+1)*16
pic_height_in_map_units_minus1   ue   // height = (2 - frame_mbs_only_flag)*(v+1)*16
frame_mbs_only_flag              u(1)
if == 0: mb_adaptive_frame_field_flag u(1)
direct_8x8_inference_flag        u(1)
frame_cropping_flag              u(1); if set: 4 × ue (left,right,top,bottom crop)
vui_parameters_present_flag      u(1)  // VUI follows; NOT needed → stop here
```

`scaling_list(size)`: `lastScale = nextScale = 8; for j in 0..size: if nextScale != 0:
delta_scale = se; nextScale = (lastScale + delta_scale + 256) % 256; lastScale =
(nextScale == 0) ? lastScale : nextScale`.

Fields the slice-header parser needs from SPS: `log2_max_frame_num_minus4`,
`pic_order_cnt_type`, `log2_max_pic_order_cnt_lsb_minus4`, `delta_pic_order_always_zero_flag`,
`frame_mbs_only_flag`, `separate_colour_plane_flag`. Also keep width/height for the
"reference matches damaged file" check.

**PPS parsing order** (§7.3.2.2):

```
pic_parameter_set_id ue; seq_parameter_set_id ue
entropy_coding_mode_flag u(1)                      // CAVLC 0 / CABAC 1
bottom_field_pic_order_in_frame_present_flag u(1)  // needed by slice header
num_slice_groups_minus1 ue
if > 0: slice_group_map_type ue; (map-type-dependent fields; map types 0,2,6 have loops) — Baseline-only, rare
num_ref_idx_l0_default_active_minus1 ue; num_ref_idx_l1_default_active_minus1 ue
weighted_pred_flag u(1); weighted_bipred_idc u(2)
pic_init_qp_minus26 se; pic_init_qs_minus26 se; chroma_qp_index_offset se
deblocking_filter_control_present_flag u(1); constrained_intra_pred_flag u(1)
redundant_pic_cnt_present_flag u(1)                // needed by slice header
(optional: transform_8x8_mode_flag, pic_scaling_matrix_present_flag, second_chroma_qp_index_offset)
```

**Slice header up to POC** (§7.3.3): parse after the 1-byte NAL header, on RBSP bytes.

```
first_mb_in_slice     ue           // 0 ⇒ first slice of a picture
slice_type            ue           // 0..9 (P,B,I,SP,SI; +5 = "all slices same type"); >9 ⇒ not a slice
pic_parameter_set_id  ue           // must exist in avcC
if separate_colour_plane_flag: colour_plane_id u(2)
frame_num             u(log2_max_frame_num_minus4 + 4)
field_pic_flag = 0; bottom_field_flag = 0
if !frame_mbs_only_flag: field_pic_flag u(1); if set: bottom_field_flag u(1)
if nal_unit_type == 5: idr_pic_id ue
if pic_order_cnt_type == 0:
    pic_order_cnt_lsb u(log2_max_pic_order_cnt_lsb_minus4 + 4)
    if bottom_field_pic_order_in_frame_present_flag && !field_pic_flag: delta_pic_order_cnt_bottom se
if pic_order_cnt_type == 1 && !delta_pic_order_always_zero_flag:
    delta_pic_order_cnt[0] se
    if bottom_field_pic_order_in_frame_present_flag && !field_pic_flag: delta_pic_order_cnt[1] se
if redundant_pic_cnt_present_flag: redundant_pic_cnt ue
// stop here
```

Verified on ffmpeg output: IDR → `first_mb 0, slice_type 7, pps 0, frame_num 0, idr_pic_id 0,
poc_lsb 0`; next P → `frame_num 1, poc_lsb 6`; next B → `frame_num 2, poc_lsb 2`.

**POC (type 0) reconstruction** (§8.2.1.1), needed for composition offsets:

```
MaxLsb = 1 << (log2_max_pic_order_cnt_lsb_minus4 + 4)
on IDR: prevMsb = 0; prevLsb = 0
for each picture (decode order) with lsb:
    if lsb < prevLsb && prevLsb - lsb >= MaxLsb/2: msb = prevMsb + MaxLsb
    elif lsb > prevLsb && lsb - prevLsb > MaxLsb/2: msb = prevMsb - MaxLsb
    else: msb = prevMsb
    poc = msb + lsb
    if nal_ref_idc != 0: prevMsb = msb; prevLsb = lsb   // only reference pictures update prev
```

Type 2: output order == decode order (no B-frames) ⇒ ctts all zero. Type 1: compute via
§8.2.1.2 or fall back to zero ctts (rare in camera files). UNVERIFIED (claim that type 1 is rare in consumer camera output)

### 2.6 HEVC / H.265 bitstream (ITU-T H.265 §7.3)

NAL header (2 bytes): `forbidden_zero_bit(1) nal_unit_type(6) nuh_layer_id(6)
nuh_temporal_id_plus1(3)`; `nuh_temporal_id_plus1 != 0` (except EOB 37 may be 0 — untrunc
checks this), `nuh_layer_id` normally 0.

| Type | Name | Class |
|---|---|---|
| 0/1 | TRAIL_N / TRAIL_R | VCL |
| 2/3 | TSA_N/R, 4/5 STSA_N/R, 6/7 RADL_N/R, 8/9 RASL_N/R | VCL |
| 10–15 | reserved non-IRAP | VCL (reject) |
| 16/17/18 | BLA_W_LP / BLA_W_RADL / BLA_N_LP | VCL, IRAP, keyframe |
| 19 | IDR_W_RADL | VCL, **keyframe** |
| 20 | IDR_N_LP | VCL, **keyframe** (x265 default first frame) |
| 21 | CRA_NUT | VCL, IRAP, keyframe for `stss` (open GOP: RASL may follow) |
| 22–23 | RSV_IRAP | reject |
| 32/33/34 | VPS/SPS/PPS | non-VCL prefix |
| 35 | AUD | starts AU |
| 36/37 | EOS/EOB | |
| 38 | FD (filler) | |
| 39 | PREFIX_SEI | AU prefix |
| 40 | SUFFIX_SEI | after VCL, same AU |
| 41–47, 48–63 | reserved / unspecified | reject (>40 ⇒ not HEVC, untrunc rule) |

**profile_tier_level(1, maxNumSubLayersMinus1)** (§7.3.3) – must be skipped exactly:

```
general: profile_space u(2) tier u(1) profile_idc u(5) compat_flags u(32)
         progressive/interlaced/non_packed/frame_only u(4) + 43 reserved + 1 (inbld) = 44 bits
         level_idc u(8)                                  → 96 bits total
for i in 0..maxNumSubLayersMinus1: sub_layer_profile_present_flag[i] u(1); sub_layer_level_present_flag[i] u(1)
if maxNumSubLayersMinus1 > 0: for i in maxNumSubLayersMinus1..8: reserved_zero_2bits u(2)
for i in 0..maxNumSubLayersMinus1:
    if profile_present[i]: 88 bits (2+1+5+32+4+43+1)
    if level_present[i]:   u(8)
```

**SPS** (§7.3.2.2) up to the fields the slice parser needs:

```
sps_video_parameter_set_id u(4); sps_max_sub_layers_minus1 u(3); sps_temporal_id_nesting_flag u(1)
profile_tier_level(1, sps_max_sub_layers_minus1)
sps_seq_parameter_set_id ue
chroma_format_idc ue; if == 3: separate_colour_plane_flag u(1)
pic_width_in_luma_samples ue; pic_height_in_luma_samples ue
conformance_window_flag u(1); if set: 4 × ue
bit_depth_luma_minus8 ue; bit_depth_chroma_minus8 ue
log2_max_pic_order_cnt_lsb_minus4 ue                    // ← slice_pic_order_cnt_lsb width
sps_sub_layer_ordering_info_present_flag u(1)
for i in (flag ? 0 : max_sub_layers_minus1)..=max_sub_layers_minus1: 3 × ue
log2_min_luma_coding_block_size_minus3 ue               // ← for CtbSizeY
log2_diff_max_min_luma_coding_block_size ue             // ← for CtbSizeY
log2_min_luma_transform_block_size_minus2 ue; log2_diff_max_min_luma_transform_block_size ue
max_transform_hierarchy_depth_inter ue; max_transform_hierarchy_depth_intra ue
scaling_list_enabled_flag u(1); if set: sps_scaling_list_data_present_flag u(1); if set: scaling_list_data()
amp_enabled_flag u(1); sample_adaptive_offset_enabled_flag u(1)
pcm_enabled_flag u(1); if set: u(4) u(4) ue ue u(1)
num_short_term_ref_pic_sets ue; for each: st_ref_pic_set(i)   // complex; only needed beyond POC → stop before
// everything after this (long-term refs, temporal MVP, strong intra smoothing, VUI) is not needed
```

For the slice header we need: `separate_colour_plane_flag`, `pic_width/height_in_luma_samples`,
`log2_max_pic_order_cnt_lsb_minus4`, `CtbLog2SizeY = log2_min_luma_coding_block_size_minus3 +
3 + log2_diff_max_min_luma_coding_block_size`, giving `PicSizeInCtbsY = ceil(W/CtbSizeY) ×
ceil(H/CtbSizeY)`. Parsing stops right after `log2_diff_max_min_luma_coding_block_size`, so
scaling lists and st_ref_pic_set never need to be decoded.

**PPS** (§7.3.2.3) prefix:

```
pps_pic_parameter_set_id ue; pps_seq_parameter_set_id ue
dependent_slice_segments_enabled_flag u(1)
output_flag_present_flag u(1)
num_extra_slice_header_bits u(3)
sign_data_hiding_enabled_flag u(1); cabac_init_present_flag u(1)
// stop
```

**Slice segment header** (§7.3.6.1) up to POC:

```
first_slice_segment_in_pic_flag u(1)                    // bit 7 of the first payload byte ⇒ new picture
if 16 <= nal_unit_type <= 23: no_output_of_prior_pics_flag u(1)
slice_pic_parameter_set_id ue
dependent_slice_segment_flag = 0
if !first_slice_segment_in_pic_flag:
    if dependent_slice_segments_enabled_flag: dependent_slice_segment_flag u(1)
    slice_segment_address u(ceil(log2(PicSizeInCtbsY)))
if !dependent_slice_segment_flag:
    for i in 0..num_extra_slice_header_bits: slice_reserved_flag u(1)
    slice_type ue                                        // 0 B, 1 P, 2 I; >2 ⇒ not a slice
    if output_flag_present_flag: pic_output_flag u(1)
    if separate_colour_plane_flag: colour_plane_id u(2)
    if nal_unit_type != 19 && nal_unit_type != 20:      // IDR has no POC lsb (POC = 0)
        slice_pic_order_cnt_lsb u(log2_max_pic_order_cnt_lsb_minus4 + 4)
        // short_term_ref_pic_set_sps_flag … follow; not needed
```

Verified on x265 output: first sample = IDR_N_LP (20) `first_slice 1, slice_type 2`; next
TRAIL_R (1) `poc_lsb 4`; next TRAIL_N (0) `poc_lsb 1`.

HEVC POC: same MSB logic as H.264 type 0 with `MaxPicOrderCntLsb`; for IRAP pictures with
`NoRaslOutputFlag=1` (IDR, BLA, first CRA) `PicOrderCntMsb = 0`; `prevTid0Pic` is the
previous picture with TemporalId 0 that is not RASL/RADL/sub-layer non-reference.

### 2.7 AAC in MP4 (ISO/IEC 14496-3 §4.4.2 `raw_data_block()`)

An `mp4a` sample is one `raw_data_block` (no ADTS header, no LATM). 1024 PCM samples per
frame (960 if frameLengthFlag). Sizes: 128 kbps/48 kHz ≈ 341 B average, 256 kbps ≈ 683 B;
observed 128 kbps mono sine: 276–453 B. Practical range 20–1500 B; a frame > 2048 B at
≤ 256 kbps is implausible (hard cap from 14496-3: 6144 bits × channels).

A raw_data_block is a sequence of syntactic elements, each starting with `id_syn_ele u(3)`:

| id | Element | First bits after id |
|---|---|---|
| 0 | SCE single channel | element_instance_tag u(4), then individual_channel_stream: global_gain u(8), ics_info |
| 1 | CPE channel pair | element_instance_tag u(4), common_window u(1), if set: ics_info, ms_mask_present u(2) |
| 2 | CCE coupling | rare |
| 3 | LFE | like SCE |
| 4 | DSE data stream | tag u(4), byte_aligned u(1), count u(8) [+esc u(8)] |
| 5 | PCE program config | only when channelConfiguration==0 |
| 6 | FIL fill | count u(4), if 15: esc_count u(8) (+14); payload (often SBR data or encoder padding) |
| 7 | END | terminates the block; then byte-align |

`ics_info`: `ics_reserved_bit u(1)` (**must be 0**), `window_sequence u(2)` (0 ONLY_LONG,
1 LONG_START, 2 EIGHT_SHORT, 3 LONG_STOP), `window_shape u(1)`, then if EIGHT_SHORT:
`max_sfb u(4)`, `scale_factor_grouping u(7)`; else `max_sfb u(6)`, `predictor_data_present
u(1)`. `max_sfb` must be ≤ num_swb for the sample rate (FFmpeg `aactab.c`, indexed by
samplingFrequencyIndex 0..12): long windows (1024) `{41,41,47,49,49,51,47,47,43,43,43,40,40}`
i.e. 96/88.2 kHz: 41, 64 kHz: 47, 48/44.1 kHz: 49, 32 kHz: 51, 24/22.05 kHz: 47,
16/12/11.025 kHz: 43, 8/7.35 kHz: 40; short windows (128) `{12,12,12,14,14,14,15,15,15,15,15,15,15}`;
960-frame variant `{40,40,46,49,49,49,46,46,42,42,42,40,40}`.

Observed ffmpeg-aac mono: frame 1 = `01 62 9c d9 …` → SCE, tag 0, global_gain 177,
reserved 0, EIGHT_SHORT, max_sfb 14; frame 2 → ONLY_LONG, max_sfb 48. Frame 0 (priming)
started with a FIL element (`de 02 …`): id 6, count 15 – encoders may emit FIL first.

End-of-frame property: the block ends with END (`111`) followed by zero alignment bits
(byte_alignment() pads with 0 bits UNVERIFIED), so the last byte `b` satisfies: for some
k in 0..=5, `(b >> k) & 7 == 7 && b & ((1<<k)-1) == 0`, or `b == 0xC0`/`0x80` with the
`111` straddling the previous byte. Observed last bytes: 0xA7, 0x38. This is a *filter*
(≈ 30% false-accept on random bytes), not a proof.

### 2.8 PCM / LPCM in MOV (QuickTime)

Sample entries `sowt` (s16 LE), `twos` (s16 BE), `lpcm` (v2 entry, flags describe
layout), `in24`, `in32`, `fl32`, `fl64`, `raw ` (u8), `NONE`. One **sample** = one PCM
frame (all channels at one instant); `stsz.sample_size` = bytes per frame (channels ×
bytes/sample), `stts` delta = 1, `mdhd.timescale` = sample rate. Chunks hold thousands of
frames (observed: 2048, 1024, 3×1024…). Content is unstructured: **no boundary can be
detected from the data**; only the chunk pattern (§3.6.6) can place PCM chunks.

### 2.9 AVI (Microsoft RIFF, OpenDML 1.02)

All integers LE. Chunk = `ckID(4) ckSize(4) data[ckSize] pad-to-even`. `ckSize` excludes
header and pad. `LIST` = `'LIST' listSize listType data`. `RIFF` = `'RIFF' fileSize 'AVI '`,
`fileSize` = total − 8.

```
RIFF ('AVI '
  LIST ('hdrl'
    'avih' (56 bytes)
    LIST ('strl' 'strh' 'strf' ['strd'] ['strn'] ['indx'])   × streams
    [LIST ('odml' 'dmlh')]
  )
  [LIST ('INFO' …)] ['JUNK']
  LIST ('movi' { '##dc' | '##db' | '##wb' | '##pc' | '##tx' | 'ix##' | LIST('rec ' …) }* )
  ['idx1']
)
RIFF ('AVIX' LIST ('movi' …))*          ← OpenDML extension chunks, each ≤ ~1 GiB
```

**avih** (AVIMAINHEADER, 56 bytes):

| Off | Field |
|---|---|
| 0 | dwMicroSecPerFrame (33333 @ 30 fps) |
| 4 | dwMaxBytesPerSec |
| 8 | dwPaddingGranularity |
| 12 | dwFlags: 0x10 HASINDEX, 0x20 MUSTUSEINDEX, 0x100 ISINTERLEAVED, 0x800 TRUSTCKTYPE, 0x10000 WASCAPTUREFILE, 0x20000 COPYRIGHTED |
| 16 | dwTotalFrames (frames in the **first** RIFF only when OpenDML; `dmlh` holds the grand total) |
| 20 | dwInitialFrames |
| 24 | dwStreams |
| 28 | dwSuggestedBufferSize |
| 32 | dwWidth |
| 36 | dwHeight |
| 40 | dwReserved[4] |

**strh** (AVISTREAMHEADER, 56 bytes): fccType 4CC (`vids`, `auds`, `txts`, `mids`),
fccHandler 4CC (`FMP4`, `XVID`, `H264`, `MJPG`, `DIVX`, or 0), dwFlags, wPriority u16,
wLanguage u16, dwInitialFrames, dwScale, dwRate (fps = dwRate/dwScale), dwStart, dwLength
(frames or audio blocks), dwSuggestedBufferSize, dwQuality, dwSampleSize (0 for video,
block align for audio), rcFrame (4 × i16).

**strf**: video = BITMAPINFOHEADER (40 bytes: biSize, biWidth, biHeight, biPlanes u16,
biBitCount u16, biCompression 4CC, biSizeImage, …); audio = WAVEFORMATEX (18 bytes:
wFormatTag u16 (1 PCM, 0x55 MP3, 0xFF AAC, 0x2000 AC-3), nChannels u16, nSamplesPerSec,
nAvgBytesPerSec, nBlockAlign u16, wBitsPerSample u16, cbSize u16 + extra).

**movi chunk ids**: `##dc` compressed video, `##db` uncompressed video, `##wb` audio,
`##pc` palette, `##tx` text, where `##` is the 2-digit decimal stream index (`00`, `01`).
`ix##` = OpenDML standard index placed inside movi. `rec ` LIST groups chunks.

**idx1** (AVIOLDINDEX): array of 16-byte entries: dwChunkId 4CC, dwFlags (0x10
AVIIF_KEYFRAME, 0x01 AVIIF_LIST, 0x100 AVIIF_NO_TIME), dwOffset, dwSize. `dwOffset` is
relative to the position of the `movi` 4CC (i.e. `LIST` header + 8) and points at the
chunk's **ckID**; some old writers store absolute file offsets — detect by checking whether
`movi_pos + dwOffset` or `dwOffset` lands on the expected ckID. Observed ffmpeg: first entry
`00dc, 0x10, 4, 0x26AC` (4 = right after the `movi` tag).

**OpenDML indx / ix##** (AVIMETAINDEX base, 24 bytes + entries): wLongsPerEntry u16,
bIndexSubType u8 (0, or 1 = AVI_INDEX_2FIELD), bIndexType u8 (0 AVI_INDEX_OF_INDEXES =
super index, 1 AVI_INDEX_OF_CHUNKS = standard index, 0x80 AVI_INDEX_IS_DATA),
nEntriesInUse u32, dwChunkId 4CC, then:
- super index (`indx` in strl, wLongsPerEntry=4): dwReserved[3]; entries { qwOffset u64
  (absolute offset of the `ix##` chunk), dwSize u32 (its size), dwDuration u32 };
- standard index (`ix##`, wLongsPerEntry=2): qwBaseOffset u64, dwReserved u32; entries
  { dwOffset u32 (qwBaseOffset + dwOffset = offset of chunk **data**, i.e. after the 8-byte
  chunk header; FFmpeg avidec computes `pos = rl32 + base - 8` to get the ckID), dwSize u32
  (bit 31 set ⇒ delta frame / not keyframe; low 31 bits = size) }. FFmpeg requires
  wLongsPerEntry == 2, bIndexSubType == 0 for a standard index.

**dmlh** (in LIST odml): dwTotalFrames u32 across all RIFF chunks (+ reserved to 248 bytes
in some writers).

### 2.10 Matroska / WebM (EBML, RFC 8794; Matroska spec)

**VINT** (used for both element IDs and sizes): count leading zero bits of the first byte;
length = zeros + 1 (1..8 bytes); for *sizes* the marker bit is removed and the remaining
7×len bits are the value BE; for *IDs* the marker bit is kept (ID `0x1A45DFA3` is stored
as those 4 bytes). A size whose value bits are all 1 (`0xFF`, `0x7FFF`, …,
`0x01FFFFFFFFFFFFFF`) is **unknown size**: the element extends until a sibling/parent-level
element ID is found. Only master elements (Segment, Cluster) are written with unknown size
in practice (ffmpeg `-live 1`, crashed recorders, OBS before remux). Max ID length 4, max
size length 8 (declared in EBML header).

| Element | ID | Level | Notes |
|---|---|---|---|
| EBML | 1A 45 DF A3 | 0 | file magic; children: EBMLVersion 42 86, EBMLReadVersion 42 F7, EBMLMaxIDLength 42 F2, EBMLMaxSizeLength 42 F3, DocType 42 82 (`matroska` / `webm`), DocTypeVersion 42 87, DocTypeReadVersion 42 85 |
| Segment | 18 53 80 67 | 0 | often unknown size |
| SeekHead | 11 4D 9B 74 | 1 | |
| Info | 15 49 A9 66 | 1 | TimestampScale 2A D7 B1 (ns, default 1 000 000), Duration 44 89 (float), MuxingApp 4D 80, WritingApp 57 41 |
| Tracks | 16 54 AE 6B | 1 | TrackEntry AE: TrackNumber D7, TrackUID 73 C5, TrackType 83 (1 video, 2 audio, 17 subtitle), CodecID 86 (`V_MPEG4/ISO/AVC`, `V_MPEGH/ISO/HEVC`, `V_VP9`, `V_AV1`, `A_AAC`, `A_OPUS`, `A_VORBIS`), CodecPrivate 63 A2 (avcC / hvcC bytes for AVC/HEVC), Video E0 (PixelWidth B0, PixelHeight BA), Audio E1 |
| Cluster | 1F 43 B6 75 | 1 | Timestamp E7 (u, in TimestampScale units), Position A7, PrevSize AB, SimpleBlock A3, BlockGroup A0 (Block A1, BlockDuration 9B, ReferenceBlock FB) |
| Cues | 1C 53 BB 6B | 1 | CuePoint BB |
| Chapters | 10 43 A7 70 | 1 | |
| Tags | 12 54 C3 67 | 1 | |
| Attachments | 19 41 A4 69 | 1 | |
| Void | EC | any | padding |
| CRC-32 | BF | any | first child, 4 bytes |

**SimpleBlock payload**: TrackNumber (VINT, size-style), Timestamp i16 (relative to
Cluster Timestamp), Flags u8: 0x80 keyframe, 0x08 invisible, 0x06 lacing (00 none, 01 Xiph,
11 EBML, 10 fixed-size), 0x01 discardable; then frame data (or lace count+sizes). `Block`
inside BlockGroup has the same layout but bits 0x80/0x01 are reserved; a BlockGroup
without ReferenceBlock is a keyframe.

Observed ffmpeg: non-live file Segment size `01 00 00 00 00 03 40 0F` (8-byte known);
`-live 1` writes `01 FF FF FF FF FF FF FF` (unknown).

### 2.11 MPEG-TS and M2TS (ISO/IEC 13818-1 §2.4.3)

Packet = 188 bytes, sync byte `0x47` at offset 0. **M2TS/AVCHD/Blu-ray** = 192-byte
packets: 4-byte `TP_extra_header` (copy_permission_indicator 2 bits + arrival_time_stamp
30 bits @ 27 MHz) **before** each 188-byte packet, so `0x47` is at offset 4, 196, 388….
Observed ffmpeg `-mpegts_m2ts_mode 1`: `0e bf 46 22 47 40 11 10 …`.

TS header (4 bytes): `sync 0x47 | TEI(1) PUSI(1) priority(1) PID(13) | TSC(2) AFC(2) CC(4)`.
AFC: 01 payload only, 10 adaptation only, 11 both, 00 reserved (invalid). Adaptation field:
length u8, flags u8 (0x80 discontinuity, 0x40 random_access, 0x10 PCR present → 6 bytes:
33-bit base, 6 reserved, 9-bit ext). PID 0x0000 PAT, 0x0001 CAT, 0x0010 NIT (DVB),
0x0011 SDT, 0x1FFF null. Payload with PUSI=1 on a PSI PID starts with `pointer_field u8`.

PAT (table_id 0x00): `table_id u8 | ssi(1) 0 r(2) section_length(12) | transport_stream_id
u16 | r(2) version(5) current_next(1) | section_number u8 | last_section_number u8 |
{ program_number u16 | r(3) PID(13) }* | CRC32`. program_number 0 ⇒ NIT PID.

PMT (table_id 0x02): same 8-byte prefix with program_number, then `r(3) PCR_PID(13) |
r(4) program_info_length(12) | descriptors | { stream_type u8 | r(3) elementary_PID(13) |
r(4) ES_info_length(12) | descriptors }* | CRC32`. stream_type: 0x01/0x02 MPEG-1/2 video,
0x03/0x04 MPEG audio, 0x0F AAC ADTS, 0x11 AAC LATM, 0x1B H.264, 0x24 HEVC, 0x06 private PES
(AC-3/DVB subs), 0x81 AC-3 (ATSC), 0x82/0x86 DTS. AVCHD/Blu-ray use PMT PID 0x0100, video
PID 0x1011, audio 0x1100+, PCR 0x1001. UNVERIFIED (HDMV PID conventions)

PES (inside payload after PUSI): `00 00 01 stream_id(E0–EF video, C0–DF audio, BD private 1,
BF private 2) | PES_packet_length u16 (0 allowed for video in TS) | '10' scrambling(2)
priority alignment copyright original | PTS_DTS_flags(2) + 6 flag bits | PES_header_data_length
u8 | [PTS 5 bytes: '0010'/'0011' pts[32..30] 1 pts[29..15] 1 pts[14..0] 1] [DTS 5 bytes '0001' …]`.

### 2.12 MPEG-PS / VOB (ISO/IEC 13818-1 §2.5, DVD-Video)

Pack header: `00 00 01 BA`, then MPEG-2: 10 bytes `'01' SCR[32..30] m SCR[29..15] m
SCR[14..0] m SCR_ext(9) m | program_mux_rate(22) m m | reserved(5) pack_stuffing_length(3)`
+ stuffing bytes. Byte 4 top bits `01` ⇒ MPEG-2 (observed `44`); `0010` ⇒ MPEG-1 (8-byte
pack header). System header `00 00 01 BB` + length u16. PES packets as above with
`PES_packet_length` always non-zero in PS. Program end code `00 00 01 B9` (ffmpeg's `vob`
muxer does **not** write it; DVD VOBs often do not either). DVD-Video VOB: every pack is
exactly 2048 bytes and starts with a pack header; navigation packs contain private stream 2
(`BF`) PCI/DSI; AC-3 in private stream 1 (`BD`, sub-stream id 0x80–0x87), LPCM 0xA0–0xA7,
subpictures 0x20–0x3F. Observed ffmpeg `-f vob`: 274 × 2048-byte packs, no B9.

---

## 3 Algorithms (pseudocode)

### 3.1 Damage-tolerant box walker

Never trust `size`. Every walk is bounded by the parent's end and by the reader's end.

```
struct Box { typ: [u8;4], hdr_len: u8, start: u64, size: u64 /*0 ⇒ to end*/, uuid: Option<[u8;16]> }

walk(r, start, end) -> Vec<Box> (stops at first inconsistency, returns what it has + reason):
    pos = start
    while pos + 8 <= end:
        size32 = be32(r, pos); typ = r[pos+4..pos+8]
        hdr = 8
        if size32 == 1: size = be64(r, pos+8); hdr = 16
        elif size32 == 0: size = end - pos
        else: size = size32
        if typ == 'uuid': hdr += 16
        if !is_plausible_type(typ): return (boxes, BadType{pos})      // all 4 bytes printable ASCII 0x20..0x7E, or known binary types
        if size < hdr: return (boxes, BadSize{pos})
        if pos + size > end:                                           // truncated or lying size
            boxes.push(Box{.., size: end - pos, truncated: true}); return (boxes, Truncated{pos, claimed: size})
        boxes.push(Box{typ, hdr, pos, size})
        pos += size
    return (boxes, Ok)
```

Top-level sanity for ISO BMFF: the sequence must start with `ftyp` (or `wide`/`free`/`skip`
then `ftyp` on some old QuickTime files UNVERIFIED), contain at most one `moov`
and ≥ 1 `mdat` (or `moof`/`mdat` pairs), and every box type must be one of the known
top-level types `{ftyp,free,skip,wide,mdat,moov,moof,mfra,meta,uuid,sidx,styp,pdin,udta,junk,PICT,prfl}`.
An unknown 4CC at top level is treated as "end of file" for carving purposes.

### 3.2 Carving: find the exact end

**ISO BMFF.** Start at `ftyp`. Walk top-level boxes. End = end of the last box that
*completes* within the search window **and** after which either (a) EOF of the search
window, (b) another `ftyp` (next file), or (c) an implausible type. Rules:

1. `mdat` with `size==0`: file ends where the next non-`mdat` plausible box ends (normally
   `moov` written after it). If no `moov` follows within 64 MiB of plausible data, treat as
   "mdat open-ended": end = position of next `ftyp`/other magic or first sector of a different
   content class (e.g. FAT directory, zeros ≥ 1 MiB).
2. If `moov` exists: compute `max_extent = max over tracks of (last chunk offset + sizes of its
   samples)`. If `max_extent > end_of_mdat` the file is truncated/mis-sized; the carve end =
   `max(box_walk_end, max_extent)` clipped to window.
3. If `moov` precedes `mdat` (faststart) and `mdat.size` is sane: end = `mdat` end, and the
   `max_extent` check confirms it.
4. Minimum size: 24 bytes (ftyp 16–32 + mdat header); anything without an `mdat`/`moov` is a
   stub.

**AVI.** End = `8 + RIFF.fileSize` if that lands on EOF-of-window or on another magic and
the chunk walk reaches `idx1` consistently. Otherwise walk: hdrl → movi chunks (each
chunk id must be `##xx` with `##` < dwStreams, or `ix##`, `LIST`, `JUNK`) → optional
`idx1` → then zero or more `RIFF AVIX` (each `LIST movi`). End = end of last complete chunk.
A RIFF size of 0 or 0xFFFFFFFF (crashed writer) ⇒ always walk.

**Matroska.** Walk EBML header (known size) then Segment. If Segment has known size and
`segment_start + size` is consistent (next bytes are not another level-1 element) → that is
the end. Otherwise walk level-1 elements; for each Cluster with unknown size walk its
children until an ID that is level-1 (Cluster, Cues, Tags, …) or not a valid Cluster child
is found. End = end of last complete SimpleBlock/BlockGroup. Minimum: EBML header + Segment
+ Info + Tracks + one Cluster (≈ 200 B).

**MPEG-TS/M2TS.** Decide packet size at the candidate start: try 188 and 192 (and 204
for DVB-FEC); require ≥ 8 consecutive packets with `0x47` at the stride. End = first position
where the stride fails (allow up to 2 missing syncs in a row to survive single bad sectors,
then resync). Require a PAT (PID 0) within the first 2000 packets to call it a file rather
than a fragment. Carve start for M2TS = sync − 4.

**MPEG-PS/VOB.** Start at `00 00 01 BA`. Walk: pack header (parse stuffing length), then
PES packets (`00 00 01 xx`, xx ≥ 0xBC except BA/B9) using `PES_packet_length` (always valid
in PS), until the next `00 00 01 BA`. For VOB additionally require 2048-byte alignment of
packs. End = after `00 00 01 B9` if present, else after the last pack that parses.

### 3.3 Validation levels

```
Level 0  magic ok, box/chunk walk completes             → "structurally complete"
Level 1  (ISO) moov parses; every track's sample map is inside mdat; stco monotonic;
         Σ stsz == stsz.sample_count; stsc first_chunk strictly increasing; stts sum ==
         mdhd.duration (±1 sample); stss ascending and ≤ sample_count
Level 2  sample-level: first NAL of each video sample has forbidden_zero_bit==0 and a VCL/
         prefix type; AVC: first slice has first_mb_in_slice==0; HEVC: first_slice_segment
         flag set; AAC frames start with id_syn_ele ∈ 0..6 and ics_reserved_bit==0; sizes
         within reference percentiles. Sample every chunk (not every sample) for speed.
Level 3  decode oracle (optional feature): feed first/middle/last GOP to a decoder crate
         (openh264/symphonia) and require no errors.
```

Reported outcomes: `Playable`, `Truncated{good_until}`, `MoovMissing`, `MoovCorrupt{box}`,
`MdatSizeWrong{claimed, actual}`, `HolesInMdat{ranges}` (from the ddrescue mapfile overlaid on
the file's extents, see edge cases), `IndexMissing` (AVI), `UnknownSizeSegment` (MKV).

### 3.4 Parsing a healthy `moov` into `SampleTable`

```
struct Track { id, handler: Vide|Soun|Meta|Tmcd|Other, timescale, duration, lang,
               stsd_raw: Vec<u8>, entries: Vec<SampleEntry>, stts, ctts, stsc, stsz, stco: Vec<u64>,
               stss: Option<Vec<u32>>, elst: Option<..>, tkhd_raw: Vec<u8>, mdhd_raw, hdlr_raw, minf_header_raw (vmhd/smhd+dinf) }
```

Keep raw bytes of `tkhd`, `mdhd`, `hdlr`, `vmhd/smhd`, `dinf`, `stsd` so the writer can copy
them verbatim (only durations and IDs are patched). Parse `avcC`/`hvcC`/`esds` into
`CodecConfig { nal_len_size, sps, pps, (hevc) vps, (aac) asc }`.

### 3.5 Reference profile (statistics taken from the healthy file)

For each track of the reference (same camera, same mode: resolution, fps, codec):

```
struct TrackProfile {
    codec: Avc{nal_len_size, sps_info, pps_info} | Hevc{..} | Aac{asc, frame_len} | Pcm{bytes_per_frame} | Gpmd | Tmcd | Other,
    sample_delta_mode: u32,              // most common stts delta (video: 512/1001/…; aac: 1024)
    size_min, size_p01, size_median, size_p99, size_max,
    keyframe_size_p99,                   // sizes of stss samples
    chunk_samples_mode: u32, chunk_samples_min, chunk_samples_max,   // from expanded stsc
    chunk_bytes_median, chunk_bytes_max,
    ctts_pattern: Vec<i64>,              // first GOP's ctts offsets / frame_duration (e.g. [2,4,1,1,…])
    gop_len: u32,                        // mode of stss gaps
    first_bytes_pattern: MutualPattern,  // bytes common to the first 16–32 bytes of every sample (untrunc "mutual pattern"):
                                         //   start with sample[0][..32]; for each next sample set is_mutual[i] = false where bytes differ
}
struct FileProfile { track_order: Vec<track_idx> /* chunk order in mdat, e.g. [V, A, V, A…] */, brand, mvhd_timescale, free_padding_inside_mdat: Option<u32> }
```

`track_order` is derived by sorting all chunks of all tracks by offset and recording the
track sequence; the dominant period (e.g. `V A`) is the chunk pattern used in §3.6.6.
Also record `gap_between_chunks` (should be 0; non-zero constant ⇒ writer pads chunks, e.g.
to 4 KiB — treat as `free`).

### 3.6 `mdat` scan and `moov` rebuild (the untrunc / recoverMP4 approach)

Inputs: damaged file D (with `mdat` located, or the whole file if no boxes are recognisable),
reference profile R. Output: `Vec<Track>` with rebuilt tables.

```
rebuild(D, R):
    (mdat_start, mdat_end) = locate_mdat(D)       // §3.9 cases
    pos = mdat_start; unknown_run = 0
    tracks = R.tracks.filter(|t| t.handler in {Vide, Soun})   // drop meta/tmcd/text
    prev_track = None
    loop while pos < mdat_end:
        pos = skip_padding(D, pos)                // zeros (≤ 64 KiB), 'free'/'skip'/'wide' boxes, 'mdat' headers (nested
                                                  // mdat from camera restart)  — record skipped ranges
        if pos >= mdat_end: break
        cand = []
        for t in tracks:
            if let Some(m) = t.matcher.match_at(D, pos, mdat_end - pos, R.track[t]):   // §3.6.1–3.6.6
                cand.push((t, m))                 // m = { len, duration, keyframe, confidence }
        choose = select(cand, expected_next_track(prev_track, R.track_order, chunk_state))
        if choose is None:
            // nothing matches: advance. Step 1 byte if unknown_run small, else 4-byte then 512-byte steps
            unknown_run += step; pos += step
            if unknown_run > R.chunk_bytes_max * 4: give up on this region → record hole, jump to next sector-aligned
               position that matches any matcher with high confidence (keyframe search, §3.6.7)
            continue
        if unknown_run > 0: holes.push((pos - unknown_run, pos)); unknown_run = 0
        append_sample(choose.t, offset=pos, size=choose.m.len, dur=choose.m.duration, key=choose.m.keyframe)
        pos += choose.m.len; prev_track = choose.t
    finalize_chunks(tracks)                        // group consecutive samples of the same track into chunks
    clip_tail(tracks)                              // drop the last sample of each track if it was cut by EOF
    return tracks
```

`select`: prefer a candidate from the expected track when its confidence ≥ medium; otherwise
the highest confidence; ties → video (its matcher is far more specific than audio's).

#### 3.6.1 AVC matcher (`avc1`/`avc3`)

```
match_avc(buf, max, R) -> Option<Match>:
    pos = 0; len = 0; key = false; seen_slice = false; prev: Option<SliceHdr> = None
    loop:
        if pos + nal_len_size > max: break
        n = be_uint(buf[pos..], nal_len_size)
        if n == 0 || n > max - pos - nal_len_size || n > R.size_max * 2: break      // length implausible ⇒ end (or garbage)
        h = buf[pos + nal_len_size]
        if h & 0x80 != 0: break                                                      // forbidden bit
        typ = h & 0x1F; ref_idc = (h >> 5) & 3
        match typ:
            9 (AUD): if seen_slice: break /* next AU */ else {}                      // AUD at start is part of this AU
            7 (SPS), 8 (PPS), 6 (SEI), 14, 15: if seen_slice: break                  // prefix NALs after slices ⇒ next AU
                       (for SPS: optionally re-derive sps_info if avc3)
            1 | 5:
                s = parse_slice_header(rbsp(buf[pos+nal_len_size+1 ..][..64]), R.sps, R.pps)?   // failure ⇒ break
                if s.slice_type > 9 || s.pps_id != R.pps.id: break
                if seen_slice && (s.first_mb_in_slice == 0 || is_new_picture(prev, s, ref_idc)): break
                if typ == 5: key = true
                seen_slice = true; prev = s
            12 (filler): {}                                                          // keep inside sample (x264 CBR)
            2..=4: seen_slice = true
            10 | 11: {} 
            _ : if seen_slice: break else return None                              // unknown type at AU start ⇒ not AVC here
        pos += nal_len_size + n; len = pos
        if pos == max: break
    if !seen_slice: return None
    conf = if len within [R.size_min/2, R.size_p99*1.5] { High } else { Medium }
    return Match{ len, duration: R.sample_delta_mode, keyframe: key, confidence: conf, poc: prev.poc_lsb }
```

`is_new_picture(prev, cur, ref_idc)` (H.264 §7.4.1.2.4): `frame_num` differs, `pps_id`
differs, `field_pic_flag` differs, `bottom_field_flag` differs, `nal_ref_idc` differs with one
of them 0, `pic_order_cnt_type==0` and `pic_order_cnt_lsb` differs (or
`delta_pic_order_cnt_bottom` differs), `idr_pic_flag` differs, both IDR and `idr_pic_id`
differs. (untrunc enables the POC/idr_pic_id checks only in strict mode because some
encoders violate them; keep them behind a `strict_nal` option, default on, auto-off if the
reference file itself trips them.)

Keyframe for `stss`: AU contains a type-5 NAL. (x264 open-GOP "recovery point" SEI
keyframes are type 1 with SEI 6 payload type 6 — treat as non-sync; QuickTime would put them
in `stps`.)

Also require for the **first** NAL at a candidate position: `typ ∈ {1,5,6,7,8,9,12,14}`; any
other type at offset 0 means "not a video sample here" (fast reject, no parsing).

#### 3.6.2 HEVC matcher (`hvc1`/`hev1`)

```
match_hevc(buf, max, R):
    pos = 0; seen_slice = false; key = false; prev_layer/prev_tid = None
    loop:
        n = be_uint(buf[pos..], nal_len_size); same plausibility checks as AVC
        b0 = buf[pos+L]; b1 = buf[pos+L+1]
        if b0 & 0x80: break
        typ = (b0 >> 1) & 0x3F; layer = ((b0 & 1) << 5) | (b1 >> 3); tid1 = b1 & 7
        if typ > 40: break;  if typ != 37 && tid1 == 0: break;  if typ == 37 && tid1 != 0: break
        match typ:
            0..=9 | 16..=21:
                first_slice = buf[pos+L+2] >> 7
                if seen_slice && first_slice == 1: break               // new picture
                if seen_slice && (layer != prev_layer || tid1 != prev_tid): break
                if R.pps.dependent_slice_segments_enabled_flag || !first_slice: full-parse slice header (§2.6) to validate slice_type ≤ 2
                if typ in {16,17,18,19,20,21}: key = true              // BLA/IDR/CRA
                seen_slice = true
            35 (AUD): if seen_slice: break
            32 | 33 | 34 | 39: if seen_slice: break                     // VPS/SPS/PPS/prefix SEI start next AU
            40 (suffix SEI), 38 (FD), 36 (EOS): {}                      // stay in this AU
            10..=15 | 22..=31 | 41..=47: break                          // reserved
            _ (48..63): if seen_slice: break else return None
        pos += L + n; …
```

Keyframe for `stss`: IDR (19/20), BLA (16–18), CRA (21). Note the ponchio untrunc fork
omits IDR_N_LP (20) from its slice list; x265's first frame *is* type 20 — do not copy that
bug.

#### 3.6.3 POC → composition offsets (`ctts`)

Per video track after the scan (H.264 type 0 / HEVC), using `frame_dur =
R.sample_delta_mode`:

```
derive_ctts(samples):           // samples in decode order with {poc_lsb, keyframe, nal_type}
    for each GOP g = [k, next_key):                                   // k = index of a sync sample
        poc = reconstruct_poc(samples[g])                             // §2.5 MSB rule; IDR ⇒ 0; HEVC IDR has no lsb ⇒ 0
        order = argsort(poc)                                          // display rank within GOP
        rank[i] = position of i in order
        shift_g = max over i of (i - rank[i])                         // reorder depth of this GOP (0 ⇒ no B-frames)
    shift = max over GOPs of shift_g            // use one global shift so pts is monotonic across GOPs
    if R.ctts_pattern available: shift = max(shift, R.ctts_pattern.min())   // match reference delay (ffmpeg: 2 for bf=2)
    ctts[i] = (rank[i] - (i - gop_start) + shift) * frame_dur         // ≥ 0 always
    if all ctts == 0: omit ctts box
    elst: single entry { segment_duration = movie duration, media_time = shift * frame_dur, rate 1 }  (ctts v0)
          — or write ctts v1 with offsets - shift*frame_dur and omit elst; prefer v0 + elst (QuickTime/Android friendlier)
```

Verified against ffmpeg (`-bf 2`): POC lsb sequence 0,6,2,4,… → ranks 0,3,1,2 → with
shift 2 gives ctts 1024, 2048, 512, 512 = exactly the encoder's table (frame_dur 512).

Open-GOP HEVC (CRA + RASL): RASL pictures have POC < CRA's POC and must be ranked
*before* the CRA; sorting the whole stream by reconstructed POC rather than per-GOP handles
it; keep GOP boundaries at IDR/BLA only in that case.

#### 3.6.4 AAC matcher (`mp4a` with ASC object type 2/5/29)

There is no length field. Strategy (in order of strength):

1. **Expected-size window** from the reference: `[R.size_min − 20%, R.size_p99 × 1.3]`.
2. **Start plausibility** on the first 2–4 bytes: `id = b[0] >> 5`; mono ⇒ id must be 0
   (SCE) or 6 (FIL) or 4 (DSE); stereo ⇒ 1 (CPE) or 6/4; `element_instance_tag` must be
   < 16 and is 0 in every camera file seen (tag ≠ 0 ⇒ low confidence); for SCE/CPE-with-
   common_window: `ics_reserved_bit == 0`, `max_sfb ≤ num_swb(sr, window_sequence)`.
   For HE-AAC (AOT 5/29) the core AAC-LC frame is the same; the FIL element carrying SBR
   data appears before END.
3. **End detection**: the candidate ends at the first byte offset `e` in the size window
   where (a) the END-pattern of §2.7 holds at `b[e-1]`, and (b) the bytes at `e` pass
   the *start plausibility* test for the next element (or the next expected track's
   matcher, typically video, matches at `e`). Among several `e`, prefer the one where the
   next-track matcher matches with High confidence (video NAL header is very specific), then
   the one closest to `R.size_median`.
4. **Decode oracle** (feature `aac-oracle`): run `symphonia-codec-aac` on `b[0..e]` with
   the ASC; a decode error ⇒ reject `e`. This is what untrunc does with libavcodec: it
   decodes and takes *consumed bytes* as the length (`consumed == maxlength` ⇒ "cannot
   determine", `consumed == 6` ⇒ silence frame high confidence, `< 400` medium, else high).
   Without libav we cannot get "consumed", so use the explicit candidate `e` enumeration.
5. **Chunk-pattern fallback** (§3.6.6): when audio samples are being scanned inside an
   audio chunk whose *total byte length* is known (next video sample position found by a
   forward keyframe/NAL search), split the chunk into `n = R.chunk_samples_mode` frames by
   searching boundaries from both ends.

Duration per frame = 1024 (or 960) at `mdhd.timescale = sample rate`. The last audio frame
of the file may be short (ffmpeg writes a shorter stts delta); just use 1024.

#### 3.6.5 PCM matcher (`sowt`/`twos`/`lpcm`/`in24`)

No structure. Only placeable by the chunk pattern: after a video sample sequence of
`R.chunk_samples_mode` samples, assume an audio chunk of `R.chunk_bytes_median` bytes
(rounded to `bytes_per_frame`), **and** verify the next video matcher fires exactly at the
end; if not, search forward for the next video NAL start within ±`R.chunk_bytes_max` and
take that as the chunk end (`len` must be a multiple of `bytes_per_frame`; round down and
record the remainder as a hole). untrunc gives PCM confidence 2 ("we really have no idea")
and uses `stats.fixed_size`. A heuristic PCM sanity check: 16-bit audio has small
byte-to-byte differences in the high bytes (`looksLikeTwosOrSowt`: differences of the MSB
bytes mostly in 8..0xF0 range are *not* PCM); zero runs and low-entropy are PCM-like.

#### 3.6.6 Chunk-pattern driver

```
state = { expect: iterator over R.track_order cycle, samples_in_chunk: 0 }
expected_next_track(prev, pattern, st):
    if prev is None: return pattern[0]
    t = prev
    if st.samples_in_chunk >= R.track[t].chunk_samples_mode: return pattern.next_after(t)   // chunk complete → switch
    return t                                                                                 // stay in this chunk
on each accepted sample: if same track as prev: st.samples_in_chunk += 1 else st.samples_in_chunk = 1
```

Use the expectation only to *rank* candidates, never to reject a High-confidence video match:
real files vary chunk sizes (observed ffmpeg audio chunks of 1–5 frames; cameras keep them
constant, e.g. 30 video / 47 audio at 30 fps 48 kHz = 1 s). Learn `chunk_samples_mode`
per track from the reference and, after the first few chunk transitions in the damaged
file, update it with the observed values (untrunc `-dyn`).

#### 3.6.7 Resynchronisation after a hole

When nothing matches for more than `R.chunk_bytes_max` bytes (unreadable sectors are
zero-filled by the imager; the mapfile tells exactly where), jump to the next 512-byte
boundary and search for: (a) `00 00 xx xx 65|25|45` style IDR NAL (AVC: length prefix with
`(b[4] & 0x1F) == 5` and `first_mb_in_slice == 0` and `frame_num` parse OK), (b) HEVC
`(b[4] >> 1) & 0x3F ∈ {19,20,21}` with `b[6] & 0x80`, (c) x264/x265 SEI user data
(`06 05 ff ff` prefix then "x264 - core"), (d) AAC start plausibility + END pattern
consistent with `R.size_median`. Mark samples between the hole and the next keyframe as
non-sync; the output still references them (decoders conceal), or drop them when the user
asks for "clean" output.

### 3.7 Writing the repaired file

```
write(out, R, tracks, mdat_src_ranges):
    // 1. decide offsets: ftyp + moov first (streamable), then mdat
    ftyp  = R.ftyp_raw (or synthesise: major 'isom', minor 0x200, compat [isom, iso2, avc1|hvc1?, mp41] ; 'qt  ' if R was QuickTime)
    moov  = build_moov(tracks, placeholder offsets)
    mdat_hdr_len = (total_media_bytes + 8 > 0xFFFFFFFF) ? 16 : 8
    mdat_pos = len(ftyp) + len(moov)
    patch stco/co64: new_offset = mdat_pos + mdat_hdr_len + (old_offset - mdat_src_data_start) − bytes_of_holes_skipped_before(old_offset)
       (if co64 is needed because mdat_pos + size > 4 GiB, rebuild moov with co64 — moov size changes ⇒ iterate once)
    out.write(ftyp); out.write(moov)
    write mdat header: size32 = total+8 (or 1 + largesize = total+16)
    copy media bytes: for each accepted sample range in file order (skipping holes/padding)
```

`build_moov`:

```
mvhd: v0 (v1 if any duration > u32), timescale = R.mvhd.timescale (1000 typical), duration = max over tracks of track_duration_in_movie_ts, next_track_ID = max id + 1
for each track (video first, then audio; ids 1, 2):
    tkhd: copy R.tkhd_raw; set track_ID, duration (movie ts), flags 0x3 (enabled | in_movie) (0x7 for QT)
    edts/elst: omit unless ctts shift > 0 or AAC priming known from R (copy R.elst with segment_duration = new track duration; keep media_time)
    mdia:
      mdhd: copy R (timescale, language); duration = Σ stts
      hdlr: copy R
      minf: vmhd|smhd + dinf (copy R) + stbl:
        stsd: copy R.stsd_raw verbatim (keeps avcC/hvcC/esds/pasp/colr/btrt)
        stts: run-length-encode [duration per sample]; AAC: one entry (n, 1024); video: one entry (n, frame_dur)
        ctts: (video only, if any offset ≠ 0) run-length from derive_ctts
        stss: (video only) 1-based indices of keyframes; omit if every sample is a keyframe
        stsc: run-length-encode samples_per_chunk with sample_description_index 1
        stsz: sample_size = common size if all equal (PCM) else 0 + per-sample table
        stco/co64: one entry per chunk
    (drop sdtp, sgpd, sbgp, stps, cslg, stsh, tref, udta from R; they would be inconsistent)
udta: optionally a 'meta'/'©too' string "DiskRescue" — keep out by default
```

Run-length encoding `(count, value)` for stts/ctts/stsc: merge consecutive equal values;
for stsc the key is `samples_per_chunk` and `first_chunk` = index of the first chunk of
the run (1-based).

### 3.8 Table clipping (moov present, mdat truncated or holed)

```
clip(track, mdat_end):
    build sample map; find first sample n such that offset+size > mdat_end (or offset lies inside a hole)
    keep samples [0, n); for video, optionally back off to the last keyframe before n for clean ends
    truncate stsz, stts (recompute runs for n samples), ctts, stss (drop entries > n), sdtp
    chunks: keep chunks whose first sample < n; stsc re-encoded; stco truncated
    durations recomputed; elst segment_duration = new duration
    if n == 0: drop the track entirely (audio-only or video-only output is still valid)
```

If the file's `stco` offsets exceed the file size by a constant (file was carved with its
head cut), add the constant and retry (`moov`-relative consistency check: the first chunk
of the first track is normally at `mdat_start + 8/16`).

### 3.9 Locating `mdat` in the damaged file (case table)

| Case | Detection | Action |
|---|---|---|
| A. `moov` present, `mdat` truncated | box walk ends inside `mdat`; `max_extent > file_len` | §3.8 clip; rewrite `mdat` size |
| B. `moov` at end, file truncated (most common crash case: Canon/Sony/GoPro/phones) | `ftyp` ok, `mdat` with size 0 or size > file, no `moov` | §3.6 rebuild using reference; `mdat` data starts after its header |
| C. `mdat` size field 0 or wrong but `moov` exists | walker reports claimed ≠ actual | if `moov` is after the mdat and reachable by scanning forward for `moov` at 4-byte alignment: just fix the size field (size = moov_pos − mdat_pos); else case A |
| D. No boxes at all (head sector lost) | raw data at offset 0 matches a matcher | treat whole file as `mdat` data, rebuild, synthesise `ftyp` from R |
| E. `moov` present but `stco` points outside / garbage (bad sectors inside `moov`) | table checks fail | discard `moov`, rebuild (B); keep `stsd` from this file's moov if parsable, else from R |
| F. Multiple `mdat` boxes / nested `mdat` header mid-stream (camera appended after restart) | walker sees second `mdat` | skip the 8/16-byte header in the scan (`skip_padding`), concatenate |
| G. `moov` present but belongs to a *different* (earlier) recording (stale index from unallocated space) | sample map's first NAL check fails at level 2, or durations wildly off | treat as B |
| H. Fragmented (`moof`) | `mvex` in moov | cut after last complete `moof`+`mdat`; write nothing else |

Finding a trailing `moov` when the `mdat` size is 0/wrong: scan backwards from EOF for
`moov` whose claimed size == distance to EOF (fast path), else forward scan at 4-byte
alignment inside the last 64 MiB.

### 3.10 Camera-specific tracks and quirks (what to drop)

| Vendor | Extra tracks / boxes | Rebuild action |
|---|---|---|
| GoPro (HERO) | `gpmd` (telemetry, handler `meta`), `tmcd` timecode, `fdsc` (firmware), text `GoPro…` tracks; `free` padding; brand `qt  `/`mp41` | drop all but `vide`+`soun`. gpmd samples are KLV: 4CC in `{DEVC, DVID, DVNM, STRM, STNM, RMRK, SCAL, SIUN, UNIT, TYPE, TSMP, TIMO, EMPT}` + `type u8 size u8 repeat u16` → sample length = 8 + size×repeat (untrunc: `(be32(b+4) & 0xffff) + 8` for the outer DEVC) — only needed to *skip* them in `mdat` |
| Canon (EOS/PowerShot MOV) | `uuid` CNTH thumbnail in moov, `CNDA`, `tmcd` track, `free`/`skip` before `mdat` reserve; `moov` at end | drop tmcd; copy video `stsd` verbatim (contains `colr`, `pasp`, `fiel`) |
| Sony (XAVC S MP4) | `rtmd` real-time metadata track (`meta`), `uuid` with XML (`PROF`), `free` | drop rtmd; untrunc handles "Sony XAVC" explicitly |
| DJI (MP4/MOV) | `djmd` and `dbgi` meta tracks, `free` | drop |
| Insta360 | trailing proprietary metadata after `moov`, brand `qt  ` | ignore trailer |
| Samsung/Android (MediaMuxer) | `moov` at end; `free` 2^? reserve; sometimes `sefd` trailer (Samsung) | ignore trailer |
| Apple iPhone | `moov` at end; `mebx` metadata track, `tmcd`; HEVC `hvc1` with `dvh1` Dolby Vision | drop mebx/tmcd; keep hvc1 |
| Dashcams (Novatek/Ambarella) | `moov` at *start* (preallocated, sometimes **zeroed** when the crash hit) ; GPS in `free`/`moov.udta` | case E/B |

Keep only tracks whose `hdlr` is `vide` or `soun` and whose codec is one of avc1/avc3/hvc1/
hev1/mp4v/mjpa/jpeg/apch*/ (video) and mp4a/sowt/twos/lpcm/in24/ac-3/ec-3/alac/samr (audio).

### 3.11 AVI repair

```
repair_avi(f):
    parse RIFF/hdrl; locate movi list (by walk, not by sizes)
    walk movi chunks; keep entries { id, flags, offset_from_movi, size } for every ##dc/##db/##wb/##pc with a plausible id (stream index < dwStreams) and size ≤ remaining
    stop at first implausible chunk (junk from a bad sector): try resync by scanning forward 2-byte aligned for '00dc'/'01wb' etc. within 1 MiB
    keyframe flag: for ##dc with fccHandler in {MJPG, DIVX?}: MJPG all keyframes; MPEG-4 part 2: first byte pattern 00 00 01 B6 and (b[4] >> 6) == 0 (I-VOP); H.264-in-AVI: Annex B, NAL type 5 present; else copy from damaged idx1 when present
    rebuild idx1 (entries in file order, dwOffset relative to movi tag)
    avih.dwTotalFrames = count of video chunks of stream 0 (first RIFF); dmlh.dwTotalFrames = total
    strh.dwLength per stream = chunk count (video) or Σ audio bytes / nBlockAlign (PCM) or chunk count (compressed audio)
    RIFF size = file_len − 8 ; LIST movi size = movi_end − movi_data_start ; LIST sizes of hdrl unchanged
    write: hdrl (patched in place) + movi (truncated at last good chunk, pad to even) + new idx1
    OpenDML (AVIX) inputs: emit a single standard RIFF if total < 2 GiB, else keep AVIX structure and rebuild ix## + indx
```

Video streams in AVI are Annex B / raw codec frames, so sample boundaries are the chunk
boundaries: no bitstream parsing needed except for keyframe flags.

### 3.12 Matroska / TS / PS repair

- **MKV**: output = EBML header + Segment (unknown size kept, or known size recomputed) +
  copied SeekHead-less Info/Tracks + clusters up to the last complete block; drop trailing
  partial cluster bytes; drop Cues (players rebuild) and SeekHead (or write a Void of the
  same size). Rewrite Info.Duration from the last cluster timestamp + last block timestamp.
- **TS/M2TS**: output = packets from the first PAT through the last packet with a valid
  sync and correct size; drop packets with TEI set; optionally drop until the first
  random_access_indicator / IDR on the video PID so playback starts clean.
- **PS/VOB**: output = packs from the first pack header through the last complete PES;
  append `00 00 01 B9`.

---

## 4 Edge cases and failure modes seen on real failing media

1. **Zero-filled holes**: the imaging engine writes 0x00 for unreadable blocks (ddrescue
   default); inside `mdat` a 4 KiB zero run produces NAL length 0 (AVC loop breaks) or
   corrupts a slice silently (length prefix intact, payload zeroed). Overlay the ddrescue
   mapfile (`-` bad, `*` non-trimmed, `/` non-scraped) on the file's extent list and mark
   samples overlapping bad ranges as damaged *before* scanning; never treat a hole as a
   sample boundary. In `moov`, a hole corrupts tables silently: validate every table (§3.3
   level 1) and prefer rebuild over trusting a holed `moov`.
2. **Partial sector at the end**: SD cards and USB sticks die mid-write; the last cluster
   holds stale data from a previous file (FAT does not zero). The last video sample often
   parses "fine" into foreign NALs — enforce the `R.size_p99` cap and drop the last sample of
   every track (`clip_tail`).
3. **Fragmented FAT/exFAT files**: a carved (not filesystem-recovered) MP4 may contain
   interleaved clusters of another file. Symptoms: a different SPS/PPS id, resolution change,
   AAC `max_sfb` violations, `frame_num` jumping by > 1 with `gaps_in_frame_num` = 0.
   Treat as a hole and resync (§3.6.7). When a filesystem is available, always prefer the
   filesystem's cluster chain over carving.
4. **`mdat` > 4 GiB on FAT32**: cameras split at 4 GiB (Canon/GoPro *.MP4 chapters,
   Sony `C0001.MP4`+`C0002.MP4` etc.); each part is a full MP4; the second part's first GOP
   is a keyframe. Do not try to join; repair each part. On exFAT single files > 4 GiB use
   `co64`; a `stco` whose offsets wrapped (writer bug) shows chunk offsets *decreasing* — add
   `k × 2^32`.
5. **Camera writes `moov` at the end but pre-allocates a `free` of exactly the final `moov`
   size at the start**: after a crash, the `free` is still `free`; do not mistake the reserve
   for a damaged `moov`. Some firmware writes a *zeroed* `moov` (type and size present,
   payload zeros) — detect by `mvhd.timescale == 0`.
6. **Stale `moov` from an older recording** (same card, same scene): tables parse, sizes
   are plausible, but offsets point at the wrong frames. Level-2 validation (first NAL of
   each chunk) catches this because a chunk start lands mid-NAL; fall back to rebuild.
7. **Encoder drift between reference and damaged file**: different resolution/profile
   (`avcC` differs), different frame rate (stts delta) → refuse to use the reference unless
   the *first* IDR in the damaged file parses with the reference SPS (width/height match
   via `pic_width_in_mbs_minus1`), and warn. If the damaged file's own `stsd` survived
   (case E), prefer it.
8. **Variable frame rate** (phones): stts has many entries; the rebuild writes a constant
   delta → audio drifts vs video. untrunc mitigates by stretching video to match audio
   duration (`-sv`/`-sa`-like options). Implement: after rebuild, if |video_dur − audio_dur|
   > 0.5 s, set the video `stts` delta so that durations match (video_dur ≈ audio_dur).
9. **H.264 with multiple slices per picture** (some Ambarella dashcams, 4 slices):
   `first_mb_in_slice` is 0 only for the first slice; rely on it rather than on the
   §7.4.1.2.4 comparisons (which would be equal for all 4 slices, correctly).
10. **Field-coded / interlaced AVC** (`frame_mbs_only_flag = 0`, AVCHD 1080i cameras as
    MP4/MOV): two field pictures per frame; each field is a *separate* NAL with its own
    slice header (`field_pic_flag = 1`, bottom alternating); the MP4 sample = both fields.
    Group fields by `(frame_num, poc)` pairs: a new sample starts only when
    `frame_num` changes or `bottom_field_flag` goes from 1 to 0.
11. **AAC with `channelConfiguration = 0`** (PCE in stream) and **HE-AAC v2** (PS): element
    id checks still hold; SBR data is in a FIL element; frame sizes are smaller (≈ 48 kbps
    → 128 B). Use the reference statistics; the ASC tells you AOT.
12. **PCM audio in MOV (Canon/Zoom/BMPCC `sowt`/`in24`/`lpcm`)** cannot be delimited —
    only the chunk pattern works; verify by requiring the next video match at the end of
    the assumed chunk, else the audio track's chunk boundary is moved to the found video
    start (the pattern is self-correcting in both directions as long as video is found).
13. **Mixed-up ordering after resync**: after a hole, audio and video chunk phase is
    unknown; try both expected tracks at the resync point.
14. **mdat size 1 with largesize 0 / garbage**: QuickTime writers that convert `wide`+`mdat`
    in place; if the largesize is absurd, treat as size 0.
15. **AVI**: `idx1` present but offsets absolute (old VirtualDub), `idx1` referencing `rec `
    lists, JUNK between chunks, odd-sized chunks without padding (writer bug), `movi` list
    size 0 (crash). `dwTotalFrames` of 0 with a huge file ⇒ crash during capture: rebuild.
16. **MKV from crashed OBS/recorders**: unknown-size Segment and Cluster, no Cues, Duration
    = 0; last block partial. Also EBML Void elements of 0 size, and CRC-32 mismatches
    (ignore CRC for repair).
17. **TS/M2TS from camcorders**: `.MTS` files are often exactly N × 192 bytes but the
    first packet may be a partial (start carving at the first 0x47 whose stride holds).
    AVCHD splits at 2 GiB (`00000.MTS`, `00001.MTS`) with continuity across files.
18. **Very large reference file**: the reference parse must not load `mdat`; it only
    needs `moov` (seek to it) plus, for `first_bytes_pattern`, the first 32 bytes of ~200
    samples (random access by offsets).

---

## 5 Test fixture strategy (ffmpeg, python3, coreutils only)

All commands verified on the dev box (ffmpeg 8.0.1 with libx264, libx265, native `aac`,
`pcm_s16le`, `mpeg2video`, `mpeg4`, `libvpx`, `libopus`). Use `-f lavfi` sources so no
input media is needed. Fixtures are built by `tests/fixtures/build_video.sh` into
`target/fixtures/video/` (not committed).

### 5.1 Healthy references

```sh
V='-f lavfi -i testsrc2=size=320x240:rate=30'
A='-f lavfi -i sine=frequency=440:sample_rate=48000'
# H.264 + AAC, moov first (faststart), B-frames, 1 s GOP   → ref_h264.mp4 (≈330 KiB)
ffmpeg -y $V $A -t 3 -c:v libx264 -preset ultrafast -g 30 -bf 2 -pix_fmt yuv420p -c:a aac -b:a 128k -movflags +faststart ref_h264.mp4
# HEVC (hvc1 tag) + AAC, moov at end (default)
ffmpeg -y $V $A -t 3 -c:v libx265 -preset ultrafast -x265-params log-level=none -tag:v hvc1 -pix_fmt yuv420p -c:a aac -b:a 128k ref_h265.mp4
# QuickTime MOV with PCM audio (sowt), brand 'qt  ', wide+mdat, moov at end
ffmpeg -y $V $A -t 3 -c:v libx264 -preset ultrafast -pix_fmt yuv420p -c:a pcm_s16le ref_pcm.mov
# Stereo AAC (CPE elements) and 44.1 kHz variant
ffmpeg -y $V -f lavfi -i "sine=frequency=440:sample_rate=44100,aformat=channel_layouts=stereo" -t 3 -c:v libx264 -preset ultrafast -pix_fmt yuv420p -c:a aac -b:a 192k ref_stereo.mp4
# No B-frames (ctts absent) and 1080p (multi-chunk sizes closer to cameras)
ffmpeg -y -f lavfi -i testsrc2=size=1920x1080:rate=30 $A -t 4 -c:v libx264 -preset ultrafast -bf 0 -g 30 -pix_fmt yuv420p -c:a aac ref_1080_nobf.mp4
# Interlaced/field coding (edge case 10)
ffmpeg -y -f lavfi -i testsrc2=size=1920x1080:rate=30 -t 2 -c:v libx264 -preset ultrafast -flags +ildct+ilme -x264-params interlaced=1 -pix_fmt yuv420p ref_interlaced.mp4
# Fragmented MP4 (note-only path)
ffmpeg -y $V -t 2 -c:v libx264 -preset ultrafast -pix_fmt yuv420p -movflags frag_keyframe+empty_moov ref_frag.mp4
# AVI (MPEG-4 part 2 + PCM), with idx1 and dmlh
ffmpeg -y $V $A -t 3 -c:v mpeg4 -q:v 5 -c:a pcm_s16le ref.avi
# OpenDML AVI with AVIX chunks + indx/ix##: ffmpeg 8 has no option to shrink the RIFF limit
# (AVI_MAX_RIFF_SIZE = 1 GiB, and 'indx' is only materialised from its JUNK placeholder when a
# second RIFF is needed), so the OpenDML fixture is opt-in (DISKRESCUE_BIG_FIXTURES=1): raw video
# writes ~1.1 GiB in a few seconds without encoding.
ffmpeg -y -f lavfi -i color=c=gray:size=1920x1080:rate=30 $A -t 12 -c:v rawvideo -pix_fmt yuv420p -c:a pcm_s16le ref_odml_big.avi
# Small OpenDML fixture for unit tests: synthesise 'indx' + 'ix00' from ref.avi's idx1 with
# tests/tools/make_odml.py (writes a standard index: wLongsPerEntry 2, bIndexType 1, qwBaseOffset =
# file offset of the 'movi' tag, dwOffset = chunk data offset − base, bit 31 set on non-key frames).
# WebM/MKV (VP9 + Opus), known sizes; and a live-style file with unknown-size Segment/Clusters
ffmpeg -y $V $A -t 3 -c:v libvpx-vp9 -deadline realtime -cpu-used 8 -c:a libopus ref.webm
ffmpeg -y -i ref.webm -c copy -live 1 -f matroska live_unknown_size.mkv
ffmpeg -y $V $A -t 3 -c:v libx264 -preset ultrafast -pix_fmt yuv420p -c:a aac -f matroska ref_h264.mkv
# MPEG-TS (188) and M2TS (192, AVCHD-style)
ffmpeg -y $V $A -t 3 -c:v libx264 -preset ultrafast -pix_fmt yuv420p -c:a aac ref.ts
ffmpeg -y -i ref_h264.mp4 -c copy -f mpegts -mpegts_m2ts_mode 1 ref.m2ts
# MPEG-PS / VOB (MPEG-2 video + MP2), 2048-byte packs, no end code
ffmpeg -y $V $A -t 3 -c:v mpeg2video -q:v 5 -c:a mp2 -f vob ref.vob
```

Observed shapes of these files (use as test expectations): `ref_h264.mp4`: ftyp 32 B,
moov 4468 B at 32, free 8, mdat at 4508 (329740 B); video stsc `[(1,2),(2,1)]`, 90
samples, stss [1,31,61], ctts 63 entries; audio 142 frames, stsc 78 entries, stts
`[(141,1024),(1,640)]`, sgpd/sbgp roll. `ref_pcm.mov`: ftyp 20 B (`qt  `), wide 8, mdat
28.., moov at 583692; audio stsz fixed 2 B × 144000, stsc chunks of 2048/1024/…

### 5.2 Damaged fixtures

```sh
# 1. truncated, moov at end lost (case B)                    → needs reference rebuild
head -c 100000 ref_h265.mp4 > dmg_trunc_no_moov.mp4
# 2. truncated, moov first (case A: clip tables, fix mdat size)
head -c 200000 ref_h264.mp4 > dmg_trunc_faststart.mp4
# 3. moov stripped entirely (case B/D)
python3 - <<'EOF'
import struct
src=open('ref_h264.mp4','rb').read(); out=bytearray(); pos=0
while pos+8<=len(src):
    sz,typ=struct.unpack_from('>I4s',src,pos)
    if sz==1: sz=struct.unpack_from('>Q',src,pos+8)[0]
    elif sz==0: sz=len(src)-pos
    if typ!=b'moov': out+=src[pos:pos+sz]
    pos+=sz
open('dmg_nomoov.mp4','wb').write(out)
EOF
# 4. mdat size field zeroed (case C) — ffprobe still opens it; our validator must flag MdatSizeWrong
python3 - <<'EOF'
import struct
b=bytearray(open('ref_h264.mp4','rb').read()); pos=0
while pos+8<=len(b):
    sz,typ=struct.unpack_from('>I4s',b,pos)
    if typ==b'mdat': struct.pack_into('>I',b,pos,0); break
    pos+=sz
open('dmg_mdat_size0.mp4','wb').write(b)
EOF
# 5. zero-filled bad sectors inside mdat (4 KiB at 100000) and inside moov (512 B at 1300)
cp ref_h264.mp4 dmg_hole_mdat.mp4; dd if=/dev/zero of=dmg_hole_mdat.mp4 bs=1 seek=100000 count=4096 conv=notrunc
cp ref_h264.mp4 dmg_hole_moov.mp4; dd if=/dev/zero of=dmg_hole_moov.mp4 bs=1 seek=1300 count=512 conv=notrunc
# 6. head lost (case D): drop first 4096 bytes of the moov-at-end file
tail -c +4097 ref_h265.mp4 > dmg_headless.mp4
# 7. stale moov: moov of a *different* recording glued to this mdat (case G)
ffmpeg -y -f lavfi -i testsrc2=size=320x240:rate=30 -t 3 -c:v libx264 -preset ultrafast -g 15 -bf 0 -pix_fmt yuv420p -an -movflags +faststart other.mp4
python3 - <<'EOF'
import struct
def boxes(b):
    pos=0
    while pos+8<=len(b):
        sz,typ=struct.unpack_from('>I4s',b,pos); yield typ,pos,sz; pos+=sz
a=open('ref_h264.mp4','rb').read(); o=open('other.mp4','rb').read()
moov=[(p,s) for t,p,s in boxes(o) if t==b'moov'][0]
out=bytearray()
for t,p,s in boxes(a):
    out += o[moov[0]:moov[0]+moov[1]] if t==b'moov' else a[p:p+s]
open('dmg_stale_moov.mp4','wb').write(out)
EOF
# 8. AVI: index stripped and RIFF size zeroed; AVI truncated mid-movi
python3 - <<'EOF'
import struct
b=bytearray(open('ref.avi','rb').read()); i=b.rfind(b'idx1'); b=b[:i]; struct.pack_into('<I',b,4,0); open('dmg_noidx.avi','wb').write(b)
EOF
head -c 300000 ref.avi > dmg_trunc.avi
# 9. MKV truncated mid-cluster; TS with a 1500-byte junk run (breaks the 188 stride); VOB truncated mid-pack
head -c 150000 live_unknown_size.mkv > dmg_trunc.mkv
cp ref.ts dmg_junk.ts; dd if=/dev/urandom of=dmg_junk.ts bs=1 seek=100000 count=1500 conv=notrunc
head -c 300001 ref.vob > dmg_trunc.vob
# 10. carving image: several files back to back with sector gaps, plus a decoy 'ftyp' inside junk
{ cat ref_h264.mp4; head -c 4096 /dev/zero; cat ref.avi; head -c 512 /dev/zero; printf '\0\0\0\x20ftypjunk'; head -c 1000 /dev/urandom; cat ref.ts; cat ref.webm; cat ref.vob; cat ref_pcm.mov; } > carve_concat.img
```

### 5.3 What each test asserts

| Fixture | Expectation |
|---|---|
| `carve_concat.img` | 6 files found at exact offsets with exact lengths (byte-identical to sources); the decoy `ftyp` produces no file |
| `ref_*.mp4/.mov` | validate = Playable; sample map of every track consistent; sample 0 of video parses as IDR with `first_mb 0`; `derive_ctts` on the *healthy* mdat reproduces the file's own `ctts` table exactly (regression for POC logic: expect `[1024, 2048, 512, 512, …]` for `ref_h264.mp4`) |
| `dmg_trunc_no_moov.mp4`, `dmg_nomoov.mp4`, `dmg_headless.mp4` | rebuild with the matching `ref_*` as reference ⇒ `ffprobe -v error -show_entries stream=nb_frames,codec_name` succeeds, `ffmpeg -v error -i out.mp4 -f null -` decodes with **no errors**, video frame count = Σ accepted samples, audio frame count ≥ 95 % of video_duration × 46.875 |
| `dmg_trunc_faststart.mp4` | clip ⇒ decodes clean; `mdat` size patched; sample count == samples fitting in 200000 B minus 1 |
| `dmg_mdat_size0.mp4` | validator reports `MdatSizeWrong{claimed:0}`; repair patches size only (output byte-identical to `ref_h264.mp4`) |
| `dmg_hole_mdat.mp4` | validator reports `HolesInMdat` with the sample indices overlapping 100000..104096 (given a mapfile marking that range bad); rebuild skips the hole and output decodes with no errors except in that GOP |
| `dmg_hole_moov.mp4` | level-1 validation fails ⇒ rebuild path chosen automatically; output decodes clean |
| `dmg_stale_moov.mp4` | level-2 validation fails (chunk starts mid-NAL) ⇒ rebuild chosen |
| `dmg_noidx.avi`, `dmg_trunc.avi` | rebuilt `idx1` equals ffmpeg's original entries (ids/flags/offsets/sizes) for the surviving chunks; RIFF sizes and `dwTotalFrames` correct; ffprobe reports `nb_frames` = surviving video chunks |
| `dmg_trunc.mkv` | carve end = end of last complete SimpleBlock; `ffmpeg -f null` no errors |
| `dmg_junk.ts` | carver resyncs; output packet count = original − packets overlapped by junk |
| `dmg_trunc.vob` | end = last complete 2048-byte pack |

Round-trip oracle for the moov writer: rebuild from the **healthy** `ref_h264.mp4` using
itself as reference and compare the resulting sample map (offset,size,dts,cts,sync) with
the original's — must be identical except for the dropped `sgpd/sbgp/sdtp` boxes and the
final short AAC frame duration.

### 5.4 Deterministic, root-less, fast

All fixtures are < 1 MiB each; the build script checks `ffmpeg -encoders` for
`libx264|libx265|aac|libvpx-vp9` and skips (not fails) tests whose encoder is missing. Use
`-preset ultrafast`, `-t 3`. No network, no root, no ddrescue/photorec/exiftool/smartctl.

### 5.5 Reference parser for CI cross-checks

Keep the Python bit-reader used during this research (SPS/PPS/slice for AVC+HEVC, AAC
element header, box walker) under `tests/tools/ref_parse.py`; a test shells out to it with
`python3` and compares its JSON output with the Rust parser on every fixture
(`log2_max_frame_num_minus4`, `pic_order_cnt_type`, `log2_max_pic_order_cnt_lsb_minus4`,
`frame_mbs_only_flag`, `pic_width_in_mbs_minus1`, per-sample `first_mb_in_slice`,
`frame_num`, `pic_order_cnt_lsb`; HEVC `log2_max_pic_order_cnt_lsb_minus4`,
`CtbLog2SizeY`, `first_slice_segment_in_pic_flag`, `slice_pic_order_cnt_lsb`).

### 5.6 Verified values (from this research run; use as literal assertions)

- `ref_h264.mp4` avcC: `01 4d 40 0d ff e1 00 18 …`; SPS → profile 77, level 13,
  `log2_max_frame_num_minus4 0`, `poc_type 0`, `log2_max_pic_order_cnt_lsb_minus4 2`,
  `max_num_ref_frames 4`, 320×240, `frame_mbs_only 1`, VUI present. PPS: CAVLC,
  `bottom_field_pic_order_in_frame_present 0`. Sample 0 = SEI(655 B) + IDR(8055 B); sample 1
  = P `frame_num 1 poc_lsb 6`; sample 2 = B `frame_num 2 poc_lsb 2`.
- `ref_h265.mp4` hvcC: lengthSizeMinusOne 3, 4 arrays (VPS 24 B, SPS 42 B, PPS 6 B, SEI
  2338 B); SPS: profile 1, level 60, 320×240, `log2_max_pic_order_cnt_lsb_minus4 4`,
  `CtbLog2SizeY 5`; PPS: no dependent slices, 0 extra bits. Sample 0 = IDR_N_LP (20); sample 1
  = TRAIL_R `poc_lsb 4`; sample 3 = TRAIL_N `poc_lsb 1`.
- AAC ASC `11 88 56 e5 00` → LC, 48 kHz, mono, 1024, sync-ext SBR absent. Frames: 453 B
  (starts with FIL), 359 B (SCE, EIGHT_SHORT, max_sfb 14, last byte 0x38), 276 B (SCE,
  ONLY_LONG, max_sfb 48).
- `ref.avi`: RIFF size 663004 = len − 8; avih flags 0x910, 90 frames, 2 streams; strh
  `vids`/`FMP4` scale 1 rate 30 length 90; `movi` tag at 9978; idx1 at 659308 with 231
  entries, first `00dc 0x10 4 0x26AC`; `dmlh` present, no `indx`.
- `ref.ts`: `47 40 11 10` (PID 0x0011 SDT first), stride 188 holds; `ref.m2ts`: `0e bf 46 22`
  prefix then `47 40 11 10`, stride 192.
- `ref.vob`: `00 00 01 BA 44 …` MPEG-2 pack, system header `00 00 01 BB 00 0C`, 274 packs of
  2048 B, **no** `00 00 01 B9`.
- `ref.webm` Segment size known (`01 00 00 00 00 03 40 0F`); `live_unknown_size.mkv` Segment
  size `01 FF FF FF FF FF FF FF`, DocType `matroska`.

---

## 6 Recommended Rust crates

Versions are from `cargo search` on 2026-10-12 (so they are current, not guesses).

| Need | Crate | Version | Verdict |
|---|---|---|---|
| Box walking on damaged input | **hand-written** | — | `mp4` (0.14.0, alfg) and `mp4parse` (0.17.0, Mozilla) both abort on the first malformed box, need a complete `moov`, and `mp4parse` refuses `size==0`/odd layouts and does not expose raw `stsd`; neither can write a `moov` with arbitrary tables (`mp4` writes but only from its own `Mp4Config`). Our walker is ~300 lines and must be tolerant by design (§3.1). Use `mp4parse` **only in tests** as an oracle for healthy fixtures. |
| Bit reading (exp-Golomb, slice headers, AAC headers) | `bitstream-io` | 4.10.0 | pure Rust, BE/LE bit readers, well maintained; `bitreader` 0.3.11 is simpler but unmaintained (2+ years). Write `read_ue/read_se` on top (5 lines). |
| H.264 SPS/PPS/slice header | `h264-reader` | 0.9.0 | pure Rust, Annex B + AVC (`avcC`) support, parses SPS incl. scaling lists/VUI, PPS, and `SliceHeader` with `first_mb_in_slice`, `frame_num`, `pic_order_cnt_lsb`. Use it for SPS/PPS; for the slice header prefer the 40-line hand parser (§2.5) so that a truncated/garbage slice returns a cheap `None` instead of an error chain, and so we control exactly how many bytes are read. (0.9 `SliceHeader` exposes `first_mb_in_slice: u32`, `frame_num: u16`, `pic_order_cnt_lsb: Option<PicOrderCountLsb>`, `idr_pic_id: Option<u32>`, `field_pic`; `SliceHeader::from_bits(ctx, reader, nal_header, ext)` returns the header plus the matching SPS/PPS.) |
| HEVC | hand-written (§2.6) | — | no maintained pure-Rust HEVC header parser found (`oxideav-*` 0.0.x is pre-alpha). PTL/SPS/PPS/slice prefix is ~150 lines. |
| AAC decode oracle (optional feature) | `symphonia` (`symphonia-codec-aac`) | 0.6.1 | pure Rust AAC-LC decoder; use to validate candidate frame boundaries. Not needed for the default path. |
| Video decode oracle (optional, tests only) | `openh264` (links Cisco's C library, not pure) | 0.9.8 | or call `ffmpeg -f null -` from tests; avoid C deps in the shipped binary. |
| Matroska | `ebml-iterable` 0.7.1 / `matroska-demuxer` 0.8.1 / `matroska` 0.30.1 | | all assume well-formed input; `ebml-iterable` is the closest to a raw walker but a 30-line VINT/ID walker (§2.10) is simpler and tolerant. Hand-write. |
| AVI | `riff` 2.0.0 | | generic RIFF chunk reader; strict on sizes. Hand-write the walker (RIFF is trivial), reuse nothing. |
| MPEG-TS | `mpeg2ts-reader` 0.18.2 | | solid push parser (PAT/PMT/PES), pure Rust; usable for validation and PID discovery. Carving stride detection is hand-written. |
| MPEG-PS | hand-written | — | no suitable crate. |
| CRC-32 (MKV, PSI) | `crc` | 3.4.0 | MPEG-2 CRC = CRC-32/MPEG-2 (poly 0x04C11DB7, init 0xFFFFFFFF, no reflect, no xorout); Matroska CRC-32 = IEEE (reflected). |
| Memory-mapped IO | `memmap2` | 0.9.11 | for scanning multi-GB images; fall back to buffered `pread` on Windows oddities. |

Rule of thumb: **parsers in the repair path are hand-written, bounded, and return
`Option`/`Result` without panicking on any input**; third-party parsers are confined to tests
and to optional "oracle" features. Fuzz the box walker, the NAL/slice parsers and the AAC
header parser with `cargo-fuzz` against the fixtures.

---

## 7 Sources

Primary specifications
- ISO/IEC 14496-12 (ISO base media file format), box definitions §4.2, §8.2–8.7. Public
  copy of the 2015 edition: https://standards.iso.org/ittf/PubliclyAvailableStandards/ (search 14496-12).
- ISO/IEC 14496-15 (AVC/HEVC in ISO BMFF): `avcC` §5.3.3.1, `hvcC` §8.3.3.1.
- ISO/IEC 14496-1 §7.2.6 (ES_Descriptor, DecoderConfigDescriptor, expandable size).
- ISO/IEC 14496-3 §1.6.2.1 AudioSpecificConfig, §4.4.2 raw_data_block, §4.4.2.1 ics_info.
- ITU-T H.264 (08/2021) §7.3.2.1.1 SPS, §7.3.2.2 PPS, §7.3.3 slice header, §7.4.1.2.4
  first VCL NAL detection, §8.2.1 POC: https://www.itu.int/rec/T-REC-H.264
- ITU-T H.265 (V10) §7.3.1.2 NAL header, §7.3.2.2 SPS, §7.3.2.3 PPS, §7.3.3 PTL,
  §7.3.6.1 slice segment header, §8.3.1 POC: https://www.itu.int/rec/T-REC-H.265
- Apple QuickTime File Format Specification (sample description versions, `wide`, `qt  `
  brand, `tmcd`, `sowt`/`twos`/`lpcm`): https://developer.apple.com/documentation/quicktime-file-format
- MP4RA registered brands and codecs: https://mp4ra.org/registered-types/brands
- Microsoft AVI RIFF File Reference: https://learn.microsoft.com/en-us/windows/win32/directshow/avi-riff-file-reference
- AVIOLDINDEX (idx1 entry, dwOffset semantics, flags): https://learn.microsoft.com/en-us/previous-versions/windows/desktop/api/Aviriff/ns-aviriff-avioldindex
- AVIMETAINDEX / OpenDML index types: https://learn.microsoft.com/en-us/previous-versions/windows/desktop/api/aviriff/ns-aviriff-avimetaindex
- OpenDML AVI File Format Extensions v1.02 (AVISUPERINDEX, AVISTDINDEX, dmlh, AVIX).
- Matroska element IDs: https://www.matroska.org/technical/elements.html ; EBML RFC 8794: https://www.rfc-editor.org/rfc/rfc8794
- Matroska Block/SimpleBlock structure: https://www.matroska.org/technical/basics.html
- ISO/IEC 13818-1 (MPEG-2 Systems): TS packet §2.4.3.2, adaptation field §2.4.3.4, PAT §2.4.4.3,
  PMT §2.4.4.8, PES §2.4.3.6, pack header §2.5.3.3. Blu-ray/AVCHD 192-byte TP_extra_header:
  BD-ROM Part 3 (summarised in FFmpeg `libavformat/mpegts.c`, `mpegtsenc.c` `-mpegts_m2ts_mode`).

Repair tools modelled
- untrunc (Federico Ponchio): https://github.com/ponchio/untrunc — `codec_avc1.cpp`
  (slice header fields, §7.4.1.2.4 checks), `codec_hev1.cpp` (NAL header validation,
  `first_slice_segment_in_pic_flag`, keyframe types; note it omits IDR_N_LP from slices),
  `codec_mp4a.cpp` (libavcodec decode → consumed bytes; chances 32000 for 6-byte silence,
  50 for < 400 B, 100 otherwise; "horrible hack" byte pairs), `codec_pcm.cpp`
  (`stats.fixed_size`, chances 2), `codec_gpmd.cpp` (GoPro KLV fourcc set, `(be32+4 & 0xffff)+8`).
- untrunc (anthwlock fork, actively maintained): https://github.com/anthwlock/untrunc —
  `src/mp4.cpp` (repair loop, `chkOffset`, `-s` skip-unknown, `-dyn` dynamic chunk stats,
  `track_order_`, 64-bit `co64` when > 4 GiB, output order ftyp+moov+mdat, pruning of
  `cslg`/`stps`), `src/avc1/nal-slice.cpp` and `sps-info.cpp` (exact SPS fields parsed,
  `isInNewFrame` rules and which are strict-only), `src/avc1/avc1.cpp` (AU termination on
  SPS/AUD/other NAL after a slice, filler handling, ref_idc rule), `src/hvc1/hvc1.cpp`
  (layer/temporal id consistency, prefix SEI ends AU, alternative lengths), `src/mutual_pattern.cpp`
  (first-bytes pattern intersection), README (GoPro/Sony XAVC support, >2 GB, twos/sowt fixed-width).
- recoverMP4 (Aero Quartet) behaviour description (reference-file approach, moov rebuild):
  https://www.aeroquartet.com/movierepair/recover (product page; algorithm details not public).
- FFmpeg `libavformat/mov.c`, `movenc.c` (how `stsc`/`elst`/`ctts` are written; `-movflags`),
  `avienc.c`/`avidec.c` (idx1 offsets, OpenDML `ix##`), `matroskaenc.c` (`-live`),
  `mpegtsenc.c` (`mpegts_m2ts_mode`), `mpegenc.c` (vob packs), `libavcodec/aactab.c`
  (`ff_aac_num_swb_1024/128/960`): https://github.com/FFmpeg/FFmpeg
- h264-reader 0.9 `SliceHeader` API: https://docs.rs/h264-reader/latest/h264_reader/nal/slice/struct.SliceHeader.html
- MPEG-4 Audio (AudioSpecificConfig, frequency index table): https://wiki.multimedia.cx/index.php/MPEG-4_Audio

Crate versions (cargo search, 2026-10-12): mp4 0.14.0, mp4parse 0.17.0, symphonia 0.6.1,
bitreader 0.3.11, bitstream-io 4.10.0, h264-reader 0.9.0, matroska 0.30.1, matroska-demuxer
0.8.1, ebml-iterable 0.7.1, riff 2.0.0, mpeg2ts-reader 0.18.2.

Local verification (this document): ffmpeg 8.0.1-3ubuntu2 output parsed with an independent
Python bit-reader; all "Observed"/"Verified" statements above come from those runs.
