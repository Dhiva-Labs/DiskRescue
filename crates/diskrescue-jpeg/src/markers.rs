//! JPEG markers and the tolerant header walk (SOI up to the first SOS).
//!
//! # Algorithm
//!
//! [`walk_headers`] reads the segment chain through a bounded read-ahead
//! window (never the whole file). Every marker with a payload carries a
//! big-endian length that includes its own two bytes; the walk hops from
//! segment to segment by those lengths, which is what keeps it out of
//! embedded thumbnails (an Exif thumbnail's SOI/EOI live inside the APP1
//! payload and are never looked at as markers).
//!
//! Tolerated and reported in [`Headers::issues`]:
//!
//! * fill bytes (`FF FF ...`) before a marker,
//! * a doubled SOI at the start (`FF D8 FF D8`),
//! * stray RSTn/TEM markers between segments,
//! * stray bytes or a wrong segment length: when the walk does not land on
//!   a marker it resynchronises within a bounded window (64 KiB forward
//!   from where the length pointed, or inside the previous segment when the
//!   length was too long) on a marker whose own length lands on another
//!   valid marker (two-link chain check). When the previous segment is an
//!   APPn/COM, its length is considered wrong and corrected; otherwise the
//!   skipped bytes are reported as stray.
//!
//! The walk stops at the first SOS (the entropy-coded data that follows is
//! the scan analyzer's job), at an EOI or nested SOI, at the end of the
//! readable data, or after [`MAX_HEADER_BYTES`] / [`MAX_SEGMENTS`].
//!
//! Limits: hierarchical (DHP/EXP) files are walked but not interpreted;
//! extended-XMP is identified but not reassembled.

use diskrescue_core::source::Source;
use diskrescue_core::util::bytes::{u16be, u32be};
use diskrescue_core::{Error, Range, Result};

use crate::cursor::{Cursor, HEADER_CHUNK};
use crate::exif::{parse_app1, parse_mpf, ExifInfo, MpfInfo};
use crate::huffman::HuffDecoder;

/// TEM (arithmetic temporary), no payload.
pub const TEM: u8 = 0x01;
/// SOF0 baseline DCT.
pub const SOF0: u8 = 0xC0;
/// SOF1 extended sequential DCT.
pub const SOF1: u8 = 0xC1;
/// SOF2 progressive DCT.
pub const SOF2: u8 = 0xC2;
/// SOF3 lossless (Huffman).
pub const SOF3: u8 = 0xC3;
/// DHT define Huffman tables.
pub const DHT: u8 = 0xC4;
/// JPG (reserved).
pub const JPG: u8 = 0xC8;
/// DAC define arithmetic conditioning.
pub const DAC: u8 = 0xCC;
/// RST0 (first restart marker).
pub const RST0: u8 = 0xD0;
/// RST7 (last restart marker).
pub const RST7: u8 = 0xD7;
/// SOI start of image.
pub const SOI: u8 = 0xD8;
/// EOI end of image.
pub const EOI: u8 = 0xD9;
/// SOS start of scan.
pub const SOS: u8 = 0xDA;
/// DQT define quantisation tables.
pub const DQT: u8 = 0xDB;
/// DNL define number of lines.
pub const DNL: u8 = 0xDC;
/// DRI define restart interval.
pub const DRI: u8 = 0xDD;
/// DHP define hierarchical progression.
pub const DHP: u8 = 0xDE;
/// EXP expand reference components.
pub const EXP: u8 = 0xDF;
/// APP0 (JFIF, JFXX, AVI1).
pub const APP0: u8 = 0xE0;
/// APP1 (Exif, XMP).
pub const APP1: u8 = 0xE1;
/// APP2 (ICC profile, MPF).
pub const APP2: u8 = 0xE2;
/// APP13 (Photoshop IRB).
pub const APP13: u8 = 0xED;
/// APP14 (Adobe).
pub const APP14: u8 = 0xEE;
/// APP15.
pub const APP15: u8 = 0xEF;
/// COM comment.
pub const COM: u8 = 0xFE;

/// Largest header (SOI..SOS) accepted, in bytes.
pub const MAX_HEADER_BYTES: u64 = 16 << 20;
/// Most segments walked before giving up.
pub const MAX_SEGMENTS: usize = 4096;
/// Window searched when the walk has to resynchronise.
pub const RESYNC_WINDOW: u64 = 64 * 1024;

/// SOFn marker (any coding process; excludes DHT, JPG and DAC).
pub fn is_sof(m: u8) -> bool {
    (0xC0..=0xCF).contains(&m) && m != DHT && m != JPG && m != DAC
}

/// RST0..RST7.
pub fn is_rst(m: u8) -> bool {
    (RST0..=RST7).contains(&m)
}

/// APP0..APP15.
pub fn is_app(m: u8) -> bool {
    (APP0..=APP15).contains(&m)
}

/// Markers that may legitimately appear between SOI and SOS (or between
/// scans) and carry a length field.
pub fn is_header_segment(m: u8) -> bool {
    is_sof(m) || is_app(m) || matches!(m, DHT | DAC | SOS | DQT | DNL | DRI | DHP | EXP | COM)
}

/// Short name of a marker (`SOF0`, `APP1`, `RST3`, ...).
pub fn marker_name(m: u8) -> String {
    match m {
        SOF0..=0xCF if is_sof(m) => format!("SOF{}", m - SOF0),
        DHT => "DHT".into(),
        JPG => "JPG".into(),
        DAC => "DAC".into(),
        RST0..=RST7 => format!("RST{}", m - RST0),
        SOI => "SOI".into(),
        EOI => "EOI".into(),
        SOS => "SOS".into(),
        DQT => "DQT".into(),
        DNL => "DNL".into(),
        DRI => "DRI".into(),
        DHP => "DHP".into(),
        EXP => "EXP".into(),
        APP0..=APP15 => format!("APP{}", m - APP0),
        COM => "COM".into(),
        TEM => "TEM".into(),
        _ => format!("0x{m:02X}"),
    }
}

/// Smallest plausible length field for a segment marker.
fn length_plausible(m: u8, len: u16) -> bool {
    match m {
        DRI | DNL => len == 4,
        DQT => len >= 67,
        DHT => len >= 19,
        SOS => len >= 8,
        m if is_sof(m) => len >= 11,
        _ => len >= 2,
    }
}

/// One marker segment with a length field.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Segment {
    /// Marker code (the byte after `FF`).
    pub marker: u8,
    /// File-relative offset of the `FF` of the marker.
    pub offset: u64,
    /// Effective length including the two length bytes (differs from
    /// `declared_len` when the walk corrected a damaged length).
    pub len: u32,
    /// Length as stored in the file.
    pub declared_len: u16,
    /// File-relative offset of the first payload byte (`offset + 4`).
    pub data_offset: u64,
}

impl Segment {
    /// File-relative offset one past the segment.
    pub fn end(&self) -> u64 {
        self.offset + 2 + self.len as u64
    }

    /// Payload length (without marker and length bytes).
    pub fn payload_len(&self) -> u64 {
        (self.len as u64).saturating_sub(2)
    }
}

/// One frame component from the SOF segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Component {
    /// Component id (`Ci`).
    pub id: u8,
    /// Horizontal sampling factor (1..=4).
    pub h: u8,
    /// Vertical sampling factor (1..=4).
    pub v: u8,
    /// Quantisation table selector (0..=3).
    pub tq: u8,
}

/// The frame header (SOFn).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FrameInfo {
    /// SOF marker code (`0xC0` baseline, `0xC2` progressive, ...).
    pub marker: u8,
    /// File-relative offset of the SOF marker.
    pub offset: u64,
    /// Sample precision in bits.
    pub precision: u8,
    /// Number of lines (0 means "defined by a DNL segment").
    pub height: u16,
    /// Samples per line.
    pub width: u16,
    /// Components in frame order.
    pub components: Vec<Component>,
    /// Progressive DCT (SOF2/6/10/14).
    pub progressive: bool,
    /// Lossless process (SOF3/7/11/15): not a photo; rejected by the format.
    pub lossless: bool,
    /// Arithmetic coding (SOF9..SOF15): walked but not Huffman-validated.
    pub arithmetic: bool,
    /// Differential (hierarchical) process.
    pub differential: bool,
}

impl FrameInfo {
    /// Largest horizontal sampling factor.
    pub fn hmax(&self) -> u8 {
        self.components
            .iter()
            .map(|c| c.h)
            .max()
            .unwrap_or(1)
            .max(1)
    }

    /// Largest vertical sampling factor.
    pub fn vmax(&self) -> u8 {
        self.components
            .iter()
            .map(|c| c.v)
            .max()
            .unwrap_or(1)
            .max(1)
    }

    /// MCUs per row of an interleaved scan.
    pub fn mcus_x(&self) -> u32 {
        (self.width as u32).div_ceil(8 * self.hmax() as u32)
    }

    /// MCU rows of an interleaved scan.
    pub fn mcus_y(&self) -> u32 {
        (self.height as u32).div_ceil(8 * self.vmax() as u32)
    }

    /// Index of the component with id `id`.
    pub fn component_index(&self, id: u8) -> Option<usize> {
        self.components.iter().position(|c| c.id == id)
    }

    /// Huffman-coded DCT process (what the scan analyzer can decode).
    pub fn is_huffman_dct(&self) -> bool {
        !self.arithmetic && !self.lossless && !self.differential
    }

    /// Short description of the coding process.
    pub fn process(&self) -> &'static str {
        match (
            self.lossless,
            self.progressive,
            self.arithmetic,
            self.differential,
        ) {
            (true, _, _, _) => "lossless",
            (_, _, _, true) => "hierarchical",
            (_, true, true, _) => "progressive-arithmetic",
            (_, true, false, _) => "progressive",
            (_, false, true, _) => "sequential-arithmetic",
            _ if self.marker == SOF1 => "extended-sequential",
            _ => "baseline",
        }
    }

    /// Chroma subsampling notation for 3-component images (`4:2:0`, ...).
    pub fn subsampling(&self) -> Option<String> {
        if self.components.len() != 3 {
            return None;
        }
        let (y, c1, c2) = (self.components[0], self.components[1], self.components[2]);
        if (c1.h, c1.v) != (1, 1) || (c2.h, c2.v) != (1, 1) {
            return Some(format!(
                "{}x{},{}x{},{}x{}",
                y.h, y.v, c1.h, c1.v, c2.h, c2.v
            ));
        }
        Some(match (y.h, y.v) {
            (1, 1) => "4:4:4".into(),
            (2, 1) => "4:2:2".into(),
            (2, 2) => "4:2:0".into(),
            (1, 2) => "4:4:0".into(),
            (4, 1) => "4:1:1".into(),
            (h, v) => format!("{h}x{v},1x1,1x1"),
        })
    }

    /// Structural sanity check used to reject false positives.
    pub fn check(&self) -> std::result::Result<(), String> {
        if self.width == 0 {
            return Err("SOF width is 0".into());
        }
        let n = self.components.len();
        if n == 0 || n > 4 {
            return Err(format!("{n} components"));
        }
        if self.lossless {
            if !(2..=16).contains(&self.precision) {
                return Err(format!("precision {}", self.precision));
            }
        } else if self.precision != 8 && self.precision != 12 {
            return Err(format!("precision {}", self.precision));
        }
        for c in &self.components {
            if !(1..=4).contains(&c.h) || !(1..=4).contains(&c.v) || c.tq > 3 {
                return Err(format!(
                    "component {} has sampling {}x{} / table {}",
                    c.id, c.h, c.v, c.tq
                ));
            }
        }
        if n > 1 {
            let blocks: u32 = self
                .components
                .iter()
                .map(|c| c.h as u32 * c.v as u32)
                .sum();
            if blocks > 10 {
                return Err(format!("{blocks} blocks per MCU (more than 10)"));
            }
        }
        for (i, c) in self.components.iter().enumerate() {
            if self.components[..i].iter().any(|d| d.id == c.id) {
                return Err(format!("duplicate component id {}", c.id));
            }
        }
        if (self.width as u64) * (self.height as u64) > (1u64 << 31) {
            return Err("more than 2^31 pixels".into());
        }
        Ok(())
    }
}

/// A component reference inside an SOS header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ScanComponent {
    /// Component selector (matches a frame component id).
    pub selector: u8,
    /// DC table selector.
    pub dc_table: u8,
    /// AC table selector.
    pub ac_table: u8,
}

/// A scan header (SOS).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ScanInfo {
    /// File-relative offset of the SOS marker.
    pub offset: u64,
    /// File-relative offset of the first entropy-coded byte.
    pub data_offset: u64,
    /// Components in the scan.
    pub components: Vec<ScanComponent>,
    /// Start of spectral selection.
    pub ss: u8,
    /// End of spectral selection.
    pub se: u8,
    /// Successive approximation high bit.
    pub ah: u8,
    /// Successive approximation low bit.
    pub al: u8,
}

/// A Huffman table as stored in a DHT segment.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HuffmanTable {
    /// 0 = DC, 1 = AC.
    pub class: u8,
    /// Destination id (0..=3).
    pub id: u8,
    /// Number of codes of each length 1..=16.
    pub bits: [u8; 16],
    /// Symbols in code order.
    pub values: Vec<u8>,
    /// File-relative offset of the DHT segment that defined it.
    pub offset: u64,
}

impl HuffmanTable {
    /// True when this is the ITU T.81 Annex K table for its class/id.
    pub fn is_standard(&self) -> bool {
        crate::tables::standard_table(self.class, self.id)
            .is_some_and(|s| s.bits == self.bits && s.values == self.values)
    }
}

/// A quantisation table from a DQT segment.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct QuantTable {
    /// Destination id (0..=3).
    pub id: u8,
    /// 0 = 8-bit entries, 1 = 16-bit entries.
    pub precision: u8,
    /// The 64 entries in zigzag order.
    pub values: Vec<u16>,
    /// File-relative offset of the DQT segment.
    pub offset: u64,
}

/// What an APPn segment contains.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum AppKind {
    /// APP0 `JFIF\0`.
    Jfif,
    /// APP0 `JFXX\0` (JFIF extension, may hold a thumbnail).
    Jfxx,
    /// APP0 `AVI1` (Motion-JPEG frame: implies the standard Huffman tables).
    Avi1,
    /// APP1 `Exif\0`.
    Exif,
    /// APP1 XMP packet.
    Xmp,
    /// APP1 extended XMP chunk.
    ExtendedXmp,
    /// APP2 ICC profile chunk.
    Icc,
    /// APP2 Multi-Picture Format index.
    Mpf,
    /// APP13 Photoshop image resources.
    Photoshop,
    /// APP14 Adobe (colour transform flag).
    Adobe,
    /// Anything else; holds the printable identifier, if any.
    Other(String),
}

/// An application segment.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AppSegment {
    /// Marker (`0xE0..=0xEF`).
    pub marker: u8,
    /// File-relative offset of the marker.
    pub offset: u64,
    /// Length including the two length bytes.
    pub len: u32,
    /// Identification.
    pub kind: AppKind,
}

/// Hints from XMP about a video appended after the JPEG (motion photos).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MotionPhotoHint {
    /// `GCamera:MotionPhoto`, `MicroVideo` or a `MotionPhoto` container item.
    pub motion_photo: bool,
    /// `GCamera:MicroVideoOffset`: bytes from the end of the file to the
    /// start of the video.
    pub micro_video_offset: Option<u64>,
    /// `Item:Length` of the video item (Motion Photo v2 container).
    pub video_length: Option<u64>,
}

/// A problem the tolerant walk worked around.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum HeaderIssue {
    /// A second SOI right after the first (dropped).
    DoubleSoi {
        /// Offset of the extra SOI.
        offset: u64,
    },
    /// Bytes that belong to no segment, skipped.
    StrayBytes {
        /// First stray byte.
        offset: u64,
        /// Number of stray bytes.
        len: u64,
    },
    /// A segment whose length field was wrong; `fixed` is the length that
    /// makes the chain continue.
    BadLength {
        /// Offset of the segment's marker.
        offset: u64,
        /// Marker code.
        marker: u8,
        /// Length stored in the file.
        declared: u16,
        /// Length that was used instead.
        fixed: u32,
    },
    /// A reserved or misplaced marker, skipped.
    InvalidMarker {
        /// Offset of the `FF`.
        offset: u64,
        /// Marker code.
        marker: u8,
    },
    /// A second SOF (ignored; the first one is used).
    DuplicateSof {
        /// Offset of the ignored SOF.
        offset: u64,
    },
    /// A DQT/DHT/SOF/SOS/DRI segment with inconsistent content.
    BadSegment {
        /// Offset of the segment's marker.
        offset: u64,
        /// Marker code.
        marker: u8,
        /// What is wrong.
        detail: String,
    },
}

impl HeaderIssue {
    /// One-line description for reports.
    pub fn describe(&self) -> String {
        match self {
            HeaderIssue::DoubleSoi { offset } => format!("duplicate SOI at offset {offset}"),
            HeaderIssue::StrayBytes { offset, len } => {
                format!("{len} stray bytes between header segments at offset {offset}")
            }
            HeaderIssue::BadLength {
                offset,
                marker,
                declared,
                fixed,
            } => format!(
                "{} segment at offset {offset} has length {declared}, should be {fixed}",
                marker_name(*marker)
            ),
            HeaderIssue::InvalidMarker { offset, marker } => format!(
                "invalid marker {} in header at offset {offset}",
                marker_name(*marker)
            ),
            HeaderIssue::DuplicateSof { offset } => format!("second SOF at offset {offset}"),
            HeaderIssue::BadSegment {
                offset,
                marker,
                detail,
            } => format!(
                "bad {} segment at offset {offset}: {detail}",
                marker_name(*marker)
            ),
        }
    }
}

/// Why the header walk stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum HeaderEnd {
    /// Reached the first SOS (normal).
    Sos,
    /// An EOI before any scan.
    Eoi {
        /// Offset of the EOI marker.
        offset: u64,
    },
    /// A new SOI inside the headers (concatenated or corrupt file).
    NestedSoi {
        /// Offset of the SOI marker.
        offset: u64,
    },
    /// The readable data ended inside the headers (source end or limit).
    Truncated {
        /// First missing byte.
        offset: u64,
    },
    /// A read error inside the headers.
    Unreadable {
        /// First unreadable byte.
        offset: u64,
    },
    /// No marker where one was expected and no resynchronisation possible.
    NoMarker {
        /// Where a marker was expected.
        offset: u64,
    },
    /// More than [`MAX_HEADER_BYTES`] or [`MAX_SEGMENTS`].
    TooLarge {
        /// Where the walk gave up.
        offset: u64,
    },
}

/// Everything the header walk learned.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Headers {
    /// Offset of the SOI in the walked source (0 for a file that starts
    /// with its SOI; more means junk before the SOI). All other offsets in
    /// this struct are relative to the SOI.
    pub soi_offset: u64,
    /// Segments in file order (the SOS segment last, when reached).
    pub segments: Vec<Segment>,
    /// The frame header.
    pub sof: Option<FrameInfo>,
    /// Quantisation tables.
    pub dqt: Vec<QuantTable>,
    /// Huffman tables (later definitions of the same class/id follow
    /// earlier ones).
    pub dht: Vec<HuffmanTable>,
    /// Restart interval from DRI (in MCUs).
    pub dri: Option<u16>,
    /// Offset of the first SOS marker.
    pub sos_offset: Option<u64>,
    /// Offset of the first entropy-coded byte of the first scan.
    pub scan_data_offset: Option<u64>,
    /// Scan headers seen by the walk (the first scan; later scans of a
    /// progressive file are listed by the scan analyzer).
    pub scans: Vec<ScanInfo>,
    /// Exif metadata (first APP1 Exif segment).
    pub exif: Option<ExifInfo>,
    /// APPn segments.
    pub app_segments: Vec<AppSegment>,
    /// MPF index (APP2 `MPF\0`).
    pub mpf: Option<MpfInfo>,
    /// Motion-photo hints from XMP.
    pub motion_photo: Option<MotionPhotoHint>,
    /// Embedded thumbnails (Exif IFD1, JFXX, Photoshop), file-relative.
    pub thumbnails: Vec<Range>,
    /// APP0 JFIF present.
    pub jfif: bool,
    /// APP0 AVI1 present (Motion-JPEG frame).
    pub avi1: bool,
    /// APP14 Adobe transform flag.
    pub adobe_transform: Option<u8>,
    /// Problems worked around.
    pub issues: Vec<HeaderIssue>,
    /// Observations for reports.
    pub notes: Vec<String>,
    /// Why the walk stopped.
    pub end: HeaderEnd,
}

impl Headers {
    /// An empty result (used when no header could be walked at all).
    pub fn empty() -> Self {
        Headers {
            soi_offset: 0,
            segments: Vec::new(),
            sof: None,
            dqt: Vec::new(),
            dht: Vec::new(),
            dri: None,
            sos_offset: None,
            scan_data_offset: None,
            scans: Vec::new(),
            exif: None,
            app_segments: Vec::new(),
            mpf: None,
            motion_photo: None,
            thumbnails: Vec::new(),
            jfif: false,
            avi1: false,
            adobe_transform: None,
            issues: Vec::new(),
            notes: Vec::new(),
            end: HeaderEnd::NoMarker { offset: 0 },
        }
    }

    /// True when the walk reached the first scan after a SOF.
    pub fn reached_scan(&self) -> bool {
        self.end == HeaderEnd::Sos
            && self.sof.is_some()
            && self.scan_data_offset.is_some()
            && !self.scans.is_empty()
    }

    /// Quantisation table `id`, if defined.
    pub fn quant_table(&self, id: u8) -> Option<&QuantTable> {
        self.dqt.iter().rev().find(|q| q.id == id)
    }

    /// Huffman table `class`/`id`, if defined (last definition wins).
    pub fn huffman_table(&self, class: u8, id: u8) -> Option<&HuffmanTable> {
        self.dht
            .iter()
            .rev()
            .find(|t| t.class == class && t.id == id)
    }

    /// Quantisation tables referenced by the frame but not defined.
    pub fn missing_quant_tables(&self) -> Vec<u8> {
        let Some(sof) = &self.sof else {
            return Vec::new();
        };
        if sof.lossless {
            return Vec::new();
        }
        let mut v: Vec<u8> = sof
            .components
            .iter()
            .map(|c| c.tq)
            .filter(|&t| self.quant_table(t).is_none())
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    /// Huffman tables referenced by the first scan but not defined, as
    /// `(class, id)` pairs.
    pub fn missing_huffman_tables(&self) -> Vec<(u8, u8)> {
        let (Some(sof), Some(scan)) = (&self.sof, self.scans.first()) else {
            return Vec::new();
        };
        if sof.arithmetic {
            return Vec::new();
        }
        let dc = scan.ss == 0 && scan.ah == 0 || !sof.progressive;
        let ac = scan.se > 0;
        let mut v = Vec::new();
        for c in &scan.components {
            if dc && self.huffman_table(0, c.dc_table).is_none() {
                v.push((0, c.dc_table));
            }
            if ac && self.huffman_table(1, c.ac_table).is_none() {
                v.push((1, c.ac_table));
            }
        }
        v.sort_unstable();
        v.dedup();
        v
    }

    /// True when no DHT segment exists at all (Motion-JPEG convention).
    pub fn has_no_dht(&self) -> bool {
        !self.segments.iter().any(|s| s.marker == DHT)
    }
}

/// Walk the header segments of the JPEG that starts at `start` in `src`,
/// reading at most `limit` bytes. Fails only when there is no SOI at
/// `start`; every other problem is reported inside [`Headers`].
pub fn walk_headers(src: &dyn Source, start: u64, limit: u64) -> Result<Headers> {
    let mut cur = Cursor::new(src, start, limit, HEADER_CHUNK);
    walk(&mut cur)
}

/// Header walk over an existing cursor (shared with measure/validate so the
/// data is read once).
pub(crate) fn walk(cur: &mut Cursor<'_>) -> Result<Headers> {
    if cur.byte(0) != Some(0xFF) || cur.byte(1) != Some(SOI) {
        return Err(Error::corrupt("JPEG header", "no SOI marker at the start"));
    }
    let mut h = Headers::empty();
    h.soi_offset = cur.base();
    let mut pos = 2u64;
    let mut steps = 0usize;
    loop {
        steps += 1;
        if steps > MAX_SEGMENTS * 2 || pos > MAX_HEADER_BYTES {
            h.end = HeaderEnd::TooLarge { offset: pos };
            break;
        }
        // Fill bytes: any number of FF before the marker code.
        while cur.byte(pos) == Some(0xFF) && cur.byte(pos + 1) == Some(0xFF) {
            pos += 1;
            if pos > MAX_HEADER_BYTES {
                break;
            }
        }
        let (b0, b1) = (cur.byte(pos), cur.byte(pos + 1));
        let m = match (b0, b1) {
            (Some(0xFF), Some(m)) => m,
            (None, _) | (Some(0xFF), None) => {
                h.end = data_end(cur, pos);
                break;
            }
            _ => match resync_here(cur, &mut h, pos) {
                Some(q) => {
                    pos = q;
                    continue;
                }
                None => {
                    h.end = HeaderEnd::NoMarker { offset: pos };
                    break;
                }
            },
        };
        match m {
            SOI if h.segments.is_empty() => {
                h.issues.push(HeaderIssue::DoubleSoi { offset: pos });
                pos += 2;
                continue;
            }
            SOI => {
                h.end = HeaderEnd::NestedSoi { offset: pos };
                break;
            }
            EOI => {
                h.end = HeaderEnd::Eoi { offset: pos };
                break;
            }
            m if is_rst(m) || m == TEM => {
                h.issues.push(HeaderIssue::InvalidMarker {
                    offset: pos,
                    marker: m,
                });
                pos += 2;
                continue;
            }
            m if !is_header_segment(m) => {
                h.issues.push(HeaderIssue::InvalidMarker {
                    offset: pos,
                    marker: m,
                });
                match resync_here(cur, &mut h, pos + 1) {
                    Some(q) => {
                        pos = q;
                        continue;
                    }
                    None => {
                        h.end = HeaderEnd::NoMarker { offset: pos };
                        break;
                    }
                }
            }
            _ => {}
        }
        let Some(declared) = cur.u16be(pos + 2) else {
            h.end = data_end(cur, pos);
            break;
        };
        if !length_plausible(m, declared) {
            h.issues.push(HeaderIssue::BadSegment {
                offset: pos,
                marker: m,
                detail: format!("implausible length {declared}"),
            });
            match resync_here(cur, &mut h, pos + 2) {
                Some(q) => {
                    pos = q;
                    continue;
                }
                None => {
                    h.end = HeaderEnd::NoMarker { offset: pos };
                    break;
                }
            }
        }
        let mut seg = Segment {
            marker: m,
            offset: pos,
            len: declared as u32,
            declared_len: declared,
            data_offset: pos + 4,
        };
        if seg.end() > cur.limit() && m != SOS {
            // Either the file really ends here or the length is too long;
            // look for a chain inside the would-be payload first (a partial
            // Exif payload is parsed so its thumbnail is not mistaken for
            // the continuation of the outer header).
            let mut skip = h.thumbnails.clone();
            if m == APP1 {
                let avail = cur.limit().saturating_sub(seg.data_offset) as usize;
                let partial = cur.read_vec(seg.data_offset, avail.min(0xFFFF));
                if let Some(t) = parse_app1(&partial, seg.data_offset).and_then(|e| e.thumbnail) {
                    skip.push(t);
                }
            }
            match best_candidate(cur, seg.data_offset, cur.limit(), &skip, true) {
                Some(q) if is_app(m) || m == COM => {
                    let fixed = (q - pos - 2) as u32;
                    h.issues.push(HeaderIssue::BadLength {
                        offset: pos,
                        marker: m,
                        declared,
                        fixed,
                    });
                    seg.len = fixed;
                }
                _ => {
                    h.end = data_end(cur, cur.limit().max(pos));
                    break;
                }
            }
        }
        let payload = cur.read_vec(seg.data_offset, seg.payload_len() as usize);
        if (payload.len() as u64) < seg.payload_len() {
            h.end = data_end(cur, seg.data_offset + payload.len() as u64);
            break;
        }
        parse_segment(&mut h, &seg, &payload);
        let end = seg.end();
        h.segments.push(seg);
        if m == SOS {
            if h.scans.is_empty() {
                // Unusable SOS: keep walking is pointless, the data after it
                // is entropy coded.
                h.notes.push("first SOS segment is malformed".into());
            }
            h.sos_offset = Some(pos);
            h.scan_data_offset = Some(end);
            h.end = HeaderEnd::Sos;
            break;
        }
        pos = end;
    }
    if h.sof.is_none() && h.end == HeaderEnd::Sos {
        h.notes.push("SOS reached without a SOF segment".into());
    }
    Ok(h)
}

/// Classify the end of readable data (always at the cursor's limit).
fn data_end(cur: &Cursor<'_>, _pos: u64) -> HeaderEnd {
    match cur.read_error_offset() {
        Some(e) if e == cur.limit() => HeaderEnd::Unreadable { offset: e },
        _ => HeaderEnd::Truncated {
            offset: cur.limit(),
        },
    }
}

/// The walk expected a marker at `pos` but found something else: find
/// where the chain continues, record what was skipped or corrected, and
/// return the new position.
fn resync_here(cur: &mut Cursor<'_>, h: &mut Headers, pos: u64) -> Option<u64> {
    let skip = h.thumbnails.clone();
    let prev_end = h.segments.last().map(|s| s.end());
    let prev_data = h.segments.last().map(|s| s.data_offset);
    // Forward: stray bytes, or the previous length was too short.
    let fwd = best_candidate(cur, pos, pos + RESYNC_WINDOW, &skip, false);
    // Backward: the previous segment's length was too long.
    let back = match (prev_data, prev_end) {
        (Some(d), Some(e)) if e == pos => best_candidate(cur, d, pos, &skip, false),
        _ => None,
    };
    let q = match (fwd, back) {
        (Some(f), Some(b)) => {
            let (fs, bs) = (chain_score(cur, f), chain_score(cur, b));
            if bs > fs {
                b
            } else {
                f
            }
        }
        (Some(f), None) => f,
        (None, Some(b)) => b,
        (None, None) => return None,
    };
    let last = h.segments.last().cloned();
    match last {
        Some(prev) if prev.end() == pos && prev.marker != SOS => {
            let fixed = q.saturating_sub(prev.offset + 2);
            let opaque = is_app(prev.marker) || prev.marker == COM;
            if q < pos || (opaque && fixed <= 0xFFFF) {
                h.issues.push(HeaderIssue::BadLength {
                    offset: prev.offset,
                    marker: prev.marker,
                    declared: prev.declared_len,
                    fixed: fixed as u32,
                });
                if let Some(s) = h.segments.last_mut() {
                    s.len = fixed as u32;
                }
                if q < pos && matches!(prev.marker, DHT | DQT) {
                    // Re-parse the shortened table segment.
                    h.dht.retain(|t| t.offset != prev.offset);
                    h.dqt.retain(|t| t.offset != prev.offset);
                    let mut fixed_seg = prev.clone();
                    fixed_seg.len = fixed as u32;
                    let payload =
                        cur.read_vec(fixed_seg.data_offset, fixed_seg.payload_len() as usize);
                    parse_segment(h, &fixed_seg, &payload);
                }
            } else {
                h.issues.push(HeaderIssue::StrayBytes {
                    offset: pos,
                    len: q - pos,
                });
            }
        }
        _ => {
            if q > pos {
                h.issues.push(HeaderIssue::StrayBytes {
                    offset: pos,
                    len: q - pos,
                });
            }
        }
    }
    Some(q)
}

/// First position in `from..to` (outside `skip`) where a plausible segment
/// chain starts; with `need_sos`, only chains that reach an SOS count.
/// Embedded JPEGs (an `FF D8 FF` inside the window, typically a thumbnail
/// inside a damaged APPn) are stepped over up to their EOI so their
/// segments are never mistaken for the outer file's.
fn best_candidate(
    cur: &mut Cursor<'_>,
    from: u64,
    to: u64,
    skip: &[Range],
    need_sos: bool,
) -> Option<u64> {
    let mut q = from;
    let mut tries = 0usize;
    while let Some(f) = cur.find_ff(q, to) {
        tries += 1;
        if tries > 4096 {
            return None;
        }
        q = f + 1;
        if let Some(r) = skip.iter().find(|r| r.contains(f)) {
            q = r.end;
            continue;
        }
        if cur.byte(f + 1) == Some(SOI) && cur.byte(f + 2) == Some(0xFF) {
            // Step over the embedded image to its EOI.
            let mut p = f + 2;
            let mut found = None;
            while let Some(g) = cur.find_ff(p, to) {
                if cur.byte(g + 1) == Some(EOI) {
                    found = Some(g + 2);
                    break;
                }
                p = g + 1;
            }
            q = found?;
            continue;
        }
        if is_chain_start(cur, f) {
            if need_sos && chain_score(cur, f).0 == 0 {
                continue;
            }
            return Some(f);
        }
    }
    None
}

/// Whether a header segment starts at `q` whose length lands on another
/// valid marker (two-link check). An SOS only needs a consistent length.
fn is_chain_start(cur: &mut Cursor<'_>, q: u64) -> bool {
    if cur.byte(q) != Some(0xFF) {
        return false;
    }
    let Some(m) = cur.byte(q + 1) else {
        return false;
    };
    if !is_header_segment(m) {
        return false;
    }
    let Some(len) = cur.u16be(q + 2) else {
        return false;
    };
    if !length_plausible(m, len) {
        return false;
    }
    if m == SOS {
        return matches!(cur.byte(q + 4), Some(ns) if (1..=4).contains(&ns) && len as u32 == 6 + 2 * ns as u32);
    }
    let mut p = q + 2 + len as u64;
    let mut guard = 0;
    while cur.byte(p) == Some(0xFF) && cur.byte(p + 1) == Some(0xFF) && guard < 16 {
        p += 1;
        guard += 1;
    }
    cur.byte(p) == Some(0xFF)
        && cur
            .byte(p + 1)
            .is_some_and(|m2| is_header_segment(m2) || m2 == EOI)
}

/// Follow the length chain from `q`: `(1 if an SOS was reached else 0,
/// segments hopped)`; tuple order makes reaching SOS dominate.
fn chain_score(cur: &mut Cursor<'_>, q: u64) -> (u32, u32) {
    let mut p = q;
    let mut hops = 0u32;
    while hops < 64 {
        let mut guard = 0;
        while cur.byte(p) == Some(0xFF) && cur.byte(p + 1) == Some(0xFF) && guard < 16 {
            p += 1;
            guard += 1;
        }
        if cur.byte(p) != Some(0xFF) {
            break;
        }
        let Some(m) = cur.byte(p + 1) else {
            break;
        };
        if m == SOS {
            return (1, hops + 1);
        }
        if !is_header_segment(m) {
            break;
        }
        let Some(len) = cur.u16be(p + 2) else {
            break;
        };
        if !length_plausible(m, len) {
            break;
        }
        hops += 1;
        p += 2 + len as u64;
    }
    (0, hops)
}

/// Interpret one segment's payload.
fn parse_segment(h: &mut Headers, seg: &Segment, p: &[u8]) {
    let m = seg.marker;
    match m {
        m if is_sof(m) => match parse_sof(m, seg.offset, p) {
            Ok(f) => {
                if h.sof.is_some() {
                    h.issues
                        .push(HeaderIssue::DuplicateSof { offset: seg.offset });
                } else {
                    h.sof = Some(f);
                }
            }
            Err(e) => h.issues.push(HeaderIssue::BadSegment {
                offset: seg.offset,
                marker: m,
                detail: e,
            }),
        },
        DHT => {
            let (tables, err) = parse_dht(p, seg.offset);
            h.dht.extend(tables);
            if let Some(e) = err {
                h.issues.push(HeaderIssue::BadSegment {
                    offset: seg.offset,
                    marker: m,
                    detail: e,
                });
            }
        }
        DQT => {
            let (tables, err) = parse_dqt(p, seg.offset);
            h.dqt.extend(tables);
            if let Some(e) = err {
                h.issues.push(HeaderIssue::BadSegment {
                    offset: seg.offset,
                    marker: m,
                    detail: e,
                });
            }
        }
        DRI => {
            if let Some(ri) = u16be(p, 0) {
                h.dri = Some(ri);
            }
        }
        SOS => match parse_sos(p, seg.offset, seg.end()) {
            Ok(s) => h.scans.push(s),
            Err(e) => h.issues.push(HeaderIssue::BadSegment {
                offset: seg.offset,
                marker: m,
                detail: e,
            }),
        },
        DAC => h
            .notes
            .push("DAC segment (arithmetic conditioning) present".into()),
        DNL | DHP | EXP => h
            .notes
            .push(format!("{} segment in header", marker_name(m))),
        m if is_app(m) => parse_app(h, seg, p),
        _ => {}
    }
}

/// Parse a SOF payload.
pub(crate) fn parse_sof(m: u8, offset: u64, p: &[u8]) -> std::result::Result<FrameInfo, String> {
    if p.len() < 6 {
        return Err("SOF too short".into());
    }
    let nf = p[5] as usize;
    if nf == 0 || p.len() < 6 + 3 * nf {
        return Err(format!("SOF declares {nf} components in {} bytes", p.len()));
    }
    let components = (0..nf)
        .map(|i| Component {
            id: p[6 + 3 * i],
            h: p[7 + 3 * i] >> 4,
            v: p[7 + 3 * i] & 15,
            tq: p[8 + 3 * i],
        })
        .collect();
    Ok(FrameInfo {
        marker: m,
        offset,
        precision: p[0],
        height: u16be(p, 1).unwrap_or(0),
        width: u16be(p, 3).unwrap_or(0),
        components,
        progressive: matches!(m, 0xC2 | 0xC6 | 0xCA | 0xCE),
        lossless: matches!(m, 0xC3 | 0xC7 | 0xCB | 0xCF),
        arithmetic: m >= 0xC9,
        differential: matches!(m, 0xC5 | 0xC6 | 0xC7 | 0xCD | 0xCE | 0xCF),
    })
}

/// Parse a DHT payload into tables; the error names the first bad table.
pub(crate) fn parse_dht(p: &[u8], offset: u64) -> (Vec<HuffmanTable>, Option<String>) {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < p.len() {
        let tc_th = p[i];
        let (class, id) = (tc_th >> 4, tc_th & 15);
        if class > 1 || id > 3 {
            return (out, Some(format!("table class {class} id {id}")));
        }
        let Some(bits_slice) = p.get(i + 1..i + 17) else {
            return (out, Some("table truncated".into()));
        };
        let mut bits = [0u8; 16];
        bits.copy_from_slice(bits_slice);
        let n: usize = bits.iter().map(|&b| b as usize).sum();
        let Some(values) = p.get(i + 17..i + 17 + n) else {
            return (out, Some("table values truncated".into()));
        };
        if let Err(e) = HuffDecoder::new(&bits, values) {
            return (out, Some(format!("invalid table {class}/{id}: {e}")));
        }
        out.push(HuffmanTable {
            class,
            id,
            bits,
            values: values.to_vec(),
            offset,
        });
        i += 17 + n;
    }
    (out, None)
}

/// Parse a DQT payload.
pub(crate) fn parse_dqt(p: &[u8], offset: u64) -> (Vec<QuantTable>, Option<String>) {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < p.len() {
        let (pq, tq) = (p[i] >> 4, p[i] & 15);
        if pq > 1 || tq > 3 {
            return (out, Some(format!("precision {pq} id {tq}")));
        }
        let n = if pq == 0 { 64 } else { 128 };
        let Some(raw) = p.get(i + 1..i + 1 + n) else {
            return (out, Some("table truncated".into()));
        };
        let values: Vec<u16> = if pq == 0 {
            raw.iter().map(|&b| b as u16).collect()
        } else {
            raw.chunks_exact(2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .collect()
        };
        out.push(QuantTable {
            id: tq,
            precision: pq,
            values,
            offset,
        });
        i += 1 + n;
    }
    (out, None)
}

/// Parse an SOS payload; `data_offset` is the first entropy byte.
pub(crate) fn parse_sos(
    p: &[u8],
    offset: u64,
    data_offset: u64,
) -> std::result::Result<ScanInfo, String> {
    let ns = *p.first().ok_or("empty SOS")? as usize;
    if !(1..=4).contains(&ns) || p.len() != 1 + 2 * ns + 3 {
        return Err(format!("SOS with {ns} components in {} bytes", p.len()));
    }
    let components = (0..ns)
        .map(|j| ScanComponent {
            selector: p[1 + 2 * j],
            dc_table: p[2 + 2 * j] >> 4,
            ac_table: p[2 + 2 * j] & 15,
        })
        .collect();
    let k = 1 + 2 * ns;
    Ok(ScanInfo {
        offset,
        data_offset,
        components,
        ss: p[k],
        se: p[k + 1],
        ah: p[k + 2] >> 4,
        al: p[k + 2] & 15,
    })
}

fn parse_app(h: &mut Headers, seg: &Segment, p: &[u8]) {
    let m = seg.marker;
    let kind = if m == APP0 && p.starts_with(b"JFIF\0") {
        h.jfif = true;
        AppKind::Jfif
    } else if m == APP0 && p.starts_with(b"JFXX\0") {
        if p.get(5) == Some(&0x10) && p.get(6..8) == Some(&[0xFF, 0xD8]) {
            h.thumbnails.push(Range::new(
                seg.data_offset + 6,
                seg.data_offset + p.len() as u64,
            ));
        }
        AppKind::Jfxx
    } else if m == APP0 && p.starts_with(b"AVI1") {
        h.avi1 = true;
        AppKind::Avi1
    } else if m == APP1 && p.starts_with(b"Exif\0") {
        if h.exif.is_none() {
            h.exif = parse_app1(p, seg.data_offset);
            if let Some(t) = h.exif.as_ref().and_then(|e| e.thumbnail) {
                h.thumbnails.push(t);
            }
        }
        AppKind::Exif
    } else if m == APP1 && p.starts_with(b"http://ns.adobe.com/xap/1.0/\0") {
        merge_hint(h, parse_xmp(p));
        AppKind::Xmp
    } else if m == APP1 && p.starts_with(b"http://ns.adobe.com/xmp/extension/\0") {
        merge_hint(h, parse_xmp(p));
        AppKind::ExtendedXmp
    } else if m == APP2 && p.starts_with(b"ICC_PROFILE\0") {
        AppKind::Icc
    } else if m == APP2 && p.starts_with(b"MPF\0") {
        if h.mpf.is_none() {
            h.mpf = parse_mpf(p, seg.data_offset);
        }
        AppKind::Mpf
    } else if m == APP13 && p.starts_with(b"Photoshop 3.0\0") {
        if let Some(t) = photoshop_thumbnail(p, seg.data_offset) {
            h.thumbnails.push(t);
        }
        AppKind::Photoshop
    } else if m == APP14 && p.starts_with(b"Adobe") {
        h.adobe_transform = p.get(11).copied();
        AppKind::Adobe
    } else {
        let id: String = p
            .iter()
            .take(32)
            .take_while(|&&c| c != 0)
            .filter(|c| c.is_ascii_graphic() || **c == b' ')
            .map(|&c| c as char)
            .collect();
        AppKind::Other(id)
    };
    h.app_segments.push(AppSegment {
        marker: m,
        offset: seg.offset,
        len: seg.len,
        kind,
    });
}

fn merge_hint(h: &mut Headers, hint: Option<MotionPhotoHint>) {
    let Some(new) = hint else {
        return;
    };
    let cur = h.motion_photo.get_or_insert_with(MotionPhotoHint::default);
    cur.motion_photo |= new.motion_photo;
    cur.micro_video_offset = cur.micro_video_offset.or(new.micro_video_offset);
    cur.video_length = cur.video_length.or(new.video_length);
}

/// Extract motion-photo hints from an XMP packet (plain substring search;
/// no XML parser is needed for these few keys).
pub(crate) fn parse_xmp(p: &[u8]) -> Option<MotionPhotoHint> {
    let text = String::from_utf8_lossy(p);
    let mut hint = MotionPhotoHint::default();
    for key in [
        "MotionPhoto=\"1\"",
        "MicroVideo=\"1\"",
        "<GCamera:MotionPhoto>1",
        "<GCamera:MicroVideo>1",
    ] {
        if text.contains(key) {
            hint.motion_photo = true;
        }
    }
    hint.micro_video_offset = number_after(&text, "MicroVideoOffset");
    if let Some(i) = text.find("Semantic=\"MotionPhoto\"") {
        hint.motion_photo = true;
        let window_start = text[..i]
            .rfind("<Container:Item")
            .unwrap_or(i.saturating_sub(400));
        let window_end = (i + 400).min(text.len());
        if let Some(w) = text.get(window_start..window_end) {
            hint.video_length = number_after(w, "Length");
        }
    }
    (hint != MotionPhotoHint::default()).then_some(hint)
}

/// The decimal number following `key` and an `="` / `>` separator.
fn number_after(text: &str, key: &str) -> Option<u64> {
    let i = text.find(key)? + key.len();
    let rest = text.get(i..)?;
    let rest = rest.trim_start_matches(['=', '"', '>', ' ', '\'']);
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() || digits.len() > 18 {
        return None;
    }
    digits.parse().ok()
}

/// Photoshop IRB resource 0x040C (or 0x0409) thumbnail, file-relative.
fn photoshop_thumbnail(p: &[u8], data_offset: u64) -> Option<Range> {
    let mut i = 14usize;
    for _ in 0..1000 {
        if p.get(i..i + 4)? != b"8BIM" {
            return None;
        }
        let id = u16be(p, i + 4)?;
        let name_len = *p.get(i + 6)? as usize;
        let mut j = i + 7 + name_len;
        if (1 + name_len) % 2 == 1 {
            j += 1;
        }
        let size = u32be(p, j)? as usize;
        let data = j + 4;
        if data.checked_add(size)? > p.len() {
            return None;
        }
        if (id == 0x040C || id == 0x0409)
            && size > 30
            && p.get(data + 28..data + 30) == Some(&[0xFF, 0xD8])
        {
            return Some(Range::with_len(
                data_offset + (data + 28) as u64,
                (size - 28) as u64,
            ));
        }
        i = data + size + (size & 1);
    }
    None
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::cursor::SliceSource;

    /// A tiny but structurally complete baseline header + fake scan.
    pub(crate) fn tiny_jpeg() -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8];
        v.extend_from_slice(&[0xFF, 0xE0, 0, 16]);
        v.extend_from_slice(b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0");
        v.extend_from_slice(&[0xFF, 0xDB, 0, 67, 0]);
        v.extend_from_slice(&[1u8; 64]);
        v.extend_from_slice(&[0xFF, 0xC0, 0, 11, 8, 0, 8, 0, 8, 1, 1, 0x11, 0]);
        v.extend_from_slice(&crate::tables::standard_dht());
        v.extend_from_slice(&[0xFF, 0xDA, 0, 8, 1, 1, 0x00, 0, 63, 0]);
        // DC cat 0 ("00") + EOB ("1010") padded with ones: 0b0010_1011.
        v.push(0x2B);
        v.extend_from_slice(&[0xFF, 0xD9]);
        v
    }

    #[test]
    fn walks_a_clean_header() {
        let d = tiny_jpeg();
        let h = walk_headers(&SliceSource(&d), 0, d.len() as u64).unwrap();
        assert_eq!(h.end, HeaderEnd::Sos);
        assert!(h.reached_scan());
        assert!(h.jfif);
        let sof = h.sof.as_ref().unwrap();
        assert_eq!((sof.width, sof.height), (8, 8));
        assert_eq!(sof.mcus_x(), 1);
        assert_eq!(h.dht.len(), 4);
        assert!(h.dht.iter().all(|t| t.is_standard()));
        assert_eq!(h.dqt.len(), 1);
        assert!(h.issues.is_empty(), "{:?}", h.issues);
        assert!(h.missing_huffman_tables().is_empty());
        assert!(h.missing_quant_tables().is_empty());
        assert_eq!(h.scans[0].components.len(), 1);
        assert_eq!(h.scan_data_offset, Some(d.len() as u64 - 3));
    }

    #[test]
    fn tolerates_fill_double_soi_and_stray_bytes() {
        let d = tiny_jpeg();
        // FF D8 FF D8 + fill bytes + 3 stray bytes after APP0.
        let mut x = vec![0xFF, 0xD8];
        x.extend_from_slice(&d[..20]);
        x.extend_from_slice(&[0x00, 0x13, 0x37]);
        x.extend_from_slice(&[0xFF, 0xFF]);
        x.extend_from_slice(&d[20..]);
        let h = walk_headers(&SliceSource(&x), 0, x.len() as u64).unwrap();
        assert!(h.reached_scan(), "{:?} {:?}", h.end, h.issues);
        assert!(h
            .issues
            .iter()
            .any(|i| matches!(i, HeaderIssue::DoubleSoi { .. })));
        // APP0 is opaque, so the three bytes are attributed to it.
        assert!(h
            .issues
            .iter()
            .any(|i| matches!(i, HeaderIssue::BadLength { marker: APP0, .. })));
    }

    #[test]
    fn repairs_a_too_long_app_length() {
        let mut d = tiny_jpeg();
        d[5] = 200; // APP0 length 16 -> 200
        let h = walk_headers(&SliceSource(&d), 0, d.len() as u64).unwrap();
        assert!(h.reached_scan(), "{:?} {:?}", h.end, h.issues);
        assert!(h.issues.iter().any(|i| matches!(
            i,
            HeaderIssue::BadLength {
                marker: APP0,
                declared: 200,
                fixed: 16,
                ..
            }
        )));
    }

    #[test]
    fn stops_at_eoi_and_nested_soi_and_truncation() {
        let d = tiny_jpeg();
        let mut e = d[..20].to_vec();
        e.extend_from_slice(&[0xFF, 0xD9]);
        let h = walk_headers(&SliceSource(&e), 0, e.len() as u64).unwrap();
        assert_eq!(h.end, HeaderEnd::Eoi { offset: 20 });
        let mut n = d[..20].to_vec();
        n.extend_from_slice(&d);
        let h = walk_headers(&SliceSource(&n), 0, n.len() as u64).unwrap();
        assert_eq!(h.end, HeaderEnd::NestedSoi { offset: 20 });
        for cut in 2..d.len() {
            let h = walk_headers(&SliceSource(&d[..cut]), 0, cut as u64).unwrap();
            if cut < d.len() - 5 {
                assert!(!h.reached_scan() || cut >= 30, "cut {cut}");
            }
        }
        assert!(walk_headers(&SliceSource(b"\x00\x01"), 0, 2).is_err());
        assert!(walk_headers(&SliceSource(b""), 0, 0).is_err());
    }

    #[test]
    fn xmp_hints() {
        let x = b"http://ns.adobe.com/xap/1.0/\0<x GCamera:MotionPhoto=\"1\" GCamera:MicroVideoOffset=\"12345\"/>";
        let h = parse_xmp(x).unwrap();
        assert!(h.motion_photo);
        assert_eq!(h.micro_video_offset, Some(12345));
        let y = b"<Container:Item Item:Mime=\"video/mp4\" Item:Semantic=\"MotionPhoto\" Item:Length=\"999\"/>";
        let h = parse_xmp(y).unwrap();
        assert_eq!(h.video_length, Some(999));
        assert!(parse_xmp(b"nothing here").is_none());
    }

    #[test]
    fn frame_checks() {
        let d = tiny_jpeg();
        let h = walk_headers(&SliceSource(&d), 0, d.len() as u64).unwrap();
        let mut f = h.sof.unwrap();
        assert!(f.check().is_ok());
        f.components[0].h = 5;
        assert!(f.check().is_err());
        f.components[0].h = 1;
        f.width = 0;
        assert!(f.check().is_err());
        assert_eq!(marker_name(0xC2), "SOF2");
        assert_eq!(marker_name(0xE1), "APP1");
        assert_eq!(marker_name(0xD3), "RST3");
    }

    #[test]
    fn random_input_never_panics() {
        let mut x = 0x9E37_79B9u32;
        let base = tiny_jpeg();
        for round in 0..3000 {
            let mut d = base.clone();
            let n = 1 + round % 8;
            for _ in 0..n {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                let p = 2 + (x as usize) % (d.len() - 2);
                d[p] = (x >> 11) as u8;
            }
            let cut = (x as usize >> 3) % (d.len() + 1);
            let _ = walk_headers(&SliceSource(&d[..cut]), 0, u64::MAX);
            let _ = walk_headers(&SliceSource(&d), 0, d.len() as u64);
        }
    }
}
