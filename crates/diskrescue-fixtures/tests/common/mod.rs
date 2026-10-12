//! Helpers shared by the integration tests.
#![allow(dead_code)]

pub mod readers;

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use diskrescue_fixtures::tools;

/// True (and prints a skip line) when any of `names` is missing.
pub fn skip_unless(names: &[&str]) -> bool {
    for n in names {
        if !tools::have(n) {
            eprintln!("skipped: {n} missing");
            return true;
        }
    }
    false
}

/// Run a checker and return `(success, combined output)`.
pub fn check(cmd: &str, args: &[&str]) -> (bool, String) {
    let out = tools::run_raw(cmd, args).expect("checker runs");
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), text)
}

/// Read `(offset, length)` ranges from an image and concatenate them.
pub fn read_extents(img: &Path, extents: &[(u64, u64)]) -> Vec<u8> {
    let mut f = std::fs::File::open(img).unwrap();
    let mut out = Vec::new();
    for &(off, len) in extents {
        f.seek(SeekFrom::Start(off)).unwrap();
        let mut buf = vec![0u8; len as usize];
        f.read_exact(&mut buf).unwrap();
        out.extend_from_slice(&buf);
    }
    out
}

/// Deterministic pseudo-random file content.
pub fn data(len: usize, seed: u64) -> Vec<u8> {
    diskrescue_fixtures::rng::Rng::new(seed).bytes(len)
}

/// Open `bytes` with Pillow, verify and fully decode it; returns
/// `(format, width, height, mode)`.
pub fn pillow_open(bytes: &[u8], ext: &str) -> (String, u32, u32, String) {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join(format!("x.{ext}"));
    std::fs::write(&p, bytes).unwrap();
    let script = "import sys\nfrom PIL import Image\nim = Image.open(sys.argv[1]); im.verify()\nim = Image.open(sys.argv[1]); im.load()\nprint(im.format, im.size[0], im.size[1], im.mode)";
    let out = tools::python(script, &[p.to_str().unwrap()]).expect("Pillow opens the file");
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let mut it = text.split_whitespace();
    (
        it.next().unwrap().to_string(),
        it.next().unwrap().parse().unwrap(),
        it.next().unwrap().parse().unwrap(),
        it.next().unwrap().to_string(),
    )
}

/// `ffprobe -v error -show_entries stream=codec_name,codec_type -of csv=p=0`
/// output lines for `bytes` written as `name`; `None` when ffprobe rejects
/// the file.
pub fn ffprobe_streams(bytes: &[u8], name: &str) -> Option<Vec<String>> {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join(name);
    std::fs::write(&p, bytes).unwrap();
    let out = tools::run_raw(
        "ffprobe",
        &[
            "-v",
            "error",
            "-show_entries",
            "stream=codec_name,codec_type",
            "-of",
            "csv=p=0",
            p.to_str().unwrap(),
        ],
    )
    .unwrap();
    if !out.status.success() || !out.stderr.is_empty() {
        return None;
    }
    let mut v: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().trim_end_matches(',').to_string())
        .filter(|l| !l.is_empty())
        .collect();
    // Formats with programs (MPEG-TS) list each stream twice.
    v.sort();
    v.dedup();
    Some(v)
}

/// Compare two byte buffers; on mismatch report lengths and the first
/// differing offset instead of dumping the buffers.
pub fn assert_same(got: &[u8], want: &[u8], what: &str) {
    if got == want {
        return;
    }
    let first = got.iter().zip(want).position(|(a, b)| a != b);
    panic!(
        "{what}: buffers differ (got {} bytes, want {} bytes, first difference at {first:?})",
        got.len(),
        want.len()
    );
}

/// `assert_eq!` for byte buffers with a compact failure message.
#[macro_export]
macro_rules! assert_bytes {
    ($a:expr, $b:expr $(,)?) => {
        $crate::common::assert_same(&$a, &$b, "assert_bytes")
    };
    ($a:expr, $b:expr, $($arg:tt)+) => {
        $crate::common::assert_same(&$a, &$b, &format!($($arg)+))
    };
}
