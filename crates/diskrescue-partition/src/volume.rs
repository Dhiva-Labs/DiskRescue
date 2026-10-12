//! Table plus per-partition sources: the entry point the recovery pipeline
//! uses.

use std::sync::Arc;

use diskrescue_core::partition::{PartitionEntry, PartitionTable};
use diskrescue_core::source::SubSource;
use diskrescue_core::{Result, SharedSource, Source};

use crate::probe::{probe_at, FsProbe};
use crate::scan::{scan_lost_partitions, ScanOptions};
use crate::table::detect;

/// Size of the regions at each end of the device examined by the quick scan.
const QUICK_SCAN_BYTES: u64 = 64 << 20;

/// One partition of the device, ready to hand to a filesystem parser.
#[derive(Clone)]
pub struct Volume {
    /// The table entry (with `fs` filled in when a filesystem was recognised).
    pub entry: PartitionEntry,
    /// A view of exactly the partition's bytes.
    pub source: SharedSource,
    /// What the filesystem probe found at the start of the partition.
    pub probe: Option<FsProbe>,
}

impl std::fmt::Debug for Volume {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Volume")
            .field("entry", &self.entry)
            .field("source", &self.source.describe())
            .field("probe", &self.probe)
            .finish()
    }
}

/// Read the partition table, probe every partition and return a
/// [`SubSource`] view of each.
///
/// `sector_size` is the preferred logical sector size (512 normally; 4096 for
/// 4Kn media; 0 to detect). The result includes:
///
/// * the table (GPT/MBR/superfloppy, see [`crate::read_partition_table`]),
/// * one [`Volume`] per entry, with [`PartitionEntry::fs`] set from the probe,
/// * when no entry holds a recognisable filesystem (no table, or a table that
///   points at nothing), a quick scan of the first and last 64 MiB at sector
///   granularity: volumes found there are added to the table with origin
///   `"scan"` and a warning. Full-device scans are the caller's choice
///   ([`scan_lost_partitions`]).
pub fn volumes(src: &SharedSource, sector_size: u32) -> Result<(PartitionTable, Vec<Volume>)> {
    let hint = (sector_size != 0).then_some(sector_size);
    let detected = detect(src.as_ref(), hint)?;
    let mut table = detected.table;
    let mut probes = detected.probes;

    if !probes.iter().any(Option::is_some) {
        let found = quick_scan(src)?;
        if found.is_empty() {
            table.warnings.push(
                "no usable partition table and the quick scan of the first and last 64 MiB found no volumes"
                    .to_string(),
            );
        } else {
            table.warnings.push(format!(
                "no usable partition table; the quick scan of the first and last 64 MiB found {} volume(s)",
                found.len()
            ));
            for e in found {
                if table.entries.iter().all(|t| t.start != e.start) {
                    table.entries.push(e);
                }
            }
            table.entries.sort_by_key(|e| (e.start, e.size));
            for (i, e) in table.entries.iter_mut().enumerate() {
                e.index = i as u32 + 1;
            }
            probes = table
                .entries
                .iter_mut()
                .map(|e| {
                    let p = probe_at(src.as_ref(), e.start, Some(e.size));
                    e.fs = p.as_ref().map(|p| p.kind);
                    p
                })
                .collect();
        }
    }

    let vols = table
        .entries
        .iter()
        .cloned()
        .zip(probes)
        .map(|(entry, probe)| {
            let name = format!("partition {} of {}", entry.index, src.describe());
            let source: SharedSource =
                Arc::new(SubSource::named(src.clone(), entry.start, entry.size, name));
            Volume {
                entry,
                source,
                probe,
            }
        })
        .collect();
    Ok((table, vols))
}

/// Scan the first and last 64 MiB (or the whole device when it is small).
fn quick_scan(src: &SharedSource) -> Result<Vec<PartitionEntry>> {
    let dev = src.size();
    let mut ranges = Vec::new();
    if dev <= 2 * QUICK_SCAN_BYTES {
        ranges.push((0, dev));
    } else {
        ranges.push((0, QUICK_SCAN_BYTES));
        ranges.push((dev - QUICK_SCAN_BYTES, dev));
    }
    let mut out: Vec<PartitionEntry> = Vec::new();
    for (start, end) in ranges {
        let opts = ScanOptions {
            step: 512,
            start,
            end: Some(end),
            cancel: None,
            max_results: 64,
        };
        for e in scan_lost_partitions(src, &opts, &mut |_, _| {})? {
            if out.iter().all(|o| o.start != e.start) {
                out.push(e);
            }
        }
    }
    Ok(out)
}
