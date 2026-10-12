//! The carver with the real format crates (JPEG, PNG, stills and RAW, ISO
//! BMFF, video and audio) over the raw carving images built by
//! `diskrescue-fixtures` (`raw::*`, each with its ground truth).
//!
//! Needs `ffmpeg` (the fixtures' video samples); every test prints
//! `skipped: ffmpeg missing` and passes without it. Slow parts run under a
//! deadline so a hang fails instead of blocking.
//!
//! Known format-crate limitations the expectations below allow for (each is
//! named where it is allowed):
//!
//! * `KNOWN_OVERCLAIM`: the HEIF and MP4 measures report a file whose tail
//!   is missing (cut at 60 % and followed by other data) as `Complete` at
//!   confidence 90, trusting `iloc` / box sizes without checking that the
//!   data they point to is there.
//! * `KNOWN_LOST`: a moov-less MP4 (confidence 25, `Truncated`) whose
//!   header lies inside such an over-long, confident file is suppressed: a
//!   weak candidate is no evidence against a strong parent.
//! * `video.ts` in the holes image: the MPEG-TS measure stops at a known
//!   unreadable range instead of resynchronising after it, so the stream
//!   comes back in two pieces and the packets between the hole and the next
//!   512-byte-aligned packet are in neither.

mod common;

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use common::check_invariants;
use diskrescue_carve::{CarveOptions, CarvedFile, Carver, ScanSummary};
use diskrescue_core::format::{
    Confidence, ExtentStatus, FileFormat, FormatRegistry, Measured, Signature, Validation,
};
use diskrescue_core::media::MediaCategory;
use diskrescue_core::source::{FileSource, MemSource};
use diskrescue_core::{Range, RangeSet, Result, SharedSource};
use diskrescue_fixtures::raw::{self, RawImage, RawObject};
use diskrescue_fixtures::tools;

const KNOWN_OVERCLAIM: &[&str] = &["image.heic", "video_faststart.mp4"];
const KNOWN_LOST: &[&str] = &["video_moovlast.mp4", "video.mov"];

/// Bytes a measure may stop short of a cut point that falls inside a
/// marker or chunk header (a dangling 0xFF holds no data).
const CUT_MARKER_SLACK: u64 = 16;

/// Formats that end with a marker (EOI, IEND, trailer): a cut copy must
/// never come back as complete. Size-field formats (BMP, RIFF, TIFF, ISO
/// BMFF, ...) cannot tell and are not held to this here.
const END_MARKER_KINDS: &[&str] = &["jpeg", "png", "gif"];

/// Every real format, as the recover pipeline registers them.
fn registry() -> FormatRegistry {
    let mut reg = FormatRegistry::new();
    reg.register(Arc::new(diskrescue_jpeg::JpegFormat));
    reg.register(Arc::new(diskrescue_png::PngFormat));
    for f in diskrescue_stills::formats()
        .into_iter()
        .chain(diskrescue_isobmff::formats())
        .chain(diskrescue_video::formats())
    {
        reg.register(f);
    }
    reg
}

/// Run `f` on another thread; fail if it takes longer than `secs`.
fn with_deadline<T: Send + 'static>(secs: u64, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    match rx.recv_timeout(Duration::from_secs(secs)) {
        Ok(v) => v,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            panic!("did not finish within {secs} s (hang?)")
        }
        Err(e) => panic!("worker thread failed: {e}"),
    }
}

struct Fixtures {
    _dir: tempfile::TempDir,
    images: Vec<RawImage>,
}

/// All raw images, built once per test binary (seed 1).
fn fixtures() -> Option<&'static Fixtures> {
    static F: OnceLock<Option<Fixtures>> = OnceLock::new();
    F.get_or_init(|| {
        if !tools::have("ffmpeg") {
            return None;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let images = raw::all(dir.path(), 1).expect("raw fixtures");
        Some(Fixtures { _dir: dir, images })
    })
    .as_ref()
}

fn image(name: &str) -> Option<&'static RawImage> {
    let Some(f) = fixtures() else {
        eprintln!("skipped: ffmpeg missing");
        return None;
    };
    let file = format!("{name}.img");
    Some(
        f.images
            .iter()
            .find(|i| i.path.file_name().is_some_and(|n| n == file.as_str()))
            .unwrap_or_else(|| panic!("no {file} among the raw fixtures")),
    )
}

/// Bad ranges of a ddrescue mapfile (every status but `+`).
fn parse_mapfile(path: &Path) -> RangeSet {
    let text = std::fs::read_to_string(path).expect("mapfile");
    let mut bad = RangeSet::new();
    let hex = |s: &str| u64::from_str_radix(s.trim_start_matches("0x"), 16).expect("hex");
    for line in text
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .skip(1)
    {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() >= 3 && f[2] != "+" {
            bad.insert(Range::with_len(hex(f[0]), hex(f[1])));
        }
    }
    bad
}

fn source(img: &RawImage) -> SharedSource {
    let mut s = FileSource::open(&img.path).expect("image");
    if let Some(m) = &img.mapfile {
        s = s.with_unreadable(parse_mapfile(m));
    }
    Arc::new(s)
}

fn options(alignment: u64) -> CarveOptions {
    let mut o = CarveOptions::new(registry());
    o.alignment = alignment;
    o.chunk_size = 8 << 20;
    o
}

fn scan(o: CarveOptions, src: &SharedSource) -> (Vec<CarvedFile>, ScanSummary) {
    let carver = Carver::new(o).expect("carver");
    let mut files = Vec::new();
    let sum = carver
        .scan(src, &mut |_| {}, &mut |f| {
            files.push(f);
            true
        })
        .expect("scan");
    check_invariants(&files, src.size());
    (files, sum)
}

fn carve_image(img: &RawImage, alignment: u64) -> (Vec<CarvedFile>, ScanSummary, SharedSource) {
    let src = source(img);
    let s2 = src.clone();
    let (files, sum) = with_deadline(300, move || scan(options(alignment), &s2));
    (files, sum, src)
}

fn start(o: &RawObject) -> u64 {
    o.extents[0].0
}

fn at(files: &[CarvedFile], start: u64) -> Option<&CarvedFile> {
    files.iter().find(|f| f.start == start)
}

fn hash(src: &SharedSource, f: &CarvedFile) -> String {
    Carver::hash(src, f).expect("hash")
}

/// The carved file at `o`'s start, which must have exactly `o`'s bytes.
fn assert_found_by_hash(files: &[CarvedFile], src: &SharedSource, o: &RawObject) {
    let f = at(files, start(o)).unwrap_or_else(|| panic!("{} not carved", o.name));
    assert_eq!(f.len, o.extents[0].1, "{}: length", o.name);
    assert_eq!(hash(src, f), o.blake3, "{}: content", o.name);
}

/// Every carved file must be a truth object or lie inside one.
fn assert_no_stray_files(files: &[CarvedFile], objects: &[RawObject]) {
    for f in files {
        let inside = objects.iter().any(|o| {
            o.extents
                .iter()
                .any(|&(s, l)| f.start >= s && f.start < s + l)
        });
        assert!(
            inside,
            "stray file {} at {} len {}",
            f.format, f.start, f.len
        );
    }
}

#[test]
fn contiguous_files_are_carved_exactly() {
    for name in ["contig", "fake_capacity"] {
        let Some(img) = image(name) else {
            return;
        };
        let (files, sum, src) = carve_image(img, 512);
        assert_eq!(files.len(), img.objects.len(), "{name}: {files:#?}");
        for o in &img.objects {
            assert_found_by_hash(&files, &src, o);
        }
        for f in &files {
            assert!(f.parent.is_none());
            assert!(f.confidence >= Confidence::LOW, "{f:?}");
            assert!(f.unreadable.is_empty());
        }
        assert_eq!(sum.files, img.objects.len() as u64);
        assert_eq!(sum.unreadable_skipped, 0);
    }
}

#[test]
fn unaligned_files_need_a_deep_scan() {
    let Some(img) = image("unaligned") else {
        return;
    };
    // Default alignment: only objects that happen to start on a sector are
    // found; anything else carved is a piece of a truth object (an MPEG-TS
    // stream picked up at a packet that happens to be sector-aligned).
    let (files, _, src) = carve_image(img, 512);
    for o in &img.objects {
        if start(o) % 512 == 0 {
            assert_found_by_hash(&files, &src, o);
        } else {
            assert!(
                at(&files, start(o)).is_none(),
                "{} at an unaligned start",
                o.name
            );
        }
    }
    assert_no_stray_files(&files, &img.objects);
    // Deep scan: everything, except weak signatures (BMP's `BM`, the
    // MPEG-TS sync byte), which stay on the sector grid (carving spec 3.1).
    let (files, _, src) = carve_image(img, 1);
    for o in &img.objects {
        let weak = matches!(o.kind.as_str(), "bmp" | "ts");
        if weak && start(o) % 512 != 0 {
            assert!(at(&files, start(o)).is_none(), "{}", o.name);
        } else {
            assert_found_by_hash(&files, &src, o);
        }
    }
    assert_no_stray_files(&files, &img.objects);
}

#[test]
fn embedded_pictures_are_not_separate_files() {
    let Some(img) = image("nested") else {
        return;
    };
    let containers: Vec<&RawObject> = img.objects.iter().filter(|o| o.status == "valid").collect();
    let inner: Vec<&RawObject> = img
        .objects
        .iter()
        .filter(|o| o.status == "nested")
        .collect();
    // CR2 previews and thumbnail, MJPEG frames, MP3 and MP4 cover art.
    assert!(inner.len() >= 4);
    for alignment in [512u64, 1] {
        let (files, _, src) = carve_image(img, alignment);
        assert_eq!(
            files.len(),
            containers.len(),
            "alignment {alignment}: {files:#?}"
        );
        for o in &containers {
            assert_found_by_hash(&files, &src, o);
        }
        for o in &inner {
            assert!(
                at(&files, start(o)).is_none(),
                "{} carved separately at alignment {alignment}",
                o.name
            );
        }
    }
    // On request, the pictures a container reports come out as children.
    let src = source(img);
    let mut o = options(512);
    o.promote_nested = true;
    let (files, _) = scan(o, &src);
    let kids: Vec<&CarvedFile> = files.iter().filter(|f| f.parent.is_some()).collect();
    for name in ["raw.cr2#jpeg0", "cover.mp3#jpeg0", "covr.mp4#jpeg0"] {
        let o = inner.iter().find(|o| o.name == name).expect(name);
        let k = kids
            .iter()
            .find(|k| k.start == start(o))
            .unwrap_or_else(|| panic!("{name} not promoted: {files:#?}"));
        assert_eq!(hash(&src, k), o.blake3, "{name}");
        assert_eq!(k.format, "jpeg");
    }
}

#[test]
fn motion_photo_is_one_file() {
    let Some(img) = image("motion_photo") else {
        return;
    };
    let (files, _, src) = carve_image(img, 512);
    assert_eq!(files.len(), 1, "{files:#?}");
    assert_found_by_hash(&files, &src, &img.objects[0]);
    assert_eq!(files[0].format, "jpeg");
    assert_eq!(files[0].status, ExtentStatus::Complete);
}

#[test]
fn random_data_yields_nothing() {
    let Some(img) = image("weak_sigs") else {
        return;
    };
    for alignment in [512u64, 1] {
        let (files, sum, _) = carve_image(img, alignment);
        assert!(files.is_empty(), "alignment {alignment}: {files:#?}");
        assert_eq!(sum.files, 0);
    }
}

#[test]
fn truncated_files_are_never_lost() {
    let Some(img) = image("truncated") else {
        return;
    };
    let (files, _, src) = carve_image(img, 512);
    let mut lost = Vec::new();
    for o in &img.objects {
        let (s, prefix) = o.extents[0];
        let Some(f) = at(&files, s) else {
            lost.push(o.name.as_str());
            continue;
        };
        // The stored prefix is in the carved file (whatever the format
        // could tell about where the data really ends). A JPEG cut inside a
        // marker loses the dangling 0xFF, which holds no data.
        assert!(
            f.len + CUT_MARKER_SLACK >= prefix,
            "{}: {} < {prefix}",
            o.name,
            f.len
        );
        if f.len >= prefix {
            let mut head = f.clone();
            head.len = prefix;
            assert_eq!(hash(&src, &head), o.blake3, "{}", o.name);
        }
        // A cut file must not be reported as an intact, confident one.
        if f.status == ExtentStatus::Complete && f.confidence.value() >= 80 {
            assert!(
                KNOWN_OVERCLAIM.contains(&o.name.as_str()),
                "{} reported complete at {}: {:?}",
                o.name,
                f.confidence.value(),
                f.notes
            );
        }
    }
    assert!(
        lost.iter().all(|n| KNOWN_LOST.contains(n)),
        "lost truncated files: {lost:?}"
    );
    // The overlap rules recover what an over-long length field used to
    // swallow: the TIFF after the WebP and the HEIC inside the CR2's claim.
    for name in ["image.tif", "image.heic", "raw.cr2", "image.webp"] {
        let o = img.objects.iter().find(|o| o.name == name).expect(name);
        assert!(at(&files, start(o)).is_some(), "{name} lost");
    }
}

#[test]
fn unreadable_ranges_are_reported_and_files_kept() {
    let Some(img) = image("holes") else {
        return;
    };
    assert!(img.objects.iter().any(|o| o.unreadable_bytes > 0));
    let (files, sum, src) = carve_image(img, 512);
    let bad = parse_mapfile(img.mapfile.as_ref().expect("mapfile"));
    assert_eq!(sum.unreadable_skipped, bad.total_len());
    for o in &img.objects {
        let (s, len) = o.extents[0];
        let f = at(&files, s).unwrap_or_else(|| panic!("{} lost", o.name));
        if o.unreadable_bytes == 0 {
            assert_found_by_hash(&files, &src, o);
            continue;
        }
        assert!(
            f.unreadable.total_len() > 0,
            "{}: hole not reported",
            o.name
        );
        let mut out = Vec::new();
        let rep = Carver::extract(&src, f, &mut out).expect("extract");
        assert_eq!(rep.unreadable, f.unreadable, "{}", o.name);
        if o.kind == "ts" {
            // Split at the hole by the MPEG-TS measure (see the module docs).
            assert!(f.len < len);
            continue;
        }
        assert_eq!(f.len, len, "{}", o.name);
        assert_eq!(f.unreadable.total_len(), o.unreadable_bytes, "{}", o.name);
    }
    assert_no_stray_files(&files, &img.objects);
}

#[test]
fn fragmented_jpegs_keep_their_first_fragment() {
    let Some(img) = image("bifrag") else {
        return;
    };
    let (files, _, src) = carve_image(img, 512);
    for o in &img.objects {
        let (s, first) = o.extents[0];
        match o.status.as_str() {
            "valid" => {
                let f = at(&files, s).unwrap_or_else(|| panic!("{} lost", o.name));
                assert_eq!(hash(&src, f), o.blake3, "{}", o.name);
            }
            _ => {
                let f = at(&files, s).unwrap_or_else(|| panic!("{} lost", o.name));
                assert!(f.len >= first, "{}: {} < {first}", o.name, f.len);
                assert_ne!(f.status, ExtentStatus::Complete, "{}", o.name);
            }
        }
    }
}

#[test]
fn output_does_not_depend_on_threads_or_chunks() {
    let Some(f) = fixtures() else {
        eprintln!("skipped: ffmpeg missing");
        return;
    };
    for img in &f.images {
        let src = source(img);
        let mut outputs = Vec::new();
        for (workers, chunk) in [(1usize, 8usize << 20), (8, 64 << 10), (3, 1 << 20)] {
            let mut o = options(512);
            o.workers = workers;
            o.chunk_size = chunk;
            let s2 = src.clone();
            outputs.push(with_deadline(300, move || scan(o, &s2).0));
        }
        assert_eq!(outputs[0], outputs[1], "{}", img.path.display());
        assert_eq!(outputs[0], outputs[2], "{}", img.path.display());
    }
}

/// Counts `measure` calls of the wrapped format.
struct Counting(Arc<dyn FileFormat>, AtomicU64);

impl FileFormat for Counting {
    fn id(&self) -> &'static str {
        self.0.id()
    }
    fn name(&self) -> &'static str {
        self.0.name()
    }
    fn extensions(&self) -> &'static [&'static str] {
        self.0.extensions()
    }
    fn category(&self) -> MediaCategory {
        self.0.category()
    }
    fn signatures(&self) -> &'static [Signature] {
        self.0.signatures()
    }
    fn min_size(&self) -> u64 {
        self.0.min_size()
    }
    fn max_size(&self) -> u64 {
        self.0.max_size()
    }
    fn priority(&self) -> u8 {
        self.0.priority()
    }
    fn probe(&self, head: &[u8]) -> bool {
        self.0.probe(head)
    }
    fn may_contain(&self, inner: &str) -> bool {
        self.0.may_contain(inner)
    }
    fn measure(&self, src: &SharedSource, start: u64, limit: u64) -> Result<Option<Measured>> {
        self.1.fetch_add(1, Ordering::Relaxed);
        self.0.measure(src, start, limit)
    }
    fn validate(&self, src: &SharedSource) -> Result<Validation> {
        self.0.validate(src)
    }
}

fn ffmpeg(args: &[&str]) -> bool {
    tools::run("ffmpeg", args).is_ok()
}

#[test]
fn long_mpeg_ts_and_mp3_runs_are_measured_a_few_times() {
    if !tools::have("ffmpeg") {
        eprintln!("skipped: ffmpeg missing");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let ts = dir.path().join("long.ts");
    let mp3 = dir.path().join("long.mp3");
    let ts_s = ts.to_string_lossy().into_owned();
    let mp3_s = mp3.to_string_lossy().into_owned();
    let quiet = ["-hide_banner", "-loglevel", "error", "-nostdin", "-y"];
    let mut a: Vec<&str> = quiet.to_vec();
    a.extend([
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=320x240:rate=25",
        "-t",
        "40",
        "-c:v",
        "mpeg2video",
        "-b:v",
        "4M",
        "-f",
        "mpegts",
        &ts_s,
    ]);
    assert!(ffmpeg(&a), "ffmpeg mpegts");
    let mut a: Vec<&str> = quiet.to_vec();
    a.extend([
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:duration=900",
        "-c:a",
        "libmp3lame",
        "-b:a",
        "128k",
        "-id3v2_version",
        "0",
        "-write_xing",
        "0",
        &mp3_s,
    ]);
    if !ffmpeg(&a) {
        eprintln!("skipped: ffmpeg without libmp3lame");
        return;
    }
    for (path, fmt) in [(&ts, "mts"), (&mp3, "mp3")] {
        let data = std::fs::read(path).unwrap();
        assert!(data.len() > 8 << 20, "{fmt}: {} bytes", data.len());
        let mut img = vec![0u8; 8192];
        img.extend_from_slice(&data);
        img.extend(common::random_bytes(1 << 20, 3));
        let src: SharedSource = Arc::new(MemSource::new(img));
        for alignment in [512u64, 1] {
            let counters: Vec<Arc<Counting>> = registry()
                .all()
                .iter()
                .map(|f| Arc::new(Counting(f.clone(), AtomicU64::new(0))))
                .collect();
            let mut reg = FormatRegistry::new();
            for c in &counters {
                reg.register(c.clone());
            }
            let mut o = CarveOptions::new(reg);
            o.alignment = alignment;
            let s2 = src.clone();
            let t = Instant::now();
            let (files, _) = with_deadline(120, move || scan(o, &s2));
            let secs = t.elapsed().as_secs_f64();
            assert_eq!(files.len(), 1, "{fmt} alignment {alignment}: {files:#?}");
            assert_eq!((files[0].start, files[0].len), (8192, data.len() as u64));
            assert_eq!(files[0].format, fmt);
            assert_eq!(
                hash(&src, &files[0]),
                blake3::hash(&data).to_hex().to_string()
            );
            let n = counters
                .iter()
                .find(|c| c.0.id() == fmt)
                .unwrap()
                .1
                .load(Ordering::Relaxed);
            assert!(
                n <= 8,
                "{fmt} measured {n} times at alignment {alignment} (quadratic walk?)"
            );
            assert!(secs < 60.0, "{fmt}: {secs:.1} s");
        }
    }
}

#[test]
fn power_cut_video_with_a_bad_sector_is_kept() {
    let Some(img) = image("truncated") else {
        return;
    };
    // The moov-less MP4 of the truncated image, alone, with one unreadable
    // sector in its mdat: the MP4 measure scores it 25 - 20 = 5.
    let o = img
        .objects
        .iter()
        .find(|o| o.name == "video_moovlast.mp4")
        .unwrap();
    let (s, l) = o.extents[0];
    let data = std::fs::read(&img.path).unwrap();
    let mut buf = vec![0u8; 8192];
    buf.extend_from_slice(&data[s as usize..(s + l) as usize]);
    buf.resize(buf.len() + 65_536, 0);
    let bad = RangeSet::from_range(Range::with_len(8192 + 16_384, 512));
    let src: SharedSource = Arc::new(MemSource::new(buf).with_unreadable(bad));
    let (files, _) = scan(options(512), &src);
    assert_eq!(files.len(), 1, "{files:#?}");
    assert_eq!(files[0].format, "mp4");
    assert_eq!(files[0].start, 8192);
    assert!(files[0].len >= l);
    assert_eq!(files[0].unreadable.total_len(), 512);
}

#[test]
fn real_signatures_over_random_data_do_not_break_anything() {
    if fixtures().is_none() {
        eprintln!("skipped: ffmpeg missing");
        return;
    }
    // Every real magic at aligned and unaligned offsets of random data, and
    // every sample truncated at many lengths: no panic, no hang, the output
    // contract holds and does not depend on thread timing.
    let magics: &[&[u8]] = &[
        b"\xFF\xD8\xFF\xE0",
        b"\xFF\xD8\xFF\xDB",
        b"\x89PNG\r\n\x1a\n",
        b"GIF89a",
        b"BM",
        b"RIFF\0\0\0\0WEBP",
        b"RIFF\0\0\0\0AVI ",
        b"RIFF\0\0\0\0WAVE",
        b"II*\0",
        b"MM\0*",
        b"II*\0\x10\0\0\0CR\x02\0",
        b"IIRO",
        b"IIU\0",
        b"FUJIFILMCCD-RAW ",
        b"8BPS\0\x01",
        b"\0\0\0\x18ftypisom",
        b"\0\0\0\x18ftypheic",
        b"\0\0\0\x18ftypcrx ",
        b"\x1A\x45\xDF\xA3",
        b"ID3\x03\0\0\0\0\0\0",
        b"\xFF\xFB\x90\0",
        b"fLaC\0",
        b"\0\0\x01\xBA\x44",
        b"G",
    ];
    for seed in 0..4u64 {
        let mut img = common::random_bytes(4 << 20, 900 + seed);
        let mut rng = common::Rng::new(seed);
        for i in 0..4000usize {
            let m = magics[i % magics.len()];
            let mut at = rng.below((4 << 20) - 64) as usize;
            if i % 2 == 0 {
                at = at / 512 * 512;
            }
            img[at..at + m.len()].copy_from_slice(m);
        }
        let src: SharedSource = Arc::new(MemSource::new(img));
        let mut outs = Vec::new();
        for (alignment, workers) in [(512u64, 1usize), (512, 8), (1, 4)] {
            let mut o = options(alignment);
            o.workers = workers;
            o.chunk_size = 1 << 20;
            let s2 = src.clone();
            outs.push(with_deadline(300, move || scan(o, &s2).0));
        }
        assert_eq!(outs[0], outs[1], "seed {seed}");
    }
    let contig = image("contig").unwrap();
    let data = std::fs::read(&contig.path).unwrap();
    for o in &contig.objects {
        let (s, l) = o.extents[0];
        let body = &data[s as usize..(s + l) as usize];
        let step = (l / 23).max(1) as usize;
        let mut cuts: Vec<usize> = (1..body.len()).step_by(step).collect();
        cuts.extend([1, 2, 3, 4, 8, 12, 16, 100, 511, 512, 513]);
        let cuts: BTreeSet<usize> = cuts.into_iter().filter(|&c| c < body.len()).collect();
        for cut in cuts {
            let mut img = vec![0u8; 4096];
            img.extend_from_slice(&body[..cut]);
            img.extend(common::random_bytes(8192, cut as u64));
            let src: SharedSource = Arc::new(MemSource::new(img));
            let name = o.name.clone();
            let (files, _) = with_deadline(60, move || scan(options(512), &src));
            if !END_MARKER_KINDS.contains(&o.kind.as_str()) {
                continue;
            }
            if let Some(f) = files.iter().find(|f| f.start == 4096) {
                assert!(
                    f.status != ExtentStatus::Complete,
                    "{name} cut at {cut} carved as a complete file: {f:?}"
                );
            }
        }
    }
}

#[test]
#[ignore = "benchmark: run in release mode with --ignored --nocapture"]
fn scan_1gib_random_all_real_formats_over_1gbps() {
    let size = 1usize << 30;
    let src: SharedSource = Arc::new(MemSource::new(common::random_bytes(size, 4321)));
    let rate = |alignment: u64| {
        let mut o = CarveOptions::new(registry());
        o.alignment = alignment;
        let carver = Carver::new(o).unwrap();
        let t = Instant::now();
        let sum = carver.scan(&src, &mut |_| {}, &mut |_| true).unwrap();
        assert_eq!(sum.files, 0);
        (size as f64 / t.elapsed().as_secs_f64(), sum.candidates)
    };
    let _ = rate(512);
    let mut best = 0f64;
    for _ in 0..3 {
        let (r, c) = rate(512);
        println!(
            "1 GiB random, {} real formats: {:.0} MB/s ({c} candidates)",
            registry().len(),
            r / 1e6
        );
        best = best.max(r);
    }
    let (deep, c) = rate(1);
    println!(
        "deep scan (alignment 1): {:.0} MB/s ({c} candidates)",
        deep / 1e6
    );
    if cfg!(debug_assertions) {
        eprintln!("debug build: rate not asserted");
    } else {
        assert!(best > 1e9, "scanner rate {:.2} GB/s", best / 1e9);
        assert!(deep > 1e9, "deep scan rate {:.2} GB/s", deep / 1e9);
    }
}
