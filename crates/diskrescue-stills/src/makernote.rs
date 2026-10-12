//! Maker-note preview pointers.
//!
//! Several RAW formats keep a JPEG preview that is *only* referenced from the
//! manufacturer's private maker note, and can sit beyond every strip and
//! standard IFD pointer. A walk that ignored them would cut such files short.
//! This module understands the four layouts that matter, identified by the
//! header of the maker note itself (not by the camera make):
//!
//! | Header | Layout | Pointer |
//! |---|---|---|
//! | `Nikon\0` | own TIFF header at +10, offsets relative to it | tag 0x0011 `PreviewIFD` holding 0x0201 / 0x0202 |
//! | `AOC\0` | `II`/`MM` at +4, IFD at +6, offsets from the file start | tags 0x0004 `PreviewImageStart`, 0x0003 `PreviewImageLength` |
//! | `OLYMPUS\0`, `OM SYSTEM\0` | `II`/`MM` marker, IFD at +12 / +16, offsets relative to the maker note | tag 0x2020 `CameraSettings`: 0x0101 start, 0x0102 length |
//! | `OLYMP\0` | IFD at +8, offsets from the file start | same as above |
//! | `SONY DSC \0\0\0` | IFD at +12, offsets from the file start | 0x2001 `PreviewImage`; 0xB000 `FileFormat` (SR2 vs ARW) |
//!
//! Anything unrecognised or unreadable yields nothing. The caller verifies
//! that each returned pointer really lands on `FF D8` before trusting it.

use diskrescue_core::util::bytes::Endian;

use crate::tiff::{parse_ifd, read_uints, Entry, Ifd};
use crate::win::Win;

/// What a maker note contributed.
#[derive(Debug, Default)]
pub(crate) struct MakerInfo {
    /// `(file-relative offset, length)` of JPEG previews.
    pub previews: Vec<(u64, u64)>,
    /// Sony `FileFormat` bytes.
    pub sony_file_format: Option<[u8; 4]>,
}

fn marker_endian(b: &[u8]) -> Option<Endian> {
    match b.get(..2)? {
        b"II" => Some(Endian::Little),
        b"MM" => Some(Endian::Big),
        _ => None,
    }
}

fn scalar(e: &Entry, endian: Endian) -> Option<u64> {
    e.scalar(endian)
}

fn pair(ifd: &Ifd, endian: Endian, start_tag: u16, len_tag: u16) -> Option<(u64, u64)> {
    let s = scalar(ifd.find(start_tag)?, endian)?;
    let l = scalar(ifd.find(len_tag)?, endian)?;
    (l > 0).then_some((s, l))
}

/// Parse the maker note of `len` bytes at file offset `off`.
pub(crate) fn parse(win: &Win, outer: Endian, _make: &str, off: u64, len: u64) -> MakerInfo {
    let mut info = MakerInfo::default();
    if len < 16 || off >= win.avail() {
        return info;
    }
    let Ok(head) = win.read_upto(off, 32) else {
        return info;
    };
    if head.starts_with(b"Nikon\0") {
        nikon(win, off, &mut info);
    } else if head.starts_with(b"AOC\0") {
        pentax(win, outer, off, &head, &mut info);
    } else if head.starts_with(b"OLYMPUS\0") {
        if let Some(e) = marker_endian(&head[8..]) {
            olympus(win, e, off, off + 12, off, &mut info);
        }
    } else if head.starts_with(b"OM SYSTEM\0") {
        if let Some(e) = marker_endian(&head[12..]) {
            olympus(win, e, off, off + 16, off, &mut info);
        }
    } else if head.starts_with(b"OLYMP\0") {
        olympus(win, outer, off, off + 8, 0, &mut info);
    } else if head.starts_with(b"SONY DSC \0\0\0") || head.starts_with(b"SONY CAM \0\0\0") {
        sony(win, outer, off + 12, &mut info);
    }
    info
}

fn nikon(win: &Win, off: u64, info: &mut MakerInfo) {
    let base = off + 10;
    let Ok(h) = win.read(base, 8) else {
        return;
    };
    let Some((crate::tiff::Magic::Classic, endian)) = crate::tiff::Magic::detect(&h) else {
        return;
    };
    let Some(ifd_off) = endian.u32(&h, 4) else {
        return;
    };
    let Ok(Some(ifd)) = parse_ifd(win, base + ifd_off as u64, endian, false) else {
        return;
    };
    let Some(pv) = ifd.find(0x0011).and_then(|e| scalar(e, endian)) else {
        return;
    };
    let Ok(Some(pifd)) = parse_ifd(win, base.saturating_add(pv), endian, false) else {
        return;
    };
    if let Some((s, l)) = pair(&pifd, endian, 0x0201, 0x0202) {
        info.previews.push((base.saturating_add(s), l));
    }
}

fn pentax(win: &Win, outer: Endian, off: u64, head: &[u8], info: &mut MakerInfo) {
    let (endian, ifd_off) = match marker_endian(&head[4..]) {
        Some(e) => (e, off + 6),
        None => (outer, off + 4),
    };
    let Ok(Some(ifd)) = parse_ifd(win, ifd_off, endian, false) else {
        return;
    };
    if let Some((s, l)) = pair(&ifd, endian, 0x0004, 0x0003) {
        info.previews.push((s, l));
    }
}

fn olympus(win: &Win, endian: Endian, _mn: u64, ifd_off: u64, base: u64, info: &mut MakerInfo) {
    let Ok(Some(ifd)) = parse_ifd(win, ifd_off, endian, false) else {
        return;
    };
    let Some(cs) = ifd.find(0x2020).and_then(|e| scalar(e, endian)) else {
        return;
    };
    let Ok(Some(cifd)) = parse_ifd(win, base.saturating_add(cs), endian, false) else {
        return;
    };
    if let Some((s, l)) = pair(&cifd, endian, 0x0101, 0x0102) {
        info.previews.push((base.saturating_add(s), l));
    }
}

fn sony(win: &Win, endian: Endian, ifd_off: u64, info: &mut MakerInfo) {
    let Ok(Some(ifd)) = parse_ifd(win, ifd_off, endian, false) else {
        return;
    };
    if let Some(e) = ifd.find(0xB000) {
        if e.inline && e.typ == 1 && e.count == 4 {
            let b = e.inline_bytes();
            info.sony_file_format = Some([b[0], b[1], b[2], b[3]]);
        } else if let Ok(v) = read_uints(win, endian, e, 0, 4) {
            if v.len() == 4 {
                info.sony_file_format = Some([v[0] as u8, v[1] as u8, v[2] as u8, v[3] as u8]);
            }
        }
    }
    if let Some(e) = ifd.find(0x2001) {
        if !e.inline && matches!(e.typ, 1 | 7) && e.count > 0 {
            info.previews.push((e.value_or_offset, e.count));
        }
    }
}
