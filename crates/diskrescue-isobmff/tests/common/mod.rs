//! Shared helpers for the integration tests: fixture generation with
//! ffmpeg (into a per-process temporary directory), ffprobe queries and
//! byte-level box manipulation.

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};

use diskrescue_core::source::{FileSource, MemSource};
use diskrescue_core::SharedSource;

/// True when `tool -version` runs.
pub fn have(tool: &str) -> bool {
    Command::new(tool)
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// True when ffmpeg and ffprobe are available; prints the skip message
/// otherwise.
pub fn ffmpeg_or_skip() -> bool {
    if have("ffmpeg") && have("ffprobe") {
        true
    } else {
        eprintln!("skipped: ffmpeg missing");
        false
    }
}

fn workdir() -> &'static Path {
    static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
    DIR.get_or_init(|| tempfile::tempdir().expect("tempdir"))
        .path()
}

/// A scratch path inside the per-process work directory.
pub fn scratch(name: &str) -> PathBuf {
    workdir().join(name)
}

const V: &[&str] = &["-f", "lavfi", "-i", "testsrc2=size=320x240:rate=30"];
const V_REF: &[&str] = &["-f", "lavfi", "-i", "testsrc=size=320x240:rate=30"];
const A: &[&str] = &["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000"];
const A_REF: &[&str] = &["-f", "lavfi", "-i", "sine=frequency=660:sample_rate=48000"];
const H264: &[&str] = &[
    "-c:v", "libx264", "-g", "30", "-bf", "2", "-pix_fmt", "yuv420p",
];
const H265: &[&str] = &[
    "-c:v",
    "libx265",
    "-x265-params",
    "log-level=none",
    "-tag:v",
    "hvc1",
    "-pix_fmt",
    "yuv420p",
];
const AAC: &[&str] = &["-c:a", "aac", "-b:a", "128k"];

fn recipe(name: &str) -> Option<(Vec<&'static str>, &'static str)> {
    let mut a: Vec<&'static str> = Vec::new();
    let ext = match name {
        "h264" | "h264_faststart" | "h264_ref" | "h264_6s" => {
            a.extend(if name == "h264_ref" { V_REF } else { V });
            a.extend(if name == "h264_ref" { A_REF } else { A });
            a.extend(["-t", if name == "h264_6s" { "6" } else { "3" }]);
            a.extend(H264);
            a.extend(AAC);
            if name == "h264_faststart" {
                a.extend(["-movflags", "+faststart"]);
            }
            "mp4"
        }
        "h264_bf0" => {
            a.extend(V);
            a.extend(A);
            a.extend([
                "-t", "3", "-c:v", "libx264", "-g", "30", "-bf", "0", "-pix_fmt", "yuv420p",
            ]);
            a.extend(AAC);
            "mp4"
        }
        "h264_bf0_ref" => {
            a.extend(V_REF);
            a.extend(A_REF);
            a.extend([
                "-t", "3", "-c:v", "libx264", "-g", "30", "-bf", "0", "-pix_fmt", "yuv420p",
            ]);
            a.extend(AAC);
            "mp4"
        }
        "h265" | "h265_ref" => {
            a.extend(if name == "h265_ref" { V_REF } else { V });
            a.extend(if name == "h265_ref" { A_REF } else { A });
            a.extend(["-t", "3"]);
            a.extend(H265);
            a.extend(AAC);
            "mp4"
        }
        "mov" | "mov_ref" => {
            a.extend(if name == "mov_ref" { V_REF } else { V });
            a.extend(if name == "mov_ref" { A_REF } else { A });
            a.extend(["-t", "3"]);
            a.extend(H264);
            a.extend(AAC);
            a.extend(["-f", "mov"]);
            "mov"
        }
        "mov_pcm" | "mov_pcm_ref" => {
            a.extend(if name == "mov_pcm_ref" { V_REF } else { V });
            a.extend(if name == "mov_pcm_ref" { A_REF } else { A });
            a.extend(["-t", "3"]);
            a.extend(H264);
            a.extend(["-c:a", "pcm_s16le", "-f", "mov"]);
            "mov"
        }
        "3gp" => {
            a.extend(V);
            a.extend(A);
            a.extend(["-t", "3"]);
            a.extend(H264);
            a.extend(["-c:a", "aac", "-b:a", "64k", "-f", "3gp", "-brand", "3gp4"]);
            "3gp"
        }
        "m4a" | "m4a_ref" => {
            a.extend(if name == "m4a_ref" { A_REF } else { A });
            a.extend(["-t", "3"]);
            a.extend(AAC);
            a.extend(["-f", "ipod"]);
            "m4a"
        }
        "video_only" | "video_only_ref" => {
            a.extend(if name == "video_only_ref" { V_REF } else { V });
            a.extend(["-t", "3"]);
            a.extend(H264);
            "mp4"
        }
        "long" | "long_ref" => {
            a.extend(if name == "long_ref" { V_REF } else { V });
            a.extend(if name == "long_ref" { A_REF } else { A });
            a.extend(["-t", if name == "long" { "60" } else { "3" }]);
            a.extend([
                "-c:v", "libx264", "-preset", "veryfast", "-g", "30", "-bf", "2", "-pix_fmt",
                "yuv420p",
            ]);
            a.extend(AAC);
            "mp4"
        }
        "h264_annexb" => {
            a.extend(V);
            a.extend(["-t", "1"]);
            a.extend(H264);
            a.extend(["-f", "h264"]);
            "h264"
        }
        "h265_annexb" => {
            a.extend(V);
            a.extend(["-t", "1"]);
            a.extend(H265);
            a.extend(["-f", "hevc"]);
            "hevc"
        }
        _ => return None,
    };
    Some((a, ext))
}

/// Path of the named fixture, generated on first use.
pub fn fixture(name: &str) -> PathBuf {
    static CACHE: OnceLock<Mutex<HashMap<String, PathBuf>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut g = cache.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(p) = g.get(name) {
        return p.clone();
    }
    let (args, ext) = recipe(name).unwrap_or_else(|| panic!("unknown fixture {name}"));
    let path = workdir().join(format!("{name}.{ext}"));
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-v", "error", "-y"])
        .args(&args)
        .arg(&path)
        .output()
        .expect("run ffmpeg");
    assert!(
        out.status.success(),
        "ffmpeg failed for {name}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    g.insert(name.to_string(), path.clone());
    path
}

/// Open a file as a shared source.
pub fn open(p: &Path) -> SharedSource {
    Arc::new(FileSource::open(p).expect("open"))
}

/// Wrap bytes as a shared source.
pub fn mem(b: Vec<u8>) -> SharedSource {
    Arc::new(MemSource::new(b))
}

/// Write bytes to a scratch file and return the path.
pub fn write_scratch(name: &str, b: &[u8]) -> PathBuf {
    let p = scratch(name);
    std::fs::write(&p, b).expect("write scratch");
    p
}

/// Top-level boxes: (type, offset, size).
pub fn top_boxes(b: &[u8]) -> Vec<([u8; 4], usize, usize)> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + 8 <= b.len() {
        let mut sz = u32::from_be_bytes(b[pos..pos + 4].try_into().unwrap()) as usize;
        let typ: [u8; 4] = b[pos + 4..pos + 8].try_into().unwrap();
        if sz == 1 {
            sz = u64::from_be_bytes(b[pos + 8..pos + 16].try_into().unwrap()) as usize;
        } else if sz == 0 {
            sz = b.len() - pos;
        }
        if sz < 8 {
            break;
        }
        out.push((typ, pos, sz));
        pos += sz;
    }
    out
}

/// Remove every top-level box of type `kind`.
pub fn strip_box(b: &[u8], kind: &[u8; 4]) -> Vec<u8> {
    let mut out = Vec::new();
    for (t, p, s) in top_boxes(b) {
        if &t != kind {
            out.extend_from_slice(&b[p..(p + s).min(b.len())]);
        }
    }
    out
}

/// Set the size field of the first top-level box of type `kind` to 0.
pub fn zero_box_size(b: &[u8], kind: &[u8; 4]) -> Vec<u8> {
    let mut out = b.to_vec();
    for (t, p, _) in top_boxes(b) {
        if &t == kind {
            out[p..p + 4].copy_from_slice(&[0, 0, 0, 0]);
            break;
        }
    }
    out
}

/// Deterministic pseudo-random bytes.
pub fn noise(n: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

/// Run ffprobe with JSON output.
pub fn ffprobe_json(path: &Path, args: &[&str]) -> serde_json::Value {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-of", "json"])
        .args(args)
        .arg(path)
        .output()
        .expect("run ffprobe");
    assert!(
        out.status.success(),
        "ffprobe failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("ffprobe json")
}

/// `ffprobe -v error` accepts the file with nothing on stderr.
pub fn ffprobe_clean(path: &Path) -> Result<(), String> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_format", "-show_streams"])
        .arg(path)
        .output()
        .map_err(|e| e.to_string())?;
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    if out.status.success() && err.trim().is_empty() {
        Ok(())
    } else {
        Err(err)
    }
}

/// `ffmpeg -v error -i file -f null -` decodes without any message.
pub fn decodes_clean(path: &Path) -> Result<(), String> {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-v", "error", "-i"])
        .arg(path)
        .args(["-f", "null", "-"])
        .output()
        .map_err(|e| e.to_string())?;
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    if out.status.success() && err.trim().is_empty() {
        Ok(())
    } else {
        Err(err)
    }
}

/// Stream summary: (codec, width, height, nb_frames, duration seconds).
#[derive(Debug, Clone, PartialEq)]
pub struct StreamInfo {
    pub codec: String,
    pub kind: String,
    pub width: u64,
    pub height: u64,
    pub frames: u64,
    pub duration: f64,
}

pub fn streams(path: &Path) -> Vec<StreamInfo> {
    let j = ffprobe_json(path, &["-show_streams", "-count_packets"]);
    j["streams"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|s| StreamInfo {
            codec: s["codec_name"].as_str().unwrap_or("").to_string(),
            kind: s["codec_type"].as_str().unwrap_or("").to_string(),
            width: s["width"].as_u64().unwrap_or(0),
            height: s["height"].as_u64().unwrap_or(0),
            frames: s["nb_read_packets"]
                .as_str()
                .and_then(|x| x.parse().ok())
                .unwrap_or(0),
            duration: s["duration"]
                .as_str()
                .and_then(|x| x.parse().ok())
                .unwrap_or(0.0),
        })
        .collect()
}

/// Packet (pos, size, pts, dts, keyframe) of one stream selector (`v`/`a`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Packet {
    pub pos: u64,
    pub size: u64,
    pub pts: i64,
    pub dts: i64,
    pub key: bool,
}

pub fn packets(path: &Path, sel: &str) -> Vec<Packet> {
    let j = ffprobe_json(
        path,
        &[
            "-select_streams",
            sel,
            "-show_entries",
            "packet=pos,size,pts,dts,flags",
        ],
    );
    j["packets"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|p| Packet {
            pos: p["pos"].as_str().and_then(|x| x.parse().ok()).unwrap_or(0),
            size: p["size"].as_str().and_then(|x| x.parse().ok()).unwrap_or(0),
            pts: p["pts"].as_i64().unwrap_or(i64::MIN),
            dts: p["dts"].as_i64().unwrap_or(i64::MIN),
            key: p["flags"].as_str().unwrap_or("").starts_with('K'),
        })
        .collect()
}

/// Presentation times of decoded video frames, in output order.
pub fn frame_pts(path: &Path) -> Vec<i64> {
    let j = ffprobe_json(
        path,
        &["-select_streams", "v", "-show_entries", "frame=pts"],
    );
    j["frames"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|f| f["pts"].as_i64())
        .collect()
}
