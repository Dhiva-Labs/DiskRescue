//! Free space: the block bitmaps turned into volume byte ranges.
//!
//! Bit `i` of group `g`'s block bitmap (LSB first) is block
//! `first_data_block + g * blocks_per_group + i`; a clear bit is free. A
//! group with a trusted `BLOCK_UNINIT` flag has no valid bitmap: it is free
//! except for its superblock/GDT copy (classic or `meta_bg` layout, the same
//! rule libext2fs uses) and any group's bitmaps or inode table located
//! inside it. A group whose bitmap is unreadable, or whose descriptor is
//! invalid, is treated the same way so the carver still scans it (better
//! to look at allocated blocks than to miss free ones). Blocks before
//! `first_data_block` (the 1 KiB boot block) and beyond `blocks_count` are
//! never reported.

use diskrescue_core::{Range, RangeSet};

use crate::superblock::features::INCOMPAT_META_BG;
use crate::ExtFs;

impl ExtFs {
    /// Superblock and descriptor-table blocks of group `g` as block ranges.
    pub(crate) fn super_and_gdt_blocks(&self, g: u32) -> Vec<(u64, u64)> {
        let sb = &self.sb;
        let gb = sb.group_first_block(g);
        let has_super = sb.group_has_super(g);
        let dpb = sb.descs_per_block() as u64;
        let desc_blocks = (sb.group_count() as u64).div_ceil(dpb);
        let meta_bg = sb.has_incompat(INCOMPAT_META_BG);
        let old_desc_blocks = if meta_bg {
            sb.first_meta_bg as u64
        } else {
            desc_blocks + sb.reserved_gdt_blocks as u64
        };
        let mut out = Vec::new();
        if has_super {
            out.push((gb, gb + 1));
        }
        let meta_group = g as u64 / dpb;
        if !meta_bg || meta_group < sb.first_meta_bg as u64 {
            if has_super && old_desc_blocks > 0 {
                out.push((gb + 1, gb + 1 + old_desc_blocks));
            }
        } else {
            let r = g as u64 % dpb;
            if r == 0 || r == 1 || r == dpb - 1 {
                let b = gb + has_super as u64;
                out.push((b, b + 1));
            }
        }
        out
    }

    /// Compute [`diskrescue_core::fs::FileSystem::unallocated`].
    pub(crate) fn compute_unallocated(&self) -> RangeSet {
        let sb = &self.sb;
        let bs = sb.block_size as u64;
        let bpg = sb.blocks_per_group as u64;
        let bc = sb.blocks_count;
        let itb = sb.inode_table_blocks();
        // Bitmaps and inode tables of every group (flex_bg puts them in
        // other groups), in block units.
        let mut meta = RangeSet::new();
        for d in self.groups.iter().filter(|d| d.valid) {
            meta.insert(Range::with_len(d.block_bitmap, 1));
            meta.insert(Range::with_len(d.inode_bitmap, 1));
            meta.insert(Range::with_len(d.inode_table, itb));
        }
        let mut free: Vec<Range> = Vec::new();
        let mut layout_groups = 0u32;
        for g in 0..sb.group_count() {
            let start = sb.group_first_block(g);
            if start >= bc {
                break;
            }
            let end = (start + bpg).min(bc);
            let d = self.groups.get(g as usize);
            let bitmap = match d {
                Some(d) if d.valid && !(self.flags_trusted(d) && d.block_uninit()) => {
                    let got = d
                        .block_bitmap
                        .checked_mul(bs)
                        .and_then(|o| self.read_meta(o, bpg.div_ceil(8) as usize).ok());
                    if got.is_none() {
                        log::warn!(
                            "block bitmap of group {g} unreadable; treating the group as free"
                        );
                        layout_groups += 1;
                    }
                    got
                }
                Some(d) if d.valid => None,
                _ => {
                    layout_groups += 1;
                    None
                }
            };
            match bitmap {
                Some(bm) => scan_bitmap(&bm, start, (end - start) as usize, &mut free),
                None => {
                    let mut set = RangeSet::from_range(Range::new(start, end));
                    for (a, b) in self.super_and_gdt_blocks(g) {
                        set.remove(Range::new(a, b));
                    }
                    for r in meta.intersection_with_range(Range::new(start, end)).iter() {
                        set.remove(r);
                    }
                    free.extend(set.iter());
                }
            }
        }
        if layout_groups > 0 {
            log::warn!(
                "{layout_groups} group(s) without a usable block bitmap counted as free (except metadata)"
            );
        }
        free.into_iter()
            .map(|r| Range::new(r.start.saturating_mul(bs), r.end.saturating_mul(bs)))
            .collect()
    }
}

/// Append runs of clear bits among the first `nbits` bits of `bm` (block
/// `base + i` for bit `i`). Missing bitmap bytes count as allocated.
fn scan_bitmap(bm: &[u8], base: u64, nbits: usize, out: &mut Vec<Range>) {
    let mut run: Option<usize> = None;
    let push = |s: usize, e: usize, out: &mut Vec<Range>| {
        out.push(Range::new(base + s as u64, base + e as u64));
    };
    let mut i = 0usize;
    while i < nbits {
        let byte = bm.get(i / 8).copied().unwrap_or(0xFF);
        if i % 8 == 0 && i + 8 <= nbits && (byte == 0 || byte == 0xFF) {
            if byte == 0 {
                run.get_or_insert(i);
            } else if let Some(s) = run.take() {
                push(s, i, out);
            }
            i += 8;
            continue;
        }
        if byte & (1 << (i % 8)) == 0 {
            run.get_or_insert(i);
        } else if let Some(s) = run.take() {
            push(s, i, out);
        }
        i += 1;
    }
    if let Some(s) = run {
        push(s, nbits, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitmap_runs() {
        let mut out = Vec::new();
        // bits: 0..8 used, 8..16 free, 16 used, 17..20 free (nbits 20)
        scan_bitmap(&[0xFF, 0x00, 0x01, 0x00], 100, 20, &mut out);
        assert_eq!(out, vec![Range::new(108, 116), Range::new(117, 120)]);
        out.clear();
        scan_bitmap(&[0x00], 0, 16, &mut out);
        assert_eq!(out, vec![Range::new(0, 8)]);
    }
}
