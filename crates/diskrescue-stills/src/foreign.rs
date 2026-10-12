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

use diskrescue_core::Range;

use crate::win::Win;

pub(crate) use diskrescue_core::util::foreign::{
    identify, Hit, Sig, MAX_READ_ERRORS, MAX_SAMPLES, PROBE, SECTOR,
};

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
