//! Update sequence arrays ("fixups").
//!
//! Every multi-sector NTFS structure (FILE and INDX records) protects itself
//! against torn writes: before writing, NTFS replaces the last two bytes of
//! every 512-byte stride with the record's update sequence number (USN) and
//! saves the original bytes in the update sequence array (USA). A reader
//! must check each stride's last two bytes against the USN and put the saved
//! bytes back **before** parsing anything else.
//!
//! A mismatch means the stride was not written together with the rest of the
//! record: a power loss or a failing drive tore the multi-sector write. The
//! record is still restored (the saved bytes are the best available guess)
//! and the mismatching strides are reported so callers can lower confidence.
//! The stride is always 512 bytes regardless of the medium's sector size.

use diskrescue_core::util::bytes::u16le;

/// Fixup stride in bytes (NTFS_BLOCK_SIZE).
pub const STRIDE: usize = 512;

/// Why a record's update sequence array could not be applied at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FixupError {
    /// The buffer is not a whole number of 512-byte strides.
    BadLength(usize),
    /// `usa_ofs` / `usa_count` are inconsistent with the record size.
    BadArray {
        /// Offset of the array as stored in the header.
        usa_ofs: u16,
        /// Entry count as stored in the header.
        usa_count: u16,
    },
}

impl std::fmt::Display for FixupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FixupError::BadLength(n) => write!(f, "record length {n} is not a multiple of 512"),
            FixupError::BadArray { usa_ofs, usa_count } => write!(
                f,
                "invalid update sequence array (offset {usa_ofs}, count {usa_count})"
            ),
        }
    }
}

/// Apply the update sequence array of the record in `buf` (header at offset
/// 0: `usa_ofs` at 0x04, `usa_count` at 0x06). Returns the 0-based indices
/// of 512-byte strides whose check bytes did not match the USN (torn
/// strides). The saved bytes are restored into every stride either way.
pub fn apply_fixups(buf: &mut [u8]) -> Result<Vec<usize>, FixupError> {
    if buf.len() < STRIDE || buf.len() % STRIDE != 0 {
        return Err(FixupError::BadLength(buf.len()));
    }
    let usa_ofs = u16le(buf, 4).unwrap_or(0);
    let usa_count = u16le(buf, 6).unwrap_or(0);
    let strides = buf.len() / STRIDE;
    let ofs = usa_ofs as usize;
    let count = usa_count as usize;
    // The array must describe exactly one entry per stride, be 2-aligned,
    // start after the 8-byte common header, and fit in the first stride
    // without touching that stride's own check bytes.
    if count != strides + 1 || ofs < 8 || ofs % 2 != 0 || ofs + 2 * count > STRIDE - 2 {
        return Err(FixupError::BadArray { usa_ofs, usa_count });
    }
    let usn = [buf[ofs], buf[ofs + 1]];
    let mut torn = Vec::new();
    for s in 0..strides {
        let at = (s + 1) * STRIDE - 2;
        if buf[at] != usn[0] || buf[at + 1] != usn[1] {
            torn.push(s);
        }
        let saved = ofs + 2 * (s + 1);
        buf[at] = buf[saved];
        buf[at + 1] = buf[saved + 1];
    }
    Ok(torn)
}

/// Inverse of [`apply_fixups`], used by tests to build on-disk images:
/// stores the last two bytes of every stride in the array and replaces them
/// with `usn`.
#[cfg(test)]
pub(crate) fn protect(buf: &mut [u8], usn: u16) {
    let ofs = u16le(buf, 4).unwrap() as usize;
    let strides = buf.len() / STRIDE;
    buf[ofs..ofs + 2].copy_from_slice(&usn.to_le_bytes());
    for s in 0..strides {
        let at = (s + 1) * STRIDE - 2;
        let saved = ofs + 2 * (s + 1);
        buf[saved] = buf[at];
        buf[saved + 1] = buf[at + 1];
        buf[at..at + 2].copy_from_slice(&usn.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> Vec<u8> {
        let mut r = vec![0u8; 1024];
        r[0..4].copy_from_slice(b"FILE");
        r[4..6].copy_from_slice(&0x30u16.to_le_bytes());
        r[6..8].copy_from_slice(&3u16.to_le_bytes());
        r[510] = 0x11;
        r[511] = 0x22;
        r[1022] = 0x33;
        r[1023] = 0x44;
        r
    }

    #[test]
    fn roundtrip_and_torn_detection() {
        let orig = record();
        let mut r = orig.clone();
        protect(&mut r, 0x0007);
        assert_eq!(&r[510..512], &[7, 0]);
        let mut good = r.clone();
        assert_eq!(apply_fixups(&mut good).unwrap(), Vec::<usize>::new());
        // Everything but the array itself is back to the original.
        assert_eq!(good[..0x30], orig[..0x30]);
        assert_eq!(good[0x36..], orig[0x36..]);
        let mut torn = r.clone();
        torn[1022] = 0xAA;
        assert_eq!(apply_fixups(&mut torn).unwrap(), vec![1]);
        // Saved bytes are restored even for the torn stride.
        assert_eq!(&torn[1022..1024], &[0x33, 0x44]);
    }

    #[test]
    fn rejects_bad_arrays() {
        let mut r = record();
        r[6] = 9;
        assert!(apply_fixups(&mut r).is_err());
        let mut r = record();
        r[4..6].copy_from_slice(&600u16.to_le_bytes());
        assert!(apply_fixups(&mut r).is_err());
        let mut r = record();
        r[4..6].copy_from_slice(&0x31u16.to_le_bytes());
        assert!(apply_fixups(&mut r).is_err());
        let mut short = vec![0u8; 100];
        assert!(apply_fixups(&mut short).is_err());
        let mut odd = vec![0u8; 700];
        assert!(apply_fixups(&mut odd).is_err());
    }

    #[test]
    fn four_k_records() {
        let mut r = vec![0u8; 4096];
        r[4..6].copy_from_slice(&0x30u16.to_le_bytes());
        r[6..8].copy_from_slice(&9u16.to_le_bytes());
        for (i, b) in r.iter_mut().enumerate().skip(0x50) {
            *b = (i % 251) as u8;
        }
        let orig = r.clone();
        protect(&mut r, 0x1234);
        assert_eq!(apply_fixups(&mut r).unwrap(), Vec::<usize>::new());
        assert_eq!(r[0x50..], orig[0x50..]);
    }
}
