//! Large files are processed in bounded chunks, never read whole.

mod common;

use std::io::BufWriter;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use common::*;
use diskrescue_core::format::{
    Confidence, ExtentStatus, FileFormat, RepairContext, RepairResult, ValidationStatus,
};
use diskrescue_core::source::{FileSource, ReadError, SharedSource, Source};
use diskrescue_png::{chunks, lenient, PngFormat};

/// A file followed by an in-memory tail.
struct Concat {
    head: SharedSource,
    tail: Vec<u8>,
}

impl Source for Concat {
    fn size(&self) -> u64 {
        self.head.size() + self.tail.len() as u64
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, ReadError> {
        let hs = self.head.size();
        if offset < hs {
            let n = ((hs - offset) as usize).min(buf.len());
            return self.head.read_at(offset, &mut buf[..n]);
        }
        let o = (offset - hs) as usize;
        if o >= self.tail.len() {
            return Ok(0);
        }
        let n = (self.tail.len() - o).min(buf.len());
        buf[..n].copy_from_slice(&self.tail[o..o + n]);
        Ok(n)
    }

    fn describe(&self) -> String {
        "concat".into()
    }
}

/// Write a noisy RGB PNG of about `target` bytes to `path`.
fn write_big_png(path: &std::path::Path, w: u32, h: u32) -> Vec<u8> {
    let mut rng = Rng::new(0xB16);
    let mut rgb = vec![0u8; (w * h * 3) as usize];
    rng.fill(&mut rgb);
    let f = std::fs::File::create(path).unwrap();
    let mut enc = png::Encoder::new(BufWriter::new(f), w, h);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::Fast);
    let mut wr = enc.write_header().unwrap();
    wr.write_image_data(&rgb).unwrap();
    wr.finish().unwrap();
    // Keep a few rows for spot checks.
    rgb[..(w * 8 * 3) as usize].to_vec()
}

const MIB: usize = 1 << 20;

#[test]
fn fifty_megabyte_png_is_measured_in_a_streaming_fashion() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("big.png");
    let (w, h) = (4200u32, 4200u32);
    let first_rows = write_big_png(&path, w, h);
    let size = std::fs::metadata(&path).unwrap().len();
    assert!(size > 50_000_000, "fixture too small: {size}");

    let file: SharedSource = Arc::new(FileSource::open(&path).unwrap());
    let counting = Arc::new(Counting::new(file.clone()));
    let src: SharedSource = counting.clone();

    // measure(): exact extent, one pass, small reads.
    let t = Instant::now();
    let m = PngFormat.measure(&src, 0, u64::MAX).unwrap().unwrap();
    let dt = t.elapsed();
    assert_eq!(m.len, size);
    assert_eq!(m.status, ExtentStatus::Complete);
    assert!(m.confidence >= Confidence::HIGH);
    assert_eq!((m.meta.width, m.meta.height), (Some(w), Some(h)));
    let max = counting.max_read.load(Ordering::Relaxed);
    let total = counting.total.load(Ordering::Relaxed);
    assert!(max <= MIB, "largest read was {max} bytes");
    assert!(
        total <= size + 2 * MIB as u64,
        "read {total} bytes for a {size} byte file"
    );
    assert!(dt.as_secs() < 30, "measure took {dt:?}");
    eprintln!(
        "measure: {dt:?}, {} reads, max {max}",
        counting.calls.load(Ordering::Relaxed)
    );

    // The same file followed by 3 MiB of junk measures identically.
    let mut rng = Rng::new(5);
    let concat: SharedSource = Arc::new(Concat {
        head: file.clone(),
        tail: rng.bytes(3 * MIB),
    });
    let counting2 = Arc::new(Counting::new(concat));
    let src2: SharedSource = counting2.clone();
    let m2 = PngFormat.measure(&src2, 0, u64::MAX).unwrap().unwrap();
    assert_eq!(m2.len, size);
    assert!(counting2.max_read.load(Ordering::Relaxed) <= MIB);
    assert!(counting2.total.load(Ordering::Relaxed) <= size + 2 * MIB as u64);

    // A limit stops the walk early and reads no more than the limit.
    let counting3 = Arc::new(Counting::new(file.clone()));
    let src3: SharedSource = counting3.clone();
    let limit = 10 * MIB as u64;
    let m3 = PngFormat.measure(&src3, 0, limit).unwrap().unwrap();
    assert_eq!(m3.status, ExtentStatus::Capped);
    assert!(m3.len <= limit);
    assert!(counting3.total.load(Ordering::Relaxed) <= limit + MIB as u64);

    // validate(): Valid, streaming (two passes: CRC walk + inflate).
    let counting4 = Arc::new(Counting::new(file.clone()));
    let src4: SharedSource = counting4.clone();
    let t = Instant::now();
    let v = PngFormat.validate(&src4).unwrap();
    eprintln!("validate: {:?}", t.elapsed());
    assert_eq!(v.status, ValidationStatus::Valid, "{:?}", v.problems);
    assert!(counting4.max_read.load(Ordering::Relaxed) <= MIB);
    assert!(counting4.total.load(Ordering::Relaxed) <= 2 * size + 2 * MIB as u64);

    // Spot-check the pixels through the lenient decoder (statistics only: no
    // pixel buffer for a 50 MB image).
    let walk = chunks::walk(&*file, 0, u64::MAX).unwrap();
    assert!(!walk.idat_ranges.is_empty());
    let img = lenient::analyze(&*file, &walk).unwrap();
    assert_eq!(img.rows_recovered, h);
    assert!(img.pixels.is_empty());
    let _ = first_rows;
}

#[test]
fn truncated_big_png_is_repaired_without_loading_it() {
    // Cut a multi-megabyte image in half; repair streams rows through the
    // encoder and the result decodes with the original dimensions.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mid.png");
    let (w, h) = (1600u32, 1200u32);
    let first = write_big_png(&path, w, h);
    let bytes = std::fs::read(&path).unwrap();
    let cut = bytes.len() / 2;
    let src: SharedSource = Arc::new(MemSource2(bytes[..cut].to_vec()));
    let counting = Arc::new(Counting::new(src));
    let shared: SharedSource = counting.clone();
    let mut out = Vec::new();
    let o = PngFormat
        .repair(&shared, &mut out, &RepairContext::default())
        .unwrap();
    assert_eq!(o.result, RepairResult::PartiallyRepaired);
    assert!(counting.max_read.load(Ordering::Relaxed) <= MIB);
    let d = png_decode(&out).unwrap();
    assert_eq!((d.width, d.height), (w, h));
    assert_eq!(&d.data[..first.len()], &first[..]);
}

struct MemSource2(Vec<u8>);

impl Source for MemSource2 {
    fn size(&self) -> u64 {
        self.0.len() as u64
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, ReadError> {
        if offset >= self.0.len() as u64 {
            return Ok(0);
        }
        let o = offset as usize;
        let n = (self.0.len() - o).min(buf.len());
        buf[..n].copy_from_slice(&self.0[o..o + n]);
        Ok(n)
    }

    fn describe(&self) -> String {
        "mem".into()
    }
}
