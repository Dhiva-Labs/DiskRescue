//! Streaming file content out of the volume.
//!
//! * Resident data is copied from the MFT record.
//! * Uncompressed non-resident data is read run by run in chunks of at most
//!   [`CHUNK`] bytes. Sparse runs and bytes past the initialized size are
//!   written as zeros and reported as holes; unreadable sectors, runs
//!   outside the image and VCNs whose mapping was lost are written as zeros
//!   and reported as unreadable. For deleted files every cluster is checked
//!   against `$Bitmap`; clusters now allocated to something else are
//!   reported as reused (their current bytes are still written).
//! * Compressed data is processed one compression unit at a time: a unit
//!   whose runs cover all its clusters is stored raw, a unit that is
//!   entirely sparse is zeros, and a unit with fewer real clusters followed
//!   by sparse ones is LZNT1-decompressed. A unit with unreadable input or a
//!   decompression error is written as far as it decoded (rest zeros) and
//!   reported as unreadable.
//! * EFS-encrypted content is refused (the ciphertext is useless without the
//!   user's keys).

use std::io::Write;

use diskrescue_core::fs::{EntryConfidence, FileEntry, FileReadReport};
use diskrescue_core::{Error, Range};

use crate::bitmap::ClusterState;
use crate::lznt1::decompress_unit;
use crate::stream::{
    read_plain, AllocationMap, Annotations, ExtentKind, NonResStream, Stream, StreamData,
};
use crate::volume::NtfsFs;
use crate::walk::{flags, split_id, ORPHANS_DIR, SUB_ADS, SUB_SYNTHETIC};
use crate::Result;

/// Read granularity for file content.
const CHUNK: u64 = 1 << 20;
/// Largest compression unit accepted (Windows uses 64 KiB).
const MAX_UNIT: u64 = 4 << 20;

impl NtfsFs {
    pub(crate) fn read_file_impl(
        &self,
        entry: &FileEntry,
        sink: &mut dyn Write,
    ) -> Result<FileReadReport> {
        if entry.is_dir {
            return Err(Error::InvalidArgument(format!(
                "{} is a directory",
                entry.path
            )));
        }
        let (rec_no, sub) = split_id(entry.id);
        if sub >= SUB_SYNTHETIC || entry.confidence == EntryConfidence::DeletedNoData {
            return Err(Error::NotFound(format!(
                "{}: the data location is unknown (only the name survives); carve for it",
                entry.path
            )));
        }
        let info = self.file_info(rec_no)?.ok_or_else(|| {
            Error::NotFound(format!(
                "MFT record {rec_no} of {} is unreadable",
                entry.path
            ))
        })?;
        let mut report = FileReadReport::default();
        // Record-level problems: torn write, lost extension records, damaged
        // run lists.
        report.notes.extend(info.warnings.iter().cloned());
        if entry.path.starts_with(ORPHANS_DIR) {
            report
                .notes
                .push("original directory unknown; placed under $orphans".into());
        }
        if entry.flags.iter().any(|f| f == flags::HARDLINK) {
            report
                .notes
                .push("hard link: the same data is reported under another name too".into());
        }
        let stream = if sub >= SUB_ADS {
            info.ads.get((sub - SUB_ADS) as usize)
        } else {
            info.data.as_ref()
        };
        let Some(stream) = stream else {
            if info.record.in_use() {
                report.notes.push("file has no data attribute".into());
                return Ok(report);
            }
            return Err(Error::NotFound(format!(
                "{}: deleted record has no data attribute",
                entry.path
            )));
        };
        if info.encrypted {
            return Err(Error::unsupported(format!(
                "{} is EFS-encrypted; its content cannot be recovered",
                entry.path
            )));
        }
        let deleted = !info.record.in_use();
        self.copy_stream(stream, deleted, sink, &mut report)?;
        Ok(report)
    }

    fn copy_stream(
        &self,
        stream: &Stream,
        deleted: bool,
        sink: &mut dyn Write,
        report: &mut FileReadReport,
    ) -> Result<()> {
        match &stream.data {
            StreamData::Resident(v) => {
                sink.write_all(v)?;
                report.bytes_written = v.len() as u64;
                Ok(())
            }
            StreamData::NonResident(n) => {
                if stream.is_compressed() {
                    if !self.opts.decompress {
                        return Err(Error::unsupported(
                            "file is NTFS-compressed and decompression is disabled",
                        ));
                    }
                    self.copy_compressed(n, deleted, sink, report)
                } else {
                    self.copy_plain(n, deleted, sink, report)
                }
            }
        }
    }

    fn copy_plain(
        &self,
        n: &NonResStream,
        deleted: bool,
        sink: &mut dyn Write,
        report: &mut FileReadReport,
    ) -> Result<()> {
        let mut ann = Annotations::default();
        let reuse: Option<&dyn AllocationMap> = if deleted {
            self.bitmap.as_ref().map(|b| b as &dyn AllocationMap)
        } else {
            None
        };
        let size = n.data_size;
        let mut buf = vec![0u8; CHUNK.min(size) as usize];
        let mut pos = 0u64;
        while pos < size {
            let len = CHUNK.min(size - pos) as usize;
            read_plain(
                &self.disk,
                n,
                self.cluster,
                pos,
                &mut buf[..len],
                &mut ann,
                reuse,
                false,
            )?;
            sink.write_all(&buf[..len])?;
            pos += len as u64;
            report.bytes_written = pos;
        }
        if deleted && self.bitmap.is_none() && size > 0 {
            report
                .notes
                .push("$Bitmap unavailable: could not check whether clusters were reused".into());
        }
        self.finish(n, ann, report);
        Ok(())
    }

    fn copy_compressed(
        &self,
        n: &NonResStream,
        deleted: bool,
        sink: &mut dyn Write,
        report: &mut FileReadReport,
    ) -> Result<()> {
        let unit_clusters = 1u64
            .checked_shl(n.compression_unit as u32)
            .filter(|_| n.compression_unit < 32)
            .ok_or_else(|| Error::corrupt("compressed attribute", "absurd compression unit"))?;
        let unit = unit_clusters.saturating_mul(self.cluster);
        if unit > MAX_UNIT {
            return Err(Error::unsupported(format!(
                "compression unit of {unit} bytes is not supported"
            )));
        }
        let mut ann = Annotations::default();
        let size = n.data_size;
        let units = size.div_ceil(unit);
        let mut decompressed_units = 0u64;
        let mut bad_units = 0u64;
        for u in 0..units {
            let lstart = u * unit;
            let lend = lstart.saturating_add(unit).min(size);
            let vstart = u * unit_clusters;
            let vend = vstart.saturating_add(unit_clusters);
            // Collect the unit's pieces.
            let mut pieces: Vec<(u64, u64, ExtentKind)> = Vec::new();
            let mut v = vstart;
            while v < vend {
                match n.find(v) {
                    Some(i) => {
                        let e = n.extents[i];
                        let end = e.end().min(vend);
                        let kind = match e.kind {
                            ExtentKind::Real(lcn) => ExtentKind::Real(lcn + (v - e.vcn)),
                            k => k,
                        };
                        pieces.push((v, end - v, kind));
                        v = end;
                    }
                    None => {
                        pieces.push((v, vend - v, ExtentKind::Missing));
                        v = vend;
                    }
                }
            }
            let real: Vec<(u64, u64)> = pieces
                .iter()
                .filter_map(|&(_, len, k)| match k {
                    ExtentKind::Real(l) => Some((l, len)),
                    _ => None,
                })
                .collect();
            let real_count: u64 = real.iter().map(|r| r.1).sum();
            let any_missing = pieces.iter().any(|p| p.2 == ExtentKind::Missing);
            let mut out: Vec<u8>;
            if any_missing {
                out = vec![0u8; (lend - lstart) as usize];
                ann.unreadable.insert(Range::new(lstart, lend));
            } else if real_count == 0 {
                out = vec![0u8; (lend - lstart) as usize];
                ann.holes.insert(Range::new(lstart, lend));
            } else {
                // Gather the real clusters.
                let mut input = vec![0u8; (real_count * self.cluster) as usize];
                let mut bad_input = false;
                let mut at = 0usize;
                for &(lcn, len) in &real {
                    let bytes = (len * self.cluster) as usize;
                    let bad =
                        self.disk
                            .read(lcn * self.cluster, &mut input[at..at + bytes], false)?;
                    if !bad.is_empty() {
                        bad_input = true;
                    }
                    at += bytes;
                }
                if deleted {
                    if let Some(bm) = &self.bitmap {
                        let mut hit = false;
                        for &(lcn, len) in &real {
                            bm.for_each_state(&self.disk, lcn, lcn + len, &mut |_, _, st| {
                                if st == ClusterState::Allocated {
                                    hit = true;
                                }
                            })?;
                        }
                        if hit {
                            ann.reused.insert(Range::new(lstart, lend));
                        }
                    }
                }
                if real_count >= unit_clusters {
                    // Stored uncompressed.
                    input.truncate((lend - lstart) as usize);
                    out = input;
                    if bad_input {
                        ann.unreadable.insert(Range::new(lstart, lend));
                    }
                } else {
                    let (data, err) = decompress_unit(&input, unit as usize);
                    decompressed_units += 1;
                    out = data;
                    out.truncate((lend - lstart) as usize);
                    if bad_input || err.is_some() {
                        bad_units += 1;
                        ann.unreadable.insert(Range::new(lstart, lend));
                        if let Some(e) = err {
                            report.notes.push(format!(
                                "compression unit at byte {lstart}: {e}; output may be incomplete"
                            ));
                        }
                    }
                }
            }
            // Bytes past the initialized size are zeros.
            if lend > n.initialized_size {
                let from = n.initialized_size.max(lstart);
                out[(from - lstart) as usize..].fill(0);
                ann.holes.insert(Range::new(from, lend));
            }
            sink.write_all(&out)?;
            report.bytes_written = lend;
        }
        if decompressed_units > 0 {
            report.notes.push(format!(
                "NTFS-compressed: {decompressed_units} compression unit(s) decompressed (LZNT1)"
            ));
        }
        if bad_units > 0 {
            report.notes.push(format!(
                "{bad_units} compression unit(s) could not be fully decompressed"
            ));
        }
        self.finish(n, ann, report);
        Ok(())
    }

    fn finish(&self, n: &NonResStream, ann: Annotations, report: &mut FileReadReport) {
        if n.initialized_size < n.data_size {
            report.notes.push(format!(
                "bytes {}..{} lie beyond the initialized size and read as zeros",
                n.initialized_size, n.data_size
            ));
        }
        if !n.complete {
            report
                .notes
                .push("part of the data mapping is lost (missing attribute pieces)".into());
        }
        if !ann.unreadable.is_empty() {
            report.notes.push(format!(
                "{} byte(s) unreadable and zero-filled",
                ann.unreadable.total_len()
            ));
        }
        if !ann.reused.is_empty() {
            report.notes.push(format!(
                "{} byte(s) lie in clusters now allocated to other data; likely overwritten",
                ann.reused.total_len()
            ));
        }
        report.holes = ann.holes;
        report.unreadable = ann.unreadable;
        report.reused = ann.reused;
    }
}
