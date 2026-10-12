//! The volume cluster bitmap (`$Bitmap`, MFT record 6, unnamed $DATA).
//!
//! One bit per cluster, least significant bit first, set = allocated. The
//! bitmap of a multi-terabyte volume is hundreds of megabytes, so it is
//! never loaded whole: 64 KiB chunks are read on demand through the stream
//! reader and kept in a small LRU cache. Bits that could not be read, or
//! that lie beyond the bitmap's data, are reported as
//! [`ClusterState::Unknown`] so callers can be honest about them.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::io::Disk;
use crate::stream::{read_plain, AllocationMap, Annotations, NonResStream, Stream, StreamData};
use crate::Result;

/// Allocation state of a cluster.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClusterState {
    /// Bit clear: free space.
    Free,
    /// Bit set: in use by a live file or metadata.
    Allocated,
    /// The bitmap could not be read for this cluster.
    Unknown,
}

const CHUNK: u64 = 64 * 1024;
const CACHE_CHUNKS: usize = 64;

struct Chunk {
    bytes: Vec<u8>,
    /// Bitmap-relative byte ranges that were unreadable.
    bad: diskrescue_core::RangeSet,
}

/// Cached chunks with their last-use tick, plus the tick counter.
type ChunkCache = (HashMap<u64, (Arc<Chunk>, u64)>, u64);

/// Lazily loaded cluster bitmap.
pub(crate) struct ClusterBitmap {
    data: BitmapData,
    total_clusters: u64,
    cluster: u64,
    cache: Mutex<ChunkCache>,
}

enum BitmapData {
    Resident(Vec<u8>),
    NonResident(NonResStream),
}

impl ClusterBitmap {
    /// Wrap the assembled `$Bitmap:$DATA` stream.
    pub fn new(stream: Stream, total_clusters: u64, cluster: u64) -> Self {
        let data = match stream.data {
            StreamData::Resident(v) => BitmapData::Resident(v),
            StreamData::NonResident(n) => BitmapData::NonResident(n),
        };
        ClusterBitmap {
            data,
            total_clusters,
            cluster,
            cache: Mutex::new((HashMap::new(), 0)),
        }
    }

    /// Bytes of bitmap data available.
    pub fn byte_len(&self) -> u64 {
        match &self.data {
            BitmapData::Resident(v) => v.len() as u64,
            BitmapData::NonResident(n) => n.data_size,
        }
    }

    fn chunk(&self, disk: &Disk, index: u64) -> Result<Arc<Chunk>> {
        {
            let mut g = self.cache.lock().unwrap_or_else(|p| p.into_inner());
            g.1 += 1;
            let tick = g.1;
            if let Some(e) = g.0.get_mut(&index) {
                e.1 = tick;
                return Ok(e.0.clone());
            }
        }
        let start = index * CHUNK;
        let len = self.byte_len().saturating_sub(start).min(CHUNK);
        let chunk = match &self.data {
            BitmapData::Resident(v) => Chunk {
                bytes: v[start as usize..(start + len) as usize].to_vec(),
                bad: Default::default(),
            },
            BitmapData::NonResident(n) => {
                let mut buf = vec![0u8; len as usize];
                let mut ann = Annotations::default();
                read_plain(disk, n, self.cluster, start, &mut buf, &mut ann, None, true)?;
                let mut bad = ann.unreadable;
                // Beyond the initialized size the bitmap is all zeros, which
                // a healthy volume never has for allocated metadata; trust it.
                bad = bad.shifted_down(start);
                Chunk { bytes: buf, bad }
            }
        };
        let chunk = Arc::new(chunk);
        let mut g = self.cache.lock().unwrap_or_else(|p| p.into_inner());
        if g.0.len() >= CACHE_CHUNKS {
            if let Some((&victim, _)) = g.0.iter().min_by_key(|(_, (_, t))| *t) {
                g.0.remove(&victim);
            }
        }
        let tick = g.1;
        g.0.insert(index, (chunk.clone(), tick));
        Ok(chunk)
    }
}

impl AllocationMap for ClusterBitmap {
    fn for_each_state(
        &self,
        disk: &Disk,
        start: u64,
        end: u64,
        f: &mut dyn FnMut(u64, u64, ClusterState),
    ) -> Result<()> {
        let mut run_start = start;
        let mut run_state: Option<ClusterState> = None;
        let mut emit = |c: u64, st: ClusterState, f: &mut dyn FnMut(u64, u64, ClusterState)| {
            if run_state != Some(st) {
                if let Some(prev) = run_state {
                    f(run_start, c, prev);
                }
                run_start = c;
                run_state = Some(st);
            }
        };
        let byte_len = self.byte_len();
        let mut c = start;
        while c < end {
            let byte = c / 8;
            if c >= self.total_clusters || byte >= byte_len {
                emit(c, ClusterState::Unknown, f);
                break;
            }
            let ci = byte / CHUNK;
            let chunk = self.chunk(disk, ci)?;
            let chunk_start_cluster = ci * CHUNK * 8;
            let chunk_end_cluster = (chunk_start_cluster + chunk.bytes.len() as u64 * 8)
                .min(end)
                .min(self.total_clusters);
            while c < chunk_end_cluster {
                let b = ((c / 8) - ci * CHUNK) as usize;
                if chunk.bad.contains(b as u64) {
                    emit(c, ClusterState::Unknown, f);
                    c = ((c / 8) + 1) * 8;
                    continue;
                }
                let v = chunk.bytes[b];
                if c % 8 == 0 && c + 8 <= chunk_end_cluster && (v == 0 || v == 0xFF) {
                    emit(
                        c,
                        if v == 0 {
                            ClusterState::Free
                        } else {
                            ClusterState::Allocated
                        },
                        f,
                    );
                    c += 8;
                    continue;
                }
                let bit = (v >> (c % 8)) & 1;
                emit(
                    c,
                    if bit == 1 {
                        ClusterState::Allocated
                    } else {
                        ClusterState::Free
                    },
                    f,
                );
                c += 1;
            }
            if chunk.bytes.is_empty() {
                // Defensive: no progress possible.
                emit(c, ClusterState::Unknown, f);
                break;
            }
        }
        if let Some(st) = run_state {
            f(run_start, end, st);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskrescue_core::source::MemSource;

    fn collect(bm: &ClusterBitmap, disk: &Disk, s: u64, e: u64) -> Vec<(u64, u64, ClusterState)> {
        let mut v = Vec::new();
        bm.for_each_state(disk, s, e, &mut |a, b, st| v.push((a, b, st)))
            .unwrap();
        v
    }

    #[test]
    fn runs_of_states() {
        let disk = Disk::new(Arc::new(MemSource::new(vec![0u8; 4096])));
        let stream = Stream {
            name: String::new(),
            flags: 0,
            data: StreamData::Resident(vec![0b0000_1111, 0xFF, 0x00, 0b1000_0000]),
            warnings: vec![],
        };
        let bm = ClusterBitmap::new(stream, 30, 4096);
        use ClusterState::*;
        assert_eq!(
            collect(&bm, &disk, 0, 30),
            vec![
                (0, 4, Allocated),
                (4, 8, Free),
                (8, 16, Allocated),
                (16, 30, Free)
            ]
        );
        assert_eq!(
            collect(&bm, &disk, 2, 10),
            vec![(2, 4, Allocated), (4, 8, Free), (8, 10, Allocated)]
        );
        // Beyond the volume: unknown.
        assert_eq!(
            collect(&bm, &disk, 28, 40),
            vec![(28, 30, Free), (30, 40, Unknown)]
        );
        assert!(collect(&bm, &disk, 5, 5).is_empty());
    }
}
