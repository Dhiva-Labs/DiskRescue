//! TIFF and the TIFF-based camera RAW formats.
//!
//! All of these share one structure (see [`crate::tiff`]) and one extent
//! rule, so they share one implementation and differ only in how a file is
//! *classified*:
//!
//! | Evidence (first match wins) | Format |
//! |---|---|
//! | `CR` + major version 2 at byte 8 | CR2 |
//! | `DNGVersion` (0xC612) in IFD0 | DNG |
//! | magic `IIRO` / `IIRS` / `MMOR` | ORF |
//! | magic `IIU\0` | RW2 (RWL when the make is Leica) |
//! | Make starts with `NIKON` | NEF (NRW for Coolpix) |
//! | Make starts with `SONY` | ARW (SR2 when the maker note says so) |
//! | Make starts with `PENTAX` / `RICOH` | PEF |
//! | Make `SAMSUNG` | SRW |
//! | Make contains `SEIKO EPSON` | ERF |
//! | Make Hasselblad / Mamiya / Leaf / Phase One | 3FR / MEF / MOS / IIQ |
//! | anything else | TIFF |
//!
//! [`TiffFormat`] accepts every file that has the structure and reports the
//! classification in `meta.extra["variant"]` and the suggested extension in
//! `meta.extra["extension"]`; this is how SRW, ERF, SR2 and the Phase One /
//! Hasselblad / Mamiya / Leaf formats are served without separate format
//! objects. The dedicated formats ([`Cr2Format`], [`NefFormat`], ...) accept
//! only their own variant and carry a higher carving priority, so at the same
//! start offset the specific format wins over generic TIFF.
//!
//! # Extent
//!
//! `len` is the walk's `max_extent`. When the source ends first the result is
//! `Truncated` with `err_offset` at the end of the readable data. Panasonic
//! RW2 raw data has no stored length; see [`measure_rw2`] for the estimate.
//!
//! The extent is then checked against the bytes (see [`verify_extent`]):
//! JPEG streams the IFDs point at must start with `FF D8`, and no other file
//! may start on the sector grid past the last verified structure; either
//! failure ends the file there (`Truncated`, confidence lowered by 30).
//!
//! An Exif block measured on its own (IFD0 with Exif/GPS pointers but no
//! strips, tiles, JPEG stream or SubIFDs) is not a picture: confidence
//! `LOW` and the note "EXIF block without image data".
//!
//! # Limits
//!
//! Pixel data is not decoded, so a damaged strip is not noticed. The quality
//! of a measurement is the quality of the IFDs.

use std::collections::HashMap;

use diskrescue_core::format::{
    Confidence, ExtentStatus, FileFormat, Measured, Nested, Signature, Validation,
};
use diskrescue_core::media::{MediaCategory, MediaMeta};
use diskrescue_core::source::SharedSource;
use diskrescue_core::{Range, Result};

use crate::common::{cut_at, foreign_note, starts_with_ci, validate_with, Inspected};
use crate::foreign::{self, Sig};
use crate::tiff::{exif_meta, Head, Magic, Refs, Reject, TiffWalk, Walker};
use crate::win::{read_error, Win};

const MAX_SIZE: u64 = 4 << 30;
/// Most `FF D8` checks of JPEG offsets per file (spread over all of them).
const MAX_JPEG_CHECKS: usize = 1024;
/// Note on an Exif block measured on its own.
pub(crate) const EXIF_BLOCK_NOTE: &str = "EXIF block without image data";

/// The concrete kind of a TIFF-structured file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Variant {
    Tiff,
    Cr2,
    Nef,
    Arw,
    Dng,
    Pef,
    Orf,
    Rw2,
    Srw,
    Erf,
    Hasselblad,
    Mamiya,
    Leaf,
    PhaseOne,
}

impl Variant {
    fn id(self) -> &'static str {
        match self {
            Variant::Tiff => "tiff",
            Variant::Cr2 => "cr2",
            Variant::Nef => "nef",
            Variant::Arw => "arw",
            Variant::Dng => "dng",
            Variant::Pef => "pef",
            Variant::Orf => "orf",
            Variant::Rw2 => "rw2",
            Variant::Srw => "srw",
            Variant::Erf => "erf",
            Variant::Hasselblad => "3fr",
            Variant::Mamiya => "mef",
            Variant::Leaf => "mos",
            Variant::PhaseOne => "iiq",
        }
    }
}

/// Classify from IFD0 and the header (see the module table).
pub(crate) fn classify(h: &Head) -> Variant {
    if matches!(h.cr2, Some((2, _, _))) {
        return Variant::Cr2;
    }
    if h.dng_version.is_some() {
        return Variant::Dng;
    }
    match h.magic {
        Magic::Orf => return Variant::Orf,
        Magic::Rw2 => return Variant::Rw2,
        Magic::BigTiff => return Variant::Tiff,
        Magic::Classic => {}
    }
    let make = h.make.as_deref().unwrap_or("");
    if starts_with_ci(make, "NIKON") {
        Variant::Nef
    } else if starts_with_ci(make, "SONY") {
        Variant::Arw
    } else if starts_with_ci(make, "PENTAX") || starts_with_ci(make, "RICOH") {
        Variant::Pef
    } else if starts_with_ci(make, "SAMSUNG") {
        Variant::Srw
    } else if make.to_ascii_uppercase().contains("SEIKO EPSON") {
        Variant::Erf
    } else if starts_with_ci(make, "HASSELBLAD") {
        Variant::Hasselblad
    } else if starts_with_ci(make, "MAMIYA") {
        Variant::Mamiya
    } else if starts_with_ci(make, "LEAF") {
        Variant::Leaf
    } else if starts_with_ci(make, "PHASE ONE") {
        Variant::PhaseOne
    } else {
        Variant::Tiff
    }
}

/// Suggested file extension for a measured file.
fn extension(variant: Variant, w: &TiffWalk) -> &'static str {
    match variant {
        Variant::Nef
            if w.model
                .as_deref()
                .is_some_and(|m| starts_with_ci(m, "COOLPIX")) =>
        {
            "nrw"
        }
        Variant::Arw if w.sony_file_format.is_some_and(|f| f[0] == 1) => "sr2",
        Variant::Rw2
            if w.make
                .as_deref()
                .is_some_and(|m| starts_with_ci(m, "LEICA")) =>
        {
            "rwl"
        }
        Variant::Tiff => "tif",
        v => v.id(),
    }
}

/// Which files a format object accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Accept {
    /// Generic TIFF: any classic or BigTIFF structure, whatever the variant.
    AnyTiff,
    /// One specific variant.
    Only(Variant),
}

impl Accept {
    fn allows_magic(self, m: Magic) -> bool {
        match self {
            Accept::AnyTiff => matches!(m, Magic::Classic | Magic::BigTiff),
            Accept::Only(Variant::Orf) => m == Magic::Orf,
            Accept::Only(Variant::Rw2) => m == Magic::Rw2,
            Accept::Only(_) => m == Magic::Classic,
        }
    }

    fn allows(self, v: Variant) -> bool {
        match self {
            Accept::AnyTiff => true,
            Accept::Only(want) => want == v,
        }
    }
}

fn probe_family(head: &[u8], accept: Accept) -> bool {
    let Some((magic, _)) = Magic::detect(head) else {
        return false;
    };
    if !accept.allows_magic(magic) {
        return false;
    }
    let win = Win::from_bytes(head);
    match Walker::start(&win, magic) {
        Ok(w) => accept.allows(classify(&w.head)),
        // IFD0 lies outside the probe window (libtiff writes it last): only
        // the generic format can say yes without seeing it.
        Err(Reject::Beyond) => accept == Accept::AnyTiff,
        Err(_) => false,
    }
}

/// Panasonic RW2 raw-data size estimate (dcraw `panasonic_load_raw`): 16
/// bytes per 14 pixels, read in 0x4000-byte blocks. Valid for RawFormat 4.
fn rw2_estimate(w: &TiffWalk) -> Option<(u64, u64)> {
    let raw_off = w.rw2_raw_offset?;
    if w.rw2_raw_format != Some(4) {
        return None;
    }
    let (sw, sh) = w.rw2_sensor?;
    if sw == 0 || sh == 0 || sw > 20_000 || sh > 20_000 {
        return None;
    }
    let exact = (sw as u64 * sh as u64 * 8).div_ceil(7);
    let padded = exact.div_ceil(0x4000) * 0x4000;
    Some((raw_off + exact, raw_off + padded))
}

fn build_meta(variant: Variant, w: &TiffWalk) -> MediaMeta {
    let mut meta = exif_meta(w);
    meta.extra.insert("variant".into(), variant.id().into());
    meta.extra
        .insert("extension".into(), extension(variant, w).into());
    meta.extra.insert("ifds".into(), w.ifd_count().to_string());
    if let Some(v) = w.dng_version {
        meta.extra.insert(
            "dng_version".into(),
            format!("{}.{}.{}.{}", v[0], v[1], v[2], v[3]),
        );
    }
    if let Some((maj, min, _)) = w.cr2 {
        meta.extra
            .insert("cr2_version".into(), format!("{maj}.{min}"));
    }
    if let Some(s) = w.cr2_slices {
        meta.extra
            .insert("cr2_slices".into(), format!("{},{},{}", s[0], s[1], s[2]));
    }
    if let Some(c) = w.compression {
        meta.extra.insert("compression".into(), c.to_string());
    }
    meta
}

fn nested_of(w: &TiffWalk, len: u64) -> Vec<Nested> {
    let mut v: Vec<Nested> = Vec::new();
    for p in &w.previews {
        if p.offset >= len {
            continue;
        }
        let end = p.offset.saturating_add(p.len).min(len);
        v.push(Nested {
            range: Range::new(p.offset, end),
            format: Some("jpeg".into()),
            role: p.role,
        });
    }
    v
}

/// True when the file is an Exif block rather than a picture: a classic
/// TIFF whose IFD0 points at Exif or GPS data but holds no image data of its
/// own (no strips, tiles, JPEG stream, SubIFDs or RW2 raw data). That is what
/// sits inside a JPEG APP1, a WebP `EXIF`, a PNG `eXIf` or a HEIF Exif item;
/// an IFD1 thumbnail does not make it a photo.
fn is_exif_block(w: &TiffWalk) -> bool {
    if w.magic != Magic::Classic || w.cr2.is_some() {
        return false;
    }
    let Some(ifd0) = w.ifds.iter().find(|i| i.offset == w.ifd0_offset) else {
        return false;
    };
    let has = |tag: u16| ifd0.find(tag).is_some();
    let image = [0x0111, 0x0144, 0x0201, 0x014A, 0x0118, 0x002E, 0x0127]
        .into_iter()
        .any(has);
    !image && (has(0x8769) || has(0x8825))
}

/// Turn a finished walk into a measurement.
fn build(variant: Variant, w: &TiffWalk) -> Inspected {
    let declared = w.max_extent;
    let avail = w.avail;
    let mut conf = if w.payload_count == 0 {
        Confidence::MEDIUM
    } else if variant == Variant::Tiff {
        Confidence::new(80)
    } else {
        Confidence::HIGH
    };
    conf = conf.lowered((w.anomalies.min(5) * 5) as u8);
    let mut expected = Some(declared);

    let rw2_unknown = variant == Variant::Rw2 && w.rw2_raw_offset.is_some_and(|o| declared <= o);
    let mut m = if rw2_unknown {
        measure_rw2(w, conf)
    } else if let Some(e) = w.read_error {
        Measured::truncated(
            declared.min(avail),
            conf.lowered(30),
            e,
            format!("unreadable bytes at {e} while walking the file structure"),
        )
    } else if declared <= avail {
        Measured::complete(declared, conf)
    } else if w.capped {
        Measured::capped(avail, conf.lowered(40)).with_note(format!(
            "file structure declares {declared} bytes, beyond the measurement limit"
        ))
    } else {
        Measured::truncated(
            avail,
            conf.lowered(30),
            avail,
            format!("file ends at {avail} but its structure declares {declared} bytes"),
        )
    };
    if rw2_unknown {
        expected = None;
    }
    m.meta = build_meta(variant, w);
    m.nested = nested_of(w, m.len);
    m.notes.extend(w.notes.iter().cloned());
    if is_exif_block(w) {
        // Not a picture: never carved as a photo on its own strength.
        m.confidence = m.confidence.min(Confidence::LOW);
        m.notes.push(EXIF_BLOCK_NOTE.into());
    }
    Inspected::new(m, expected)
}

/// Check the measured extent against the bytes it covers.
///
/// Strips, tiles and raw data are opaque, but two things are cheap to check:
///
/// * every offset where the structure says a JPEG stream starts
///   (`JPEGInterchangeFormat`, JPEG-compressed strips and tiles, `JpgFromRaw`,
///   maker-note previews) must hold `FF D8`;
/// * no other file may start on the sector grid inside the extent (see
///   [`crate::foreign`]). A JPEG start is the file's own when an IFD entry
///   points at it or it lies in a blob or JPEG header that may contain one;
///   a TIFF header likewise only inside such a blob or JPEG header.
///
/// A failure past the last plausible IFD and verified JPEG start ends the
/// file there (`Truncated`): the size fields describe bytes that are no
/// longer this file's. A failure before that point, where the file's own
/// structure is still intact further on, is only noted (an overwritten
/// middle; the carver handles the embedded file).
fn verify_extent(win: &Win, refs: &Refs, ins: &mut Inspected) {
    let declared = ins.expected.unwrap_or(ins.measured.len);
    let m = &mut ins.measured;
    if m.status == ExtentStatus::UnknownLength || m.len == 0 {
        return;
    }
    // JPEG starts, spread evenly when there are very many (tiled DNGs). A
    // group of strips or tiles only counts once its first one is a JPEG.
    let check = |off: u64| -> Option<bool> {
        let mut b = [0u8; 2];
        match win.peek(off, &mut b) {
            Some(2) => Some(b == [0xFF, 0xD8]),
            _ => None, // beyond the readable data or unreadable: no evidence
        }
    };
    let js = &refs.jpeg_starts;
    let mut group_ok: HashMap<u32, bool> = HashMap::new();
    for j in js {
        if let Some(g) = j.group {
            group_ok
                .entry(g)
                .or_insert_with(|| j.off < m.len && check(j.off) == Some(true));
        }
    }
    let step = js.len().div_ceil(MAX_JPEG_CHECKS).max(1);
    let mut verified_end = refs.structure_end;
    let mut missing: Vec<(u64, u16)> = Vec::new();
    for j in js.iter().step_by(step).chain(js.last()) {
        if j.off >= m.len || j.group.is_some_and(|g| !group_ok[&g]) {
            continue;
        }
        match check(j.off) {
            Some(true) => verified_end = verified_end.max(j.off + 2),
            Some(false) => missing.push((j.off, j.tag)),
            None => {}
        }
    }
    // `missing` is in offset order: the first one past the verified
    // structure ends the file; the ones before it cost a little confidence,
    // once.
    let split = missing.partition_point(|j| j.0 < verified_end);
    let (inside, past) = missing.split_at(split);
    if let Some(&(off, tag)) = inside.first() {
        m.confidence = m.confidence.lowered(10);
        m.notes.push(format!(
            "no JPEG data (FF D8) at {} offset(s) the IFDs point at, the first at {off} (tag {tag:#06x})",
            inside.len()
        ));
    }
    let mut cut: Option<(u64, String)> = past.first().map(|&(off, tag)| {
        (
            off,
            format!(
                "no JPEG data (FF D8) at offset {off}, where tag {tag:#06x} points: the file is cut short or overwritten from there on"
            ),
        )
    });
    let allowed = |off: u64, sig: Sig| match sig {
        Sig::Jpeg => refs.is_pointed(off) || refs.is_exempt(off),
        Sig::Tiff => refs.is_exempt(off),
        _ => false,
    };
    let scan_to = cut.as_ref().map_or(m.len, |c| c.0);
    if let Some(hit) = foreign::scan(win, verified_end, scan_to, &allowed) {
        cut = Some((hit.offset, foreign_note(hit, declared, "its structure")));
    }
    if let Some((at, note)) = cut {
        cut_at(m, at, note);
    }
}

/// Measurement of a Panasonic RW2 whose raw data has no stored length.
///
/// For RawFormat 4 the size follows from the sensor dimensions
/// (`w * h * 8 / 7` bytes, in 0x4000-byte blocks); the result is
/// `UnknownLength` with low confidence and a length inside
/// `[estimate, estimate + 0x4000)`, clipped to the readable data. For other
/// formats only the known structure is measured and the carver is expected to
/// extend the file to the next header.
fn measure_rw2(w: &TiffWalk, conf: Confidence) -> Measured {
    let known = w.max_extent;
    let avail = w.avail;
    let low = Confidence::new(conf.value().min(35));
    let estimate = rw2_estimate(w);
    let (mut len, mut status, mut err, note) = match estimate {
        Some((lo, hi)) if avail >= lo => (
            hi.min(avail).max(known.min(avail)),
            ExtentStatus::UnknownLength,
            None,
            format!(
                "RW2 raw data has no stored length; estimated from the {}x{} sensor",
                w.rw2_sensor.map(|s| s.0).unwrap_or(0),
                w.rw2_sensor.map(|s| s.1).unwrap_or(0)
            ),
        ),
        Some((lo, _)) => (
            avail,
            ExtentStatus::Truncated,
            Some(avail),
            format!("file ends at {avail}, before the estimated raw data end {lo}"),
        ),
        None if known <= avail => (
            known,
            ExtentStatus::UnknownLength,
            None,
            "RW2 raw data has no stored length; only the header, previews and the raw start are measured"
                .to_string(),
        ),
        None => (
            avail,
            ExtentStatus::Truncated,
            Some(avail),
            format!("file ends at {avail} but its structure declares {known} bytes"),
        ),
    };
    // A limit that cuts the file short is a cap, not a truncation.
    let cut_by_limit = w.capped
        && (status == ExtentStatus::Truncated
            || estimate.is_some_and(|(_, hi)| hi > avail)
            || known > avail);
    if cut_by_limit {
        len = avail;
        status = ExtentStatus::Capped;
        err = None;
    }
    Measured {
        len,
        status,
        confidence: low,
        err_offset: err,
        meta: MediaMeta::default(),
        nested: Vec::new(),
        notes: vec![note],
    }
}

fn inspect_family(
    src: &SharedSource,
    start: u64,
    limit: u64,
    accept: Accept,
) -> Result<Option<Inspected>> {
    let limit = limit.min(MAX_SIZE);
    let win = Win::new(src, start, limit, 4096);
    let head = match win.read_upto(0, 16) {
        Ok(h) => h,
        Err(f) => return Err(read_error(&win, f.offset())),
    };
    let Some((magic, _)) = Magic::detect(&head) else {
        return Ok(None);
    };
    if !accept.allows_magic(magic) {
        return Ok(None);
    }
    let walker = match Walker::start(&win, magic) {
        Ok(w) => w,
        Err(Reject::Io(at)) => return Err(read_error(&win, at)),
        Err(_) => return Ok(None),
    };
    let variant = classify(&walker.head);
    if !accept.allows(variant) {
        return Ok(None);
    }
    let (walk, refs) = walker.run_with_refs();
    let mut built = build(variant, &walk);
    verify_extent(&win, &refs, &mut built);
    crate::common::log_measured(
        &win,
        variant.id(),
        &built.measured,
        walk.read_error.is_some(),
    );
    Ok(Some(built))
}

static TIFF_SIGNATURES: [Signature; 4] = [
    Signature::at_start(b"II*\0"),
    Signature::at_start(b"MM\0*"),
    Signature::at_start(b"II+\0\x08\0\0\0"),
    Signature::at_start(b"MM\0+\0\x08\0\0"),
];
static CLASSIC_SIGNATURES: [Signature; 2] =
    [Signature::at_start(b"II*\0"), Signature::at_start(b"MM\0*")];
static CR2_SIGNATURES: [Signature; 1] = [Signature::at_start(b"II*\0").with_also(b"CR\x02", 8)];
static ORF_SIGNATURES: [Signature; 3] = [
    Signature::at_start(b"IIRO"),
    Signature::at_start(b"IIRS"),
    Signature::at_start(b"MMOR"),
];
static RW2_SIGNATURES: [Signature; 1] = [Signature::at_start(b"IIU\0")];

macro_rules! tiff_family_format {
    (
        $(#[$doc:meta])*
        $ty:ident, id: $id:literal, name: $name:literal, exts: $exts:expr,
        category: $cat:expr, sigs: $sigs:expr, min: $min:expr, priority: $prio:expr,
        accept: $accept:expr
    ) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, Default)]
        pub struct $ty;

        impl FileFormat for $ty {
            fn id(&self) -> &'static str {
                $id
            }

            fn name(&self) -> &'static str {
                $name
            }

            fn extensions(&self) -> &'static [&'static str] {
                $exts
            }

            fn category(&self) -> MediaCategory {
                $cat
            }

            fn signatures(&self) -> &'static [Signature] {
                $sigs
            }

            fn min_size(&self) -> u64 {
                $min
            }

            fn max_size(&self) -> u64 {
                MAX_SIZE
            }

            fn priority(&self) -> u8 {
                $prio
            }

            fn probe(&self, head: &[u8]) -> bool {
                probe_family(head, $accept)
            }

            fn may_contain(&self, inner: &str) -> bool {
                matches!(inner, "jpeg" | "tiff")
            }

            fn measure(
                &self,
                src: &SharedSource,
                start: u64,
                limit: u64,
            ) -> Result<Option<Measured>> {
                Ok(inspect_family(src, start, limit, $accept)?.map(|i| i.measured))
            }

            fn validate(&self, src: &SharedSource) -> Result<Validation> {
                validate_with($name, src, |s, st, l| inspect_family(s, st, l, $accept))
            }
        }
    };
}

tiff_family_format!(
    /// Generic TIFF (classic and BigTIFF) and, through the `variant` /
    /// `extension` metadata, the RAW formats without a dedicated object
    /// (SRW, ERF, SR2, 3FR, MEF, MOS, IIQ).
    TiffFormat, id: "tiff", name: "TIFF image", exts: &["tif", "tiff"],
    category: MediaCategory::Photo, sigs: &TIFF_SIGNATURES, min: 512, priority: 50,
    accept: Accept::AnyTiff
);
tiff_family_format!(
    /// Canon CR2.
    Cr2Format, id: "cr2", name: "Canon CR2 raw", exts: &["cr2"],
    category: MediaCategory::RawPhoto, sigs: &CR2_SIGNATURES, min: 64 * 1024, priority: 60,
    accept: Accept::Only(Variant::Cr2)
);
tiff_family_format!(
    /// Nikon NEF / NRW.
    NefFormat, id: "nef", name: "Nikon NEF raw", exts: &["nef", "nrw"],
    category: MediaCategory::RawPhoto, sigs: &CLASSIC_SIGNATURES, min: 64 * 1024, priority: 60,
    accept: Accept::Only(Variant::Nef)
);
tiff_family_format!(
    /// Sony ARW / SR2.
    ArwFormat, id: "arw", name: "Sony ARW raw", exts: &["arw", "sr2", "srf"],
    category: MediaCategory::RawPhoto, sigs: &CLASSIC_SIGNATURES, min: 64 * 1024, priority: 60,
    accept: Accept::Only(Variant::Arw)
);
tiff_family_format!(
    /// Adobe DNG (any TIFF whose IFD0 carries `DNGVersion`).
    DngFormat, id: "dng", name: "Adobe DNG raw", exts: &["dng"],
    category: MediaCategory::RawPhoto, sigs: &CLASSIC_SIGNATURES, min: 8 * 1024, priority: 62,
    accept: Accept::Only(Variant::Dng)
);
tiff_family_format!(
    /// Pentax / Ricoh PEF.
    PefFormat, id: "pef", name: "Pentax PEF raw", exts: &["pef"],
    category: MediaCategory::RawPhoto, sigs: &CLASSIC_SIGNATURES, min: 64 * 1024, priority: 60,
    accept: Accept::Only(Variant::Pef)
);
tiff_family_format!(
    /// Olympus ORF.
    OrfFormat, id: "orf", name: "Olympus ORF raw", exts: &["orf"],
    category: MediaCategory::RawPhoto, sigs: &ORF_SIGNATURES, min: 64 * 1024, priority: 60,
    accept: Accept::Only(Variant::Orf)
);
tiff_family_format!(
    /// Panasonic / Leica RW2 / RWL.
    Rw2Format, id: "rw2", name: "Panasonic RW2 raw", exts: &["rw2", "rwl", "raw"],
    category: MediaCategory::RawPhoto, sigs: &RW2_SIGNATURES, min: 64 * 1024, priority: 60,
    accept: Accept::Only(Variant::Rw2)
);
