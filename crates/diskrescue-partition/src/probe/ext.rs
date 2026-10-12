//! ext2/ext3/ext4 superblock validation.
//!
//! The superblock sits 1024 bytes into the volume. It is accepted when the
//! magic `0xEF53` is present and the geometry is self-consistent (block size
//! shift at most 6, inodes and blocks per group within one bitmap block, inode
//! size a power of two between 128 and the block size, revision at most 1,
//! `first_data_block` matching the block size, a non-zero block count). The
//! generation is told apart by feature flags: `has_journal` means ext3 or
//! later; extents, 64bit, flex_bg (and other ext4-only features) mean ext4.
//!
//! With the `sparse_super` layout every filesystem keeps superblock copies at
//! the start of block groups 1, 3, 5, 7, 9, 25, 27, 49, 81... [`probe_backup`]
//! looks for them at the positions implied by the default `8 * block_size`
//! blocks per group when the primary superblock is damaged, and accepts a copy
//! only when its own `s_block_group_nr` matches the group it was found in.

use diskrescue_core::fs::FsKind;
use diskrescue_core::util::bytes::{ascii_trimmed, u16le, u32le};

use super::{Ctx, FsProbe};

/// Block groups that carry a superblock backup (sparse_super), in the order
/// they are tried.
pub(crate) const BACKUP_GROUPS: [u64; 9] = [1, 3, 5, 7, 9, 25, 27, 49, 81];

const COMPAT_HAS_JOURNAL: u32 = 0x4;
const INCOMPAT_RECOVER: u32 = 0x4;
const INCOMPAT_EXTENTS: u32 = 0x40;
const INCOMPAT_64BIT: u32 = 0x80;
const INCOMPAT_MMP: u32 = 0x100;
const INCOMPAT_FLEX_BG: u32 = 0x200;
const INCOMPAT_EA_INODE: u32 = 0x400;
const INCOMPAT_LARGEDIR: u32 = 0x4000;
const INCOMPAT_INLINE_DATA: u32 = 0x8000;
const RO_HUGE_FILE: u32 = 0x8;
const RO_GDT_CSUM: u32 = 0x10;
const RO_DIR_NLINK: u32 = 0x20;
const RO_EXTRA_ISIZE: u32 = 0x40;
const RO_BIGALLOC: u32 = 0x200;
const RO_METADATA_CSUM: u32 = 0x400;

/// Validated superblock fields.
#[derive(Debug, Clone)]
pub(crate) struct ExtSb {
    pub block_size: u64,
    pub blocks_count: u64,
    pub first_data_block: u32,
    pub blocks_per_group: u32,
    pub group_nr: u16,
    pub compat: u32,
    pub incompat: u32,
    pub ro_compat: u32,
    pub log_cluster_size: u32,
    pub label: Option<String>,
    pub uuid: [u8; 16],
    pub notes: Vec<String>,
}

impl ExtSb {
    /// Volume size in bytes.
    pub fn size_bytes(&self) -> u64 {
        self.blocks_count.saturating_mul(self.block_size)
    }

    /// Number of block groups.
    pub fn group_count(&self) -> u64 {
        let usable = self
            .blocks_count
            .saturating_sub(u64::from(self.first_data_block));
        usable.div_ceil(u64::from(self.blocks_per_group.max(1)))
    }

    /// Allocation unit in bytes (cluster for bigalloc, else block).
    pub fn cluster_bytes(&self) -> u64 {
        if self.ro_compat & RO_BIGALLOC != 0 && self.log_cluster_size < 30 {
            1024u64 << self.log_cluster_size
        } else {
            self.block_size
        }
    }

    /// Generation by feature flags.
    pub fn kind(&self) -> FsKind {
        let ext4_incompat = INCOMPAT_EXTENTS
            | INCOMPAT_64BIT
            | INCOMPAT_FLEX_BG
            | INCOMPAT_MMP
            | INCOMPAT_EA_INODE
            | INCOMPAT_LARGEDIR
            | INCOMPAT_INLINE_DATA;
        let ext4_ro = RO_HUGE_FILE
            | RO_GDT_CSUM
            | RO_DIR_NLINK
            | RO_EXTRA_ISIZE
            | RO_BIGALLOC
            | RO_METADATA_CSUM;
        if self.incompat & ext4_incompat != 0 || self.ro_compat & ext4_ro != 0 {
            FsKind::Ext4
        } else if self.compat & COMPAT_HAS_JOURNAL != 0 {
            FsKind::Ext3
        } else {
            FsKind::Ext2
        }
    }
}

/// Byte offset of the first block of group `g` for the given geometry, from
/// the volume start. This is where the superblock backup of that group lives
/// (for 1 KiB blocks the group's first block is `first_data_block + g * bpg`).
pub(crate) fn group_start_offset(
    g: u64,
    block_size: u64,
    blocks_per_group: u64,
    first_data_block: u64,
) -> Option<u64> {
    g.checked_mul(blocks_per_group)?
        .checked_add(first_data_block)?
        .checked_mul(block_size)
}

/// Validate a 1024-byte superblock image. `None` when any hard check fails.
pub(crate) fn parse_sb(sb: &[u8]) -> Option<ExtSb> {
    if sb.len() < 1024 || u16le(sb, 0x38)? != 0xEF53 {
        return None;
    }
    let mut notes = Vec::new();
    let log_bs = u32le(sb, 0x18)?;
    if log_bs > 6 {
        return None;
    }
    let block_size = 1024u64 << log_bs;
    let rev = u32le(sb, 0x4C)?;
    if rev > 1 {
        return None;
    }
    let compat = u32le(sb, 0x5C)?;
    let incompat = u32le(sb, 0x60)?;
    let ro_compat = u32le(sb, 0x64)?;
    let bigalloc = ro_compat & RO_BIGALLOC != 0;
    let bpg = u32le(sb, 0x20)?;
    let ipg = u32le(sb, 0x28)?;
    let bits_per_block = 8 * block_size;
    if ipg == 0 || u64::from(ipg) > bits_per_block || bpg == 0 {
        return None;
    }
    if !bigalloc && u64::from(bpg) > bits_per_block {
        return None;
    }
    let inode_size = if rev == 0 {
        128
    } else {
        u64::from(u16le(sb, 0x58)?)
    };
    if !inode_size.is_power_of_two() || inode_size < 128 || inode_size > block_size {
        return None;
    }
    let first_data_block = u32le(sb, 0x14)?;
    let expected_fdb = u32::from(block_size == 1024 && !bigalloc);
    if first_data_block != expected_fdb {
        return None;
    }
    let mut blocks = u64::from(u32le(sb, 0x04)?);
    if incompat & INCOMPAT_64BIT != 0 {
        blocks |= u64::from(u32le(sb, 0x150)?) << 32;
    }
    if blocks <= u64::from(first_data_block) || blocks.checked_mul(block_size).is_none() {
        return None;
    }
    let inodes_count = u32le(sb, 0x00)?;
    let mut parsed = ExtSb {
        block_size,
        blocks_count: blocks,
        first_data_block,
        blocks_per_group: bpg,
        group_nr: u16le(sb, 0x5A)?,
        compat,
        incompat,
        ro_compat,
        log_cluster_size: u32le(sb, 0x1C)?,
        label: {
            let l = ascii_trimmed(sb.get(0x78..0x88)?);
            if l.is_empty() {
                None
            } else {
                Some(l)
            }
        },
        uuid: sb.get(0x68..0x78)?.try_into().ok()?,
        notes: Vec::new(),
    };
    if u64::from(inodes_count) != u64::from(ipg).saturating_mul(parsed.group_count()) {
        notes.push("inode count does not match the group geometry".to_string());
    }
    if incompat & INCOMPAT_RECOVER != 0 {
        notes.push("journal needs recovery".to_string());
    }
    if bigalloc {
        notes.push("bigalloc filesystem".to_string());
    }
    parsed.notes = notes;
    Some(parsed)
}

fn build(ctx: &Ctx, sb: &ExtSb, via_backup: bool) -> FsProbe {
    let mut p = FsProbe::new(sb.kind());
    p.size = Some(sb.size_bytes());
    p.cluster_size = u32::try_from(sb.cluster_bytes()).ok();
    p.label.clone_from(&sb.label);
    p.via_backup = via_backup;
    p.notes.clone_from(&sb.notes);
    p.notes.push(format!("uuid {}", super::uuid_text(&sb.uuid)));
    if via_backup {
        p.notes.push(format!(
            "primary superblock is damaged; using the backup in block group {}",
            sb.group_nr
        ));
    } else if sb.group_nr != 0 {
        p.notes.push(format!(
            "superblock claims to be the copy of block group {}",
            sb.group_nr
        ));
    }
    if sb.size_bytes() > ctx.avail {
        p.notes.push(format!(
            "filesystem claims {} bytes but only {} are available",
            sb.size_bytes(),
            ctx.avail
        ));
    }
    p
}

/// Probe the primary superblock at byte 1024.
pub(crate) fn probe_primary(ctx: &Ctx) -> Option<FsProbe> {
    let sb = parse_sb(ctx.head.get(1024..2048)?)?;
    Some(build(ctx, &sb, false))
}

/// Probe backup superblocks at the starts of block groups 1, 3, 5, ...
pub(crate) fn probe_backup(ctx: &Ctx) -> Option<FsProbe> {
    for log_bs in 0u32..=2 {
        let bs = 1024u64 << log_bs;
        let bpg = 8 * bs;
        let fdb = u64::from(bs == 1024);
        for g in BACKUP_GROUPS {
            let Some(pos) = group_start_offset(g, bs, bpg, fdb) else {
                break;
            };
            if pos.saturating_add(1024) > ctx.avail {
                break;
            }
            let Some(raw) = ctx.read(pos, 1024) else {
                continue;
            };
            let Some(sb) = parse_sb(&raw) else { continue };
            if sb.block_size == bs
                && u64::from(sb.blocks_per_group) == bpg
                && u64::from(sb.group_nr) == g
            {
                return Some(build(ctx, &sb, true));
            }
        }
    }
    None
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A valid ext4 superblock image: 4 KiB blocks, 1 GiB, 8 groups.
    pub(crate) fn superblock(log_bs: u32, blocks: u32, group_nr: u16) -> Vec<u8> {
        let bs = 1024u64 << log_bs;
        let bpg = (8 * bs) as u32;
        let fdb = u32::from(bs == 1024);
        let groups = (blocks - fdb).div_ceil(bpg);
        let ipg = 8192u32.min(bpg);
        let mut s = vec![0u8; 1024];
        s[0x00..0x04].copy_from_slice(&(ipg * groups).to_le_bytes());
        s[0x04..0x08].copy_from_slice(&blocks.to_le_bytes());
        s[0x14..0x18].copy_from_slice(&fdb.to_le_bytes());
        s[0x18..0x1C].copy_from_slice(&log_bs.to_le_bytes());
        s[0x20..0x24].copy_from_slice(&bpg.to_le_bytes());
        s[0x28..0x2C].copy_from_slice(&ipg.to_le_bytes());
        s[0x38..0x3A].copy_from_slice(&0xEF53u16.to_le_bytes());
        s[0x4C..0x50].copy_from_slice(&1u32.to_le_bytes());
        s[0x58..0x5A].copy_from_slice(&256u16.to_le_bytes());
        s[0x5A..0x5C].copy_from_slice(&group_nr.to_le_bytes());
        s[0x78..0x7D].copy_from_slice(b"label");
        s
    }

    #[test]
    fn classifies_generations() {
        let mut s = superblock(2, 262144, 0);
        assert_eq!(parse_sb(&s).unwrap().kind(), FsKind::Ext2);
        s[0x5C] = 0x04; // has_journal
        assert_eq!(parse_sb(&s).unwrap().kind(), FsKind::Ext3);
        s[0x60] = 0x40; // extents
        assert_eq!(parse_sb(&s).unwrap().kind(), FsKind::Ext4);
        s[0x60] = 0x00;
        s[0x61] = 0x02; // flex_bg (0x200)
        assert_eq!(parse_sb(&s).unwrap().kind(), FsKind::Ext4);
        s[0x61] = 0x00;
        s[0x60] = 0x80; // 64bit
        assert_eq!(parse_sb(&s).unwrap().kind(), FsKind::Ext4);
    }

    #[test]
    fn geometry_is_validated() {
        let good = superblock(2, 262144, 0);
        let b = parse_sb(&good).unwrap();
        assert_eq!(b.size_bytes(), 1 << 30);
        assert_eq!(b.group_count(), 8);
        assert_eq!(b.label.as_deref(), Some("label"));
        assert!(b.notes.is_empty(), "{:?}", b.notes);
        for (off, val) in [(0x18usize, 9u8), (0x38, 0), (0x28, 0), (0x4C, 7)] {
            let mut s = good.clone();
            s[off] = val;
            if off == 0x28 {
                s[0x29..0x2C].fill(0);
            }
            assert!(parse_sb(&s).is_none(), "byte {off:#x}={val} accepted");
        }
        let mut s = good.clone();
        s[0x14] = 1; // first_data_block wrong for 4 KiB
        assert!(parse_sb(&s).is_none());
        let mut s = good.clone();
        s[0x58..0x5A].copy_from_slice(&100u16.to_le_bytes());
        assert!(parse_sb(&s).is_none());
        let mut s = good.clone();
        s[0x04..0x08].copy_from_slice(&0u32.to_le_bytes());
        assert!(parse_sb(&s).is_none());
        assert!(parse_sb(&good[..512]).is_none());
        // 1 KiB blocks need first_data_block = 1.
        let k = superblock(0, 65536, 0);
        assert_eq!(parse_sb(&k).unwrap().first_data_block, 1);
    }

    #[test]
    fn huge_block_counts_do_not_overflow() {
        // 1 KiB blocks, 64bit, 2^50 blocks, one block per group: the inode
        // total (groups * inodes per group) would overflow a u64.
        let mut s = superblock(0, 65536, 0);
        s[0x04..0x08].copy_from_slice(&0u32.to_le_bytes());
        s[0x150..0x154].copy_from_slice(&(1u32 << 18).to_le_bytes());
        s[0x60] = 0x80;
        s[0x20..0x24].copy_from_slice(&1u32.to_le_bytes());
        let b = parse_sb(&s).unwrap();
        assert_eq!(b.blocks_count, 1u64 << 50);
        assert!(b.group_count() > 1 << 49);
        assert!(!b.notes.is_empty());
    }

    #[test]
    fn group_offsets() {
        // 1 KiB blocks: group 1 starts at block 8193.
        assert_eq!(group_start_offset(1, 1024, 8192, 1), Some(8193 * 1024));
        // 4 KiB blocks: group 1 at block 32768.
        assert_eq!(group_start_offset(1, 4096, 32768, 0), Some(32768 * 4096));
        assert_eq!(group_start_offset(u64::MAX, 4096, 32768, 0), None);
    }
}
