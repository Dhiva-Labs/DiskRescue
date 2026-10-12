//! Small read helpers shared by the parsers.
//!
//! Every parser in this crate reads through these functions so that
//! direct-I/O devices (which require sector aligned offsets and lengths) work
//! the same way as image files, and so that a bad sector is reported as "this
//! structure is unreadable" instead of aborting the whole analysis.

use diskrescue_core::{Error, ReadError, ReadErrorKind, Result, Source};

/// Largest single read any helper in this crate will issue.
pub(crate) const MAX_READ: usize = 16 << 20;

/// Alignment unit the source demands (1 for files and memory).
fn align_unit(src: &dyn Source) -> u64 {
    if src.requires_alignment() {
        u64::from(src.sector_size().max(1))
    } else {
        1
    }
}

/// Read up to `len` bytes at `offset`, clamped to the end of the source.
///
/// The request is widened to the source's alignment when needed; the returned
/// vector holds exactly the bytes asked for (shorter near the end of the
/// source, empty past it).
pub(crate) fn read_raw(
    src: &dyn Source,
    offset: u64,
    len: usize,
) -> std::result::Result<Vec<u8>, ReadError> {
    let size = src.size();
    let len = len.min(MAX_READ);
    if offset >= size || len == 0 {
        return Ok(Vec::new());
    }
    let unit = align_unit(src);
    let start = offset - offset % unit;
    let want_end = offset.saturating_add(len as u64).min(size);
    let end = if unit > 1 {
        want_end.div_ceil(unit).saturating_mul(unit)
    } else {
        want_end
    };
    let mut buf = vec![0u8; (end - start) as usize];
    let mut done = 0usize;
    while done < buf.len() {
        let n = src.read_at(start + done as u64, &mut buf[done..])?;
        if n == 0 {
            break;
        }
        done += n;
    }
    buf.truncate(done);
    let skip = (offset - start) as usize;
    if skip >= buf.len() {
        return Ok(Vec::new());
    }
    let mut out = buf.split_off(skip);
    out.truncate(len);
    Ok(out)
}

/// Errors that mean the source itself is unusable (as opposed to one bad
/// sector): the caller should stop and report them.
pub(crate) fn is_fatal(err: &ReadError) -> bool {
    matches!(
        err.kind,
        ReadErrorKind::DeviceGone | ReadErrorKind::Unaligned | ReadErrorKind::Unsupported
    )
}

/// Like [`read_raw`] but a media error (bad sector, timeout) becomes
/// `Ok(None)`. Fatal source errors are still returned as `Err`.
pub(crate) fn read_soft(src: &dyn Source, offset: u64, len: usize) -> Result<Option<Vec<u8>>> {
    match read_raw(src, offset, len) {
        Ok(v) => Ok(Some(v)),
        Err(e) if is_fatal(&e) => Err(Error::Read(e)),
        Err(e) => {
            log::debug!("unreadable at {offset} (len {len}): {e}");
            Ok(None)
        }
    }
}

/// Read for probes: any error or an empty result is `None`.
pub(crate) fn read_opt(src: &dyn Source, offset: u64, len: usize) -> Option<Vec<u8>> {
    match read_raw(src, offset, len) {
        Ok(v) if !v.is_empty() => Some(v),
        Ok(_) => None,
        Err(e) => {
            log::debug!("probe read failed at {offset} (len {len}): {e}");
            None
        }
    }
}

/// True when every byte is zero.
pub(crate) fn all_zero(b: &[u8]) -> bool {
    b.iter().all(|&x| x == 0)
}

/// Round `v` up to a multiple of `unit` (saturating).
pub(crate) fn align_up(v: u64, unit: u64) -> u64 {
    if unit <= 1 {
        v
    } else {
        v.div_ceil(unit).saturating_mul(unit)
    }
}

/// Number of trailing zero bits of an offset, capped at 20 (1 MiB). Used to
/// prefer the more "partitioner aligned" of two candidate volume starts.
pub(crate) fn alignment_bits(v: u64) -> u32 {
    if v == 0 {
        20
    } else {
        v.trailing_zeros().min(20)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskrescue_core::source::MemSource;

    #[test]
    fn raw_reads_are_clamped() {
        let s = MemSource::new((0..100u8).collect());
        assert_eq!(read_raw(&s, 95, 10).unwrap(), vec![95, 96, 97, 98, 99]);
        assert!(read_raw(&s, 100, 10).unwrap().is_empty());
        assert!(read_raw(&s, 0, 0).unwrap().is_empty());
        assert_eq!(read_opt(&s, 1000, 4), None);
    }

    #[test]
    fn helpers() {
        assert_eq!(align_up(5, 4), 8);
        assert_eq!(align_up(8, 4), 8);
        assert_eq!(align_up(u64::MAX, 4096), u64::MAX);
        assert_eq!(alignment_bits(0), 20);
        assert_eq!(alignment_bits(4096), 12);
        assert_eq!(alignment_bits(1 << 30), 20);
        assert!(all_zero(&[0, 0]));
        assert!(!all_zero(&[0, 1]));
    }
}
