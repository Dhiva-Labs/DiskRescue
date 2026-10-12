//! The output directory layout.
//!
//! ```text
//! <out>/
//!   files/<volume>/<original path>      existing files, by their original path
//!   deleted/<volume>/<original path>    deleted files recovered through the filesystem
//!   carved/<category>/<YYYY>/<MM>/...   carved files with a capture date
//!   carved/<category>/undated/...       carved files without one
//!   repaired/<same relative path>       repaired copies (originals are kept)
//!   report.json, report.txt
//! ```
//!
//! Files are always created with `create_new`, so an existing file is never
//! overwritten; on a name clash the next free `name (N).ext` is used.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use diskrescue_core::fs::{FileEntry, FsKind};
use diskrescue_core::media::MediaCategory;
use diskrescue_core::time::Timestamp;

use crate::naming::{carved_name, sanitize_component, sanitize_relative_path, unique_path};

/// The output tree rooted at a directory chosen by the user.
#[derive(Debug, Clone)]
pub struct OutputTree {
    root: PathBuf,
}

impl OutputTree {
    /// Use `root` (created if missing).
    pub fn create(root: impl Into<PathBuf>) -> std::io::Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        Ok(OutputTree { root })
    }

    /// The root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Directory name for a volume: `vol<N>_<fs>[_<label>]`.
    pub fn volume_dir(index: u32, fs: Option<FsKind>, label: Option<&str>) -> String {
        let fs = fs.map(|f| f.name()).unwrap_or("unknown");
        match label.map(str::trim).filter(|l| !l.is_empty()) {
            Some(l) => sanitize_component(&format!("vol{index}_{fs}_{l}")),
            None => format!("vol{index}_{fs}"),
        }
    }

    /// Relative path for a file recovered through a filesystem.
    pub fn fs_relative(volume_dir: &str, entry: &FileEntry) -> PathBuf {
        let top = if entry.deleted { "deleted" } else { "files" };
        Path::new(top)
            .join(sanitize_component(volume_dir))
            .join(sanitize_relative_path(&entry.path))
    }

    /// Relative path for a carved file.
    pub fn carved_relative(
        category: MediaCategory,
        created: Option<Timestamp>,
        offset: u64,
        ext: &str,
    ) -> PathBuf {
        let date_dir = match created.filter(Timestamp::is_plausible) {
            Some(ts) => PathBuf::from(ts.year_month_path()),
            None => PathBuf::from("undated"),
        };
        Path::new("carved")
            .join(category.dir_name())
            .join(date_dir)
            .join(carved_name(offset, created, ext))
    }

    /// Relative path for the repaired copy of `original` (itself relative).
    /// Files from outside the tree (the `repair` command) keep only their name.
    pub fn repaired_relative(original: &Path) -> PathBuf {
        let rel: PathBuf = if original.is_absolute() {
            PathBuf::from(original.file_name().unwrap_or_default())
        } else {
            original.to_path_buf()
        };
        Path::new("repaired").join(rel)
    }

    /// Create a new file at `rel` (relative to the root), making parent
    /// directories as needed and choosing a free name on clashes. Returns
    /// the open file and its path relative to the root.
    pub fn create_new(&self, rel: &Path) -> std::io::Result<(File, PathBuf)> {
        let full = self.root.join(rel);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent)?;
        }
        for _ in 0..16 {
            let candidate = unique_path(&full)?;
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&candidate)
            {
                Ok(f) => {
                    let rel = candidate
                        .strip_prefix(&self.root)
                        .map(Path::to_path_buf)
                        .unwrap_or(candidate);
                    return Ok((f, rel));
                }
                // Lost a race with another worker for the same name: try again.
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("could not create a unique file for {}", full.display()),
        ))
    }

    /// Absolute path for a root-relative path.
    pub fn absolute(&self, rel: &Path) -> PathBuf {
        self.root.join(rel)
    }
}

/// Set modification and access times on a recovered file (best effort).
pub fn set_file_times(file: &File, modified: Option<Timestamp>, accessed: Option<Timestamp>) {
    let to_system = |ts: Timestamp| -> Option<std::time::SystemTime> {
        if ts.unix_secs >= 0 {
            std::time::UNIX_EPOCH
                .checked_add(std::time::Duration::new(ts.unix_secs as u64, ts.nanos))
        } else {
            None
        }
    };
    let mut times = std::fs::FileTimes::new();
    let mut any = false;
    if let Some(m) = modified.and_then(to_system) {
        times = times.set_modified(m);
        any = true;
    }
    if let Some(a) = accessed.or(modified).and_then(to_system) {
        times = times.set_accessed(a);
        any = true;
    }
    if any {
        if let Err(e) = file.set_times(times) {
            log::debug!("could not set file times: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskrescue_core::fs::EntryConfidence;

    fn entry(path: &str, deleted: bool) -> FileEntry {
        FileEntry {
            id: 1,
            parent_id: None,
            path: path.into(),
            name: path.rsplit('/').next().unwrap().into(),
            size: 3,
            is_dir: false,
            deleted,
            confidence: if deleted {
                EntryConfidence::DeletedGuessed
            } else {
                EntryConfidence::Allocated
            },
            created: None,
            modified: None,
            accessed: None,
            extents: vec![],
            resident: false,
            flags: vec![],
        }
    }

    #[test]
    fn layout_paths() {
        assert_eq!(
            OutputTree::volume_dir(1, Some(FsKind::Fat32), Some("CANON ")),
            "vol1_fat32_CANON"
        );
        assert_eq!(OutputTree::volume_dir(2, None, None), "vol2_unknown");
        assert_eq!(
            OutputTree::fs_relative("vol1_fat32", &entry("DCIM/100CANON/IMG_1.JPG", false)),
            PathBuf::from("files/vol1_fat32/DCIM/100CANON/IMG_1.JPG")
        );
        assert_eq!(
            OutputTree::fs_relative("vol1_fat32", &entry("../x/IMG.JPG", true)),
            PathBuf::from("deleted/vol1_fat32/x/IMG.JPG")
        );
        let ts = Timestamp::from_exif_str("2021:12:31 23:59:59").unwrap();
        assert_eq!(
            OutputTree::carved_relative(MediaCategory::Video, Some(ts), 0x200, "mp4"),
            PathBuf::from("carved/videos/2021/12/f000000000200_20211231_235959.mp4")
        );
        assert_eq!(
            OutputTree::carved_relative(MediaCategory::Photo, None, 0x200, "jpg"),
            PathBuf::from("carved/photos/undated/f000000000200.jpg")
        );
        assert_eq!(
            OutputTree::repaired_relative(Path::new("/home/u/broken.mp4")),
            PathBuf::from("repaired/broken.mp4")
        );
        assert_eq!(
            OutputTree::repaired_relative(Path::new("carved/videos/undated/f1.mp4")),
            PathBuf::from("repaired/carved/videos/undated/f1.mp4")
        );
    }

    #[test]
    fn never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let tree = OutputTree::create(dir.path().join("out")).unwrap();
        let (mut f1, r1) = tree.create_new(Path::new("files/a/b.jpg")).unwrap();
        use std::io::Write;
        f1.write_all(b"one").unwrap();
        let (_f2, r2) = tree.create_new(Path::new("files/a/b.jpg")).unwrap();
        assert_eq!(r1, PathBuf::from("files/a/b.jpg"));
        assert_eq!(r2, PathBuf::from("files/a/b (1).jpg"));
        assert_eq!(std::fs::read(tree.absolute(&r1)).unwrap(), b"one");
        let ts = Timestamp::from_exif_str("2020:01:02 03:04:05").unwrap();
        set_file_times(&f1, Some(ts), None);
        drop(f1);
        let m = std::fs::metadata(tree.absolute(&r1))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(
            m.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64,
            ts.unix_secs
        );
    }
}
