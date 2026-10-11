//! Bounds-checked integer readers for on-disk structures.
//!
//! All functions return `None` instead of panicking when the slice is too
//! short, so parsers of damaged data can stay panic-free.

/// Read a `u8`.
#[inline]
pub fn u8_at(b: &[u8], off: usize) -> Option<u8> {
    b.get(off).copied()
}

/// Little-endian `u16`.
#[inline]
pub fn u16le(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(off..off + 2)?.try_into().ok()?))
}

/// Big-endian `u16`.
#[inline]
pub fn u16be(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_be_bytes(b.get(off..off + 2)?.try_into().ok()?))
}

/// Little-endian `u32`.
#[inline]
pub fn u32le(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?))
}

/// Big-endian `u32`.
#[inline]
pub fn u32be(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(off..off + 4)?.try_into().ok()?))
}

/// Little-endian `u64`.
#[inline]
pub fn u64le(b: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(off..off + 8)?.try_into().ok()?))
}

/// Big-endian `u64`.
#[inline]
pub fn u64be(b: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_be_bytes(b.get(off..off + 8)?.try_into().ok()?))
}

/// Little-endian `i32`.
#[inline]
pub fn i32le(b: &[u8], off: usize) -> Option<i32> {
    u32le(b, off).map(|v| v as i32)
}

/// Big-endian `i32`.
#[inline]
pub fn i32be(b: &[u8], off: usize) -> Option<i32> {
    u32be(b, off).map(|v| v as i32)
}

/// Little-endian `i64`.
#[inline]
pub fn i64le(b: &[u8], off: usize) -> Option<i64> {
    u64le(b, off).map(|v| v as i64)
}

/// Little-endian 24-bit unsigned.
#[inline]
pub fn u24le(b: &[u8], off: usize) -> Option<u32> {
    let s = b.get(off..off + 3)?;
    Some(s[0] as u32 | (s[1] as u32) << 8 | (s[2] as u32) << 16)
}

/// Big-endian 24-bit unsigned.
#[inline]
pub fn u24be(b: &[u8], off: usize) -> Option<u32> {
    let s = b.get(off..off + 3)?;
    Some((s[0] as u32) << 16 | (s[1] as u32) << 8 | s[2] as u32)
}

/// A sub-slice, or `None` when out of bounds.
#[inline]
pub fn slice(b: &[u8], off: usize, len: usize) -> Option<&[u8]> {
    b.get(off..off.checked_add(len)?)
}

/// Endianness selector for formats that support both (TIFF).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endian {
    Little,
    Big,
}

impl Endian {
    /// Read a `u16` in this endianness.
    #[inline]
    pub fn u16(self, b: &[u8], off: usize) -> Option<u16> {
        match self {
            Endian::Little => u16le(b, off),
            Endian::Big => u16be(b, off),
        }
    }

    /// Read a `u32` in this endianness.
    #[inline]
    pub fn u32(self, b: &[u8], off: usize) -> Option<u32> {
        match self {
            Endian::Little => u32le(b, off),
            Endian::Big => u32be(b, off),
        }
    }

    /// Read a `u64` in this endianness.
    #[inline]
    pub fn u64(self, b: &[u8], off: usize) -> Option<u64> {
        match self {
            Endian::Little => u64le(b, off),
            Endian::Big => u64be(b, off),
        }
    }
}

/// Decode a NUL-padded or space-padded ASCII field, trimming both.
pub fn ascii_trimmed(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).trim().to_string()
}

/// Decode UTF-16LE code units into a string, stopping at the first NUL
/// (used by FAT LFN, exFAT, NTFS, GPT names). Invalid surrogates become
/// U+FFFD.
pub fn utf16le_string(b: &[u8]) -> String {
    let units: Vec<u16> = b
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&u| u != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

/// Decode UTF-16LE code units of a known length without NUL termination.
pub fn utf16le_exact(b: &[u8], units: usize) -> String {
    let v: Vec<u16> = b
        .chunks_exact(2)
        .take(units)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16_lossy(&v)
}

/// Format a GUID stored in the mixed-endian layout used by GPT and Windows
/// (first three fields little-endian) as the usual text form.
pub fn guid_mixed_endian(b: &[u8]) -> Option<String> {
    if b.len() < 16 {
        return None;
    }
    Some(format!(
        "{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        u32le(b, 0)?,
        u16le(b, 4)?,
        u16le(b, 6)?,
        b[8],
        b[9],
        b[10],
        b[11],
        b[12],
        b[13],
        b[14],
        b[15]
    ))
}

/// Lowercase hex of a byte slice.
pub fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        use std::fmt::Write;
        let _ = write!(s, "{x:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readers_are_bounds_checked() {
        let b = [1u8, 2, 3, 4, 5, 6, 7, 8, 9];
        assert_eq!(u16le(&b, 0), Some(0x0201));
        assert_eq!(u16be(&b, 0), Some(0x0102));
        assert_eq!(u32le(&b, 1), Some(0x05040302));
        assert_eq!(u32be(&b, 1), Some(0x02030405));
        assert_eq!(u64le(&b, 1), Some(0x0908070605040302));
        assert_eq!(u64le(&b, 2), None);
        assert_eq!(u24be(&b, 0), Some(0x010203));
        assert_eq!(u24le(&b, 0), Some(0x030201));
        assert_eq!(slice(&b, 8, 2), None);
        assert_eq!(slice(&b, usize::MAX, 2), None);
        assert_eq!(Endian::Big.u16(&b, 7), Some(0x0809));
    }

    #[test]
    fn strings_and_guids() {
        assert_eq!(ascii_trimmed(b"  SanDisk\0\0"), "SanDisk");
        assert_eq!(utf16le_string(&[b'H', 0, b'i', 0, 0, 0, b'x', 0]), "Hi");
        assert_eq!(utf16le_exact(&[b'H', 0, b'i', 0, b'!', 0], 2), "Hi");
        let g = [
            0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E,
            0xC9, 0x3B,
        ];
        assert_eq!(
            guid_mixed_endian(&g).unwrap(),
            "C12A7328-F81F-11D2-BA4B-00A0C93EC93B"
        );
        assert_eq!(hex(&[0xde, 0xad]), "dead");
    }
}
