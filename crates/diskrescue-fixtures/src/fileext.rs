//! Positional reads and writes on every platform.
//!
//! Test helpers patch images at fixed offsets; `std::os::unix::fs::FileExt`
//! exists only on Unix. This trait has the same two methods on Unix and
//! Windows, so the helpers (and the tests that need no external tools) build
//! and run on both.

use std::fs::File;
use std::io;

/// `read_exact_at` and `write_all_at` for [`File`], as on Unix.
pub trait FileExt {
    /// Fill `buf` with the bytes at `offset`.
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()>;
    /// Write all of `buf` at `offset`.
    fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()>;
}

#[cfg(unix)]
impl FileExt for File {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        std::os::unix::fs::FileExt::read_exact_at(self, buf, offset)
    }

    fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        std::os::unix::fs::FileExt::write_all_at(self, buf, offset)
    }
}

#[cfg(windows)]
impl FileExt for File {
    fn read_exact_at(&self, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
        use std::os::windows::fs::FileExt as _;
        while !buf.is_empty() {
            match self.seek_read(buf, offset) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "failed to fill whole buffer",
                    ))
                }
                Ok(n) => {
                    let rest = buf;
                    buf = &mut rest[n..];
                    offset += n as u64;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn write_all_at(&self, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
        use std::os::windows::fs::FileExt as _;
        while !buf.is_empty() {
            match self.seek_write(buf, offset) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "failed to write whole buffer",
                    ))
                }
                Ok(n) => {
                    buf = &buf[n..];
                    offset += n as u64;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::FileExt;

    #[test]
    fn writes_and_reads_at_offsets() {
        let dir = tempfile::tempdir().unwrap();
        let f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(dir.path().join("f"))
            .unwrap();
        f.write_all_at(b"world", 6).unwrap();
        f.write_all_at(b"hello ", 0).unwrap();
        let mut buf = [0u8; 11];
        f.read_exact_at(&mut buf, 0).unwrap();
        assert_eq!(&buf, b"hello world");
        let mut past = [0u8; 4];
        let e = f.read_exact_at(&mut past, 9).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof);
    }
}
