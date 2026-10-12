//! exFAT.
//!
//! Opening: the main boot region (sectors 0-11) is validated and its
//! checksum sector verified; when either fails and the options allow it,
//! the backup region (sectors 12-23) is tried for every legal sector size.
//! A structurally valid region with a bad checksum is used only when no
//! better copy exists, with a warning. Cluster counts that exceed what the
//! volume or the FAT can hold are clipped with a warning.
//!
//! The root directory (always FAT-chained) supplies the Allocation Bitmap
//! (0x81), the Up-case Table (0x82, used to verify NameHash) and the Volume
//! Label (0x83). The bitmap, not the FAT, decides allocation: NoFatChain
//! files leave their FAT entries at zero.
//!
//! Listing: entry sets are parsed per directory. A set must be complete
//! (stream extension plus enough name entries); a live set whose checksum
//! fails is listed with a note, a deleted one only when its NameHash still
//! verifies. Live files: NoFatChain = contiguous clusters, else
//! the FAT chain. Deleted sets keep everything except bit 7 of each type
//! byte: NoFatChain files have exact extents (`DeletedIntact` while no
//! cluster has been reallocated), FAT-chained ones use the chain when it is
//! still intact in the FAT (Linux leaves it) and the contiguity guess
//! otherwise (`DeletedGuessed`). Bytes between ValidDataLength and
//! DataLength read as zeros (reported as holes); the entry's `flags` carry
//! `valid_data_length=<n>` when it is smaller than the size.
//!
//! Orphans: free clusters (and allocated clusters nothing claims, when the
//! walk was complete) whose first 4 KiB contain an entry set with a
//! verifying checksum are listed under `$orphans/cluster_<n>/`.
//!
//! Limits: TexFAT is read through the active FAT/bitmap only; Windows CE
//! access control entries and vendor extensions are ignored.

mod boot;
mod dir;
mod walk;

pub use boot::BootSector;

use std::io::Write;
use std::sync::Arc;

use diskrescue_core::fs::{FileEntry, FileReadReport, FileSystem, FsKind, VolumeInfo, WalkControl};
use diskrescue_core::util::bytes::{u32le, u64le, u8_at};
use diskrescue_core::{Error, Range, RangeSet, Result, SharedSource};

use crate::common::{self, ClusterMap, ClusterState, DirData};
use crate::io::MetaReader;
use crate::table::{FatEntry, FatTable, FatWidth, Runs};
use crate::util::{div_ceil, exfat_table_checksum};
use crate::FatOptions;

pub(crate) use boot::probe_sector;

/// Location of the allocation bitmap.
#[derive(Debug, Clone)]
struct Bitmap {
    /// Volume extents holding the bitmap bytes, in order.
    extents: Vec<Range>,
    /// Bitmap length in bytes.
    len: u64,
}

/// A parsed exFAT volume.
pub struct ExFatFs {
    src: SharedSource,
    meta: Arc<MetaReader>,
    boot: BootSector,
    geo: boot::Geometry,
    table: FatTable,
    bitmap: Option<Bitmap>,
    /// Clusters used by the bitmap and up-case table.
    system_runs: Vec<(u32, u32)>,
    upcase: Option<Vec<u16>>,
    info: VolumeInfo,
    opts: FatOptions,
}

impl std::fmt::Debug for ExFatFs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExFatFs")
            .field("source", &self.src.describe())
            .field("info", &self.info)
            .finish()
    }
}

/// Try the boot region at `off`; returns the boot sector, geometry and
/// whether the checksum sector matched.
fn try_region(
    meta: &MetaReader,
    off: u64,
    want_shift: Option<u8>,
) -> Option<(BootSector, boot::Geometry, bool)> {
    let (sec, bad) = meta.read_vec(off, 512);
    if !bad.is_empty() {
        return None;
    }
    let bs = BootSector::parse(&sec)?;
    if want_shift.is_some_and(|s| s != bs.bytes_per_sector_shift) {
        return None;
    }
    let geo = bs.geometry().ok()?;
    let bps = geo.bytes_per_sector as usize;
    let (region, bad) = meta.read_vec(off, 12 * bps);
    let ok = bad.is_empty() && boot::region_checksum_ok(&region, bps);
    Some((bs, geo, ok))
}

fn decompress_upcase(raw: &[u8]) -> Vec<u16> {
    let mut table: Vec<u16> = (0..=0xFFFFu32).map(|c| c as u16).collect();
    let units: Vec<u16> = raw
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let mut ch: u32 = 0;
    let mut i = 0usize;
    while i < units.len() && ch < 0x10000 {
        let v = units[i];
        if v == 0xFFFF && i + 1 < units.len() {
            ch += units[i + 1] as u32;
            i += 2;
            continue;
        }
        table[ch as usize] = v;
        ch += 1;
        i += 1;
    }
    table
}

/// Open an exFAT volume. See the module docs.
pub fn open_exfat(src: SharedSource, opts: &FatOptions) -> Result<ExFatFs> {
    let meta = Arc::new(MetaReader::new(src.clone()));
    let size = src.size();
    if size < 512 {
        return Err(Error::corrupt(
            "exFAT boot sector",
            format!("volume is only {size} bytes"),
        ));
    }
    let mut warnings = Vec::new();
    let main = try_region(&meta, 0, None);
    let mut chosen = None;
    if let Some((b, g, true)) = &main {
        chosen = Some((b.clone(), g.clone()));
    }
    if chosen.is_none() && opts.use_backup_boot_sector {
        let mut backup = None;
        for shift in 9u8..=12 {
            if let Some(r) = try_region(&meta, 12u64 << shift, Some(shift)) {
                backup = Some(r);
                break;
            }
        }
        let main_reason = match &main {
            None => "main boot sector unreadable or invalid",
            Some(_) => "main boot region checksum mismatch",
        };
        match (backup, &main) {
            (Some((b, g, true)), _) => {
                warnings.push(format!(
                    "{main_reason}; using the backup boot region at sector 12"
                ));
                chosen = Some((b, g));
            }
            (_, Some((b, g, false))) => {
                warnings.push(
                    "boot region checksum mismatch and no better backup; using the main boot sector"
                        .to_string(),
                );
                chosen = Some((b.clone(), g.clone()));
            }
            (Some((b, g, false)), None) => {
                warnings.push(
                    "main boot sector invalid; using the backup boot sector although its checksum does not match"
                        .to_string(),
                );
                chosen = Some((b, g));
            }
            _ => {}
        }
    } else if chosen.is_none() {
        if let Some((b, g, false)) = &main {
            warnings.push("boot region checksum mismatch".to_string());
            chosen = Some((b.clone(), g.clone()));
        }
    }
    let Some((boot, geo)) = chosen else {
        let detail = match BootSector::parse(&meta.read_vec(0, 512).0) {
            Some(b) => match b.geometry() {
                Err(r) => r,
                Ok(_) => "boot region unreadable".to_string(),
            },
            None => "boot sector unreadable".to_string(),
        };
        return Err(Error::corrupt("exFAT boot sector", detail));
    };
    warnings.extend(geo.warnings.iter().cloned());
    if size < geo.total_size {
        warnings.push(format!(
            "image is {} bytes shorter than the volume; data near the end is missing",
            geo.total_size - size
        ));
    }
    let table = FatTable::new(
        meta.clone(),
        FatWidth::ExFat,
        geo.fats.clone(),
        geo.fat_bytes,
        geo.max_cluster,
    );
    let mut fs = ExFatFs {
        src,
        meta,
        boot,
        geo,
        table,
        bitmap: None,
        system_runs: Vec::new(),
        upcase: None,
        info: VolumeInfo {
            kind: FsKind::ExFat,
            label: None,
            cluster_size: 0,
            total_size: 0,
            data_start: 0,
            serial: None,
            warnings: Vec::new(),
        },
        opts: opts.clone(),
    };
    let label = fs.load_root_metadata(&mut warnings);
    fs.info = VolumeInfo {
        kind: FsKind::ExFat,
        label,
        cluster_size: fs.geo.cluster_size.min(u32::MAX as u64) as u32,
        total_size: fs.geo.total_size,
        data_start: fs.geo.heap,
        serial: Some(fs.boot.serial()),
        warnings,
    };
    for w in &fs.info.warnings {
        log::warn!("{}: {w}", fs.src.describe());
    }
    Ok(fs)
}

impl ExFatFs {
    /// Read the root directory's critical primaries (bitmap, up-case
    /// table, label). Returns the label.
    fn load_root_metadata(&mut self, warnings: &mut Vec<String>) -> Option<String> {
        let root = self.geo.root;
        if !self.valid(root) {
            warnings.push("allocation bitmap unavailable (root directory missing)".to_string());
            return None;
        }
        let ch = self.table.follow(
            root,
            (common::MAX_DIR_BYTES / self.geo.cluster_size).max(1),
            false,
        );
        let ext = common::runs_to_extents(self, &ch.runs, None);
        let mut bytes = Vec::new();
        for r in &ext {
            bytes.extend_from_slice(&self.meta.read_vec(r.start, r.len() as usize).0);
        }
        let mut label = None;
        let mut bitmap_entry = None;
        let mut upcase_entry = None;
        let active = if self.boot.number_of_fats == 2 {
            (self.boot.volume_flags & 1) as u8
        } else {
            0
        };
        for e in bytes.chunks_exact(32) {
            match e[0] {
                0x00 => break,
                0x81 => {
                    if e[1] & 1 == active && bitmap_entry.is_none() {
                        bitmap_entry = Some((u32le(e, 20)?, u64le(e, 24)?));
                    }
                }
                0x82 if upcase_entry.is_none() => {
                    upcase_entry = Some((u32le(e, 4)?, u32le(e, 20)?, u64le(e, 24)?));
                }
                0x83 if label.is_none() => {
                    let n = (u8_at(e, 1)? as usize).min(11);
                    let units: Vec<u16> = (0..n)
                        .map(|k| u16::from_le_bytes([e[2 + 2 * k], e[3 + 2 * k]]))
                        .collect();
                    let l = String::from_utf16_lossy(&units);
                    if !l.is_empty() {
                        label = Some(l);
                    }
                }
                _ => {}
            }
        }
        // Allocation bitmap.
        match bitmap_entry {
            Some((first, len)) => {
                let need = div_ceil(self.geo.cluster_count as u64, 8);
                if len < need {
                    warnings.push(format!(
                        "allocation bitmap is {len} bytes, {need} needed; clusters beyond it are treated as unknown"
                    ));
                }
                let len = len.min(need.max(1));
                match self.system_extents(first, len, "allocation bitmap", warnings) {
                    Some((ext, runs)) => {
                        self.system_runs.extend(runs);
                        self.bitmap = Some(Bitmap { extents: ext, len });
                    }
                    None => warnings.push("allocation bitmap unusable".to_string()),
                }
            }
            None => warnings.push(
                "no allocation bitmap entry in the root directory; allocation state unknown"
                    .to_string(),
            ),
        }
        // Up-case table (only used to verify NameHash).
        if let Some((checksum, first, len)) = upcase_entry {
            if len == 0 || len > 128 * 1024 {
                warnings.push(format!(
                    "up-case table length {len} is implausible; ignored"
                ));
            } else if let Some((ext, runs)) =
                self.system_extents(first, len, "up-case table", warnings)
            {
                self.system_runs.extend(runs);
                let mut raw = Vec::with_capacity(len as usize);
                let mut bad_any = false;
                for r in &ext {
                    let (b, bad) = self.meta.read_vec(r.start, r.len() as usize);
                    bad_any |= !bad.is_empty();
                    raw.extend_from_slice(&b);
                }
                if bad_any {
                    warnings.push(
                        "up-case table unreadable; name hashes checked with ASCII rules"
                            .to_string(),
                    );
                } else {
                    if exfat_table_checksum(&raw) != checksum {
                        warnings.push("up-case table checksum mismatch".to_string());
                    }
                    self.upcase = Some(decompress_upcase(&raw));
                }
            }
        }
        label
    }

    /// Extents of a FAT-described system structure; falls back to
    /// contiguous clusters when the chain is broken.
    fn system_extents(
        &self,
        first: u32,
        len: u64,
        what: &str,
        warnings: &mut Vec<String>,
    ) -> Option<(Vec<Range>, Runs)> {
        if !self.valid(first) {
            warnings.push(format!("{what} starts at invalid cluster {first}"));
            return None;
        }
        let n = div_ceil(len, self.geo.cluster_size);
        let ch = self.table.follow(first, n, true);
        let runs = if ch.clusters == n && !ch.broken {
            ch.runs
        } else {
            warnings.push(format!(
                "{what} cluster chain is damaged; assuming it is contiguous"
            ));
            common::contiguous_runs(self, first, n).0
        };
        Some((common::runs_to_extents(self, &runs, Some(len)), runs))
    }

    /// Volume facts (same as [`FileSystem::info`]).
    pub fn volume_info(&self) -> &VolumeInfo {
        &self.info
    }

    /// The Main (or backup) Boot Sector in use.
    pub fn boot_sector(&self) -> &BootSector {
        &self.boot
    }

    /// Number of clusters (after clipping).
    pub fn cluster_count(&self) -> u32 {
        self.geo.cluster_count
    }

    /// Volume byte offset of cluster `cluster` (valid clusters start at 2).
    pub fn cluster_to_offset(&self, cluster: u32) -> u64 {
        self.offset_of(cluster)
    }

    /// Volume-relative extents of the FAT chain starting at
    /// `first_cluster` (cycle-protected, at most the cluster count). Note
    /// that NoFatChain files are not described by the FAT.
    pub fn chain(&self, first_cluster: u32) -> Vec<Range> {
        let ch = self
            .table
            .follow(first_cluster, self.geo.cluster_count as u64, false);
        common::runs_to_extents(self, &ch.runs, None)
    }

    /// Raw FAT entry of `cluster`.
    pub fn fat_entry(&self, cluster: u32) -> Option<u32> {
        self.table.raw(cluster)
    }

    /// Number of clusters whose bitmap bit is clear.
    pub fn free_clusters(&self) -> u64 {
        let mut n = 0u64;
        common::for_each_state(self, |_, s| {
            if s == ClusterState::Free {
                n += 1;
            }
            true
        });
        n
    }

    pub(crate) fn opts(&self) -> &FatOptions {
        &self.opts
    }

    pub(crate) fn meta(&self) -> &MetaReader {
        &self.meta
    }

    pub(crate) fn upcase(&self) -> Option<&[u16]> {
        self.upcase.as_deref()
    }

    pub(crate) fn root_cluster(&self) -> u32 {
        self.geo.root
    }

    pub(crate) fn system_runs(&self) -> &[(u32, u32)] {
        &self.system_runs
    }

    /// FAT entry of `c` decoded.
    pub(crate) fn fat_state(&self, c: u32) -> Option<FatEntry> {
        self.table.entry(c)
    }

    pub(crate) fn follow(&self, first: u32, limit: u64, size_limited: bool) -> crate::table::Chain {
        self.table.follow(first, limit, size_limited)
    }

    /// Read clusters `runs` as directory data.
    pub(crate) fn read_dir_runs(&self, runs: &[(u32, u32)]) -> DirData {
        let cs = self.geo.cluster_size;
        let mut data = DirData {
            bytes: Vec::new(),
            segments: Vec::new(),
            bad: RangeSet::new(),
        };
        for &(s, n) in runs {
            let off = self.offset_of(s);
            let len = n as u64 * cs;
            let base = data.bytes.len() as u64;
            let (bytes, bad) = self.meta.read_vec(off, len as usize);
            for r in bad.iter() {
                data.bad
                    .insert(Range::new(base + (r.start - off), base + (r.end - off)));
            }
            data.bytes.extend_from_slice(&bytes);
            data.segments.push((base, off, len));
        }
        data
    }

    /// Read bitmap bytes `[byte, byte+len)` with per-byte readability.
    fn bitmap_bytes(&self, byte: u64, len: usize) -> (Vec<u8>, Vec<bool>) {
        let mut out = vec![0u8; len];
        let mut ok = vec![false; len];
        let Some(bm) = &self.bitmap else {
            return (out, ok);
        };
        let mut ext_base = 0u64;
        for r in &bm.extents {
            let el = r.len();
            let a = byte.max(ext_base);
            let b = (byte + len as u64).min(ext_base + el).min(bm.len);
            if a < b {
                let (bytes, bad) = self
                    .meta
                    .read_vec(r.start + (a - ext_base), (b - a) as usize);
                let o = (a - byte) as usize;
                out[o..o + bytes.len()].copy_from_slice(&bytes);
                let vol0 = r.start + (a - ext_base);
                for k in 0..bytes.len() {
                    ok[o + k] = !bad.contains(vol0 + k as u64);
                }
            }
            ext_base += el;
            if ext_base >= byte + len as u64 {
                break;
            }
        }
        (out, ok)
    }
}

impl ClusterMap for ExFatFs {
    fn cluster_bytes(&self) -> u64 {
        self.geo.cluster_size
    }

    fn heap_offset(&self) -> u64 {
        self.geo.heap
    }

    fn max_cluster(&self) -> u32 {
        self.geo.max_cluster
    }

    fn meta_limit(&self) -> u32 {
        let Some(bm) = &self.bitmap else {
            return 1;
        };
        let src = self.meta.size();
        let mut bytes = 0u64;
        for r in &bm.extents {
            if r.start >= src {
                break;
            }
            let inside = r.end.min(src) - r.start;
            bytes += inside;
            if inside < r.len() {
                break;
            }
        }
        let bytes = bytes.min(bm.len);
        (bytes * 8 + 1).min(u32::MAX as u64) as u32
    }

    fn source_len(&self) -> u64 {
        self.meta.size()
    }

    fn states(&self, first: u32, count: u32) -> Vec<ClusterState> {
        let mut out = vec![ClusterState::Unknown; count as usize];
        if count == 0 || first < 2 {
            return out;
        }
        let i0 = (first - 2) as u64;
        let byte0 = i0 / 8;
        let byte1 = (i0 + count as u64 - 1) / 8;
        let (bytes, ok) = self.bitmap_bytes(byte0, (byte1 - byte0 + 1) as usize);
        for k in 0..count as u64 {
            let c = first as u64 + k;
            if c > self.geo.max_cluster as u64 {
                break;
            }
            let i = i0 + k;
            let bi = (i / 8 - byte0) as usize;
            if ok[bi] {
                out[k as usize] = if bytes[bi] >> (i % 8) & 1 == 1 {
                    ClusterState::Allocated
                } else {
                    ClusterState::Free
                };
            }
        }
        out
    }
}

impl FileSystem for ExFatFs {
    fn info(&self) -> &VolumeInfo {
        &self.info
    }

    fn walk(&self, visit: &mut dyn FnMut(&FileEntry) -> WalkControl) -> Result<()> {
        walk::run(self, visit)
    }

    fn read_file(&self, entry: &FileEntry, sink: &mut dyn Write) -> Result<FileReadReport> {
        common::read_entry(&self.src, self, entry, sink)
    }

    fn unallocated(&self) -> Result<RangeSet> {
        Ok(common::unallocated_ranges(self, self.src.size()))
    }

    fn source(&self) -> &SharedSource {
        &self.src
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upcase_decompression() {
        // Identity run of 97 ('a' starts at 97), then a..c -> A..C.
        let mut raw = Vec::new();
        for u in [0xFFFFu16, 97, 65, 66, 67] {
            raw.extend_from_slice(&u.to_le_bytes());
        }
        let t = decompress_upcase(&raw);
        assert_eq!(t[97], 65);
        assert_eq!(t[99], 67);
        assert_eq!(t[5], 5);
        assert_eq!(t[100], 100);
    }
}
