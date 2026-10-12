//! Spotting the start of another file inside a declared extent.
//!
//! A size field says how long a file *was*. When the file was cut short and
//! other files were written after it, or its clusters were reused after a
//! deletion, the bytes the field covers belong to those other files, and a
//! measurement that trusts the field swallows them. Compressed data cannot be
//! checked cheaply, but the start of another file can: files on a filesystem
//! begin on a sector boundary, and most formats begin with a strong
//! signature. [`identify`] recognises those signatures (checking a few header
//! fields beyond the magic so random data almost never matches) and
//! [`scan_source`] samples sector-aligned offsets of a range.
//!
//! At most [`MAX_SAMPLES`] samples are read per scan (the stride doubles for
//! longer ranges), sectors the source knows to be unreadable are skipped, and
//! the scan gives up after [`MAX_READ_ERRORS`] failed reads.

use crate::source::Source;
use crate::util::bytes::{u16be, u32be, u32le};
use crate::Range;

/// Sampling grid.
pub const SECTOR: u64 = 512;
/// Bytes read per sample.
pub const PROBE: usize = 16;
/// Most samples per scan.
pub const MAX_SAMPLES: u64 = 8192;
/// Failed reads after which the scan stops.
pub const MAX_READ_ERRORS: u32 = 8;

/// A file signature the scan recognises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sig {
    Jpeg,
    Png,
    Gif,
    Webp,
    Avi,
    Wav,
    Isobmff,
    Matroska,
    Tiff,
    Psd,
    Raf,
    Flac,
    Mp3,
    MpegPs,
}

impl Sig {
    /// Name for notes.
    pub fn name(self) -> &'static str {
        match self {
            Sig::Jpeg => "JPEG",
            Sig::Png => "PNG",
            Sig::Gif => "GIF",
            Sig::Webp => "WebP",
            Sig::Avi => "AVI",
            Sig::Wav => "WAV",
            Sig::Isobmff => "MP4/MOV/HEIF",
            Sig::Matroska => "Matroska",
            Sig::Tiff => "TIFF",
            Sig::Psd => "PSD",
            Sig::Raf => "RAF",
            Sig::Flac => "FLAC",
            Sig::Mp3 => "MP3",
            Sig::MpegPs => "MPEG-PS",
        }
    }
}

/// A recognised file start, file-relative.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hit {
    pub offset: u64,
    pub sig: Sig,
}

fn printable(b: &[u8]) -> bool {
    b.iter().all(|c| (0x20..=0x7E).contains(c))
}

/// The JPEG marker segment after `FF D8 FF` must look like the start of a
/// real file, not three bytes of pixel data.
fn jpeg_start(b: &[u8]) -> bool {
    let (Some(&m), Some(len)) = (b.get(3), u16be(b, 4)) else {
        return false;
    };
    let id = |n: usize| b.get(6..6 + n);
    match m {
        0xE0 => len >= 16 && matches!(id(5), Some(b"JFIF\0" | b"JFXX\0")),
        0xE1 => len >= 8 && (id(6) == Some(b"Exif\0\0") || id(5) == Some(b"http:")),
        0xE2..=0xEF => len >= 6 && id(4).is_some_and(printable),
        0xDB => len >= 67 && b.get(6).is_some_and(|&t| t >> 4 <= 1 && t & 0x0F <= 3),
        0xC4 => len >= 19 && b.get(6).is_some_and(|&t| t >> 4 <= 1 && t & 0x0F <= 3),
        0xDD => len == 4,
        0xFE => len >= 3,
        0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF => {
            len >= 11 && b.get(6).is_some_and(|p| matches!(p, 8 | 12 | 16))
        }
        _ => false,
    }
}

/// Identify a file signature at the start of `b` (up to [`PROBE`] bytes;
/// fewer near the end of the window, in which case longer checks fail).
pub fn identify(b: &[u8]) -> Option<Sig> {
    let at = |o: usize, pat: &[u8]| b.get(o..o + pat.len()) == Some(pat);
    let first = *b.first()?;
    match first {
        0xFF if at(0, &[0xFF, 0xD8, 0xFF]) && jpeg_start(b) => Some(Sig::Jpeg),
        0x89 if at(0, b"\x89PNG\r\n\x1a\n") && u32be(b, 8) == Some(13) && at(12, b"IHDR") => {
            Some(Sig::Png)
        }
        b'G' if at(0, b"GIF87a") || at(0, b"GIF89a") => Some(Sig::Gif),
        b'R' if at(0, b"RIFF") => {
            if at(8, b"WEBP") {
                Some(Sig::Webp)
            } else if at(8, b"AVI ") {
                Some(Sig::Avi)
            } else if at(8, b"WAVE") {
                Some(Sig::Wav)
            } else {
                None
            }
        }
        b'R' if at(0, b"RF64") && at(8, b"WAVE") => Some(Sig::Wav),
        0x1A if at(0, &[0x1A, 0x45, 0xDF, 0xA3]) && b.get(4).is_some_and(|&v| v != 0) => {
            Some(Sig::Matroska)
        }
        b'I' => {
            let ok = (at(0, b"II*\0") && u32le(b, 4).is_some_and(|o| o >= 8))
                || ((at(0, b"IIRO") || at(0, b"IIRS")) && u32le(b, 4) == Some(8))
                || (at(0, b"IIU\0") && u32le(b, 4) == Some(0x18))
                || at(0, b"II+\0\x08\0\0\0");
            if ok {
                Some(Sig::Tiff)
            } else if at(0, b"ID3")
                && b.get(3).is_some_and(|v| (2..=4).contains(v))
                && b.get(4) == Some(&0)
                && b.get(5).is_some_and(|f| f & 0x0F == 0)
                && b.get(6..10).is_some_and(|s| s.iter().all(|&x| x < 0x80))
            {
                Some(Sig::Mp3)
            } else {
                None
            }
        }
        b'M' => {
            let ok = (at(0, b"MM\0*") && u32be(b, 4).is_some_and(|o| o >= 8))
                || (at(0, b"MMOR") && u32be(b, 4) == Some(8))
                || at(0, b"MM\0+\0\x08\0\0");
            ok.then_some(Sig::Tiff)
        }
        b'8' if at(0, b"8BPS")
            && (at(4, &[0, 1]) || at(4, &[0, 2]))
            && at(6, &[0, 0, 0, 0, 0, 0]) =>
        {
            Some(Sig::Psd)
        }
        b'F' if at(0, b"FUJIFILMCCD-RAW ") => Some(Sig::Raf),
        b'f' if at(0, b"fLaC")
            && b.get(4).is_some_and(|&t| t & 0x7F == 0)
            && at(5, &[0, 0, 34]) =>
        {
            Some(Sig::Flac)
        }
        0 if at(0, &[0, 0, 1, 0xBA])
            && b.get(4)
                .is_some_and(|&x| x & 0xC4 == 0x44 || x & 0xF1 == 0x21) =>
        {
            Some(Sig::MpegPs)
        }
        _ => {
            // ISO BMFF: a plausible box size and `ftyp` with a printable brand.
            let size = u32be(b, 0)?;
            ((8..=4096).contains(&size)
                && size % 4 == 0
                && at(4, b"ftyp")
                && b.get(8..12).is_some_and(|br| {
                    br.iter()
                        .all(|c| c.is_ascii_alphanumeric() || *c == b' ' || *c == b'_')
                }))
            .then_some(Sig::Isobmff)
        }
    }
}

/// Look for the start of another file at sector-aligned offsets in
/// `from..to`, relative to `file_start` (offset 0, the file's own header, is
/// never tested). `allowed(offset, sig)` returns true for hits the file's own
/// structure explains. Returns the first unexplained hit. Reads whole sectors
/// so direct-I/O sources work.
pub fn scan_source(
    src: &dyn Source,
    file_start: u64,
    from: u64,
    to: u64,
    allowed: &dyn Fn(u64, Sig) -> bool,
) -> Option<Hit> {
    let size = src.size();
    let to = to.min(size.saturating_sub(file_start));
    let from = from.max(1);
    if from >= to {
        return None;
    }
    let sector = (src.sector_size().max(512) as u64).max(SECTOR);
    let phase = (sector - file_start % sector) % sector;
    let mut stride = sector;
    while (to - from) / stride > MAX_SAMPLES {
        stride *= 2;
    }
    let mut p = if from <= phase {
        phase
    } else {
        phase + (from - phase).div_ceil(stride) * stride
    };
    let holes = src.unreadable();
    let mut errors = 0u32;
    let mut buf = vec![0u8; sector as usize];
    while p < to {
        let abs = file_start.saturating_add(p);
        let in_hole = holes
            .as_ref()
            .is_some_and(|h| h.intersects(Range::with_len(abs, PROBE as u64)));
        if !in_hole {
            match src.read_at(abs, &mut buf) {
                Ok(n) => {
                    let n = n.min(PROBE).min((to - p) as usize);
                    if let Some(sig) = identify(&buf[..n]) {
                        if !allowed(p, sig) {
                            return Some(Hit { offset: p, sig });
                        }
                    }
                }
                Err(_) => {
                    errors += 1;
                    if errors >= MAX_READ_ERRORS {
                        return None;
                    }
                }
            }
        }
        p = p.saturating_add(stride);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::MemSource;

    #[test]
    fn identifies_strong_signatures_only() {
        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
        jpeg.extend_from_slice(b"JFIF\0");
        assert_eq!(identify(&jpeg), Some(Sig::Jpeg));
        assert_eq!(identify(&[0xFF, 0xD8, 0xFF, 0x00, 0x00, 0x00]), None);
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        assert_eq!(identify(&png), Some(Sig::Png));
        let mut mp4 = 24u32.to_be_bytes().to_vec();
        mp4.extend_from_slice(b"ftypisom");
        assert_eq!(identify(&mp4), Some(Sig::Isobmff));
        assert_eq!(identify(&[0u8; 16]), None);
        assert_eq!(identify(&[]), None);
    }

    #[test]
    fn scan_finds_unexplained_starts_on_the_grid() {
        let mut data = vec![0x55u8; 8192];
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        data[4096..4096 + png.len()].copy_from_slice(&png);
        // Off the grid: ignored.
        data[1000..1000 + png.len()].copy_from_slice(&png);
        let src = MemSource::new(data);
        let hit = scan_source(&src, 0, 1, 8192, &|_, _| false).unwrap();
        assert_eq!(hit.offset, 4096);
        assert_eq!(hit.sig, Sig::Png);
        assert!(scan_source(&src, 0, 1, 8192, &|o, _| o == 4096).is_none());
        assert!(scan_source(&src, 0, 1, 4096, &|_, _| false).is_none());
        // File starting at 512: offsets are file-relative.
        let hit = scan_source(&src, 512, 1, 7680, &|_, _| false).unwrap();
        assert_eq!(hit.offset, 3584);
    }
}
