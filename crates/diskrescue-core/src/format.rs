//! The interface every file format implements for carving, validation and
//! repair.
//!
//! A format is stateless and shared across threads. The carver calls
//! [`FileFormat::measure`] at every signature hit to learn the exact extent
//! of the file; the recover pipeline calls [`FileFormat::validate`] on
//! extracted files and [`FileFormat::repair`] when validation reports damage.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use crate::media::{MediaCategory, MediaMeta};
use crate::source::SharedSource;
use crate::Result;

/// A byte pattern that marks the start of a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    /// Bytes to match.
    pub bytes: &'static [u8],
    /// Offset of `bytes` from the start of the file (e.g. 4 for `ftyp`).
    pub offset: u64,
    /// Optional second pattern that must also match (e.g. a brand inside
    /// `ftyp`), with its own offset from the file start.
    pub also: Option<(&'static [u8], u64)>,
}

impl Signature {
    /// Pattern at file offset 0.
    pub const fn at_start(bytes: &'static [u8]) -> Self {
        Signature {
            bytes,
            offset: 0,
            also: None,
        }
    }

    /// Pattern at a given file offset.
    pub const fn at(bytes: &'static [u8], offset: u64) -> Self {
        Signature {
            bytes,
            offset,
            also: None,
        }
    }

    /// Pattern with a secondary requirement.
    pub const fn with_also(mut self, bytes: &'static [u8], offset: u64) -> Self {
        self.also = Some((bytes, offset));
        self
    }

    /// Number of bytes from the file start needed to evaluate this signature.
    pub fn probe_len(&self) -> u64 {
        let a = self.offset + self.bytes.len() as u64;
        match self.also {
            Some((b, o)) => a.max(o + b.len() as u64),
            None => a,
        }
    }

    /// Check the signature against the first bytes of a candidate file.
    pub fn matches(&self, head: &[u8]) -> bool {
        let ok = |pat: &[u8], off: u64| -> bool {
            let off = off as usize;
            head.len() >= off + pat.len() && &head[off..off + pat.len()] == pat
        };
        ok(self.bytes, self.offset) && self.also.map_or(true, |(b, o)| ok(b, o))
    }
}

/// How confident a measurement is that the bytes form one real file.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    /// Signature only; no structural check passed.
    Low,
    /// Structure parsed; end found by walking the format.
    Medium,
    /// Structure parsed and the content validated (checksums, decode).
    High,
}

/// The extent of a candidate file found at a signature hit.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Measured {
    /// Length in bytes from the signature start.
    pub len: u64,
    /// True when the format's own end marker/size was found; false when the
    /// parser stopped at damage, at the source end, or at a size cap.
    pub complete: bool,
    /// Confidence in this being a real, single file.
    pub confidence: Confidence,
    /// Metadata gathered on the way.
    pub meta: MediaMeta,
    /// Byte ranges (relative to the file start) that another format should
    /// not claim as separate files: embedded thumbnails, previews, nested
    /// containers. The carver suppresses signature hits inside them.
    pub nested: Vec<crate::ranges::Range>,
    /// Problems or observations ("no EOI marker, truncated at source end").
    pub notes: Vec<String>,
}

impl Measured {
    /// A complete file of `len` bytes.
    pub fn complete(len: u64, confidence: Confidence) -> Self {
        Measured {
            len,
            complete: true,
            confidence,
            meta: MediaMeta::default(),
            nested: Vec::new(),
            notes: Vec::new(),
        }
    }

    /// A file whose end could not be established.
    pub fn truncated(len: u64, confidence: Confidence, note: impl Into<String>) -> Self {
        Measured {
            len,
            complete: false,
            confidence,
            meta: MediaMeta::default(),
            nested: Vec::new(),
            notes: vec![note.into()],
        }
    }
}

/// Outcome of validating a whole file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationStatus {
    /// Structure and content check out.
    Valid,
    /// Recognised but damaged (truncated, bad checksums, decode errors).
    Damaged,
    /// Not a file of this format at all.
    Invalid,
}

/// Validation result.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Validation {
    /// Verdict.
    pub status: ValidationStatus,
    /// Fraction of the content that decodes/parses (0.0..=1.0), when the
    /// format can estimate it (rows of a JPEG, samples of a video).
    pub intact_fraction: Option<f32>,
    /// Problems found, most important first.
    pub problems: Vec<String>,
    /// Metadata extracted.
    pub meta: MediaMeta,
    /// Repairs the format believes it could apply (names understood by
    /// `repair`), empty when none.
    pub repairable_by: Vec<String>,
}

impl Validation {
    /// A clean result.
    pub fn valid(meta: MediaMeta) -> Self {
        Validation {
            status: ValidationStatus::Valid,
            intact_fraction: Some(1.0),
            problems: Vec::new(),
            meta,
            repairable_by: Vec::new(),
        }
    }

    /// An invalid result.
    pub fn invalid(reason: impl Into<String>) -> Self {
        Validation {
            status: ValidationStatus::Invalid,
            intact_fraction: Some(0.0),
            problems: vec![reason.into()],
            meta: MediaMeta::default(),
            repairable_by: Vec::new(),
        }
    }
}

/// What a repair attempt produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepairResult {
    /// Output validates as a correct file.
    Repaired,
    /// Output is better than the input but still damaged (partial image).
    PartiallyRepaired,
    /// Input was already fine; output equals input.
    NoRepairNeeded,
    /// Nothing could be done; no output written.
    Unrepairable,
}

/// Repair outcome.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RepairOutcome {
    /// Verdict.
    pub result: RepairResult,
    /// Steps applied, in order ("appended EOI", "rebuilt moov from donor").
    pub applied: Vec<String>,
    /// Bytes written to the output.
    pub output_len: u64,
    /// Validation of the output, when it was re-validated.
    pub validation: Option<Validation>,
    /// Notes for the user ("colours may be off: donor quantisation tables").
    pub notes: Vec<String>,
}

/// Inputs available to a repair.
#[derive(Debug, Clone, Default)]
pub struct RepairContext {
    /// Healthy files from the same device/camera, used as donors for
    /// headers, Huffman tables, sample descriptions.
    pub donors: Vec<PathBuf>,
    /// Allow repairs that change content (truncate at damage, pad rows).
    pub aggressive: bool,
    /// Scratch directory for temporaries, if needed.
    pub scratch_dir: Option<PathBuf>,
}

/// A file format known to DiskRescue.
pub trait FileFormat: Send + Sync {
    /// Stable identifier (`jpeg`, `png`, `mp4`, `cr2`).
    fn id(&self) -> &'static str;

    /// Human name (`JPEG image`).
    fn name(&self) -> &'static str;

    /// Preferred extension first (`["jpg", "jpeg"]`).
    fn extensions(&self) -> &'static [&'static str];

    /// Category used to organise output.
    fn category(&self) -> MediaCategory;

    /// Signatures that start a file of this format.
    fn signatures(&self) -> &'static [Signature];

    /// Smallest plausible file size.
    fn min_size(&self) -> u64;

    /// Largest plausible file size; the carver stops measuring beyond it.
    fn max_size(&self) -> u64;

    /// Carving priority when several formats claim the same bytes; higher
    /// wins (a container beats a format embedded in it).
    fn priority(&self) -> u8 {
        50
    }

    /// Given a source positioned so that the file starts at `start`, walk the
    /// format and return its extent. Must not read beyond `start + limit`.
    /// Return `Ok(None)` when the bytes are clearly not this format (false
    /// positive). Must be cheap for false positives and streaming for large
    /// files (do not read the whole file into memory).
    fn measure(&self, src: &SharedSource, start: u64, limit: u64) -> Result<Option<Measured>>;

    /// Deep check of a complete file (`src` spans exactly the file).
    fn validate(&self, src: &SharedSource) -> Result<Validation>;

    /// Attempt to repair `src` into `out`. Default: unsupported.
    fn repair(
        &self,
        src: &SharedSource,
        out: &mut dyn Write,
        ctx: &RepairContext,
    ) -> Result<RepairOutcome> {
        let _ = (src, out, ctx);
        Err(crate::Error::unsupported(format!(
            "{} files cannot be repaired by this version",
            self.name()
        )))
    }

    /// Whether `repair` does anything for this format.
    fn can_repair(&self) -> bool {
        false
    }
}

/// A set of formats, with lookup helpers.
#[derive(Clone, Default)]
pub struct FormatRegistry {
    formats: Vec<Arc<dyn FileFormat>>,
}

impl FormatRegistry {
    /// Empty registry.
    pub fn new() -> Self {
        FormatRegistry {
            formats: Vec::new(),
        }
    }

    /// Add a format. Later registrations with the same id replace earlier ones.
    pub fn register(&mut self, f: Arc<dyn FileFormat>) {
        self.formats.retain(|x| x.id() != f.id());
        self.formats.push(f);
    }

    /// Builder-style registration.
    pub fn with(mut self, f: Arc<dyn FileFormat>) -> Self {
        self.register(f);
        self
    }

    /// All formats.
    pub fn all(&self) -> &[Arc<dyn FileFormat>] {
        &self.formats
    }

    /// Find by id.
    pub fn by_id(&self, id: &str) -> Option<Arc<dyn FileFormat>> {
        self.formats.iter().find(|f| f.id() == id).cloned()
    }

    /// Find by file extension (case-insensitive, with or without dot).
    pub fn by_extension(&self, ext: &str) -> Option<Arc<dyn FileFormat>> {
        let ext = ext.trim_start_matches('.').to_ascii_lowercase();
        self.formats
            .iter()
            .find(|f| f.extensions().iter().any(|e| *e == ext))
            .cloned()
    }

    /// Formats whose signatures match the given file head, best priority first.
    pub fn matching(&self, head: &[u8]) -> Vec<Arc<dyn FileFormat>> {
        let mut v: Vec<_> = self
            .formats
            .iter()
            .filter(|f| f.signatures().iter().any(|s| s.matches(head)))
            .cloned()
            .collect();
        v.sort_by_key(|f| std::cmp::Reverse(f.priority()));
        v
    }

    /// Longest `probe_len` over all signatures (how many bytes the carver
    /// needs to look at to evaluate every signature).
    pub fn max_probe_len(&self) -> u64 {
        self.formats
            .iter()
            .flat_map(|f| f.signatures().iter().map(Signature::probe_len))
            .max()
            .unwrap_or(0)
    }

    /// Number of formats.
    pub fn len(&self) -> usize {
        self.formats.len()
    }

    /// True when empty.
    pub fn is_empty(&self) -> bool {
        self.formats.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_matching() {
        let s = Signature::at(b"ftyp", 4).with_also(b"isom", 8);
        assert_eq!(s.probe_len(), 12);
        assert!(s.matches(b"\0\0\0\x18ftypisom\0\0\0\0"));
        assert!(!s.matches(b"\0\0\0\x18ftypqt  "));
        assert!(!s.matches(b"\0\0\0\x18ftyp"));
        let j = Signature::at_start(b"\xFF\xD8\xFF");
        assert!(j.matches(b"\xFF\xD8\xFF\xE1"));
        assert!(!j.matches(b"\xFF\xD8"));
    }
}
