//! Parsed `moov` tree: movie header, tracks, sample descriptions, sample
//! tables and the raw boxes a writer needs to copy verbatim.

use diskrescue_core::time::Timestamp;
use diskrescue_core::util::bytes::{u16be, u32be, u64be};
use diskrescue_core::{Error, Result, Source};

use super::meta::{parse_meta_box, parse_udta, MetaInfo};
use super::{full_box_header, read_payload, BoxHeader, BoxIter, SliceBox, MAX_MOOV_SIZE};

/// Movie header (`mvhd`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Mvhd {
    /// FullBox version (0 or 1).
    pub version: u8,
    /// Seconds since 1904-01-01 UTC.
    pub creation_time: u64,
    /// Seconds since 1904-01-01 UTC.
    pub modification_time: u64,
    /// Ticks per second of the movie time scale.
    pub timescale: u32,
    /// Duration in movie ticks (all ones = unknown).
    pub duration: u64,
    /// Preferred rate, 16.16 fixed.
    pub rate: u32,
    /// Preferred volume, 8.8 fixed.
    pub volume: u16,
    /// Transformation matrix.
    pub matrix: [u32; 9],
    /// Next track id.
    pub next_track_id: u32,
}

impl Mvhd {
    /// Creation time as a timestamp (None when zero or implausible).
    pub fn created(&self) -> Option<Timestamp> {
        Timestamp::from_mp4(self.creation_time).filter(|t| t.is_plausible())
    }

    /// Duration in milliseconds, when known.
    pub fn duration_ms(&self) -> Option<u64> {
        let unknown = if self.version == 1 {
            u64::MAX
        } else {
            u32::MAX as u64
        };
        if self.timescale == 0 || self.duration == unknown {
            return None;
        }
        Some((self.duration as u128 * 1000 / self.timescale as u128) as u64)
    }
}

/// Track header (`tkhd`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Tkhd {
    /// FullBox version.
    pub version: u8,
    /// Flags (0x1 enabled, 0x2 in movie, 0x4 in preview, 0x8 in poster).
    pub flags: u32,
    /// Seconds since 1904.
    pub creation_time: u64,
    /// Seconds since 1904.
    pub modification_time: u64,
    /// Track id (non-zero).
    pub track_id: u32,
    /// Duration in movie ticks.
    pub duration: u64,
    /// Layer.
    pub layer: i16,
    /// Alternate group.
    pub alternate_group: i16,
    /// Volume 8.8.
    pub volume: i16,
    /// Matrix (rotation).
    pub matrix: [u32; 9],
    /// Width, 16.16 fixed.
    pub width_fixed: u32,
    /// Height, 16.16 fixed.
    pub height_fixed: u32,
}

impl Tkhd {
    /// Integer display width.
    pub fn width(&self) -> u32 {
        self.width_fixed >> 16
    }
    /// Integer display height.
    pub fn height(&self) -> u32 {
        self.height_fixed >> 16
    }
}

/// Media header (`mdhd`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Mdhd {
    /// FullBox version.
    pub version: u8,
    /// Seconds since 1904.
    pub creation_time: u64,
    /// Seconds since 1904.
    pub modification_time: u64,
    /// Media ticks per second.
    pub timescale: u32,
    /// Media duration in ticks.
    pub duration: u64,
    /// Packed ISO-639-2 language code (or a QuickTime language number).
    pub language_code: u16,
    /// Decoded language (`und`, `eng`, or `qt:N`).
    pub language: String,
    /// QuickTime quality / ISO pre_defined.
    pub quality: u16,
}

/// One edit list entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Elst {
    /// Duration in movie ticks.
    pub segment_duration: u64,
    /// Start in media ticks, -1 for an empty edit.
    pub media_time: i64,
    /// Rate integer part.
    pub rate_integer: i16,
    /// Rate fraction.
    pub rate_fraction: i16,
}

/// Sample sizes from `stsz` / `stz2`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SampleSizes {
    /// Every sample has the same size.
    Constant {
        /// Size of each sample.
        size: u32,
        /// Number of samples.
        count: u32,
    },
    /// Per-sample sizes.
    Table(Vec<u32>),
}

impl Default for SampleSizes {
    fn default() -> Self {
        SampleSizes::Table(Vec::new())
    }
}

impl SampleSizes {
    /// Number of samples.
    pub fn count(&self) -> u32 {
        match self {
            SampleSizes::Constant { count, .. } => *count,
            SampleSizes::Table(v) => v.len() as u32,
        }
    }
    /// Size of sample `i` (0-based).
    pub fn get(&self, i: u32) -> Option<u32> {
        match self {
            SampleSizes::Constant { size, count } => (i < *count).then_some(*size),
            SampleSizes::Table(v) => v.get(i as usize).copied(),
        }
    }
}

/// H.264 decoder configuration (`avcC`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AvcC {
    /// AVCProfileIndication.
    pub profile: u8,
    /// profile_compatibility.
    pub compatibility: u8,
    /// AVCLevelIndication.
    pub level: u8,
    /// Bytes in each NAL length prefix (1, 2 or 4).
    pub nal_length_size: u8,
    /// Sequence parameter set NAL units (with NAL header).
    pub sps: Vec<Vec<u8>>,
    /// Picture parameter set NAL units (with NAL header).
    pub pps: Vec<Vec<u8>>,
}

/// HEVC decoder configuration (`hvcC`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HvcC {
    /// general_profile_idc.
    pub profile_idc: u8,
    /// general_level_idc.
    pub level_idc: u8,
    /// Bytes in each NAL length prefix.
    pub nal_length_size: u8,
    /// VPS NAL units.
    pub vps: Vec<Vec<u8>>,
    /// SPS NAL units.
    pub sps: Vec<Vec<u8>>,
    /// PPS NAL units.
    pub pps: Vec<Vec<u8>>,
    /// SEI NAL units carried in the record.
    pub sei: Vec<Vec<u8>>,
}

/// Elementary stream descriptor (`esds`) of an MPEG-4 audio/visual entry.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Esds {
    /// objectTypeIndication (0x40 = MPEG-4 audio / AAC).
    pub object_type: u8,
    /// DecoderSpecificInfo (the AudioSpecificConfig for AAC).
    pub audio_specific_config: Vec<u8>,
    /// Channel configuration from the ASC (0 when unknown).
    pub channels: u8,
    /// Sampling rate from the ASC (0 when unknown).
    pub sample_rate: u32,
    /// Maximum bitrate.
    pub max_bitrate: u32,
    /// Average bitrate.
    pub avg_bitrate: u32,
}

/// One sample description.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SampleEntry {
    /// Entry type (`avc1`, `hvc1`, `mp4a`, `sowt`, `CRAW` ...).
    pub kind: [u8; 4],
    /// The complete entry box (header included), copied verbatim by writers.
    pub raw: Vec<u8>,
    /// data_reference_index.
    pub data_reference_index: u16,
    /// Parsed `avcC`.
    pub avcc: Option<AvcC>,
    /// Parsed `hvcC`.
    pub hvcc: Option<HvcC>,
    /// Parsed `esds`.
    pub esds: Option<Esds>,
    /// Coded width (visual entries).
    pub width: u16,
    /// Coded height (visual entries).
    pub height: u16,
    /// Channel count (audio entries).
    pub channels: u16,
    /// Bits per sample (audio entries).
    pub sample_size_bits: u16,
    /// Sampling rate in Hz (audio entries).
    pub sample_rate: u32,
    /// QuickTime sound description version (0, 1, 2).
    pub qt_audio_version: u16,
    /// Bytes per PCM frame for uncompressed audio, when derivable.
    pub bytes_per_frame: Option<u32>,
}

impl SampleEntry {
    /// Short codec name for reports (`h264`, `hevc`, `aac`, `pcm_s16le` ...).
    pub fn codec_name(&self) -> String {
        codec_name(&self.kind, self.esds.as_ref().map(|e| e.object_type))
    }
}

/// Codec name for a sample entry type.
pub fn codec_name(kind: &[u8; 4], object_type: Option<u8>) -> String {
    match kind {
        b"avc1" | b"avc2" | b"avc3" | b"avc4" => "h264".into(),
        b"hvc1" | b"hev1" | b"dvh1" | b"dvhe" => "hevc".into(),
        b"av01" => "av1".into(),
        b"vp08" => "vp8".into(),
        b"vp09" => "vp9".into(),
        b"mp4v" => match object_type {
            Some(0x6A) | Some(0x61) | Some(0x60) => "mpeg2video".into(),
            Some(0x6C) => "mjpeg".into(),
            _ => "mpeg4".into(),
        },
        b"s263" | b"h263" => "h263".into(),
        b"jpeg" | b"mjpa" | b"mjpb" => "mjpeg".into(),
        b"apch" | b"apcn" | b"apcs" | b"apco" | b"ap4h" | b"ap4x" => "prores".into(),
        b"mp4a" => match object_type {
            Some(0x69) | Some(0x6B) => "mp3".into(),
            _ => "aac".into(),
        },
        b"sowt" => "pcm_s16le".into(),
        b"twos" => "pcm_s16be".into(),
        b"lpcm" => "pcm".into(),
        b"in24" => "pcm_s24".into(),
        b"in32" => "pcm_s32".into(),
        b"fl32" => "pcm_f32".into(),
        b"fl64" => "pcm_f64".into(),
        b"raw " => "pcm_u8".into(),
        b"ulaw" => "pcm_mulaw".into(),
        b"alaw" => "pcm_alaw".into(),
        b"ac-3" => "ac3".into(),
        b"ec-3" => "eac3".into(),
        b"alac" => "alac".into(),
        b"Opus" => "opus".into(),
        b"fLaC" => "flac".into(),
        b"samr" => "amr_nb".into(),
        b"sawb" => "amr_wb".into(),
        b".mp3" => "mp3".into(),
        b"CRAW" => "craw".into(),
        _ => super::fourcc_str(kind).trim().to_ascii_lowercase(),
    }
}

/// Raw boxes of a track that writers copy verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TrakRaw {
    /// The `mdia/hdlr` box (with header).
    pub hdlr: Vec<u8>,
    /// Children of `minf` other than `stbl` (`vmhd`/`smhd`/`gmhd`, `dinf`,
    /// QuickTime data `hdlr`), each with its header, in file order.
    pub minf_boxes: Vec<Vec<u8>>,
    /// The `tref` box, when present.
    pub tref: Option<Vec<u8>>,
}

/// One track.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Trak {
    /// Track header.
    pub tkhd: Tkhd,
    /// Media header.
    pub mdhd: Mdhd,
    /// Handler type (`vide`, `soun`, `meta`, `tmcd`, `text` ...).
    pub handler: [u8; 4],
    /// Handler name.
    pub handler_name: String,
    /// Sample descriptions.
    pub stsd: Vec<SampleEntry>,
    /// Decoding time deltas, run-length coded `(count, delta)`.
    pub stts: Vec<(u32, u32)>,
    /// Composition offsets `(count, offset)`, when present.
    pub ctts: Option<Vec<(u32, i32)>>,
    /// Version of the `ctts` box (1 = signed offsets).
    pub ctts_version: u8,
    /// Sample-to-chunk runs `(first_chunk, samples_per_chunk, sdi)`, 1-based.
    pub stsc: Vec<(u32, u32, u32)>,
    /// Sample sizes.
    pub stsz: SampleSizes,
    /// Chunk offsets (`stco` and `co64` merged).
    pub stco: Vec<u64>,
    /// True when the offsets came from `co64`.
    pub uses_co64: bool,
    /// Sync samples (1-based); `None` means every sample is a sync sample.
    pub stss: Option<Vec<u32>>,
    /// Edit list.
    pub elst: Option<Vec<Elst>>,
    /// Media time scale (copy of `mdhd.timescale`).
    pub timescale: u32,
    /// Media duration (copy of `mdhd.duration`).
    pub duration: u64,
    /// Language (copy of `mdhd.language`).
    pub language: String,
    /// Raw boxes for writers.
    pub raw: TrakRaw,
    /// Problems found while parsing this track.
    pub problems: Vec<String>,
}

impl Trak {
    /// True for video tracks.
    pub fn is_video(&self) -> bool {
        &self.handler == b"vide"
    }
    /// True for audio tracks.
    pub fn is_audio(&self) -> bool {
        &self.handler == b"soun"
    }
    /// First sample description.
    pub fn entry(&self) -> Option<&SampleEntry> {
        self.stsd.first()
    }
    /// Codec name of the first sample description.
    pub fn codec(&self) -> Option<String> {
        self.entry().map(SampleEntry::codec_name)
    }
    /// Media duration in milliseconds.
    pub fn duration_ms(&self) -> Option<u64> {
        if self.timescale == 0 {
            return None;
        }
        Some((self.duration as u128 * 1000 / self.timescale as u128) as u64)
    }
}

/// Canon metadata found in `moov/uuid 85c0b687-820f-11e0-8111-f4ce462b6a48`
/// (CR3 files and Canon MOV/MP4 movies).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CanonInfo {
    /// `CNCV` compressor version string.
    pub cncv: Option<String>,
    /// `CTBO` records `(index, offset, size)`.
    pub ctbo: Vec<(u32, u64, u64)>,
    /// Absolute payload ranges `(offset, len)` of `CMT1`..`CMT4` (TIFF).
    pub cmt: [Option<(u64, u64)>; 4],
    /// `THMB` JPEG `(offset, len, width, height)`.
    pub thmb: Option<(u64, u64, u16, u16)>,
    /// `CNTH/CNDA` JPEG thumbnail `(offset, len)` (Canon movies).
    pub cnth: Option<(u64, u64)>,
}

/// Canon metadata uuid inside `moov`.
pub const CANON_MOOV_UUID: [u8; 16] = [
    0x85, 0xc0, 0xb6, 0x87, 0x82, 0x0f, 0x11, 0xe0, 0x81, 0x11, 0xf4, 0xce, 0x46, 0x2b, 0x6a, 0x48,
];

/// The parsed movie box.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Moov {
    /// Movie header.
    pub mvhd: Mvhd,
    /// Tracks in file order.
    pub tracks: Vec<Trak>,
    /// Metadata from `udta` and `meta`.
    pub udta_meta: MetaInfo,
    /// True when `mvex` (fragmented movie) is present.
    pub has_mvex: bool,
    /// Raw `udta` / `meta` boxes (with headers) for writers that preserve
    /// user data.
    pub user_boxes: Vec<Vec<u8>>,
    /// Canon metadata, when present.
    pub canon: Option<CanonInfo>,
    /// Problems found while parsing.
    pub problems: Vec<String>,
}

impl Moov {
    /// First video track.
    pub fn video(&self) -> Option<&Trak> {
        self.tracks.iter().find(|t| t.is_video())
    }
    /// First audio track.
    pub fn audio(&self) -> Option<&Trak> {
        self.tracks.iter().find(|t| t.is_audio())
    }
}

/// Read the `moov` box from `src` (bounded by [`MAX_MOOV_SIZE`]) and parse
/// it. A `moov` that runs past the end of the source is parsed as far as
/// it goes.
pub fn parse_moov(src: &dyn Source, moov: &BoxHeader) -> Result<Moov> {
    let buf = read_payload(src, moov, src.size(), MAX_MOOV_SIZE)?;
    parse_moov_bytes(&buf, moov.payload_offset())
}

/// Parse a `moov` payload held in memory; `base` is the absolute file
/// offset of `payload[0]` (used for nested-object ranges).
pub fn parse_moov_bytes(payload: &[u8], base: u64) -> Result<Moov> {
    let mut moov = Moov::default();
    let mut have_mvhd = false;
    for b in BoxIter::new(payload, base).take(100_000) {
        if b.truncated {
            moov.problems
                .push(format!("'{}' box truncated", b.header.kind_str()));
        }
        match &b.header.kind {
            b"mvhd" => {
                moov.mvhd = parse_mvhd(b.payload)?;
                have_mvhd = true;
            }
            b"trak" => match parse_trak(&b) {
                Ok(t) => {
                    if moov.tracks.len() < 1024 {
                        moov.tracks.push(t)
                    }
                }
                Err(e) => moov.problems.push(format!("track skipped: {e}")),
            },
            b"udta" => {
                parse_udta(b.payload, b.header.payload_offset(), &mut moov.udta_meta);
                if let Some(cnth) = find_cnth(b.payload, b.header.payload_offset()) {
                    moov.canon.get_or_insert_with(CanonInfo::default).cnth = Some(cnth);
                }
                moov.user_boxes.push(b.raw.to_vec());
            }
            b"meta" => {
                parse_meta_box(b.payload, b.header.payload_offset(), &mut moov.udta_meta);
                moov.user_boxes.push(b.raw.to_vec());
            }
            b"mvex" => moov.has_mvex = true,
            b"uuid" if b.header.uuid == Some(CANON_MOOV_UUID) => {
                moov.canon = Some(parse_canon_uuid(b.payload, b.header.payload_offset()));
            }
            _ => {}
        }
    }
    if !have_mvhd {
        return Err(Error::corrupt("moov box", "mvhd missing"));
    }
    if moov.mvhd.timescale == 0 {
        return Err(Error::corrupt(
            "moov box",
            "mvhd timescale is 0 (zeroed moov)",
        ));
    }
    Ok(moov)
}

fn fixed_matrix(p: &[u8], off: usize) -> [u32; 9] {
    let mut m = [0u32; 9];
    for (i, v) in m.iter_mut().enumerate() {
        *v = u32be(p, off + 4 * i).unwrap_or(0);
    }
    m
}

fn short(what: &'static str) -> Error {
    Error::corrupt(what, "box too short")
}

fn parse_mvhd(p: &[u8]) -> Result<Mvhd> {
    let (version, _) = full_box_header(p).ok_or_else(|| short("mvhd"))?;
    let (ct, mt, ts, dur, rest) = if version == 1 {
        (
            u64be(p, 4),
            u64be(p, 12),
            u32be(p, 20),
            u64be(p, 24),
            32usize,
        )
    } else {
        (
            u32be(p, 4).map(u64::from),
            u32be(p, 8).map(u64::from),
            u32be(p, 12),
            u32be(p, 16).map(u64::from),
            20usize,
        )
    };
    let next = u32be(p, rest + 76).unwrap_or(0);
    Ok(Mvhd {
        version,
        creation_time: ct.ok_or_else(|| short("mvhd"))?,
        modification_time: mt.ok_or_else(|| short("mvhd"))?,
        timescale: ts.ok_or_else(|| short("mvhd"))?,
        duration: dur.ok_or_else(|| short("mvhd"))?,
        rate: u32be(p, rest).unwrap_or(0x0001_0000),
        volume: u16be(p, rest + 4).unwrap_or(0x0100),
        matrix: if p.len() >= rest + 52 {
            fixed_matrix(p, rest + 16)
        } else {
            IDENTITY
        },
        next_track_id: next,
    })
}

/// The identity transformation matrix.
pub const IDENTITY: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];

fn parse_tkhd(p: &[u8]) -> Result<Tkhd> {
    let (version, flags) = full_box_header(p).ok_or_else(|| short("tkhd"))?;
    let (ct, mt, id, dur, rest) = if version == 1 {
        (
            u64be(p, 4),
            u64be(p, 12),
            u32be(p, 20),
            u64be(p, 28),
            36usize,
        )
    } else {
        (
            u32be(p, 4).map(u64::from),
            u32be(p, 8).map(u64::from),
            u32be(p, 12),
            u32be(p, 20).map(u64::from),
            24usize,
        )
    };
    // rest: reserved 8, layer 2, alt 2, volume 2, reserved 2, matrix 36, w 4, h 4
    Ok(Tkhd {
        version,
        flags,
        creation_time: ct.ok_or_else(|| short("tkhd"))?,
        modification_time: mt.ok_or_else(|| short("tkhd"))?,
        track_id: id.ok_or_else(|| short("tkhd"))?,
        duration: dur.ok_or_else(|| short("tkhd"))?,
        layer: u16be(p, rest + 8).unwrap_or(0) as i16,
        alternate_group: u16be(p, rest + 10).unwrap_or(0) as i16,
        volume: u16be(p, rest + 12).unwrap_or(0) as i16,
        matrix: if p.len() >= rest + 52 {
            fixed_matrix(p, rest + 16)
        } else {
            IDENTITY
        },
        width_fixed: u32be(p, rest + 52).unwrap_or(0),
        height_fixed: u32be(p, rest + 56).unwrap_or(0),
    })
}

/// Decode a packed `mdhd` language code.
pub fn decode_language(code: u16) -> String {
    if code == 0 || code == 0x7FFF || code < 0x400 {
        return if code == 0 || code == 0x7FFF {
            "und".into()
        } else {
            format!("qt:{code}")
        };
    }
    let c = |s: u16| (((code >> s) & 0x1F) as u8 + 0x60) as char;
    let s: String = [c(10), c(5), c(0)].iter().collect();
    if s.chars().all(|ch| ch.is_ascii_lowercase()) {
        s
    } else {
        "und".into()
    }
}

fn parse_mdhd(p: &[u8]) -> Result<Mdhd> {
    let (version, _) = full_box_header(p).ok_or_else(|| short("mdhd"))?;
    let (ct, mt, ts, dur, rest) = if version == 1 {
        (
            u64be(p, 4),
            u64be(p, 12),
            u32be(p, 20),
            u64be(p, 24),
            32usize,
        )
    } else {
        (
            u32be(p, 4).map(u64::from),
            u32be(p, 8).map(u64::from),
            u32be(p, 12),
            u32be(p, 16).map(u64::from),
            20usize,
        )
    };
    let lang = u16be(p, rest).unwrap_or(0x55C4);
    Ok(Mdhd {
        version,
        creation_time: ct.ok_or_else(|| short("mdhd"))?,
        modification_time: mt.ok_or_else(|| short("mdhd"))?,
        timescale: ts.ok_or_else(|| short("mdhd"))?,
        duration: dur.ok_or_else(|| short("mdhd"))?,
        language_code: lang,
        language: decode_language(lang),
        quality: u16be(p, rest + 2).unwrap_or(0),
    })
}

fn parse_hdlr(p: &[u8]) -> ([u8; 4], String) {
    let handler: [u8; 4] = p
        .get(8..12)
        .and_then(|s| s.try_into().ok())
        .unwrap_or([0; 4]);
    let name_bytes = p.get(24..).unwrap_or(&[]);
    let name = if let Some((&len, rest)) = name_bytes.split_first() {
        // QuickTime Pascal string when the length byte matches.
        if len as usize == rest.len()
            || (len as usize) < rest.len() && rest.len() - 1 == len as usize
        {
            String::from_utf8_lossy(&rest[..(len as usize).min(rest.len())]).into_owned()
        } else {
            let end = name_bytes
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(name_bytes.len());
            String::from_utf8_lossy(&name_bytes[..end]).into_owned()
        }
    } else {
        String::new()
    };
    (handler, name)
}

fn parse_elst(p: &[u8], problems: &mut Vec<String>) -> Vec<Elst> {
    let Some((version, _)) = full_box_header(p) else {
        return Vec::new();
    };
    let count = u32be(p, 4).unwrap_or(0) as usize;
    let esz = if version == 1 { 20 } else { 12 };
    let avail = p.len().saturating_sub(8) / esz;
    if count > avail {
        problems.push("elst truncated".into());
    }
    let mut out = Vec::new();
    for i in 0..count.min(avail).min(4096) {
        let o = 8 + i * esz;
        let e = if version == 1 {
            Elst {
                segment_duration: u64be(p, o).unwrap_or(0),
                media_time: u64be(p, o + 8).unwrap_or(0) as i64,
                rate_integer: u16be(p, o + 16).unwrap_or(1) as i16,
                rate_fraction: u16be(p, o + 18).unwrap_or(0) as i16,
            }
        } else {
            Elst {
                segment_duration: u32be(p, o).unwrap_or(0) as u64,
                media_time: u32be(p, o + 4).unwrap_or(0) as i32 as i64,
                rate_integer: u16be(p, o + 8).unwrap_or(1) as i16,
                rate_fraction: u16be(p, o + 10).unwrap_or(0) as i16,
            }
        };
        out.push(e);
    }
    out
}

/// Entry count of a table box and the number of whole entries actually
/// present (clipped to the payload).
fn table_count(
    p: &[u8],
    header_len: usize,
    entry_len: usize,
    name: &str,
    problems: &mut Vec<String>,
) -> usize {
    let count = u32be(p, header_len - 4).unwrap_or(0) as usize;
    let avail = p.len().saturating_sub(header_len) / entry_len;
    if count > avail {
        problems.push(format!(
            "{name} truncated: {count} entries claimed, {avail} present"
        ));
    }
    count.min(avail)
}

fn parse_pairs(p: &[u8], name: &str, problems: &mut Vec<String>) -> Vec<(u32, u32)> {
    let n = table_count(p, 8, 8, name, problems);
    (0..n)
        .map(|i| {
            (
                u32be(p, 8 + i * 8).unwrap_or(0),
                u32be(p, 12 + i * 8).unwrap_or(0),
            )
        })
        .collect()
}

fn parse_u32_list(p: &[u8], name: &str, problems: &mut Vec<String>) -> Vec<u32> {
    let n = table_count(p, 8, 4, name, problems);
    (0..n).map(|i| u32be(p, 8 + i * 4).unwrap_or(0)).collect()
}

fn parse_stsz(p: &[u8], problems: &mut Vec<String>) -> SampleSizes {
    let size = u32be(p, 4).unwrap_or(0);
    let count = u32be(p, 8).unwrap_or(0);
    if size != 0 {
        return SampleSizes::Constant { size, count };
    }
    let n = table_count(p, 12, 4, "stsz", problems);
    SampleSizes::Table((0..n).map(|i| u32be(p, 12 + i * 4).unwrap_or(0)).collect())
}

fn parse_stz2(p: &[u8], problems: &mut Vec<String>) -> SampleSizes {
    let field = p.get(7).copied().unwrap_or(0) as usize;
    let count = u32be(p, 8).unwrap_or(0) as usize;
    let data = p.get(12..).unwrap_or(&[]);
    let avail = match field {
        4 => data.len() * 2,
        8 => data.len(),
        16 => data.len() / 2,
        _ => {
            problems.push(format!("stz2 field size {field} invalid"));
            0
        }
    };
    if count > avail {
        problems.push("stz2 truncated".into());
    }
    let n = count.min(avail);
    let v = (0..n)
        .map(|i| match field {
            4 => {
                let b = data[i / 2];
                if i % 2 == 0 {
                    (b >> 4) as u32
                } else {
                    (b & 0xF) as u32
                }
            }
            8 => data[i] as u32,
            _ => u16be(data, i * 2).unwrap_or(0) as u32,
        })
        .collect();
    SampleSizes::Table(v)
}

fn parse_trak(b: &SliceBox<'_>) -> Result<Trak> {
    let mut t = Trak::default();
    let mut have_tkhd = false;
    let mut have_mdia = false;
    for c in BoxIter::new(b.payload, b.header.payload_offset()).take(10_000) {
        match &c.header.kind {
            b"tkhd" => {
                t.tkhd = parse_tkhd(c.payload)?;
                have_tkhd = true;
            }
            b"edts" => {
                for e in BoxIter::new(c.payload, c.header.payload_offset()).take(16) {
                    if &e.header.kind == b"elst" {
                        t.elst = Some(parse_elst(e.payload, &mut t.problems));
                    }
                }
            }
            b"tref" => t.raw.tref = Some(c.raw.to_vec()),
            b"mdia" => {
                have_mdia = true;
                parse_mdia(&c, &mut t)?;
            }
            _ => {}
        }
    }
    if !have_tkhd {
        return Err(Error::corrupt("trak box", "tkhd missing"));
    }
    if !have_mdia {
        return Err(Error::corrupt("trak box", "mdia missing"));
    }
    t.timescale = t.mdhd.timescale;
    t.duration = t.mdhd.duration;
    t.language = t.mdhd.language.clone();
    Ok(t)
}

fn parse_mdia(b: &SliceBox<'_>, t: &mut Trak) -> Result<()> {
    let mut minf: Option<SliceBox<'_>> = None;
    let mut have_mdhd = false;
    for c in BoxIter::new(b.payload, b.header.payload_offset()).take(1000) {
        match &c.header.kind {
            b"mdhd" => {
                t.mdhd = parse_mdhd(c.payload)?;
                have_mdhd = true;
            }
            b"hdlr" => {
                let (h, n) = parse_hdlr(c.payload);
                t.handler = h;
                t.handler_name = n;
                t.raw.hdlr = c.raw.to_vec();
            }
            b"minf" => minf = Some(c),
            _ => {}
        }
    }
    if !have_mdhd {
        return Err(Error::corrupt("mdia box", "mdhd missing"));
    }
    let Some(minf) = minf else {
        t.problems.push("minf missing".into());
        return Ok(());
    };
    let mut have_stbl = false;
    for c in BoxIter::new(minf.payload, minf.header.payload_offset()).take(1000) {
        if &c.header.kind == b"stbl" {
            have_stbl = true;
            parse_stbl(&c, t);
        } else {
            t.raw.minf_boxes.push(c.raw.to_vec());
        }
    }
    if !have_stbl {
        t.problems.push("stbl missing".into());
    }
    Ok(())
}

fn parse_stbl(b: &SliceBox<'_>, t: &mut Trak) {
    let mut have = [false; 4]; // stsd, stts, stsc, stsz
    let mut have_co = false;
    for c in BoxIter::new(b.payload, b.header.payload_offset()).take(1000) {
        if c.truncated {
            t.problems
                .push(format!("'{}' box truncated", c.header.kind_str()));
        }
        let p = c.payload;
        match &c.header.kind {
            b"stsd" => {
                have[0] = true;
                t.stsd = parse_stsd(&c, &t.handler, &mut t.problems);
            }
            b"stts" => {
                have[1] = true;
                t.stts = parse_pairs(p, "stts", &mut t.problems);
            }
            b"ctts" => {
                t.ctts_version = p.first().copied().unwrap_or(0);
                t.ctts = Some(
                    parse_pairs(p, "ctts", &mut t.problems)
                        .into_iter()
                        .map(|(c, o)| (c, o as i32))
                        .collect(),
                );
            }
            b"stsc" => {
                have[2] = true;
                let n = table_count(p, 8, 12, "stsc", &mut t.problems);
                t.stsc = (0..n)
                    .map(|i| {
                        (
                            u32be(p, 8 + i * 12).unwrap_or(0),
                            u32be(p, 12 + i * 12).unwrap_or(0),
                            u32be(p, 16 + i * 12).unwrap_or(0),
                        )
                    })
                    .collect();
            }
            b"stsz" => {
                have[3] = true;
                t.stsz = parse_stsz(p, &mut t.problems);
            }
            b"stz2" => {
                have[3] = true;
                t.stsz = parse_stz2(p, &mut t.problems);
            }
            b"stco" => {
                have_co = true;
                t.stco = parse_u32_list(p, "stco", &mut t.problems)
                    .into_iter()
                    .map(u64::from)
                    .collect();
            }
            b"co64" => {
                have_co = true;
                t.uses_co64 = true;
                let n = table_count(p, 8, 8, "co64", &mut t.problems);
                t.stco = (0..n).map(|i| u64be(p, 8 + i * 8).unwrap_or(0)).collect();
            }
            b"stss" => t.stss = Some(parse_u32_list(p, "stss", &mut t.problems)),
            _ => {}
        }
    }
    for (ok, name) in have.iter().zip(["stsd", "stts", "stsc", "stsz"]) {
        if !ok {
            t.problems.push(format!("{name} missing"));
        }
    }
    if !have_co {
        t.problems.push("stco/co64 missing".into());
    }
}

const VISUAL_KINDS: &[&[u8; 4]] = &[
    b"avc1", b"avc2", b"avc3", b"avc4", b"hvc1", b"hev1", b"dvh1", b"dvhe", b"mp4v", b"jpeg",
    b"mjpa", b"mjpb", b"apch", b"apcn", b"apcs", b"apco", b"ap4h", b"ap4x", b"av01", b"vp08",
    b"vp09", b"s263", b"h263", b"encv", b"CRAW", b"2vuy", b"yuv2", b"raw ", b"dvc ", b"dvcp",
    b"AVdn", b"AVdh", b"xdvc", b"mx5p",
];

const PCM_KINDS: &[&[u8; 4]] = &[
    b"sowt", b"twos", b"lpcm", b"in24", b"in32", b"fl32", b"fl64", b"raw ", b"NONE", b"ulaw",
    b"alaw",
];

/// True for uncompressed PCM sample entry types.
pub fn is_pcm_kind(k: &[u8; 4]) -> bool {
    PCM_KINDS.contains(&k)
}

fn parse_stsd(b: &SliceBox<'_>, handler: &[u8; 4], problems: &mut Vec<String>) -> Vec<SampleEntry> {
    let count = u32be(b.payload, 4).unwrap_or(0).min(1024) as usize;
    let Some(body) = b.payload.get(8..) else {
        problems.push("stsd too short".into());
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in BoxIter::new(body, b.header.payload_offset() + 8).take(count) {
        out.push(parse_sample_entry(&e, handler));
    }
    if out.len() < count {
        problems.push(format!(
            "stsd: {} of {count} sample entries readable",
            out.len()
        ));
    }
    out
}

fn parse_sample_entry(e: &SliceBox<'_>, handler: &[u8; 4]) -> SampleEntry {
    let p = e.payload;
    let mut s = SampleEntry {
        kind: e.header.kind,
        raw: e.raw.to_vec(),
        data_reference_index: u16be(p, 6).unwrap_or(1),
        ..Default::default()
    };
    let visual = handler == b"vide" || VISUAL_KINDS.contains(&&s.kind);
    let audio = handler == b"soun" && !visual || &s.kind == b"mp4a" || is_pcm_kind(&s.kind);
    let children_at;
    if visual && handler != b"soun" {
        s.width = u16be(p, 24).unwrap_or(0);
        s.height = u16be(p, 26).unwrap_or(0);
        children_at = 78;
    } else if audio {
        s.qt_audio_version = u16be(p, 8).unwrap_or(0);
        s.channels = u16be(p, 16).unwrap_or(0);
        s.sample_size_bits = u16be(p, 18).unwrap_or(0);
        s.sample_rate = u32be(p, 24).unwrap_or(0) >> 16;
        children_at = match s.qt_audio_version {
            1 => {
                let bpf = u32be(p, 36).unwrap_or(0);
                if bpf > 0 && bpf <= 4096 {
                    s.bytes_per_frame = Some(bpf);
                }
                44
            }
            2 => {
                if let Some(bits) = u64be(p, 32) {
                    let r = f64::from_bits(bits);
                    if r.is_finite() && r > 0.0 && r < 10_000_000.0 {
                        s.sample_rate = r as u32;
                    }
                }
                s.channels = u32be(p, 40).unwrap_or(0).min(64) as u16;
                s.sample_size_bits = u32be(p, 48).unwrap_or(0).min(64) as u16;
                let bpp = u32be(p, 56).unwrap_or(0);
                let fpp = u32be(p, 60).unwrap_or(0);
                if bpp > 0 && fpp == 1 && bpp <= 4096 {
                    s.bytes_per_frame = Some(bpp);
                }
                64
            }
            _ => 28,
        };
        if s.bytes_per_frame.is_none() && is_pcm_kind(&s.kind) {
            let bytes = match &s.kind {
                b"in24" => 3,
                b"in32" | b"fl32" => 4,
                b"fl64" => 8,
                b"raw " | b"ulaw" | b"alaw" => 1,
                _ => (s.sample_size_bits as u32).div_ceil(8).max(1),
            };
            let ch = (s.channels as u32).max(1);
            s.bytes_per_frame = Some(bytes * ch);
        }
    } else {
        children_at = 8;
    }
    if let Some(kids) = p.get(children_at..) {
        scan_entry_children(
            kids,
            e.header.payload_offset() + children_at as u64,
            &mut s,
            0,
        );
    }
    if let Some(es) = &s.esds {
        if s.sample_rate == 0 && es.sample_rate != 0 {
            s.sample_rate = es.sample_rate;
        }
    }
    s
}

fn scan_entry_children(kids: &[u8], base: u64, s: &mut SampleEntry, depth: usize) {
    if depth > 4 {
        return;
    }
    for c in BoxIter::new(kids, base).take(256) {
        match &c.header.kind {
            b"avcC" => s.avcc = parse_avcc(c.payload),
            b"hvcC" => s.hvcc = parse_hvcc(c.payload),
            b"esds" => s.esds = parse_esds(c.payload),
            b"wave" => scan_entry_children(c.payload, c.header.payload_offset(), s, depth + 1),
            b"mp4a" if depth > 0 => {
                // QuickTime 'wave' may nest a v0 mp4a entry; its children follow 28 bytes.
                if let Some(k) = c.payload.get(28..) {
                    scan_entry_children(k, c.header.payload_offset() + 28, s, depth + 1)
                }
            }
            _ => {}
        }
    }
}

/// Parse an `avcC` payload.
pub fn parse_avcc(p: &[u8]) -> Option<AvcC> {
    if p.len() < 7 || p[0] != 1 {
        return None;
    }
    let mut c = AvcC {
        profile: p[1],
        compatibility: p[2],
        level: p[3],
        nal_length_size: (p[4] & 3) + 1,
        ..Default::default()
    };
    let nsps = (p[5] & 0x1F) as usize;
    let mut pos = 6;
    for _ in 0..nsps {
        let len = u16be(p, pos)? as usize;
        c.sps.push(p.get(pos + 2..pos + 2 + len)?.to_vec());
        pos += 2 + len;
    }
    let npps = *p.get(pos)? as usize;
    pos += 1;
    for _ in 0..npps {
        let Some(len) = u16be(p, pos) else { break };
        let len = len as usize;
        let Some(n) = p.get(pos + 2..pos + 2 + len) else {
            break;
        };
        c.pps.push(n.to_vec());
        pos += 2 + len;
    }
    Some(c)
}

/// Parse an `hvcC` payload.
pub fn parse_hvcc(p: &[u8]) -> Option<HvcC> {
    if p.len() < 23 || p[0] != 1 {
        return None;
    }
    let mut c = HvcC {
        profile_idc: p[1] & 0x1F,
        level_idc: p[12],
        nal_length_size: (p[21] & 3) + 1,
        ..Default::default()
    };
    let arrays = p[22] as usize;
    let mut pos = 23;
    for _ in 0..arrays {
        let Some(&t) = p.get(pos) else { break };
        let typ = t & 0x3F;
        let Some(n) = u16be(p, pos + 1) else { break };
        pos += 3;
        for _ in 0..n {
            let Some(len) = u16be(p, pos) else {
                return Some(c);
            };
            let len = len as usize;
            let Some(nal) = p.get(pos + 2..pos + 2 + len) else {
                return Some(c);
            };
            let v = nal.to_vec();
            match typ {
                32 => c.vps.push(v),
                33 => c.sps.push(v),
                34 => c.pps.push(v),
                39 | 40 => c.sei.push(v),
                _ => {}
            }
            pos += 2 + len;
        }
    }
    Some(c)
}

/// Read an MPEG-4 descriptor header: `(tag, payload_len, header_len)`.
fn descriptor(p: &[u8], pos: usize) -> Option<(u8, usize, usize)> {
    let tag = *p.get(pos)?;
    let mut len = 0usize;
    let mut i = 1;
    loop {
        let b = *p.get(pos + i)?;
        len = (len << 7) | (b & 0x7F) as usize;
        i += 1;
        if b & 0x80 == 0 || i > 4 {
            break;
        }
    }
    Some((tag, len, i))
}

/// Parse an `esds` payload (FullBox).
pub fn parse_esds(p: &[u8]) -> Option<Esds> {
    let (tag, len, hl) = descriptor(p, 4)?;
    if tag != 3 {
        return None;
    }
    let es_end = (4 + hl + len).min(p.len());
    let mut pos = 4 + hl;
    let flags = *p.get(pos + 2)?;
    pos += 3;
    if flags & 0x80 != 0 {
        pos += 2;
    }
    if flags & 0x40 != 0 {
        pos += 1 + *p.get(pos)? as usize;
    }
    if flags & 0x20 != 0 {
        pos += 2;
    }
    let mut out = Esds::default();
    let mut guard = 0;
    while pos < es_end && guard < 16 {
        guard += 1;
        let (t, l, h) = descriptor(p, pos)?;
        if t == 4 {
            out.object_type = *p.get(pos + h)?;
            out.max_bitrate = u32be(p, pos + h + 5).unwrap_or(0);
            out.avg_bitrate = u32be(p, pos + h + 9).unwrap_or(0);
            let mut q = pos + h + 13;
            let dend = (pos + h + l).min(p.len());
            while q < dend {
                let Some((t2, l2, h2)) = descriptor(p, q) else {
                    break;
                };
                if t2 == 5 {
                    if let Some(asc) = p.get(q + h2..(q + h2 + l2).min(p.len())) {
                        out.audio_specific_config = asc.to_vec();
                    }
                    break;
                }
                q += h2 + l2;
            }
        }
        pos += h + l;
    }
    if let Some(asc) = crate::aac::parse_audio_specific_config(&out.audio_specific_config) {
        out.channels = asc.channels;
        out.sample_rate = asc.sample_rate;
    }
    Some(out)
}

fn find_cnth(udta: &[u8], base: u64) -> Option<(u64, u64)> {
    let c = BoxIter::new(udta, base).find(|b| &b.header.kind == b"CNTH")?;
    let d =
        BoxIter::new(c.payload, c.header.payload_offset()).find(|b| &b.header.kind == b"CNDA")?;
    Some((d.header.payload_offset(), d.payload.len() as u64))
}

/// Parse the payload of the Canon metadata uuid box.
pub fn parse_canon_uuid(p: &[u8], base: u64) -> CanonInfo {
    let mut info = CanonInfo::default();
    for b in BoxIter::new(p, base).take(256) {
        match &b.header.kind {
            b"CNCV" => {
                let end = b
                    .payload
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(b.payload.len());
                info.cncv = Some(String::from_utf8_lossy(&b.payload[..end]).into_owned());
            }
            b"CTBO" => {
                let n = u32be(b.payload, 0).unwrap_or(0).min(64) as usize;
                for i in 0..n {
                    let o = 4 + i * 20;
                    let (Some(idx), Some(off), Some(sz)) = (
                        u32be(b.payload, o),
                        u64be(b.payload, o + 4),
                        u64be(b.payload, o + 12),
                    ) else {
                        break;
                    };
                    info.ctbo.push((idx, off, sz));
                }
            }
            b"CMT1" | b"CMT2" | b"CMT3" | b"CMT4" => {
                let i = (b.header.kind[3] - b'1') as usize;
                info.cmt[i] = Some((b.header.payload_offset(), b.payload.len() as u64));
            }
            b"THMB" => {
                let w = u16be(b.payload, 4).unwrap_or(0);
                let h = u16be(b.payload, 6).unwrap_or(0);
                let size = u32be(b.payload, 8).unwrap_or(0) as u64;
                let avail = b.payload.len().saturating_sub(16) as u64;
                if size > 0 {
                    info.thmb = Some((b.header.payload_offset() + 16, size.min(avail), w, h));
                }
            }
            b"CNTH" => {
                if let Some(d) = BoxIter::new(b.payload, b.header.payload_offset())
                    .find(|x| &x.header.kind == b"CNDA")
                {
                    info.cnth = Some((d.header.payload_offset(), d.payload.len() as u64));
                }
            }
            _ => {}
        }
    }
    info
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn languages() {
        assert_eq!(decode_language(0x55C4), "und");
        assert_eq!(decode_language(0x15C7), "eng");
        assert_eq!(decode_language(0x7FFF), "und");
        assert_eq!(decode_language(0), "und");
    }

    #[test]
    fn esds_and_avcc() {
        // From an ffmpeg file: mono 48 kHz AAC-LC.
        let esds = [
            0, 0, 0, 0, 3, 0x80, 0x80, 0x80, 0x25, 0, 2, 0, 4, 0x80, 0x80, 0x80, 0x17, 0x40, 0x15,
            0, 0, 0, 0, 1, 0xf4, 0, 0, 1, 0xe8, 0x9b, 5, 0x80, 0x80, 0x80, 5, 0x11, 0x88, 0x56,
            0xe5, 0, 6, 0x80, 0x80, 0x80, 1, 2,
        ];
        let e = parse_esds(&esds).unwrap();
        assert_eq!(e.object_type, 0x40);
        assert_eq!(e.audio_specific_config, vec![0x11, 0x88, 0x56, 0xe5, 0]);
        assert_eq!(e.channels, 1);
        assert_eq!(e.sample_rate, 48000);
        let avcc = [
            1, 0x64, 0, 0x0d, 0xff, 0xe1, 0, 2, 0x67, 0x64, 1, 0, 2, 0x68, 0xeb,
        ];
        let a = parse_avcc(&avcc).unwrap();
        assert_eq!(a.nal_length_size, 4);
        assert_eq!(a.sps, vec![vec![0x67, 0x64]]);
        assert_eq!(a.pps, vec![vec![0x68, 0xeb]]);
        assert!(parse_avcc(&[1, 2]).is_none());
        assert!(parse_esds(&[0; 3]).is_none());
    }
}
