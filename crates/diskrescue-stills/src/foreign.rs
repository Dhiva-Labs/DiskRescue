//! Spotting the start of another file inside a declared extent.
//!
//! A size field (the RIFF size, the BMP file size, a TIFF strip table) says
//! how long a file *was*. When the file was cut short and other files were
//! written after it, or its clusters were reused after a deletion, the bytes
//! the field covers belong to those other files, and a measurement that
//! trusts the field swallows them. Pixel arrays and compressed bitstreams
//! cannot be checked cheaply, but the start of another file can: files on a
//! filesystem begin on a sector boundary, and most formats begin with a
//! strong signature.
//!
//! [`scan`] samples sector-aligned offsets inside a range of the candidate,
//! reads [`PROBE`] bytes at each and tests them against the signatures of
//! the formats DiskRescue carves (JPEG, PNG, GIF, RIFF WebP/AVI/WAV, ISO
//! BMFF, Matroska, the TIFF family, PSD, RAF, FLAC, ID3-tagged MP3, MPEG-PS).
//! Each test checks a few header fields beyond the magic, so random pixel
//! data almost never matches. The caller decides which hits its own
//! structure explains (an EXIF block inside a WebP `EXIF` chunk, a JPEG
//! strip that an IFD entry points to).
//!
//! # Bounds
//!
//! At most [`MAX_SAMPLES`] reads of [`PROBE`] bytes each, straight from the
//! source (no cache block per sample). When the range holds more sectors
//! than that, the stride doubles (it stays a power of two times 512, anchored
//! at the file start when that is sector-aligned, so cluster starts are
//! still sampled). Sectors the source already knows to be unreadable are
//! not read, and the scan gives up after [`MAX_READ_ERRORS`] failed reads.

use diskrescue_core::util::bytes::{u16be, u32be, u32le};
use diskrescue_core::Range;

use crate::win::Win;

/// Sampling grid.
pub(crate) const SECTOR: u64 = 512;
/// Bytes read per sample.
pub(crate) const PROBE: usize = 16;
/// Most samples per scan.
pub(crate) const MAX_SAMPLES: u64 = 8192;
/// Failed reads after which the scan stops.
pub(crate) const MAX_READ_ERRORS: u32 = 8;

/// A file signature the scan recognises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Sig {
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
    pub(crate) fn name(self) -> &'static str {
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
pub(crate) struct Hit {
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
pub(crate) fn identify(b: &[u8]) -> Option<Sig> {
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
/// `from..to` (file-relative, clamped to the window; offset 0, the file's
/// own header, is never tested). `allowed(offset, sig)` returns true for
/// hits the file's own structure explains. Returns the first unexplained
/// hit.
pub(crate) fn scan(
    win: &Win,
    from: u64,
    to: u64,
    allowed: &dyn Fn(u64, Sig) -> bool,
) -> Option<Hit> {
    let to = to.min(win.avail());
    let from = from.max(1);
    if from >= to {
        return None;
    }
    // First sector boundary at or after the file start, file-relative.
    let phase = (SECTOR - win.start() % SECTOR) % SECTOR;
    let mut stride = SECTOR;
    while (to - from) / stride > MAX_SAMPLES {
        stride *= 2;
    }
    let mut p = if from <= phase {
        phase
    } else {
        phase + (from - phase).div_ceil(stride) * stride
    };
    let holes = win.unreadable();
    let mut errors = 0u32;
    let mut buf = [0u8; PROBE];
    while p < to {
        let in_hole = holes
            .as_ref()
            .is_some_and(|h| h.intersects(Range::with_len(p, PROBE as u64)));
        if !in_hole {
            match win.peek(p, &mut buf) {
                Some(n) => {
                    if let Some(sig) = identify(&buf[..n]) {
                        if !allowed(p, sig) {
                            return Some(Hit { offset: p, sig });
                        }
                    }
                }
                None => {
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
    use diskrescue_core::source::{MemSource, SharedSource};
    use diskrescue_core::RangeSet;
    use std::sync::Arc;

    fn heads() -> Vec<(Vec<u8>, Sig)> {
        let mut v: Vec<(Vec<u8>, Sig)> = Vec::new();
        let mut p = |b: &[u8], s: Sig| v.push((b.to_vec(), s));
        p(
            b"\xFF\xD8\xFF\xE0\x00\x10JFIF\x00\x01\x01\x00\x00\x01",
            Sig::Jpeg,
        );
        p(b"\xFF\xD8\xFF\xE1\x2A\x10Exif\x00\x00II*\x00", Sig::Jpeg);
        p(
            b"\xFF\xD8\xFF\xDB\x00\x43\x00\x08\x06\x06\x07\x06\x05\x08",
            Sig::Jpeg,
        );
        p(b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0DIHDR", Sig::Png);
        p(b"GIF89a\x10\x00\x10\x00\x80\x00\x00", Sig::Gif);
        p(b"RIFF\x24\x10\x00\x00WEBPVP8 ", Sig::Webp);
        p(b"RIFF\x24\x10\x00\x00AVI LIST", Sig::Avi);
        p(b"RIFF\x24\x10\x00\x00WAVEfmt ", Sig::Wav);
        p(b"\x00\x00\x00\x18ftypheic\x00\x00\x00\x00", Sig::Isobmff);
        p(b"\x1A\x45\xDF\xA3\x9F\x42\x86\x81\x01", Sig::Matroska);
        p(b"II*\x00\x08\x00\x00\x00\x0C\x00", Sig::Tiff);
        p(b"MM\x00*\x00\x00\x00\x08\x00\x0C", Sig::Tiff);
        p(b"IIU\x00\x18\x00\x00\x00", Sig::Tiff);
        p(b"8BPS\x00\x01\x00\x00\x00\x00\x00\x00\x00\x03", Sig::Psd);
        p(b"FUJIFILMCCD-RAW ", Sig::Raf);
        p(b"fLaC\x00\x00\x00\x22\x10\x00", Sig::Flac);
        p(b"ID3\x04\x00\x00\x00\x00\x01\x00", Sig::Mp3);
        p(b"\x00\x00\x01\xBA\x44\x00\x04\x00\x04\x01", Sig::MpegPs);
        v
    }

    #[test]
    fn known_signatures_are_identified() {
        for (h, s) in heads() {
            assert_eq!(identify(&h), Some(s), "{h:02x?}");
        }
    }

    #[test]
    fn near_misses_and_noise_are_not_signatures() {
        for b in [
            &b"\xFF\xD8\xFF\xE0\x00\x10ABCD\x00"[..], // APP0 without JFIF
            b"\xFF\xD8\xFF\xFF\xFF\xFF\xFF\xFF",      // fill
            b"\xFF\xD8\xFF\xDB\x00\x10\x00",          // DQT too short
            b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0DIDAT", // no IHDR
            b"RIFF\x24\x10\x00\x00ABCD",              // unknown form
            b"II*\x00\x02\x00\x00\x00",               // IFD inside the header
            b"\x00\x00\x00\x03ftypheic",              // box too small
            b"\x00\x00\x00\x18ftyp\x01\x02\x03\x04",  // binary brand
            b"ID3\x09\x00\x00\x00\x00\x00\x00",       // bad version
            b"\x00\x00\x01\xBA\x00\x00",              // bad pack header
            b"",
        ] {
            assert_eq!(identify(b), None, "{b:02x?}");
        }
        assert_eq!(identify(&[0u8; 16]), None);
        assert_eq!(identify(&[0xFFu8; 16]), None);
        // Truncated heads fail the longer checks instead of panicking.
        for (h, _) in heads() {
            for n in 0..h.len() {
                let _ = identify(&h[..n]);
            }
        }
    }

    fn win_over(data: Vec<u8>, start: u64, holes: Option<RangeSet>) -> Win {
        let mut m = MemSource::new(data);
        if let Some(h) = holes {
            m = m.with_unreadable(h);
        }
        let src: SharedSource = Arc::new(m);
        Win::new(&src, start, u64::MAX, 4096)
    }

    #[test]
    fn scan_finds_the_first_aligned_unexplained_start() {
        let mut d = vec![0x11u8; 64 * 1024];
        let jpeg = &heads()[0].0;
        let png = &heads()[3].0;
        // Off the grid: ignored. On the grid: found.
        d[1000..1000 + jpeg.len()].copy_from_slice(jpeg);
        d[8192..8192 + png.len()].copy_from_slice(png);
        d[16384..16384 + jpeg.len()].copy_from_slice(jpeg);
        let w = win_over(d.clone(), 0, None);
        let hit = scan(&w, 0, w.avail(), &|_, _| false).unwrap();
        assert_eq!(
            hit,
            Hit {
                offset: 8192,
                sig: Sig::Png
            }
        );
        // An explained hit is skipped.
        let hit = scan(&w, 0, w.avail(), &|o, _| o == 8192).unwrap();
        assert_eq!(hit.offset, 16384);
        // The range is honoured.
        assert!(scan(&w, 0, 8192, &|_, _| false).is_none());
        assert_eq!(scan(&w, 9000, 20_000, &|_, _| false).unwrap().offset, 16384);
        // An unaligned window start samples absolute sector boundaries.
        let w = win_over(d.clone(), 100, None);
        let hit = scan(&w, 0, w.avail(), &|_, _| false).unwrap();
        assert_eq!(hit.offset, 8192 - 100);
        // Known-unreadable sectors are not sampled.
        let holes = RangeSet::from_range(Range::new(8000, 8400));
        let w = win_over(d, 0, Some(holes));
        assert_eq!(scan(&w, 0, w.avail(), &|_, _| false).unwrap().offset, 16384);
    }

    #[test]
    fn scan_is_bounded_on_huge_ranges() {
        // A sparse 1 GiB source: the sample count stays at the cap.
        struct Counting(std::sync::atomic::AtomicU64);
        impl diskrescue_core::Source for Counting {
            fn size(&self) -> u64 {
                1 << 30
            }
            fn read_at(
                &self,
                _: u64,
                buf: &mut [u8],
            ) -> std::result::Result<usize, diskrescue_core::ReadError> {
                self.0
                    .fetch_add(buf.len() as u64, std::sync::atomic::Ordering::Relaxed);
                buf.fill(0);
                Ok(buf.len())
            }
            fn describe(&self) -> String {
                "counting".into()
            }
        }
        let c = Arc::new(Counting(Default::default()));
        let src: SharedSource = c.clone();
        let w = Win::new(&src, 0, u64::MAX, 4096);
        assert!(scan(&w, 0, w.avail(), &|_, _| false).is_none());
        let read = c.0.load(std::sync::atomic::Ordering::Relaxed);
        assert!(read <= MAX_SAMPLES * PROBE as u64, "{read} bytes read");
        assert!(read >= MAX_SAMPLES / 2 * PROBE as u64, "{read} bytes read");
    }
}
