//! Reading the results of a recovery: the report, one item per file, and
//! thumbnails.

use std::io::Cursor;
use std::panic::AssertUnwindSafe;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use diskrescue_core::format::{FileFormat, NestedRole, ValidationStatus};
use diskrescue_core::media::{MediaCategory, MediaMeta};
use diskrescue_core::source::FileSource;
use diskrescue_core::time::Timestamp;
use diskrescue_core::{SharedSource, Source, SourceExt};
use diskrescue_recover::pipeline::VerifyResult;
use diskrescue_recover::report::{Origin, Outcome, Report};
use image::{ImageFormat, ImageReader, RgbaImage};

use crate::error::{explain_io, AppError, Result};

/// Images with more pixels than this are never decoded.
pub const MAX_PIXELS: u64 = 50_000_000;
/// Largest allocation a thumbnail decode may make, and the largest file or
/// embedded preview that is read into memory to decode it.
pub const MAX_BYTES: u64 = 64 << 20;

/// Load `output_dir/report.json`.
pub fn load(output_dir: &Path) -> Result<Report> {
    let path = output_dir.join("report.json");
    Report::read_json(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            AppError::failed(format!(
                "{} holds no report.json: it is not the output folder of a recovery, or the run did not \
                 get far enough to write one.",
                output_dir.display()
            ))
        } else if e.kind() == std::io::ErrorKind::Other {
            AppError::failed(format!("{} is not a readable report: {e}", path.display()))
        } else {
            AppError::failed(explain_io(&e, Some(&path)))
        }
    })
}

/// One file of a report, ready to show.
#[derive(Debug, Clone, PartialEq)]
pub struct ResultItem {
    /// Report id.
    pub id: u64,
    /// The recovered file (absolute), when one was written.
    pub path: Option<PathBuf>,
    /// The repaired copy (absolute), when a repair produced one.
    pub repaired: Option<PathBuf>,
    /// Photo, RAW, video, ...
    pub category: MediaCategory,
    /// What happened to it.
    pub outcome: Outcome,
    /// Size in bytes.
    pub size: u64,
    /// Format id (`jpeg`, `mp4`).
    pub format: Option<String>,
    /// Capture date, when known.
    pub captured: Option<Timestamp>,
    /// Problems the validation found.
    pub problems: Vec<String>,
    /// Other notes (repair steps, unreadable areas, ...).
    pub notes: Vec<String>,
    /// Where it came from (`vol1:/DCIM/IMG_0001.JPG (deleted)`,
    /// `carved at offset 0x2c41000`).
    pub origin: String,
    /// Found as a deleted file through the filesystem.
    pub deleted: bool,
    /// Bytes that could not be read and are zero-filled.
    pub unreadable_bytes: u64,
    /// Metadata (dimensions, duration, camera, ...).
    pub meta: MediaMeta,
}

/// `root/rel` when `rel` is a plain relative path (no `..`, not absolute).
fn inside(root: &Path, rel: &Path) -> Option<PathBuf> {
    let plain = rel.components().all(|c| matches!(c, Component::Normal(_)));
    // Joined name by name, so a report written with `/` reads back with the
    // platform's separator.
    (plain && rel.components().next().is_some())
        .then(|| rel.components().fold(root.to_path_buf(), |p, c| p.join(c)))
}

/// One [`ResultItem`] per file of `report`; `root` is the output folder
/// the report's paths are relative to (made absolute). Paths that would
/// lead outside it are dropped.
pub fn items(report: &Report, root: &Path) -> Vec<ResultItem> {
    let root = std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf());
    let root = root.as_path();
    report
        .files
        .iter()
        .map(|f| {
            let repaired = f
                .repair
                .as_ref()
                .and_then(|r| r.output.as_deref())
                .and_then(|p| inside(root, p));
            let path = f.output.as_deref().and_then(|p| inside(root, p));
            let mut notes: Vec<String> = Vec::new();
            if let Some(r) = &f.repair {
                if !r.applied.is_empty() {
                    notes.push(format!("repair: {}", r.applied.join(", ")));
                }
                if let Some(d) = &r.donor {
                    notes.push(format!("used {} as the reference", d.display()));
                }
                notes.extend(r.notes.iter().cloned());
            }
            notes.extend(f.notes.iter().cloned());
            ResultItem {
                id: f.id,
                path,
                repaired,
                category: f.category,
                outcome: f.outcome,
                size: f.size,
                format: f.format.clone(),
                captured: f.meta.created,
                problems: f
                    .validation
                    .as_ref()
                    .map(|v| v.problems.clone())
                    .unwrap_or_default(),
                notes,
                origin: f.origin_text(),
                deleted: matches!(f.origin, Origin::Filesystem { deleted: true, .. }),
                unreadable_bytes: f.unreadable_bytes,
                meta: f.meta.clone(),
            }
        })
        .collect()
}

/// True when the file ended up usable (intact or repaired at least partly).
pub fn is_usable(o: Outcome) -> bool {
    matches!(
        o,
        Outcome::Recovered
            | Outcome::RecoveredUnverified
            | Outcome::Repaired
            | Outcome::PartiallyRepaired
    )
}

/// The verdict for one verified file: (label, needs attention).
pub fn verify_verdict(r: &VerifyResult) -> (&'static str, bool) {
    match (&r.validation, &r.error) {
        (Some(v), _) => match v.status {
            ValidationStatus::Valid => ("ok", false),
            ValidationStatus::Damaged => ("DAMAGED", true),
            ValidationStatus::Invalid => ("INVALID", true),
        },
        (None, Some(_)) => ("UNRECOGNISED", true),
        (None, None) => ("UNKNOWN", true),
    }
}

/// True when `dir` holds at least one file (looking a few levels down).
pub fn has_files(dir: &Path) -> bool {
    fn walk(dir: &Path, depth: u32) -> bool {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return false;
        };
        for e in rd.flatten().take(10_000) {
            match e.file_type() {
                Ok(t) if t.is_file() => return true,
                Ok(t) if t.is_dir() && depth < 8 && walk(&e.path(), depth + 1) => return true,
                _ => {}
            }
        }
        false
    }
    walk(dir, 0)
}

// ---------------------------------------------------------------- thumbnails

fn limits() -> image::Limits {
    let mut l = image::Limits::default();
    l.max_alloc = Some(MAX_BYTES);
    l
}

fn fits(w: u32, h: u32) -> bool {
    w > 0 && h > 0 && u64::from(w) * u64::from(h) <= MAX_PIXELS
}

fn shrink(img: image::DynamicImage, max_px: u32) -> RgbaImage {
    if img.width() > max_px || img.height() > max_px {
        img.thumbnail(max_px, max_px).to_rgba8()
    } else {
        img.to_rgba8()
    }
}

/// Decode an in-memory image after checking its dimensions.
fn decode_bytes(bytes: &[u8], format: Option<ImageFormat>, max_px: u32) -> Option<RgbaImage> {
    let reader = |bytes: &[u8]| -> Option<ImageReader<Cursor<Vec<u8>>>> {
        let mut r = ImageReader::new(Cursor::new(bytes.to_vec()));
        match format {
            Some(f) => r.set_format(f),
            None => r = r.with_guessed_format().ok()?,
        }
        r.limits(limits());
        Some(r)
    };
    std::panic::catch_unwind(AssertUnwindSafe(|| {
        let (w, h) = reader(bytes)?.into_dimensions().ok()?;
        if !fits(w, h) {
            return None;
        }
        let img = reader(bytes)?.decode().ok()?;
        Some(shrink(img, max_px))
    }))
    .ok()
    .flatten()
}

/// Decode an image file after checking its size and dimensions.
fn decode_file(path: &Path, format: ImageFormat, max_px: u32) -> Option<RgbaImage> {
    let len = std::fs::metadata(path).ok()?.len();
    if len == 0 || len > MAX_BYTES {
        return None;
    }
    let reader = || -> Option<ImageReader<std::io::BufReader<std::fs::File>>> {
        let mut r = ImageReader::open(path).ok()?;
        r.set_format(format);
        r.limits(limits());
        Some(r)
    };
    std::panic::catch_unwind(AssertUnwindSafe(|| {
        let (w, h) = reader()?.into_dimensions().ok()?;
        if !fits(w, h) {
            return None;
        }
        let img = reader()?.decode().ok()?;
        Some(shrink(img, max_px))
    }))
    .ok()
    .flatten()
}

/// The JPEG previews and thumbnails a format's `measure` finds inside the
/// file, decoded smallest-sufficient first.
fn embedded_preview(path: &Path, fmt: &dyn FileFormat, max_px: u32) -> Option<RgbaImage> {
    let src: SharedSource = Arc::new(FileSource::open(path).ok()?);
    let len = src.size();
    let measured = std::panic::catch_unwind(AssertUnwindSafe(|| fmt.measure(&src, 0, len)))
        .ok()?
        .ok()??;
    let mut cands: Vec<(Vec<u8>, u64)> = Vec::new();
    for n in &measured.nested {
        let wanted_role = matches!(n.role, NestedRole::Preview | NestedRole::Thumbnail);
        let jpeg = n.format.as_deref().is_none_or(|f| f == "jpeg");
        if !wanted_role || !jpeg || n.range.len() < 4 || n.range.len() > MAX_BYTES {
            continue;
        }
        if n.range.end > len {
            continue;
        }
        let Ok(bytes) = src.read_vec(n.range.start, n.range.len() as usize) else {
            continue;
        };
        if !bytes.starts_with(&[0xFF, 0xD8]) {
            continue;
        }
        let dims = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let mut r = ImageReader::new(Cursor::new(&bytes[..]));
            r.set_format(ImageFormat::Jpeg);
            r.into_dimensions().ok()
        }))
        .ok()
        .flatten();
        let Some((w, h)) = dims.filter(|(w, h)| fits(*w, *h)) else {
            continue;
        };
        cands.push((bytes, u64::from(w.max(h))));
    }
    // The smallest one at least `max_px` wide first; then the larger ones
    // that are too small, biggest first.
    cands.sort_by_key(|(_, side)| {
        if *side >= u64::from(max_px) {
            (0, *side)
        } else {
            (1, u64::MAX - *side)
        }
    });
    cands
        .iter()
        .find_map(|(bytes, _)| decode_bytes(bytes, Some(ImageFormat::Jpeg), max_px))
}

/// A thumbnail of a photo, at most `max_px` on its longer side.
///
/// JPEG, PNG, GIF, BMP, WebP and TIFF files are decoded with the `image`
/// crate, but never when the file is larger than [`MAX_BYTES`], the picture
/// has more than [`MAX_PIXELS`] pixels, or decoding would allocate more than
/// [`MAX_BYTES`]; such files (and files that fail to decode) fall back to
/// the JPEG thumbnail embedded in them, if any. Camera RAW files, CR3 and
/// HEIC use the embedded JPEG preview or thumbnail that the format's
/// measurement finds. Videos and anything unrecognised give `None`.
/// Decoding runs under `catch_unwind`: a decoder bug on damaged data gives
/// `None`, never a crash.
pub fn thumbnail(path: &Path, max_px: u32) -> Option<RgbaImage> {
    let max_px = max_px.max(1);
    let formats = diskrescue_recover::registry::all_formats();
    let fmt = diskrescue_recover::pipeline::identify_file(&formats, path)?;
    let direct = match fmt.id() {
        "jpeg" => Some(ImageFormat::Jpeg),
        "png" => Some(ImageFormat::Png),
        "gif" => Some(ImageFormat::Gif),
        "bmp" => Some(ImageFormat::Bmp),
        "webp" => Some(ImageFormat::WebP),
        "tiff" => Some(ImageFormat::Tiff),
        _ => None,
    };
    if let Some(f) = direct {
        if let Some(img) = decode_file(path, f, max_px) {
            return Some(img);
        }
        return embedded_preview(path, fmt.as_ref(), max_px);
    }
    match fmt.category() {
        MediaCategory::Photo | MediaCategory::RawPhoto => {
            embedded_preview(path, fmt.as_ref(), max_px)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskrescue_recover::report::{FileRecord, RepairRecord};

    #[test]
    fn items_resolve_paths_inside_the_output_only() {
        let mut r = Report::new(None);
        let mut f = FileRecord::new(
            Origin::Input {
                path: PathBuf::from("a.jpg"),
            },
            MediaCategory::Photo,
            10,
        );
        f.output = Some(PathBuf::from("files/a.jpg"));
        f.repair = Some(RepairRecord {
            result: diskrescue_core::format::RepairResult::Repaired,
            applied: vec!["appended EOI".into()],
            notes: vec![],
            donor: None,
            output: Some(PathBuf::from("../../etc/passwd")),
        });
        r.add_file(f);
        let it = items(&r, Path::new("/out"));
        let root = std::path::absolute("/out").unwrap();
        assert_eq!(it[0].path, Some(root.join("files").join("a.jpg")));
        assert_eq!(
            it[0].path.as_ref().map(|p| p.display().to_string()),
            Some(root.join("files").join("a.jpg").display().to_string())
        );
        assert!(it[0].repaired.is_none());
        assert_eq!(it[0].notes, vec!["repair: appended EOI".to_string()]);
        assert!(is_usable(Outcome::PartiallyRepaired) && !is_usable(Outcome::Failed));
    }

    #[test]
    fn missing_report_is_explained() {
        let dir = tempfile::tempdir().unwrap();
        let e = load(dir.path()).unwrap_err();
        assert!(e.message().contains("no report.json"), "{e}");
        std::fs::write(dir.path().join("report.json"), "{").unwrap();
        assert!(load(dir.path())
            .unwrap_err()
            .message()
            .contains("not a readable report"));
    }

    #[test]
    fn huge_dimensions_are_never_decoded() {
        // A PNG header claiming 100000 x 100000 pixels.
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&100_000u32.to_be_bytes());
        png.extend_from_slice(&100_000u32.to_be_bytes());
        png.extend_from_slice(&[8, 6, 0, 0, 0]);
        let crc = crc32(&png[12..29]);
        png.extend_from_slice(&crc.to_be_bytes());
        assert!(decode_bytes(&png, Some(ImageFormat::Png), 64).is_none());
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("huge.png");
        std::fs::write(&p, &png).unwrap();
        assert!(thumbnail(&p, 64).is_none());
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for b in data {
            crc ^= u32::from(*b);
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }
}
