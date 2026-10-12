//! Partition table discovery: GPT, MBR/EBR and "superfloppy" volumes.
//!
//! # Algorithm
//!
//! 1. **GPT first.** For each candidate logical sector size (the caller's hint,
//!    the source's own sector size, 512, 4096) look for `EFI PART` at LBA 1 and
//!    at the last LBA. A valid header wins even when sector 0 holds a regular
//!    MBR instead of the protective one (reported as a warning). Hybrid MBRs
//!    contribute the entries the GPT does not cover.
//! 2. **Sector 0.** If it is a filesystem boot sector (superfloppy: FAT, exFAT,
//!    NTFS, ext...) the result is [`PartitionScheme::None`] with one entry
//!    spanning the volume. Otherwise a sector ending in `55 AA` with at least
//!    one non-empty entry is an MBR; extended partitions are walked through
//!    their EBR chains. LBAs are multiplied by the sector size that makes the
//!    entries point at recognisable filesystems (so a 4Kn image with a 4096-byte
//!    MBR is interpreted correctly).
//! 3. Entries are clipped to the device, sorted by start, numbered from 1,
//!    checked for overlaps, and probed so [`PartitionEntry::fs`] is filled.
//!
//! The type byte or type GUID is only a description: whether a partition holds
//! a filesystem is decided by probing its first sectors.

use diskrescue_core::fs::FsKind;
use diskrescue_core::partition::{PartitionEntry, PartitionScheme, PartitionTable};
use diskrescue_core::{Result, SharedSource, Source};

use crate::gpt::{self, GptInfo};
use crate::io::read_soft;
use crate::mbr::{self, Mbr};
use crate::probe::{probe_at, FsProbe};

/// A partition table together with the logical sector size it was read with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableDetection {
    /// The table.
    pub table: PartitionTable,
    /// Logical sector size used for LBA arithmetic (512 or 4096 normally).
    pub sector_size: u32,
}

/// Internal result: the detection plus per-entry probes (same order as
/// `table.entries`).
pub(crate) struct Detected {
    pub table: PartitionTable,
    pub sector_size: u32,
    pub probes: Vec<Option<FsProbe>>,
}

/// Read the partition table of `src`, detecting the logical sector size
/// (512 or 4096 bytes).
///
/// Never fails on damaged media: unreadable or invalid structures become
/// [`PartitionTable::warnings`]. Only fatal source errors (the device
/// disappeared) are returned as `Err`. When there is no table and no
/// filesystem at offset 0 the result has scheme [`PartitionScheme::Unknown`]
/// and no entries.
pub fn read_partition_table(src: &SharedSource) -> Result<PartitionTable> {
    Ok(detect(src.as_ref(), None)?.table)
}

/// Like [`read_partition_table`] but with the logical sector size preferred
/// by the caller (512 by default elsewhere; 4096 for a 4Kn device). Zero means
/// "detect". If the data does not fit the preferred size, other sizes are
/// tried.
pub fn read_partition_table_with(src: &SharedSource, sector_size: u32) -> Result<PartitionTable> {
    let hint = (sector_size != 0).then_some(sector_size);
    Ok(detect(src.as_ref(), hint)?.table)
}

/// Like [`read_partition_table_with`] but also returns the sector size that
/// was used.
pub fn read_partition_table_detailed(
    src: &SharedSource,
    sector_size: Option<u32>,
) -> Result<TableDetection> {
    let hint = sector_size.filter(|&s| s != 0);
    let d = detect(src.as_ref(), hint)?;
    Ok(TableDetection {
        table: d.table,
        sector_size: d.sector_size,
    })
}

/// Candidate logical sector sizes in the order they are tried.
fn sector_candidates(src: &dyn Source, hint: Option<u32>) -> Vec<u32> {
    let mut v: Vec<u32> = Vec::with_capacity(4);
    let mut push = |x: u32| {
        if matches!(x, 512 | 1024 | 2048 | 4096) && !v.contains(&x) {
            v.push(x);
        }
    };
    if let Some(h) = hint {
        push(h);
    }
    push(src.sector_size());
    push(512);
    push(4096);
    v
}

/// Clip `e` to a device of `dev_size` bytes; `false` when nothing is left.
fn clip(e: &mut PartitionEntry, dev_size: u64, warnings: &mut Vec<String>) -> bool {
    if dev_size == 0 {
        return true;
    }
    if e.start >= dev_size {
        warnings.push(format!(
            "partition at byte {} ({}) starts beyond the end of the device ({} bytes); ignored",
            e.start, e.type_desc, dev_size
        ));
        return false;
    }
    if e.end() > dev_size {
        warnings.push(format!(
            "partition at byte {} ({}) extends past the end of the device; clipped from {} to {} bytes",
            e.start,
            e.type_desc,
            e.size,
            dev_size - e.start
        ));
        e.size = dev_size - e.start;
    }
    true
}

/// Sort, number, warn about overlaps and probe the entries.
fn finalize(
    src: &dyn Source,
    scheme: PartitionScheme,
    mut entries: Vec<PartitionEntry>,
    mut warnings: Vec<String>,
    disk_guid: Option<String>,
    sector_size: u32,
    check_zero_start: bool,
) -> Detected {
    let dev_size = src.size();
    entries.retain_mut(|e| clip(e, dev_size, &mut warnings));
    entries.sort_by_key(|e| (e.start, e.size));
    for (i, e) in entries.iter_mut().enumerate() {
        e.index = i as u32 + 1;
    }
    if check_zero_start {
        for e in entries.iter().filter(|e| e.start == 0) {
            warnings.push(format!(
                "partition {} starts at LBA 0 and overlaps the partition table",
                e.index
            ));
        }
    }
    let mut max_end = 0u64;
    let mut owner = 0u32;
    for e in &entries {
        if e.start < max_end {
            warnings.push(format!("partition {} overlaps partition {owner}", e.index));
        }
        if e.end() > max_end {
            max_end = e.end();
            owner = e.index;
        }
    }
    let mut probes = Vec::with_capacity(entries.len());
    for e in &mut entries {
        let p = probe_at(src, e.start, Some(e.size));
        e.fs = p.as_ref().map(|p| p.kind);
        probes.push(p);
    }
    Detected {
        table: PartitionTable {
            scheme,
            entries,
            disk_guid,
            warnings,
        },
        sector_size,
        probes,
    }
}

fn gpt_entries(info: &GptInfo, warnings: &mut Vec<String>) -> Vec<PartitionEntry> {
    let ss = u64::from(info.sector_size);
    let origin = if info.used_backup {
        "gpt-backup"
    } else {
        "gpt"
    };
    let h = &info.header;
    let mut out = Vec::new();
    for e in &info.entries {
        if e.last_lba < e.first_lba {
            warnings.push(format!(
                "GPT entry {} has its last LBA ({}) before its first LBA ({}); ignored",
                e.slot + 1,
                e.last_lba,
                e.first_lba
            ));
            continue;
        }
        let (Some(start), Some(size)) = (
            e.first_lba.checked_mul(ss),
            (e.last_lba - e.first_lba)
                .checked_add(1)
                .and_then(|n| n.checked_mul(ss)),
        ) else {
            warnings.push(format!(
                "GPT entry {} overflows the address space; ignored",
                e.slot + 1
            ));
            continue;
        };
        if e.first_lba < h.first_usable_lba || e.last_lba > h.last_usable_lba {
            warnings.push(format!(
                "GPT entry {} lies outside the usable LBA range {}..={}",
                e.slot + 1,
                h.first_usable_lba,
                h.last_usable_lba
            ));
        }
        out.push(PartitionEntry {
            index: 0,
            start,
            size,
            type_id: e.type_guid.clone(),
            type_desc: gpt::type_description(&e.type_guid)
                .map_or_else(|| format!("Unknown ({})", e.type_guid), str::to_string),
            name: (!e.name.is_empty()).then(|| e.name.clone()),
            guid: Some(e.unique_guid.clone()),
            bootable: e.attributes & 4 != 0,
            origin: origin.to_string(),
            fs: None,
        });
    }
    out
}

fn build_gpt(src: &dyn Source, info: GptInfo, mbr: Option<&Mbr>) -> Result<Detected> {
    let mut warnings = info.warnings.clone();
    let mut entries = gpt_entries(&info, &mut warnings);
    let ss = info.sector_size;
    match mbr {
        Some(m) if m.used_count() > 0 && m.has_protective_entry() => {
            // Hybrid MBR: mirror entries are redundant, others are kept.
            let extra: Vec<PartitionEntry> = match mbr::walk(src, m, ss) {
                Ok(w) => w.entries,
                Err(_) => Vec::new(),
            };
            let mut added = 0;
            for e in extra {
                let overlaps = entries
                    .iter()
                    .any(|g| e.start < g.end() && g.start < e.start.saturating_add(e.size));
                if !overlaps {
                    entries.push(e);
                    added += 1;
                }
            }
            if m.used().any(|e| !e.is_protective()) {
                warnings.push(format!(
                    "hybrid MBR: sector 0 mirrors GPT partitions in an MBR table ({added} MBR-only partition(s) added)"
                ));
            }
        }
        Some(m) if m.used_count() > 0 => warnings.push(
            "protective MBR is missing: sector 0 holds a regular MBR with partitions; using the GPT"
                .to_string(),
        ),
        Some(_) | None => {
            warnings.push("protective MBR is missing or empty (sector 0)".to_string());
        }
    }
    Ok(finalize(
        src,
        PartitionScheme::Gpt,
        entries,
        warnings,
        Some(info.header.disk_guid.clone()),
        ss,
        true,
    ))
}

/// Pick the sector size under which the MBR entries make most sense.
fn choose_mbr_sector_size(src: &dyn Source, mbr: &Mbr, cands: &[u32]) -> u32 {
    let Some(&first) = cands.first() else {
        return 512;
    };
    if cands.len() == 1 {
        return first;
    }
    let size = src.size();
    let mut best = (first, i64::MIN);
    for &ss in cands {
        let mut score = 0i64;
        for e in mbr.used().filter(|e| !e.is_protective()) {
            let start = u64::from(e.first_lba) * u64::from(ss);
            if start >= size {
                score -= 1;
                continue;
            }
            score += 1;
            if e.is_extended() {
                let ebr = read_soft(src, start, 512).ok().flatten();
                if ebr.is_some_and(|s| s.len() >= 512 && s[510] == 0x55 && s[511] == 0xAA) {
                    score += 2;
                }
            } else {
                let len = u64::from(e.sectors) * u64::from(ss);
                if probe_at(src, start, Some(len)).is_some() {
                    score += 4;
                }
            }
        }
        if score > best.1 {
            best = (ss, score);
        }
    }
    best.0
}

fn superfloppy(src: &dyn Source, probe: FsProbe, mut warnings: Vec<String>, ss: u32) -> Detected {
    let dev = src.size();
    let mut size = probe.size.unwrap_or(dev);
    if size == 0 || size > dev {
        if size > dev {
            warnings.push(format!(
                "filesystem claims {size} bytes but the device holds only {dev}; clipped"
            ));
        }
        size = dev;
    }
    let desc = format!("{} (no partition table)", probe.display_name());
    let entry = PartitionEntry {
        index: 1,
        start: 0,
        size,
        type_id: "none".to_string(),
        type_desc: desc,
        name: probe.label.clone(),
        guid: None,
        bootable: false,
        origin: "superfloppy".to_string(),
        fs: Some(probe.kind),
    };
    Detected {
        table: PartitionTable {
            scheme: PartitionScheme::None,
            entries: vec![entry],
            disk_guid: None,
            warnings,
        },
        sector_size: ss,
        probes: vec![Some(probe)],
    }
}

/// Core of every public entry point.
pub(crate) fn detect(src: &dyn Source, hint: Option<u32>) -> Result<Detected> {
    let dev = src.size();
    let cands = sector_candidates(src, hint);
    let default_ss = cands.first().copied().unwrap_or(512);
    if dev == 0 {
        let mut t = PartitionTable::empty(PartitionScheme::Unknown);
        t.warnings.push("the source is empty".to_string());
        return Ok(Detected {
            table: t,
            sector_size: default_ss,
            probes: Vec::new(),
        });
    }

    let sec0 = read_soft(src, 0, 512)?;
    let mut warnings = Vec::new();
    if sec0.is_none() {
        warnings.push("sector 0 is unreadable".to_string());
    }
    let mbr = sec0
        .as_deref()
        .and_then(mbr::parse_mbr)
        .filter(|m| m.boot_signature_ok);

    // 1. GPT.
    for &ss in &cands {
        if let Some(info) = gpt::read_gpt(src, ss)? {
            let mut d = build_gpt(src, info, mbr.as_ref())?;
            d.table.warnings.splice(0..0, warnings);
            return Ok(d);
        }
    }

    // 2. Filesystem at offset 0 vs MBR.
    let fs0 = probe_at(src, 0, None);
    let used = mbr.as_ref().map_or(0, Mbr::used_count);
    match fs0 {
        // A real MBR wins over a filesystem that merely shares the device:
        // one visible only through backup structures, or one whose signature
        // lives far from the boot sector (ISO 9660 in a hybrid image, ext
        // superblocks, ...). FAT, exFAT and NTFS boot sectors occupy sector 0
        // itself, so there the stricter filesystem validation decides.
        Some(p)
            if used > 0
                && (p.via_backup
                    || !matches!(
                        p.kind,
                        FsKind::Fat12
                            | FsKind::Fat16
                            | FsKind::Fat32
                            | FsKind::ExFat
                            | FsKind::Ntfs
                    )) =>
        {
            warnings.push(format!(
                "sector 0 holds an MBR; the {} signature at offset 0 is ignored (hybrid image or leftover)",
                p.display_name()
            ));
        }
        Some(p) => {
            if used > 0 {
                warnings.push(
                    "sector 0 is a filesystem boot sector that also resembles an MBR".to_string(),
                );
            }
            return Ok(superfloppy(src, p, warnings, default_ss));
        }
        None => {}
    }

    let Some(mbr) = mbr else {
        warnings.push(
            "no partition table or recognisable filesystem found at the start of the device"
                .to_string(),
        );
        let mut t = PartitionTable::empty(PartitionScheme::Unknown);
        t.warnings = warnings;
        return Ok(Detected {
            table: t,
            sector_size: default_ss,
            probes: Vec::new(),
        });
    };

    if mbr.used_count() == 0 {
        warnings.push("MBR partition table is empty".to_string());
        let mut t = PartitionTable::empty(PartitionScheme::Mbr);
        t.warnings = warnings;
        return Ok(Detected {
            table: t,
            sector_size: default_ss,
            probes: Vec::new(),
        });
    }

    let ss = choose_mbr_sector_size(src, &mbr, &cands);
    let walk = mbr::walk(src, &mbr, ss)?;
    warnings.extend(walk.warnings);
    if mbr.has_protective_entry() {
        warnings.push(
            "protective MBR present but no valid GPT header was found at LBA 1 or the last LBA"
                .to_string(),
        );
    }
    let scheme = if mbr.has_protective_entry() && walk.entries.is_empty() {
        PartitionScheme::Gpt
    } else {
        PartitionScheme::Mbr
    };
    Ok(finalize(
        src,
        scheme,
        walk.entries,
        warnings,
        None,
        ss,
        true,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskrescue_core::source::MemSource;

    fn entry(start: u64, size: u64) -> PartitionEntry {
        PartitionEntry {
            index: 0,
            start,
            size,
            type_id: "0x83".into(),
            type_desc: "Linux".into(),
            name: None,
            guid: None,
            bootable: false,
            origin: "mbr".into(),
            fs: None,
        }
    }

    #[test]
    fn candidate_sector_sizes() {
        let s = MemSource::new(vec![0u8; 16]);
        assert_eq!(sector_candidates(&s, None), vec![512, 4096]);
        assert_eq!(sector_candidates(&s, Some(4096)), vec![4096, 512]);
        assert_eq!(sector_candidates(&s, Some(7)), vec![512, 4096]);
        assert_eq!(sector_candidates(&s, Some(2048)), vec![2048, 512, 4096]);
    }

    #[test]
    fn clipping() {
        let mut w = Vec::new();
        let mut e = entry(1000, 500);
        assert!(clip(&mut e, 2000, &mut w) && e.size == 500 && w.is_empty());
        let mut e = entry(1800, 500);
        assert!(clip(&mut e, 2000, &mut w) && e.size == 200);
        assert_eq!(w.len(), 1);
        let mut e = entry(2000, 10);
        assert!(!clip(&mut e, 2000, &mut w));
        assert_eq!(w.len(), 2);
        // An unknown device size (0) never clips.
        let mut e = entry(1 << 40, 1 << 20);
        assert!(clip(&mut e, 0, &mut w));
    }

    #[test]
    fn finalize_orders_numbers_and_flags_overlaps() {
        let src = MemSource::new(vec![0u8; 1 << 20]);
        let d = finalize(
            &src,
            PartitionScheme::Mbr,
            vec![entry(4096, 4096), entry(0, 8192), entry(600_000, 1 << 30)],
            Vec::new(),
            None,
            512,
            true,
        );
        let t = d.table;
        assert_eq!(t.entries.len(), 3);
        assert_eq!(
            t.entries
                .iter()
                .map(|e| (e.index, e.start))
                .collect::<Vec<_>>(),
            vec![(1, 0), (2, 4096), (3, 600_000)]
        );
        assert_eq!(t.entries[2].end(), 1 << 20);
        let w = t.warnings.join("|");
        assert!(w.contains("starts at LBA 0"), "{w}");
        assert!(w.contains("partition 2 overlaps partition 1"), "{w}");
        assert!(w.contains("clipped"), "{w}");
        assert_eq!(d.probes.len(), 3);
    }
}
