//! Raw carving images: media files laid out in a flat byte image with no
//! filesystem, plus the ground truth a carver is expected to reproduce.
//!
//! Each builder writes `<dir>/<name>.img` and `<dir>/<name>.truth.json` and
//! returns the same data as a [`RawImage`]:
//!
//! | Image | What it contains | Truth |
//! |---|---|---|
//! | [`contig`] | one of every sample at 4 KiB alignment, separated by zero / 0xFF / random / text fills | all `valid` |
//! | [`unaligned`] | the same at random byte offsets | all `valid`, `aligned` tells which are found at the default alignment |
//! | [`nested`] | CR2-like with JPEG previews, MJPEG AVI, MP3 with a JPEG cover, MP4 with a cover | containers `valid`, inner JPEGs `nested` |
//! | [`bifrag`] | one JPEG split at a cluster boundary with gaps of 1..64 clusters filled with zeros, random data, another JPEG's middle, or a complete small JPEG; plus an unaligned split | `fragment` (two extents) / `truncated` |
//! | [`truncated`] | every sample cut at 60 % and followed by a fill | `truncated` |
//! | [`holes`] | [`contig`] with 4 KiB ranges zeroed and a ddrescue mapfile marking them `-` | `valid` with `unreadable_bytes` |
//! | [`weak_sigs`] | random data | no objects |
//! | [`fake_capacity`] | [`contig`] twice | second copy `duplicate` |
//! | [`motion_photo`] | JPEG with Google MicroVideo XMP followed by an MP4 | one `motion_photo` |
//!
//! Needs `ffmpeg` for the video samples (the builders return
//! `Err(Missing)` without it); progressive and restart-marker JPEG samples
//! are added when Pillow is available. Everything is deterministic in the
//! seed.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::damage;
use crate::error::{FixtureError, Result};
use crate::media::{
    self, AudioCodec, Container, ExifOpts, JpegOpts, PngColor, PngOpts, StillOpts, TiffOpts,
    VideoCodec, VideoOpts,
};
use crate::rng::Rng;
use crate::tools;

/// A file the builders can place.
#[derive(Debug, Clone)]
pub struct Sample {
    /// Short name (`photo_exif.jpg`).
    pub name: String,
    /// Format tag (`jpeg`, `png`, `mp4`, ...).
    pub kind: &'static str,
    /// Content.
    pub data: Vec<u8>,
}

/// One expected object in a raw image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawObject {
    /// Sample name.
    pub name: String,
    /// Format tag.
    pub kind: String,
    /// `valid`, `truncated`, `nested`, `fragment`, `duplicate` or
    /// `motion_photo`.
    pub status: String,
    /// `(offset, length)` ranges of the object's bytes (two for a
    /// fragmented file, in file order).
    pub extents: Vec<(u64, u64)>,
    /// BLAKE3 of the complete, undamaged file (`valid`, `fragment`,
    /// `duplicate`, `motion_photo`) or of the stored prefix (`truncated`).
    pub blake3: String,
    /// True when the (first) extent starts at a multiple of 4096.
    pub aligned: bool,
    /// Bytes inside the object that were made unreadable ([`holes`]).
    pub unreadable_bytes: u64,
    /// Index of the object this one duplicates.
    pub duplicate_of: Option<usize>,
    /// Remark.
    pub note: String,
}

/// A raw image and its truth.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawImage {
    /// Image path.
    pub path: PathBuf,
    /// Image size in bytes.
    pub size: u64,
    /// Expected objects in file order.
    pub objects: Vec<RawObject>,
    /// ddrescue mapfile describing unreadable ranges ([`holes`] only).
    pub mapfile: Option<PathBuf>,
    /// Samples that could not be built on this machine.
    pub skipped: Vec<String>,
}

// ---------------------------------------------------------------- samples

/// Build the sample set. Requires ffmpeg for the video samples.
pub fn samples() -> Result<(Vec<Sample>, Vec<String>)> {
    tools::require("ffmpeg")?;
    let mut out = Vec::new();
    let mut skipped = Vec::new();
    let mut add = |name: &str, kind: &'static str, data: Vec<u8>| {
        out.push(Sample {
            name: name.to_string(),
            kind,
            data,
        });
    };
    add(
        "photo_exif.jpg",
        "jpeg",
        media::jpeg(&JpegOpts {
            width: 480,
            height: 320,
            quality: 85,
            seed: 1,
            exif: Some(ExifOpts::default()),
            ..Default::default()
        })?,
    );
    add(
        "photo_small.jpg",
        "jpeg",
        media::baseline_jpeg(200, 150, 70, 2),
    );
    match media::jpeg(&JpegOpts {
        width: 400,
        height: 300,
        quality: 80,
        seed: 3,
        progressive: true,
        ..Default::default()
    }) {
        Ok(d) => add("photo_prog.jpg", "jpeg", d),
        Err(e) if e.is_missing() => skipped.push("photo_prog.jpg (Pillow)".into()),
        Err(e) => return Err(e),
    }
    match media::jpeg(&JpegOpts {
        width: 400,
        height: 300,
        quality: 80,
        seed: 4,
        restart_interval: Some(4),
        ..Default::default()
    }) {
        Ok(d) => add("photo_rst.jpg", "jpeg", d),
        Err(e) if e.is_missing() => skipped.push("photo_rst.jpg (Pillow)".into()),
        Err(e) => return Err(e),
    }
    add(
        "image.png",
        "png",
        media::png(&PngOpts {
            width: 320,
            height: 240,
            seed: 5,
            ..Default::default()
        }),
    );
    add(
        "image_interlaced.png",
        "png",
        media::png(&PngOpts {
            width: 200,
            height: 160,
            seed: 6,
            interlaced: true,
            color: PngColor::Rgba,
            ..Default::default()
        }),
    );
    let still = StillOpts {
        width: 160,
        height: 120,
        seed: 7,
    };
    add("image.gif", "gif", media::gif(&still));
    add("image.bmp", "bmp", media::bmp(&still));
    add("image.webp", "webp", media::webp(&still));
    add(
        "image.tif",
        "tiff",
        media::tiff(&TiffOpts {
            width: 160,
            height: 120,
            seed: 8,
            ..Default::default()
        }),
    );
    add(
        "raw.cr2",
        "cr2",
        media::cr2_like(&TiffOpts {
            width: 256,
            height: 192,
            seed: 9,
            ..Default::default()
        }),
    );
    match media::heic(&Default::default()) {
        Ok(d) => add("image.heic", "heic", d),
        Err(e) if e.is_missing() => skipped.push("image.heic (libx265)".into()),
        Err(e) => return Err(e),
    }
    let v = |c: Container, vc: VideoCodec, a: AudioCodec, fast: bool, seed: u64| VideoOpts {
        container: c,
        video: vc,
        audio: a,
        seconds: 1.5,
        fps: 15,
        width: 240,
        height: 160,
        faststart: fast,
        seed,
        ..Default::default()
    };
    let videos: [(&str, &'static str, VideoOpts); 9] = [
        (
            "video_moovlast.mp4",
            "mp4",
            v(Container::Mp4, VideoCodec::H264, AudioCodec::Aac, false, 11),
        ),
        (
            "video_faststart.mp4",
            "mp4",
            v(Container::Mp4, VideoCodec::H264, AudioCodec::Aac, true, 12),
        ),
        (
            "video.mov",
            "mov",
            v(Container::Mov, VideoCodec::H264, AudioCodec::Aac, false, 13),
        ),
        (
            "video.3gp",
            "3gp",
            v(
                Container::ThreeGp,
                VideoCodec::H264,
                AudioCodec::Aac,
                false,
                14,
            ),
        ),
        (
            "video.avi",
            "avi",
            v(
                Container::Avi,
                VideoCodec::Mpeg4,
                AudioCodec::Pcm,
                false,
                15,
            ),
        ),
        (
            "video.mkv",
            "mkv",
            v(Container::Mkv, VideoCodec::H264, AudioCodec::Aac, false, 16),
        ),
        (
            "video.webm",
            "webm",
            v(
                Container::WebM,
                VideoCodec::Vp8,
                AudioCodec::None,
                false,
                17,
            ),
        ),
        (
            "video.ts",
            "ts",
            v(
                Container::MpegTs,
                VideoCodec::H264,
                AudioCodec::Aac,
                false,
                18,
            ),
        ),
        (
            "video.mpg",
            "mpg",
            v(
                Container::MpegPs,
                VideoCodec::Mpeg2,
                AudioCodec::Mp3,
                false,
                19,
            ),
        ),
    ];
    for (name, kind, opts) in videos {
        match media::video(&opts) {
            Ok(d) => out.push(Sample {
                name: name.into(),
                kind,
                data: d,
            }),
            Err(e) if e.is_missing() => skipped.push(format!("{name} ({e})")),
            Err(e) => return Err(e),
        }
    }
    Ok((out, skipped))
}

// ---------------------------------------------------------------- layout

#[derive(Debug, Clone, Copy)]
enum Fill {
    Zeros,
    Ones,
    Random,
    Text,
}

const FILLS: [Fill; 4] = [Fill::Zeros, Fill::Ones, Fill::Random, Fill::Text];

struct Layout {
    buf: Vec<u8>,
    objects: Vec<RawObject>,
    rng: Rng,
}

impl Layout {
    fn new(seed: u64) -> Layout {
        Layout {
            buf: Vec::new(),
            objects: Vec::new(),
            rng: Rng::new(seed ^ 0x5A5A_0000),
        }
    }

    fn fill(&mut self, kind: Fill, len: usize) {
        match kind {
            Fill::Zeros => self.buf.resize(self.buf.len() + len, 0),
            Fill::Ones => self.buf.resize(self.buf.len() + len, 0xFF),
            Fill::Random => {
                let v = self.rng.bytes(len);
                self.buf.extend_from_slice(&v);
            }
            Fill::Text => {
                let line = b"The quick brown fox jumps over the lazy dog. 0123456789\n";
                for i in 0..len {
                    self.buf.push(line[i % line.len()]);
                }
            }
        }
    }

    fn align(&mut self, a: usize, kind: Fill) {
        let pad = (a - self.buf.len() % a) % a;
        self.fill(kind, pad);
    }

    fn len(&self) -> u64 {
        self.buf.len() as u64
    }

    /// Append `data` and record an object.
    fn put(&mut self, name: &str, kind: &str, data: &[u8], status: &str, note: &str) -> usize {
        let off = self.len();
        self.buf.extend_from_slice(data);
        self.objects.push(RawObject {
            name: name.to_string(),
            kind: kind.to_string(),
            status: status.to_string(),
            extents: vec![(off, data.len() as u64)],
            blake3: diskrescue_core::util::hash::blake3_hex(data),
            aligned: off % 4096 == 0,
            unreadable_bytes: 0,
            duplicate_of: None,
            note: note.to_string(),
        });
        self.objects.len() - 1
    }

    fn finish(self, dir: &Path, name: &str, skipped: Vec<String>) -> Result<RawImage> {
        write_image(dir, name, &self.buf, self.objects, None, skipped)
    }
}

fn write_image(
    dir: &Path,
    name: &str,
    buf: &[u8],
    objects: Vec<RawObject>,
    mapfile: Option<PathBuf>,
    skipped: Vec<String>,
) -> Result<RawImage> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{name}.img"));
    std::fs::write(&path, buf)?;
    let img = RawImage {
        path,
        size: buf.len() as u64,
        objects,
        mapfile,
        skipped,
    };
    let json = serde_json::to_string_pretty(&img)
        .map_err(|e| FixtureError::invalid(format!("truth serialisation: {e}")))?;
    std::fs::write(dir.join(format!("{name}.truth.json")), json)?;
    Ok(img)
}

/// Lay every sample out at 4 KiB alignment with varied fills.
fn contig_layout(seed: u64) -> Result<(Layout, Vec<String>)> {
    let (samples, skipped) = samples()?;
    let mut l = Layout::new(seed);
    l.fill(Fill::Zeros, 4096);
    for (i, s) in samples.iter().enumerate() {
        l.put(&s.name, s.kind, &s.data, "valid", "");
        l.align(4096, FILLS[i % 4]);
        let extra = 4096 * (1 + l.rng.below_usize(3));
        l.fill(FILLS[(i + 1) % 4], extra);
    }
    Ok((l, skipped))
}

/// All samples at 4 KiB alignment, separated by fills.
pub fn contig(dir: &Path, seed: u64) -> Result<RawImage> {
    let (l, skipped) = contig_layout(seed)?;
    l.finish(dir, "contig", skipped)
}

/// All samples at random byte offsets.
pub fn unaligned(dir: &Path, seed: u64) -> Result<RawImage> {
    let (samples, skipped) = samples()?;
    let mut l = Layout::new(seed);
    l.fill(Fill::Random, 1000);
    for (i, s) in samples.iter().enumerate() {
        // Odd lengths keep the next file off every power-of-two boundary.
        let gap = (513 + l.rng.below_usize(7000)) | 1;
        l.fill(FILLS[i % 4], gap);
        let note = if l.len() % 4096 == 0 {
            "happens to be aligned"
        } else {
            "not 4 KiB aligned"
        };
        l.put(&s.name, s.kind, &s.data, "valid", note);
    }
    l.fill(Fill::Zeros, 777);
    l.finish(dir, "unaligned", skipped)
}

/// Containers with embedded JPEGs: only the container is an expected
/// object, the inner pictures are `nested`.
pub fn nested(dir: &Path, seed: u64) -> Result<RawImage> {
    tools::require("ffmpeg")?;
    let mut l = Layout::new(seed);
    l.fill(Fill::Zeros, 4096);
    let mut skipped = Vec::new();

    // Records JPEGs found inside the object just placed.
    fn mark_nested(l: &mut Layout, parent: usize, limit: usize) {
        let (off, len) = l.objects[parent].extents[0];
        let body = l.buf[off as usize..(off + len) as usize].to_vec();
        let mut found = 0;
        let mut i = 8;
        while i + 4 < body.len() && found < limit {
            if body[i] == 0xFF
                && body[i + 1] == 0xD8
                && body[i + 2] == 0xFF
                && matches!(body[i + 3], 0xE0..=0xEF | 0xDB | 0xC0 | 0xC4 | 0xFE)
            {
                // End at the next EOI.
                if let Some(rel) = body[i..].windows(2).position(|w| w == [0xFF, 0xD9]) {
                    let end = i + rel + 2;
                    let data = &body[i..end];
                    l.objects.push(RawObject {
                        name: format!("{}#jpeg{}", l.objects[parent].name, found),
                        kind: "jpeg".into(),
                        status: "nested".into(),
                        extents: vec![(off + i as u64, data.len() as u64)],
                        blake3: diskrescue_core::util::hash::blake3_hex(data),
                        aligned: (off + i as u64) % 4096 == 0,
                        unreadable_bytes: 0,
                        duplicate_of: None,
                        note: format!("inside object {parent}"),
                    });
                    found += 1;
                    i = end;
                    continue;
                }
            }
            i += 1;
        }
    }

    // CR2-like with preview and thumbnail JPEGs.
    let cr2 = media::cr2_like(&TiffOpts {
        width: 256,
        height: 192,
        seed: 21,
        ..Default::default()
    });
    let p = l.put(
        "raw.cr2",
        "cr2",
        &cr2,
        "valid",
        "TIFF with JPEG preview + thumbnail",
    );
    mark_nested(&mut l, p, 4);
    l.align(4096, Fill::Zeros);

    // MJPEG AVI: every frame is a JPEG.
    let avi = media::video(&VideoOpts {
        container: Container::Avi,
        video: VideoCodec::Mjpeg,
        audio: AudioCodec::Pcm,
        seconds: 1.0,
        fps: 10,
        width: 160,
        height: 120,
        seed: 22,
        ..Default::default()
    })?;
    let p = l.put("mjpeg.avi", "avi", &avi, "valid", "every frame is a JPEG");
    mark_nested(&mut l, p, 64);
    l.align(4096, Fill::Zeros);

    // MP3 with a JPEG cover and MP4 with a cover picture, via ffmpeg.
    let dirt = tempfile::tempdir()?;
    let cover = dirt.path().join("cover.jpg");
    std::fs::write(&cover, media::baseline_jpeg(120, 120, 60, 23))?;
    let cover_s = cover.to_string_lossy().into_owned();
    let mp3 = dirt.path().join("a.mp3");
    let mp3_s = mp3.to_string_lossy().into_owned();
    let res = tools::run(
        "ffmpeg",
        &[
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=1",
            "-i",
            &cover_s,
            "-map",
            "0:a",
            "-map",
            "1:v",
            "-c:a",
            "libmp3lame",
            "-c:v",
            "copy",
            "-id3v2_version",
            "3",
            "-metadata:s:v",
            "title=Album cover",
            "-fflags",
            "+bitexact",
            &mp3_s,
        ],
    );
    match res {
        Ok(_) => {
            let d = std::fs::read(&mp3)?;
            let p = l.put("cover.mp3", "mp3", &d, "valid", "ID3v2 APIC holds a JPEG");
            mark_nested(&mut l, p, 1);
            l.align(4096, Fill::Zeros);
        }
        Err(e) if e.is_missing() => skipped.push(format!("cover.mp3 ({e})")),
        Err(_) => skipped.push("cover.mp3 (ffmpeg cannot write it here)".into()),
    }
    let mp4 = dirt.path().join("c.mp4");
    let mp4_s = mp4.to_string_lossy().into_owned();
    let res = tools::run(
        "ffmpeg",
        &[
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=160x120:rate=10:duration=1",
            "-i",
            &cover_s,
            "-map",
            "0:v",
            "-map",
            "1:v",
            "-c:v:0",
            "mpeg4",
            "-c:v:1",
            "copy",
            "-disposition:v:1",
            "attached_pic",
            "-fflags",
            "+bitexact",
            "-f",
            "mp4",
            &mp4_s,
        ],
    );
    match res {
        Ok(_) => {
            let d = std::fs::read(&mp4)?;
            let p = l.put("covr.mp4", "mp4", &d, "valid", "covr atom holds a JPEG");
            mark_nested(&mut l, p, 1);
            l.align(4096, Fill::Zeros);
        }
        Err(e) if e.is_missing() => skipped.push(format!("covr.mp4 ({e})")),
        Err(_) => skipped.push("covr.mp4 (ffmpeg cannot write it here)".into()),
    }
    l.fill(Fill::Zeros, 4096);
    l.finish(dir, "nested", skipped)
}

/// Fragmented JPEGs: one file split at a 4 KiB boundary with a gap of 1, 2,
/// 4 ... 64 clusters filled in four different ways, plus one split at a
/// non-cluster boundary (which a carver must report as truncated).
pub fn bifrag(dir: &Path, seed: u64) -> Result<RawImage> {
    let mut l = Layout::new(seed);
    // A JPEG big enough to split at a 4 KiB boundary with plenty left over.
    let target = media::jpeg(&JpegOpts {
        width: 640,
        height: 480,
        quality: 85,
        seed: 31,
        exif: Some(ExifOpts::default()),
        ..Default::default()
    })?;
    let other = media::baseline_jpeg(1024, 768, 90, 32);
    let newer = media::baseline_jpeg(48, 32, 50, 33);
    if newer.len() > 4096 || other.len() < 64 * 4096 + other.len() / 4 {
        return Err(FixtureError::corrupt(
            "bifrag samples",
            format!("unexpected JPEG sizes {} / {}", newer.len(), other.len()),
        ));
    }
    l.fill(Fill::Zeros, 4096);
    let whole = diskrescue_core::util::hash::blake3_hex(&target);
    let split = 8 * 4096; // first 8 clusters, then the gap
    for &gap in &[1usize, 2, 4, 8, 16, 32, 64] {
        for (fi, fill) in ["zeros", "random", "other-jpeg-middle", "newer-jpeg"]
            .iter()
            .enumerate()
        {
            l.align(4096, Fill::Zeros);
            let start = l.len();
            l.buf.extend_from_slice(&target[..split]);
            let gap_start = l.len();
            let gap_len = gap * 4096;
            match fi {
                0 => l.fill(Fill::Zeros, gap_len),
                1 => l.fill(Fill::Random, gap_len),
                2 => {
                    // Scan data from the middle of another picture.
                    let from = other.len() / 4;
                    l.buf.extend_from_slice(&other[from..from + gap_len]);
                }
                _ => {
                    l.buf.extend_from_slice(&newer);
                    let have = (l.len() - gap_start) as usize;
                    l.fill(Fill::Zeros, gap_len - have);
                }
            }
            let second_off = l.len();
            l.buf.extend_from_slice(&target[split..]);
            l.objects.push(RawObject {
                name: format!("frag_gap{gap}_{fill}.jpg"),
                kind: "jpeg".into(),
                status: "fragment".into(),
                extents: vec![
                    (start, split as u64),
                    (second_off, (target.len() - split) as u64),
                ],
                blake3: whole.clone(),
                aligned: start % 4096 == 0,
                unreadable_bytes: 0,
                duplicate_of: None,
                note: format!("gap of {gap} clusters filled with {fill}"),
            });
            if fi == 3 {
                // The complete small file inside the gap is itself an object.
                l.objects.push(RawObject {
                    name: format!("newer_gap{gap}.jpg"),
                    kind: "jpeg".into(),
                    status: "valid".into(),
                    extents: vec![(gap_start, newer.len() as u64)],
                    blake3: diskrescue_core::util::hash::blake3_hex(&newer),
                    aligned: gap_start % 4096 == 0,
                    unreadable_bytes: 0,
                    duplicate_of: None,
                    note: "the newer file sitting between the two fragments".into(),
                });
            }
        }
    }
    // Split off the cluster grid: the first part is not a whole number of clusters.
    l.align(4096, Fill::Zeros);
    let start = l.len();
    let cut = 8 * 4096 + 1000;
    l.buf.extend_from_slice(&target[..cut]);
    l.align(4096, Fill::Zeros);
    l.fill(Fill::Random, 4 * 4096);
    l.buf.extend_from_slice(&target[cut..]);
    l.objects.push(RawObject {
        name: "split_off_grid.jpg".into(),
        kind: "jpeg".into(),
        status: "truncated".into(),
        extents: vec![(start, cut as u64)],
        blake3: diskrescue_core::util::hash::blake3_hex(&target[..cut]),
        aligned: true,
        unreadable_bytes: 0,
        duplicate_of: None,
        note: "split not on a cluster boundary: only the prefix is recoverable".into(),
    });
    l.fill(Fill::Zeros, 4096);
    l.finish(dir, "bifrag", Vec::new())
}

/// Every sample cut at 60 % and followed by a fill.
pub fn truncated(dir: &Path, seed: u64) -> Result<RawImage> {
    let (samples, skipped) = samples()?;
    let mut l = Layout::new(seed);
    l.fill(Fill::Zeros, 4096);
    for (i, s) in samples.iter().enumerate() {
        let cut = damage::truncate(&s.data, s.data.len() * 60 / 100);
        l.put(&s.name, s.kind, &cut, "truncated", "cut at 60 %");
        l.align(4096, FILLS[i % 4]);
        l.fill(Fill::Random, 4096 * 2);
    }
    l.finish(dir, "truncated", skipped)
}

/// Text of a ddrescue mapfile for an image of `size` bytes in which the
/// given `(start, len)` ranges are bad (`-`) and everything else finished
/// (`+`).
pub fn mapfile_text(size: u64, bad: &[(u64, u64)]) -> String {
    let mut sorted: Vec<(u64, u64)> = bad.iter().copied().filter(|b| b.1 > 0).collect();
    sorted.sort_unstable();
    let mut s = String::new();
    s.push_str("# Mapfile. Created by DiskRescue fixtures\n");
    s.push_str("# Command line: ddrescue -d fixture.img out.img fixture.map\n");
    s.push_str("# current_pos  current_status  current_pass\n");
    s.push_str("0x00000000     +               1\n");
    s.push_str("#      pos        size  status\n");
    let mut pos = 0u64;
    for (start, len) in sorted {
        let start = start.min(size);
        let end = (start + len).min(size);
        if start < pos || end <= start {
            continue;
        }
        if start > pos {
            s.push_str(&format!("0x{pos:08X}  0x{:08X}  +\n", start - pos));
        }
        s.push_str(&format!("0x{start:08X}  0x{:08X}  -\n", end - start));
        pos = end;
    }
    if pos < size {
        s.push_str(&format!("0x{pos:08X}  0x{:08X}  +\n", size - pos));
    }
    s
}

/// [`contig`] with 4 KiB ranges zeroed inside some objects and a mapfile
/// that marks them bad.
pub fn holes(dir: &Path, seed: u64) -> Result<RawImage> {
    let (mut l, skipped) = contig_layout(seed)?;
    let mut bad = Vec::new();
    let mut rng = Rng::new(seed ^ 0xB0B);
    for (i, o) in l.objects.iter_mut().enumerate() {
        let (off, len) = o.extents[0];
        // Every third object gets one hole in its middle third, on a 4 KiB boundary.
        if i % 3 != 1 || len < 3 * 4096 {
            continue;
        }
        let lo = (off + len / 3).div_ceil(4096) * 4096;
        let hi = ((off + 2 * len / 3) / 4096) * 4096;
        if hi <= lo {
            continue;
        }
        let at = lo + 4096 * rng.below((hi - lo) / 4096);
        if at + 4096 > off + len {
            continue;
        }
        bad.push((at, 4096u64));
        o.unreadable_bytes = 4096;
        o.note = "contains an unreadable 4 KiB hole".into();
    }
    for &(at, n) in &bad {
        damage::zero_range(&mut l.buf, at as usize, n as usize);
    }
    let size = l.len();
    let map_path = dir.join("holes.map");
    std::fs::create_dir_all(dir)?;
    std::fs::write(&map_path, mapfile_text(size, &bad))?;
    write_image(dir, "holes", &l.buf, l.objects, Some(map_path), skipped)
}

/// Random data of `size` bytes: a carver must find nothing.
pub fn weak_sigs(dir: &Path, seed: u64, size: usize) -> Result<RawImage> {
    let mut rng = Rng::new(seed ^ 0xFEED);
    let buf = rng.bytes(size);
    write_image(dir, "weak_sigs", &buf, Vec::new(), None, Vec::new())
}

/// [`contig`] repeated twice, as on a fake-capacity drive that wraps around.
pub fn fake_capacity(dir: &Path, seed: u64) -> Result<RawImage> {
    let (l, skipped) = contig_layout(seed)?;
    let half = l.buf.len() as u64;
    let n = l.objects.len();
    let mut buf = l.buf.clone();
    buf.extend_from_slice(&l.buf);
    let mut objects = l.objects.clone();
    for (i, o) in l.objects.iter().enumerate() {
        let mut d = o.clone();
        for e in &mut d.extents {
            e.0 += half;
        }
        d.aligned = d.extents[0].0 % 4096 == 0;
        d.status = "duplicate".into();
        d.duplicate_of = Some(i);
        objects.push(d);
    }
    debug_assert_eq!(objects.len(), 2 * n);
    write_image(dir, "fake_capacity", &buf, objects, None, skipped)
}

/// A JPEG whose XMP declares a Google MicroVideo, followed immediately by an
/// MP4 (a Motion Photo).
pub fn motion_photo(dir: &Path, seed: u64) -> Result<RawImage> {
    let mp4 = media::video(&VideoOpts {
        seconds: 1.0,
        fps: 15,
        width: 160,
        height: 120,
        faststart: true,
        seed,
        ..Default::default()
    })?;
    let plain = media::baseline_jpeg(480, 320, 80, seed);
    let xmp = format!(
        "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\
         <rdf:Description xmlns:GCamera=\"http://ns.google.com/photos/1.0/camera/\" \
         GCamera:MicroVideo=\"1\" GCamera:MicroVideoVersion=\"1\" \
         GCamera:MicroVideoOffset=\"{}\" GCamera:MicroVideoPresentationTimestampUs=\"500000\"/>\
         </rdf:RDF></x:xmpmeta>",
        mp4.len()
    );
    let ns = b"http://ns.adobe.com/xap/1.0/\0";
    let len = 2 + ns.len() + xmp.len();
    let mut seg = vec![0xFF, 0xE1];
    seg.extend_from_slice(&(len as u16).to_be_bytes());
    seg.extend_from_slice(ns);
    seg.extend_from_slice(xmp.as_bytes());
    let jpeg = media::jpeg::splice_after_soi(&plain, &seg)?;
    let mut l = Layout::new(seed);
    l.fill(Fill::Zeros, 8192);
    let start = l.len();
    l.buf.extend_from_slice(&jpeg);
    l.buf.extend_from_slice(&mp4);
    let total = (jpeg.len() + mp4.len()) as u64;
    l.objects.push(RawObject {
        name: "motion.jpg".into(),
        kind: "jpeg".into(),
        status: "motion_photo".into(),
        extents: vec![(start, total)],
        blake3: diskrescue_core::util::hash::blake3_hex(&l.buf[start as usize..]),
        aligned: start % 4096 == 0,
        unreadable_bytes: 0,
        duplicate_of: None,
        note: format!(
            "JPEG of {} bytes + MP4 trailer of {} bytes",
            jpeg.len(),
            mp4.len()
        ),
    });
    l.fill(Fill::Zeros, 8192);
    l.finish(dir, "motion_photo", Vec::new())
}

/// Build every raw image into `dir`.
pub fn all(dir: &Path, seed: u64) -> Result<Vec<RawImage>> {
    Ok(vec![
        contig(dir, seed)?,
        unaligned(dir, seed)?,
        nested(dir, seed)?,
        bifrag(dir, seed)?,
        truncated(dir, seed)?,
        holes(dir, seed)?,
        weak_sigs(dir, seed, 8 << 20)?,
        fake_capacity(dir, seed)?,
        motion_photo(dir, seed)?,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapfile_format() {
        let t = mapfile_text(0x10000, &[(0x2000, 0x1000), (0x8000, 0x2000)]);
        let body: Vec<&str> = t.lines().filter(|l| !l.starts_with('#')).collect();
        assert_eq!(
            body,
            [
                "0x00000000     +               1",
                "0x00000000  0x00002000  +",
                "0x00002000  0x00001000  -",
                "0x00003000  0x00005000  +",
                "0x00008000  0x00002000  -",
                "0x0000A000  0x00006000  +",
            ]
        );
        // No bad ranges: one finished block. Overlaps and out-of-range are clamped.
        let t = mapfile_text(0x1000, &[]);
        assert!(t.contains("0x00000000  0x00001000  +"));
        let t = mapfile_text(0x1000, &[(0x800, 0x10000), (0x900, 0x10)]);
        assert!(t.contains("0x00000800  0x00000800  -"));
        assert!(!t.contains("0x00000900"));
    }

    #[test]
    fn layout_helpers() {
        let mut l = Layout::new(1);
        l.fill(Fill::Text, 10);
        l.align(16, Fill::Ones);
        assert_eq!(l.buf.len(), 16);
        assert_eq!(l.buf[10], 0xFF);
        let id = l.put("x", "jpeg", &[1, 2, 3], "valid", "");
        assert_eq!(l.objects[id].extents, vec![(16, 3)]);
        assert!(!l.objects[id].aligned);
    }
}
