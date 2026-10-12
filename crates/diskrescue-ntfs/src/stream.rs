//! Attribute streams: assembling the pieces of one attribute (possibly
//! spread over several extension records) into a single extent map, and
//! reading logical byte ranges from it.
//!
//! An assembled non-resident stream is a list of [`Extent`]s that covers
//! VCN 0 up to the end of the allocation without gaps. Each extent is
//! either real clusters, a sparse hole, or *missing*: VCNs whose mapping is
//! unknown because a piece of the attribute could not be loaded or its run
//! list was damaged. Missing bytes read as zeros and are reported as
//! unreadable, never as legitimate holes.
//!
//! Sizes come from the piece whose lowest VCN is 0. Bytes beyond the
//! initialized size read as zeros (and are reported as holes); bytes beyond
//! the mapped extents read as zeros and are reported as unreadable.

use diskrescue_core::{Range, RangeSet};

use crate::attr::{Attribute, AttributeBody, ATTR_COMPRESSION_MASK};
use crate::io::Disk;
use crate::Result;

/// What a range of VCNs maps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExtentKind {
    /// Real clusters starting at this LCN.
    Real(u64),
    /// A hole that reads as zeros.
    Sparse,
    /// Mapping unknown (piece or run list lost).
    Missing,
}

/// A contiguous VCN range of a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Extent {
    pub vcn: u64,
    pub len: u64,
    pub kind: ExtentKind,
}

impl Extent {
    pub fn end(&self) -> u64 {
        self.vcn.saturating_add(self.len)
    }
}

/// A non-resident stream assembled from all its pieces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NonResStream {
    pub extents: Vec<Extent>,
    pub data_size: u64,
    pub initialized_size: u64,
    pub allocated_size: u64,
    pub compression_unit: u8,
    /// Every VCN of the allocation is mapped (no missing extents).
    pub complete: bool,
    /// The piece carrying the sizes (lowest VCN 0) was present.
    pub sizes_known: bool,
}

impl NonResStream {
    /// One past the last mapped VCN.
    pub fn vcn_end(&self) -> u64 {
        self.extents.last().map(|e| e.end()).unwrap_or(0)
    }

    /// Index of the extent containing `vcn`, if any.
    pub fn find(&self, vcn: u64) -> Option<usize> {
        let i = self.extents.partition_point(|e| e.end() <= vcn);
        (i < self.extents.len() && self.extents[i].vcn <= vcn).then_some(i)
    }

    /// Volume-relative byte ranges of the real clusters, in file order,
    /// clipped to `limit` logical bytes (pass `u64::MAX` for no clipping).
    pub fn byte_extents(&self, cluster: u64, limit: u64) -> Vec<Range> {
        let mut out: Vec<Range> = Vec::new();
        for e in &self.extents {
            let ExtentKind::Real(lcn) = e.kind else {
                continue;
            };
            let lstart = e.vcn.saturating_mul(cluster);
            if lstart >= limit {
                break;
            }
            let bytes = e.len.saturating_mul(cluster).min(limit - lstart);
            let pstart = lcn.saturating_mul(cluster);
            let r = Range::with_len(pstart, bytes);
            match out.last_mut() {
                Some(last) if last.end == r.start => last.end = r.end,
                _ => out.push(r),
            }
        }
        out
    }

    /// Real cluster runs as `(lcn, count)`, clipped to `limit_vcn`.
    pub fn real_clusters(&self, limit_vcn: u64) -> Vec<(u64, u64)> {
        let mut out = Vec::new();
        for e in &self.extents {
            if e.vcn >= limit_vcn {
                break;
            }
            if let ExtentKind::Real(lcn) = e.kind {
                out.push((lcn, e.len.min(limit_vcn - e.vcn)));
            }
        }
        out
    }
}

/// The content of an attribute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StreamData {
    Resident(Vec<u8>),
    NonResident(NonResStream),
}

/// One attribute stream (type + name), assembled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Stream {
    pub name: String,
    pub flags: u16,
    pub data: StreamData,
    pub warnings: Vec<String>,
}

impl Stream {
    /// Logical size in bytes.
    pub fn size(&self) -> u64 {
        match &self.data {
            StreamData::Resident(v) => v.len() as u64,
            StreamData::NonResident(n) => n.data_size,
        }
    }

    /// True when the attribute header says compressed and the stream is
    /// non-resident with a compression unit.
    pub fn is_compressed(&self) -> bool {
        match &self.data {
            StreamData::Resident(_) => false,
            StreamData::NonResident(n) => {
                self.flags & ATTR_COMPRESSION_MASK != 0 && n.compression_unit != 0
            }
        }
    }

    /// The non-resident part, if any.
    pub fn non_resident(&self) -> Option<&NonResStream> {
        match &self.data {
            StreamData::NonResident(n) => Some(n),
            StreamData::Resident(_) => None,
        }
    }
}

/// Assemble the pieces of one attribute stream. `pieces` must all share
/// type and name. Returns `None` for an empty slice.
pub(crate) fn assemble(pieces: &[&Attribute], cluster: u64) -> Option<Stream> {
    let first = pieces.first()?;
    let mut warnings = Vec::new();
    if let Some(res) = pieces.iter().find_map(|a| match &a.body {
        AttributeBody::Resident(r) => Some(r),
        AttributeBody::NonResident(_) => None,
    }) {
        if pieces.len() > 1 {
            warnings.push(format!(
                "attribute \"{}\" has a resident value and {} other piece(s); using the resident value",
                first.name,
                pieces.len() - 1
            ));
        }
        return Some(Stream {
            name: first.name.clone(),
            flags: first.flags,
            data: StreamData::Resident(res.value.clone()),
            warnings,
        });
    }
    let mut nr: Vec<(&Attribute, &crate::attr::NonResidentInfo)> = pieces
        .iter()
        .filter_map(|a| a.non_resident().map(|n| (*a, n)))
        .collect();
    nr.sort_by_key(|(_, n)| n.lowest_vcn);
    let head = nr.iter().find(|(_, n)| n.lowest_vcn == 0).map(|(_, n)| *n);
    let flags = nr
        .iter()
        .find(|(_, n)| n.lowest_vcn == 0)
        .map(|(a, _)| a.flags)
        .unwrap_or(first.flags);
    let mut extents: Vec<Extent> = Vec::new();
    let mut cur = 0u64;
    let mut complete = true;
    for (a, n) in &nr {
        if n.lowest_vcn < cur {
            warnings.push(format!(
                "attribute piece in record {} overlaps VCN {}; ignored",
                a.record, cur
            ));
            continue;
        }
        if n.lowest_vcn > cur {
            complete = false;
            warnings.push(format!(
                "VCNs {cur}..{} of attribute \"{}\" are not mapped (missing piece)",
                n.lowest_vcn, a.name
            ));
            extents.push(Extent {
                vcn: cur,
                len: n.lowest_vcn - cur,
                kind: ExtentKind::Missing,
            });
            cur = n.lowest_vcn;
        }
        for r in &n.runs {
            if r.vcn != cur {
                break;
            }
            extents.push(Extent {
                vcn: r.vcn,
                len: r.len,
                kind: match r.lcn {
                    Some(l) => ExtentKind::Real(l),
                    None => ExtentKind::Sparse,
                },
            });
            cur = r.end_vcn();
        }
        if let Some(e) = &n.runs_error {
            warnings.push(format!(
                "data runs of attribute \"{}\" in record {}: {e}",
                a.name, a.record
            ));
        }
        let piece_end = n.highest_vcn.wrapping_add(1);
        if n.highest_vcn != u64::MAX && piece_end > cur {
            complete = false;
            extents.push(Extent {
                vcn: cur,
                len: piece_end - cur,
                kind: ExtentKind::Missing,
            });
            cur = piece_end;
        }
    }
    let (data_size, mut initialized_size, allocated_size, compression_unit, sizes_known) =
        match head {
            Some(h) => (
                h.data_size,
                h.initialized_size,
                h.allocated_size,
                h.compression_unit,
                true,
            ),
            None => {
                complete = false;
                warnings.push(format!(
                    "first piece of attribute \"{}\" is missing; sizes estimated from the runs",
                    first.name
                ));
                let bytes = cur.saturating_mul(cluster);
                let cu = nr.first().map(|(_, n)| n.compression_unit).unwrap_or(0);
                (bytes, bytes, bytes, cu, false)
            }
        };
    // A tail of the allocation not described by any piece.
    let alloc_vcns = allocated_size.div_ceil(cluster.max(1));
    let needed_vcns = data_size.div_ceil(cluster.max(1));
    let want = alloc_vcns.min(needed_vcns.max(cur));
    if want > cur {
        complete = false;
        extents.push(Extent {
            vcn: cur,
            len: want - cur,
            kind: ExtentKind::Missing,
        });
    }
    if initialized_size > data_size {
        initialized_size = data_size;
    }
    Some(Stream {
        name: first.name.clone(),
        flags,
        data: StreamData::NonResident(NonResStream {
            extents,
            data_size,
            initialized_size,
            allocated_size,
            compression_unit,
            complete,
            sizes_known,
        }),
        warnings,
    })
}

/// File-relative annotations collected while reading a stream.
#[derive(Debug, Clone, Default)]
pub(crate) struct Annotations {
    pub holes: RangeSet,
    pub unreadable: RangeSet,
    pub reused: RangeSet,
}

/// Something that can say which clusters are currently allocated.
pub(crate) trait AllocationMap {
    /// Calls `f(start_lcn, end_lcn, state)` for consecutive runs of equal
    /// state covering `[start, end)`.
    fn for_each_state(
        &self,
        disk: &Disk,
        start: u64,
        end: u64,
        f: &mut dyn FnMut(u64, u64, crate::bitmap::ClusterState),
    ) -> Result<()>;
}

/// Read logical bytes `[off, off + buf.len())` of an uncompressed
/// non-resident stream into `buf`. `logical_base` is added to every
/// annotation (so callers can read a window of a larger stream). When
/// `reuse` is given, clusters it reports as allocated are annotated as
/// reused.
#[allow(clippy::too_many_arguments)]
pub(crate) fn read_plain(
    disk: &Disk,
    s: &NonResStream,
    cluster: u64,
    off: u64,
    buf: &mut [u8],
    ann: &mut Annotations,
    reuse: Option<&dyn AllocationMap>,
    cached: bool,
) -> Result<()> {
    let len = buf.len() as u64;
    let end = off.saturating_add(len);
    buf.fill(0);
    // Bytes beyond the initialized size are zeros by definition.
    let init_end = s.initialized_size.max(off).min(end);
    if init_end < end {
        ann.holes.insert(Range::new(init_end, end));
    }
    let mut pos = off;
    while pos < init_end {
        let vcn = pos / cluster;
        let Some(i) = s.find(vcn) else {
            ann.unreadable.insert(Range::new(pos, init_end));
            break;
        };
        let e = s.extents[i];
        let ext_end_byte = e.end().saturating_mul(cluster).min(init_end);
        let piece_end = ext_end_byte.max(pos + 1).min(init_end);
        let a = (pos - off) as usize;
        let b = (piece_end - off) as usize;
        match e.kind {
            ExtentKind::Sparse => {
                ann.holes.insert(Range::new(pos, piece_end));
            }
            ExtentKind::Missing => {
                ann.unreadable.insert(Range::new(pos, piece_end));
            }
            ExtentKind::Real(lcn) => {
                let within = pos - e.vcn.saturating_mul(cluster);
                let phys = lcn.checked_mul(cluster).and_then(|p| p.checked_add(within));
                match phys {
                    None => {
                        ann.unreadable.insert(Range::new(pos, piece_end));
                    }
                    Some(p) => {
                        let bad = disk.read(p, &mut buf[a..b], cached)?;
                        for r in bad.iter() {
                            let ls = pos + (r.start - p);
                            ann.unreadable.insert(Range::new(ls, ls + r.len()));
                        }
                        if let Some(map) = reuse {
                            let c0 = lcn + within / cluster;
                            let c1 = lcn + (within + (piece_end - pos)).div_ceil(cluster);
                            let base_l = pos - (within % cluster);
                            map.for_each_state(disk, c0, c1, &mut |cs, ce, st| {
                                if st == crate::bitmap::ClusterState::Allocated {
                                    let ls = base_l + (cs - c0) * cluster;
                                    let le = base_l + (ce - c0) * cluster;
                                    let r = Range::new(ls.max(pos), le.min(piece_end));
                                    if !r.is_empty() {
                                        ann.reused.insert(r);
                                    }
                                }
                            })?;
                        }
                    }
                }
            }
        }
        pos = piece_end;
    }
    Ok(())
}

/// Read a whole (small) stream into memory, capped at `max` bytes. Used for
/// attribute lists, index allocations' bitmaps and similar metadata.
pub(crate) fn read_all(
    disk: &Disk,
    stream: &Stream,
    cluster: u64,
    max: u64,
) -> Result<(Vec<u8>, Annotations)> {
    let mut ann = Annotations::default();
    match &stream.data {
        StreamData::Resident(v) => Ok((v.clone(), ann)),
        StreamData::NonResident(n) => {
            let size = n.data_size.min(max);
            let mut buf = vec![0u8; size as usize];
            read_plain(disk, n, cluster, 0, &mut buf, &mut ann, None, true)?;
            Ok((buf, ann))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attr::NonResidentInfo;
    use crate::runs::Run;
    use diskrescue_core::source::MemSource;
    use std::sync::Arc;

    fn piece(
        lowest: u64,
        highest: u64,
        runs: Vec<Run>,
        sizes: (u64, u64, u64),
        record: u64,
    ) -> Attribute {
        Attribute {
            type_code: 0x80,
            name: String::new(),
            flags: 0,
            instance: 0,
            record,
            offset: 0,
            body: AttributeBody::NonResident(NonResidentInfo {
                lowest_vcn: lowest,
                highest_vcn: highest,
                compression_unit: 0,
                allocated_size: sizes.0,
                data_size: sizes.1,
                initialized_size: sizes.2,
                compressed_size: None,
                runs,
                runs_error: None,
            }),
        }
    }

    #[test]
    fn splices_pieces_and_marks_gaps() {
        let p0 = piece(
            0,
            3,
            vec![Run {
                vcn: 0,
                lcn: Some(10),
                len: 4,
            }],
            (12 * 512, 12 * 512 - 100, 12 * 512 - 100),
            70,
        );
        let p2 = piece(
            8,
            11,
            vec![Run {
                vcn: 8,
                lcn: Some(40),
                len: 4,
            }],
            (0, 0, 0),
            71,
        );
        let s = assemble(&[&p2, &p0], 512).unwrap();
        let n = s.non_resident().unwrap();
        assert!(!n.complete);
        assert_eq!(n.extents.len(), 3);
        assert_eq!(n.extents[1].kind, ExtentKind::Missing);
        assert_eq!(n.extents[1].vcn, 4);
        assert_eq!(n.extents[1].len, 4);
        assert_eq!(n.vcn_end(), 12);
        assert_eq!(n.data_size, 12 * 512 - 100);
        let bx = n.byte_extents(512, n.data_size);
        assert_eq!(bx[0], Range::new(10 * 512, 14 * 512));
        assert_eq!(bx[1], Range::new(40 * 512, 40 * 512 + 4 * 512 - 100));
    }

    #[test]
    fn reads_with_holes_missing_and_init_size() {
        let mut img = vec![0u8; 64 * 512];
        for (i, b) in img.iter_mut().enumerate() {
            *b = (i / 512) as u8 + 1;
        }
        let disk = Disk::new(Arc::new(MemSource::new(img)));
        let p0 = piece(
            0,
            5,
            vec![
                Run {
                    vcn: 0,
                    lcn: Some(10),
                    len: 2,
                },
                Run {
                    vcn: 2,
                    lcn: None,
                    len: 1,
                },
                Run {
                    vcn: 3,
                    lcn: Some(20),
                    len: 1,
                },
            ],
            (6 * 512, 6 * 512, 5 * 512 + 10),
            1,
        );
        let s = assemble(&[&p0], 512).unwrap();
        let n = s.non_resident().unwrap();
        let mut buf = vec![0xAAu8; 6 * 512];
        let mut ann = Annotations::default();
        read_plain(&disk, n, 512, 0, &mut buf, &mut ann, None, false).unwrap();
        assert!(buf[..512].iter().all(|&b| b == 11));
        assert!(buf[512..1024].iter().all(|&b| b == 12));
        assert!(buf[1024..1536].iter().all(|&b| b == 0));
        assert!(buf[1536..2048].iter().all(|&b| b == 21));
        // VCN 4..6 missing; past initialized size zeros.
        assert!(buf[2048..].iter().all(|&b| b == 0));
        assert!(ann.holes.contains(1024));
        assert!(ann.holes.contains(5 * 512 + 10));
        assert!(ann.unreadable.contains(2048));
        assert!(!ann.unreadable.contains(5 * 512 + 10));
    }
}
