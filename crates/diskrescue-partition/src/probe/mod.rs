//! Filesystem probing: signature plus consistency checks, no full parsing.
//!
//! [`probe_filesystem`] answers "is there a filesystem starting here, and how
//! big does it say it is?" by validating the boot sector (FAT, exFAT, NTFS) or
//! superblock (ext2/3/4) the way the filesystem drivers do, without walking
//! any directory. The real parsers live in the filesystem crates; this module
//! exists so partition tables can be sanity-checked, superfloppies detected and
//! lost partitions found.
//!
//! # Order of attempts
//!
//! 1. primary structures: exFAT, NTFS, FAT12/16/32, ext2/3/4, then the
//!    recognise-only families (btrfs, XFS, HFS+, APFS, ISO 9660, LUKS, swap,
//!    squashfs);
//! 2. only when every primary check failed, the redundant copies: the FAT32
//!    backup boot sector (sector 6), the exFAT backup boot region (sector 12,
//!    verified with its checksum sector), the NTFS backup boot sector (last
//!    sector of the volume) and ext2/3/4 backup superblocks at the start of
//!    block groups 1, 3, 5, 7, 9, 25, 27, 49 and 81. A successful probe through
//!    a copy sets [`FsProbe::via_backup`].
//!
//! Every offset and size read from the medium is range-checked before use; a
//! probe never allocates more than a few KiB and never panics on hostile data.

use diskrescue_core::fs::FsKind;
use diskrescue_core::{SharedSource, Source};

use crate::io::read_opt;

pub(crate) mod exfat;
pub(crate) mod ext;
pub(crate) mod fat;
pub(crate) mod ntfs;
pub(crate) mod other;

/// Number of bytes read from the start of a volume for the first round of
/// signature checks (boot sector, ext superblock, HFS+ header, swap magic).
pub(crate) const HEAD_LEN: usize = 4096;

/// What [`probe_filesystem`] learned about the volume at an offset.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FsProbe {
    /// Filesystem family. [`FsKind::Other`] means "recognised but not parsed";
    /// the first entry of [`FsProbe::notes`] then names it (`"btrfs"`, `"xfs"`,
    /// `"hfs+"`, `"apfs"`, `"iso9660"`, `"luks"`, `"swap"`, `"squashfs"`).
    pub kind: FsKind,
    /// Volume size in bytes as the filesystem itself reports it (for NTFS this
    /// includes the trailing backup boot sector). `None` when the structure
    /// carries no usable size.
    pub size: Option<u64>,
    /// Volume label when the boot sector or superblock stores one.
    pub label: Option<String>,
    /// Allocation unit (cluster / block) in bytes.
    pub cluster_size: Option<u32>,
    /// True when the primary boot sector / superblock was damaged and a backup
    /// copy was used instead.
    pub via_backup: bool,
    /// Extra findings: serial numbers, features, consistency warnings.
    pub notes: Vec<String>,
}

impl FsProbe {
    pub(crate) fn new(kind: FsKind) -> Self {
        FsProbe {
            kind,
            size: None,
            label: None,
            cluster_size: None,
            via_backup: false,
            notes: Vec::new(),
        }
    }

    /// Display name: the family name for parsed filesystems, the recognised
    /// name (`"btrfs"`, ...) for [`FsKind::Other`].
    pub fn display_name(&self) -> String {
        match self.kind {
            FsKind::Other => self
                .notes
                .first()
                .cloned()
                .unwrap_or_else(|| "other".to_string()),
            k => k.name().to_string(),
        }
    }
}

/// Read context for one probe: the source, where the volume starts and how
/// many bytes belong to it.
pub(crate) struct Ctx<'a> {
    pub src: &'a dyn Source,
    pub off: u64,
    /// Bytes available to the volume (partition length or rest of the source).
    pub avail: u64,
    /// First [`HEAD_LEN`] bytes of the volume (possibly shorter or empty).
    pub head: &'a [u8],
}

impl Ctx<'_> {
    /// Read `len` bytes at volume-relative offset `rel`. `None` when the range
    /// starts outside the volume or cannot be read.
    pub fn read(&self, rel: u64, len: usize) -> Option<Vec<u8>> {
        if rel >= self.avail {
            return None;
        }
        let l = (self.avail - rel).min(len as u64) as usize;
        read_opt(self.src, self.off.checked_add(rel)?, l)
    }
}

/// Decode a space/NUL padded label; empty and the stock "NO NAME" give `None`.
pub(crate) fn label_from(bytes: &[u8]) -> Option<String> {
    let s = diskrescue_core::util::bytes::ascii_trimmed(bytes);
    if s.is_empty() || s.eq_ignore_ascii_case("NO NAME") {
        None
    } else {
        Some(s)
    }
}

/// Standard (big-endian) text form of a 16-byte UUID.
pub(crate) fn uuid_text(b: &[u8]) -> String {
    use diskrescue_core::util::bytes::hex;
    if b.len() < 16 {
        return String::new();
    }
    format!(
        "{}-{}-{}-{}-{}",
        hex(&b[0..4]),
        hex(&b[4..6]),
        hex(&b[6..8]),
        hex(&b[8..10]),
        hex(&b[10..16])
    )
}

/// Probe for a filesystem starting at byte `offset` of `src`.
///
/// The volume is assumed to extend to the end of the source; use a
/// [`diskrescue_core::source::SubSource`] over the partition so that
/// end-of-volume structures (the NTFS backup boot sector) are looked for in the
/// right place. Returns `None` when nothing recognisable starts there.
pub fn probe_filesystem(src: &SharedSource, offset: u64) -> Option<FsProbe> {
    probe_at(src.as_ref(), offset, None)
}

/// [`probe_filesystem`] with an optional volume length in bytes (partition
/// size), used to locate end-of-volume backups.
pub(crate) fn probe_at(src: &dyn Source, offset: u64, len: Option<u64>) -> Option<FsProbe> {
    let size = src.size();
    if offset >= size {
        return None;
    }
    let avail = len.map_or(size - offset, |l| l.min(size - offset));
    if avail == 0 {
        return None;
    }
    let head = read_opt(src, offset, HEAD_LEN.min(avail as usize)).unwrap_or_default();
    let ctx = Ctx {
        src,
        off: offset,
        avail,
        head: &head,
    };

    let primary = exfat::probe_primary(&ctx)
        .or_else(|| ntfs::probe_primary(&ctx))
        .or_else(|| fat::probe_primary(&ctx))
        .or_else(|| ext::probe_primary(&ctx))
        .or_else(|| other::probe(&ctx));
    if primary.is_some() {
        return primary;
    }
    exfat::probe_backup(&ctx)
        .or_else(|| fat::probe_backup(&ctx))
        .or_else(|| ntfs::probe_backup(&ctx))
        .or_else(|| ext::probe_backup(&ctx))
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskrescue_core::source::MemSource;
    use std::sync::Arc;

    #[test]
    fn empty_and_tiny_sources_probe_to_none() {
        let s: SharedSource = Arc::new(MemSource::new(Vec::new()));
        assert!(probe_filesystem(&s, 0).is_none());
        let s: SharedSource = Arc::new(MemSource::new(vec![0u8; 100]));
        assert!(probe_filesystem(&s, 0).is_none());
        assert!(probe_filesystem(&s, 5000).is_none());
        let s: SharedSource = Arc::new(MemSource::new(vec![0xFFu8; 70_000]));
        assert!(probe_filesystem(&s, 0).is_none());
        assert!(probe_filesystem(&s, u64::MAX).is_none());
    }

    #[test]
    fn label_filtering() {
        assert_eq!(label_from(b"NO NAME    "), None);
        assert_eq!(label_from(b"          "), None);
        assert_eq!(label_from(b"PHOTOS     ").as_deref(), Some("PHOTOS"));
    }
}
