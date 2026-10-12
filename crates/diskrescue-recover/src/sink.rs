//! Writers used while extracting files.

use std::io::{self, Write};

/// Writes through to an inner writer while hashing (BLAKE3) and counting.
pub struct HashingWriter<W: Write> {
    inner: W,
    hasher: blake3::Hasher,
    written: u64,
}

impl<W: Write> HashingWriter<W> {
    /// Wrap `inner`.
    pub fn new(inner: W) -> Self {
        HashingWriter {
            inner,
            hasher: blake3::Hasher::new(),
            written: 0,
        }
    }

    /// Bytes written so far.
    pub fn written(&self) -> u64 {
        self.written
    }

    /// Finish: flush and return the inner writer, the hex hash and the count.
    pub fn finish(mut self) -> io::Result<(W, String, u64)> {
        self.inner.flush()?;
        Ok((
            self.inner,
            self.hasher.finalize().to_hex().to_string(),
            self.written,
        ))
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Captures the first `limit` bytes written and then refuses more with
/// `ErrorKind::WriteZero`. Used to sniff the head of a file through a
/// filesystem's `read_file` without extracting all of it.
pub struct HeadSink {
    buf: Vec<u8>,
    limit: usize,
}

impl HeadSink {
    /// Capture up to `limit` bytes.
    pub fn new(limit: usize) -> Self {
        HeadSink {
            buf: Vec::with_capacity(limit.min(1 << 20)),
            limit,
        }
    }

    /// The captured bytes.
    pub fn into_inner(self) -> Vec<u8> {
        self.buf
    }

    /// True once `limit` bytes were captured.
    pub fn is_full(&self) -> bool {
        self.buf.len() >= self.limit
    }
}

impl Write for HeadSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let room = self.limit - self.buf.len();
        if room == 0 {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "head captured"));
        }
        let n = room.min(buf.len());
        self.buf.extend_from_slice(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashing_writer_matches_blake3() {
        let mut w = HashingWriter::new(Vec::new());
        w.write_all(b"hello ").unwrap();
        w.write_all(b"world").unwrap();
        let (inner, hash, n) = w.finish().unwrap();
        assert_eq!(inner, b"hello world");
        assert_eq!(n, 11);
        assert_eq!(hash, blake3::hash(b"hello world").to_hex().to_string());
    }

    #[test]
    fn head_sink_stops() {
        let mut h = HeadSink::new(4);
        assert_eq!(h.write(b"ab").unwrap(), 2);
        assert_eq!(h.write(b"cdef").unwrap(), 2);
        assert!(h.is_full());
        assert_eq!(h.write(b"x").unwrap_err().kind(), io::ErrorKind::WriteZero);
        assert!(h.write_all(b"y").is_err());
        assert_eq!(h.into_inner(), b"abcd");
    }
}
