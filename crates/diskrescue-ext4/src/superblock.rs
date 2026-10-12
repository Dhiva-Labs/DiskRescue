//! The ext2/3/4 superblock: decoding, validation and backup locations.
//!
//! The primary superblock lives at byte 1024 of the volume and is 1024 bytes
//! long. Copies live at the start of block groups 1, 3, 5, 7, 9, 25, 27, ...
//! (powers of 3, 5 and 7) with `sparse_super`, in every group without it, or
//! in the two groups named by `s_backup_bgs` with `sparse_super2`.
//!
//! Validation follows the kernel's mount-time checks closely (magic, block
//! size, inode size, per-group counts, `inodes_count == groups *
//! inodes_per_group`) so a random 0xEF53 in garbage is not accepted. A size
//! larger than the image is *not* an error: rescued images are often short.

use diskrescue_core::fs::FsKind;
use diskrescue_core::util::bytes::{ascii_trimmed, u16le, u32le, u8_at};
use diskrescue_core::{Error, Result};

use crate::csum::crc32c_raw;

/// Byte offset of the primary superblock inside the volume.
pub const SUPERBLOCK_OFFSET: u64 = 1024;
/// Size of the on-disk superblock structure.
pub const SUPERBLOCK_SIZE: usize = 1024;
/// `s_magic`.
pub const EXT_MAGIC: u16 = 0xEF53;
/// Highest group count accepted (keeps hostile superblocks from causing
/// absurd allocations; 2^22 groups of 32768 4 KiB blocks is 512 TiB).
pub const MAX_GROUPS: u64 = 1 << 22;

/// Feature flag constants (`s_feature_compat`, `s_feature_incompat`,
/// `s_feature_ro_compat`).
pub mod features {
    /// COMPAT: the filesystem has a journal (ext3/ext4).
    pub const COMPAT_HAS_JOURNAL: u32 = 0x4;
    /// COMPAT: extended attributes.
    pub const COMPAT_EXT_ATTR: u32 = 0x8;
    /// COMPAT: reserved GDT blocks for online growth.
    pub const COMPAT_RESIZE_INODE: u32 = 0x10;
    /// COMPAT: hashed (htree) directories.
    pub const COMPAT_DIR_INDEX: u32 = 0x20;
    /// COMPAT: only the two groups in `s_backup_bgs` hold backups.
    pub const COMPAT_SPARSE_SUPER2: u32 = 0x200;
    /// COMPAT: orphan file.
    pub const COMPAT_ORPHAN_FILE: u32 = 0x1000;

    /// INCOMPAT: compression (never finished upstream).
    pub const INCOMPAT_COMPRESSION: u32 = 0x1;
    /// INCOMPAT: directory entries carry a file type byte.
    pub const INCOMPAT_FILETYPE: u32 = 0x2;
    /// INCOMPAT: the journal needs recovery.
    pub const INCOMPAT_RECOVER: u32 = 0x4;
    /// INCOMPAT: this is an external journal device, not a filesystem.
    pub const INCOMPAT_JOURNAL_DEV: u32 = 0x8;
    /// INCOMPAT: meta block groups (descriptor blocks spread over the disk).
    pub const INCOMPAT_META_BG: u32 = 0x10;
    /// INCOMPAT: files may use extent trees.
    pub const INCOMPAT_EXTENTS: u32 = 0x40;
    /// INCOMPAT: 64-bit block numbers and larger group descriptors.
    pub const INCOMPAT_64BIT: u32 = 0x80;
    /// INCOMPAT: multi-mount protection.
    pub const INCOMPAT_MMP: u32 = 0x100;
    /// INCOMPAT: flexible block groups.
    pub const INCOMPAT_FLEX_BG: u32 = 0x200;
    /// INCOMPAT: large extended attribute values in inodes.
    pub const INCOMPAT_EA_INODE: u32 = 0x400;
    /// INCOMPAT: data in directory entries (Lustre).
    pub const INCOMPAT_DIRDATA: u32 = 0x1000;
    /// INCOMPAT: metadata checksum seed stored in the superblock.
    pub const INCOMPAT_CSUM_SEED: u32 = 0x2000;
    /// INCOMPAT: large directories (> 2 GiB or 3-level htree).
    pub const INCOMPAT_LARGEDIR: u32 = 0x4000;
    /// INCOMPAT: small files and directories stored inside the inode.
    pub const INCOMPAT_INLINE_DATA: u32 = 0x8000;
    /// INCOMPAT: fscrypt encryption.
    pub const INCOMPAT_ENCRYPT: u32 = 0x10000;
    /// INCOMPAT: case-insensitive directories.
    pub const INCOMPAT_CASEFOLD: u32 = 0x20000;

    /// RO_COMPAT: superblock backups only in groups 0, 1 and powers of 3/5/7.
    pub const RO_COMPAT_SPARSE_SUPER: u32 = 0x1;
    /// RO_COMPAT: files larger than 2 GiB.
    pub const RO_COMPAT_LARGE_FILE: u32 = 0x2;
    /// RO_COMPAT: btree directories (never used).
    pub const RO_COMPAT_BTREE_DIR: u32 = 0x4;
    /// RO_COMPAT: `i_blocks` may be in filesystem-block units.
    pub const RO_COMPAT_HUGE_FILE: u32 = 0x8;
    /// RO_COMPAT: crc16 group descriptor checksums.
    pub const RO_COMPAT_GDT_CSUM: u32 = 0x10;
    /// RO_COMPAT: directories may have more than 65000 subdirectories.
    pub const RO_COMPAT_DIR_NLINK: u32 = 0x20;
    /// RO_COMPAT: inodes have extra fields beyond 128 bytes.
    pub const RO_COMPAT_EXTRA_ISIZE: u32 = 0x40;
    /// RO_COMPAT: quota inodes.
    pub const RO_COMPAT_QUOTA: u32 = 0x100;
    /// RO_COMPAT: allocation in clusters of several blocks.
    pub const RO_COMPAT_BIGALLOC: u32 = 0x200;
    /// RO_COMPAT: crc32c checksums on all metadata.
    pub const RO_COMPAT_METADATA_CSUM: u32 = 0x400;

    /// INCOMPAT bits an ext3 driver understands; anything else means ext4.
    pub const EXT3_INCOMPAT_SUPP: u32 = INCOMPAT_FILETYPE | INCOMPAT_RECOVER | INCOMPAT_META_BG;
    /// RO_COMPAT bits an ext3 driver understands; anything else means ext4.
    pub const EXT3_RO_COMPAT_SUPP: u32 =
        RO_COMPAT_SPARSE_SUPER | RO_COMPAT_LARGE_FILE | RO_COMPAT_BTREE_DIR;
}

use features::*;

/// A decoded and validated superblock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Superblock {
    /// `s_inodes_count`.
    pub inodes_count: u32,
    /// `s_blocks_count` (with the high word when `64bit`).
    pub blocks_count: u64,
    /// Reserved blocks (with the high word when `64bit`).
    pub r_blocks_count: u64,
    /// Free blocks (stale in backups).
    pub free_blocks_count: u64,
    /// Free inodes (stale in backups).
    pub free_inodes_count: u32,
    /// First block of group 0 (1 for 1 KiB blocks, else 0).
    pub first_data_block: u32,
    /// `s_log_block_size`.
    pub log_block_size: u32,
    /// Block size in bytes (`1024 << log_block_size`).
    pub block_size: u32,
    /// `s_log_cluster_size` (bigalloc).
    pub log_cluster_size: u32,
    /// Blocks per group.
    pub blocks_per_group: u32,
    /// Clusters per group (bigalloc).
    pub clusters_per_group: u32,
    /// Inodes per group.
    pub inodes_per_group: u32,
    /// Last mount time (Unix seconds).
    pub mtime: u32,
    /// Last write time (Unix seconds).
    pub wtime: u32,
    /// `s_state`: 1 clean, 2 errors, 4 orphans being recovered.
    pub state: u16,
    /// `s_errors` policy.
    pub errors: u16,
    /// `s_minor_rev_level`.
    pub minor_rev_level: u16,
    /// `s_rev_level` (0 original, 1 dynamic).
    pub rev_level: u32,
    /// Creator OS (0 Linux).
    pub creator_os: u32,
    /// First non-reserved inode.
    pub first_ino: u32,
    /// On-disk inode record size.
    pub inode_size: u16,
    /// Group number of this superblock copy (0 for the primary).
    pub block_group_nr: u16,
    /// `s_feature_compat`.
    pub feature_compat: u32,
    /// `s_feature_incompat`.
    pub feature_incompat: u32,
    /// `s_feature_ro_compat`.
    pub feature_ro_compat: u32,
    /// Volume UUID.
    pub uuid: [u8; 16],
    /// Volume label (may be empty).
    pub volume_name: String,
    /// Path where the volume was last mounted.
    pub last_mounted: String,
    /// Blocks reserved after the GDT for online growth.
    pub reserved_gdt_blocks: u16,
    /// Journal inode number.
    pub journal_inum: u32,
    /// Head of the orphan inode list.
    pub last_orphan: u32,
    /// Effective group descriptor size (32, or `s_desc_size` with `64bit`).
    pub desc_size: u16,
    /// First meta block group (`meta_bg`).
    pub first_meta_bg: u32,
    /// Filesystem creation time.
    pub mkfs_time: u32,
    /// `s_min_extra_isize`.
    pub min_extra_isize: u16,
    /// `s_log_groups_per_flex`.
    pub log_groups_per_flex: u8,
    /// `s_checksum_type` (1 = crc32c).
    pub checksum_type: u8,
    /// `sparse_super2` backup groups.
    pub backup_bgs: [u32; 2],
    /// User quota inode.
    pub usr_quota_inum: u32,
    /// Group quota inode.
    pub grp_quota_inum: u32,
    /// Project quota inode.
    pub prj_quota_inum: u32,
    /// `s_lpf_ino` (lost+found inode when encrypted).
    pub lpf_ino: u32,
    /// `s_checksum_seed` (`metadata_csum_seed`).
    pub checksum_seed: u32,
    /// Orphan file inode (`orphan_file`).
    pub orphan_file_inum: u32,
    /// Stored superblock checksum.
    pub checksum: u32,
    /// Whether the stored checksum matches (None without `metadata_csum`).
    pub checksum_ok: Option<bool>,
}

fn corrupt(detail: impl Into<String>) -> Error {
    Error::corrupt("ext superblock", detail)
}

impl Superblock {
    /// Decode and validate a 1024-byte superblock.
    pub fn parse(buf: &[u8]) -> Result<Superblock> {
        if buf.len() < SUPERBLOCK_SIZE {
            return Err(corrupt(format!("only {} bytes", buf.len())));
        }
        let r32 = |o: usize| u32le(buf, o).unwrap_or(0);
        let r16 = |o: usize| u16le(buf, o).unwrap_or(0);
        let r8 = |o: usize| u8_at(buf, o).unwrap_or(0);
        let magic = r16(0x38);
        if magic != EXT_MAGIC {
            return Err(corrupt(format!("bad magic {magic:#06x}")));
        }
        let incompat = r32(0x60);
        let is64 = incompat & INCOMPAT_64BIT != 0;
        let hi = |o: usize| if is64 { (r32(o) as u64) << 32 } else { 0 };
        let log_block_size = r32(0x18);
        if log_block_size > 6 {
            return Err(corrupt(format!("log_block_size {log_block_size} > 6")));
        }
        let block_size = 1024u32 << log_block_size;
        let rev_level = r32(0x4C);
        let (first_ino, inode_size) = if rev_level == 0 {
            (11, 128)
        } else {
            (r32(0x54), r16(0x58))
        };
        let raw_desc = r16(0xFE);
        let desc_size = if is64 { raw_desc } else { 32 };
        let mut uuid = [0u8; 16];
        uuid.copy_from_slice(&buf[0x68..0x78]);
        let checksum = r32(0x3FC);
        let ro = r32(0x64);
        let checksum_ok = if ro & RO_COMPAT_METADATA_CSUM != 0 {
            Some(crc32c_raw(!0, &buf[..0x3FC]) == checksum)
        } else {
            None
        };
        let sb = Superblock {
            inodes_count: r32(0x00),
            blocks_count: r32(0x04) as u64 | hi(0x150),
            r_blocks_count: r32(0x08) as u64 | hi(0x154),
            free_blocks_count: r32(0x0C) as u64 | hi(0x158),
            free_inodes_count: r32(0x10),
            first_data_block: r32(0x14),
            log_block_size,
            block_size,
            log_cluster_size: r32(0x1C),
            blocks_per_group: r32(0x20),
            clusters_per_group: r32(0x24),
            inodes_per_group: r32(0x28),
            mtime: r32(0x2C),
            wtime: r32(0x30),
            state: r16(0x3A),
            errors: r16(0x3C),
            minor_rev_level: r16(0x3E),
            rev_level,
            creator_os: r32(0x48),
            first_ino,
            inode_size,
            block_group_nr: r16(0x5A),
            feature_compat: r32(0x5C),
            feature_incompat: incompat,
            feature_ro_compat: ro,
            uuid,
            volume_name: ascii_trimmed(&buf[0x78..0x88]),
            last_mounted: ascii_trimmed(&buf[0x88..0xC8]),
            reserved_gdt_blocks: r16(0xCE),
            journal_inum: r32(0xE0),
            last_orphan: r32(0xE8),
            desc_size,
            first_meta_bg: r32(0x104),
            mkfs_time: r32(0x108),
            min_extra_isize: r16(0x15C),
            log_groups_per_flex: r8(0x174),
            checksum_type: r8(0x175),
            backup_bgs: [r32(0x24C), r32(0x250)],
            usr_quota_inum: r32(0x240),
            grp_quota_inum: r32(0x244),
            prj_quota_inum: r32(0x26C),
            lpf_ino: r32(0x268),
            checksum_seed: r32(0x270),
            orphan_file_inum: r32(0x280),
            checksum,
            checksum_ok,
        };
        sb.validate()?;
        Ok(sb)
    }

    fn validate(&self) -> Result<()> {
        let bs = self.block_size as u64;
        if self.rev_level > 1 {
            return Err(corrupt(format!("rev_level {}", self.rev_level)));
        }
        let isz = self.inode_size as u64;
        if !(128..=bs).contains(&isz) || !isz.is_power_of_two() {
            return Err(corrupt(format!("inode size {isz}")));
        }
        if self.blocks_per_group == 0 || self.blocks_per_group as u64 > 8 * bs {
            return Err(corrupt(format!(
                "blocks_per_group {}",
                self.blocks_per_group
            )));
        }
        let inodes_per_block = bs / isz;
        if (self.inodes_per_group as u64) < inodes_per_block
            || self.inodes_per_group as u64 > 8 * bs
        {
            return Err(corrupt(format!(
                "inodes_per_group {}",
                self.inodes_per_group
            )));
        }
        let bigalloc = self.has_ro_compat(RO_COMPAT_BIGALLOC);
        let expected_fdb = if bs == 1024 && !bigalloc { 1 } else { 0 };
        if self.first_data_block != expected_fdb {
            return Err(corrupt(format!(
                "first_data_block {} (expected {expected_fdb})",
                self.first_data_block
            )));
        }
        if self.blocks_count <= self.first_data_block as u64 {
            return Err(corrupt(format!("blocks_count {}", self.blocks_count)));
        }
        let groups = self.group_count_u64();
        if groups > MAX_GROUPS {
            return Err(corrupt(format!("{groups} block groups")));
        }
        if groups * self.inodes_per_group as u64 != self.inodes_count as u64 {
            return Err(corrupt(format!(
                "inodes_count {} != {} groups * {} inodes",
                self.inodes_count, groups, self.inodes_per_group
            )));
        }
        if self.first_ino < 11 || self.first_ino >= self.inodes_count {
            return Err(corrupt(format!("first_ino {}", self.first_ino)));
        }
        if self.has_incompat(INCOMPAT_64BIT) {
            let d = self.desc_size as u64;
            if !(64..=1024).contains(&d) || !d.is_power_of_two() || d > bs {
                return Err(corrupt(format!("desc_size {d} with 64bit")));
            }
        }
        Ok(())
    }

    /// Whether a COMPAT feature bit is set.
    pub fn has_compat(&self, bit: u32) -> bool {
        self.feature_compat & bit != 0
    }

    /// Whether an INCOMPAT feature bit is set.
    pub fn has_incompat(&self, bit: u32) -> bool {
        self.feature_incompat & bit != 0
    }

    /// Whether a RO_COMPAT feature bit is set.
    pub fn has_ro_compat(&self, bit: u32) -> bool {
        self.feature_ro_compat & bit != 0
    }

    /// ext2, ext3 or ext4, decided from the feature flags the way blkid does:
    /// anything an ext3 driver does not understand makes it ext4; otherwise
    /// a journal makes it ext3.
    pub fn kind(&self) -> FsKind {
        if self.feature_incompat & !EXT3_INCOMPAT_SUPP != 0
            || self.feature_ro_compat & !EXT3_RO_COMPAT_SUPP != 0
        {
            FsKind::Ext4
        } else if self.has_compat(COMPAT_HAS_JOURNAL) {
            FsKind::Ext3
        } else {
            FsKind::Ext2
        }
    }

    fn group_count_u64(&self) -> u64 {
        (self.blocks_count - self.first_data_block as u64).div_ceil(self.blocks_per_group as u64)
    }

    /// Number of block groups.
    pub fn group_count(&self) -> u32 {
        self.group_count_u64().min(MAX_GROUPS) as u32
    }

    /// First block of group `g`.
    pub fn group_first_block(&self, g: u32) -> u64 {
        self.first_data_block as u64 + g as u64 * self.blocks_per_group as u64
    }

    /// Blocks occupied by one group's inode table.
    pub fn inode_table_blocks(&self) -> u64 {
        (self.inodes_per_group as u64 * self.inode_size as u64).div_ceil(self.block_size as u64)
    }

    /// Group descriptors per block.
    pub fn descs_per_block(&self) -> u32 {
        (self.block_size / self.desc_size.max(32) as u32).max(1)
    }

    /// Whether group descriptor checksums (and therefore the UNINIT flags)
    /// are in use.
    pub fn has_group_desc_csum(&self) -> bool {
        self.has_ro_compat(RO_COMPAT_METADATA_CSUM) || self.has_ro_compat(RO_COMPAT_GDT_CSUM)
    }

    /// Seed for `metadata_csum` checksums.
    pub fn csum_seed(&self) -> u32 {
        if self.has_incompat(INCOMPAT_CSUM_SEED) {
            self.checksum_seed
        } else {
            crc32c_raw(!0, &self.uuid)
        }
    }

    /// Whether group `g` holds a superblock (and GDT) backup.
    pub fn group_has_super(&self, g: u32) -> bool {
        if g == 0 {
            return true;
        }
        if self.has_compat(COMPAT_SPARSE_SUPER2) {
            return g == self.backup_bgs[0] || g == self.backup_bgs[1];
        }
        if g <= 1 || !self.has_ro_compat(RO_COMPAT_SPARSE_SUPER) {
            return true;
        }
        if g & 1 == 0 {
            return false;
        }
        is_power_of(g, 3) || is_power_of(g, 5) || is_power_of(g, 7)
    }

    /// UUID in the usual 8-4-4-4-12 text form.
    pub fn uuid_string(&self) -> String {
        let h = diskrescue_core::util::bytes::hex(&self.uuid);
        format!(
            "{}-{}-{}-{}-{}",
            &h[0..8],
            &h[8..12],
            &h[12..16],
            &h[16..20],
            &h[20..32]
        )
    }

    /// Inodes that are filesystem metadata and never reachable from a
    /// directory (journal, quota files, orphan file, the reserved range).
    pub fn is_special_inode(&self, ino: u32) -> bool {
        ino < self.first_ino
            || [
                self.journal_inum,
                self.usr_quota_inum,
                self.grp_quota_inum,
                self.prj_quota_inum,
                self.orphan_file_inum,
            ]
            .contains(&ino)
    }
}

fn is_power_of(mut n: u32, base: u32) -> bool {
    if n == 0 {
        return false;
    }
    while n % base == 0 {
        n /= base;
    }
    n == 1
}

/// Groups that carry superblock backups with `sparse_super`, in ascending
/// order, below `limit`: 1 and the powers of 3, 5 and 7.
pub fn sparse_backup_groups(limit: u64) -> Vec<u32> {
    let mut v = vec![1u64];
    for base in [3u64, 5, 7] {
        let mut p = base;
        while p < limit {
            v.push(p);
            p *= base;
        }
    }
    v.retain(|&g| g < limit);
    v.sort_unstable();
    v.dedup();
    v.into_iter().map(|g| g as u32).collect()
}

/// Byte offset of the superblock copy in group `g`, assuming the default
/// geometry (`8 * block_size` blocks per group) for `block_size`.
pub fn default_backup_offset(block_size: u64, g: u32) -> u64 {
    let fdb = if block_size == 1024 { 1 } else { 0 };
    (fdb + g as u64 * 8 * block_size) * block_size
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Build a plausible superblock for unit tests.
    pub(crate) fn synth(bs: u32, groups: u32) -> Vec<u8> {
        let mut b = vec![0u8; 1024];
        let log = bs.trailing_zeros() - 10;
        let bpg = 8 * bs;
        let fdb: u32 = if bs == 1024 { 1 } else { 0 };
        let ipg = 256u32;
        let blocks = fdb as u64 + groups as u64 * bpg as u64;
        b[0x00..0x04].copy_from_slice(&(ipg * groups).to_le_bytes());
        b[0x04..0x08].copy_from_slice(&(blocks as u32).to_le_bytes());
        b[0x14..0x18].copy_from_slice(&fdb.to_le_bytes());
        b[0x18..0x1C].copy_from_slice(&log.to_le_bytes());
        b[0x20..0x24].copy_from_slice(&bpg.to_le_bytes());
        b[0x24..0x28].copy_from_slice(&bpg.to_le_bytes());
        b[0x28..0x2C].copy_from_slice(&ipg.to_le_bytes());
        b[0x38..0x3A].copy_from_slice(&EXT_MAGIC.to_le_bytes());
        b[0x4C..0x50].copy_from_slice(&1u32.to_le_bytes());
        b[0x54..0x58].copy_from_slice(&11u32.to_le_bytes());
        b[0x58..0x5A].copy_from_slice(&256u16.to_le_bytes());
        b[0x60..0x64].copy_from_slice(&INCOMPAT_FILETYPE.to_le_bytes());
        b[0x64..0x68].copy_from_slice(&RO_COMPAT_SPARSE_SUPER.to_le_bytes());
        b[0x68..0x78].copy_from_slice(&[0x11; 16]);
        b[0x78..0x7D].copy_from_slice(b"LABEL");
        b
    }

    #[test]
    fn parse_and_kind() {
        let mut b = synth(4096, 3);
        let sb = Superblock::parse(&b).unwrap();
        assert_eq!(sb.block_size, 4096);
        assert_eq!(sb.group_count(), 3);
        assert_eq!(sb.kind(), FsKind::Ext2);
        assert_eq!(sb.volume_name, "LABEL");
        assert_eq!(sb.uuid_string(), "11111111-1111-1111-1111-111111111111");
        b[0x5C] = COMPAT_HAS_JOURNAL as u8;
        assert_eq!(Superblock::parse(&b).unwrap().kind(), FsKind::Ext3);
        b[0x60..0x64].copy_from_slice(&(INCOMPAT_FILETYPE | INCOMPAT_EXTENTS).to_le_bytes());
        assert_eq!(Superblock::parse(&b).unwrap().kind(), FsKind::Ext4);
    }

    #[test]
    fn rejects_inconsistent_fields() {
        let good = synth(1024, 2);
        assert!(Superblock::parse(&good).is_ok());
        let mut b = good.clone();
        b[0x38] = 0;
        assert!(Superblock::parse(&b).is_err());
        let mut b = good.clone();
        b[0x18] = 7; // 128 KiB blocks
        assert!(Superblock::parse(&b).is_err());
        let mut b = good.clone();
        b[0x00] ^= 1; // inodes_count mismatch
        assert!(Superblock::parse(&b).is_err());
        let mut b = good.clone();
        b[0x14] = 0; // first_data_block wrong for 1 KiB
        assert!(Superblock::parse(&b).is_err());
        let mut b = good.clone();
        b[0x58..0x5A].copy_from_slice(&300u16.to_le_bytes());
        assert!(Superblock::parse(&b).is_err());
        let mut b = good;
        b[0x60..0x64].copy_from_slice(&INCOMPAT_64BIT.to_le_bytes());
        b[0xFE..0x100].copy_from_slice(&32u16.to_le_bytes());
        assert!(Superblock::parse(&b).is_err());
        assert!(Superblock::parse(&[0u8; 100]).is_err());
    }

    #[test]
    fn backup_groups() {
        assert_eq!(
            sparse_backup_groups(100),
            vec![1, 3, 5, 7, 9, 25, 27, 49, 81]
        );
        let sb = Superblock::parse(&synth(4096, 30)).unwrap();
        let with: Vec<u32> = (0..30).filter(|&g| sb.group_has_super(g)).collect();
        assert_eq!(with, vec![0, 1, 3, 5, 7, 9, 25, 27]);
        assert_eq!(default_backup_offset(4096, 1), 134_217_728);
        assert_eq!(default_backup_offset(1024, 1), 8_389_632);
    }
}
