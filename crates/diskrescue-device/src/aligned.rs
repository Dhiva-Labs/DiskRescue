//! Aligned, reusable read buffers.
//!
//! Direct I/O (`O_DIRECT`, `FILE_FLAG_NO_BUFFERING`) requires the user
//! buffer address to be aligned (to the logical sector size, the queue's
//! DMA alignment or, on Windows, the physical sector size). Page alignment
//! satisfies every known requirement, so buffers here are aligned to
//! `max(4096, sector)`.
//!
//! The implementation over-allocates a `Vec<u8>` by `align` bytes and uses
//! the aligned window inside it. A `Vec`'s heap block never moves when the
//! `Vec` itself is moved, so the window stays aligned while the buffer is
//! handed between the caller and a worker thread. No `unsafe` is involved.
//!
//! [`BufferPool`] keeps a few buffers around so steady-state reads do not
//! allocate.

use std::sync::Mutex;

/// Minimum alignment of direct-I/O buffers. 4 KiB covers the logical
/// sector size of 512n/512e/4Kn disks and every `queue/dma_alignment` mask
/// the kernel reports; larger sectors raise the alignment to the sector.
pub const PAGE: usize = 4096;

/// Largest buffer the pool keeps for reuse.
const MAX_POOLED_BYTES: usize = 16 << 20;

/// Number of buffers the pool keeps.
const MAX_POOLED: usize = 8;

/// A heap buffer whose usable window starts at an aligned address.
#[derive(Debug)]
pub struct AlignedBuf {
    storage: Vec<u8>,
    offset: usize,
    capacity: usize,
    align: usize,
}

impl AlignedBuf {
    /// Allocate `capacity` zeroed bytes aligned to `align` (rounded up to a
    /// power of two, at least 1).
    pub fn new(capacity: usize, align: usize) -> Self {
        let align = align.max(1).next_power_of_two();
        let storage = vec![0u8; capacity + align];
        let addr = storage.as_ptr() as usize;
        let offset = (align - addr % align) % align;
        AlignedBuf {
            storage,
            offset,
            capacity,
            align,
        }
    }

    /// Usable bytes.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Alignment of the usable window.
    pub fn align(&self) -> usize {
        self.align
    }

    /// The first `len` bytes (clamped to the capacity).
    pub fn slice(&self, len: usize) -> &[u8] {
        let len = len.min(self.capacity);
        &self.storage[self.offset..self.offset + len]
    }

    /// The first `len` bytes, mutable (clamped to the capacity).
    pub fn slice_mut(&mut self, len: usize) -> &mut [u8] {
        let len = len.min(self.capacity);
        &mut self.storage[self.offset..self.offset + len]
    }

    /// Address of the usable window (for alignment assertions in tests).
    pub fn addr(&self) -> usize {
        self.storage.as_ptr() as usize + self.offset
    }
}

/// A small pool of [`AlignedBuf`]s with a fixed alignment.
#[derive(Debug)]
pub struct BufferPool {
    align: usize,
    bufs: Mutex<Vec<AlignedBuf>>,
}

impl BufferPool {
    /// Pool whose buffers are aligned to `align`.
    pub fn new(align: usize) -> Self {
        BufferPool {
            align: align.max(1).next_power_of_two(),
            bufs: Mutex::new(Vec::new()),
        }
    }

    /// Alignment of every buffer handed out.
    pub fn align(&self) -> usize {
        self.align
    }

    /// A buffer with at least `len` usable bytes. Its contents are
    /// unspecified (previous reads may have left data in it).
    pub fn take(&self, len: usize) -> AlignedBuf {
        if let Ok(mut bufs) = self.bufs.lock() {
            if let Some(i) = bufs.iter().position(|b| b.capacity() >= len) {
                return bufs.swap_remove(i);
            }
        }
        AlignedBuf::new(len, self.align)
    }

    /// Return a buffer for reuse. Oversized buffers and buffers beyond the
    /// pool limit are freed.
    pub fn put(&self, buf: AlignedBuf) {
        if buf.capacity() > MAX_POOLED_BYTES || buf.align() != self.align {
            return;
        }
        if let Ok(mut bufs) = self.bufs.lock() {
            if bufs.len() < MAX_POOLED {
                bufs.push(buf);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alignment_holds_after_moves() {
        for align in [1usize, 512, 4096, 65536] {
            let b = AlignedBuf::new(1000, align);
            let moved = vec![b];
            let b = moved.into_iter().next().unwrap();
            assert_eq!(b.addr() % align, 0);
            assert_eq!(b.slice(5000).len(), 1000);
        }
    }

    #[test]
    fn pool_reuses() {
        let p = BufferPool::new(4096);
        let b = p.take(8192);
        let addr = b.addr();
        p.put(b);
        let b2 = p.take(4096);
        assert_eq!(b2.addr(), addr);
        assert_eq!(b2.addr() % 4096, 0);
        p.put(AlignedBuf::new(MAX_POOLED_BYTES + 1, 4096));
        assert!(p.take(MAX_POOLED_BYTES + 1).capacity() > MAX_POOLED_BYTES);
    }
}
