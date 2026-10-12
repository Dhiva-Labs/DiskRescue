//! What the user asked to recover.

use std::collections::BTreeSet;

use diskrescue_core::fs::{EntryConfidence, FileEntry};
use diskrescue_core::media::MediaCategory;

/// Which files to recover.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Selection {
    /// Categories to keep. Empty means everything.
    pub categories: BTreeSet<MediaCategory>,
    /// Recover files that still exist on the filesystem.
    pub existing: bool,
    /// Recover deleted files.
    pub deleted: bool,
    /// Skip files smaller than this many bytes (thumbnails, icons).
    pub min_size: u64,
    /// Skip files larger than this (0 = no limit).
    pub max_size: u64,
    /// Also keep files whose extension is unknown but whose content sniffs
    /// as a wanted format.
    pub sniff_unknown: bool,
}

impl Default for Selection {
    fn default() -> Self {
        Selection::photos_and_videos()
    }
}

impl Selection {
    /// Photos (including RAW) and videos: the default.
    pub fn photos_and_videos() -> Self {
        Selection {
            categories: [
                MediaCategory::Photo,
                MediaCategory::RawPhoto,
                MediaCategory::Video,
            ]
            .into_iter()
            .collect(),
            existing: true,
            deleted: true,
            min_size: 0,
            max_size: 0,
            sniff_unknown: true,
        }
    }

    /// Every file regardless of type.
    pub fn everything() -> Self {
        Selection {
            categories: BTreeSet::new(),
            ..Selection::photos_and_videos()
        }
    }

    /// True when `category` is wanted.
    pub fn wants_category(&self, category: MediaCategory) -> bool {
        self.categories.is_empty() || self.categories.contains(&category)
    }

    /// True when every category is wanted (no type filtering at all).
    pub fn wants_everything(&self) -> bool {
        self.categories.is_empty()
    }

    /// Size limits.
    pub fn wants_size(&self, size: u64) -> bool {
        size >= self.min_size && (self.max_size == 0 || size <= self.max_size)
    }

    /// Existing/deleted switch plus "is there any data to recover".
    pub fn wants_entry_state(&self, entry: &FileEntry) -> bool {
        if entry.is_dir {
            return false;
        }
        if entry.deleted {
            self.deleted && entry.confidence != EntryConfidence::DeletedNoData
        } else {
            self.existing
        }
    }
}

/// Category guessed from a file extension, for filesystem entries before
/// their content has been looked at.
pub fn category_for_extension(ext: &str) -> Option<MediaCategory> {
    let e = ext.trim_start_matches('.').to_ascii_lowercase();
    Some(match e.as_str() {
        "jpg" | "jpeg" | "jpe" | "jfif" | "png" | "gif" | "bmp" | "webp" | "tif" | "tiff"
        | "heic" | "heif" | "avif" | "psd" | "mpo" | "jxl" | "ico" => MediaCategory::Photo,
        "cr2" | "cr3" | "crw" | "nef" | "nrw" | "arw" | "srf" | "sr2" | "dng" | "pef" | "orf"
        | "rw2" | "raf" | "srw" | "erf" | "3fr" | "iiq" | "x3f" | "rwl" | "kdc" | "dcr" | "mrw"
        | "mef" | "mos" => MediaCategory::RawPhoto,
        "mp4" | "m4v" | "mov" | "qt" | "3gp" | "3g2" | "avi" | "mkv" | "webm" | "mts" | "m2ts"
        | "ts" | "mpg" | "mpeg" | "vob" | "wmv" | "asf" | "flv" | "lrv" | "insv" | "mod"
        | "tod" | "dv" => MediaCategory::Video,
        "mp3" | "wav" | "flac" | "m4a" | "aac" | "ogg" | "opus" | "wma" | "aiff" | "aif"
        | "amr" => MediaCategory::Audio,
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "odt" | "ods" | "txt"
        | "rtf" => MediaCategory::Document,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskrescue_core::ranges::Range;

    fn entry(name: &str, deleted: bool, conf: EntryConfidence) -> FileEntry {
        FileEntry {
            id: 1,
            parent_id: None,
            path: name.into(),
            name: name.into(),
            size: 100,
            is_dir: false,
            deleted,
            confidence: conf,
            created: None,
            modified: None,
            accessed: None,
            extents: vec![Range::new(0, 100)],
            resident: false,
            flags: vec![],
        }
    }

    #[test]
    fn default_selection() {
        let s = Selection::default();
        assert!(s.wants_category(MediaCategory::Photo));
        assert!(s.wants_category(MediaCategory::RawPhoto));
        assert!(s.wants_category(MediaCategory::Video));
        assert!(!s.wants_category(MediaCategory::Document));
        assert!(Selection::everything().wants_category(MediaCategory::Document));
        assert!(s.wants_entry_state(&entry("a.jpg", false, EntryConfidence::Allocated)));
        assert!(s.wants_entry_state(&entry("a.jpg", true, EntryConfidence::DeletedGuessed)));
        assert!(!s.wants_entry_state(&entry("a.jpg", true, EntryConfidence::DeletedNoData)));
        let mut dir = entry("d", false, EntryConfidence::Allocated);
        dir.is_dir = true;
        assert!(!s.wants_entry_state(&dir));
    }

    #[test]
    fn sizes_and_extensions() {
        let s = Selection {
            min_size: 10,
            max_size: 1000,
            ..Selection::default()
        };
        assert!(!s.wants_size(9));
        assert!(s.wants_size(10));
        assert!(!s.wants_size(1001));
        assert_eq!(category_for_extension("JPG"), Some(MediaCategory::Photo));
        assert_eq!(
            category_for_extension(".cr3"),
            Some(MediaCategory::RawPhoto)
        );
        assert_eq!(category_for_extension("MTS"), Some(MediaCategory::Video));
        assert_eq!(category_for_extension("exe"), None);
    }
}
