//! Finding NTFS volumes whose boot sectors are both destroyed.
//!
//! When sector 0 and the backup boot sector of an NTFS volume are gone the
//! volume can still be rebuilt from its MFT, because MFT record 0 (`$MFT`)
//! describes the MFT's own location in clusters and the other system
//! records give the remaining geometry:
//!
//! 1. The source is scanned in 512-byte steps for copies of record 0: a
//!    `FILE` record whose self number is 0, whose name is `$MFT` and whose
//!    unnamed `$DATA` is non-resident. Both the MFT's own record 0 and its
//!    `$MFTMirr` copy qualify.
//! 2. The **cluster size** comes from the record's own allocation: a
//!    non-resident attribute piece starting at VCN 0 covers
//!    `highest_vcn + 1` clusters and `allocated_size` bytes. The records
//!    that follow (`$MFTMirr`, `$LogFile`, `$Bitmap`, `$Boot`, ...) vote too,
//!    and when two copies of record 0 were found, their distance divided by
//!    the distance of their LCNs (`$MFT` vs `$MFTMirr`) decides.
//! 3. A copy found at byte `P` is either the MFT's record 0 (volume start
//!    `P - mft_lcn * cluster`) or the mirror copy (start
//!    `P - mftmirr_lcn * cluster`). Each hypothesis is checked against the
//!    disk: record 0 at the MFT location, the mirror copy at the mirror
//!    location (neither counts when it is the copy itself), the root
//!    directory (record 5) reached through the MFT's run list, and the
//!    `$UpCase` table where record 10 says it is. The best-supported start
//!    wins. The last check matters on large clusters, where the mirror
//!    cluster can hold copies of the root too.
//! 4. The **volume size** comes from `$BadClus:$Bad` (a sparse stream as
//!    long as the volume) or, failing that, from the length of `$Bitmap`.
//!    The **index record size** comes from the root's `$INDEX_ROOT`.
//!
//! The result carries a reconstructed [`BootSector`] that
//! [`crate::open_with_boot`] accepts, so a caller (the partition scanner)
//! can open the volume through a window of the disk starting at
//! [`FoundVolume::start`]. The bytes per sector are not recorded anywhere
//! in the MFT; 512 is assumed (it only matters for the boot sector copy,
//! which is gone anyway). The serial number is lost with the boot sector.
//!
//! The scan reads the requested range once in 1 MiB chunks, examines at
//! most [`MAX_CANDIDATES`] record-0 copies and never writes anything.

use std::collections::BTreeMap;

use diskrescue_core::util::bytes::{u16le, u32le};
use diskrescue_core::{Range, Result, SharedSource};

use crate::attr::{types, Attribute, FileName};
use crate::boot::{BootSector, MAX_CLUSTER_SIZE, MAX_RECORD_SIZE, MIN_RECORD_SIZE};
use crate::index::IndexRoot;
use crate::io::Disk;
use crate::record::MftRecord;
use crate::volume::is_mft_record_zero;

/// Scan granularity: volumes and MFT records start on 512-byte sectors.
pub const SCAN_STEP: u64 = 512;
/// Upper bound on record-0 copies examined in one scan.
pub const MAX_CANDIDATES: usize = 256;
/// Bytes read per scan step.
const SCAN_CHUNK: usize = 1 << 20;

/// An NTFS volume rebuilt from its MFT.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundVolume {
    /// Byte offset of the volume's first sector in the scanned source.
    pub start: u64,
    /// Volume size in bytes as the MFT describes it (not clipped to the
    /// source; compare with the source size to detect truncation).
    pub size: u64,
    /// Reconstructed boot sector; pass it to [`crate::open_with_boot`]
    /// together with a window of the source starting at `start`.
    pub boot: BootSector,
    /// Byte offset (in the scanned source) of the record-0 copy used.
    pub record0_offset: u64,
    /// True when that copy is the `$MFTMirr` one.
    pub from_mirror: bool,
    /// Number of independent checks that agreed with the reconstruction
    /// (record 0 at the MFT location, the mirror copy at the mirror
    /// location, the root directory through the run list, a size record).
    pub score: u32,
    /// How each value was obtained.
    pub evidence: Vec<String>,
}

/// One record-0 copy found by the scan.
#[derive(Debug, Clone)]
struct Copy {
    pos: u64,
    rsize: u64,
    attrs: Vec<Attribute>,
}

impl Copy {
    /// First LCN of the unnamed `$DATA` of record 0 (the MFT location).
    fn mft_lcn(&self) -> Option<u64> {
        first_lcn(&self.attrs, "")
    }
}

/// First real LCN of the unnamed (or named) `$DATA` piece starting at VCN 0.
fn first_lcn(attrs: &[Attribute], name: &str) -> Option<u64> {
    attrs
        .iter()
        .filter(|a| a.type_code == types::DATA && a.name == name)
        .filter_map(|a| a.non_resident())
        .find(|n| n.lowest_vcn == 0)
        .and_then(|n| n.runs.first())
        .and_then(|r| r.lcn)
}

/// Cluster sizes implied by single-piece non-resident attributes: each
/// piece starting at VCN 0 with `highest_vcn + 1` clusters and an
/// allocated size in bytes.
fn cluster_votes(attrs: &[Attribute], votes: &mut BTreeMap<u64, u32>) {
    for a in attrs {
        let Some(n) = a.non_resident() else {
            continue;
        };
        if n.lowest_vcn != 0 || n.highest_vcn == u64::MAX || n.allocated_size == 0 {
            continue;
        }
        // The piece must describe the whole allocation on its own.
        let clusters = n.highest_vcn + 1;
        if n.runs.last().map(|r| r.end_vcn()) != Some(clusters) {
            continue;
        }
        if n.allocated_size % clusters != 0 {
            continue;
        }
        let c = n.allocated_size / clusters;
        if c.is_power_of_two() && (512..=MAX_CLUSTER_SIZE).contains(&c) {
            *votes.entry(c).or_insert(0) += 1;
        }
    }
}

/// Scan `range` of `src` for NTFS volumes whose MFT survives, whether or
/// not their boot sectors do. Results are sorted by start offset; a volume
/// is reported once even when both its record-0 copies were found.
pub fn find_volumes(src: &SharedSource, range: Range) -> Result<Vec<FoundVolume>> {
    let disk = Disk::new(src.clone());
    let end = range.end.min(disk.size());
    let Some(mut pos) = range.start.checked_next_multiple_of(SCAN_STEP) else {
        return Ok(Vec::new());
    };
    let mut copies: Vec<Copy> = Vec::new();
    let mut buf = vec![0u8; SCAN_CHUNK];
    'scan: while pos < end {
        let len = ((end - pos) as usize).min(SCAN_CHUNK);
        let bad = disk.read(pos, &mut buf[..len], false)?;
        let mut off = 0usize;
        while off + 0x30 <= len {
            let at = pos + off as u64;
            if looks_like_record_zero(&buf[off..len]) && !bad.intersects(Range::with_len(at, 512)) {
                if let Some(c) = load_copy(&disk, at)? {
                    copies.push(c);
                    if copies.len() >= MAX_CANDIDATES {
                        log::warn!(
                            "NTFS discovery: more than {MAX_CANDIDATES} copies of MFT record 0; scan stopped at byte {at}"
                        );
                        break 'scan;
                    }
                }
            }
            off += SCAN_STEP as usize;
        }
        pos += len as u64;
    }
    let mut found: Vec<FoundVolume> = Vec::new();
    for i in 0..copies.len() {
        let Some(v) = rebuild(&disk, &copies, i)? else {
            continue;
        };
        match found.iter_mut().find(|f| f.start == v.start) {
            // Keep the better supported copy; on a tie the MFT's own record
            // 0 rather than the mirror's.
            Some(f) if (f.score, !f.from_mirror) >= (v.score, !v.from_mirror) => {}
            Some(f) => *f = v,
            None => found.push(v),
        }
    }
    found.sort_by_key(|f| f.start);
    Ok(found)
}

/// Cheap pre-filter on the first bytes of a candidate: `FILE`, a sane
/// update sequence offset, self number 0 (NTFS 3.1), a plausible record
/// size, and "$MFT" in UTF-16 within the first sector.
fn looks_like_record_zero(b: &[u8]) -> bool {
    if b.len() < 0x30 || &b[0..4] != b"FILE" {
        return false;
    }
    let usa_ofs = u16le(b, 4).unwrap_or(0);
    if usa_ofs < 0x28 || usa_ofs % 2 != 0 {
        return false;
    }
    if usa_ofs >= 0x30 && u32le(b, 0x2C) != Some(0) {
        return false;
    }
    let alloc = u32le(b, 0x1C).unwrap_or(0);
    if !alloc.is_power_of_two() || !(MIN_RECORD_SIZE..=MAX_RECORD_SIZE).contains(&alloc) {
        return false;
    }
    const NAME: [u8; 8] = [b'$', 0, b'M', 0, b'F', 0, b'T', 0];
    let head = &b[..b.len().min(512)];
    head.windows(NAME.len()).any(|w| w == NAME)
}

/// Read and validate a record-0 copy at `pos`.
fn load_copy(disk: &Disk, pos: u64) -> Result<Option<Copy>> {
    let mut head = [0u8; 0x30];
    if !disk.read(pos, &mut head, false)?.is_empty() {
        return Ok(None);
    }
    let rsize = u32le(&head, 0x1C).unwrap_or(0) as u64;
    if !rsize.is_power_of_two()
        || !(MIN_RECORD_SIZE as u64..=MAX_RECORD_SIZE as u64).contains(&rsize)
    {
        return Ok(None);
    }
    let Some(rec) = read_record(disk, pos, rsize, 0)? else {
        return Ok(None);
    };
    let (attrs, _) = rec.own_attributes(None);
    if !is_mft_record_zero(&rec, &attrs) || first_lcn(&attrs, "").is_none() {
        return Ok(None);
    }
    Ok(Some(Copy { pos, rsize, attrs }))
}

/// Read and decode the record of size `rsize` at byte `pos`, expecting
/// number `n`. `None` when unreadable or not a valid FILE record.
fn read_record(disk: &Disk, pos: u64, rsize: u64, n: u64) -> Result<Option<MftRecord>> {
    let mut buf = vec![0u8; rsize as usize];
    if !disk.read(pos, &mut buf, true)?.is_empty() {
        return Ok(None);
    }
    Ok(MftRecord::parse(n, buf).ok())
}

/// The record following a copy of record 0 at `copy.pos` (records 1..=7 sit
/// right behind record 0 in both the MFT and, for 1..=3, the mirror).
fn neighbour(disk: &Disk, copy: &Copy, n: u64) -> Result<Option<(MftRecord, Vec<Attribute>)>> {
    let Some(pos) = copy.pos.checked_add(n * copy.rsize) else {
        return Ok(None);
    };
    let Some(rec) = read_record(disk, pos, copy.rsize, n)? else {
        return Ok(None);
    };
    if rec.self_number.is_some_and(|s| s as u64 != n) || rec.is_extension() {
        return Ok(None);
    }
    let (attrs, _) = rec.own_attributes(None);
    Ok(Some((rec, attrs)))
}

/// Try to rebuild a volume from copy `i`.
fn rebuild(disk: &Disk, copies: &[Copy], i: usize) -> Result<Option<FoundVolume>> {
    let copy = &copies[i];
    let Some(l0) = copy.mft_lcn() else {
        return Ok(None);
    };
    let mut evidence = Vec::new();
    // Cluster size: votes from record 0 and the records behind it.
    let mut votes = BTreeMap::new();
    cluster_votes(&copy.attrs, &mut votes);
    let mut l1 = None;
    let mut neighbours = Vec::new();
    for n in 1..=7u64 {
        if let Some((rec, attrs)) = neighbour(disk, copy, n)? {
            cluster_votes(&attrs, &mut votes);
            if n == 1 {
                l1 = first_lcn(&attrs, "");
            }
            neighbours.push((n, rec, attrs));
        }
    }
    // Another copy of record 0 with the same MFT location pins the cluster
    // size by placement: (P_a - P_b) / (L_a - L_b) where L is the LCN each
    // copy lives at (the MFT's or the mirror's).
    if let Some(l1) = l1 {
        for other in copies.iter().filter(|o| o.pos != copy.pos) {
            if other.mft_lcn() != Some(l0) || l0 == l1 {
                continue;
            }
            let (dp, dl) = if other.pos > copy.pos {
                (other.pos - copy.pos, l1.abs_diff(l0))
            } else {
                (copy.pos - other.pos, l1.abs_diff(l0))
            };
            if dl != 0 && dp % dl == 0 {
                let c = dp / dl;
                if c.is_power_of_two() && (512..=MAX_CLUSTER_SIZE).contains(&c) {
                    *votes.entry(c).or_insert(0) += 2;
                    evidence.push(format!(
                        "cluster size {c} from the distance between the two copies of record 0"
                    ));
                }
            }
        }
    }
    let Some((&cluster, _)) = votes.iter().max_by_key(|(c, v)| (**v, **c)) else {
        return Ok(None);
    };
    evidence.push(format!(
        "cluster size {cluster} ({} vote(s) from allocation sizes and placement)",
        votes[&cluster]
    ));
    // Volume start hypotheses: this copy is the MFT's record 0, or the
    // mirror's.
    let mut hyps = Vec::new();
    if let Some(s) = copy.pos.checked_sub(l0.saturating_mul(cluster)) {
        hyps.push((s, false));
    }
    if let Some(l1) = l1.filter(|l1| *l1 != l0) {
        if let Some(s) = copy.pos.checked_sub(l1.saturating_mul(cluster)) {
            hyps.push((s, true));
        }
    }
    let mut best: Option<(u32, u64, bool, Vec<String>)> = None;
    for (start, mirror) in hyps {
        if start % SCAN_STEP != 0 {
            continue;
        }
        let (score, notes) = check_start(disk, copy, start, cluster, l0, l1, mirror)?;
        if best.as_ref().is_none_or(|b| score > b.0) {
            best = Some((score, start, mirror, notes));
        }
    }
    let Some((score, start, from_mirror, notes)) = best else {
        return Ok(None);
    };
    if score == 0 {
        return Ok(None);
    }
    evidence.extend(notes);
    // Volume size: $BadClus:$Bad spans the volume; $Bitmap has a bit per
    // cluster.
    let mut total_clusters = None;
    for (n, _, attrs) in &neighbours {
        if *n == 8 {
            if let Some(s) = data_size(attrs, "$Bad") {
                if s > 0 && s % cluster == 0 {
                    total_clusters = Some(s / cluster);
                    evidence.push(format!("volume size {s} from $BadClus:$Bad"));
                }
            }
        }
    }
    if total_clusters.is_none() {
        let rec8 = record_via_mft(disk, copy, start, cluster, 8)?;
        if let Some((_, attrs)) = rec8 {
            if let Some(s) = data_size(&attrs, "$Bad").filter(|s| *s > 0 && s % cluster == 0) {
                total_clusters = Some(s / cluster);
                evidence.push(format!("volume size {s} from $BadClus:$Bad"));
            }
        }
    }
    if total_clusters.is_none() {
        let rec6 = match neighbours.iter().find(|(n, _, _)| *n == 6) {
            Some((_, _, a)) => Some(a.clone()),
            None => record_via_mft(disk, copy, start, cluster, 6)?.map(|(_, a)| a),
        };
        if let Some(s) = rec6.as_deref().and_then(|a| data_size(a, "")) {
            if s > 0 {
                total_clusters = Some(s.saturating_mul(8));
                evidence.push(format!(
                    "volume size estimated from the {s}-byte $Bitmap (up to 63 clusters too large)"
                ));
            }
        }
    }
    let Some(total_clusters) = total_clusters.filter(|t| *t > l0) else {
        return Ok(None);
    };
    let index_record_size = record_via_mft(disk, copy, start, cluster, 5)?
        .and_then(|(_, attrs)| {
            attrs
                .iter()
                .find(|a| a.type_code == types::INDEX_ROOT)
                .and_then(|a| a.value())
                .and_then(IndexRoot::parse)
        })
        .map(|ir| ir.block_size)
        .filter(|s| s.is_power_of_two() && (512..=65536).contains(s))
        .unwrap_or(4096);
    let spc = (cluster / 512) as u32;
    let spc_raw = if spc <= 0x80 {
        spc as u8
    } else {
        (256 - spc.trailing_zeros()) as u8
    };
    let rsize = copy.rsize;
    let size_byte = |size: u64| -> i8 {
        if size >= cluster {
            (size / cluster) as i8
        } else {
            -(size.trailing_zeros() as i8)
        }
    };
    let size = total_clusters.saturating_mul(cluster);
    let boot = BootSector {
        bytes_per_sector: 512,
        sectors_per_cluster_raw: spc_raw,
        sectors_per_cluster: spc,
        cluster_size: cluster as u32,
        total_sectors: total_clusters.saturating_mul(spc as u64),
        mft_lcn: l0,
        mftmirr_lcn: l1.unwrap_or(l0),
        clusters_per_mft_record_raw: size_byte(rsize),
        clusters_per_index_record_raw: size_byte(index_record_size as u64),
        mft_record_size: rsize as u32,
        index_record_size,
        serial: 0,
        has_signature: false,
        offset: 0,
        is_backup: false,
        warnings: vec![format!(
            "boot sector reconstructed from MFT record 0 at byte {} of the scanned source; the volume serial number is lost",
            copy.pos
        )],
    };
    Ok(Some(FoundVolume {
        start,
        size,
        boot,
        record0_offset: copy.pos,
        from_mirror,
        score,
        evidence,
    }))
}

/// Data size of the `$DATA` stream `name` (piece at VCN 0).
fn data_size(attrs: &[Attribute], name: &str) -> Option<u64> {
    attrs
        .iter()
        .filter(|a| a.type_code == types::DATA && a.name == name)
        .find(|a| a.non_resident().is_none_or(|n| n.lowest_vcn == 0))
        .map(|a| a.data_size())
}

/// Volume byte offset of MFT byte `off` through record 0's run list.
fn mft_byte(copy: &Copy, cluster: u64, off: u64) -> Option<u64> {
    let vcn = off / cluster;
    let n = copy
        .attrs
        .iter()
        .filter(|a| a.type_code == types::DATA && a.name.is_empty())
        .filter_map(|a| a.non_resident())
        .find(|n| n.lowest_vcn == 0)?;
    let r = n.runs.iter().find(|r| r.vcn <= vcn && vcn < r.end_vcn())?;
    let lcn = r.lcn?;
    lcn.checked_add(vcn - r.vcn)?
        .checked_mul(cluster)?
        .checked_add(off % cluster)
}

/// Record `n` read through record 0's run list, assuming the volume starts
/// at `start`.
fn record_via_mft(
    disk: &Disk,
    copy: &Copy,
    start: u64,
    cluster: u64,
    n: u64,
) -> Result<Option<(MftRecord, Vec<Attribute>)>> {
    let Some(rel) = mft_byte(copy, cluster, n * copy.rsize) else {
        return Ok(None);
    };
    let Some(abs) = start.checked_add(rel) else {
        return Ok(None);
    };
    let Some(rec) = read_record(disk, abs, copy.rsize, n)? else {
        return Ok(None);
    };
    if rec.self_number.is_some_and(|s| s as u64 != n) {
        return Ok(None);
    }
    let (attrs, _) = rec.own_attributes(None);
    Ok(Some((rec, attrs)))
}

/// Score a volume start: record 0 where the MFT should be, its copy where
/// the mirror should be, and the root directory through the run list.
fn check_start(
    disk: &Disk,
    copy: &Copy,
    start: u64,
    cluster: u64,
    l0: u64,
    l1: Option<u64>,
    copy_is_mirror: bool,
) -> Result<(u32, Vec<String>)> {
    let mut score = 0;
    let mut notes = Vec::new();
    let is_rec0 = |pos: u64| -> Result<bool> {
        let Some(rec) = read_record(disk, pos, copy.rsize, 0)? else {
            return Ok(false);
        };
        let (attrs, _) = rec.own_attributes(None);
        Ok(is_mft_record_zero(&rec, &attrs) && first_lcn(&attrs, "") == Some(l0))
    };
    if let Some(p) = l0
        .checked_mul(cluster)
        .and_then(|x| x.checked_add(start))
        .filter(|p| copy_is_mirror && *p != copy.pos)
    {
        if is_rec0(p)? {
            score += 1;
            notes.push(format!("MFT record 0 found at the MFT location (byte {p})"));
        }
    }
    if let Some(p) = l1
        .filter(|l| *l != l0)
        .and_then(|l| l.checked_mul(cluster))
        .and_then(|x| x.checked_add(start))
        .filter(|p| !copy_is_mirror && *p != copy.pos)
    {
        if is_rec0(p)? {
            score += 1;
            notes.push(format!("$MFTMirr copy of record 0 found at byte {p}"));
        }
    }
    if let Some((rec, attrs)) = record_via_mft(disk, copy, start, cluster, 5)? {
        let root = rec.is_directory()
            && attrs
                .iter()
                .filter(|a| a.type_code == types::FILE_NAME)
                .filter_map(|a| a.value().and_then(FileName::parse))
                .any(|f| f.name == "." && f.parent_record() == 5);
        if root {
            score += 1;
            notes.push("root directory (record 5) found through the MFT run list".into());
        }
    }
    // $UpCase starts with the identity mapping of the first code units.
    if let Some((_, attrs)) = record_via_mft(disk, copy, start, cluster, 10)? {
        let at = first_lcn(&attrs, "")
            .and_then(|l| l.checked_mul(cluster))
            .and_then(|x| x.checked_add(start));
        if let Some(at) = at {
            let mut b = [0u8; 32];
            if disk.read(at, &mut b, false)?.is_empty()
                && (0..16u16).all(|i| u16le(&b, 2 * i as usize) == Some(i))
            {
                score += 1;
                notes.push(format!("$UpCase table found at byte {at}"));
            }
        }
    }
    Ok((score, notes))
}
