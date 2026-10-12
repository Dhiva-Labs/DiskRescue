//! Directory block parsing, including deleted-name recovery from slack.
//!
//! A directory block is a chain of `ext4_dir_entry_2` records linked by
//! `rec_len`. Entries never cross a block. Unlink does not erase an entry:
//! the previous entry's `rec_len` grows to swallow it (or, for the first
//! entry of a block, the inode field is zeroed), so the old inode number,
//! name and type survive until the slot is reused. Kernels since 5.14 also
//! wipe the name and inode of deleted entries, so on recently written
//! volumes this recovers fewer names; volumes written by older kernels,
//! e2fsprogs/debugfs or other tools keep them.
//!
//! Slack scanning: inside every entry, the bytes between the end of its
//! name (`8 + name_len` rounded up to 4) and the end of its `rec_len` are
//! scanned in 4-byte steps (entries are always 4-byte aligned) for a
//! plausible hidden entry: inode in `1..=inodes_count`, `name_len > 0`,
//! the name fits, `rec_len` sane and inside the block, a valid file type
//! (with `filetype`), and a name without NUL, `/` or control characters.
//!
//! Hash-tree directories need no hashing: their leaf blocks are ordinary
//! linear blocks. Block 0 (dx_root) holds only `.` and `..`, whose slack is
//! the hash index (not scanned), and interior dx_node blocks start with a
//! fake empty entry spanning the block (skipped entirely). With
//! `metadata_csum` each block ends with a 12-byte checksum tail entry
//! (inode 0, rec_len 12, name_len 0, type 0xDE), which is skipped.

use diskrescue_core::util::bytes::{u16le, u32le};

/// One directory entry as found on disk.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RawDirent {
    /// Inode number.
    pub inode: u32,
    /// Raw name bytes.
    pub name: Vec<u8>,
    /// `file_type` byte (0 when the volume has no `filetype` feature).
    pub file_type: u8,
    /// Found in slack: the entry was deleted.
    pub hidden: bool,
}

/// How to parse directory blocks.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DirParseOpts {
    /// `INCOMPAT_FILETYPE`: name_len is one byte followed by the type.
    pub filetype: bool,
    /// Highest valid inode number.
    pub inodes_count: u32,
    /// Look for deleted entries in slack space.
    pub scan_slack: bool,
}

/// Decode an on-disk `rec_len` for a block of `block_len` bytes
/// (kernel `ext4_rec_len_from_disk`).
pub(crate) fn decode_rec_len(raw: u16, block_len: usize) -> usize {
    if raw == 0xFFFF || raw == 0 {
        block_len
    } else {
        (raw as usize & 0xFFFC) | ((raw as usize & 3) << 16)
    }
}

fn round4(n: usize) -> usize {
    (n + 3) & !3
}

fn name_plausible(name: &[u8]) -> bool {
    !name.is_empty()
        && name != b"."
        && name != b".."
        && name.iter().all(|&c| c >= 0x20 && c != b'/' && c != 0x7F)
}

/// True for an htree interior node: a fake entry with inode 0 and no name
/// spanning the whole block.
pub(crate) fn is_dx_node(block: &[u8]) -> bool {
    let len = block.len();
    u32le(block, 0) == Some(0)
        && u16le(block, 4).map(|r| decode_rec_len(r, len)) == Some(len)
        && block.get(6) == Some(&0)
}

/// Parse one directory block (or inline-data region) and append its
/// entries to `out`. Returns false when the chain was corrupt (the rest of
/// the block was then salvaged as slack when scanning is enabled).
pub(crate) fn parse_block(block: &[u8], opts: DirParseOpts, out: &mut Vec<RawDirent>) -> bool {
    let len = block.len();
    let mut pos = 0usize;
    let mut clean = true;
    // Each step advances by at least 8 bytes, so this bounds the loop.
    while pos + 8 <= len {
        let (Some(ino), Some(raw_rec)) = (u32le(block, pos), u16le(block, pos + 4)) else {
            break;
        };
        let rec = decode_rec_len(raw_rec, len);
        let (name_len, ftype) = if opts.filetype {
            (block[pos + 6] as usize, block[pos + 7])
        } else {
            (u16le(block, pos + 6).unwrap_or(0) as usize, 0)
        };
        // Checksum tail.
        if ino == 0 && rec == 12 && block[pos + 6] == 0 && block[pos + 7] == 0xDE && pos + 12 == len
        {
            break;
        }
        if rec < 8 || rec % 4 != 0 || pos + rec > len || 8 + name_len > rec {
            clean = false;
            if opts.scan_slack {
                scan_slack(block, pos, len, opts, out);
            }
            break;
        }
        if ino != 0 && name_len > 0 {
            if ino <= opts.inodes_count {
                out.push(RawDirent {
                    inode: ino,
                    name: block[pos + 8..pos + 8 + name_len].to_vec(),
                    file_type: ftype,
                    hidden: false,
                });
            } else {
                clean = false;
            }
        }
        if opts.scan_slack {
            let s = pos + 8 + round4(name_len);
            let e = pos + rec;
            if s + 8 < e {
                scan_slack(block, s, e, opts, out);
            }
        }
        pos += rec;
    }
    clean
}

/// Look for plausible deleted entries in `block[start..end]`.
fn scan_slack(
    block: &[u8],
    start: usize,
    end: usize,
    opts: DirParseOpts,
    out: &mut Vec<RawDirent>,
) {
    let mut p = round4(start);
    while p + 8 < end {
        match plausible_at(block, p, end, opts) {
            Some(d) => {
                p += 8 + round4(d.name.len());
                out.push(d);
            }
            None => p += 4,
        }
    }
}

fn plausible_at(block: &[u8], p: usize, end: usize, opts: DirParseOpts) -> Option<RawDirent> {
    let ino = u32le(block, p)?;
    if ino < 2 || ino > opts.inodes_count {
        return None;
    }
    let rec = decode_rec_len(u16le(block, p + 4)?, block.len());
    let (name_len, ftype) = if opts.filetype {
        (*block.get(p + 6)? as usize, *block.get(p + 7)?)
    } else {
        let n = u16le(block, p + 6)? as usize;
        if n > 255 {
            return None;
        }
        (n, 0)
    };
    if name_len == 0 || p + 8 + name_len > end {
        return None;
    }
    if rec < 8 + name_len || rec % 4 != 0 || p + rec > block.len() {
        return None;
    }
    if opts.filetype && !(1..=7).contains(&ftype) {
        return None;
    }
    let name = &block[p + 8..p + 8 + name_len];
    if !name_plausible(name) {
        return None;
    }
    Some(RawDirent {
        inode: ino,
        name: name.to_vec(),
        file_type: ftype,
        hidden: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPTS: DirParseOpts = DirParseOpts {
        filetype: true,
        inodes_count: 1000,
        scan_slack: true,
    };

    fn put(b: &mut [u8], pos: usize, ino: u32, rec: u16, name: &[u8], ft: u8) {
        b[pos..pos + 4].copy_from_slice(&ino.to_le_bytes());
        b[pos + 4..pos + 6].copy_from_slice(&rec.to_le_bytes());
        b[pos + 6] = name.len() as u8;
        b[pos + 7] = ft;
        b[pos + 8..pos + 8 + name.len()].copy_from_slice(name);
    }

    #[test]
    fn live_and_deleted_entries() {
        let mut b = vec![0u8; 1024];
        put(&mut b, 0, 2, 12, b".", 2);
        put(&mut b, 12, 2, 12, b"..", 2);
        // big.bin swallowed del.txt (16 bytes) and gone (12 bytes).
        put(&mut b, 24, 12, 44, b"big.bin", 1);
        put(&mut b, 40, 13, 28, b"del.txt", 1);
        put(&mut b, 56, 14, 12, b"gone", 2);
        put(&mut b, 68, 16, 1024 - 68 - 12, b"keep.txt", 1);
        // Checksum tail.
        put(&mut b, 1012, 0, 12, b"", 0xDE);
        let mut out = Vec::new();
        assert!(parse_block(&b, OPTS, &mut out));
        let names: Vec<(String, bool)> = out
            .iter()
            .map(|d| (String::from_utf8_lossy(&d.name).into_owned(), d.hidden))
            .collect();
        assert_eq!(
            names,
            vec![
                (".".into(), false),
                ("..".into(), false),
                ("big.bin".into(), false),
                ("del.txt".into(), true),
                ("gone".into(), true),
                ("keep.txt".into(), false),
            ]
        );
        assert_eq!(out[3].inode, 13);
        assert_eq!(out[4].file_type, 2);
        // Without slack scanning only live entries.
        let mut live = Vec::new();
        parse_block(
            &b,
            DirParseOpts {
                scan_slack: false,
                ..OPTS
            },
            &mut live,
        );
        assert_eq!(live.len(), 4);
    }

    #[test]
    fn rec_len_decoding_and_dx_node() {
        assert_eq!(decode_rec_len(12, 4096), 12);
        assert_eq!(decode_rec_len(0xFFFF, 65536), 65536);
        assert_eq!(decode_rec_len(0, 65536), 65536);
        let mut b = vec![0u8; 4096];
        b[4..6].copy_from_slice(&4096u16.to_le_bytes());
        assert!(is_dx_node(&b));
        b[6] = 3;
        assert!(!is_dx_node(&b));
    }

    #[test]
    fn corrupt_chain_is_salvaged() {
        let mut b = vec![0u8; 1024];
        put(&mut b, 0, 20, 16, b"a.jpg", 1);
        // Bad rec_len (not a multiple of 4) followed by a real-looking entry.
        put(&mut b, 16, 21, 13, b"bad", 1);
        put(&mut b, 32, 22, 1024 - 32, b"later.png", 1);
        let mut out = Vec::new();
        assert!(!parse_block(&b, OPTS, &mut out));
        assert_eq!(out[0].name, b"a.jpg");
        assert!(out.iter().any(|d| d.name == b"later.png" && d.hidden));
    }

    #[test]
    fn hostile_blocks_do_not_panic() {
        let mut seed = 99u64;
        for round in 0..3000 {
            let len = [12usize, 56, 1024, 4096][round % 4];
            let mut b = vec![0u8; len];
            for x in b.iter_mut() {
                seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                *x = (seed >> 56) as u8;
            }
            let mut out = Vec::new();
            parse_block(&b, OPTS, &mut out);
            parse_block(
                &b,
                DirParseOpts {
                    filetype: false,
                    ..OPTS
                },
                &mut out,
            );
            for d in &out {
                assert!(d.inode >= 1 && d.inode <= 1000);
            }
        }
    }
}
