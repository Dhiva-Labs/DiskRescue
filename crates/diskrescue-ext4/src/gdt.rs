//! Block group descriptors.
//!
//! The descriptor table (GDT) follows the superblock: block
//! `first_data_block + 1` onwards in the classic layout. With `meta_bg`,
//! descriptor block `i >= s_first_meta_bg` lives in the first group of meta
//! group `i` (after that group's superblock backup, if any), with copies in
//! the second and last group of the meta group.
//!
//! Loading is per descriptor block: the primary copy is read first; when it
//! is unreadable or a descriptor in it is structurally invalid (bitmaps or
//! inode table outside the volume), the backups (classic layout: the GDT copy
//! after each backup superblock; `meta_bg`: the two in-meta-group copies) are
//! consulted and the first structurally valid version of each descriptor is
//! used. Descriptors taken from a backup are marked `from_backup`: their
//! locations are reliable (they never change outside resize) but their
//! UNINIT flags and free counts are stale, so callers must not trust them.

use diskrescue_core::util::bytes::{u16le, u32le};

use crate::csum::{crc16, crc32c_raw};
use crate::superblock::{features::*, sparse_backup_groups, Superblock};

/// `bg_flags` bits.
pub mod bg_flags {
    /// Inode table and bitmap not initialised; no inodes allocated here.
    pub const INODE_UNINIT: u16 = 0x1;
    /// Block bitmap not initialised; compute it from the layout.
    pub const BLOCK_UNINIT: u16 = 0x2;
    /// Inode table has been zeroed.
    pub const INODE_ZEROED: u16 = 0x4;
}

/// One decoded group descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupDesc {
    /// Group number.
    pub group: u32,
    /// Block number of the block bitmap.
    pub block_bitmap: u64,
    /// Block number of the inode bitmap.
    pub inode_bitmap: u64,
    /// First block of the inode table.
    pub inode_table: u64,
    /// Free blocks in the group.
    pub free_blocks_count: u32,
    /// Free inodes in the group.
    pub free_inodes_count: u32,
    /// Directories in the group.
    pub used_dirs_count: u32,
    /// `bg_flags` (see [`bg_flags`]).
    pub flags: u16,
    /// Inodes at the end of the table that were never used.
    pub itable_unused: u32,
    /// Stored checksum.
    pub checksum: u16,
    /// Whether the stored checksum matches (None without checksums).
    pub checksum_ok: Option<bool>,
    /// Taken from a backup copy of the GDT.
    pub from_backup: bool,
    /// Structurally valid (all locations inside the volume). Invalid
    /// descriptors are placeholders; their group cannot be read.
    pub valid: bool,
}

impl GroupDesc {
    /// Decode the descriptor of `group` from `raw` (at least `desc_size`
    /// bytes; shorter input yields an invalid placeholder).
    pub fn parse(group: u32, raw: &[u8], sb: &Superblock) -> GroupDesc {
        let ds = sb.desc_size as usize;
        if raw.len() < ds.max(32) {
            return GroupDesc::placeholder(group);
        }
        let wide = ds >= 64;
        let lo32 = |o: usize| u32le(raw, o).unwrap_or(0) as u64;
        let lo16 = |o: usize| u16le(raw, o).unwrap_or(0) as u32;
        let hi32 = |o: usize| if wide { lo32(o) << 32 } else { 0 };
        let hi16 = |o: usize| if wide { lo16(o) << 16 } else { 0 };
        let checksum = u16le(raw, 0x1E).unwrap_or(0);
        let mut d = GroupDesc {
            group,
            block_bitmap: lo32(0x00) | hi32(0x20),
            inode_bitmap: lo32(0x04) | hi32(0x24),
            inode_table: lo32(0x08) | hi32(0x28),
            free_blocks_count: lo16(0x0C) | hi16(0x2C),
            free_inodes_count: lo16(0x0E) | hi16(0x2E),
            used_dirs_count: lo16(0x10) | hi16(0x30),
            flags: u16le(raw, 0x12).unwrap_or(0),
            itable_unused: lo16(0x1C) | hi16(0x32),
            checksum,
            checksum_ok: desc_checksum(sb, group, &raw[..ds]).map(|c| c == checksum),
            from_backup: false,
            valid: false,
        };
        d.valid = d.structurally_valid(sb);
        d
    }

    /// An invalid descriptor standing in for one that could not be read.
    pub fn placeholder(group: u32) -> GroupDesc {
        GroupDesc {
            group,
            block_bitmap: 0,
            inode_bitmap: 0,
            inode_table: 0,
            free_blocks_count: 0,
            free_inodes_count: 0,
            used_dirs_count: 0,
            flags: 0,
            itable_unused: 0,
            checksum: 0,
            checksum_ok: None,
            from_backup: false,
            valid: false,
        }
    }

    fn structurally_valid(&self, sb: &Superblock) -> bool {
        let lo = (sb.first_data_block as u64).max(1);
        let bc = sb.blocks_count;
        let inside = |b: u64| b >= lo && b < bc;
        inside(self.block_bitmap)
            && inside(self.inode_bitmap)
            && self.inode_table >= lo
            && self
                .inode_table
                .checked_add(sb.inode_table_blocks())
                .is_some_and(|end| end <= bc)
            && self.free_inodes_count <= sb.inodes_per_group
            && self.free_blocks_count <= sb.blocks_per_group
    }

    /// `INODE_UNINIT` is set.
    pub fn inode_uninit(&self) -> bool {
        self.flags & bg_flags::INODE_UNINIT != 0
    }

    /// `BLOCK_UNINIT` is set.
    pub fn block_uninit(&self) -> bool {
        self.flags & bg_flags::BLOCK_UNINIT != 0
    }
}

/// The checksum a descriptor should carry, or `None` when the filesystem
/// has neither `metadata_csum` nor `gdt_csum`. `raw` is the descriptor
/// (`desc_size` bytes) as stored.
pub fn desc_checksum(sb: &Superblock, group: u32, raw: &[u8]) -> Option<u16> {
    let ds = (sb.desc_size as usize).min(raw.len());
    if ds < 0x20 {
        return None;
    }
    let le_group = group.to_le_bytes();
    if sb.has_ro_compat(RO_COMPAT_METADATA_CSUM) {
        let mut c = crc32c_raw(sb.csum_seed(), &le_group);
        c = crc32c_raw(c, &raw[..0x1E]);
        c = crc32c_raw(c, &[0, 0]);
        c = crc32c_raw(c, &raw[0x20..ds]);
        return Some((c & 0xFFFF) as u16);
    }
    if sb.has_ro_compat(RO_COMPAT_GDT_CSUM) {
        let mut c = crc16(0xFFFF, &sb.uuid);
        c = crc16(c, &le_group);
        c = crc16(c, &raw[..0x1E]);
        if sb.has_incompat(INCOMPAT_64BIT) {
            c = crc16(c, &raw[0x20..ds]);
        }
        return Some(c);
    }
    None
}

/// Primary location (block number) of descriptor block `i`.
pub(crate) fn primary_gdt_block(sb: &Superblock, i: u64) -> u64 {
    if !sb.has_incompat(INCOMPAT_META_BG) || i < sb.first_meta_bg as u64 {
        return sb.first_data_block as u64 + 1 + i;
    }
    let bg = i
        .saturating_mul(sb.descs_per_block() as u64)
        .min(u32::MAX as u64) as u32;
    sb.group_first_block(bg) + sb.group_has_super(bg) as u64
}

/// Backup locations of descriptor block `i`, most useful first.
pub(crate) fn backup_gdt_blocks(sb: &Superblock, i: u64, max: usize) -> Vec<u64> {
    let groups = sb.group_count() as u64;
    let mut out = Vec::new();
    if !sb.has_incompat(INCOMPAT_META_BG) || i < sb.first_meta_bg as u64 {
        let candidates: Vec<u32> = if sb.has_compat(COMPAT_SPARSE_SUPER2) {
            sb.backup_bgs.iter().copied().filter(|&g| g != 0).collect()
        } else if sb.has_ro_compat(RO_COMPAT_SPARSE_SUPER) {
            sparse_backup_groups(groups)
        } else {
            (1..groups.min(64) as u32).collect()
        };
        for g in candidates {
            if (g as u64) < groups && sb.group_has_super(g) {
                out.push(sb.group_first_block(g) + 1 + i);
            }
            if out.len() >= max {
                break;
            }
        }
    } else {
        let dpb = sb.descs_per_block() as u64;
        let bg = i.saturating_mul(dpb);
        for alt in [bg + 1, bg + dpb - 1] {
            if alt < groups && alt != bg {
                let g = alt as u32;
                out.push(sb.group_first_block(g) + sb.group_has_super(g) as u64);
            }
        }
        out.dedup();
        out.truncate(max);
    }
    out
}

/// Result of loading the descriptor table.
pub(crate) struct LoadedGdt {
    pub descs: Vec<GroupDesc>,
    pub warnings: Vec<String>,
}

/// Load all group descriptors. `read_block` returns `None` for unreadable
/// or out-of-range blocks. `source_blocks` is the number of whole blocks
/// the source holds; descriptor blocks wholly beyond it end the table (with
/// a warning) so a hostile superblock cannot make us allocate placeholders
/// for groups that do not exist in the image.
pub(crate) fn load(
    sb: &Superblock,
    source_blocks: u64,
    read_block: &dyn Fn(u64) -> Option<Vec<u8>>,
) -> LoadedGdt {
    let groups = sb.group_count();
    let dpb = sb.descs_per_block();
    let ds = sb.desc_size as usize;
    let nblocks = (groups as u64).div_ceil(dpb as u64);
    let mut descs: Vec<GroupDesc> = Vec::new();
    let mut warnings = Vec::new();
    let mut from_backup = 0u32;
    let mut missing = 0u32;
    for i in 0..nblocks {
        let first = (i * dpb as u64) as u32;
        let count = dpb.min(groups - first);
        let primary_loc = primary_gdt_block(sb, i);
        let backups = backup_gdt_blocks(sb, i, 6);
        if primary_loc >= source_blocks && backups.iter().all(|&b| b >= source_blocks) {
            warnings.push(format!(
                "group descriptors for groups {first}..{groups} lie beyond the end of the image; \
                 those groups are unavailable"
            ));
            break;
        }
        let primary = read_block(primary_loc);
        let mut slot: Vec<GroupDesc> = (0..count)
            .map(|j| {
                let g = first + j;
                match &primary {
                    Some(b) => GroupDesc::parse(g, b.get(j as usize * ds..).unwrap_or(&[]), sb),
                    None => GroupDesc::placeholder(g),
                }
            })
            .collect();
        if slot.iter().any(|d| !d.valid) {
            for loc in backups {
                let Some(b) = read_block(loc) else { continue };
                for (j, d) in slot.iter_mut().enumerate() {
                    if d.valid {
                        continue;
                    }
                    let mut alt = GroupDesc::parse(d.group, b.get(j * ds..).unwrap_or(&[]), sb);
                    if alt.valid {
                        alt.from_backup = true;
                        from_backup += 1;
                        *d = alt;
                    }
                }
                if slot.iter().all(|d| d.valid) {
                    break;
                }
            }
        }
        missing += slot.iter().filter(|d| !d.valid).count() as u32;
        descs.extend(slot);
    }
    if from_backup > 0 {
        warnings.push(format!(
            "{from_backup} group descriptor(s) taken from a backup GDT (UNINIT flags ignored)"
        ));
    }
    if missing > 0 {
        warnings.push(format!(
            "{missing} group descriptor(s) unreadable or invalid in every copy; their inodes are lost"
        ));
    }
    let bad_csum = descs
        .iter()
        .filter(|d| d.valid && !d.from_backup && d.checksum_ok == Some(false))
        .count();
    if bad_csum > 0 {
        warnings.push(format!(
            "{bad_csum} group descriptor(s) have bad checksums (flags not trusted)"
        ));
    }
    LoadedGdt { descs, warnings }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::superblock::tests::synth;

    #[test]
    fn parse_and_validate() {
        let sb = Superblock::parse(&synth(4096, 2)).unwrap();
        let mut raw = vec![0u8; 32];
        raw[0..4].copy_from_slice(&3u32.to_le_bytes());
        raw[4..8].copy_from_slice(&4u32.to_le_bytes());
        raw[8..12].copy_from_slice(&5u32.to_le_bytes());
        raw[0x0C..0x0E].copy_from_slice(&100u16.to_le_bytes());
        raw[0x12..0x14].copy_from_slice(&bg_flags::BLOCK_UNINIT.to_le_bytes());
        let d = GroupDesc::parse(0, &raw, &sb);
        assert!(d.valid);
        assert!(d.block_uninit());
        assert!(!d.inode_uninit());
        assert_eq!(d.inode_table, 5);
        assert_eq!(d.checksum_ok, None);
        raw[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(!GroupDesc::parse(0, &raw, &sb).valid);
        assert!(!GroupDesc::parse(0, &raw[..10], &sb).valid);
        assert!(!GroupDesc::parse(0, &[0u8; 32], &sb).valid);
    }

    #[test]
    fn load_falls_back_to_backup() {
        let sb = Superblock::parse(&synth(1024, 2)).unwrap();
        // Descriptor block with two valid descriptors.
        let mut good = vec![0u8; 1024];
        for (g, base) in [(0usize, 3u32), (1, 8195)] {
            let o = g * 32;
            good[o..o + 4].copy_from_slice(&base.to_le_bytes());
            good[o + 4..o + 8].copy_from_slice(&(base + 1).to_le_bytes());
            good[o + 8..o + 12].copy_from_slice(&(base + 2).to_le_bytes());
        }
        let backup_loc = sb.group_first_block(1) + 1;
        let g2 = good.clone();
        let loaded = load(&sb, 1 << 20, &move |b| {
            if b == backup_loc {
                Some(g2.clone())
            } else {
                None
            }
        });
        assert_eq!(loaded.descs.len(), 2);
        assert!(loaded.descs.iter().all(|d| d.valid && d.from_backup));
        assert!(!loaded.warnings.is_empty());
        // Nothing readable and the table beyond the source: empty, warned.
        let none = load(&sb, 2, &|_| None);
        assert!(none.descs.is_empty());
        assert!(!none.warnings.is_empty());
    }
}
