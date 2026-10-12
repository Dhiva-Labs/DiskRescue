//! FAT directory entries: 32-byte short (8.3) entries and long-name (LFN)
//! entries.
//!
//! Live names: LFN entries are collected in order (sequence numbers
//! descending, the first one carrying the 0x40 "last" flag) and accepted
//! only when the run is complete and every entry's checksum equals the
//! checksum of the following short entry. Otherwise the run is an orphan:
//! it is reported in the entry's notes and the 8.3 name is used, with the
//! NT lower-case flags (byte 0x0C bits 3/4) applied.
//!
//! Deleted names: a delete overwrites byte 0 of the short entry and of each
//! LFN entry with 0xE5, so the LFN sequence numbers and the short name's
//! first character are lost. The run of deleted LFN entries physically
//! preceding the short entry is reassembled by adjacency. Because the
//! checksum covers the lost first byte, exactly one first byte reproduces
//! it; the run is accepted when that byte is a plausible short-name
//! character (and matches the long name's first letter when that is a
//! plain ASCII letter or digit) and the run is well formed (one shared
//! checksum, a 0x0000 terminator followed only by 0xFFFF padding). Without
//! an accepted run the 8.3 name is used with `_` as first character.
//!
//! End of directory: a 0x00 first byte ends the listing, except inside a
//! sector known to be unreadable (zero-filled), which is skipped, and except
//! when a later 512-byte block still passes the directory heuristic, in
//! which case parsing resumes there and those entries are reported as
//! deleted ("found past the end-of-directory marker").

use diskrescue_core::time::Timestamp;
use diskrescue_core::util::bytes::{u16le, u32le};
use diskrescue_core::Range;

pub(crate) use crate::common::DirData;
use crate::util::{decode_oem, lfn_checksum};

/// Read-only attribute.
pub(crate) const ATTR_READ_ONLY: u8 = 0x01;
/// Hidden attribute.
pub(crate) const ATTR_HIDDEN: u8 = 0x02;
/// System attribute.
pub(crate) const ATTR_SYSTEM: u8 = 0x04;
/// Volume label attribute.
pub(crate) const ATTR_VOLUME_ID: u8 = 0x08;
/// Directory attribute.
pub(crate) const ATTR_DIRECTORY: u8 = 0x10;
/// Long-name entry attribute combination.
pub(crate) const ATTR_LONG_NAME: u8 = 0x0F;

/// One decoded short entry with its (long) name.
#[derive(Debug, Clone)]
pub(crate) struct RawItem {
    /// Volume offset of the short entry (unique id).
    pub id: u64,
    /// Short name bytes (first byte restored when known).
    #[cfg_attr(not(test), allow(dead_code))]
    pub short: [u8; 11],
    /// Best name: long name, else decoded 8.3 name.
    pub name: String,
    /// DIR_Attr.
    pub attr: u8,
    /// DIR_FstClusHI.
    pub hi: u16,
    /// DIR_FstClusLO.
    pub lo: u16,
    /// DIR_FileSize.
    pub size: u32,
    /// Entry is marked deleted (or lies past the end-of-directory marker).
    pub deleted: bool,
    /// Creation time.
    pub created: Option<Timestamp>,
    /// Last write time.
    pub modified: Option<Timestamp>,
    /// Last access date.
    pub accessed: Option<Timestamp>,
    /// Notes for the report.
    pub notes: Vec<String>,
}

impl RawItem {
    /// First cluster; the high word only counts on FAT32.
    pub(crate) fn first_cluster(&self, fat32: bool) -> u32 {
        if fat32 {
            ((self.hi as u32) << 16) | self.lo as u32
        } else {
            self.lo as u32
        }
    }

    /// Directory flag.
    pub(crate) fn is_dir(&self) -> bool {
        self.attr & ATTR_DIRECTORY != 0
    }
}

/// Result of parsing one directory.
#[derive(Debug, Default)]
pub(crate) struct ParsedDir {
    /// Files and subdirectories (dot entries and labels excluded).
    pub items: Vec<RawItem>,
    /// Volume label entry, if present and live.
    pub label: Option<String>,
    /// Cluster recorded in the `.` entry.
    pub dot: Option<u32>,
    /// Cluster recorded in the `..` entry.
    pub dotdot: Option<u32>,
    /// Directory-level warnings.
    pub notes: Vec<String>,
}

/// Bytes never valid in a short name.
fn invalid_short_char(b: u8) -> bool {
    b < 0x20
        || matches!(
            b,
            0x22 | 0x2A
                | 0x2B
                | 0x2C
                | 0x2E
                | 0x2F
                | 0x3A
                | 0x3B
                | 0x3C
                | 0x3D
                | 0x3E
                | 0x3F
                | 0x5B
                | 0x5C
                | 0x5D
                | 0x7C
                | 0x7F
        )
}

/// True when `e` is the `.` or `..` entry; returns which.
pub(crate) fn dot_kind(e: &[u8]) -> Option<u8> {
    if e[11] & ATTR_DIRECTORY == 0 || e[11] == ATTR_LONG_NAME {
        return None;
    }
    if &e[0..11] == b".          " {
        Some(1)
    } else if &e[0..11] == b"..         " {
        Some(2)
    } else {
        None
    }
}

/// Structural plausibility of a short entry (used to drop junk). With
/// `strict`, reserved bits of DIR_NTRes must be clear too; live entries of a
/// directory reached through the tree are not dropped for those bits alone
/// (some implementations leave junk there, and dropping a live file is
/// worse than listing a strange one).
fn short_plausible(e: &[u8], deleted: bool, strict: bool) -> bool {
    if e[11] & 0xC0 != 0 {
        return false;
    }
    if strict && e[12] & !0x18 != 0 {
        return false;
    }
    let start = if deleted { 1 } else { 0 };
    for (i, &b) in e[..11].iter().enumerate().skip(start) {
        if i == 0 && b == 0x05 {
            continue;
        }
        if i == 0 && b == b' ' {
            return false;
        }
        if invalid_short_char(b) {
            return false;
        }
    }
    true
}

fn fat_date_ok(d: u16) -> bool {
    if d == 0 {
        return true;
    }
    let day = d & 0x1F;
    let month = (d >> 5) & 0x0F;
    let year = (d >> 9) & 0x7F;
    (1..=31).contains(&day) && (1..=12).contains(&month) && year < 120
}

fn fat_time_ok(t: u16) -> bool {
    (t & 0x1F) <= 29 && ((t >> 5) & 0x3F) <= 59 && (t >> 11) <= 23
}

/// What the directory heuristic concluded about a block.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Sniff {
    /// The block looks like directory entries.
    pub is_dir: bool,
    /// Cluster of a leading `.` entry.
    pub dot: Option<u32>,
    /// Cluster of a following `..` entry.
    pub dotdot: Option<u32>,
}

/// Directory heuristic (filesystems spec 2.2.9) over the first entries of
/// `head` (normally one sector of a cluster).
pub(crate) fn sniff_dir(head: &[u8], fat32: bool, max_cluster: u32) -> Sniff {
    let mut s = Sniff::default();
    let n = (head.len() / 32).min(16);
    if n == 0 {
        return s;
    }
    let cluster_of = |e: &[u8]| -> u32 {
        let lo = u16le(e, 26).unwrap_or(0) as u32;
        let hi = if fat32 {
            u16le(e, 20).unwrap_or(0) as u32
        } else {
            0
        };
        (hi << 16) | lo
    };
    // Leading "." and ".." entries are the strong signal.
    if n >= 2 && dot_kind(&head[0..32]) == Some(1) && dot_kind(&head[32..64]) == Some(2) {
        let d = cluster_of(&head[0..32]);
        let dd = cluster_of(&head[32..64]);
        if d >= 2 && d <= max_cluster && dd <= max_cluster {
            s.is_dir = true;
            s.dot = Some(d);
            s.dotdot = Some(dd);
            return s;
        }
        return s;
    }
    let mut shorts = 0usize;
    let mut lfn_ok = 0usize;
    let mut lfn_chk: Option<u8> = None;
    let mut lfn_matched = false;
    for i in 0..n.min(8) {
        let e = &head[i * 32..i * 32 + 32];
        let b0 = e[0];
        if b0 == 0 {
            break;
        }
        let attr = e[11];
        if attr == ATTR_LONG_NAME {
            let seq = b0 & 0x3F;
            let seq_ok = b0 == 0xE5 || (b0 & 0x80 == 0 && (1..=20).contains(&seq));
            if !seq_ok || e[12] != 0 || u16le(e, 26) != Some(0) {
                return s;
            }
            lfn_ok += 1;
            lfn_chk = Some(e[13]);
            continue;
        }
        let deleted = b0 == 0xE5;
        if !short_plausible(e, deleted, true) {
            return s;
        }
        if dot_kind(e).is_some() {
            return s;
        }
        let crt_time = u16le(e, 14).unwrap_or(0);
        let crt_date = u16le(e, 16).unwrap_or(0);
        let acc_date = u16le(e, 18).unwrap_or(0);
        let wrt_time = u16le(e, 22).unwrap_or(0);
        let wrt_date = u16le(e, 24).unwrap_or(0);
        if e[13] > 199
            || !fat_date_ok(crt_date)
            || !fat_date_ok(acc_date)
            || !fat_date_ok(wrt_date)
            || !fat_time_ok(crt_time)
            || !fat_time_ok(wrt_time)
        {
            return s;
        }
        if !deleted && wrt_date == 0 && attr & ATTR_VOLUME_ID == 0 {
            return s;
        }
        if !fat32 && u16le(e, 20) != Some(0) {
            return s;
        }
        let c = cluster_of(e);
        if c == 1 || c > max_cluster {
            return s;
        }
        let size = u32le(e, 28).unwrap_or(0);
        if attr & ATTR_DIRECTORY != 0 && size != 0 {
            return s;
        }
        if let Some(chk) = lfn_chk.take() {
            let mut short = [0u8; 11];
            short.copy_from_slice(&e[..11]);
            if !deleted && lfn_checksum(&short) == chk {
                lfn_matched = true;
            }
            if deleted {
                // The first byte is lost; the run is checked on parse.
                lfn_matched = true;
            }
        }
        shorts += 1;
    }
    s.is_dir = shorts >= 2 || (shorts >= 1 && lfn_ok >= 1 && lfn_matched);
    s
}

/// Live LFN run being collected.
#[derive(Default)]
struct LfnRun {
    parts: Vec<[u16; 13]>,
    expect: u8,
    chk: u8,
    active: bool,
}

fn lfn_units(e: &[u8]) -> [u16; 13] {
    let mut u = [0u16; 13];
    let offs = [1usize, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];
    for (k, &o) in offs.iter().enumerate() {
        u[k] = u16::from_le_bytes([e[o], e[o + 1]]);
    }
    u
}

/// Assemble name units from parts in name order; `None` when malformed
/// (data after the terminator that is not 0xFFFF padding, or empty).
fn assemble(parts_in_order: &[[u16; 13]]) -> Option<String> {
    let mut units: Vec<u16> = Vec::with_capacity(parts_in_order.len() * 13);
    let mut ended = false;
    for (pi, p) in parts_in_order.iter().enumerate() {
        for &u in p {
            if ended {
                if u != 0xFFFF {
                    return None;
                }
            } else if u == 0 {
                ended = true;
                if pi + 1 != parts_in_order.len() {
                    return None;
                }
            } else {
                units.push(u);
            }
        }
    }
    if units.is_empty() || units.len() > 255 {
        return None;
    }
    Some(String::from_utf16_lossy(&units))
}

impl LfnRun {
    fn reset(&mut self) -> bool {
        let had = self.active;
        *self = LfnRun::default();
        had
    }

    /// Feed one live LFN entry; returns true when an unfinished run was
    /// discarded.
    fn push(&mut self, e: &[u8]) -> bool {
        let ord = e[0];
        let seq = ord & 0x3F;
        let structural = e[12] == 0 && u16le(e, 26) == Some(0) && (1..=20).contains(&seq);
        if ord & 0x40 != 0 {
            let discarded = self.reset();
            if structural {
                self.active = true;
                self.expect = seq - 1;
                self.chk = e[13];
                self.parts.push(lfn_units(e));
            }
            return discarded;
        }
        if self.active && structural && seq == self.expect && e[13] == self.chk && seq >= 1 {
            self.parts.push(lfn_units(e));
            self.expect -= 1;
            false
        } else {
            self.reset();
            true
        }
    }

    /// Take the long name if the run is complete and matches `short`.
    /// `Err(())` means a run was present but did not match.
    fn take(&mut self, short: &[u8; 11]) -> Result<Option<String>, ()> {
        if !self.active {
            return Ok(None);
        }
        let ok = self.expect == 0 && self.chk == lfn_checksum(short);
        let parts: Vec<[u16; 13]> = self.parts.iter().rev().copied().collect();
        self.reset();
        if !ok {
            return Err(());
        }
        assemble(&parts).map(Some).ok_or(())
    }
}

/// Plausible short-name first byte for the deleted-name brute force.
fn plausible_first(b: u8) -> bool {
    b == 0x05 || (b > 0x20 && b != 0xE5 && !invalid_short_char(b) && !b.is_ascii_lowercase())
}

/// Try to rebuild a deleted entry's long name from the deleted LFN entries
/// at `lfn_idx` (physical order, nearest last). Returns the name and the
/// recovered first byte of the short name.
fn deleted_long_name(bytes: &[u8], lfn_idx: &[usize], short: &[u8; 11]) -> Option<(String, u8)> {
    if lfn_idx.is_empty() {
        return None;
    }
    let nearest = *lfn_idx.last()?;
    let chk = bytes[nearest * 32 + 13];
    // Nearest-first: name order.
    let mut parts: Vec<[u16; 13]> = Vec::new();
    for &i in lfn_idx.iter().rev().take(20) {
        let e = &bytes[i * 32..i * 32 + 32];
        if e[13] != chk || e[12] != 0 || u16le(e, 26) != Some(0) {
            break;
        }
        let u = lfn_units(e);
        let has_end = u.contains(&0);
        parts.push(u);
        if has_end {
            break;
        }
    }
    let name = assemble(&parts)?;
    // The unique first byte that reproduces the checksum.
    let mut cand = *short;
    let mut first = None;
    for b in 0..=255u8 {
        cand[0] = b;
        if lfn_checksum(&cand) == chk {
            first = Some(b);
            break;
        }
    }
    let b = first?;
    if !plausible_first(b) {
        return None;
    }
    // Any run of deleted LFN entries "matches" some first byte, so the
    // checksum alone proves nothing here; check that the short name could
    // have been generated from the long one. When the long name starts with
    // a plain letter/digit, the short name starts with its upper-case form.
    let lead = name.trim_start_matches(['.', ' ']).chars().next()?;
    if lead.is_ascii_alphanumeric() && lead.to_ascii_uppercase() as u8 != b {
        return None;
    }
    // A plain ASCII extension is kept (upper-cased, at most 3 characters)
    // by every short-name generator.
    if !name.starts_with('.') {
        let short_ext: Vec<u8> = short[8..11]
            .iter()
            .copied()
            .filter(|&c| c != b' ')
            .collect();
        match name.rsplit_once('.') {
            Some((_, ext)) if !ext.is_empty() && ext.bytes().all(|c| c.is_ascii_alphanumeric()) => {
                let want: Vec<u8> = ext
                    .bytes()
                    .take(3)
                    .map(|c| c.to_ascii_uppercase())
                    .collect();
                if short_ext != want {
                    return None;
                }
            }
            None if !short_ext.is_empty() => return None,
            _ => {}
        }
    }
    Some((name, b))
}

/// Decode an 8.3 name; `lower` = DIR_NTRes, applied only without an LFN.
pub(crate) fn short_display(short: &[u8; 11], nt_res: u8, apply_case: bool) -> String {
    let mut base_bytes = short[..8].to_vec();
    if base_bytes[0] == 0x05 {
        base_bytes[0] = 0xE5;
    }
    let mut base = decode_oem(&base_bytes).trim_end_matches(' ').to_string();
    let mut ext = decode_oem(&short[8..11]).trim_end_matches(' ').to_string();
    if apply_case {
        if nt_res & 0x08 != 0 {
            base = base.to_ascii_lowercase();
        }
        if nt_res & 0x10 != 0 {
            ext = ext.to_ascii_lowercase();
        }
    }
    if ext.is_empty() {
        base
    } else {
        format!("{base}.{ext}")
    }
}

/// Index of the next 512-byte block at or after entry `from` that passes
/// the directory heuristic.
fn find_resume(d: &DirData, from: usize, fat32: bool, max_cluster: u32) -> Option<usize> {
    let mut pos = (from * 32).div_ceil(512) * 512;
    if pos == from * 32 {
        pos += 512;
    }
    while pos + 64 <= d.bytes.len() {
        let end = (pos + 512).min(d.bytes.len());
        let blk = &d.bytes[pos..end];
        if !d.bad.intersects(Range::new(pos as u64, end as u64))
            && sniff_dir(blk, fat32, max_cluster).is_dir
        {
            return Some(pos / 32);
        }
        pos += 512;
    }
    None
}

/// Parse a whole directory.
pub(crate) fn parse_dir(d: &DirData, fat32: bool, max_cluster: u32) -> ParsedDir {
    let mut out = ParsedDir::default();
    let bytes = &d.bytes;
    let n = bytes.len() / 32;
    let mut i = 0usize;
    let mut run = LfnRun::default();
    let mut orphan_lfn = false;
    let mut del_lfn: Vec<usize> = Vec::new();
    let mut beyond = false;
    let mut junk = 0usize;
    let mut bad_noted = false;
    while i < n {
        let rel = i * 32;
        let e = &bytes[rel..rel + 32];
        if d.bad.intersects(Range::with_len(rel as u64, 32)) {
            if !bad_noted {
                out.notes.push(
                    "part of the directory is unreadable; entries stored there are lost"
                        .to_string(),
                );
                bad_noted = true;
            }
            // Skip the zero-filled range.
            let r = d
                .bad
                .first_intersecting(Range::with_len(rel as u64, 32))
                .map(|r| r.end)
                .unwrap_or(rel as u64 + 32);
            i = (r as usize).div_ceil(32).max(i + 1);
            run.reset();
            del_lfn.clear();
            continue;
        }
        let b0 = e[0];
        if b0 == 0x00 {
            match find_resume(d, i, fat32, max_cluster) {
                Some(j) if j > i => {
                    i = j;
                    beyond = true;
                    run.reset();
                    del_lfn.clear();
                    continue;
                }
                _ => break,
            }
        }
        let attr = e[11];
        if attr == ATTR_LONG_NAME {
            if b0 == 0xE5 {
                if run.reset() {
                    orphan_lfn = true;
                }
                del_lfn.push(i);
            } else {
                del_lfn.clear();
                if run.push(e) {
                    orphan_lfn = true;
                }
            }
            i += 1;
            continue;
        }
        let deleted = b0 == 0xE5 || beyond;
        let mut short = [0u8; 11];
        short.copy_from_slice(&e[..11]);
        if !deleted {
            if let Some(k) = dot_kind(e) {
                let lo = u16le(e, 26).unwrap_or(0) as u32;
                let hi = if fat32 {
                    u16le(e, 20).unwrap_or(0) as u32
                } else {
                    0
                };
                if k == 1 {
                    out.dot = Some((hi << 16) | lo);
                } else {
                    out.dotdot = Some((hi << 16) | lo);
                }
                if run.reset() {
                    orphan_lfn = true;
                }
                del_lfn.clear();
                i += 1;
                continue;
            }
        }
        if attr & ATTR_VOLUME_ID != 0 {
            if !deleted && attr & ATTR_DIRECTORY == 0 && out.label.is_none() {
                let l = decode_oem(&short).trim_end().to_string();
                if !l.is_empty() {
                    out.label = Some(l);
                }
            }
            run.reset();
            del_lfn.clear();
            i += 1;
            continue;
        }
        if !short_plausible(e, b0 == 0xE5, deleted) {
            junk += 1;
            run.reset();
            del_lfn.clear();
            i += 1;
            continue;
        }
        let mut notes = Vec::new();
        let nt_res = e[12];
        let name;
        if b0 == 0xE5 {
            run.reset();
            match deleted_long_name(bytes, &del_lfn, &short) {
                Some((long, first)) => {
                    short[0] = first;
                    name = long;
                }
                None => {
                    let mut s = short;
                    s[0] = b'_';
                    name = short_display(&s, nt_res, true);
                    notes.push("first character lost".to_string());
                }
            }
        } else {
            match run.take(&short) {
                Ok(Some(long)) => name = long,
                Ok(None) => name = short_display(&short, nt_res, true),
                Err(()) => {
                    orphan_lfn = true;
                    name = short_display(&short, nt_res, true);
                }
            }
            if orphan_lfn {
                notes.push(
                    "orphan long-name entries before this entry were ignored (checksum or sequence mismatch)"
                        .to_string(),
                );
                log::warn!(
                    "orphan long-name entries before {:?} ignored",
                    short_display(&short, nt_res, true)
                );
            }
        }
        orphan_lfn = false;
        del_lfn.clear();
        if beyond {
            notes.push("found past the end-of-directory marker".to_string());
        }
        let crt_time = u16le(e, 14).unwrap_or(0);
        let crt_date = u16le(e, 16).unwrap_or(0);
        let acc_date = u16le(e, 18).unwrap_or(0);
        let wrt_time = u16le(e, 22).unwrap_or(0);
        let wrt_date = u16le(e, 24).unwrap_or(0);
        let hi = u16le(e, 20).unwrap_or(0);
        if !fat32 && hi != 0 {
            notes.push(format!(
                "high cluster word {hi:#06x} set on a FAT12/16 volume; ignored"
            ));
        }
        out.items.push(RawItem {
            id: d.vol_offset(rel as u64),
            short,
            name,
            attr,
            hi,
            lo: u16le(e, 26).unwrap_or(0),
            size: u32le(e, 28).unwrap_or(0),
            deleted,
            created: Timestamp::from_fat(crt_date, crt_time, e[13]),
            modified: Timestamp::from_fat(wrt_date, wrt_time, 0),
            accessed: Timestamp::from_fat(acc_date, 0, 0),
            notes,
        });
        i += 1;
    }
    if junk > 0 {
        out.notes.push(format!(
            "{junk} implausible short entries skipped (corrupt directory data)"
        ));
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use diskrescue_core::RangeSet;

    /// Build a short entry.
    pub(crate) fn short_entry(name: &[u8; 11], attr: u8, cluster: u32, size: u32) -> [u8; 32] {
        let mut e = [0u8; 32];
        e[..11].copy_from_slice(name);
        e[11] = attr;
        let date: u16 = (44 << 9) | (5 << 5) | 1;
        let time: u16 = (12 << 11) | (30 << 5);
        e[14..16].copy_from_slice(&time.to_le_bytes());
        e[16..18].copy_from_slice(&date.to_le_bytes());
        e[18..20].copy_from_slice(&date.to_le_bytes());
        e[20..22].copy_from_slice(&((cluster >> 16) as u16).to_le_bytes());
        e[22..24].copy_from_slice(&time.to_le_bytes());
        e[24..26].copy_from_slice(&date.to_le_bytes());
        e[26..28].copy_from_slice(&(cluster as u16).to_le_bytes());
        e[28..32].copy_from_slice(&size.to_le_bytes());
        e
    }

    /// LFN entries (physical order) for `name` with checksum of `short`.
    pub(crate) fn lfn_entries(name: &str, short: &[u8; 11]) -> Vec<[u8; 32]> {
        let chk = lfn_checksum(short);
        let mut units: Vec<u16> = name.encode_utf16().collect();
        if units.len() % 13 != 0 {
            units.push(0);
            while units.len() % 13 != 0 {
                units.push(0xFFFF);
            }
        }
        let count = units.len() / 13;
        let offs = [1usize, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];
        let mut out = Vec::new();
        for seq in (1..=count).rev() {
            let mut e = [0u8; 32];
            e[0] = seq as u8 | if seq == count { 0x40 } else { 0 };
            e[11] = ATTR_LONG_NAME;
            e[13] = chk;
            for (k, &o) in offs.iter().enumerate() {
                let u = units[(seq - 1) * 13 + k];
                e[o..o + 2].copy_from_slice(&u.to_le_bytes());
            }
            out.push(e);
        }
        out
    }

    fn dir(entries: &[[u8; 32]]) -> DirData {
        let mut bytes = Vec::new();
        for e in entries {
            bytes.extend_from_slice(e);
        }
        bytes.resize(4096, 0);
        let len = bytes.len() as u64;
        DirData {
            bytes,
            segments: vec![(0, 100_000, len)],
            bad: RangeSet::new(),
        }
    }

    #[test]
    fn live_and_deleted_names() {
        let s1 = *b"HOLIDA~1JPG";
        let mut ents = lfn_entries("Holiday Photo 2024.jpg", &s1);
        ents.push(short_entry(&s1, 0x20, 5, 1000));
        let s2 = *b"README  TXT";
        let mut e2 = short_entry(&s2, 0x20, 9, 10);
        e2[12] = 0x18;
        ents.push(e2);
        let s3 = *b"PHOTO2~1PNG";
        let mut del = lfn_entries("photo2.png", &s3);
        for e in &mut del {
            e[0] = 0xE5;
        }
        ents.extend(del);
        let mut e3 = short_entry(&s3, 0x20, 12, 3000);
        e3[0] = 0xE5;
        ents.push(e3);
        let mut e4 = short_entry(b"NOLFN   BIN", 0x20, 20, 3);
        e4[0] = 0xE5;
        ents.push(e4);
        let p = parse_dir(&dir(&ents), false, 1000);
        let names: Vec<_> = p.items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "Holiday Photo 2024.jpg",
                "readme.txt",
                "photo2.png",
                "_OLFN.BIN"
            ]
        );
        assert!(!p.items[0].deleted);
        assert!(p.items[2].deleted);
        assert_eq!(p.items[2].short[0], b'P');
        assert_eq!(p.items[3].notes, vec!["first character lost".to_string()]);
        assert_eq!(p.items[0].id, 100_000 + 64);
        assert_eq!(
            p.items[0].modified.unwrap().to_rfc3339(),
            "2024-05-01T12:30:00Z"
        );
    }

    #[test]
    fn deleted_lfn_of_another_file_is_not_attached() {
        // The LFN run of "Holiday.jpg" survives in front of the deleted
        // short entry of a different, LFN-less file (its own short entry
        // slot was reused): the extension does not match, so the long name
        // must not be attached.
        let mut ents = lfn_entries("Holiday.jpg", b"HOLIDAY JPG");
        for e in &mut ents {
            e[0] = 0xE5;
        }
        // Pick a short name whose brute-forced first byte is 'H' with the
        // same checksum but a different extension.
        let chk = crate::util::lfn_checksum(b"HOLIDAY JPG");
        let mut found = None;
        'outer: for a in b'A'..=b'Z' {
            for c in b'A'..=b'Z' {
                for d in b'0'..=b'9' {
                    let mut cand = *b"HXXXXX~1MOV";
                    cand[1] = a;
                    cand[2] = c;
                    cand[5] = d;
                    if crate::util::lfn_checksum(&cand) == chk {
                        found = Some(cand);
                        break 'outer;
                    }
                }
            }
        }
        let short = found.expect("a colliding short name exists");
        let mut e = short_entry(&short, 0x20, 30, 100);
        e[0] = 0xE5;
        ents.push(e);
        let p = parse_dir(&dir(&ents), false, 1000);
        assert_eq!(p.items.len(), 1);
        assert!(p.items[0].name.starts_with('_'), "{}", p.items[0].name);
        assert!(p.items[0].name.ends_with(".MOV"));
    }

    #[test]
    fn live_entry_with_reserved_ntres_bits_is_kept() {
        let mut e = short_entry(b"IMG_0001JPG", 0x20, 5, 10);
        e[12] = 0x01;
        let p = parse_dir(&dir(&[e]), false, 1000);
        assert_eq!(p.items.len(), 1);
        assert_eq!(p.items[0].name, "IMG_0001.JPG");
    }

    #[test]
    fn orphan_lfn_is_reported() {
        let s1 = *b"FILE1   TXT";
        let mut ents = lfn_entries("a long file name.txt", b"OTHER   TXT");
        ents.push(short_entry(&s1, 0x20, 5, 1));
        let p = parse_dir(&dir(&ents), false, 1000);
        assert_eq!(p.items[0].name, "FILE1.TXT");
        assert!(p.items[0].notes[0].contains("orphan"));
    }

    #[test]
    fn dot_entries_label_and_sniff() {
        let ents = [
            short_entry(b".          ", 0x10, 7, 0),
            short_entry(b"..         ", 0x10, 0, 0),
            short_entry(b"A       TXT", 0x20, 8, 1),
        ];
        let d = dir(&ents);
        let p = parse_dir(&d, true, 1000);
        assert_eq!(p.dot, Some(7));
        assert_eq!(p.dotdot, Some(0));
        assert_eq!(p.items.len(), 1);
        let s = sniff_dir(&d.bytes[..512], true, 1000);
        assert!(s.is_dir);
        assert_eq!(s.dot, Some(7));
        let lab = [short_entry(b"MYCARD     ", 0x08, 0, 0)];
        assert_eq!(
            parse_dir(&dir(&lab), true, 10).label.as_deref(),
            Some("MYCARD")
        );
        // Random bytes are not a directory.
        let junk: Vec<u8> = (0..512u32).map(|i| (i * 37 % 251) as u8).collect();
        assert!(!sniff_dir(&junk, true, 1000).is_dir);
        assert!(!sniff_dir(&[0u8; 512], true, 1000).is_dir);
    }

    #[test]
    fn zero_sector_inside_directory_is_skipped_when_bad() {
        let mut ents = vec![short_entry(b"A       TXT", 0x20, 8, 1)];
        while ents.len() < 16 {
            ents.push([0u8; 32]);
        }
        ents.push(short_entry(b"B       TXT", 0x20, 9, 1));
        let mut d = dir(&ents);
        d.bad.insert(Range::new(32, 512));
        let p = parse_dir(&d, false, 100);
        assert_eq!(p.items.len(), 2);
        assert!(!p.notes.is_empty());
    }
}
