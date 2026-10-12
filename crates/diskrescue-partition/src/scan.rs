//! Lost-partition scan.
//!
//! When the partition table is gone (zeroed first track, reformatted card,
//! overwritten MBR) the filesystems usually survive. [`scan_lost_partitions`]
//! walks the device sequentially in 8 MiB chunks and looks, at every
//! `step`-aligned offset, for cheap signatures:
//!
//! * a `55 AA` boot sector with `NTFS    `, `EXFAT   ` or a plausible FAT BPB,
//! * the ext2/3/4 magic `0xEF53` at +0x438 (primary superblock) or +0x38
//!   (a backup superblock sitting at the start of a block group),
//! * XFS, APFS, HFS+ and btrfs magics (reported when the size fits the device).
//!
//! A hit is only a candidate. Redundant copies are resolved to the volume they
//! belong to: a FAT32 backup boot sector (sector 6), an exFAT backup boot
//! region (sector 12), an NTFS backup boot sector (last sector of the volume)
//! and an ext backup superblock (block group `g`) each point back at the volume
//! start. For the boot sector families the "is this the primary or a copy?"
//! question is answered with independent evidence (an identical copy at the
//! backup position, the FAT or MFT where the primary would put them, the FSInfo
//! sector) and, failing that, partitioner alignment. The candidate start is
//! then confirmed with [`probe_filesystem`](crate::probe_filesystem), the
//! entry size is taken from the filesystem's own size field, and scanning
//! resumes after the volume, so every volume is reported once with origin
//! `"scan"`.
//!
//! # Limits
//!
//! * Only offsets that are multiples of `step` are examined (plus, when
//!   `step` is larger than the BPB-copy distance, the positions of the FAT32
//!   and exFAT backup boot sectors relative to each aligned offset). The
//!   default step of 4096 finds every modern (1 MiB aligned) partition; use
//!   512 to also find legacy sector-63 partitions and NTFS backup boot sectors.
//! * Unreadable sectors are skipped, never fatal; after 16 consecutive bad
//!   sectors 1 MiB is skipped at once.
//! * Cancellation is checked once per chunk; the entries found so far are
//!   returned.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use diskrescue_core::fs::FsKind;
use diskrescue_core::partition::PartitionEntry;
use diskrescue_core::util::bytes::u16le;
use diskrescue_core::{Range, RangeSet, Result, SharedSource, Source};

use crate::io::{align_up, alignment_bits, is_fatal, read_opt};
use crate::mbr::type_id;
use crate::probe::exfat::{self, ExfatBoot};
use crate::probe::ext::{self, ExtSb};
use crate::probe::fat::{self, FatBpb};
use crate::probe::ntfs::{self, NtfsBoot};
use crate::probe::{probe_at, FsProbe};

/// Sequential read size.
const CHUNK: usize = 8 << 20;
/// Extra bytes read past a chunk so signatures at its end can be examined
/// (btrfs needs 0x10048 bytes beyond the candidate offset).
const OVERLAP: usize = 128 << 10;

/// Parameters of a lost-partition scan.
#[derive(Clone)]
pub struct ScanOptions {
    /// Distance between examined offsets in bytes: 4096 by default, 512 for a
    /// thorough scan. Rounded up to a multiple of 512.
    pub step: u64,
    /// First byte to examine.
    pub start: u64,
    /// One past the last byte to examine (`None` = end of the source).
    pub end: Option<u64>,
    /// Set to `true` from another thread to stop the scan; the entries found
    /// so far are returned.
    pub cancel: Option<Arc<AtomicBool>>,
    /// Stop after this many volumes (`0` = unlimited).
    pub max_results: usize,
}

impl Default for ScanOptions {
    fn default() -> Self {
        ScanOptions {
            step: 4096,
            start: 0,
            end: None,
            cancel: None,
            max_results: 128,
        }
    }
}

impl std::fmt::Debug for ScanOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScanOptions")
            .field("step", &self.step)
            .field("start", &self.start)
            .field("end", &self.end)
            .field("cancel", &self.cancel.is_some())
            .field("max_results", &self.max_results)
            .finish()
    }
}

/// What a cheap signature hit looked like.
enum Hit {
    Fat(FatBpb),
    Exfat(ExfatBoot),
    Ntfs(NtfsBoot),
    /// Superblock and the byte offset (relative to the examined position) at
    /// which it was found.
    Ext(Box<ExtSb>, u64),
    /// XFS, APFS, HFS+, btrfs: confirmed by `probe_at` at the position.
    Other,
}

/// Run the scan. `progress(done, total)` is called after each chunk with the
/// number of bytes of the requested range processed so far.
///
/// Returns the volumes found, sorted by start, with origin `"scan"` and
/// `fs` set. Media errors on the source are skipped; only fatal errors (the
/// device disappeared) are returned as `Err`.
pub fn scan_lost_partitions(
    src: &SharedSource,
    opts: &ScanOptions,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<Vec<PartitionEntry>> {
    let src: &dyn Source = src.as_ref();
    let dev = src.size();
    let end = opts.end.map_or(dev, |e| e.min(dev));
    let start = opts.start.min(end);
    // Direct-I/O devices need sector aligned reads: keep every chunk start
    // aligned by making the step a multiple of the sector size.
    let unit = if src.requires_alignment() {
        u64::from(src.sector_size().max(512))
    } else {
        512
    };
    let step = opts.step.max(unit).div_ceil(unit) * unit;
    let total = end - start;
    let limit = if opts.max_results == 0 {
        usize::MAX
    } else {
        opts.max_results
    };
    let bad = if src.requires_alignment() {
        None
    } else {
        src.unreadable()
    };
    let mut sc = Scanner {
        src,
        dev,
        found: Vec::new(),
        covered_until: 0,
    };
    let cancelled = || {
        opts.cancel
            .as_ref()
            .is_some_and(|c| c.load(Ordering::Relaxed))
    };

    progress(0, total);
    let mut pos = align_up(start, step);
    'outer: while pos < end {
        if cancelled() {
            log::debug!("lost-partition scan cancelled at {pos}");
            break;
        }
        let eval_end = pos.saturating_add(CHUNK as u64).min(end);
        let read_end = eval_end.saturating_add(OVERLAP as u64).min(dev);
        let data = read_chunk(src, pos, (read_end - pos) as usize, bad.as_ref())?;
        let mut p = pos;
        while p < eval_end {
            if let Some(vol_end) = sc.eval(&data, pos, p, step) {
                p = align_up(vol_end, step).max(p + step);
                if sc.found.len() >= limit {
                    break 'outer;
                }
                continue;
            }
            p += step;
        }
        pos = p;
        progress(pos.min(end) - start.min(pos.min(end)), total);
    }
    if !cancelled() {
        progress(total, total);
    }
    let mut found = sc.found;
    found.sort_by_key(|e| e.start);
    for (i, e) in found.iter_mut().enumerate() {
        e.index = i as u32 + 1;
    }
    Ok(found)
}

/// Read `len` bytes at `off`, zero-filling unreadable parts.
fn read_chunk(src: &dyn Source, off: u64, len: usize, bad: Option<&RangeSet>) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; len];
    let ss = src.sector_size().max(512) as usize;
    let window = Range::new(off, off + len as u64);
    let ranges: Vec<Range> = match bad {
        Some(set) => set.complement_within(window).iter().collect(),
        None => vec![window],
    };
    for r in ranges {
        let a = (r.start - off) as usize;
        let b = (r.end - off) as usize;
        read_tolerant(src, r.start, &mut buf[a..b], ss)?;
    }
    Ok(buf)
}

/// Fill `buf` from `off`; failing sectors stay zero. Fatal errors propagate.
fn read_tolerant(src: &dyn Source, off: u64, buf: &mut [u8], ss: usize) -> Result<()> {
    let mut done = 0usize;
    let mut consecutive_bad = 0u32;
    while done < buf.len() {
        match src.read_at(off + done as u64, &mut buf[done..]) {
            Ok(0) => return Ok(()),
            Ok(n) => {
                done += n;
                consecutive_bad = 0;
            }
            Err(e) if is_fatal(&e) => return Err(e.into()),
            Err(e) => {
                log::debug!("scan: read error at {}: {}", e.offset, e.detail);
                // Sector-by-sector for a while, then back to bulk reads.
                let mut budget = 128;
                while done < buf.len() && budget > 0 {
                    budget -= 1;
                    let n = ss.min(buf.len() - done);
                    match src.read_at(off + done as u64, &mut buf[done..done + n]) {
                        Ok(0) => return Ok(()),
                        Ok(m) => {
                            done += m;
                            consecutive_bad = 0;
                        }
                        Err(e) if is_fatal(&e) => return Err(e.into()),
                        Err(_) => {
                            buf[done..done + n].fill(0);
                            done += n;
                            consecutive_bad += 1;
                            if consecutive_bad >= 16 {
                                let skip = (1usize << 20).min(buf.len() - done);
                                buf[done..done + skip].fill(0);
                                done += skip;
                                consecutive_bad = 0;
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

struct Scanner<'a> {
    src: &'a dyn Source,
    dev: u64,
    found: Vec<PartitionEntry>,
    covered_until: u64,
}

impl Scanner<'_> {
    fn rd(&self, off: u64, len: usize) -> Option<Vec<u8>> {
        read_opt(self.src, off, len)
    }

    /// Examine the aligned position `p` (and the BPB-copy positions relative
    /// to it). Returns the end of the volume when one was reported.
    fn eval(&mut self, data: &[u8], base: u64, p: u64, step: u64) -> Option<u64> {
        if let Some(end) = self.eval_at(data, base, p) {
            return Some(end);
        }
        for extra in [3072u64, 6144] {
            if extra % step != 0 {
                if let Some(end) = self.eval_at(data, base, p + extra) {
                    return Some(end);
                }
            }
        }
        None
    }

    fn eval_at(&mut self, data: &[u8], base: u64, q: u64) -> Option<u64> {
        let rel = usize::try_from(q.checked_sub(base)?).ok()?;
        let b = data.get(rel..)?;
        let hit = quick_hit(b)?;
        let (start, len_hint) = match &hit {
            Hit::Fat(f) => (self.resolve_fat(q, f), Some(f.size_bytes())),
            Hit::Exfat(e) => (self.resolve_exfat(q, e), Some(e.size_bytes())),
            Hit::Ntfs(n) => (self.resolve_ntfs(q, n), Some(n.size_bytes())),
            Hit::Ext(sb, off) => (ext_volume_start(q + off, sb)?, Some(sb.size_bytes())),
            Hit::Other => (q, None),
        };
        if start < self.covered_until || start >= self.dev {
            return None;
        }
        let probe = probe_at(self.src, start, len_hint)?;
        let compatible = match hit {
            Hit::Fat(_) => matches!(probe.kind, FsKind::Fat12 | FsKind::Fat16 | FsKind::Fat32),
            Hit::Exfat(_) => probe.kind == FsKind::ExFat,
            Hit::Ntfs(_) => probe.kind == FsKind::Ntfs,
            Hit::Ext(..) => matches!(probe.kind, FsKind::Ext2 | FsKind::Ext3 | FsKind::Ext4),
            Hit::Other => probe.kind == FsKind::Other,
        };
        if !compatible {
            return None;
        }
        let declared = probe.size.filter(|&s| s > 0)?;
        if matches!(hit, Hit::Other) && start.saturating_add(declared) > self.dev {
            // Nothing else vouches for these: require the volume to fit.
            return None;
        }
        let size = declared.min(self.dev - start);
        log::debug!(
            "scan: {} at {start} ({size} bytes){}",
            probe.display_name(),
            if probe.via_backup { " via backup" } else { "" }
        );
        self.found.push(entry_for(start, size, &probe));
        self.covered_until = start + size;
        Some(start + size)
    }

    fn same_sector(&self, a: u64, b: u64, len: usize) -> bool {
        match (self.rd(a, len), self.rd(b, len)) {
            (Some(x), Some(y)) => x.len() == len && x == y,
            _ => false,
        }
    }

    fn fat32_header_ok(&self, start: u64, f: &FatBpb) -> bool {
        let off = start + u64::from(f.rsvd) * u64::from(f.bps);
        self.rd(off, 4)
            .is_some_and(|b| fat::fat_header_ok(&b, f.media))
    }

    /// Is the hit at `q` the primary boot sector or the FAT32 backup (sector 6)?
    fn resolve_fat(&self, q: u64, f: &FatBpb) -> u64 {
        if !f.fat32_layout || f.bk_boot_sec == 0 {
            return q;
        }
        let bps = u64::from(f.bps);
        let k = u64::from(f.bk_boot_sec) * bps;
        let Some(s2) = q.checked_sub(k) else { return q };
        if s2 < self.covered_until {
            return q;
        }
        let a = 2 * u32::from(self.same_sector(q, q + k, 512))
            + 2 * u32::from(self.fat32_header_ok(q, f));
        let fsinfo_ok = self
            .rd(s2 + u64::from(f.fsinfo) * bps, 512)
            .is_some_and(|b| b.len() >= 488 && &b[0..4] == b"RRaA" && &b[484..488] == b"rrAa");
        let b = 2 * u32::from(self.fat32_header_ok(s2, f)) + u32::from(fsinfo_ok);
        if b > a {
            s2
        } else {
            q
        }
    }

    /// Primary exFAT boot sector or the backup region at sector 12?
    fn resolve_exfat(&self, q: u64, e: &ExfatBoot) -> u64 {
        let k = 12 * e.bps;
        let Some(s2) = q.checked_sub(k) else { return q };
        if s2 < self.covered_until {
            return q;
        }
        let fat_ok = |start: u64| {
            self.rd(start + exfat::fat_byte_offset(e), 8)
                .is_some_and(|b| exfat::fat_header_ok(&b))
        };
        let copy_after = match (self.rd(q, e.bps as usize), self.rd(q + k, e.bps as usize)) {
            (Some(x), Some(y)) => exfat::boot_sectors_equal(&x, &y),
            _ => false,
        };
        let a = 2 * u32::from(copy_after) + 2 * u32::from(fat_ok(q));
        let b = 2 * u32::from(fat_ok(s2));
        match b.cmp(&a) {
            std::cmp::Ordering::Greater => s2,
            std::cmp::Ordering::Less => q,
            std::cmp::Ordering::Equal => {
                if alignment_bits(s2) > alignment_bits(q) {
                    s2
                } else {
                    q
                }
            }
        }
    }

    /// Primary NTFS boot sector or the backup in the volume's last sector?
    fn resolve_ntfs(&self, q: u64, n: &NtfsBoot) -> u64 {
        let k = n.backup_offset();
        let Some(s2) = q.checked_sub(k) else { return q };
        if s2 < self.covered_until {
            return q;
        }
        let mft_ok = |start: u64| {
            n.mft_offset()
                .and_then(|o| self.rd(start.checked_add(o)?, 4))
                .is_some_and(|m| m == b"FILE" || m == b"BAAD")
        };
        let a = 2 * u32::from(self.same_sector(q, q + k, 512)) + 3 * u32::from(mft_ok(q));
        let b = 3 * u32::from(mft_ok(s2));
        if b > a {
            s2
        } else {
            q
        }
    }
}

/// Volume start implied by a superblock found at byte `sb_pos`.
fn ext_volume_start(sb_pos: u64, sb: &ExtSb) -> Option<u64> {
    if sb.group_nr == 0 {
        return sb_pos.checked_sub(1024);
    }
    let g = u64::from(sb.group_nr);
    if g >= sb.group_count() {
        return None;
    }
    let off = ext::group_start_offset(
        g,
        sb.block_size,
        u64::from(sb.blocks_per_group),
        u64::from(sb.first_data_block),
    )?;
    sb_pos.checked_sub(off)
}

/// Cheap signature tests at the start of `b`.
fn quick_hit(b: &[u8]) -> Option<Hit> {
    if b.len() >= 512 && b[510] == 0x55 && b[511] == 0xAA {
        if &b[3..11] == b"NTFS    " {
            if let Some(n) = ntfs::parse_boot(b) {
                return Some(Hit::Ntfs(n));
            }
        } else if &b[3..11] == b"EXFAT   " {
            if let Some(e) = exfat::parse_boot(b) {
                return Some(Hit::Exfat(e));
            }
        } else if b[0] == 0xEB || b[0] == 0xE9 {
            if let Some(f) = fat::parse_bpb(b) {
                return Some(Hit::Fat(f));
            }
        }
    }
    if u16le(b, 1024 + 0x38) == Some(0xEF53) {
        if let Some(sb) = b.get(1024..2048).and_then(ext::parse_sb) {
            return Some(Hit::Ext(Box::new(sb), 1024));
        }
    }
    if u16le(b, 0x38) == Some(0xEF53) {
        if let Some(sb) = b.get(0..1024).and_then(ext::parse_sb) {
            return Some(Hit::Ext(Box::new(sb), 0));
        }
    }
    if b.get(0..4) == Some(b"XFSB")
        || b.get(32..36) == Some(b"NXSB")
        || matches!(b.get(1024..1026), Some(b"H+") | Some(b"HX"))
        || b.get(0x10040..0x10048) == Some(b"_BHRfS_M")
    {
        return Some(Hit::Other);
    }
    None
}

/// Suggested MBR type byte for a filesystem found without a table.
fn suggested_type(p: &FsProbe) -> u8 {
    match p.kind {
        FsKind::Fat12 => 0x01,
        FsKind::Fat16 => 0x0E,
        FsKind::Fat32 => 0x0C,
        FsKind::ExFat | FsKind::Ntfs => 0x07,
        FsKind::Ext2 | FsKind::Ext3 | FsKind::Ext4 => 0x83,
        FsKind::Other => match p.notes.first().map(String::as_str) {
            Some("hfs+") | Some("hfsx") | Some("hfs") | Some("apfs") => 0xAF,
            Some("xfs") | Some("btrfs") => 0x83,
            _ => 0xDA,
        },
    }
}

fn entry_for(start: u64, size: u64, probe: &FsProbe) -> PartitionEntry {
    let t = suggested_type(probe);
    let mut desc = format!("{} (found by scan", probe.display_name());
    if probe.via_backup {
        desc.push_str(", from backup structures");
    }
    desc.push(')');
    PartitionEntry {
        index: 0,
        start,
        size,
        type_id: type_id(t),
        type_desc: desc,
        name: probe.label.clone(),
        guid: None,
        bootable: false,
        origin: "scan".to_string(),
        fs: Some(probe.kind),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::exfat::tests::boot_sector as exfat_sector;
    use crate::probe::ext::tests::superblock;
    use crate::probe::fat::tests::fat_sector;
    use crate::probe::ntfs::tests::boot_sector as ntfs_sector;
    use diskrescue_core::source::MemSource;

    #[test]
    fn tolerant_reads_zero_fill_holes() {
        let data: Vec<u8> = (0..65536u32).map(|i| (i % 251) as u8 | 1).collect();
        let mut holes = RangeSet::new();
        holes.insert(Range::new(10_000, 12_000));
        let src = MemSource::new(data.clone()).with_unreadable(holes);
        let mut buf = vec![0xAAu8; data.len()];
        read_tolerant(&src, 0, &mut buf, 512).unwrap();
        assert_eq!(&buf[..9_728], &data[..9_728]);
        assert!(buf[10_240..11_776].iter().all(|&b| b == 0));
        assert_eq!(&buf[12_288..], &data[12_288..]);
    }

    #[test]
    fn long_dead_zones_are_skipped_quickly() {
        let n = 8 << 20;
        let data = vec![7u8; n];
        let mut holes = RangeSet::new();
        holes.insert(Range::new(1 << 20, 4 << 20));
        let src = MemSource::new(data).with_unreadable(holes);
        let mut buf = vec![0u8; n];
        read_tolerant(&src, 0, &mut buf, 512).unwrap();
        assert!(buf[(1 << 20) + 4096..4 << 20].iter().all(|&b| b == 0));
        assert!(buf[5 << 20..].iter().all(|&b| b == 7));
        assert!(buf[..1 << 20].iter().all(|&b| b == 7));
    }

    #[test]
    fn chunk_reader_honours_known_unreadable_ranges() {
        let mut holes = RangeSet::new();
        holes.insert(Range::new(4096, 8192));
        let src = MemSource::new(vec![9u8; 16384]);
        let chunk = read_chunk(&src, 0, 16384, Some(&holes)).unwrap();
        assert!(chunk[..4096].iter().all(|&b| b == 9));
        assert!(chunk[4096..8192].iter().all(|&b| b == 0));
        assert!(chunk[8192..].iter().all(|&b| b == 9));
    }

    #[test]
    fn quick_hits_recognise_each_family() {
        assert!(matches!(
            quick_hit(&fat_sector(512, 1, 32, 2, 0, 0, 131072, 0, 1009)),
            Some(Hit::Fat(_))
        ));
        assert!(matches!(quick_hit(&exfat_sector()), Some(Hit::Exfat(_))));
        assert!(matches!(quick_hit(&ntfs_sector(32767)), Some(Hit::Ntfs(_))));
        let mut e = vec![0u8; 4096];
        e[1024..2048].copy_from_slice(&superblock(2, 262144, 0));
        assert!(matches!(quick_hit(&e), Some(Hit::Ext(_, 1024))));
        let mut e = vec![0u8; 4096];
        e[..1024].copy_from_slice(&superblock(2, 262144, 3));
        assert!(matches!(quick_hit(&e), Some(Hit::Ext(_, 0))));
        let mut x = vec![0u8; 4096];
        x[..4].copy_from_slice(b"XFSB");
        assert!(matches!(quick_hit(&x), Some(Hit::Other)));
        assert!(quick_hit(&vec![0u8; 4096]).is_none());
        assert!(quick_hit(&[]).is_none());
        assert!(quick_hit(&[0x55; 100]).is_none());
    }

    #[test]
    fn ext_start_from_backup_position() {
        // 4 KiB blocks, 32768 blocks per group: group 3's copy sits 3 groups in.
        let sb = ext::parse_sb(&superblock(2, 262144, 3)).unwrap();
        let group = 32768u64 * 4096;
        assert_eq!(
            ext_volume_start(10 * group + 3 * group, &sb),
            Some(10 * group)
        );
        // Primary: the start is 1 KiB before the superblock.
        let sb = ext::parse_sb(&superblock(2, 262144, 0)).unwrap();
        assert_eq!(ext_volume_start(1 << 20 | 1024, &sb), Some(1 << 20));
        assert_eq!(ext_volume_start(100, &sb), None);
        // 1 KiB blocks: group 1 is block 8193.
        let sb = ext::parse_sb(&superblock(0, 65536, 1)).unwrap();
        assert_eq!(
            ext_volume_start(8193 * 1024 + 5_000_000, &sb),
            Some(5_000_000)
        );
        // A group number beyond the group count is rejected.
        let sb = ext::parse_sb(&superblock(2, 32768, 3)).unwrap();
        assert_eq!(ext_volume_start(1 << 40, &sb), None);
    }

    #[test]
    fn default_options() {
        let o = ScanOptions::default();
        assert_eq!((o.step, o.start, o.end), (4096, 0, None));
        assert!(o.cancel.is_none() && o.max_results > 0);
        assert!(format!("{o:?}").contains("step"));
    }
}
