//! Streaming file content.
//!
//! The file's logical blocks are visited in order: mapped runs are read
//! from the source in chunks of up to 1 MiB, holes (unmapped blocks) and
//! uninitialised extents are written as zeros and listed in
//! [`FileReadReport::holes`], and blocks whose mapping is unknown or whose
//! data cannot be read are written as zeros and listed in
//! [`FileReadReport::unreadable`]. A failed chunk is retried block by block
//! and a failed block sector by sector, so one bad sector costs at most one
//! sector of data. Ranges the source already reports as unreadable (from a
//! rescue mapfile) are zero-filled and reported even if the image returns
//! bytes there. For deleted files, blocks that the block bitmap now marks
//! as in use are listed in [`FileReadReport::reused`].

use std::io::Write;

use diskrescue_core::fs::{EntryConfidence, FileEntry, FileReadReport};
use diskrescue_core::{Error, Range, RangeSet, Result};

use crate::inode::FileType;
use crate::ExtFs;

/// Bytes read from the source per request.
const CHUNK: u64 = 1 << 20;
static ZEROS: [u8; 64 * 1024] = [0u8; 64 * 1024];

fn write_zeros(sink: &mut dyn Write, mut n: u64) -> Result<()> {
    while n > 0 {
        let k = n.min(ZEROS.len() as u64) as usize;
        sink.write_all(&ZEROS[..k])?;
        n -= k as u64;
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Seg {
    Data(u64),
    Uninit,
    Unknown,
}

impl ExtFs {
    /// Read exactly `buf.len()` bytes at `offset`; `Err(n)` gives the
    /// number of leading bytes that were read before a failure or EOF.
    fn fill_exact(&self, offset: u64, buf: &mut [u8]) -> std::result::Result<(), usize> {
        let mut done = 0usize;
        while done < buf.len() {
            match self.src.read_at(offset + done as u64, &mut buf[done..]) {
                Ok(0) | Err(_) => return Err(done),
                Ok(n) => done += n,
            }
        }
        Ok(())
    }

    /// Read `len` bytes at `offset`, zero-filling what cannot be read
    /// (media errors, beyond the end of the source, known-bad ranges).
    /// Returns the bytes and the buffer-relative unreadable ranges.
    pub(crate) fn read_robust(&self, offset: u64, len: usize) -> (Vec<u8>, RangeSet) {
        let mut buf = vec![0u8; len];
        let mut bad = RangeSet::new();
        let size = self.src.size();
        let avail = if offset >= size {
            0
        } else {
            (size - offset).min(len as u64) as usize
        };
        if avail < len {
            bad.insert(Range::new(avail as u64, len as u64));
        }
        if let Err(good) = self.fill_exact(offset, &mut buf[..avail]) {
            self.salvage(offset, &mut buf[..avail], good, &mut bad);
        }
        if let Some(set) = &self.bad {
            for r in set
                .intersection_with_range(Range::with_len(offset, avail as u64))
                .iter()
            {
                let s = (r.start - offset) as usize;
                let e = (r.end - offset) as usize;
                buf[s..e].fill(0);
                bad.insert(Range::new(s as u64, e as u64));
            }
        }
        (buf, bad)
    }

    /// After a failed bulk read with `good` valid leading bytes, re-read the
    /// rest one filesystem block at a time, and failing blocks one sector at
    /// a time.
    fn salvage(&self, offset: u64, buf: &mut [u8], good: usize, bad: &mut RangeSet) {
        let bs = self.sb.block_size as u64;
        let sector = (self.src.sector_size() as u64).clamp(512, bs);
        let len = buf.len();
        let piece_end = |pos: usize, g: u64, limit: usize| -> usize {
            let abs = offset + pos as u64;
            (((abs / g) + 1) * g - offset).min(limit as u64) as usize
        };
        let mut pos = good;
        while pos < len {
            let end = piece_end(pos, bs, len);
            if self
                .fill_exact(offset + pos as u64, &mut buf[pos..end])
                .is_err()
            {
                let mut q = pos;
                while q < end {
                    let e2 = piece_end(q, sector, end);
                    if self.fill_exact(offset + q as u64, &mut buf[q..e2]).is_err() {
                        buf[q..e2].fill(0);
                        bad.insert(Range::new(q as u64, e2 as u64));
                    }
                    q = e2;
                }
            }
            pos = end;
        }
    }

    /// [`diskrescue_core::fs::FileSystem::read_file`].
    pub(crate) fn read_entry(
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
        if entry.confidence == EntryConfidence::DeletedNoData {
            return Err(Error::NotFound(format!(
                "{}: data location unknown (deleted inode without a block map); carve for it",
                entry.path
            )));
        }
        let ino = ExtFs::inode_number(entry);
        let inode = self.inode(ino)?;
        if inode.is_encrypted() {
            return Err(Error::unsupported(format!(
                "{}: encrypted (fscrypt) file",
                entry.path
            )));
        }
        let mut rep = FileReadReport::default();
        match inode.file_type() {
            FileType::Regular | FileType::Symlink => {}
            FileType::Directory => {
                return Err(Error::InvalidArgument(format!(
                    "{}: inode {ino} is a directory",
                    entry.path
                )))
            }
            _ => return Ok(rep),
        }
        let bs = self.sb.block_size as u64;
        if inode.has_inline_data() || inode.is_fast_symlink(self.sb.block_size) {
            let data = if inode.has_inline_data() {
                inode.inline_data()
            } else {
                inode.fast_symlink_target().to_vec()
            };
            sink.write_all(&data)?;
            let size = inode.size;
            if (data.len() as u64) < size {
                write_zeros(sink, size - data.len() as u64)?;
                rep.unreadable.insert(Range::new(data.len() as u64, size));
                rep.notes
                    .push("inline data shorter than the file size; tail zero-filled".into());
            }
            rep.bytes_written = size.max(data.len() as u64);
            return Ok(rep);
        }
        let deleted = entry.deleted;
        let (limit, guessed) = self.effective_limit(&inode, deleted);
        let mapping = self.map_inode(&inode, limit);
        let size = if guessed {
            rep.notes
                .push("size zeroed on deletion; using the mapped length".into());
            mapping.mapped_end().saturating_mul(bs)
        } else {
            inode.size
        };
        let check_reuse = deleted && entry.confidence != EntryConfidence::DeletedIntact;
        let nblocks = size.div_ceil(bs);
        let mut segs: Vec<(u64, u64, Seg)> = mapping
            .runs
            .iter()
            .map(|r| {
                let kind = if r.uninit {
                    Seg::Uninit
                } else {
                    Seg::Data(r.physical)
                };
                (r.logical, r.logical_end(), kind)
            })
            .chain(mapping.unknown.iter().map(|&(s, e)| (s, e, Seg::Unknown)))
            .collect();
        segs.sort_by_key(|s| s.0);
        let mut written = 0u64;
        let mut cursor = 0u64;
        let mut noted_uninit = false;
        let mut noted_unknown = false;
        for (start, end, kind) in segs {
            if start >= nblocks {
                break;
            }
            let s = start.max(cursor);
            let e = end.min(nblocks);
            if s >= e {
                continue;
            }
            if s > cursor {
                let r = file_range(cursor, s, bs, size);
                write_zeros(sink, r.len())?;
                rep.holes.insert(r);
                written += r.len();
            }
            let r = file_range(s, e, bs, size);
            match kind {
                Seg::Uninit => {
                    write_zeros(sink, r.len())?;
                    rep.holes.insert(r);
                    if !noted_uninit {
                        rep.notes
                            .push("uninitialised (preallocated) extents read as zeros".into());
                        noted_uninit = true;
                    }
                }
                Seg::Unknown => {
                    write_zeros(sink, r.len())?;
                    rep.unreadable.insert(r);
                    if !noted_unknown {
                        rep.notes.push(format!(
                            "block mapping partly unreadable: {}",
                            mapping.notes.join("; ")
                        ));
                        noted_unknown = true;
                    }
                }
                Seg::Data(phys) => {
                    let phys_start = (phys + (s - start)).saturating_mul(bs);
                    self.copy_run(phys_start, r, check_reuse, sink, &mut rep)?;
                }
            }
            written += r.len();
            cursor = e;
        }
        if cursor < nblocks {
            let r = file_range(cursor, nblocks, bs, size);
            write_zeros(sink, r.len())?;
            rep.holes.insert(r);
            written += r.len();
        }
        if !rep.unreadable.is_empty() {
            rep.notes.push(format!(
                "{} byte(s) unreadable, zero-filled",
                rep.unreadable.total_len()
            ));
        }
        if !rep.reused.is_empty() {
            rep.notes.push(format!(
                "{} byte(s) lie in blocks now allocated to other files",
                rep.reused.total_len()
            ));
        }
        rep.bytes_written = written;
        Ok(rep)
    }

    /// Copy the file range `r` whose data starts at volume byte
    /// `phys_start`.
    fn copy_run(
        &self,
        phys_start: u64,
        r: Range,
        check_reuse: bool,
        sink: &mut dyn Write,
        rep: &mut FileReadReport,
    ) -> Result<()> {
        let bs = self.sb.block_size as u64;
        let mut off = 0u64;
        while off < r.len() {
            let n = (r.len() - off).min(CHUNK);
            let phys = phys_start.saturating_add(off);
            let (buf, bad) = self.read_robust(phys, n as usize);
            for b in bad.iter() {
                rep.unreadable
                    .insert(Range::new(r.start + off + b.start, r.start + off + b.end));
            }
            sink.write_all(&buf)?;
            if check_reuse {
                let mut k = 0u64;
                while k < n {
                    if self.block_bitmap_bit(phys.saturating_add(k) / bs) == Some(true) {
                        let e = (k + bs).min(n);
                        rep.reused
                            .insert(Range::new(r.start + off + k, r.start + off + e));
                    }
                    k += bs;
                }
            }
            off += n;
        }
        Ok(())
    }
}

/// File byte range of logical blocks `[s, e)`, clipped to `size`.
fn file_range(s: u64, e: u64, bs: u64, size: u64) -> Range {
    Range::new(
        s.saturating_mul(bs).min(size),
        e.saturating_mul(bs).min(size),
    )
}
