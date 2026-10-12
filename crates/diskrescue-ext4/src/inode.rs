//! Inodes: decoding, file types, timestamps and in-inode extended
//! attributes (which hold the tail of inline data).
//!
//! Inode `N` (1-based) lives in group `(N-1) / inodes_per_group` at index
//! `(N-1) % inodes_per_group` of that group's inode table. Records are
//! `s_inode_size` bytes; the first 128 bytes are the classic ext2 layout and
//! anything beyond is only valid up to `128 + i_extra_isize`.

use diskrescue_core::time::Timestamp;
use diskrescue_core::util::bytes::{slice, u16le, u32le};
use diskrescue_core::{Error, Result};

use crate::superblock::{features::*, Superblock};

/// Root directory inode.
pub const ROOT_INO: u32 = 2;

/// `i_flags` bits used by the reader.
pub mod iflags {
    /// fscrypt-encrypted inode.
    pub const ENCRYPT: u32 = 0x800;
    /// Hash-indexed (htree) directory.
    pub const INDEX: u32 = 0x1000;
    /// `i_blocks` is in filesystem blocks rather than 512-byte sectors.
    pub const HUGE_FILE: u32 = 0x40000;
    /// The inode uses an extent tree.
    pub const EXTENTS: u32 = 0x80000;
    /// The inode stores a large extended attribute value.
    pub const EA_INODE: u32 = 0x200000;
    /// The inode holds its data inline.
    pub const INLINE_DATA: u32 = 0x1000_0000;
}

/// File type from `i_mode` (or a directory entry's type byte).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileType {
    /// Regular file.
    Regular,
    /// Directory.
    Directory,
    /// Symbolic link.
    Symlink,
    /// Character device.
    CharDevice,
    /// Block device.
    BlockDevice,
    /// Named pipe.
    Fifo,
    /// Unix socket.
    Socket,
    /// Mode bits do not name a valid type (wiped or garbage inode).
    Unknown,
}

impl FileType {
    /// Decode the type bits (12..15) of `i_mode`.
    pub fn from_mode(mode: u16) -> FileType {
        match mode & 0xF000 {
            0x8000 => FileType::Regular,
            0x4000 => FileType::Directory,
            0xA000 => FileType::Symlink,
            0x2000 => FileType::CharDevice,
            0x6000 => FileType::BlockDevice,
            0x1000 => FileType::Fifo,
            0xC000 => FileType::Socket,
            _ => FileType::Unknown,
        }
    }

    /// Decode a directory entry `file_type` byte (1..=7).
    pub fn from_dirent(t: u8) -> Option<FileType> {
        Some(match t {
            1 => FileType::Regular,
            2 => FileType::Directory,
            3 => FileType::CharDevice,
            4 => FileType::BlockDevice,
            5 => FileType::Fifo,
            6 => FileType::Socket,
            7 => FileType::Symlink,
            _ => return None,
        })
    }

    /// Short lowercase name.
    pub fn name(self) -> &'static str {
        match self {
            FileType::Regular => "file",
            FileType::Directory => "dir",
            FileType::Symlink => "symlink",
            FileType::CharDevice => "chardev",
            FileType::BlockDevice => "blockdev",
            FileType::Fifo => "fifo",
            FileType::Socket => "socket",
            FileType::Unknown => "unknown",
        }
    }
}

/// A decoded inode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inode {
    /// Inode number.
    pub ino: u32,
    /// `i_mode` (type and permission bits).
    pub mode: u16,
    /// Owner (low and high words combined).
    pub uid: u32,
    /// Group (low and high words combined).
    pub gid: u32,
    /// Logical size in bytes.
    pub size: u64,
    /// Last access.
    pub atime: Option<Timestamp>,
    /// Last inode change.
    pub ctime: Option<Timestamp>,
    /// Last modification.
    pub mtime: Option<Timestamp>,
    /// Creation (only with large inodes).
    pub crtime: Option<Timestamp>,
    /// Deletion time; also overloaded as the next orphan inode while the
    /// inode is on the orphan list.
    pub dtime: Option<Timestamp>,
    /// Raw `i_dtime`.
    pub dtime_raw: u32,
    /// Hard link count (0 = deleted or being deleted).
    pub links_count: u16,
    /// Raw `i_blocks` (512-byte units unless [`iflags::HUGE_FILE`]).
    pub blocks: u64,
    /// `i_flags`.
    pub flags: u32,
    /// `i_block`: block map, extent root, inline data or symlink target.
    pub i_block: [u8; 60],
    /// `i_generation`.
    pub generation: u32,
    /// Extended attribute block.
    pub file_acl: u64,
    /// `i_extra_isize` (0 when the inode is 128 bytes or the field is bad).
    pub extra_isize: u16,
    raw: Vec<u8>,
}

fn corrupt(detail: impl Into<String>) -> Error {
    Error::corrupt("ext inode", detail)
}

impl Inode {
    /// Decode an inode record (`raw` is `s_inode_size` bytes, at least 128).
    pub fn parse(ino: u32, raw: &[u8], sb: &Superblock) -> Result<Inode> {
        if raw.len() < 128 {
            return Err(corrupt(format!("inode {ino}: only {} bytes", raw.len())));
        }
        let r32 = |o: usize| u32le(raw, o).unwrap_or(0);
        let r16 = |o: usize| u16le(raw, o).unwrap_or(0);
        let mut extra = 0u16;
        if raw.len() > 128 {
            let e = r16(0x80);
            if e % 4 == 0 && 128 + e as usize <= raw.len() {
                extra = e;
            }
        }
        let has = |off: usize, len: usize| off + len <= 128 + extra as usize;
        let ext = |off: usize| if has(off, 4) { Some(r32(off)) } else { None };
        let ts = |secs: u32, extra_off: usize| Timestamp::from_ext4(secs, ext(extra_off));
        let mode = r16(0x00);
        let ftype = FileType::from_mode(mode);
        let size_lo = r32(0x04) as u64;
        let size_hi = r32(0x6C) as u64;
        let size = if ftype == FileType::Directory && !sb.has_incompat(INCOMPAT_LARGEDIR) {
            size_lo
        } else {
            size_lo | (size_hi << 32)
        };
        let blocks_hi = if sb.has_ro_compat(RO_COMPAT_HUGE_FILE) {
            (r16(0x74) as u64) << 32
        } else {
            0
        };
        let mut i_block = [0u8; 60];
        i_block.copy_from_slice(&raw[0x28..0x64]);
        let dtime_raw = r32(0x14);
        let crtime = if has(0x90, 4) {
            Timestamp::from_ext4(r32(0x90), ext(0x94))
        } else {
            None
        };
        Ok(Inode {
            ino,
            mode,
            uid: r16(0x02) as u32 | (r16(0x78) as u32) << 16,
            gid: r16(0x18) as u32 | (r16(0x7A) as u32) << 16,
            size,
            atime: ts(r32(0x08), 0x8C),
            ctime: ts(r32(0x0C), 0x84),
            mtime: ts(r32(0x10), 0x88),
            crtime,
            dtime: Timestamp::from_ext4(dtime_raw, None),
            dtime_raw,
            links_count: r16(0x1A),
            blocks: r32(0x1C) as u64 | blocks_hi,
            flags: r32(0x20),
            i_block,
            generation: r32(0x64),
            file_acl: r32(0x68) as u64 | (r16(0x76) as u64) << 32,
            extra_isize: extra,
            raw: raw.to_vec(),
        })
    }

    /// The raw on-disk record.
    pub fn raw(&self) -> &[u8] {
        &self.raw
    }

    /// File type from the mode bits.
    pub fn file_type(&self) -> FileType {
        FileType::from_mode(self.mode)
    }

    /// True for directories.
    pub fn is_dir(&self) -> bool {
        self.file_type() == FileType::Directory
    }

    /// Uses an extent tree.
    pub fn has_extents(&self) -> bool {
        self.flags & iflags::EXTENTS != 0
    }

    /// Stores its data inline.
    pub fn has_inline_data(&self) -> bool {
        self.flags & iflags::INLINE_DATA != 0
    }

    /// Hash-indexed directory.
    pub fn is_htree(&self) -> bool {
        self.flags & iflags::INDEX != 0
    }

    /// fscrypt-encrypted.
    pub fn is_encrypted(&self) -> bool {
        self.flags & iflags::ENCRYPT != 0
    }

    /// `i_blocks` in 512-byte units.
    pub fn blocks_512(&self, block_size: u32) -> u64 {
        if self.flags & iflags::HUGE_FILE != 0 {
            self.blocks.saturating_mul(block_size as u64 / 512)
        } else {
            self.blocks
        }
    }

    /// A symlink whose target is stored in `i_block` (target shorter than
    /// 60 bytes and no data blocks).
    pub fn is_fast_symlink(&self, block_size: u32) -> bool {
        if self.file_type() != FileType::Symlink || self.has_inline_data() || self.size >= 60 {
            return false;
        }
        if self.has_extents() {
            // A slow symlink with extents has a valid root header with entries.
            let magic = u16le(&self.i_block, 0).unwrap_or(0);
            let entries = u16le(&self.i_block, 2).unwrap_or(0);
            return !(magic == 0xF30A && entries > 0);
        }
        let acl_blocks = if self.file_acl != 0 {
            block_size as u64 / 512
        } else {
            0
        };
        self.blocks_512(block_size) <= acl_blocks
    }

    /// Value of an extended attribute stored in the inode body (after
    /// `128 + i_extra_isize`, magic 0xEA020000). `index` is the name index
    /// (7 = `system.`), `name` the suffix (`b"data"`). Values stored in
    /// separate EA inodes are not followed.
    pub fn xattr(&self, index: u8, name: &[u8]) -> Option<&[u8]> {
        let raw = &self.raw;
        if raw.len() <= 128 {
            return None;
        }
        let start = 128 + self.extra_isize as usize;
        if u32le(raw, start)? != 0xEA02_0000 {
            return None;
        }
        let first = start + 4;
        let end = raw.len();
        let mut p = first;
        // Each entry is at least 16 bytes, so this bounds the loop.
        for _ in 0..(end.saturating_sub(first) / 16 + 1) {
            if p + 4 > end || u32le(raw, p)? == 0 {
                return None;
            }
            let name_len = *raw.get(p)? as usize;
            let name_index = *raw.get(p + 1)?;
            let value_offs = u16le(raw, p + 2)? as usize;
            let value_inum = u32le(raw, p + 4)?;
            let value_size = u32le(raw, p + 8)? as usize;
            let ename = slice(raw, p + 16, name_len)?;
            if name_index == index && ename == name {
                if value_inum != 0 {
                    return None;
                }
                return slice(raw, first + value_offs, value_size);
            }
            p += (16 + name_len + 3) & !3;
        }
        None
    }

    /// The two parts of inline data: `i_block` (60 bytes) and the
    /// `system.data` attribute (absent when empty or unreadable).
    pub fn inline_parts(&self) -> (&[u8; 60], Option<&[u8]>) {
        (&self.i_block, self.xattr(7, b"data"))
    }

    /// Inline file content, clipped to `size`. May be shorter than `size`
    /// when the `system.data` attribute is missing or short.
    pub fn inline_data(&self) -> Vec<u8> {
        let size = self.size.min(usize::MAX as u64) as usize;
        let (head, tail) = self.inline_parts();
        let mut v = head[..size.min(60)].to_vec();
        if size > 60 {
            if let Some(t) = tail {
                v.extend_from_slice(&t[..t.len().min(size - 60)]);
            }
        }
        v
    }

    /// Fast symlink target bytes (clipped to `size`).
    pub fn fast_symlink_target(&self) -> &[u8] {
        &self.i_block[..(self.size as usize).min(60)]
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::superblock::tests::synth;

    pub(crate) fn sb() -> Superblock {
        Superblock::parse(&synth(4096, 1)).unwrap()
    }

    #[test]
    fn decode_fields_and_times() {
        let mut raw = vec![0u8; 256];
        raw[0..2].copy_from_slice(&0x81A4u16.to_le_bytes());
        raw[4..8].copy_from_slice(&1234u32.to_le_bytes());
        raw[0x10..0x14].copy_from_slice(&1_700_000_000u32.to_le_bytes());
        raw[0x14..0x18].copy_from_slice(&1_700_000_100u32.to_le_bytes());
        raw[0x1A..0x1C].copy_from_slice(&1u16.to_le_bytes());
        raw[0x80..0x82].copy_from_slice(&32u16.to_le_bytes());
        raw[0x88..0x8C].copy_from_slice(&(500u32 << 2).to_le_bytes());
        raw[0x90..0x94].copy_from_slice(&1_600_000_000u32.to_le_bytes());
        let i = Inode::parse(12, &raw, &sb()).unwrap();
        assert_eq!(i.file_type(), FileType::Regular);
        assert_eq!(i.size, 1234);
        assert_eq!(i.mtime.unwrap().unix_secs, 1_700_000_000);
        assert_eq!(i.mtime.unwrap().nanos, 500);
        assert_eq!(i.crtime.unwrap().unix_secs, 1_600_000_000);
        assert_eq!(i.dtime.unwrap().unix_secs, 1_700_000_100);
        assert_eq!(i.extra_isize, 32);
        // Bad extra_isize: extended fields ignored.
        raw[0x80..0x82].copy_from_slice(&250u16.to_le_bytes());
        let i = Inode::parse(12, &raw, &sb()).unwrap();
        assert_eq!(i.extra_isize, 0);
        assert!(i.crtime.is_none());
        assert!(Inode::parse(1, &raw[..100], &sb()).is_err());
    }

    #[test]
    fn inline_data_with_xattr() {
        let mut raw = vec![0u8; 256];
        raw[0..2].copy_from_slice(&0x81A4u16.to_le_bytes());
        raw[4..8].copy_from_slice(&70u32.to_le_bytes());
        raw[0x20..0x24].copy_from_slice(&iflags::INLINE_DATA.to_le_bytes());
        for (k, b) in raw[0x28..0x64].iter_mut().enumerate() {
            *b = k as u8;
        }
        raw[0x80..0x82].copy_from_slice(&32u16.to_le_bytes());
        let start = 128 + 32;
        raw[start..start + 4].copy_from_slice(&0xEA02_0000u32.to_le_bytes());
        let e = start + 4;
        raw[e] = 4; // name_len
        raw[e + 1] = 7; // system.
        raw[e + 2..e + 4].copy_from_slice(&40u16.to_le_bytes()); // value offset
        raw[e + 8..e + 12].copy_from_slice(&10u32.to_le_bytes()); // value size
        raw[e + 16..e + 20].copy_from_slice(b"data");
        let v = e + 40;
        raw[v..v + 10].copy_from_slice(b"0123456789");
        let i = Inode::parse(12, &raw, &sb()).unwrap();
        assert_eq!(i.xattr(7, b"data"), Some(&b"0123456789"[..]));
        assert_eq!(i.xattr(7, b"nope"), None);
        let d = i.inline_data();
        assert_eq!(d.len(), 70);
        assert_eq!(&d[60..], b"0123456789");
        // Value offset pointing outside the inode: rejected, no panic.
        raw[e + 2..e + 4].copy_from_slice(&5000u16.to_le_bytes());
        let i = Inode::parse(12, &raw, &sb()).unwrap();
        assert_eq!(i.xattr(7, b"data"), None);
        assert_eq!(i.inline_data().len(), 60);
    }

    #[test]
    fn fast_symlink_detection() {
        let mut raw = vec![0u8; 256];
        raw[0..2].copy_from_slice(&0xA1FFu16.to_le_bytes());
        raw[4..8].copy_from_slice(&5u32.to_le_bytes());
        raw[0x28..0x2D].copy_from_slice(b"a.txt");
        let i = Inode::parse(13, &raw, &sb()).unwrap();
        assert!(i.is_fast_symlink(4096));
        assert_eq!(i.fast_symlink_target(), b"a.txt");
        raw[0x1C..0x20].copy_from_slice(&8u32.to_le_bytes()); // has a data block
        assert!(!Inode::parse(13, &raw, &sb()).unwrap().is_fast_symlink(4096));
    }
}
