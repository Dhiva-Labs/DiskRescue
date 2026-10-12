//! Positional, read-only byte access.
//!
//! A [`Source`] is anything DiskRescue can read bytes from at an offset: a
//! block device, an image file, a partition inside either, an in-memory
//! buffer for tests, or a simulated failing device. The contract is small on
//! purpose so that device-specific behaviour (timeouts, re-opening after a
//! USB drop, direct I/O) lives entirely inside the implementation.
//!
//! # Contract
//!
//! * `read_at(offset, buf)` reads up to `buf.len()` bytes starting at
//!   `offset`. It returns the number of bytes placed at the start of `buf`.
//!   `Ok(0)` means `offset >= size()`. A short read (`0 < n < buf.len()`)
//!   happens at the end of the source, and may also happen just before an
//!   unreadable region: the implementation returns the good prefix, and a
//!   read starting at `offset + n` then reports the error. Callers that need
//!   every byte loop until `Ok(0)` or an error (see [`SourceExt`]).
//! * On failure it returns a [`ReadError`] that names the first offset the
//!   implementation could not read. Bytes before `err.offset` may or may not
//!   have been written to `buf`; callers must not rely on them.
//! * Implementations must be safe to call from several threads at once.
//! * Implementations never write to the underlying medium.
//! * Direct-I/O backed devices need `offset` and `buf.len()` to be multiples
//!   of [`Source::sector_size`]; they return [`ReadErrorKind::Unaligned`]
//!   otherwise. File and memory sources accept any alignment.

use std::fmt;
use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::ranges::RangeSet;

/// Why a read failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum ReadErrorKind {
    /// The medium reported an error (EIO and friends): a bad sector.
    Io,
    /// The read did not complete within the allowed time. The sector may be
    /// readable on retry, but the device is struggling.
    Timeout,
    /// The device disappeared (ENODEV/ENXIO, or the path vanished). Typical
    /// for USB flash drives and SD cards whose controller reset.
    DeviceGone,
    /// Offset or length violates the alignment the source requires.
    Unaligned,
    /// Offset is beyond the end of the source.
    OutOfRange,
    /// The source cannot perform this operation (e.g. reopen on a file).
    Unsupported,
}

impl ReadErrorKind {
    /// Whether the engine should consider the bytes at the failing offset
    /// genuinely unreadable (as opposed to a transient condition).
    pub fn is_media_error(self) -> bool {
        matches!(self, ReadErrorKind::Io | ReadErrorKind::Timeout)
    }
}

/// A failed read, with enough context to update a block map.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ReadError {
    /// First byte offset the implementation could not read.
    pub offset: u64,
    /// Length of the read that was attempted (from its own start).
    pub len: usize,
    /// Classification.
    pub kind: ReadErrorKind,
    /// Raw OS error number, when there is one.
    pub os_error: Option<i32>,
    /// Human-readable detail (OS message, "timed out after 30s", ...).
    pub detail: String,
}

impl ReadError {
    /// Build a read error.
    pub fn new(offset: u64, len: usize, kind: ReadErrorKind, detail: impl Into<String>) -> Self {
        ReadError {
            offset,
            len,
            kind,
            os_error: None,
            detail: detail.into(),
        }
    }

    /// Build a read error from an `std::io::Error`, classifying ENODEV/ENXIO
    /// as [`ReadErrorKind::DeviceGone`], timeouts as
    /// [`ReadErrorKind::Timeout`] and everything else as [`ReadErrorKind::Io`].
    pub fn from_io(offset: u64, len: usize, err: &std::io::Error) -> Self {
        let os = err.raw_os_error();
        let kind = match (err.kind(), os) {
            (std::io::ErrorKind::TimedOut, _) => ReadErrorKind::Timeout,
            // ENODEV = 19, ENXIO = 6 on Linux; ENOMEDIUM = 123.
            #[cfg(target_os = "linux")]
            (_, Some(19)) | (_, Some(6)) | (_, Some(123)) => ReadErrorKind::DeviceGone,
            // Windows: ERROR_DEVICE_NOT_CONNECTED = 1167, ERROR_NO_SUCH_DEVICE = 433,
            // ERROR_FILE_NOT_FOUND = 2, ERROR_NOT_READY = 21.
            #[cfg(windows)]
            (_, Some(1167)) | (_, Some(433)) | (_, Some(2)) | (_, Some(21)) => {
                ReadErrorKind::DeviceGone
            }
            _ => ReadErrorKind::Io,
        };
        ReadError {
            offset,
            len,
            kind,
            os_error: os,
            detail: err.to_string(),
        }
    }

    /// Error for an offset past the end of the source.
    pub fn out_of_range(offset: u64, len: usize, size: u64) -> Self {
        ReadError::new(
            offset,
            len,
            ReadErrorKind::OutOfRange,
            format!("offset {offset} is beyond source size {size}"),
        )
    }

    /// Error for a misaligned direct read.
    pub fn unaligned(offset: u64, len: usize, sector: u32) -> Self {
        ReadError::new(
            offset,
            len,
            ReadErrorKind::Unaligned,
            format!("offset {offset} / length {len} not aligned to sector size {sector}"),
        )
    }
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "read of {} bytes at offset {} failed ({:?}): {}",
            self.len, self.offset, self.kind, self.detail
        )
    }
}

impl std::error::Error for ReadError {}

impl From<ReadError> for std::io::Error {
    fn from(e: ReadError) -> Self {
        let kind = match e.kind {
            ReadErrorKind::Timeout => std::io::ErrorKind::TimedOut,
            ReadErrorKind::DeviceGone => std::io::ErrorKind::NotConnected,
            ReadErrorKind::OutOfRange => std::io::ErrorKind::UnexpectedEof,
            ReadErrorKind::Unaligned => std::io::ErrorKind::InvalidInput,
            ReadErrorKind::Unsupported => std::io::ErrorKind::Unsupported,
            ReadErrorKind::Io => std::io::ErrorKind::Other,
        };
        std::io::Error::new(kind, e)
    }
}

/// Read-only positional access to bytes. See the module docs for the contract.
pub trait Source: Send + Sync {
    /// Total size in bytes.
    fn size(&self) -> u64;

    /// Logical sector size. Reads on direct-I/O devices must be aligned to it.
    /// Files and memory buffers report 512 but accept any alignment.
    fn sector_size(&self) -> u32 {
        512
    }

    /// Read up to `buf.len()` bytes at `offset`. See module docs.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, ReadError>;

    /// Like [`Source::read_at`] but gives up after `timeout`, returning
    /// [`ReadErrorKind::Timeout`]. Sources that cannot enforce a timeout
    /// (files, memory) simply ignore it.
    fn read_at_timeout(
        &self,
        offset: u64,
        buf: &mut [u8],
        timeout: Duration,
    ) -> Result<usize, ReadError> {
        let _ = timeout;
        self.read_at(offset, buf)
    }

    /// Short human description ("/dev/sdb (SanDisk Ultra, 30.0 GB)",
    /// "image.img", "partition 2 of /dev/sdb").
    fn describe(&self) -> String;

    /// True for real block devices, which deserve timeouts and care.
    fn is_device(&self) -> bool {
        false
    }

    /// Whether reads must be sector aligned (direct I/O).
    fn requires_alignment(&self) -> bool {
        false
    }

    /// Try to re-establish access after the device disappeared. Device
    /// sources re-scan for the same serial number; everything else returns
    /// [`ReadErrorKind::Unsupported`].
    fn reopen(&self) -> Result<(), ReadError> {
        Err(ReadError::new(
            0,
            0,
            ReadErrorKind::Unsupported,
            "reopen is not supported by this source",
        ))
    }

    /// Byte ranges already known to be unreadable (for example from the
    /// mapfile of a rescued image). Parsers use this to flag recovered files
    /// that contain holes. `None` means "nothing known".
    fn unreadable(&self) -> Option<RangeSet> {
        None
    }

    /// Path this source was opened from, if any.
    fn path(&self) -> Option<&Path> {
        None
    }
}

/// Shared handle to a source. Parsers hold one of these so views and caches
/// can be layered freely.
pub type SharedSource = Arc<dyn Source>;

/// Convenience helpers available on every source.
pub trait SourceExt: Source {
    /// Read exactly `buf.len()` bytes or fail. A short read becomes
    /// [`ReadErrorKind::OutOfRange`].
    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), ReadError> {
        let mut done = 0usize;
        while done < buf.len() {
            let n = self.read_at(offset + done as u64, &mut buf[done..])?;
            if n == 0 {
                return Err(ReadError::out_of_range(
                    offset + done as u64,
                    buf.len() - done,
                    self.size(),
                ));
            }
            done += n;
        }
        Ok(())
    }

    /// Read `len` bytes at `offset`, clamped to the end of the source. The
    /// returned vector may be shorter than `len` near the end and empty past it.
    fn read_vec(&self, offset: u64, len: usize) -> Result<Vec<u8>, ReadError> {
        let size = self.size();
        if offset >= size {
            return Ok(Vec::new());
        }
        let avail = (size - offset).min(len as u64) as usize;
        let mut buf = vec![0u8; avail];
        let mut done = 0usize;
        while done < avail {
            let n = self.read_at(offset + done as u64, &mut buf[done..])?;
            if n == 0 {
                break;
            }
            done += n;
        }
        buf.truncate(done);
        Ok(buf)
    }

    /// Read exactly `len` bytes at `offset` into a new vector.
    fn read_exact_vec(&self, offset: u64, len: usize) -> Result<Vec<u8>, ReadError> {
        let mut buf = vec![0u8; len];
        self.read_exact_at(offset, &mut buf)?;
        Ok(buf)
    }

    /// Whether `offset..offset+len` overlaps a known-unreadable range.
    fn overlaps_unreadable(&self, offset: u64, len: u64) -> bool {
        match self.unreadable() {
            Some(set) => set.intersects(crate::ranges::Range::new(
                offset,
                offset.saturating_add(len),
            )),
            None => false,
        }
    }
}

impl<T: Source + ?Sized> SourceExt for T {}

impl<T: Source + ?Sized> Source for Arc<T> {
    fn size(&self) -> u64 {
        (**self).size()
    }
    fn sector_size(&self) -> u32 {
        (**self).sector_size()
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, ReadError> {
        (**self).read_at(offset, buf)
    }
    fn read_at_timeout(
        &self,
        offset: u64,
        buf: &mut [u8],
        timeout: Duration,
    ) -> Result<usize, ReadError> {
        (**self).read_at_timeout(offset, buf, timeout)
    }
    fn describe(&self) -> String {
        (**self).describe()
    }
    fn is_device(&self) -> bool {
        (**self).is_device()
    }
    fn requires_alignment(&self) -> bool {
        (**self).requires_alignment()
    }
    fn reopen(&self) -> Result<(), ReadError> {
        (**self).reopen()
    }
    fn unreadable(&self) -> Option<RangeSet> {
        (**self).unreadable()
    }
    fn path(&self) -> Option<&Path> {
        (**self).path()
    }
}

impl<T: Source + ?Sized> Source for Box<T> {
    fn size(&self) -> u64 {
        (**self).size()
    }
    fn sector_size(&self) -> u32 {
        (**self).sector_size()
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, ReadError> {
        (**self).read_at(offset, buf)
    }
    fn read_at_timeout(
        &self,
        offset: u64,
        buf: &mut [u8],
        timeout: Duration,
    ) -> Result<usize, ReadError> {
        (**self).read_at_timeout(offset, buf, timeout)
    }
    fn describe(&self) -> String {
        (**self).describe()
    }
    fn is_device(&self) -> bool {
        (**self).is_device()
    }
    fn requires_alignment(&self) -> bool {
        (**self).requires_alignment()
    }
    fn reopen(&self) -> Result<(), ReadError> {
        (**self).reopen()
    }
    fn unreadable(&self) -> Option<RangeSet> {
        (**self).unreadable()
    }
    fn path(&self) -> Option<&Path> {
        (**self).path()
    }
}

/// A regular file (image file) or a plain file handle to a block device
/// without any device-specific handling. Device-aware sources live in the
/// `diskrescue-device` crate.
pub struct FileSource {
    file: File,
    size: u64,
    path: PathBuf,
    unreadable: Option<RangeSet>,
}

impl FileSource {
    /// Open `path` read-only. The size is determined by seeking to the end,
    /// which works for block devices too (metadata reports 0 for them).
    pub fn open(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut file = File::open(&path)?;
        let size = file.seek(SeekFrom::End(0))?;
        Ok(FileSource {
            file,
            size,
            path,
            unreadable: None,
        })
    }

    /// Wrap an already-open file.
    pub fn from_file(file: File, path: impl Into<PathBuf>, size: u64) -> Self {
        FileSource {
            file,
            size,
            path: path.into(),
            unreadable: None,
        }
    }

    /// Attach the set of ranges known to be unreadable (from a mapfile).
    pub fn with_unreadable(mut self, set: RangeSet) -> Self {
        self.unreadable = Some(set);
        self
    }

    /// Path this source was opened from.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Source for FileSource {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, ReadError> {
        if offset >= self.size {
            return Ok(0);
        }
        let want = ((self.size - offset).min(buf.len() as u64)) as usize;
        let buf = &mut buf[..want];
        read_at_impl(&self.file, offset, buf)
    }

    fn describe(&self) -> String {
        self.path.display().to_string()
    }

    fn unreadable(&self) -> Option<RangeSet> {
        self.unreadable.clone()
    }

    fn path(&self) -> Option<&Path> {
        Some(&self.path)
    }
}

/// Positional read on a `File`, retrying on `Interrupted` and stopping on
/// the first error or short read.
pub fn read_at_impl(file: &File, offset: u64, buf: &mut [u8]) -> Result<usize, ReadError> {
    let mut done = 0usize;
    while done < buf.len() {
        let r = {
            #[cfg(unix)]
            {
                use std::os::unix::fs::FileExt;
                file.read_at(&mut buf[done..], offset + done as u64)
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::FileExt;
                file.seek_read(&mut buf[done..], offset + done as u64)
            }
        };
        match r {
            Ok(0) => break,
            Ok(n) => done += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                // Report the offset of the failing chunk, not the start of
                // the whole request, so the caller can keep the good prefix.
                return Err(ReadError::from_io(
                    offset + done as u64,
                    buf.len() - done,
                    &e,
                ));
            }
        }
    }
    Ok(done)
}

/// An in-memory source for tests and for parsing extracted files.
#[derive(Clone)]
pub struct MemSource {
    data: Arc<Vec<u8>>,
    name: String,
    unreadable: Option<RangeSet>,
}

impl MemSource {
    /// Wrap a buffer.
    pub fn new(data: Vec<u8>) -> Self {
        MemSource {
            data: Arc::new(data),
            name: "memory".to_string(),
            unreadable: None,
        }
    }

    /// Wrap a buffer with a descriptive name.
    pub fn named(data: Vec<u8>, name: impl Into<String>) -> Self {
        MemSource {
            data: Arc::new(data),
            name: name.into(),
            unreadable: None,
        }
    }

    /// Mark ranges as unreadable; reads inside them fail with
    /// [`ReadErrorKind::Io`]. Useful for simulating holes in tests.
    pub fn with_unreadable(mut self, set: RangeSet) -> Self {
        self.unreadable = Some(set);
        self
    }

    /// Borrow the underlying bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.data
    }
}

impl Source for MemSource {
    fn size(&self) -> u64 {
        self.data.len() as u64
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, ReadError> {
        let size = self.data.len() as u64;
        if offset >= size {
            return Ok(0);
        }
        let n = ((size - offset).min(buf.len() as u64)) as usize;
        if let Some(bad) = &self.unreadable {
            let r = crate::ranges::Range::new(offset, offset + n as u64);
            if let Some(hit) = bad.first_intersecting(r) {
                let good = hit.start.saturating_sub(offset) as usize;
                if good == 0 {
                    return Err(ReadError::new(
                        offset,
                        n,
                        ReadErrorKind::Io,
                        "simulated unreadable range",
                    ));
                }
                buf[..good].copy_from_slice(&self.data[offset as usize..offset as usize + good]);
                return Ok(good);
            }
        }
        buf[..n].copy_from_slice(&self.data[offset as usize..offset as usize + n]);
        Ok(n)
    }

    fn describe(&self) -> String {
        self.name.clone()
    }

    fn unreadable(&self) -> Option<RangeSet> {
        self.unreadable.clone()
    }
}

/// A window onto another source: used for partitions and for carved files
/// that are parsed in place without copying.
pub struct SubSource {
    inner: SharedSource,
    start: u64,
    len: u64,
    name: String,
}

impl SubSource {
    /// View `len` bytes of `inner` starting at `start`. The window is clamped
    /// to the inner source's size.
    pub fn new(inner: SharedSource, start: u64, len: u64) -> Self {
        let inner_size = inner.size();
        let start_c = start.min(inner_size);
        let len_c = len.min(inner_size - start_c);
        let name = format!("{} [{}+{}]", inner.describe(), start_c, len_c);
        SubSource {
            inner,
            start: start_c,
            len: len_c,
            name,
        }
    }

    /// Same as [`SubSource::new`] with a custom description.
    pub fn named(inner: SharedSource, start: u64, len: u64, name: impl Into<String>) -> Self {
        let mut s = SubSource::new(inner, start, len);
        s.name = name.into();
        s
    }

    /// Offset of this window inside the parent.
    pub fn start(&self) -> u64 {
        self.start
    }

    /// The parent source.
    pub fn parent(&self) -> &SharedSource {
        &self.inner
    }
}

impl Source for SubSource {
    fn size(&self) -> u64 {
        self.len
    }

    fn sector_size(&self) -> u32 {
        self.inner.sector_size()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, ReadError> {
        if offset >= self.len {
            return Ok(0);
        }
        let want = ((self.len - offset).min(buf.len() as u64)) as usize;
        self.inner
            .read_at(self.start + offset, &mut buf[..want])
            .map_err(|mut e| {
                e.offset = e.offset.saturating_sub(self.start);
                e
            })
    }

    fn read_at_timeout(
        &self,
        offset: u64,
        buf: &mut [u8],
        timeout: Duration,
    ) -> Result<usize, ReadError> {
        if offset >= self.len {
            return Ok(0);
        }
        let want = ((self.len - offset).min(buf.len() as u64)) as usize;
        self.inner
            .read_at_timeout(self.start + offset, &mut buf[..want], timeout)
            .map_err(|mut e| {
                e.offset = e.offset.saturating_sub(self.start);
                e
            })
    }

    fn describe(&self) -> String {
        self.name.clone()
    }

    fn is_device(&self) -> bool {
        self.inner.is_device()
    }

    fn requires_alignment(&self) -> bool {
        self.inner.requires_alignment()
    }

    fn reopen(&self) -> Result<(), ReadError> {
        self.inner.reopen()
    }

    fn unreadable(&self) -> Option<RangeSet> {
        self.inner.unreadable().map(|set| {
            let window = crate::ranges::Range::new(self.start, self.start + self.len);
            set.intersection_with_range(window).shifted_down(self.start)
        })
    }

    fn path(&self) -> Option<&Path> {
        self.inner.path()
    }
}

/// A read-through block cache for parsers that issue many small reads
/// (directory walks, box walking, MFT records). Blocks are `block_size`
/// bytes, aligned, and the cache keeps the most recently used `capacity`
/// of them. Failed reads are never cached.
pub struct BlockCache {
    inner: SharedSource,
    block_size: usize,
    capacity: usize,
    state: Mutex<CacheState>,
    tick: AtomicU64,
}

struct CacheState {
    blocks: std::collections::HashMap<u64, (Arc<Vec<u8>>, u64)>,
}

impl BlockCache {
    /// Wrap `inner`. `block_size` should be a multiple of the sector size
    /// (64 KiB is a good default); `capacity` is the number of blocks kept.
    pub fn new(inner: SharedSource, block_size: usize, capacity: usize) -> Self {
        let sector = inner.sector_size().max(1) as usize;
        let block_size = block_size.max(sector).div_ceil(sector) * sector;
        BlockCache {
            inner,
            block_size,
            capacity: capacity.max(1),
            state: Mutex::new(CacheState {
                blocks: std::collections::HashMap::new(),
            }),
            tick: AtomicU64::new(0),
        }
    }

    /// Block size in use.
    pub fn block_size(&self) -> usize {
        self.block_size
    }

    /// The wrapped source.
    pub fn inner(&self) -> &SharedSource {
        &self.inner
    }

    /// Drop every cached block.
    pub fn clear(&self) {
        self.state.lock().unwrap().blocks.clear();
    }

    fn block(&self, index: u64) -> Result<Arc<Vec<u8>>, ReadError> {
        let tick = self.tick.fetch_add(1, Ordering::Relaxed);
        {
            let mut st = self.state.lock().unwrap();
            if let Some(entry) = st.blocks.get_mut(&index) {
                entry.1 = tick;
                return Ok(entry.0.clone());
            }
        }
        let offset = index * self.block_size as u64;
        let mut buf = vec![0u8; self.block_size];
        let n = self.inner.read_at(offset, &mut buf)?;
        buf.truncate(n);
        let block = Arc::new(buf);
        // Only cache complete blocks and the final block of the source. A
        // short read in the middle stopped in front of an unreadable sector;
        // caching it would turn that sector into a silent end of data.
        let complete = n == self.block_size || offset.saturating_add(n as u64) >= self.inner.size();
        if complete {
            let mut st = self.state.lock().unwrap();
            if st.blocks.len() >= self.capacity {
                // Evict the least recently used block. Capacity is small enough
                // that a linear scan is cheaper than keeping an ordered index.
                if let Some((&victim, _)) = st.blocks.iter().min_by_key(|(_, (_, t))| *t) {
                    st.blocks.remove(&victim);
                }
            }
            st.blocks.insert(index, (block.clone(), tick));
        }
        Ok(block)
    }
}

impl Source for BlockCache {
    fn size(&self) -> u64 {
        self.inner.size()
    }

    fn sector_size(&self) -> u32 {
        self.inner.sector_size()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, ReadError> {
        let size = self.size();
        if offset >= size {
            return Ok(0);
        }
        let want = ((size - offset).min(buf.len() as u64)) as usize;
        let mut done = 0usize;
        while done < want {
            let pos = offset + done as u64;
            let index = pos / self.block_size as u64;
            let within = (pos % self.block_size as u64) as usize;
            let block = self.block(index).map_err(|mut e| {
                // Report the first byte the caller actually asked for.
                e.offset = e.offset.max(pos);
                e
            })?;
            if within >= block.len() {
                // The block ended early. At the end of the source that is
                // EOF; otherwise the next byte is unreadable: return what we
                // have, or let the inner source report the error.
                if done > 0 || pos >= size {
                    break;
                }
                return self.inner.read_at(pos, &mut buf[..want]);
            }
            let n = (block.len() - within).min(want - done);
            buf[done..done + n].copy_from_slice(&block[within..within + n]);
            done += n;
            if n == 0 {
                break;
            }
        }
        Ok(done)
    }

    fn describe(&self) -> String {
        self.inner.describe()
    }

    fn is_device(&self) -> bool {
        self.inner.is_device()
    }

    fn reopen(&self) -> Result<(), ReadError> {
        self.inner.reopen()
    }

    fn unreadable(&self) -> Option<RangeSet> {
        self.inner.unreadable()
    }

    fn path(&self) -> Option<&Path> {
        self.inner.path()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ranges::Range;

    #[test]
    fn mem_source_reads_and_clamps() {
        let s = MemSource::new((0..100u8).collect());
        let mut buf = [0u8; 10];
        assert_eq!(s.read_at(95, &mut buf).unwrap(), 5);
        assert_eq!(&buf[..5], &[95, 96, 97, 98, 99]);
        assert_eq!(s.read_at(100, &mut buf).unwrap(), 0);
        assert_eq!(s.read_vec(90, 100).unwrap().len(), 10);
        assert!(s.read_exact_vec(95, 10).is_err());
    }

    #[test]
    fn mem_source_simulated_holes() {
        let mut set = RangeSet::new();
        set.insert(Range::new(10, 20));
        let s = MemSource::new((0..100u8).collect()).with_unreadable(set);
        let mut buf = [0u8; 50];
        // Good prefix is returned as a short read.
        assert_eq!(s.read_at(0, &mut buf).unwrap(), 10);
        // Reading inside the hole fails.
        let err = s.read_at(12, &mut buf).unwrap_err();
        assert_eq!(err.kind, ReadErrorKind::Io);
        assert_eq!(err.offset, 12);
        assert!(s.overlaps_unreadable(15, 1));
        assert!(!s.overlaps_unreadable(20, 5));
    }

    #[test]
    fn sub_source_offsets_errors_and_unreadable() {
        let mut set = RangeSet::new();
        set.insert(Range::new(30, 40));
        let base: SharedSource =
            Arc::new(MemSource::new((0..100u8).collect()).with_unreadable(set));
        let sub = SubSource::new(base, 25, 50);
        assert_eq!(sub.size(), 50);
        let mut buf = [0u8; 4];
        assert_eq!(sub.read_at(0, &mut buf).unwrap(), 4);
        assert_eq!(buf, [25, 26, 27, 28]);
        let err = sub.read_at(6, &mut buf).unwrap_err();
        assert_eq!(err.offset, 6);
        let un = sub.unreadable().unwrap();
        assert_eq!(un.iter().collect::<Vec<_>>(), vec![Range::new(5, 15)]);
        assert_eq!(sub.read_at(49, &mut buf).unwrap(), 1);
        assert_eq!(sub.read_at(50, &mut buf).unwrap(), 0);
    }

    #[test]
    fn block_cache_serves_across_block_boundaries() {
        let data: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        let base: SharedSource = Arc::new(MemSource::new(data.clone()));
        let cache = BlockCache::new(base, 1024, 2);
        let got = cache.read_exact_vec(1000, 2100).unwrap();
        assert_eq!(got, data[1000..3100].to_vec());
        let tail = cache.read_vec(4990, 100).unwrap();
        assert_eq!(tail, data[4990..].to_vec());
        assert_eq!(cache.read_vec(5000, 10).unwrap().len(), 0);
    }

    #[test]
    fn block_cache_does_not_turn_holes_into_eof() {
        let mut set = RangeSet::new();
        set.insert(Range::new(1500, 1600));
        let base: SharedSource =
            Arc::new(MemSource::new((0..4096u32).map(|i| i as u8).collect()).with_unreadable(set));
        let cache = BlockCache::new(base, 1024, 4);
        // First read hits the short block [1024, 1500) and must not cache it.
        let v = cache.read_vec(1024, 2048);
        assert!(
            v.is_err(),
            "reading across a hole must fail, got {:?}",
            v.map(|x| x.len())
        );
        // Reading inside the hole reports the error at the right place.
        let mut b = [0u8; 16];
        let e = cache.read_at(1510, &mut b).unwrap_err();
        assert_eq!(e.kind, ReadErrorKind::Io);
        // Reading before the hole works and returns a short read, never a
        // fake EOF that hides the bytes after the hole.
        let mut b = [0u8; 600];
        let n = cache.read_at(1024, &mut b).unwrap();
        assert_eq!(n, 476);
        assert_eq!(
            cache.read_exact_vec(1600, 100).unwrap(),
            (1600..1700u32).map(|i| i as u8).collect::<Vec<_>>()
        );
    }

    #[test]
    fn file_source_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("img.bin");
        std::fs::write(&p, (0..=255u8).collect::<Vec<_>>()).unwrap();
        let s = FileSource::open(&p).unwrap();
        assert_eq!(s.size(), 256);
        assert_eq!(
            s.read_exact_vec(250, 6).unwrap(),
            vec![250, 251, 252, 253, 254, 255]
        );
        assert_eq!(s.read_vec(256, 4).unwrap().len(), 0);
    }
}
