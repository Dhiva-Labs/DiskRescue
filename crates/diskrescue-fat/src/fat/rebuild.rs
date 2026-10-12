//! Reconstruction of a destroyed FAT boot sector (filesystems spec 2.2.10).
//!
//! FAT12/16 volumes have no backup boot sector, so a card whose sector 0
//! is unreadable or overwritten ("the card needs formatting") would lose
//! every name. The layout can be recovered from what survives:
//!
//! 1. **FAT copies.** Every FAT starts with the media byte and two 0xFF
//!    bytes (`F8 FF FF`, `F0 FF FF`). The sectors after sector 0 are
//!    scanned for that signature; two such sectors whose contents are
//!    identical (or, for a stale second copy, share FAT\[0\]/FAT\[1\] and
//!    most following sectors) are FAT1 and FAT2. Their distance is the FAT
//!    size, the first one's sector index is the reserved sector count, and
//!    the number of FATs is 2. A single FAT cannot be told apart from its
//!    surroundings reliably, so volumes with one FAT are not rebuilt.
//! 2. **Hypotheses.** For each FAT width (12, 16; 32 when the FAT32 backup
//!    boot sector is gone too), each root directory size (FAT12/16: 1 to 256
//!    sectors) and each cluster size (1 to 128 sectors), the data area start
//!    and cluster count follow from the Microsoft rules.
//! 3. **Evidence.** The root directory (right after the FATs; cluster 2 on
//!    FAT32) is parsed for entries, and each hypothesis is scored by:
//!    subdirectories whose first cluster holds a `.` entry pointing at
//!    itself (strong; camera cards always have `DCIM`); live files whose FAT
//!    chain length equals `ceil(size / cluster size)`; media files whose
//!    first cluster starts with a photo/video signature; a FAT size that
//!    matches what a formatter would allocate for the resulting cluster
//!    count; and the width the Microsoft cluster-count rule gives.
//! 4. The best hypothesis with at least one positive piece of directory or
//!    file evidence becomes a synthetic boot sector (512-byte sectors, the
//!    found reserved count, two FATs, the found FAT size, root entries and
//!    cluster size), which then goes through the normal validation.
//!
//! Every value derived here is reported in the volume warnings. Limits: one
//! FAT, a first FAT more than 16 MiB into the volume, or a volume with no
//! files, directories or media at all cannot be reconstructed.

use std::sync::Arc;

use diskrescue_core::util::bytes::{u16le, u32le};

use super::boot::{BootSector, Geometry};
use super::dir::{dot_kind, short_display, ATTR_DIRECTORY, ATTR_LONG_NAME, ATTR_VOLUME_ID};
use crate::io::MetaReader;
use crate::table::{FatTable, FatWidth};
use crate::util::{is_media_name, looks_like_media};
use diskrescue_core::fs::FsKind;

/// Sector size the reconstruction works in (also the synthetic BPB's).
const UNIT: u64 = 512;
/// The first FAT must start within this many sectors (16 MiB).
const MAX_FAT_START: u64 = 32_768;
/// Largest FAT size considered, in sectors (64 MiB: FAT32 up to ~16M
/// clusters).
const MAX_FAT_UNITS: u64 = 131_072;
/// Most FAT signature sectors remembered.
const MAX_CANDIDATES: usize = 2048;
/// Most entries of each kind used as evidence.
const MAX_DIRS: usize = 8;
const MAX_MEDIA: usize = 8;
const MAX_CHAINS: usize = 8;
/// Most FAT copy comparisons (each reads up to 64 KiB).
const MAX_COMPARISONS: usize = 256;
/// Longest chain followed for the chain-length evidence.
const MAX_CHAIN_STEPS: u64 = 1 << 20;

/// One directory entry used as evidence.
#[derive(Debug, Clone)]
struct Item {
    cluster: u32,
    size: u32,
    is_dir: bool,
    deleted: bool,
    media: bool,
}

/// Entries of the directory at the start of `bytes`, up to the first 0x00
/// entry.
fn scan_entries(bytes: &[u8], fat32: bool) -> Vec<Item> {
    let mut out = Vec::new();
    for e in bytes.chunks_exact(32).take(4096) {
        let b0 = e[0];
        if b0 == 0 {
            break;
        }
        let attr = e[11];
        if attr == ATTR_LONG_NAME || attr & ATTR_VOLUME_ID != 0 || attr & 0xC0 != 0 {
            continue;
        }
        if dot_kind(e).is_some() || b0 == b' ' {
            continue;
        }
        let lo = u16le(e, 26).unwrap_or(0) as u32;
        let hi = if fat32 {
            u16le(e, 20).unwrap_or(0) as u32
        } else {
            0
        };
        let cluster = (hi << 16) | lo;
        if cluster < 2 {
            continue;
        }
        let mut short = [0u8; 11];
        short.copy_from_slice(&e[..11]);
        let name = short_display(&short, e[12], true);
        out.push(Item {
            cluster,
            size: u32le(e, 28).unwrap_or(0),
            is_dir: attr & ATTR_DIRECTORY != 0,
            deleted: b0 == 0xE5,
            media: is_media_name(&name),
        });
    }
    out
}

/// True for the first bytes of a FAT: media byte, then 0xFF 0xFF.
fn fat_signature(b: &[u8]) -> bool {
    b.len() >= 512
        && (b[0] == 0xF0 || b[0] >= 0xF8)
        && b[1] == 0xFF
        && b[2] == 0xFF
        && !b.iter().all(|&x| x == 0xFF)
}

/// Read one 512-byte unit, `None` when unreadable or past the end.
fn unit(meta: &MetaReader, u: u64) -> Option<Vec<u8>> {
    meta.read_uncached(u * UNIT, UNIT as usize)
        .filter(|v| v.len() == UNIT as usize)
}

/// Sectors (index, first 512 bytes) that start like a FAT.
fn fat_candidates(meta: &MetaReader, units_total: u64) -> Vec<(u64, Vec<u8>)> {
    let end = units_total.min(MAX_FAT_START + MAX_FAT_UNITS + 1);
    let chunk = 2048u64;
    let mut out = Vec::new();
    let mut u = 1u64;
    while u < end && out.len() < MAX_CANDIDATES {
        let n = chunk.min(end - u);
        match meta.read_uncached(u * UNIT, (n * UNIT) as usize) {
            Some(buf) if buf.len() as u64 == n * UNIT => {
                for (k, b) in buf.chunks_exact(UNIT as usize).enumerate() {
                    if fat_signature(b) && out.len() < MAX_CANDIDATES {
                        out.push((u + k as u64, b.to_vec()));
                    }
                }
            }
            _ => {
                for k in 0..n {
                    if let Some(b) = unit(meta, u + k) {
                        if fat_signature(&b) && out.len() < MAX_CANDIDATES {
                            out.push((u + k, b));
                        }
                    }
                }
            }
        }
        u += n;
    }
    out
}

/// Fraction of the first sectors of the two FAT copies that are equal
/// (unreadable pairs are skipped). Returns (equal, compared).
fn copy_similarity(meta: &MetaReader, s0: u64, d: u64) -> (u64, u64) {
    let k = d.min(64);
    let a = meta.read_uncached(s0 * UNIT, (k * UNIT) as usize);
    let b = meta.read_uncached((s0 + d) * UNIT, (k * UNIT) as usize);
    let (mut eq, mut n) = (0u64, 0u64);
    match (a, b) {
        (Some(a), Some(b)) if a.len() == b.len() => {
            for (x, y) in a
                .chunks_exact(UNIT as usize)
                .zip(b.chunks_exact(UNIT as usize))
            {
                n += 1;
                eq += (x == y) as u64;
            }
        }
        _ => {
            for i in 0..k {
                if let (Some(x), Some(y)) = (unit(meta, s0 + i), unit(meta, s0 + d + i)) {
                    n += 1;
                    eq += (x == y) as u64;
                }
            }
        }
    }
    (eq, n)
}

/// Candidate `(FAT1 sector, FAT size)` pairs, best first.
fn fat_pairs(meta: &MetaReader, units_total: u64) -> Vec<(u64, u64)> {
    let cands = fat_candidates(meta, units_total);
    let mut exact = Vec::new();
    let mut loose = Vec::new();
    let mut budget = MAX_COMPARISONS;
    for (i, (s0, b0)) in cands.iter().enumerate() {
        if *s0 > MAX_FAT_START || budget == 0 {
            break;
        }
        for (s1, b1) in &cands[i + 1..] {
            let d = s1 - s0;
            if d > MAX_FAT_UNITS {
                break;
            }
            if s0 + 2 * d >= units_total || budget == 0 {
                break;
            }
            if b0 == b1 {
                budget -= 1;
                let (eq, n) = copy_similarity(meta, *s0, d);
                if n > 0 && eq * 10 >= n * 9 {
                    exact.push((*s0, d));
                }
            } else if b0[..4] == b1[..4] {
                // A stale second copy still shares FAT[0], FAT[1] and most
                // of the table.
                budget -= 1;
                let (eq, n) = copy_similarity(meta, *s0, d);
                if n >= 4 && eq * 2 >= n {
                    loose.push((*s0, d));
                }
            }
            if exact.len() + loose.len() >= 8 {
                break;
            }
        }
        if exact.len() + loose.len() >= 8 {
            break;
        }
    }
    exact.extend(loose);
    exact
}

/// Cluster-count rule.
fn width_for(count: u64) -> u32 {
    if count < 4085 {
        12
    } else if count < 65525 {
        16
    } else {
        32
    }
}

fn fat_width(bits: u32) -> FatWidth {
    match bits {
        12 => FatWidth::Fat12,
        16 => FatWidth::Fat16,
        _ => FatWidth::Fat32,
    }
}

/// Entries a FAT of `units` sectors holds at `bits` per entry.
fn fat_entries(units: u64, bits: u32) -> u64 {
    units * UNIT * 8 / bits as u64
}

/// The winning hypothesis.
#[derive(Debug, Clone, Copy)]
struct Hypothesis {
    bits: u32,
    spc: u64,
    root_units: u64,
    data_start: u64,
    count: u64,
    score: i64,
    evidence: u32,
}

/// Score the data-area-dependent evidence of one hypothesis.
fn place_score(
    meta: &MetaReader,
    items: &[Item],
    data_start: u64,
    spc: u64,
    max_cluster: u64,
    fat32: bool,
) -> (i64, u32) {
    let mut score = 0i64;
    let mut ev = 0u32;
    let at = |c: u32| data_start + (c as u64 - 2) * spc;
    for it in items.iter().filter(|i| i.is_dir).take(MAX_DIRS) {
        if it.cluster as u64 > max_cluster {
            score -= 2;
            continue;
        }
        let Some(h) = meta.read_uncached(at(it.cluster) * UNIT, 64) else {
            continue;
        };
        if h.len() < 64 {
            score -= 2;
            continue;
        }
        let lo = u16le(&h, 26).unwrap_or(0) as u32;
        let hi = if fat32 {
            u16le(&h, 20).unwrap_or(0) as u32
        } else {
            0
        };
        if dot_kind(&h[..32]) == Some(1) && (hi << 16 | lo) == it.cluster {
            score += 6;
            ev += 1;
            // A root child's ".." points at the root (cluster 0).
            if !it.deleted && dot_kind(&h[32..64]) == Some(2) && u16le(&h, 58) == Some(0) {
                score += 1;
            }
        } else {
            score -= 2;
        }
    }
    for it in items
        .iter()
        .filter(|i| !i.is_dir && i.media && i.size > 0)
        .take(MAX_MEDIA)
    {
        if it.cluster as u64 > max_cluster {
            score -= 1;
            continue;
        }
        let Some(h) = meta.read_uncached(at(it.cluster) * UNIT, 512) else {
            continue;
        };
        if looks_like_media(&h) {
            score += 3;
            ev += 1;
        } else if !it.deleted {
            score -= 1;
        }
    }
    (score, ev)
}

/// Chain lengths (in clusters) of live files read with a FAT width, paired
/// with their sizes. Chains that do not end cleanly are left out.
fn chain_lengths(
    meta: &Arc<MetaReader>,
    items: &[Item],
    s0: u64,
    d: u64,
    bits: u32,
) -> Vec<(u64, u64)> {
    let entries = fat_entries(d, bits).min(u32::MAX as u64);
    if entries < 3 {
        return Vec::new();
    }
    let table = FatTable::new(
        meta.clone(),
        fat_width(bits),
        vec![s0 * UNIT, (s0 + d) * UNIT],
        d * UNIT,
        (entries - 1) as u32,
    );
    items
        .iter()
        .filter(|i| !i.is_dir && !i.deleted && i.size > 0)
        .take(MAX_CHAINS)
        .filter_map(|it| {
            // 512-byte clusters bound the useful length.
            let limit = (it.size as u64).div_ceil(UNIT) + 1;
            let ch = table.follow(it.cluster, limit.min(MAX_CHAIN_STEPS), false);
            (!ch.broken).then_some((ch.clusters, it.size as u64))
        })
        .collect()
}

/// Score the FAT size against the cluster count a formatter would size it
/// for.
fn fat_size_score(d: u64, bits: u32, count_from_size: u64) -> i64 {
    let entries = fat_entries(d, bits);
    if entries < 3 {
        return -100;
    }
    let count = count_from_size.min(entries - 2);
    let needed = ((count + 2) * bits as u64).div_ceil(8 * UNIT);
    if count_from_size > entries - 2 {
        // The source is larger than the FAT can describe: a partition or
        // device bigger than the volume. Plausible, no bonus.
        return 0;
    }
    let slack = (d / 64).max(2);
    if needed <= d && d <= needed + slack {
        4
    } else if d <= needed * 2 {
        0
    } else {
        -3
    }
}

/// Try to rebuild the geometry of a FAT volume whose boot sector (and, for
/// FAT32, backup boot sector) is gone. Returns the synthetic boot sector,
/// its geometry and the warnings describing what was inferred.
pub(super) fn reconstruct(
    meta: &Arc<MetaReader>,
    size: u64,
) -> Option<(BootSector, Geometry, Vec<String>)> {
    let units_total = size / UNIT;
    if units_total < 16 {
        return None;
    }
    for (s0, d) in fat_pairs(meta, units_total).into_iter().take(4) {
        if let Some(r) = try_pair(meta, units_total, s0, d) {
            return Some(r);
        }
    }
    None
}

fn try_pair(
    meta: &Arc<MetaReader>,
    units_total: u64,
    s0: u64,
    d: u64,
) -> Option<(BootSector, Geometry, Vec<String>)> {
    let root_start = s0 + 2 * d;
    let mut best: Option<Hypothesis> = None;
    let mut consider = |h: Hypothesis| {
        let better = match &best {
            None => true,
            Some(b) => (h.score, h.evidence) > (b.score, b.evidence),
        };
        if better {
            best = Some(h);
        }
    };
    // FAT12/16: fixed root directory after the FATs.
    let root_bytes = meta
        .read_uncached(root_start * UNIT, (256 * UNIT) as usize)
        .or_else(|| meta.read_uncached(root_start * UNIT, (32 * UNIT) as usize))
        .unwrap_or_default();
    let items16 = scan_entries(&root_bytes, false);
    for bits in [12u32, 16] {
        let chains = chain_lengths(meta, &items16, s0, d, bits);
        for spc_shift in 0..=7u32 {
            let spc = 1u64 << spc_shift;
            let cs = spc * UNIT;
            let mut chain_score = 0i64;
            let mut chain_ev = 0u32;
            for &(len, size) in &chains {
                if size.div_ceil(cs) == len {
                    chain_score += 3;
                    chain_ev += 1;
                } else {
                    chain_score -= 3;
                }
            }
            for root_units in 1..=256u64 {
                let data_start = root_start + root_units;
                if data_start >= units_total {
                    break;
                }
                let count_src = (units_total - data_start) / spc;
                let entries = fat_entries(d, bits);
                if count_src == 0 || entries < 3 {
                    continue;
                }
                let mut count = count_src.min(entries - 2);
                if width_for(count) != bits {
                    // A source larger than the volume (a partition or device
                    // bigger than what the FAT describes) still fits the
                    // width; any other mismatch contradicts the hypothesis.
                    let max_for = if bits == 12 { 4084 } else { 65_524 };
                    if count_src > entries - 2 && count > max_for {
                        count = max_for;
                    } else {
                        continue;
                    }
                }
                let (ps, pe) = place_score(meta, &items16, data_start, spc, count + 1, false);
                let mut score = chain_score + ps + fat_size_score(d, bits, count_src);
                if root_units == 32 {
                    // RootEntCnt 512: what every formatter uses for
                    // non-floppy media.
                    score += 1;
                }
                consider(Hypothesis {
                    bits,
                    spc,
                    root_units,
                    data_start,
                    count,
                    score,
                    evidence: chain_ev + pe,
                });
            }
        }
    }
    // FAT32: the root is a cluster chain, normally starting at cluster 2
    // right at the data area.
    let data_start = root_start;
    if data_start < units_total {
        let root_bytes = meta
            .read_uncached(data_start * UNIT, (128 * UNIT) as usize)
            .unwrap_or_default();
        let items32 = scan_entries(&root_bytes, true);
        let chains = chain_lengths(meta, &items32, s0, d, 32);
        for spc_shift in 0..=7u32 {
            let spc = 1u64 << spc_shift;
            let cs = spc * UNIT;
            let count_src = (units_total - data_start) / spc;
            let entries = fat_entries(d, 32);
            if count_src == 0 || entries < 3 {
                continue;
            }
            let count = count_src.min(entries - 2);
            let mut score = 0i64;
            let mut ev = 0u32;
            for &(len, size) in &chains {
                if size.div_ceil(cs) == len {
                    score += 3;
                    ev += 1;
                } else {
                    score -= 3;
                }
            }
            let (ps, pe) = place_score(meta, &items32, data_start, spc, count + 1, true);
            score += ps + fat_size_score(d, 32, count_src);
            if width_for(count) != 32 {
                score -= 4;
            }
            consider(Hypothesis {
                bits: 32,
                spc,
                root_units: 0,
                data_start,
                count,
                score,
                evidence: ev + pe,
            });
        }
    }
    let h = best?;
    if h.evidence == 0 || h.score <= 0 {
        log::debug!("FAT reconstruction at sector {s0}: no convincing hypothesis ({h:?})");
        return None;
    }
    let fat0 = unit(meta, s0)?;
    let tot = h.data_start + h.count * h.spc;
    if s0 > u16::MAX as u64 || tot > u32::MAX as u64 {
        return None;
    }
    let fat32 = h.bits == 32;
    let mut b = vec![0u8; 512];
    b[0..3].copy_from_slice(if fat32 {
        &[0xEB, 0x58, 0x90]
    } else {
        &[0xEB, 0x3C, 0x90]
    });
    b[3..11].copy_from_slice(b"REBUILT ");
    b[0x0B..0x0D].copy_from_slice(&(UNIT as u16).to_le_bytes());
    b[0x0D] = h.spc as u8;
    b[0x0E..0x10].copy_from_slice(&(s0 as u16).to_le_bytes());
    b[0x10] = 2;
    b[0x15] = fat0[0];
    if fat32 {
        b[0x20..0x24].copy_from_slice(&(tot as u32).to_le_bytes());
        b[0x24..0x28].copy_from_slice(&(d as u32).to_le_bytes());
        b[0x2C..0x30].copy_from_slice(&2u32.to_le_bytes());
    } else {
        if h.root_units * 16 > u16::MAX as u64 || d > u16::MAX as u64 {
            return None;
        }
        b[0x11..0x13].copy_from_slice(&((h.root_units * 16) as u16).to_le_bytes());
        if tot < 0x10000 {
            b[0x13..0x15].copy_from_slice(&(tot as u16).to_le_bytes());
        } else {
            b[0x20..0x24].copy_from_slice(&(tot as u32).to_le_bytes());
        }
        b[0x16..0x18].copy_from_slice(&(d as u16).to_le_bytes());
    }
    b[0x1FE] = 0x55;
    b[0x1FF] = 0xAA;
    let bs = BootSector::parse(&b)?;
    let geo = bs.geometry().ok()?;
    let want = match h.bits {
        12 => FsKind::Fat12,
        16 => FsKind::Fat16,
        _ => FsKind::Fat32,
    };
    if geo.kind != want {
        return None;
    }
    let mut notes = vec![format!(
        "boot sector reconstructed from the FAT copies and the directory structure: FAT{} with \
         {} reserved sectors, 2 FATs of {} sectors, {}{}-byte clusters, {} clusters (evidence \
         score {}, {} matching entries)",
        h.bits,
        s0,
        d,
        if fat32 {
            String::new()
        } else {
            format!("{} root entries, ", h.root_units * 16)
        },
        h.spc * UNIT,
        h.count,
        h.score,
        h.evidence
    )];
    if (tot as u128) < units_total as u128 {
        notes.push(format!(
            "the reconstructed volume ends at sector {tot}, before the end of the source"
        ));
    }
    notes.push(
        "volume label, serial number and the exact sector count were lost with the boot sector"
            .to_string(),
    );
    Some((bs, geo, notes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures() {
        let mut b = vec![0u8; 512];
        b[..3].copy_from_slice(&[0xF8, 0xFF, 0xFF]);
        assert!(fat_signature(&b));
        b[0] = 0xF0;
        assert!(fat_signature(&b));
        b[0] = 0xF1;
        assert!(!fat_signature(&b));
        assert!(!fat_signature(&[0xFFu8; 512]));
        assert!(!fat_signature(&[0xF8, 0xFF, 0xFF]));
        assert_eq!(width_for(4084), 12);
        assert_eq!(width_for(4085), 16);
        assert_eq!(width_for(65_525), 32);
        assert_eq!(fat_entries(1, 12), 341);
        assert_eq!(fat_entries(1, 16), 256);
    }

    #[test]
    fn fat_size_matches_formatter() {
        // FAT16 with 32 sectors per FAT holds 8192 entries.
        assert!(fat_size_score(32, 16, 8000) > 0);
        assert!(fat_size_score(32, 16, 1000) < 0);
        assert_eq!(fat_size_score(32, 16, 100_000), 0);
    }
}
