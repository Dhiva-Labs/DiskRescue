//! A bounded, streaming TIFF / Exif / DNG structure walker.
//!
//! [`walk`] follows the IFD graph of a TIFF-structured file (plain TIFF,
//! BigTIFF, Exif blobs and every TIFF-based camera RAW) and computes its
//! **extent**: the largest `offset + length` over every IFD, every
//! out-of-line tag value and every strip, tile, JPEG preview or private blob
//! the IFDs point at. For files written in one piece, which is what cameras
//! and libtiff do, that maximum is the file length.
//!
//! # What is followed
//!
//! * the IFD0 -> IFD1 -> ... chain (the next-IFD pointer);
//! * `SubIFDs` (0x014A), `ExifIFD` (0x8769), `GPSInfo` (0x8825) and
//!   `InteropIFD` (0xA005);
//! * the CR2 raw IFD pointer in the header (byte 12);
//! * the Sony `SR2Private` IFD (via `DNGPrivateData`, 0xC634);
//! * maker-note preview pointers (Nikon `PreviewIFD`, Pentax
//!   `PreviewImageStart`, Olympus `CameraSettings` preview, Sony
//!   `PreviewImage`), see the `makernote` module.
//!
//! # Bounds and hostile input
//!
//! Every offset is checked against the readable window before it is used.
//! At most [`MAX_IFDS`] IFDs of at most [`MAX_ENTRIES`] entries are read;
//! visited IFD offsets are remembered so cycles end; pointers nest at most
//! six levels deep; arrays of strip offsets are cut at 2^20 entries. There is
//! no recursion. Out-of-line *values* that point far beyond the readable data
//! (more than 16 MiB) are ignored rather than trusted, because a single
//! flipped bit in a count would otherwise inflate the extent of a healthy
//! file; strips, tiles and previews are always trusted because they *are*
//! the payload.
//!
//! # Limits
//!
//! Compressed data is never decoded. Panasonic RW2 files do not store the
//! length of their raw data; the walk reports that fact in
//! [`TiffWalk::rw2_raw_offset`] and leaves the estimate to the caller.

use std::collections::{HashSet, VecDeque};

use diskrescue_core::format::NestedRole;
use diskrescue_core::source::SharedSource;
use diskrescue_core::util::bytes::{ascii_trimmed, Endian};
use diskrescue_core::{Range, Result};

use crate::jpegmini;
use crate::makernote;
use crate::win::{read_error, Fail, Win};

/// Most IFDs read from one file.
pub const MAX_IFDS: usize = 64;
/// Most entries accepted in one IFD.
pub const MAX_ENTRIES: u64 = 4096;
const MAX_DEPTH: u32 = 6;
const MAX_CHAIN: usize = 16;
const MAX_ARRAY: u64 = 1 << 18;
/// Total bytes of value arrays (strip and tile tables) read from one file.
const ARRAY_BUDGET: u64 = 32 << 20;
const MAX_TEXT: u64 = 1024;
const MAX_NOTES: usize = 32;
/// How far an out-of-line value may poke past the readable data before it is
/// considered garbage rather than evidence of truncation.
const VALUE_SLACK: u64 = 16 << 20;

/// Which header variant to expect at the start of the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Magic {
    /// `II*\0` / `MM\0*`: TIFF 6.0, Exif, CR2, NEF, ARW, DNG, PEF, ...
    Classic,
    /// Olympus ORF: `IIRO`, `IIRS` or `MMOR`.
    Orf,
    /// Panasonic RW2 / Leica RWL: `IIU\0`.
    Rw2,
    /// BigTIFF: `II+\0` / `MM\0+` with 8-byte offsets and 20-byte entries.
    BigTiff,
}

impl Magic {
    /// Identify the header variant and byte order from the first bytes.
    pub fn detect(head: &[u8]) -> Option<(Magic, Endian)> {
        match head.get(..4)? {
            b"II*\0" => Some((Magic::Classic, Endian::Little)),
            b"MM\0*" => Some((Magic::Classic, Endian::Big)),
            b"IIRO" | b"IIRS" => Some((Magic::Orf, Endian::Little)),
            b"MMOR" => Some((Magic::Orf, Endian::Big)),
            b"IIU\0" => Some((Magic::Rw2, Endian::Little)),
            b"II+\0" => Some((Magic::BigTiff, Endian::Little)),
            b"MM\0+" => Some((Magic::BigTiff, Endian::Big)),
            _ => None,
        }
    }
}

/// Size in bytes of one value of TIFF type `typ`; `None` for unknown types.
pub fn type_size(typ: u16) -> Option<u64> {
    match typ {
        1 | 2 | 6 | 7 => Some(1),
        3 | 8 => Some(2),
        4 | 9 | 11 | 13 => Some(4),
        5 | 10 | 12 | 16 | 17 | 18 => Some(8),
        _ => None,
    }
}

/// One IFD entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Tag number.
    pub tag: u16,
    /// Field type (1 BYTE ... 13 IFD, 16..18 BigTIFF 64-bit types).
    pub typ: u16,
    /// Number of values.
    pub count: u64,
    /// The value/offset field read as an unsigned integer in file byte
    /// order. When [`Entry::inline`] is false this is the offset of the
    /// value data.
    pub value_or_offset: u64,
    /// True when the values fit in the value/offset field itself.
    pub inline: bool,
    raw: [u8; 8],
}

impl Entry {
    /// Total size of the values in bytes, `None` for unknown types or overflow.
    pub fn byte_len(&self) -> Option<u64> {
        self.count.checked_mul(type_size(self.typ)?)
    }

    /// The `i`-th value of an inline numeric entry.
    pub fn inline_value(&self, endian: Endian, i: usize) -> Option<u64> {
        if !self.inline || i as u64 >= self.count {
            return None;
        }
        match self.typ {
            1 | 2 | 6 | 7 => self.raw.get(i).map(|b| *b as u64),
            3 | 8 => endian.u16(&self.raw, i * 2).map(u64::from),
            4 | 9 | 11 | 13 => endian.u32(&self.raw, i * 4).map(u64::from),
            16..=18 => endian.u64(&self.raw, i * 8),
            _ => None,
        }
    }

    /// First value of an inline numeric entry.
    pub fn scalar(&self, endian: Endian) -> Option<u64> {
        self.inline_value(endian, 0)
    }

    /// The raw inline bytes (valid when [`Entry::inline`]).
    pub fn inline_bytes(&self) -> &[u8] {
        let n = self.byte_len().unwrap_or(0).min(8) as usize;
        &self.raw[..n]
    }
}

/// One Image File Directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ifd {
    /// File-relative offset of the IFD.
    pub offset: u64,
    /// Its entries, in file order.
    pub entries: Vec<Entry>,
    /// Offset of the next IFD in the chain, 0 when this is the last.
    pub next: u64,
}

impl Ifd {
    /// First entry with the given tag.
    pub fn find(&self, tag: u16) -> Option<&Entry> {
        self.entries.iter().find(|e| e.tag == tag)
    }
}

/// An embedded JPEG preview or thumbnail found by the walk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preview {
    /// File-relative offset of the `FF D8`.
    pub offset: u64,
    /// Length in bytes as declared by the file.
    pub len: u64,
    /// Thumbnail or larger preview.
    pub role: NestedRole,
    /// Pixel width from the JPEG frame header, when seen.
    pub width: Option<u32>,
    /// Pixel height from the JPEG frame header, when seen.
    pub height: Option<u32>,
}

/// Everything the walk learned about a TIFF-structured file.
#[derive(Debug, Clone)]
pub struct TiffWalk {
    /// Byte order of the file.
    pub endian: Endian,
    /// Header variant.
    pub magic: Magic,
    /// Offset of IFD0.
    pub ifd0_offset: u64,
    /// The main IFD chain: IFD0, IFD1, IFD2, ...
    pub ifds: Vec<Ifd>,
    /// IFDs reached through `SubIFDs` tags.
    pub sub_ifds: Vec<Ifd>,
    /// The first Exif IFD.
    pub exif_ifd: Option<Ifd>,
    /// Other IFDs read: GPS, Interop, the CR2 raw IFD when not in the chain,
    /// the Sony SR2 private IFD.
    pub extra_ifds: Vec<Ifd>,
    /// `max(offset + length)` over every IFD, value, strip, tile, JPEG and
    /// private-data pointer in all IFDs; the extent of the file as declared
    /// by its own structure.
    pub max_extent: u64,
    /// Camera make (IFD0).
    pub make: Option<String>,
    /// Camera model (IFD0).
    pub model: Option<String>,
    /// Software string (IFD0).
    pub software: Option<String>,
    /// `DateTimeOriginal` (Exif IFD), raw `YYYY:MM:DD HH:MM:SS` text.
    pub date_time_original: Option<String>,
    /// `DateTimeDigitized` (Exif IFD).
    pub date_time_digitized: Option<String>,
    /// `DateTime` (IFD0).
    pub date_time: Option<String>,
    /// Exif orientation 1..=8 (IFD0).
    pub orientation: Option<u8>,
    /// Pixel width: Exif `PixelXDimension`, else the largest image IFD.
    pub width: Option<u32>,
    /// Pixel height, as above.
    pub height: Option<u32>,
    /// `DNGVersion` when present in IFD0.
    pub dng_version: Option<[u8; 4]>,
    /// Embedded JPEG previews and thumbnails whose `FF D8` was seen.
    pub previews: Vec<Preview>,
    /// CR2 header: `(major, minor)` of the `CR` marker at byte 8, and the
    /// offset of the raw IFD stored at byte 12.
    pub cr2: Option<(u8, u8, u64)>,
    /// CR2 `RawImageSegmentation` (0xC640): slice count minus one, slice
    /// width and last slice width.
    pub cr2_slices: Option<[u16; 3]>,
    /// Panasonic `RawDataOffset` (0x0118), whose data has no stored length.
    pub rw2_raw_offset: Option<u64>,
    /// Panasonic `RawFormat` (0x002D).
    pub rw2_raw_format: Option<u16>,
    /// Panasonic sensor width and height (0x0002 / 0x0003).
    pub rw2_sensor: Option<(u32, u32)>,
    /// Sony `FileFormat` maker-note bytes (0xB000): 01 = SR2, 02.. = ARW.
    pub sony_file_format: Option<[u8; 4]>,
    /// Offset and length of the Exif `MakerNote`.
    pub makernote: Option<(u64, u64)>,
    /// Compression code (0x0103) of the largest image IFD.
    pub compression: Option<u32>,
    /// Number of strips, tiles and JPEG blobs that contributed to the extent.
    pub payload_count: u32,
    /// Number of structural oddities noted (unsorted tags, mismatched strip
    /// counts, bad offsets); each lowers confidence.
    pub anomalies: u32,
    /// Bytes readable from the start of the file in the window used.
    pub avail: u64,
    /// True when the window ended because of the caller's limit.
    pub capped: bool,
    /// First offset at which the medium failed during the walk.
    pub read_error: Option<u64>,
    /// Observations and problems, for reports.
    pub notes: Vec<String>,
}

impl TiffWalk {
    /// Total IFDs read.
    pub fn ifd_count(&self) -> usize {
        self.ifds.len() + self.sub_ifds.len() + self.exif_ifd.iter().count() + self.extra_ifds.len()
    }
}

/// Why a walk could not start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reject {
    /// The bytes are not this header variant.
    NotTiff,
    /// Header fine, but IFD0 does not look like one.
    Implausible,
    /// IFD0 lies beyond the readable window, so nothing can be verified.
    Beyond,
    /// The medium failed at this offset while reading the header or IFD0.
    Io(u64),
}

/// Header facts available before the full walk.
#[derive(Debug, Clone)]
pub(crate) struct Head {
    pub endian: Endian,
    pub magic: Magic,
    pub ifd0_offset: u64,
    /// CR2: `(major, minor, raw IFD offset)`.
    pub cr2: Option<(u8, u8, u64)>,
    pub make: Option<String>,
    pub model: Option<String>,
    pub dng_version: Option<[u8; 4]>,
}

/// Tags that occur in real IFD0s; used to reject garbage that happens to
/// have a plausible header.
fn is_known_tag(tag: u16) -> bool {
    const KNOWN: &[u16] = &[
        0x00FE, 0x00FF, 0x0100, 0x0101, 0x0102, 0x0103, 0x0106, 0x0107, 0x0108, 0x0109, 0x010A,
        0x010D, 0x010E, 0x010F, 0x0110, 0x0111, 0x0112, 0x0115, 0x0116, 0x0117, 0x0118, 0x0119,
        0x011A, 0x011B, 0x011C, 0x011D, 0x0122, 0x0123, 0x0124, 0x0125, 0x0128, 0x012D, 0x0131,
        0x0132, 0x013B, 0x013C, 0x013D, 0x013E, 0x013F, 0x0140, 0x0141, 0x0142, 0x0143, 0x0144,
        0x0145, 0x014A, 0x0152, 0x015B, 0x0201, 0x0202, 0x0211, 0x0212, 0x0213, 0x0214, 0x02BC,
        0x8298, 0x83BB, 0x8649, 0x8769, 0x8773, 0x8825, 0xA005,
    ];
    KNOWN.binary_search(&tag).is_ok()
        || (0xC612..=0xC7FF).contains(&tag)
        || (0x0001..=0x0010).contains(&tag)
        || (0x002D..=0x002F).contains(&tag)
        || (0x8000..=0x80FF).contains(&tag)
        || (0x9000..=0x92FF).contains(&tag)
        || (0xA000..=0xA4FF).contains(&tag)
        || (0xB000..=0xB0FF).contains(&tag)
}

fn plausible_ifd0(ifd: &Ifd) -> bool {
    let n = ifd.entries.len();
    if n == 0 {
        return false;
    }
    let valid = ifd
        .entries
        .iter()
        .filter(|e| type_size(e.typ).is_some() && e.count > 0)
        .count();
    if valid * 4 < n * 3 {
        return false;
    }
    let known = ifd.entries.iter().filter(|e| is_known_tag(e.tag)).count();
    known >= n.min(2)
}

/// Parse the IFD at `off`. `Ok(None)` when the entry count is 0 or beyond
/// [`MAX_ENTRIES`]; `Err` when the IFD is not fully readable.
pub(crate) fn parse_ifd(
    win: &Win,
    off: u64,
    endian: Endian,
    big: bool,
) -> std::result::Result<Option<Ifd>, Fail> {
    if off >= win.avail() {
        return Err(Fail::Eof(win.avail()));
    }
    let csz: u64 = if big { 8 } else { 2 };
    let hdr = win.read(off, csz as usize)?;
    let n = if big {
        endian.u64(&hdr, 0)
    } else {
        endian.u16(&hdr, 0).map(u64::from)
    }
    .unwrap_or(0);
    if n == 0 || n > MAX_ENTRIES {
        return Ok(None);
    }
    let (esz, nsz): (usize, usize) = if big { (20, 8) } else { (12, 4) };
    let body = win.read(off + csz, n as usize * esz + nsz)?;
    let mut entries = Vec::with_capacity(n as usize);
    for i in 0..n as usize {
        let b = &body[i * esz..(i + 1) * esz];
        let tag = endian.u16(b, 0).unwrap_or(0);
        let typ = endian.u16(b, 2).unwrap_or(0);
        let (count, value, mut raw) = if big {
            (
                endian.u64(b, 4).unwrap_or(0),
                endian.u64(b, 12).unwrap_or(0),
                [0u8; 8],
            )
        } else {
            (
                endian.u32(b, 4).unwrap_or(0) as u64,
                endian.u32(b, 8).unwrap_or(0) as u64,
                [0u8; 8],
            )
        };
        let vlen = if big { 8 } else { 4 };
        raw[..vlen].copy_from_slice(&b[esz - vlen..]);
        let inline = match type_size(typ).and_then(|s| count.checked_mul(s)) {
            Some(len) => len <= vlen as u64,
            None => false,
        };
        entries.push(Entry {
            tag,
            typ,
            count,
            value_or_offset: value,
            inline,
            raw,
        });
    }
    let nb = &body[n as usize * esz..];
    let next = if big {
        endian.u64(nb, 0).unwrap_or(0)
    } else {
        endian.u32(nb, 0).unwrap_or(0) as u64
    };
    Ok(Some(Ifd {
        offset: off,
        entries,
        next,
    }))
}

/// Numeric values of an entry (BYTE, SHORT, LONG, IFD, LONG8 families).
/// Out-of-line data is read from `base + offset`. At most `cap` values.
pub(crate) fn read_uints(
    win: &Win,
    endian: Endian,
    e: &Entry,
    base: u64,
    cap: u64,
) -> std::result::Result<Vec<u64>, Fail> {
    let Some(sz) = type_size(e.typ) else {
        return Ok(Vec::new());
    };
    if !matches!(e.typ, 1 | 3 | 4 | 6 | 7 | 8 | 9 | 13 | 16 | 17 | 18) {
        return Ok(Vec::new());
    }
    let n = e.count.min(cap);
    if e.inline {
        return Ok((0..n as usize)
            .filter_map(|i| e.inline_value(endian, i))
            .collect());
    }
    let off = base.saturating_add(e.value_or_offset);
    let bytes = win.read(off, (n * sz) as usize)?;
    let mut out = Vec::with_capacity(n as usize);
    for i in 0..n as usize {
        let v = match sz {
            1 => bytes.get(i).map(|b| *b as u64),
            2 => endian.u16(&bytes, i * 2).map(u64::from),
            4 => endian.u32(&bytes, i * 4).map(u64::from),
            _ => endian.u64(&bytes, i * 8),
        };
        if let Some(v) = v {
            out.push(v);
        }
    }
    Ok(out)
}

/// ASCII text of an entry (at most 1 KiB).
pub(crate) fn read_text(win: &Win, e: &Entry) -> std::result::Result<String, Fail> {
    let n = e.count.min(MAX_TEXT) as usize;
    if e.inline {
        return Ok(ascii_trimmed(&e.inline_bytes()[..n.min(8)]));
    }
    let bytes = win.read(e.value_or_offset, n)?;
    Ok(ascii_trimmed(&bytes))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Main,
    Sub,
    Exif,
    Gps,
    Interop,
    Cr2Raw,
    Sr2,
}

#[derive(Clone, Copy, Debug)]
struct Job {
    off: u64,
    kind: Kind,
    depth: u32,
    idx: usize,
}

#[derive(Default)]
struct IfdInfo {
    width: Option<u32>,
    height: Option<u32>,
    compression: Option<u32>,
    subfile: Option<u32>,
    bits: Option<u32>,
    spp: Option<u32>,
    strip_offsets: Vec<u64>,
    strip_counts: Vec<u64>,
    tile_offsets: Vec<u64>,
    tile_counts: Vec<u64>,
    jpeg_off: Option<u64>,
    jpeg_len: Option<u64>,
}

/// Most offsets kept in each list of [`Refs`].
const MAX_REFS: usize = 1 << 16;
/// Leading bytes of an embedded JPEG that may hold APPn segments (Exif with
/// its own thumbnail, MPF) and therefore other JPEG or TIFF signatures.
const JPEG_HEADER_SPAN: u64 = 128 * 1024;
/// Sector grid on which other files would start (see `crate::foreign`).
const GRID: u64 = 512;

/// What the walk learned about where things are, so that the caller can
/// check the declared extent against the bytes (`raw::verify_extent`).
/// Offsets are file-relative.
#[derive(Debug, Clone, Default)]
pub(crate) struct Refs {
    /// Offsets on the sector grid that an IFD entry points at (strips,
    /// tiles, JPEG streams, out-of-line values). A JPEG start there is part
    /// of the file.
    pub pointed: Vec<u64>,
    /// Offsets where a JPEG stream starts (`FF D8`): `JPEGInterchangeFormat`,
    /// RW2 `JpgFromRaw` and maker-note previews always, the strips and tiles
    /// of a JPEG-compressed image as a group (see [`JpegRef::group`]).
    pub jpeg_starts: Vec<JpegRef>,
    /// Ranges that may legitimately hold JPEG or TIFF signatures anywhere:
    /// out-of-line values (maker notes, Photoshop resources, embedded
    /// original raw files) and the leading bytes of embedded JPEG previews.
    pub exempt: Vec<Range>,
    /// End of the furthest plausible IFD the walk parsed.
    pub structure_end: u64,
}

/// An offset where the structure says a JPEG stream starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct JpegRef {
    /// File-relative offset.
    pub off: u64,
    /// Tag that points there.
    pub tag: u16,
    /// For the strips or tiles of one JPEG-compressed image: an id shared
    /// by all of them. Old-style JPEG (compression 6) strips need not start
    /// with SOI, and some writers put other data in compression 7 segments,
    /// so a missing `FF D8` only counts when the first segment of the group
    /// has one.
    pub group: Option<u32>,
}

impl Refs {
    fn point(&mut self, start: u64, off: u64) {
        if self.pointed.len() < MAX_REFS * 16 && start.wrapping_add(off) % GRID == 0 {
            self.pointed.push(off);
        }
    }

    fn jpeg(&mut self, off: u64, tag: u16, group: Option<u32>) {
        if self.jpeg_starts.len() < MAX_REFS {
            self.jpeg_starts.push(JpegRef { off, tag, group });
        }
    }

    fn exempt(&mut self, off: u64, len: u64) {
        if len >= 4 && self.exempt.len() < MAX_REFS {
            self.exempt.push(Range::new(off, off.saturating_add(len)));
        }
    }

    /// Sort and deduplicate the lists for lookups.
    fn seal(&mut self) {
        self.pointed.sort_unstable();
        self.pointed.dedup();
        self.jpeg_starts.sort_unstable();
        self.jpeg_starts.dedup_by_key(|j| j.off);
        self.exempt.sort_unstable_by_key(|r| (r.start, r.end));
        self.exempt.dedup();
    }

    /// Whether an IFD entry points at `off`.
    pub(crate) fn is_pointed(&self, off: u64) -> bool {
        self.pointed.binary_search(&off).is_ok()
    }

    /// Whether `off` lies in a range that may hold JPEG or TIFF signatures.
    pub(crate) fn is_exempt(&self, off: u64) -> bool {
        self.exempt.iter().any(|r| r.contains(off))
    }
}

/// Incremental walker; `start` validates the header and IFD0, `run` does the
/// rest.
pub(crate) struct Walker<'a> {
    win: &'a Win,
    pub(crate) head: Head,
    big: bool,
    ifd0: Option<Ifd>,
    out: TiffWalk,
    visited: HashSet<u64>,
    queue: VecDeque<Job>,
    exif_dims: (Option<u32>, Option<u32>),
    best_area: u64,
    seen_previews: HashSet<(u64, u64)>,
    cr2_pending: Option<u64>,
    budget: u64,
    refs: Refs,
    groups: u32,
}

impl<'a> Walker<'a> {
    /// Validate the header of the window and read IFD0.
    pub(crate) fn start(win: &'a Win, magic: Magic) -> std::result::Result<Walker<'a>, Reject> {
        let h = win.read_upto(0, 16).map_err(|f| Reject::Io(f.offset()))?;
        let (m, endian) = Magic::detect(&h).ok_or(Reject::NotTiff)?;
        if m != magic {
            return Err(Reject::NotTiff);
        }
        let big = magic == Magic::BigTiff;
        let ifd0_offset = if big {
            if endian.u16(&h, 4) != Some(8) || endian.u16(&h, 6) != Some(0) {
                return Err(Reject::NotTiff);
            }
            endian.u64(&h, 8).ok_or(Reject::NotTiff)?
        } else {
            endian.u32(&h, 4).ok_or(Reject::NotTiff)? as u64
        };
        let min = if big { 16 } else { 8 };
        if ifd0_offset < min || (magic == Magic::Rw2 && ifd0_offset > 64 * 1024) {
            return Err(Reject::Implausible);
        }
        if ifd0_offset >= win.avail() {
            return Err(Reject::Beyond);
        }
        let ifd0 = match parse_ifd(win, ifd0_offset, endian, big) {
            Ok(Some(i)) => i,
            Ok(None) => return Err(Reject::Implausible),
            Err(Fail::Eof(_)) => return Err(Reject::Beyond),
            Err(Fail::Io(at)) => return Err(Reject::Io(at)),
        };
        if !plausible_ifd0(&ifd0) {
            return Err(Reject::Implausible);
        }
        let cr2 = if magic == Magic::Classic && h.get(8..10) == Some(b"CR") && h.len() >= 16 {
            Some((h[10], h[11], endian.u32(&h, 12).unwrap_or(0) as u64))
        } else {
            None
        };
        let text = |tag: u16| -> Option<String> {
            let e = ifd0.find(tag)?;
            if !matches!(e.typ, 2 | 1 | 7) {
                return None;
            }
            read_text(win, e).ok().filter(|s| !s.is_empty())
        };
        let make = text(0x010F);
        let model = text(0x0110);
        let dng_version = ifd0.find(0xC612).and_then(|e| {
            (e.inline && e.typ == 1 && e.count == 4)
                .then(|| [e.raw[0], e.raw[1], e.raw[2], e.raw[3]])
        });
        let head = Head {
            endian,
            magic,
            ifd0_offset,
            cr2,
            make,
            model,
            dng_version,
        };
        let out = TiffWalk {
            endian,
            magic,
            ifd0_offset,
            ifds: Vec::new(),
            sub_ifds: Vec::new(),
            exif_ifd: None,
            extra_ifds: Vec::new(),
            max_extent: if big { 16 } else { 8 },
            make: head.make.clone(),
            model: head.model.clone(),
            software: None,
            date_time_original: None,
            date_time_digitized: None,
            date_time: None,
            orientation: None,
            width: None,
            height: None,
            dng_version,
            previews: Vec::new(),
            cr2,
            cr2_slices: None,
            rw2_raw_offset: None,
            rw2_raw_format: None,
            rw2_sensor: None,
            sony_file_format: None,
            makernote: None,
            compression: None,
            payload_count: 0,
            anomalies: 0,
            avail: win.avail(),
            capped: win.capped(),
            read_error: None,
            notes: Vec::new(),
        };
        Ok(Walker {
            win,
            head,
            big,
            ifd0: Some(ifd0),
            out,
            visited: HashSet::new(),
            queue: VecDeque::new(),
            exif_dims: (None, None),
            best_area: 0,
            seen_previews: HashSet::new(),
            cr2_pending: cr2.map(|c| c.2),
            budget: ARRAY_BUDGET,
            refs: Refs::default(),
            groups: 0,
        })
    }

    /// Walk everything reachable from IFD0.
    pub(crate) fn run(self) -> TiffWalk {
        self.run_with_refs().0
    }

    /// Walk everything reachable from IFD0 and also return where the walk
    /// found references into the file.
    pub(crate) fn run_with_refs(mut self) -> (TiffWalk, Refs) {
        self.queue.push_back(Job {
            off: self.head.ifd0_offset,
            kind: Kind::Main,
            depth: 0,
            idx: 0,
        });
        loop {
            while let Some(job) = self.queue.pop_front() {
                if self.visited.len() >= MAX_IFDS {
                    self.note("IFD limit reached; remaining IFDs ignored");
                    self.queue.clear();
                    break;
                }
                if !self.visited.insert(job.off) {
                    continue;
                }
                let ifd = if job.off == self.head.ifd0_offset {
                    match self.ifd0.take() {
                        Some(i) => i,
                        None => continue,
                    }
                } else {
                    match parse_ifd(self.win, job.off, self.head.endian, self.big) {
                        Ok(Some(i)) => {
                            if job.kind == Kind::Sr2
                                && (i.find(0x7200).is_none() || i.find(0x7201).is_none())
                            {
                                self.anomaly("Sony SR2 private IFD lacks its offset/length tags");
                                continue;
                            }
                            i
                        }
                        Ok(None) => {
                            self.anomaly(format!("IFD at {} has an unusable entry count", job.off));
                            continue;
                        }
                        Err(f) => {
                            self.fail(f, "an IFD");
                            // The pointer itself is evidence of more file.
                            self.add_value_end(job.off, job.off.saturating_add(6));
                            continue;
                        }
                    }
                };
                self.process(ifd, job);
            }
            // The CR2 raw IFD is normally the end of the chain; if the chain
            // was broken, still try the pointer from the header.
            match self.cr2_pending.take() {
                Some(off) if off >= 16 && !self.visited.contains(&off) => {
                    self.queue.push_back(Job {
                        off,
                        kind: Kind::Cr2Raw,
                        depth: 1,
                        idx: 3,
                    });
                }
                _ => break,
            }
        }
        self.finish()
    }

    fn note(&mut self, s: impl Into<String>) {
        if self.out.notes.len() < MAX_NOTES {
            self.out.notes.push(s.into());
        }
    }

    fn anomaly(&mut self, s: impl Into<String>) {
        self.out.anomalies = self.out.anomalies.saturating_add(1);
        self.note(s);
    }

    fn fail(&mut self, f: Fail, what: &str) {
        match f {
            Fail::Io(at) => {
                if self.out.read_error.is_none() {
                    self.out.read_error = Some(at);
                    log::warn!(
                        "TIFF walk at {}: unreadable bytes at file offset {at} while reading {what}",
                        self.win.start()
                    );
                }
                self.note(format!("unreadable bytes at {at} while reading {what}"));
            }
            Fail::Eof(_) => self.note(format!("data for {what} lies beyond the readable bytes")),
        }
    }

    fn add_end(&mut self, end: u64) {
        if end > self.out.max_extent {
            self.out.max_extent = end;
        }
    }

    /// Account for something that is part of the payload: strips, tiles,
    /// JPEG previews, raw copies. Always trusted.
    fn add_payload(&mut self, off: u64, len: u64) {
        self.out.payload_count = self.out.payload_count.saturating_add(1);
        self.add_end(off.saturating_add(len));
    }

    /// Account for a tag value or pointer. Ignored when it points absurdly
    /// far past the readable data.
    fn add_value_end(&mut self, start: u64, end: u64) {
        let avail = self.win.avail();
        if end <= avail || (start < avail && end - avail <= VALUE_SLACK) {
            self.add_end(end);
        } else {
            self.anomaly(format!(
                "ignored a value at {start}..{end} that lies far beyond the readable data"
            ));
        }
    }

    fn uints(&mut self, e: &Entry, cap: u64) -> Vec<u64> {
        let cost = e
            .count
            .min(cap)
            .saturating_mul(type_size(e.typ).unwrap_or(1));
        if !e.inline {
            if cost > self.budget {
                self.anomaly("value-array read budget exhausted; a tag array was ignored");
                return Vec::new();
            }
            self.budget -= cost;
        }
        match read_uints(self.win, self.head.endian, e, 0, cap) {
            Ok(v) => v,
            Err(f) => {
                self.fail(f, "a tag value array");
                Vec::new()
            }
        }
    }

    fn text(&mut self, e: &Entry) -> Option<String> {
        if !matches!(e.typ, 1 | 2 | 7) {
            return None;
        }
        match read_text(self.win, e) {
            Ok(s) if !s.is_empty() => Some(s),
            Ok(_) => None,
            Err(f) => {
                self.fail(f, "a text tag");
                None
            }
        }
    }

    fn process(&mut self, ifd: Ifd, job: Job) {
        let endian = self.head.endian;
        let (csz, esz, nsz): (u64, u64, u64) = if self.big { (8, 20, 8) } else { (2, 12, 4) };
        let n = ifd.entries.len() as u64;
        let ifd_end = ifd.offset + csz + n * esz + nsz;
        self.add_end(ifd_end);
        if plausible_ifd0(&ifd) {
            self.refs.structure_end = self.refs.structure_end.max(ifd_end);
        }
        let is_ifd0 = job.kind == Kind::Main && job.idx == 0;
        if ifd.entries.windows(2).any(|w| w[1].tag <= w[0].tag) {
            self.anomaly(format!(
                "IFD at {} has unsorted or duplicate tags",
                ifd.offset
            ));
        }
        let mut info = IfdInfo::default();
        for e in &ifd.entries {
            let Some(len) = e.byte_len() else {
                continue; // unknown type or overflowing count: skip the entry
            };
            if e.count == 0 {
                continue;
            }
            let payload_tag =
                matches!(e.tag, 0xC68C) || (self.is_rw2() && matches!(e.tag, 0x002E | 0x0127));
            if !e.inline {
                if e.value_or_offset < 8 {
                    self.anomaly(format!("tag {:#06x} points into the file header", e.tag));
                    continue;
                }
                let end = e.value_or_offset.saturating_add(len);
                self.refs.point(self.win.start(), e.value_or_offset);
                // Blobs (maker notes, Photoshop resources, embedded original
                // files) may hold JPEG or TIFF headers; offset tables and
                // values that point far past the data do not count.
                let table = matches!(e.tag, 0x0111 | 0x0117 | 0x0144 | 0x0145 | 0x014A);
                let sane = end <= self.win.avail().saturating_add(VALUE_SLACK);
                if !table && (payload_tag || sane) {
                    self.refs.exempt(e.value_or_offset, len);
                }
                if payload_tag {
                    self.add_payload(e.value_or_offset, len);
                } else {
                    self.add_value_end(e.value_or_offset, end);
                }
            }
            let sc = e.scalar(endian);
            match e.tag {
                0x00FE => info.subfile = sc.map(|v| v as u32),
                0x0100 => info.width = self.dim(e, sc),
                0x0101 => info.height = self.dim(e, sc),
                0x0102 => info.bits = sc.map(|v| v as u32),
                0x0103 => info.compression = sc.map(|v| v as u32),
                0x0115 => info.spp = sc.map(|v| v as u32),
                0x0111 => info.strip_offsets = self.uints(e, MAX_ARRAY),
                0x0117 => info.strip_counts = self.uints(e, MAX_ARRAY),
                0x0144 => info.tile_offsets = self.uints(e, MAX_ARRAY),
                0x0145 => info.tile_counts = self.uints(e, MAX_ARRAY),
                0x0201 => info.jpeg_off = sc,
                0x0202 => info.jpeg_len = sc,
                0x0131 if is_ifd0 => self.out.software = self.text(e),
                0x0132 if is_ifd0 => self.out.date_time = self.text(e),
                0x0112 if is_ifd0 => {
                    self.out.orientation = sc.filter(|v| (1..=8).contains(v)).map(|v| v as u8)
                }
                0x9003 if job.kind == Kind::Exif && self.out.date_time_original.is_none() => {
                    self.out.date_time_original = self.text(e);
                }
                0x9004 if job.kind == Kind::Exif && self.out.date_time_digitized.is_none() => {
                    self.out.date_time_digitized = self.text(e);
                }
                0xA002 if job.kind == Kind::Exif => self.exif_dims.0 = self.dim(e, sc),
                0xA003 if job.kind == Kind::Exif => self.exif_dims.1 = self.dim(e, sc),
                0x927C if job.kind == Kind::Exif && !e.inline => {
                    if self.out.makernote.is_none() {
                        self.out.makernote = Some((e.value_or_offset, len));
                    }
                }
                0x014A if matches!(job.kind, Kind::Main | Kind::Sub) => {
                    for off in self.uints(e, 64) {
                        self.push(off, Kind::Sub, job.depth + 1, 0);
                    }
                }
                0x8769 if matches!(job.kind, Kind::Main | Kind::Sub) => {
                    if let Some(off) = self.pointer(e, sc) {
                        self.push(off, Kind::Exif, job.depth + 1, 0);
                    }
                }
                0x8825 if matches!(job.kind, Kind::Main | Kind::Sub) => {
                    if let Some(off) = self.pointer(e, sc) {
                        self.push(off, Kind::Gps, job.depth + 1, 0);
                    }
                }
                0xA005 if job.kind == Kind::Exif => {
                    if let Some(off) = self.pointer(e, sc) {
                        self.push(off, Kind::Interop, job.depth + 1, 0);
                    }
                }
                // Sony SR2Private: the four value bytes are an IFD offset
                // whether the file calls them one LONG or four BYTEs.
                0xC634 if is_ifd0 && self.is_sony() && e.inline && len == 4 => {
                    if e.value_or_offset >= 8 {
                        self.push(e.value_or_offset, Kind::Sr2, job.depth + 1, 0);
                    }
                }
                0xC640 if job.kind == Kind::Cr2Raw || job.kind == Kind::Main => {
                    let v = self.uints(e, 3);
                    if v.len() == 3 && self.out.cr2_slices.is_none() {
                        self.out.cr2_slices = Some([v[0] as u16, v[1] as u16, v[2] as u16]);
                    }
                }
                0x0002 if self.is_rw2() && is_ifd0 => {
                    self.out.rw2_sensor = Some((
                        sc.unwrap_or(0) as u32,
                        self.out.rw2_sensor.map(|s| s.1).unwrap_or(0),
                    ))
                }
                0x0003 if self.is_rw2() && is_ifd0 => {
                    self.out.rw2_sensor = Some((
                        self.out.rw2_sensor.map(|s| s.0).unwrap_or(0),
                        sc.unwrap_or(0) as u32,
                    ))
                }
                0x002D if self.is_rw2() && is_ifd0 => {
                    self.out.rw2_raw_format = sc.map(|v| v as u16)
                }
                0x0118 if self.is_rw2() && is_ifd0 => self.out.rw2_raw_offset = sc,
                0x002E | 0x0127 if self.is_rw2() && is_ifd0 && !e.inline => {
                    self.refs.jpeg(e.value_or_offset, e.tag, None);
                    self.consider_preview(e.value_or_offset, len, false);
                }
                _ => {}
            }
        }
        if job.kind == Kind::Sr2 {
            let off = ifd.find(0x7200).and_then(|e| e.scalar(endian));
            let len = ifd.find(0x7201).and_then(|e| e.scalar(endian));
            if let (Some(off), Some(len)) = (off, len) {
                if off >= 8 {
                    self.add_payload(off, len);
                    self.refs.point(self.win.start(), off);
                }
            }
        }
        self.finish_ifd(&info, &job);
        // Next IFD in the chain.
        if ifd.next != 0 && matches!(job.kind, Kind::Main | Kind::Sub) {
            if ifd.next < 8 {
                self.anomaly("next-IFD pointer points into the file header");
            } else if job.idx + 1 < MAX_CHAIN {
                self.push(ifd.next, job.kind, job.depth, job.idx + 1);
            }
        }
        match job.kind {
            Kind::Main => self.out.ifds.push(ifd),
            Kind::Sub => self.out.sub_ifds.push(ifd),
            Kind::Exif if self.out.exif_ifd.is_none() => self.out.exif_ifd = Some(ifd),
            _ => self.out.extra_ifds.push(ifd),
        }
    }

    fn is_rw2(&self) -> bool {
        self.head.magic == Magic::Rw2
    }

    fn is_sony(&self) -> bool {
        self.head
            .make
            .as_deref()
            .is_some_and(|m| crate::common::starts_with_ci(m, "SONY"))
    }

    fn dim(&self, e: &Entry, sc: Option<u64>) -> Option<u32> {
        if e.count != 1 || !matches!(e.typ, 3 | 4 | 16) {
            return None;
        }
        sc.and_then(|v| u32::try_from(v).ok())
    }

    fn pointer(&mut self, e: &Entry, sc: Option<u64>) -> Option<u64> {
        let v = sc?;
        if e.count != 1 || v < 8 {
            self.anomaly(format!("tag {:#06x} holds a bad IFD pointer", e.tag));
            return None;
        }
        Some(v)
    }

    fn push(&mut self, off: u64, kind: Kind, depth: u32, idx: usize) {
        if depth > MAX_DEPTH {
            self.note("IFD nesting limit reached");
            return;
        }
        if off < 8 {
            self.anomaly("IFD pointer points into the file header");
            return;
        }
        if self.visited.contains(&off) {
            return;
        }
        if off >= self.win.avail() {
            // Pointer to an IFD we cannot read: it is still evidence of
            // more file, unless absurd.
            self.add_value_end(off, off.saturating_add(6));
            return;
        }
        self.queue.push_back(Job {
            off,
            kind,
            depth,
            idx,
        });
    }

    /// Per-IFD post-processing once all tags have been seen.
    fn finish_ifd(&mut self, info: &IfdInfo, job: &Job) {
        // Strips and tiles. Each strip or tile of a new-style JPEG image
        // (compression 7, or 34892 lossy DNG) is a JPEG stream of its own.
        let mut payload_bytes: u64 = 0;
        let jpeg_segments = matches!(info.compression, Some(7 | 34892))
            || (info.compression == Some(6)
                && info.jpeg_off.is_none()
                && info.strip_offsets.len() == 1);
        let start = self.win.start();
        for (offs, counts, what, tag) in [
            (&info.strip_offsets, &info.strip_counts, "strip", 0x0111u16),
            (&info.tile_offsets, &info.tile_counts, "tile", 0x0144),
        ] {
            if offs.is_empty() && counts.is_empty() {
                continue;
            }
            if offs.len() != counts.len() {
                self.anomaly(format!(
                    "{} {what} offsets but {} byte counts",
                    offs.len(),
                    counts.len()
                ));
            }
            if counts.is_empty() {
                // Offsets without lengths (old Panasonic): only the start of
                // the data is known.
                if let Some(&m) = offs.iter().max() {
                    self.add_payload(m, 0);
                }
                continue;
            }
            let group = jpeg_segments.then(|| {
                self.groups += 1;
                self.groups
            });
            for (&o, &c) in offs.iter().zip(counts.iter()) {
                if o < 8 {
                    self.anomaly(format!("{what} offset {o} points into the file header"));
                    continue;
                }
                if c == 0 {
                    continue;
                }
                payload_bytes = payload_bytes.saturating_add(c);
                self.add_payload(o, c);
                self.refs.point(start, o);
                if group.is_some() {
                    self.refs.jpeg(o, tag, group);
                }
            }
        }
        // Compare an uncompressed raster against its geometry.
        if let (Some(w), Some(h)) = (info.width, info.height) {
            let comp = info.compression.unwrap_or(1);
            if comp == 1 && payload_bytes > 0 {
                let bits = info.bits.unwrap_or(8) as u64;
                let spp = info.spp.unwrap_or(1) as u64;
                let need = (w as u64 * h as u64)
                    .saturating_mul(bits)
                    .saturating_mul(spp)
                    / 8;
                if payload_bytes < need / 2 {
                    self.anomaly(format!(
                        "uncompressed image of {w}x{h} needs about {need} bytes but its strips hold {payload_bytes}"
                    ));
                }
            }
        }
        // Dimensions: the largest non-reduced image IFD wins.
        if matches!(job.kind, Kind::Main | Kind::Sub | Kind::Cr2Raw) {
            if let (Some(w), Some(h)) = (info.width, info.height) {
                let area = w as u64 * h as u64;
                let reduced = info.subfile.is_some_and(|s| s & 1 != 0) && job.kind == Kind::Main;
                if area > self.best_area && !(reduced && self.best_area > 0) {
                    self.best_area = area;
                    self.out.width = Some(w);
                    self.out.height = Some(h);
                    self.out.compression = info.compression;
                }
            }
        }
        // JPEG previews.
        if let (Some(o), Some(l)) = (info.jpeg_off, info.jpeg_len) {
            if o >= 8 && l > 0 {
                self.add_payload(o, l);
                self.refs.point(start, o);
                self.refs.jpeg(o, 0x0201, None);
                let thumb = job.kind == Kind::Main && job.idx >= 1;
                self.consider_preview(o, l, thumb);
            }
        } else if info.jpeg_off.is_some() != info.jpeg_len.is_some() {
            self.anomaly("JPEGInterchangeFormat without its length (or the reverse)");
        }
        if matches!(info.compression, Some(6 | 7))
            && info.strip_offsets.len() == 1
            && info.strip_counts.len() == 1
        {
            let (o, c) = (info.strip_offsets[0], info.strip_counts[0]);
            if o >= 8 && c > 0 {
                let thumb = job.kind == Kind::Main && job.idx >= 1;
                self.consider_preview(o, c, thumb);
            }
        }
    }

    /// Record a JPEG blob as a nested preview when its header checks out.
    pub(crate) fn consider_preview(&mut self, off: u64, len: u64, thumb_hint: bool) {
        if !self.seen_previews.insert((off, len)) {
            return;
        }
        if off >= self.win.avail() || len < 4 {
            return;
        }
        let want = len.min(16 * 1024) as usize;
        let Ok(head) = self.win.read_upto(off, want) else {
            return;
        };
        let Some(j) = jpegmini::inspect(&head) else {
            return;
        };
        if j.lossless {
            return; // sensor data in a lossless JPEG wrapper, not a preview
        }
        // Judge by the frame size; fall back to the byte size only when the
        // header did not reveal one.
        let small = if j.width > 0 && j.height > 0 {
            j.width.max(j.height) <= 320
        } else {
            len < 32 * 1024
        };
        let role = if thumb_hint || small {
            NestedRole::Thumbnail
        } else {
            NestedRole::Preview
        };
        self.refs.exempt(off, len.min(JPEG_HEADER_SPAN));
        self.out.previews.push(Preview {
            offset: off,
            len,
            role,
            width: (j.width > 0).then_some(j.width),
            height: (j.height > 0).then_some(j.height),
        });
    }

    fn finish(mut self) -> (TiffWalk, Refs) {
        // Maker-note pointers.
        if let Some((off, len)) = self.out.makernote {
            let make = self.out.make.clone().unwrap_or_default();
            let info = makernote::parse(self.win, self.head.endian, &make, off, len);
            self.out.sony_file_format = info.sony_file_format;
            for (o, l) in info.previews {
                if o < 8 || l == 0 {
                    continue;
                }
                if o < self.win.avail() {
                    // Only trust a pointer whose target really is a JPEG.
                    let ok = self
                        .win
                        .read_upto(o, 3)
                        .map(|b| b.starts_with(&[0xFF, 0xD8]))
                        .unwrap_or(false);
                    if !ok {
                        self.anomaly(format!(
                            "maker-note preview pointer {o}+{l} does not point at a JPEG; ignored"
                        ));
                        continue;
                    }
                }
                self.add_payload(o, l);
                self.refs.point(self.win.start(), o);
                self.refs.jpeg(o, 0x927C, None);
                self.consider_preview(o, l, false);
            }
        }
        // Exif pixel dimensions beat IFD geometry (the IFD with the largest
        // area is often a preview).
        if let (Some(w), Some(h)) = self.exif_dims {
            self.out.width = Some(w);
            self.out.height = Some(h);
        } else if self.out.width.is_none() {
            if let Some((w, h)) = self.out.rw2_sensor {
                if w > 0 && h > 0 {
                    self.out.width = Some(w);
                    self.out.height = Some(h);
                }
            }
        }
        if let Some(off) = self.out.rw2_raw_offset {
            self.add_payload(off, 0);
            self.refs.point(self.win.start(), off);
        }
        let mut refs = std::mem::take(&mut self.refs);
        refs.seal();
        (self.out, refs)
    }
}

/// Walk the TIFF structure of the file that starts at `start` in `src`,
/// reading no further than `start + limit`.
///
/// Returns `Ok(None)` when the bytes are not a plausible file of the
/// requested header variant: wrong magic, an IFD0 that is out of range or
/// implausible. Returns `Err` only when the medium fails on the very first
/// bytes. Damage later on is reported inside the result (`read_error`,
/// `notes`, a `max_extent` beyond `avail`), never as an error.
pub fn walk(src: &SharedSource, start: u64, limit: u64, magic: Magic) -> Result<Option<TiffWalk>> {
    let win = Win::new(src, start, limit, 4096);
    walk_win(&win, magic)
}

pub(crate) fn walk_win(win: &Win, magic: Magic) -> Result<Option<TiffWalk>> {
    match Walker::start(win, magic) {
        Ok(w) => Ok(Some(w.run())),
        Err(Reject::Io(at)) => Err(read_error(win, at)),
        Err(_) => Ok(None),
    }
}

/// Convert a walk of an Exif blob into media metadata.
pub(crate) fn exif_meta(w: &TiffWalk) -> diskrescue_core::media::MediaMeta {
    use diskrescue_core::time::Timestamp;
    let mut m = diskrescue_core::media::MediaMeta {
        width: w.width,
        height: w.height,
        make: w.make.clone(),
        model: w.model.clone(),
        software: w.software.clone(),
        orientation: w.orientation,
        ..Default::default()
    };
    m.created = [&w.date_time_original, &w.date_time_digitized, &w.date_time]
        .into_iter()
        .flatten()
        .filter_map(|s| Timestamp::from_exif_str(s))
        .find(|t| t.is_plausible());
    m
}

/// Walk an Exif/TIFF blob of `len` bytes located at `abs` in `src`, used for
/// the Exif found inside JPEG, WebP and PSD containers. Bounded to 4 MiB.
pub(crate) fn walk_blob(src: &SharedSource, abs: u64, len: u64) -> Option<TiffWalk> {
    use diskrescue_core::source::SubSource;
    if len < 16 {
        return None;
    }
    let len = len.min(4 << 20);
    let sub: SharedSource = std::sync::Arc::new(SubSource::new(src.clone(), abs, len));
    walk(&sub, 0, len, Magic::Classic).ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskrescue_core::source::MemSource;
    use std::sync::Arc;

    fn mem(b: Vec<u8>) -> SharedSource {
        Arc::new(MemSource::new(b))
    }

    fn le_entry(tag: u16, typ: u16, count: u32, val: u32) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&tag.to_le_bytes());
        v.extend_from_slice(&typ.to_le_bytes());
        v.extend_from_slice(&count.to_le_bytes());
        v.extend_from_slice(&val.to_le_bytes());
        v
    }

    /// A little-endian TIFF: header, IFD0 at 8 with a single strip at the end.
    fn simple_tiff(strip_len: u32) -> Vec<u8> {
        let mut b = b"II*\0\x08\0\0\0".to_vec();
        let entries = [
            le_entry(0x0100, 3, 1, 4),
            le_entry(0x0101, 3, 1, 4),
            le_entry(0x0102, 3, 1, 8),
            le_entry(0x0103, 3, 1, 1),
            le_entry(0x0106, 3, 1, 1),
            le_entry(0x0111, 4, 1, 8 + 2 + 12 * 8 + 4),
            le_entry(0x0115, 3, 1, 1),
            le_entry(0x0117, 4, 1, strip_len),
        ];
        b.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        for e in &entries {
            b.extend_from_slice(e);
        }
        b.extend_from_slice(&0u32.to_le_bytes());
        b.resize(b.len() + strip_len as usize, 0x42);
        b
    }

    #[test]
    fn walks_a_minimal_tiff() {
        let t = simple_tiff(16);
        let w = walk(&mem(t.clone()), 0, u64::MAX, Magic::Classic)
            .unwrap()
            .unwrap();
        assert_eq!(w.max_extent, t.len() as u64);
        assert_eq!(w.ifds.len(), 1);
        assert_eq!((w.width, w.height), (Some(4), Some(4)));
        assert_eq!(w.anomalies, 0, "{:?}", w.notes);
    }

    #[test]
    fn declared_extent_beyond_the_data_is_visible() {
        let t = simple_tiff(16);
        let cut = t[..t.len() - 5].to_vec();
        let w = walk(&mem(cut.clone()), 0, u64::MAX, Magic::Classic)
            .unwrap()
            .unwrap();
        assert_eq!(w.max_extent, t.len() as u64);
        assert_eq!(w.avail, cut.len() as u64);
    }

    #[test]
    fn wrong_magic_and_garbage_are_rejected() {
        let t = simple_tiff(16);
        assert!(walk(&mem(t.clone()), 0, u64::MAX, Magic::Orf)
            .unwrap()
            .is_none());
        assert!(walk(&mem(t.clone()), 0, u64::MAX, Magic::BigTiff)
            .unwrap()
            .is_none());
        let mut bad = t;
        bad[4] = 0xFF;
        bad[5] = 0xFF; // IFD0 far beyond the data
        assert!(walk(&mem(bad), 0, u64::MAX, Magic::Classic)
            .unwrap()
            .is_none());
    }

    #[test]
    fn ifd_cycles_terminate() {
        let mut t = simple_tiff(16);
        let next_off = 8 + 2 + 12 * 8;
        t[next_off..next_off + 4].copy_from_slice(&8u32.to_le_bytes());
        let w = walk(&mem(t), 0, u64::MAX, Magic::Classic).unwrap().unwrap();
        assert_eq!(w.ifds.len(), 1);
    }

    #[test]
    fn huge_counts_do_not_allocate_or_panic() {
        let mut t = simple_tiff(16);
        // StripOffsets count -> 0xFFFFFFFF with an out-of-line offset.
        let e = 8 + 2 + 12 * 5;
        t[e + 4..e + 8].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        t[e + 8..e + 12].copy_from_slice(&0x10u32.to_le_bytes());
        let _ = walk(&mem(t), 0, u64::MAX, Magic::Classic).unwrap();
    }

    #[test]
    fn absurd_geometry_does_not_overflow() {
        let mut t = simple_tiff(16);
        // ImageWidth, ImageLength, BitsPerSample and SamplesPerPixel become
        // LONG 0xFFFFFFFF (entries 0, 1, 2 and 6 of the IFD at offset 8).
        for idx in [0usize, 1, 2, 6] {
            let at = 8 + 2 + 12 * idx;
            t[at + 2..at + 4].copy_from_slice(&4u16.to_le_bytes());
            t[at + 8..at + 12].copy_from_slice(&u32::MAX.to_le_bytes());
        }
        let w = walk(&mem(t), 0, u64::MAX, Magic::Classic).unwrap().unwrap();
        assert!(w.anomalies >= 1, "{:?}", w.notes);
    }

    #[test]
    fn bigtiff_header_is_supported() {
        // II+\0, offset size 8, IFD0 at 16 with one entry.
        let mut b = b"II+\0\x08\0\0\0".to_vec();
        b.extend_from_slice(&16u64.to_le_bytes());
        let entries: [(u16, u16, u64, u64); 4] = [
            (0x0100, 3, 1, 2),
            (0x0101, 3, 1, 2),
            (0x0111, 16, 1, 16 + 8 + 20 * 4 + 8),
            (0x0117, 16, 1, 4),
        ];
        b.extend_from_slice(&(entries.len() as u64).to_le_bytes());
        for (tag, typ, count, val) in entries {
            b.extend_from_slice(&tag.to_le_bytes());
            b.extend_from_slice(&typ.to_le_bytes());
            b.extend_from_slice(&count.to_le_bytes());
            b.extend_from_slice(&val.to_le_bytes());
        }
        b.extend_from_slice(&0u64.to_le_bytes());
        b.extend_from_slice(&[7; 4]);
        let w = walk(&mem(b.clone()), 0, u64::MAX, Magic::BigTiff)
            .unwrap()
            .unwrap();
        assert_eq!(w.max_extent, b.len() as u64);
        assert!(walk(&mem(b), 0, u64::MAX, Magic::Classic)
            .unwrap()
            .is_none());
    }

    #[test]
    fn random_bytes_with_a_valid_header_do_not_panic() {
        let mut x = 0x2545_F491u32;
        for i in 0..3000 {
            let mut b = if i % 2 == 0 {
                b"II*\0\x08\0\0\0".to_vec()
            } else {
                b"MM\0*\0\0\0\x08".to_vec()
            };
            for _ in 0..(40 + i % 400) {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                b.push(x as u8);
            }
            for m in [Magic::Classic, Magic::Orf, Magic::Rw2, Magic::BigTiff] {
                let _ = walk(&mem(b.clone()), 0, u64::MAX, m);
            }
        }
    }
}
