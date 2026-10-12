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
//! # Limits
//!
//! Pixel data is not decoded, so a damaged strip is not noticed. The quality
//! of a measurement is the quality of the IFDs.

use diskrescue_core::format::{
    Confidence, ExtentStatus, FileFormat, Measured, Nested, Signature, Validation,
};
use diskrescue_core::media::{MediaCategory, MediaMeta};
use diskrescue_core::source::SharedSource;
use diskrescue_core::{Range, Result};

use crate::common::{starts_with_ci, validate_with, Inspected};
use crate::tiff::{exif_meta, Head, Magic, Reject, TiffWalk, Walker};
use crate::win::{read_error, Win};

const MAX_SIZE: u64 = 4 << 30;

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
    Inspected::new(m, expected)
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
    let walk = walker.run();
    let built = build(variant, &walk);
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
