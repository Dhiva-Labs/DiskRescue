//! The recovery report: a JSON record of everything that happened plus a
//! plain-language summary.
//!
//! The report must tell the truth: which files are complete, which contain
//! unreadable sectors (zero-filled), which were guessed (deleted files with
//! assumed extents), which were repaired and how, and which are duplicates.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use diskrescue_core::device::DeviceKind;
use diskrescue_core::format::{Confidence, ExtentStatus, RepairResult, ValidationStatus};
use diskrescue_core::fs::{EntryConfidence, FsKind};
use diskrescue_core::media::{MediaCategory, MediaMeta};
use diskrescue_core::partition::PartitionScheme;
use diskrescue_core::ranges::Range;
use diskrescue_core::time::Timestamp;
use diskrescue_core::util::human;

/// Where a recovered file came from.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Origin {
    /// Read through a filesystem parser.
    Filesystem {
        /// 1-based volume index.
        volume: u32,
        /// Filesystem family.
        fs: FsKind,
        /// Path inside the volume.
        path: String,
        /// Deleted entry.
        deleted: bool,
        /// Parser's confidence in the data location.
        confidence: EntryConfidence,
    },
    /// Found by the carver.
    Carved {
        /// Byte offset in the source.
        offset: u64,
        /// Length in bytes.
        length: u64,
        /// How the measurement ended.
        status: ExtentStatus,
        /// Carver confidence.
        confidence: Confidence,
        /// Id of the file this was embedded in, when promoted from a damaged parent.
        parent: Option<u64>,
    },
    /// A file given directly to the `repair` or `verify` command.
    Input {
        /// Its path.
        path: PathBuf,
    },
}

/// Final state of a file.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Recovered and validated.
    Recovered,
    /// Recovered; the format check found no problem but the format cannot
    /// be validated deeply (unknown type, recognise-only formats).
    RecoveredUnverified,
    /// Recovered but damaged; no repair was possible or attempted.
    Damaged,
    /// Damaged, and a repaired copy validates cleanly.
    Repaired,
    /// Damaged, and a repaired copy is better but still incomplete.
    PartiallyRepaired,
    /// Same content as an earlier file; not written again.
    Duplicate,
    /// Could not be recovered (read failure, no data).
    Failed,
}

impl Outcome {
    /// Label for summaries.
    pub fn label(&self) -> &'static str {
        match self {
            Outcome::Recovered => "recovered",
            Outcome::RecoveredUnverified => "recovered (not verified)",
            Outcome::Damaged => "damaged",
            Outcome::Repaired => "repaired",
            Outcome::PartiallyRepaired => "partially repaired",
            Outcome::Duplicate => "duplicate (skipped)",
            Outcome::Failed => "failed",
        }
    }
}

/// Validation outcome as stored in the report.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ValidationRecord {
    /// Verdict.
    pub status: ValidationStatus,
    /// Fraction of the content that decodes.
    pub intact_fraction: Option<f32>,
    /// Problems found.
    pub problems: Vec<String>,
}

/// Repair outcome as stored in the report.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RepairRecord {
    /// Verdict.
    pub result: RepairResult,
    /// Steps applied.
    pub applied: Vec<String>,
    /// Notes for the user.
    pub notes: Vec<String>,
    /// Donor used, if any.
    pub donor: Option<PathBuf>,
    /// Repaired copy, relative to the output root.
    pub output: Option<PathBuf>,
}

/// One recovered (or attempted) file.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FileRecord {
    /// Report-unique id.
    pub id: u64,
    /// Where it came from.
    pub origin: Origin,
    /// Written file, relative to the output root.
    pub output: Option<PathBuf>,
    /// Detected format id.
    pub format: Option<String>,
    /// Category.
    pub category: MediaCategory,
    /// Size in bytes.
    pub size: u64,
    /// BLAKE3 of the written content.
    pub blake3: Option<String>,
    /// Final state.
    pub outcome: Outcome,
    /// Validation of the recovered bytes.
    pub validation: Option<ValidationRecord>,
    /// Repair attempt, if any.
    pub repair: Option<RepairRecord>,
    /// File-relative ranges that could not be read and were zero-filled.
    pub unreadable: Vec<Range>,
    /// Total unreadable bytes.
    pub unreadable_bytes: u64,
    /// Metadata.
    pub meta: MediaMeta,
    /// Earlier file with identical content.
    pub duplicate_of: Option<u64>,
    /// Anything else worth telling the user.
    pub notes: Vec<String>,
}

impl FileRecord {
    /// A record with defaults for everything but origin, category and size.
    pub fn new(origin: Origin, category: MediaCategory, size: u64) -> Self {
        FileRecord {
            id: 0,
            origin,
            output: None,
            format: None,
            category,
            size,
            blake3: None,
            outcome: Outcome::RecoveredUnverified,
            validation: None,
            repair: None,
            unreadable: Vec::new(),
            unreadable_bytes: 0,
            meta: MediaMeta::default(),
            duplicate_of: None,
            notes: Vec::new(),
        }
    }

    /// Short description of where the file came from.
    pub fn origin_text(&self) -> String {
        match &self.origin {
            Origin::Filesystem {
                volume,
                path,
                deleted,
                ..
            } => {
                format!(
                    "vol{volume}:/{path}{}",
                    if *deleted { " (deleted)" } else { "" }
                )
            }
            Origin::Carved { offset, .. } => format!("carved at offset {offset:#x}"),
            Origin::Input { path } => path.display().to_string(),
        }
    }
}

/// What we know about the source.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SourceRecord {
    /// Human description.
    pub description: String,
    /// Path given by the user.
    pub path: Option<PathBuf>,
    /// Device class.
    pub kind: Option<DeviceKind>,
    /// Size in bytes.
    pub size: u64,
    /// Sector size.
    pub sector_size: u32,
    /// Model.
    pub model: Option<String>,
    /// Serial.
    pub serial: Option<String>,
    /// Mapfile of the rescued image, when the source is one.
    pub mapfile: Option<PathBuf>,
    /// Bytes known to be unreadable on the source.
    pub unreadable_bytes: u64,
}

/// One volume that was examined.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct VolumeRecord {
    /// 1-based index.
    pub index: u32,
    /// Byte offset in the source.
    pub start: u64,
    /// Size in bytes.
    pub size: u64,
    /// Filesystem, when recognised.
    pub fs: Option<FsKind>,
    /// Label.
    pub label: Option<String>,
    /// Where the volume was found (`mbr`, `gpt`, `scan`, ...).
    pub origin: String,
    /// Entries listed (files only).
    pub files_listed: u64,
    /// Deleted entries listed.
    pub deleted_listed: u64,
    /// Warnings from the parser.
    pub warnings: Vec<String>,
    /// The parser could not open the volume; why.
    pub error: Option<String>,
}

/// Statistics from the carving stage.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CarveRecord {
    /// Bytes scanned.
    pub scanned: u64,
    /// Signature hits.
    pub candidates: u64,
    /// Files accepted.
    pub files: u64,
    /// Hits rejected as false positives.
    pub false_positives: u64,
    /// Whether only unallocated space was scanned.
    pub unallocated_only: bool,
}

/// Aggregates computed by [`Report::finalize`].
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Totals {
    /// Files written (excluding duplicates and failures).
    pub files_written: u64,
    /// Bytes written (originals only).
    pub bytes_written: u64,
    /// Count per category.
    pub by_category: BTreeMap<String, u64>,
    /// Count per outcome.
    pub by_outcome: BTreeMap<String, u64>,
    /// Files recovered through filesystems.
    pub from_filesystem: u64,
    /// Of those, deleted.
    pub deleted: u64,
    /// Files found by carving.
    pub carved: u64,
    /// Files containing unreadable sectors.
    pub with_holes: u64,
}

/// The full report.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Report {
    /// Tool name.
    pub tool: String,
    /// Tool version.
    pub version: String,
    /// Command line, when known.
    pub command_line: Option<String>,
    /// Start time.
    pub started: Timestamp,
    /// End time.
    pub finished: Option<Timestamp>,
    /// The source.
    pub source: SourceRecord,
    /// Partitioning scheme found.
    pub scheme: Option<PartitionScheme>,
    /// Volumes examined.
    pub volumes: Vec<VolumeRecord>,
    /// Carving statistics.
    pub carve: Option<CarveRecord>,
    /// Every file.
    pub files: Vec<FileRecord>,
    /// Aggregates.
    pub totals: Totals,
    /// Run-level warnings.
    pub warnings: Vec<String>,
    /// The run was interrupted before finishing.
    pub interrupted: bool,
}

impl Report {
    /// Empty report started now.
    pub fn new(command_line: Option<String>) -> Self {
        Report {
            tool: "diskrescue".into(),
            version: diskrescue_core::VERSION.into(),
            command_line,
            started: Timestamp::now(),
            finished: None,
            source: SourceRecord::default(),
            scheme: None,
            volumes: Vec::new(),
            carve: None,
            files: Vec::new(),
            totals: Totals::default(),
            warnings: Vec::new(),
            interrupted: false,
        }
    }

    /// Add a file record, assigning its id. Returns the id.
    pub fn add_file(&mut self, mut rec: FileRecord) -> u64 {
        let id = self.files.len() as u64 + 1;
        rec.id = id;
        self.files.push(rec);
        id
    }

    /// Mutable access by id.
    pub fn file_mut(&mut self, id: u64) -> Option<&mut FileRecord> {
        id.checked_sub(1)
            .and_then(|i| self.files.get_mut(i as usize))
    }

    /// Recompute totals and stamp the finish time.
    pub fn finalize(&mut self) {
        let mut t = Totals::default();
        for f in &self.files {
            *t.by_outcome
                .entry(f.outcome.label().to_string())
                .or_default() += 1;
            if matches!(f.outcome, Outcome::Duplicate | Outcome::Failed) {
                continue;
            }
            t.files_written += 1;
            t.bytes_written += f.size;
            *t.by_category
                .entry(f.category.dir_name().to_string())
                .or_default() += 1;
            match &f.origin {
                Origin::Filesystem { deleted, .. } => {
                    t.from_filesystem += 1;
                    if *deleted {
                        t.deleted += 1;
                    }
                }
                Origin::Carved { .. } => t.carved += 1,
                Origin::Input { .. } => {}
            }
            if f.unreadable_bytes > 0 {
                t.with_holes += 1;
            }
        }
        self.totals = t;
        self.finished = Some(Timestamp::now());
    }

    /// Write `report.json` atomically.
    pub fn write_json(&self, path: &Path) -> std::io::Result<()> {
        let json = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        write_atomic(path, &json)
    }

    /// Load a report written by [`Report::write_json`].
    pub fn read_json(path: &Path) -> std::io::Result<Self> {
        let data = std::fs::read(path)?;
        serde_json::from_slice(&data).map_err(std::io::Error::other)
    }

    /// Write `report.txt` atomically.
    pub fn write_text(&self, path: &Path) -> std::io::Result<()> {
        write_atomic(path, self.text_summary().as_bytes())
    }

    /// Human-readable summary.
    pub fn text_summary(&self) -> String {
        let mut s = String::new();
        let _ = writeln!(s, "DiskRescue {} recovery report", self.version);
        let _ = writeln!(s, "Started:  {}", self.started);
        if let Some(f) = self.finished {
            let _ = writeln!(s, "Finished: {f}");
        }
        if self.interrupted {
            let _ = writeln!(s, "NOTE: the run was interrupted; results are incomplete.");
        }
        let _ = writeln!(s);
        let src = &self.source;
        let _ = writeln!(s, "Source: {}", src.description);
        if let Some(k) = src.kind {
            let _ = writeln!(s, "  Type: {}", k.display_name());
        }
        if src.size > 0 {
            let _ = writeln!(
                s,
                "  Size: {} ({} bytes)",
                human::bytes(src.size),
                human::count(src.size)
            );
        }
        if let Some(m) = &src.model {
            let _ = writeln!(s, "  Model: {m}");
        }
        if src.unreadable_bytes > 0 {
            let _ = writeln!(
                s,
                "  Unreadable: {} could not be read from the device",
                human::bytes(src.unreadable_bytes)
            );
        }
        if let Some(scheme) = self.scheme {
            let _ = writeln!(s, "  Partitioning: {scheme:?}");
        }
        let _ = writeln!(s);

        if !self.volumes.is_empty() {
            let _ = writeln!(s, "Volumes:");
            for v in &self.volumes {
                let fs =
                    v.fs.map(|f| f.name().to_string())
                        .unwrap_or_else(|| "unrecognised".into());
                let label = v
                    .label
                    .as_deref()
                    .map(|l| format!(" \"{l}\""))
                    .unwrap_or_default();
                let _ = writeln!(
                    s,
                    "  {}. {fs}{label} at {} ({}), found via {}: {} files listed, {} deleted",
                    v.index,
                    human::bytes(v.start),
                    human::bytes(v.size),
                    v.origin,
                    v.files_listed,
                    v.deleted_listed
                );
                if let Some(e) = &v.error {
                    let _ = writeln!(s, "     could not be read: {e}");
                }
                for w in &v.warnings {
                    let _ = writeln!(s, "     warning: {w}");
                }
            }
            let _ = writeln!(s);
        }

        if let Some(c) = &self.carve {
            let space = if c.unallocated_only {
                "free space"
            } else {
                "the whole source"
            };
            let _ = writeln!(
                s,
                "Carving: scanned {} of {space}; {} signature hits, {} files, {} rejected",
                human::bytes(c.scanned),
                human::count(c.candidates),
                human::count(c.files),
                human::count(c.false_positives)
            );
            let _ = writeln!(s);
        }

        let t = &self.totals;
        let _ = writeln!(
            s,
            "Recovered {} files ({})",
            human::count(t.files_written),
            human::bytes(t.bytes_written)
        );
        for (cat, n) in &t.by_category {
            let _ = writeln!(s, "  {cat}: {n}");
        }
        let _ = writeln!(
            s,
            "  through the filesystem: {} ({} deleted); carved: {}",
            t.from_filesystem, t.deleted, t.carved
        );
        for (outcome, n) in &t.by_outcome {
            let _ = writeln!(s, "  {outcome}: {n}");
        }
        if t.with_holes > 0 {
            let _ = writeln!(
                s,
                "  {} files contain unreadable sectors (filled with zeros); see 'unreadable' in report.json",
                t.with_holes
            );
        }
        let _ = writeln!(s);

        let problems: Vec<&FileRecord> = self
            .files
            .iter()
            .filter(|f| {
                matches!(
                    f.outcome,
                    Outcome::Damaged
                        | Outcome::PartiallyRepaired
                        | Outcome::Repaired
                        | Outcome::Failed
                )
            })
            .collect();
        if !problems.is_empty() {
            let _ = writeln!(s, "Files needing attention:");
            for f in problems.iter().take(500) {
                let out = f
                    .output
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "-".into());
                let _ = writeln!(s, "  [{}] {out}  ({})", f.outcome.label(), f.origin_text());
                if let Some(v) = &f.validation {
                    for p in v.problems.iter().take(3) {
                        let _ = writeln!(s, "      problem: {p}");
                    }
                }
                if let Some(r) = &f.repair {
                    if !r.applied.is_empty() {
                        let _ = writeln!(s, "      repair: {}", r.applied.join(", "));
                    }
                    if let Some(o) = &r.output {
                        let _ = writeln!(s, "      repaired copy: {}", o.display());
                    }
                    for n in r.notes.iter().take(2) {
                        let _ = writeln!(s, "      note: {n}");
                    }
                }
            }
            if problems.len() > 500 {
                let _ = writeln!(
                    s,
                    "  ... and {} more (see report.json)",
                    problems.len() - 500
                );
            }
            let _ = writeln!(s);
        }

        let needs_reference = self.files.iter().any(|f| {
            f.outcome == Outcome::Damaged
                && f.validation
                    .as_ref()
                    .is_some_and(|v| v.problems.iter().any(|p| p.contains("moov")))
        });
        if needs_reference {
            let _ = writeln!(
                s,
                "Some videos lost their index (moov atom). They can be rebuilt from a healthy video recorded\n\
                 with the same phone or camera and the same settings:\n\
                 \x20 diskrescue repair <damaged video> --donor <healthy video> -o <output dir>"
            );
            let _ = writeln!(s);
        }

        if !self.warnings.is_empty() {
            let _ = writeln!(s, "Warnings:");
            for w in &self.warnings {
                let _ = writeln!(s, "  - {w}");
            }
        }
        s
    }
}

/// Write a file via a temporary sibling and rename, so a crash never leaves
/// a half-written report.
pub fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let tmp = path.with_extension(format!(
        "{}.tmp",
        path.extension()
            .map(|e| e.to_string_lossy().into_owned())
            .unwrap_or_default()
    ));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn totals_and_roundtrip() {
        let mut r = Report::new(Some("diskrescue recover /dev/sdb -o out".into()));
        let mut a = FileRecord::new(
            Origin::Filesystem {
                volume: 1,
                fs: FsKind::Fat32,
                path: "DCIM/IMG_1.JPG".into(),
                deleted: true,
                confidence: EntryConfidence::DeletedGuessed,
            },
            MediaCategory::Photo,
            1000,
        );
        a.outcome = Outcome::Recovered;
        a.unreadable = vec![Range::new(0, 512)];
        a.unreadable_bytes = 512;
        let id_a = r.add_file(a);
        let mut b = FileRecord::new(
            Origin::Carved {
                offset: 0x4000,
                length: 5000,
                status: ExtentStatus::Truncated,
                confidence: Confidence::MEDIUM,
                parent: None,
            },
            MediaCategory::Video,
            5000,
        );
        b.outcome = Outcome::Damaged;
        b.validation = Some(ValidationRecord {
            status: ValidationStatus::Damaged,
            intact_fraction: Some(0.5),
            problems: vec!["moov missing".into()],
        });
        r.add_file(b);
        let mut c = FileRecord::new(
            Origin::Carved {
                offset: 0x9000,
                length: 1000,
                status: ExtentStatus::Complete,
                confidence: Confidence::HIGH,
                parent: None,
            },
            MediaCategory::Photo,
            1000,
        );
        c.outcome = Outcome::Duplicate;
        c.duplicate_of = Some(id_a);
        r.add_file(c);
        r.finalize();
        assert_eq!(r.totals.files_written, 2);
        assert_eq!(r.totals.bytes_written, 6000);
        assert_eq!(r.totals.deleted, 1);
        assert_eq!(r.totals.carved, 1);
        assert_eq!(r.totals.with_holes, 1);
        assert_eq!(r.totals.by_outcome.get("duplicate (skipped)"), Some(&1));
        let text = r.text_summary();
        assert!(text.contains("Recovered 2 files"));
        assert!(text.contains("moov"));
        assert!(text.contains("diskrescue repair"));
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("report.json");
        r.write_json(&p).unwrap();
        let back = Report::read_json(&p).unwrap();
        assert_eq!(back, r);
        r.write_text(&dir.path().join("report.txt")).unwrap();
        assert!(!dir.path().join("report.json.tmp").exists());
        assert!(r.file_mut(0).is_none());
        assert!(r.file_mut(2).is_some());
    }
}
