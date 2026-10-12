//! A tiny JPEG header reader.
//!
//! The still-image containers in this crate embed JPEG previews and
//! thumbnails (RAW files, RAF, PSD). To report them as nested objects the
//! walkers need three facts about the bytes at a given offset: is it a JPEG
//! at all, is it a *lossless* JPEG (the SOF3 raw-data strips of CR2 and DNG,
//! which are sensor data and not previews), and where is its Exif block.
//! This module answers them from the first few KiB without decoding
//! anything. The full JPEG machinery lives in `diskrescue-jpeg`.

/// What the first segments of a JPEG stream say about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct JpegInfo {
    /// Frame width in pixels (0 when no SOF was seen in the examined bytes).
    pub width: u32,
    /// Frame height in pixels (0 when unknown, or defined later by DNL).
    pub height: u32,
    /// True for lossless (SOF3, SOF7, SOF11, SOF15) frames.
    pub lossless: bool,
    /// Offset and length of the TIFF header inside an `Exif\0\0` APP1
    /// segment, relative to the start of the JPEG stream.
    pub exif: Option<(usize, usize)>,
    /// True when a frame header was found.
    pub has_frame: bool,
}

/// Maximum number of marker segments examined.
const MAX_SEGMENTS: usize = 128;

/// Examine the start of a JPEG stream. Returns `None` unless `head` starts
/// with `FF D8 FF`.
pub(crate) fn inspect(head: &[u8]) -> Option<JpegInfo> {
    if head.len() < 4 || head[0] != 0xFF || head[1] != 0xD8 || head[2] != 0xFF {
        return None;
    }
    let mut info = JpegInfo {
        width: 0,
        height: 0,
        lossless: false,
        exif: None,
        has_frame: false,
    };
    let mut pos = 2usize;
    for _ in 0..MAX_SEGMENTS {
        // Skip fill bytes: any run of FF before the marker code.
        while head.get(pos) == Some(&0xFF) && head.get(pos + 1) == Some(&0xFF) {
            pos += 1;
        }
        if head.get(pos) != Some(&0xFF) {
            break;
        }
        let Some(&marker) = head.get(pos + 1) else {
            break;
        };
        pos += 2;
        match marker {
            0x00 | 0x01 | 0xD0..=0xD8 => continue,
            0xD9 | 0xDA => break,
            _ => {}
        }
        let Some(len) = head
            .get(pos..pos + 2)
            .map(|b| u16::from_be_bytes([b[0], b[1]]) as usize)
        else {
            break;
        };
        if len < 2 {
            break;
        }
        let body = pos + 2;
        let end = pos + len;
        match marker {
            0xC0..=0xCF if !matches!(marker, 0xC4 | 0xC8 | 0xCC) => {
                if !info.has_frame {
                    if let (Some(h), Some(w)) = (
                        head.get(body + 1..body + 3)
                            .map(|b| u16::from_be_bytes([b[0], b[1]])),
                        head.get(body + 3..body + 5)
                            .map(|b| u16::from_be_bytes([b[0], b[1]])),
                    ) {
                        info.height = h as u32;
                        info.width = w as u32;
                        info.has_frame = true;
                        info.lossless = matches!(marker, 0xC3 | 0xC7 | 0xCB | 0xCF);
                    }
                }
            }
            0xE1 if info.exif.is_none() && head.get(body..body + 6) == Some(b"Exif\0\0") => {
                info.exif = Some((body + 6, len.saturating_sub(8)));
            }
            _ => {}
        }
        pos = end;
        if pos >= head.len() {
            break;
        }
    }
    Some(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(marker: u8, body: &[u8]) -> Vec<u8> {
        let mut v = vec![0xFF, marker];
        v.extend_from_slice(&((body.len() + 2) as u16).to_be_bytes());
        v.extend_from_slice(body);
        v
    }

    #[test]
    fn reads_frame_and_exif() {
        let mut j = vec![0xFF, 0xD8];
        let mut exif = b"Exif\0\0".to_vec();
        exif.extend_from_slice(b"II*\0\x08\0\0\0");
        j.extend(seg(0xE1, &exif));
        j.extend(seg(0xC0, &[8, 0x00, 0x30, 0x00, 0x40, 3, 1, 0x22, 0]));
        j.extend(seg(0xDA, &[0]));
        let i = inspect(&j).unwrap();
        assert_eq!((i.width, i.height), (0x40, 0x30));
        assert!(!i.lossless);
        assert_eq!(i.exif, Some((2 + 4 + 6, 8)));
    }

    #[test]
    fn lossless_frames_are_flagged() {
        let mut j = vec![0xFF, 0xD8];
        j.extend(seg(0xC4, &[0; 17]));
        j.extend(seg(0xC3, &[14, 0x0F, 0xA0, 0x17, 0x70, 2, 1, 0x11, 0]));
        let i = inspect(&j).unwrap();
        assert!(i.lossless);
        assert_eq!((i.width, i.height), (0x1770, 0x0FA0));
    }

    #[test]
    fn rejects_non_jpeg_and_survives_garbage() {
        assert!(inspect(b"GIF89a").is_none());
        assert!(inspect(&[]).is_none());
        let mut x = 0x1234_5678u32;
        for _ in 0..2000 {
            let mut b = vec![0xFF, 0xD8, 0xFF];
            for _ in 0..200 {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                b.push(x as u8);
            }
            let _ = inspect(&b);
        }
    }
}
