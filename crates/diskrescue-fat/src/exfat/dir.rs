//! exFAT directory entry sets.
//!
//! A file is a set of 32-byte entries: one File entry (0x85), one Stream
//! Extension (0xC0) and 1..17 File Name entries (0xC1), optionally followed
//! by benign secondaries. Deleting clears bit 7 of every entry in the set
//! (0x05/0x40/0x41) and touches nothing else, so a deleted set still
//! verifies against its stored SetChecksum once bit 7 is restored.
//!
//! Acceptance rules: the set must be structurally complete (stream entry,
//! enough name entries for NameLength, consistent in-use bits). A live set
//! with a checksum mismatch is still listed with a note; a deleted set with
//! a checksum mismatch is listed only when its NameHash verifies (otherwise
//! its slots were reused and the bytes are not trustworthy).

use diskrescue_core::time::Timestamp;
use diskrescue_core::util::bytes::{u16le, u32le, u64le};
use diskrescue_core::Range;

use crate::common::DirData;
use crate::util::{exfat_name_hash, exfat_set_checksum};

/// Directory attribute bit of FileAttributes.
pub(crate) const ATTR_DIRECTORY: u16 = 0x10;

/// One decoded entry set.
#[derive(Debug, Clone)]
pub(crate) struct SetItem {
    /// Volume offset of the File entry (unique id).
    pub id: u64,
    /// The set is deleted (InUse clear) or lies past the end marker.
    pub deleted: bool,
    /// FileAttributes.
    pub attrs: u16,
    /// File name.
    pub name: String,
    /// GeneralSecondaryFlags of the stream extension.
    pub flags: u8,
    /// ValidDataLength.
    pub valid_data_length: u64,
    /// FirstCluster.
    pub first_cluster: u32,
    /// DataLength.
    pub data_length: u64,
    /// Creation time.
    pub created: Option<Timestamp>,
    /// Last modification time.
    pub modified: Option<Timestamp>,
    /// Last access time.
    pub accessed: Option<Timestamp>,
    /// Notes for the report.
    pub notes: Vec<String>,
}

impl SetItem {
    /// Directory flag.
    pub(crate) fn is_dir(&self) -> bool {
        self.attrs & ATTR_DIRECTORY != 0
    }

    /// AllocationPossible bit.
    pub(crate) fn allocation_possible(&self) -> bool {
        self.flags & 0x01 != 0
    }

    /// NoFatChain bit.
    pub(crate) fn no_fat_chain(&self) -> bool {
        self.flags & 0x02 != 0
    }
}

/// Parse result of one directory.
#[derive(Debug, Default)]
pub(crate) struct ParsedDir {
    pub items: Vec<SetItem>,
    pub notes: Vec<String>,
}

/// Up-case a UTF-16 unit with the volume table (identity when absent or
/// out of range; ASCII fallback when no table was loaded).
pub(crate) fn upcase_unit(table: Option<&[u16]>, u: u16) -> u16 {
    match table {
        Some(t) => t.get(u as usize).copied().unwrap_or(u),
        None => {
            if (b'a' as u16..=b'z' as u16).contains(&u) {
                u - 32
            } else {
                u
            }
        }
    }
}

/// Outcome of trying to read a set at entry index `i`.
enum SetParse {
    /// A set occupying `count` entries.
    Item(Box<SetItem>, usize),
    /// Not a usable set; the reason is noted for live sets.
    Skip(String),
}

/// Checksum of the set with bit 7 forced on in every entry type byte.
fn verify_checksum(set: &[u8]) -> bool {
    let stored = u16le(set, 2).unwrap_or(0);
    let mut copy = set.to_vec();
    for k in (0..copy.len()).step_by(32) {
        copy[k] |= 0x80;
    }
    exfat_set_checksum(&copy) == stored
}

fn parse_set(d: &DirData, i: usize, n: usize, upcase: Option<&[u16]>, past_end: bool) -> SetParse {
    let bytes = &d.bytes;
    let e0 = &bytes[i * 32..i * 32 + 32];
    let in_use = e0[0] & 0x80 != 0;
    let bit = if in_use { 0x80u8 } else { 0 };
    let sc = e0[1] as usize;
    if !(2..=18).contains(&sc) {
        return SetParse::Skip(format!("SecondaryCount {sc} out of range"));
    }
    if i + 1 + sc > n {
        return SetParse::Skip("entry set runs past the end of the directory".into());
    }
    let span = Range::new((i * 32) as u64, ((i + 1 + sc) * 32) as u64);
    if d.bad.intersects(span) {
        return SetParse::Skip("entry set lies partly in an unreadable sector".into());
    }
    let set = &bytes[i * 32..(i + 1 + sc) * 32];
    let stream = &set[32..64];
    if stream[0] != (0x40 | bit) {
        return SetParse::Skip("stream extension entry missing".into());
    }
    let name_len = stream[3] as usize;
    if name_len == 0 {
        return SetParse::Skip("empty name".into());
    }
    let name_entries = name_len.div_ceil(15);
    if 1 + name_entries > sc {
        return SetParse::Skip("not enough file name entries for NameLength".into());
    }
    for k in 0..name_entries {
        if set[(2 + k) * 32] != (0x41 | bit) {
            return SetParse::Skip("file name entry missing".into());
        }
    }
    for k in (1 + name_entries)..sc {
        let t = set[(1 + k) * 32];
        if t & 0x80 != bit || t & 0x40 == 0 {
            return SetParse::Skip("unexpected entry inside the set".into());
        }
    }
    let mut units: Vec<u16> = Vec::with_capacity(name_entries * 15);
    for k in 0..name_entries {
        let ne = &set[(2 + k) * 32..(3 + k) * 32];
        for j in 0..15 {
            units.push(u16::from_le_bytes([ne[2 + 2 * j], ne[3 + 2 * j]]));
        }
    }
    units.truncate(name_len);
    let up: Vec<u16> = units.iter().map(|&u| upcase_unit(upcase, u)).collect();
    let hash_ok = exfat_name_hash(&up) == u16le(stream, 4).unwrap_or(0);
    let checksum_ok = verify_checksum(set);
    let mut notes = Vec::new();
    if !checksum_ok {
        if !in_use && !hash_ok {
            return SetParse::Skip(String::new());
        }
        notes.push("entry set checksum mismatch; entry may be damaged".to_string());
    }
    if !hash_ok && upcase.is_some() {
        notes.push("name hash mismatch".to_string());
    }
    if past_end {
        notes.push("found past the end-of-directory marker".to_string());
    }
    let ts = |off: usize, ms: Option<usize>, tz: usize| {
        Timestamp::from_exfat(
            u32le(e0, off).unwrap_or(0),
            ms.map(|m| e0[m]).unwrap_or(0),
            e0[tz],
        )
    };
    let dl = u64le(stream, 24).unwrap_or(0);
    let mut vdl = u64le(stream, 8).unwrap_or(0);
    if vdl > dl {
        notes.push(format!(
            "ValidDataLength {vdl} exceeds DataLength {dl}; using DataLength"
        ));
        vdl = dl;
    }
    let attrs = u16le(e0, 4).unwrap_or(0);
    let item = SetItem {
        id: d.vol_offset((i * 32) as u64),
        deleted: !in_use || past_end,
        attrs,
        name: String::from_utf16_lossy(&units),
        flags: stream[1],
        valid_data_length: vdl,
        first_cluster: u32le(stream, 20).unwrap_or(0),
        data_length: dl,
        created: ts(8, Some(20), 22),
        modified: ts(12, Some(21), 23),
        accessed: ts(16, None, 24),
        notes,
    };
    SetParse::Item(Box::new(item), 1 + sc)
}

/// True when `head` contains at least one complete entry set (live or
/// deleted) whose checksum verifies: the exFAT directory heuristic.
pub(crate) fn sniff_dir(head: &[u8]) -> bool {
    let n = head.len() / 32;
    for i in 0..n {
        let e = &head[i * 32..i * 32 + 32];
        if e[0] & 0x7F != 0x05 {
            continue;
        }
        let sc = e[1] as usize;
        if !(2..=18).contains(&sc) || i + 1 + sc > n {
            continue;
        }
        let bit = e[0] & 0x80;
        let set = &head[i * 32..(i + 1 + sc) * 32];
        if set[32] != (0x40 | bit) || set[64] != (0x41 | bit) {
            continue;
        }
        if verify_checksum(set) {
            return true;
        }
    }
    false
}

/// Parse all entry sets of a directory.
pub(crate) fn parse_dir(d: &DirData, upcase: Option<&[u16]>) -> ParsedDir {
    let mut out = ParsedDir::default();
    let n = d.bytes.len() / 32;
    let mut i = 0usize;
    let mut past_end = false;
    let mut bad_noted = false;
    while i < n {
        let rel = (i * 32) as u64;
        if let Some(r) = d.bad.first_intersecting(Range::with_len(rel, 32)) {
            if !bad_noted {
                out.notes.push(
                    "part of the directory is unreadable; entries stored there are lost"
                        .to_string(),
                );
                bad_noted = true;
            }
            i = (r.end as usize).div_ceil(32).max(i + 1);
            continue;
        }
        let t = d.bytes[i * 32];
        if t == 0x00 {
            // End of directory, unless a later sector still holds sets.
            let mut pos = ((i * 32) / 512 + 1) * 512;
            let mut resume = None;
            while pos + 96 <= d.bytes.len() {
                let end = (pos + 512 + 608).min(d.bytes.len());
                if !d.bad.intersects(Range::new(pos as u64, end as u64))
                    && sniff_dir(&d.bytes[pos..end])
                {
                    resume = Some(pos / 32);
                    break;
                }
                pos += 512;
            }
            match resume {
                Some(j) if j > i => {
                    i = j;
                    past_end = true;
                    continue;
                }
                _ => break,
            }
        }
        if t & 0x7F == 0x05 {
            match parse_set(d, i, n, upcase, past_end) {
                SetParse::Item(item, count) => {
                    out.items.push(*item);
                    i += count;
                    continue;
                }
                SetParse::Skip(reason) => {
                    if t & 0x80 != 0 && !reason.is_empty() {
                        out.notes.push(format!(
                            "damaged entry set at directory offset {rel} skipped: {reason}"
                        ));
                    }
                }
            }
        }
        i += 1;
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use diskrescue_core::RangeSet;

    /// Build an entry set (live) for `name` with the given stream fields.
    pub(crate) fn make_set(
        name: &str,
        attrs: u16,
        flags: u8,
        first: u32,
        len: u64,
        vdl: u64,
    ) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let ne = units.len().div_ceil(15);
        let mut set = vec![0u8; 32 * (2 + ne)];
        set[0] = 0x85;
        set[1] = (1 + ne) as u8;
        set[4..6].copy_from_slice(&attrs.to_le_bytes());
        let date: u32 = (44 << 9) | (5 << 5) | 1;
        let time: u32 = (12 << 11) | (30 << 5);
        let packed = (date << 16) | time;
        for off in [8usize, 12, 16] {
            set[off..off + 4].copy_from_slice(&packed.to_le_bytes());
        }
        set[32] = 0xC0;
        set[33] = flags;
        set[35] = units.len() as u8;
        let up: Vec<u16> = units.iter().map(|&u| upcase_unit(None, u)).collect();
        set[36..38].copy_from_slice(&exfat_name_hash(&up).to_le_bytes());
        set[40..48].copy_from_slice(&vdl.to_le_bytes());
        set[52..56].copy_from_slice(&first.to_le_bytes());
        set[56..64].copy_from_slice(&len.to_le_bytes());
        for k in 0..ne {
            let e = &mut set[64 + 32 * k..96 + 32 * k];
            e[0] = 0xC1;
            for (j, &u) in units.iter().skip(15 * k).take(15).enumerate() {
                e[2 + 2 * j..4 + 2 * j].copy_from_slice(&u.to_le_bytes());
            }
        }
        let cs = exfat_set_checksum(&set);
        set[2..4].copy_from_slice(&cs.to_le_bytes());
        set
    }

    fn dir(bytes: Vec<u8>) -> DirData {
        let mut bytes = bytes;
        bytes.resize(4096, 0);
        let len = bytes.len() as u64;
        DirData {
            bytes,
            segments: vec![(0, 1 << 20, len)],
            bad: RangeSet::new(),
        }
    }

    #[test]
    fn live_and_deleted_sets() {
        let mut b = make_set("Holiday Photo 2024 exfat.jpg", 0x20, 3, 6, 100_000, 100_000);
        let mut del = make_set("photo2.png", 0x20, 3, 40, 5000, 5000);
        for k in (0..del.len()).step_by(32) {
            del[k] &= 0x7F;
        }
        b.extend_from_slice(&del);
        // A deleted set whose bytes were damaged: rejected.
        let mut junk = make_set("gone.bin", 0x20, 3, 50, 10, 10);
        for k in (0..junk.len()).step_by(32) {
            junk[k] &= 0x7F;
        }
        junk[70] ^= 0xFF; // corrupt a name character
        b.extend_from_slice(&junk);
        let p = parse_dir(&dir(b.clone()), None);
        assert_eq!(p.items.len(), 2);
        assert_eq!(p.items[0].name, "Holiday Photo 2024 exfat.jpg");
        assert!(!p.items[0].deleted);
        assert!(p.items[0].no_fat_chain());
        assert_eq!(p.items[1].name, "photo2.png");
        assert!(p.items[1].deleted);
        assert_eq!(p.items[1].first_cluster, 40);
        assert_eq!(p.items[0].id, (1 << 20) as u64);
        let mut head = b.clone();
        head.resize(512, 0);
        assert!(sniff_dir(&head));
        assert!(!sniff_dir(&[0u8; 512]));
    }

    #[test]
    fn live_checksum_mismatch_is_noted() {
        let mut b = make_set("a.txt", 0x20, 1, 0, 0, 0);
        b[2] ^= 1;
        let p = parse_dir(&dir(b), None);
        assert_eq!(p.items.len(), 1);
        assert!(p.items[0].notes[0].contains("checksum"));
    }

    #[test]
    fn long_name_255() {
        let name: String = std::iter::repeat_n('x', 255).collect();
        let b = make_set(&name, 0x20, 3, 9, 1, 1);
        assert_eq!(b.len(), 19 * 32);
        let p = parse_dir(&dir(b), None);
        assert_eq!(p.items[0].name.len(), 255);
    }
}
