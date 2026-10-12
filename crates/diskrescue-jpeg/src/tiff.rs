//! Minimal, bounded TIFF IFD reader for the TIFF structures embedded in
//! JPEG segments (Exif in APP1, MPF in APP2).
//!
//! Every offset is checked against the buffer, IFDs are capped at
//! [`MAX_ENTRIES`] entries, and unknown field types are skipped rather than
//! treated as errors (MakerNotes contain junk types).

use diskrescue_core::util::bytes::Endian;

/// Most entries read from one IFD.
pub(crate) const MAX_ENTRIES: usize = 512;

/// A TIFF structure in memory (`data[0]` is the byte-order mark).
pub(crate) struct Tiff<'a> {
    pub(crate) data: &'a [u8],
    pub(crate) endian: Endian,
}

/// One 12-byte IFD entry.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Entry {
    pub(crate) tag: u16,
    pub(crate) typ: u16,
    pub(crate) count: u32,
    /// Offset of the 4-byte value/offset field inside the TIFF.
    pub(crate) field: usize,
}

/// Size in bytes of one value of a TIFF field type, `None` when unknown.
pub(crate) fn type_size(typ: u16) -> Option<usize> {
    Some(match typ {
        1 | 2 | 6 | 7 => 1,
        3 | 8 => 2,
        4 | 9 | 11 | 13 => 4,
        5 | 10 | 12 => 8,
        _ => return None,
    })
}

impl<'a> Tiff<'a> {
    /// Parse the 8-byte header; `None` when it is not `II*\0` / `MM\0*`.
    pub(crate) fn new(data: &'a [u8]) -> Option<(Self, u32)> {
        let endian = match data.get(0..4)? {
            [b'I', b'I', 0x2A, 0x00] => Endian::Little,
            [b'M', b'M', 0x00, 0x2A] => Endian::Big,
            _ => return None,
        };
        let first = endian.u32(data, 4)?;
        Some((Tiff { data, endian }, first))
    }

    /// `u16` at `off`.
    pub(crate) fn u16(&self, off: usize) -> Option<u16> {
        self.endian.u16(self.data, off)
    }

    /// `u32` at `off`.
    pub(crate) fn u32(&self, off: usize) -> Option<u32> {
        self.endian.u32(self.data, off)
    }

    /// Read the IFD at `off`: its entries and the next-IFD offset.
    pub(crate) fn ifd(&self, off: u32) -> Option<(Vec<Entry>, u32)> {
        let off = off as usize;
        let count = self.u16(off)? as usize;
        let mut entries = Vec::with_capacity(count.min(MAX_ENTRIES));
        for i in 0..count.min(MAX_ENTRIES) {
            let e = off + 2 + i * 12;
            let (Some(tag), Some(typ), Some(cnt)) = (self.u16(e), self.u16(e + 2), self.u32(e + 4))
            else {
                break;
            };
            entries.push(Entry {
                tag,
                typ,
                count: cnt,
                field: e + 8,
            });
        }
        let next = if count <= MAX_ENTRIES {
            self.u32(off + 2 + count * 12).unwrap_or(0)
        } else {
            0
        };
        Some((entries, next))
    }

    /// The raw bytes of an entry's value, inline or out of line, bounds
    /// checked. `None` for unknown types or out-of-range data.
    pub(crate) fn value_bytes(&self, e: &Entry) -> Option<&'a [u8]> {
        let size = type_size(e.typ)?.checked_mul(e.count as usize)?;
        if size <= 4 {
            self.data.get(e.field..e.field + size)
        } else {
            let off = self.u32(e.field)? as usize;
            self.data.get(off..off.checked_add(size)?)
        }
    }

    /// First value of a SHORT or LONG entry.
    pub(crate) fn uint(&self, e: &Entry) -> Option<u32> {
        match e.typ {
            3 | 8 => self.u16(e.field).map(u32::from),
            4 | 9 | 13 => self.u32(e.field),
            1 | 7 => self.data.get(e.field).map(|&b| b as u32),
            _ => None,
        }
    }

    /// An ASCII entry as a trimmed string (empty strings become `None`).
    pub(crate) fn ascii(&self, e: &Entry) -> Option<String> {
        if e.typ != 2 && e.typ != 7 {
            return None;
        }
        let b = self.value_bytes(e)?;
        let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
        let s: String = String::from_utf8_lossy(&b[..end])
            .chars()
            .filter(|c| !c.is_control())
            .collect();
        let s = s.trim().to_string();
        (!s.is_empty()).then_some(s)
    }

    /// RATIONAL values of an entry.
    pub(crate) fn rationals(&self, e: &Entry) -> Option<Vec<f64>> {
        if e.typ != 5 && e.typ != 10 {
            return None;
        }
        let b = self.value_bytes(e)?;
        let mut v = Vec::new();
        for i in 0..(e.count as usize).min(16) {
            let n = self.endian.u32(b, i * 8)?;
            let d = self.endian.u32(b, i * 8 + 4)?;
            let (n, d) = if e.typ == 10 {
                (n as i32 as f64, d as i32 as f64)
            } else {
                (n as f64, d as f64)
            };
            if d == 0.0 {
                return None;
            }
            v.push(n / d);
        }
        Some(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_both_byte_orders() {
        // II, IFD at 8 with one SHORT entry (tag 0x0112 = 6) and next = 0.
        let mut le = b"II*\0\x08\0\0\0".to_vec();
        le.extend_from_slice(&1u16.to_le_bytes());
        le.extend_from_slice(&0x0112u16.to_le_bytes());
        le.extend_from_slice(&3u16.to_le_bytes());
        le.extend_from_slice(&1u32.to_le_bytes());
        le.extend_from_slice(&[6, 0, 0, 0]);
        le.extend_from_slice(&0u32.to_le_bytes());
        let (t, first) = Tiff::new(&le).unwrap();
        let (entries, next) = t.ifd(first).unwrap();
        assert_eq!(next, 0);
        assert_eq!(entries[0].tag, 0x0112);
        assert_eq!(t.uint(&entries[0]), Some(6));

        let mut be = b"MM\0*\0\0\0\x08".to_vec();
        be.extend_from_slice(&1u16.to_be_bytes());
        be.extend_from_slice(&0x0112u16.to_be_bytes());
        be.extend_from_slice(&3u16.to_be_bytes());
        be.extend_from_slice(&1u32.to_be_bytes());
        be.extend_from_slice(&[0, 3, 0, 0]);
        be.extend_from_slice(&0u32.to_be_bytes());
        let (t, first) = Tiff::new(&be).unwrap();
        let (entries, _) = t.ifd(first).unwrap();
        assert_eq!(t.uint(&entries[0]), Some(3));
    }

    #[test]
    fn hostile_offsets_do_not_panic() {
        let mut d = b"II*\0\xff\xff\xff\x7f".to_vec();
        d.extend_from_slice(&[0xFF; 64]);
        let (t, first) = Tiff::new(&d).unwrap();
        assert!(t.ifd(first).is_none());
        // An IFD whose count claims 65535 entries in a tiny buffer.
        let mut d = b"II*\0\x08\0\0\0\xff\xff".to_vec();
        d.extend_from_slice(&[0x02, 0x01, 0x02, 0x00, 0xff, 0xff, 0xff, 0xff, 0, 0, 0, 0]);
        let (t, first) = Tiff::new(&d).unwrap();
        let (entries, next) = t.ifd(first).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(next, 0);
        assert!(t.value_bytes(&entries[0]).is_none());
        assert!(t.ascii(&entries[0]).is_none());
    }
}
