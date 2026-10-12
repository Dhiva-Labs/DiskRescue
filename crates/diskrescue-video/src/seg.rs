//! A read-only [`Source`] assembled from pieces: literal bytes and slices of
//! another source.
//!
//! Repairs describe their output as a list of segments (patched headers,
//! runs copied from the damaged input, a freshly built index) instead of
//! writing it directly. That lets the repair validate the *planned* output
//! with the same parsers that validate any other file before a single byte
//! is written, and then stream it out in bounded chunks, without temporary
//! files and without holding the media payload in memory.

use std::io::Write;

use diskrescue_core::{ReadError, ReadErrorKind, SharedSource, Source};

/// One piece of the assembled output.
enum Piece {
    /// Literal bytes.
    Mem(Vec<u8>),
    /// `len` bytes of the input starting at `off`.
    Src { off: u64, len: u64 },
}

struct Seg {
    /// Offset of this piece in the assembled output.
    start: u64,
    len: u64,
    piece: Piece,
}

/// Output assembled from literal bytes and slices of an input source.
pub(crate) struct SegSource {
    src: SharedSource,
    segs: Vec<Seg>,
    total: u64,
}

impl SegSource {
    /// Start an empty assembly reading input slices from `src`.
    pub fn new(src: SharedSource) -> Self {
        SegSource {
            src,
            segs: Vec::new(),
            total: 0,
        }
    }

    /// Length assembled so far.
    pub fn len(&self) -> u64 {
        self.total
    }

    /// Append literal bytes.
    pub fn push_mem(&mut self, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        let len = bytes.len() as u64;
        self.segs.push(Seg {
            start: self.total,
            len,
            piece: Piece::Mem(bytes),
        });
        self.total += len;
    }

    /// Append `len` bytes of the input starting at `off`. Adjacent slices are
    /// merged.
    pub fn push_src(&mut self, off: u64, len: u64) {
        if len == 0 {
            return;
        }
        if let Some(Seg {
            len: l,
            piece: Piece::Src { off: o, len: pl },
            ..
        }) = self.segs.last_mut()
        {
            if *o + *pl == off {
                *pl += len;
                *l += len;
                self.total += len;
                return;
            }
        }
        self.segs.push(Seg {
            start: self.total,
            len,
            piece: Piece::Src { off, len },
        });
        self.total += len;
    }

    /// Stream the assembled output into `out`. Unreadable bytes in the input
    /// are replaced by zeros (one sector at a time) so the output keeps its
    /// layout; the number of replaced bytes is returned.
    pub fn write_to(&self, out: &mut dyn Write) -> std::io::Result<u64> {
        let mut zero_filled = 0u64;
        let mut buf = vec![0u8; 1 << 20];
        for seg in &self.segs {
            match &seg.piece {
                Piece::Mem(b) => out.write_all(b)?,
                Piece::Src { off, len } => {
                    let mut done = 0u64;
                    while done < *len {
                        let want = ((*len - done).min(buf.len() as u64)) as usize;
                        match self.src.read_at(off + done, &mut buf[..want]) {
                            Ok(0) => {
                                // The source is shorter than the plan; keep
                                // the layout with zeros.
                                let n = want;
                                buf[..n].fill(0);
                                out.write_all(&buf[..n])?;
                                zero_filled += n as u64;
                                done += n as u64;
                            }
                            Ok(n) => {
                                out.write_all(&buf[..n])?;
                                done += n as u64;
                            }
                            Err(e) => {
                                if !e.kind.is_media_error() && e.kind != ReadErrorKind::OutOfRange {
                                    return Err(e.into());
                                }
                                let sector = self.src.sector_size().max(512) as u64;
                                let at = e.offset.max(off + done);
                                let next = (at / sector + 1) * sector;
                                let skip = (next - (off + done)).min(*len - done);
                                // Keep any bytes before the failing offset
                                // that were not delivered.
                                buf[..skip as usize].fill(0);
                                out.write_all(&buf[..skip as usize])?;
                                zero_filled += skip;
                                done += skip;
                            }
                        }
                    }
                }
            }
        }
        out.flush()?;
        Ok(zero_filled)
    }
}

impl Source for SegSource {
    fn size(&self) -> u64 {
        self.total
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, ReadError> {
        if offset >= self.total || buf.is_empty() {
            return Ok(0);
        }
        let want = ((self.total - offset).min(buf.len() as u64)) as usize;
        let mut done = 0usize;
        let mut idx = self.segs.partition_point(|s| s.start + s.len <= offset);
        while done < want && idx < self.segs.len() {
            let seg = &self.segs[idx];
            let pos = offset + done as u64;
            let within = pos - seg.start;
            let n = ((seg.len - within).min((want - done) as u64)) as usize;
            match &seg.piece {
                Piece::Mem(b) => {
                    buf[done..done + n].copy_from_slice(&b[within as usize..within as usize + n]);
                    done += n;
                }
                Piece::Src { off, .. } => {
                    let got = self
                        .src
                        .read_at(off + within, &mut buf[done..done + n])
                        .map_err(|mut e| {
                            e.offset = seg.start + e.offset.saturating_sub(*off);
                            e
                        })?;
                    if got == 0 {
                        break;
                    }
                    done += got;
                    if got < n {
                        // Short read inside a piece: report what we have.
                        break;
                    }
                }
            }
            idx += 1;
        }
        Ok(done)
    }

    fn describe(&self) -> String {
        format!("repaired output ({} bytes)", self.total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskrescue_core::source::MemSource;
    use diskrescue_core::{Range, RangeSet, SourceExt};
    use std::sync::Arc;

    fn base() -> SharedSource {
        Arc::new(MemSource::new((0..200u8).collect()))
    }

    #[test]
    fn assembles_and_reads_across_pieces() {
        let mut s = SegSource::new(base());
        s.push_mem(vec![1, 2, 3]);
        s.push_src(10, 5);
        s.push_src(15, 5); // merged
        s.push_mem(vec![9]);
        s.push_src(100, 2);
        assert_eq!(s.len(), 3 + 10 + 1 + 2);
        let all = s.read_exact_vec(0, 16).unwrap();
        assert_eq!(
            all,
            vec![1, 2, 3, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 9, 100, 101]
        );
        assert_eq!(s.read_vec(12, 100).unwrap(), vec![19, 9, 100, 101]);
        assert_eq!(s.read_vec(16, 4).unwrap().len(), 0);
        let mut out = Vec::new();
        assert_eq!(s.write_to(&mut out).unwrap(), 0);
        assert_eq!(out, all);
    }

    #[test]
    fn unreadable_input_becomes_zeros_in_output() {
        let mut bad = RangeSet::new();
        bad.insert(Range::new(1024, 1536));
        let data: Vec<u8> = (0..4096u32).map(|i| (i % 250) as u8 + 1).collect();
        let src: SharedSource = Arc::new(MemSource::new(data.clone()).with_unreadable(bad));
        let mut s = SegSource::new(src);
        s.push_src(0, 4096);
        let mut out = Vec::new();
        let zeroed = s.write_to(&mut out).unwrap();
        assert_eq!(zeroed, 512);
        assert_eq!(out.len(), 4096);
        assert_eq!(&out[..1024], &data[..1024]);
        assert!(out[1024..1536].iter().all(|&b| b == 0));
        assert_eq!(&out[1536..], &data[1536..]);
    }
}
