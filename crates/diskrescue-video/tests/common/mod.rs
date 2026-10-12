//! Shared helpers for the integration tests: fixture generation with ffmpeg,
//! a deterministic random generator, source wrappers and ffprobe access.

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use diskrescue_core::source::MemSource;
use diskrescue_core::{ReadError, SharedSource, Source};

/// True when `tool` runs (`tool -version`).
pub fn have(tool: &str) -> bool {
    Command::new(tool)
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Announce and report a missing tool.
pub fn need(tool: &str) -> bool {
    if have(tool) {
        true
    } else {
        eprintln!("skipped: {tool} missing");
        false
    }
}

/// True when ffmpeg can encode with `encoder`.
pub fn has_encoder(encoder: &str) -> bool {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .lines()
            .any(|l| l.split_whitespace().nth(1) == Some(encoder)),
        Err(_) => false,
    }
}

/// Skip unless ffmpeg and every listed encoder exist.
pub fn need_encoders(encs: &[&str]) -> bool {
    if !need("ffmpeg") {
        return false;
    }
    for e in encs {
        if !has_encoder(e) {
            eprintln!("skipped: ffmpeg encoder {e} missing");
            return false;
        }
    }
    true
}

pub fn mem(data: Vec<u8>) -> SharedSource {
    Arc::new(MemSource::new(data))
}

pub fn mem_slice(data: &[u8]) -> SharedSource {
    mem(data.to_vec())
}

/// Deterministic xorshift generator.
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    pub fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| (self.next() >> 24) as u8).collect()
    }
}

static CACHE: OnceLock<Mutex<HashMap<String, Arc<Vec<u8>>>>> = OnceLock::new();

/// Generate (once per test process) and cache a fixture by name.
pub fn fixture(name: &str, make: impl FnOnce() -> Vec<u8>) -> Arc<Vec<u8>> {
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(v) = guard.get(name) {
        return v.clone();
    }
    let v = Arc::new(make());
    guard.insert(name.to_string(), v.clone());
    v
}

/// Run ffmpeg with `args` (output file name appended last) in a temporary
/// directory and return the bytes it wrote. Panics when ffmpeg fails.
pub fn ffmpeg(args: &[&str], out_name: &str) -> Vec<u8> {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join(out_name);
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-nostdin", "-v", "error", "-y", "-threads", "2"]);
    cmd.args(args);
    cmd.arg(&out);
    let o = cmd.output().expect("run ffmpeg");
    assert!(
        o.status.success(),
        "ffmpeg {:?} failed: {}",
        args,
        String::from_utf8_lossy(&o.stderr)
    );
    std::fs::read(&out).expect("read ffmpeg output")
}

pub const VIDEO_IN: [&str; 4] = ["-f", "lavfi", "-i", "testsrc2=size=320x240:rate=30"];
pub const AUDIO_IN: [&str; 4] = ["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000"];

/// `ffmpeg <video input> <audio input> <extra> out`.
pub fn av(extra: &[&str], out_name: &str) -> Vec<u8> {
    let mut args: Vec<&str> = Vec::new();
    args.extend_from_slice(&VIDEO_IN);
    args.extend_from_slice(&AUDIO_IN);
    args.extend_from_slice(extra);
    ffmpeg(&args, out_name)
}

/// Run `ffprobe -show_streams -show_format -of json` on a file.
pub fn ffprobe(path: &Path) -> Option<serde_json::Value> {
    let o = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-count_frames",
            "-show_streams",
            "-show_format",
            "-of",
            "json",
        ])
        .arg(path)
        .output()
        .ok()?;
    if !o.status.success() {
        return None;
    }
    serde_json::from_slice(&o.stdout).ok()
}

/// Decode the whole file with ffmpeg; true when nothing was reported on
/// stderr at error level.
pub fn decodes_cleanly(path: &Path) -> Result<(), String> {
    let o = Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-i"])
        .arg(path)
        .args(["-f", "null", "-"])
        .output()
        .map_err(|e| e.to_string())?;
    let err = String::from_utf8_lossy(&o.stderr).to_string();
    if o.status.success() && err.trim().is_empty() {
        Ok(())
    } else {
        Err(err)
    }
}

/// Write bytes to a temp file and return the dir guard and path.
pub fn write_temp(name: &str, data: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let p = dir.path().join(name);
    std::fs::write(&p, data).expect("write temp");
    (dir, p)
}

/// A source wrapper that records how much was read and the largest single
/// request, to prove streaming behaviour.
pub struct Counting {
    inner: SharedSource,
    pub total: AtomicU64,
    pub max_request: AtomicU64,
    pub calls: AtomicU64,
    /// Highest offset (exclusive) any request reached.
    pub max_end: AtomicU64,
}

impl Counting {
    pub fn new(inner: SharedSource) -> Self {
        Counting {
            inner,
            total: AtomicU64::new(0),
            max_request: AtomicU64::new(0),
            calls: AtomicU64::new(0),
            max_end: AtomicU64::new(0),
        }
    }
}

impl Source for Counting {
    fn size(&self) -> u64 {
        self.inner.size()
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, ReadError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.total.fetch_add(buf.len() as u64, Ordering::Relaxed);
        self.max_request
            .fetch_max(buf.len() as u64, Ordering::Relaxed);
        self.max_end
            .fetch_max(offset + buf.len() as u64, Ordering::Relaxed);
        self.inner.read_at(offset, buf)
    }
    fn describe(&self) -> String {
        "counting".into()
    }
}

/// A large virtual source that repeats `pattern` (no memory proportional to
/// its size).
pub struct Repeating {
    pattern: Vec<u8>,
    size: u64,
}

impl Repeating {
    pub fn new(pattern: Vec<u8>, size: u64) -> Self {
        Repeating { pattern, size }
    }
}

impl Source for Repeating {
    fn size(&self) -> u64 {
        self.size
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, ReadError> {
        if offset >= self.size {
            return Ok(0);
        }
        let n = ((self.size - offset).min(buf.len() as u64)) as usize;
        let plen = self.pattern.len() as u64;
        let mut done = 0usize;
        while done < n {
            let p = ((offset + done as u64) % plen) as usize;
            let k = (self.pattern.len() - p).min(n - done);
            buf[done..done + k].copy_from_slice(&self.pattern[p..p + k]);
            done += k;
        }
        Ok(n)
    }
    fn describe(&self) -> String {
        "repeating".into()
    }
}

/// Concatenate a file with random junk after it (to prove exact lengths).
pub fn with_trailer(data: &[u8], junk: usize, seed: u64) -> Vec<u8> {
    let mut v = data.to_vec();
    v.extend(Rng(seed).bytes(junk));
    v
}

/// Run ffmpeg on an input held in memory.
pub fn ffmpeg_from(input: &[u8], in_name: &str, args: &[&str], out_name: &str) -> Vec<u8> {
    let dir = tempfile::tempdir().expect("tempdir");
    let inp = dir.path().join(in_name);
    std::fs::write(&inp, input).expect("write input");
    let out = dir.path().join(out_name);
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-nostdin", "-v", "error", "-y", "-threads", "2", "-i"]);
    cmd.arg(&inp);
    cmd.args(args);
    cmd.arg(&out);
    let o = cmd.output().expect("run ffmpeg");
    assert!(
        o.status.success(),
        "ffmpeg {:?} failed: {}",
        args,
        String::from_utf8_lossy(&o.stderr)
    );
    std::fs::read(&out).expect("read ffmpeg output")
}

/// A small JPEG made by ffmpeg (cover-art tests).
pub fn jpeg_image() -> Option<Arc<Vec<u8>>> {
    need_encoders(&["mjpeg"]).then(|| {
        fixture("cover_jpeg", || {
            ffmpeg(
                &[
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc2=size=96x64:rate=1",
                    "-frames:v",
                    "1",
                    "-q:v",
                    "3",
                ],
                "cover.jpg",
            )
        })
    })
}

/// A small PNG made by ffmpeg.
pub fn png_image() -> Option<Arc<Vec<u8>>> {
    need_encoders(&["png"]).then(|| {
        fixture("cover_png", || {
            ffmpeg(
                &[
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc2=size=64x48:rate=1",
                    "-frames:v",
                    "1",
                ],
                "cover.png",
            )
        })
    })
}

pub fn mkv_h264_aac() -> Option<Arc<Vec<u8>>> {
    need_encoders(&["libx264", "aac"]).then(|| {
        fixture("mkv_h264_aac", || {
            av(
                &[
                    "-t",
                    "3",
                    "-c:v",
                    "libx264",
                    "-preset",
                    "ultrafast",
                    "-pix_fmt",
                    "yuv420p",
                    "-c:a",
                    "aac",
                    "-metadata",
                    "creation_time=2024-05-01T12:30:00Z",
                    "-f",
                    "matroska",
                ],
                "o.mkv",
            )
        })
    })
}

pub fn webm_vp8_vorbis() -> Option<Arc<Vec<u8>>> {
    need_encoders(&["libvpx", "libvorbis"]).then(|| {
        fixture("webm_vp8_vorbis", || {
            av(
                &[
                    "-t",
                    "2",
                    "-c:v",
                    "libvpx",
                    "-deadline",
                    "realtime",
                    "-cpu-used",
                    "8",
                    "-c:a",
                    "libvorbis",
                    "-f",
                    "webm",
                ],
                "o.webm",
            )
        })
    })
}

pub fn webm_vp9_opus() -> Option<Arc<Vec<u8>>> {
    need_encoders(&["libvpx-vp9", "libopus"]).then(|| {
        fixture("webm_vp9_opus", || {
            av(
                &[
                    "-t",
                    "2",
                    "-c:v",
                    "libvpx-vp9",
                    "-deadline",
                    "realtime",
                    "-cpu-used",
                    "8",
                    "-c:a",
                    "libopus",
                    "-f",
                    "webm",
                ],
                "o.webm",
            )
        })
    })
}

/// Matroska with unknown-size Segment and Clusters (`-live 1`).
pub fn mkv_live() -> Option<Arc<Vec<u8>>> {
    let src = mkv_h264_aac()?;
    Some(fixture("mkv_live", || {
        ffmpeg_from(
            &src,
            "in.mkv",
            &["-c", "copy", "-live", "1", "-f", "matroska"],
            "live.mkv",
        )
    }))
}

/// A fixture and the bytes of a picture embedded in it.
pub type WithCover = (Arc<Vec<u8>>, Arc<Vec<u8>>);

pub fn mkv_with_cover() -> Option<WithCover> {
    let jpg = jpeg_image()?;
    if !need_encoders(&["libx264", "aac"]) {
        return None;
    }
    let data = fixture("mkv_cover", || {
        let dir = tempfile::tempdir().expect("tempdir");
        let cover = dir.path().join("cover.jpg");
        std::fs::write(&cover, &**jpg).unwrap();
        let out = dir.path().join("o.mkv");
        let st = Command::new("ffmpeg")
            .args(["-nostdin", "-v", "error", "-y"])
            .args(VIDEO_IN)
            .args(AUDIO_IN)
            .args(["-attach"])
            .arg(&cover)
            .args([
                "-metadata:s:t",
                "mimetype=image/jpeg",
                "-metadata:s:t",
                "filename=cover.jpg",
                "-t",
                "2",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-c:a",
                "aac",
                "-f",
                "matroska",
            ])
            .arg(&out)
            .status()
            .unwrap();
        assert!(st.success());
        std::fs::read(&out).unwrap()
    });
    Some((data, jpg))
}

pub fn ts_h264_aac() -> Option<Arc<Vec<u8>>> {
    need_encoders(&["libx264", "aac"]).then(|| {
        fixture("ts_h264_aac", || {
            av(
                &[
                    "-t",
                    "3",
                    "-c:v",
                    "libx264",
                    "-preset",
                    "ultrafast",
                    "-pix_fmt",
                    "yuv420p",
                    "-c:a",
                    "aac",
                    "-f",
                    "mpegts",
                ],
                "o.ts",
            )
        })
    })
}

pub fn ts_mpeg2_mp2() -> Option<Arc<Vec<u8>>> {
    need_encoders(&["mpeg2video", "mp2"]).then(|| {
        fixture("ts_mpeg2_mp2", || {
            av(
                &[
                    "-t",
                    "2",
                    "-c:v",
                    "mpeg2video",
                    "-q:v",
                    "5",
                    "-c:a",
                    "mp2",
                    "-f",
                    "mpegts",
                ],
                "o.ts",
            )
        })
    })
}

pub fn m2ts_native() -> Option<Arc<Vec<u8>>> {
    need_encoders(&["libx264", "aac"]).then(|| {
        fixture("m2ts_native", || {
            av(
                &[
                    "-t",
                    "2",
                    "-c:v",
                    "libx264",
                    "-preset",
                    "ultrafast",
                    "-pix_fmt",
                    "yuv420p",
                    "-c:a",
                    "aac",
                    "-f",
                    "mpegts",
                    "-mpegts_m2ts_mode",
                    "1",
                ],
                "o.m2ts",
            )
        })
    })
}

/// Prefix every 188-byte packet with a 4-byte timecode, as AVCHD/Blu-ray do.
pub fn to_m2ts(ts: &[u8]) -> Vec<u8> {
    assert_eq!(ts.len() % 188, 0);
    let mut out = Vec::with_capacity(ts.len() / 188 * 192);
    for (i, p) in ts.chunks_exact(188).enumerate() {
        let t = (i as u32).wrapping_mul(1000) & 0x3FFF_FFFF;
        out.extend_from_slice(&t.to_be_bytes());
        out.extend_from_slice(p);
    }
    out
}

pub fn vob_mpeg2_ac3() -> Option<Arc<Vec<u8>>> {
    need_encoders(&["mpeg2video", "ac3"]).then(|| {
        fixture("vob_mpeg2_ac3", || {
            av(
                &[
                    "-t",
                    "3",
                    "-c:v",
                    "mpeg2video",
                    "-q:v",
                    "5",
                    "-c:a",
                    "ac3",
                    "-f",
                    "vob",
                ],
                "o.vob",
            )
        })
    })
}

pub fn vob_mpeg2_mp2() -> Option<Arc<Vec<u8>>> {
    need_encoders(&["mpeg2video", "mp2"]).then(|| {
        fixture("vob_mpeg2_mp2", || {
            av(
                &[
                    "-t",
                    "3",
                    "-c:v",
                    "mpeg2video",
                    "-q:v",
                    "5",
                    "-c:a",
                    "mp2",
                    "-f",
                    "vob",
                ],
                "o.vob",
            )
        })
    })
}

pub fn mpg_mpeg2_mp2() -> Option<Arc<Vec<u8>>> {
    need_encoders(&["mpeg2video", "mp2"]).then(|| {
        fixture("mpg_mpeg2_mp2", || {
            av(
                &[
                    "-t",
                    "3",
                    "-c:v",
                    "mpeg2video",
                    "-q:v",
                    "5",
                    "-c:a",
                    "mp2",
                    "-f",
                    "mpeg",
                ],
                "o.mpg",
            )
        })
    })
}

pub fn mpg_mpeg1() -> Option<Arc<Vec<u8>>> {
    need_encoders(&["mpeg1video", "mp2"]).then(|| {
        fixture("mpg_mpeg1", || {
            av(
                &[
                    "-t",
                    "2",
                    "-c:v",
                    "mpeg1video",
                    "-q:v",
                    "5",
                    "-c:a",
                    "mp2",
                    "-f",
                    "mpeg",
                ],
                "o.mpg",
            )
        })
    })
}

pub fn mp3_plain() -> Option<Arc<Vec<u8>>> {
    need_encoders(&["libmp3lame"]).then(|| {
        fixture("mp3_plain", || {
            ffmpeg(
                &[
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=440:sample_rate=44100",
                    "-t",
                    "3",
                    "-c:a",
                    "libmp3lame",
                    "-b:a",
                    "128k",
                    "-id3v2_version",
                    "0",
                    "-write_xing",
                    "0",
                    "-f",
                    "mp3",
                ],
                "o.mp3",
            )
        })
    })
}

pub fn wav_pcm() -> Option<Arc<Vec<u8>>> {
    need_encoders(&["pcm_s16le"]).then(|| {
        fixture("wav_pcm", || {
            ffmpeg(
                &[
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=440:sample_rate=44100",
                    "-t",
                    "2",
                    "-c:a",
                    "pcm_s16le",
                    "-f",
                    "wav",
                ],
                "o.wav",
            )
        })
    })
}

pub fn flac_stereo() -> Option<Arc<Vec<u8>>> {
    need_encoders(&["flac"]).then(|| {
        fixture("flac_stereo", || {
            ffmpeg(
                &[
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=440:sample_rate=44100,aformat=channel_layouts=stereo",
                    "-t",
                    "3",
                    "-c:a",
                    "flac",
                    "-f",
                    "flac",
                ],
                "o.flac",
            )
        })
    })
}

/// Run ffmpeg writing the container to stdout (`pipe:1`), which makes
/// muxers that need to seek leave their size fields unset.
pub fn ffmpeg_pipe(args: &[&str]) -> Vec<u8> {
    let o = Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-y", "-threads", "2"])
        .args(args)
        .arg("pipe:1")
        .output()
        .expect("run ffmpeg");
    assert!(
        o.status.success(),
        "ffmpeg {:?} failed: {}",
        args,
        String::from_utf8_lossy(&o.stderr)
    );
    o.stdout
}

/// A sine tone input of the given sample rate.
pub fn sine_args(rate: u32, secs: u32) -> Vec<String> {
    vec![
        "-f".into(),
        "lavfi".into(),
        "-i".into(),
        format!("sine=frequency=440:sample_rate={rate}"),
        "-t".into(),
        secs.to_string(),
    ]
}

/// Encode a tone with extra output arguments; the file name's extension picks
/// nothing, `-f` must be among the arguments.
pub fn tone(rate: u32, secs: u32, extra: &[&str], out_name: &str) -> Vec<u8> {
    let owned = sine_args(rate, secs);
    let mut args: Vec<&str> = owned.iter().map(String::as_str).collect();
    args.extend_from_slice(extra);
    ffmpeg(&args, out_name)
}
