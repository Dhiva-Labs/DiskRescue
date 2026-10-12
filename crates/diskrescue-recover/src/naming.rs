//! Safe, portable file names for recovered files.
//!
//! Names come from damaged media, so they can contain anything: control
//! characters, path separators, names reserved on Windows, or more bytes than
//! any filesystem accepts. Everything written to the output tree goes through
//! [`sanitize_component`], and nothing is ever overwritten: [`unique_path`]
//! picks a free name.

use std::path::{Path, PathBuf};

use diskrescue_core::time::Timestamp;

/// Longest component we emit, in bytes. Most filesystems allow 255; leave
/// room for the " (123)" suffix that [`unique_path`] may add.
pub const MAX_COMPONENT_BYTES: usize = 200;

const RESERVED_WINDOWS: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Make one path component safe on Linux, macOS and Windows.
///
/// * control characters and `< > : " / \ | ? *` become `_`
/// * leading/trailing spaces and trailing dots are removed
/// * Windows device names (`CON`, `LPT1.txt`, ...) get a `_` prefix
/// * `.` and `..` become `_`
/// * the result is cut to [`MAX_COMPONENT_BYTES`] on a character boundary,
///   keeping the extension when there is one
/// * an empty result becomes `_`
pub fn sanitize_component(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            '\u{FFFD}' => '_',
            c => c,
        })
        .collect();
    s = s
        .trim_start_matches(' ')
        .trim_end_matches([' ', '.'])
        .to_string();
    if s.is_empty() || s == "." || s == ".." {
        return "_".to_string();
    }
    let stem_upper = s.split('.').next().unwrap_or("").to_ascii_uppercase();
    if RESERVED_WINDOWS.contains(&stem_upper.as_str()) {
        s.insert(0, '_');
    }
    truncate_keep_extension(&s, MAX_COMPONENT_BYTES)
}

/// Cut `s` to at most `max` bytes on a char boundary, preserving a short
/// extension (up to 10 bytes) when present.
fn truncate_keep_extension(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let (stem, ext) = match s.rfind('.') {
        Some(i) if i > 0 && s.len() - i <= 11 => (&s[..i], &s[i..]),
        _ => (s, ""),
    };
    let budget = max.saturating_sub(ext.len());
    let mut cut = budget.min(stem.len());
    while cut > 0 && !stem.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}{}", &stem[..cut], ext)
}

/// Turn a `/`-separated path from a filesystem parser into a safe relative
/// path. Empty components, `.` and `..` are dropped, so the result can never
/// escape the directory it is joined to.
pub fn sanitize_relative_path(path: &str) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.split(['/', '\\']) {
        if comp.is_empty() || comp == "." || comp == ".." {
            continue;
        }
        out.push(sanitize_component(comp));
    }
    if out.as_os_str().is_empty() {
        out.push("_");
    }
    out
}

/// A path that does not exist yet: `path` itself, or `stem (1).ext`,
/// `stem (2).ext`, ... Gives up after 100 000 attempts and returns an error.
pub fn unique_path(path: &Path) -> std::io::Result<PathBuf> {
    if !path.exists() {
        return Ok(path.to_path_buf());
    }
    let parent = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = path.extension().map(|e| e.to_string_lossy().into_owned());
    for i in 1..100_000u32 {
        let name = match &ext {
            Some(e) => format!("{stem} ({i}).{e}"),
            None => format!("{stem} ({i})"),
        };
        let candidate = parent.join(name);
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!("no free name for {}", path.display()),
    ))
}

/// Name for a carved file: `f<offset as 12 hex digits>[_<YYYYMMDD_HHMMSS>].<ext>`.
/// The offset makes names stable across runs and lets the user find the file
/// in a hex editor; the date (when the media carried one) makes them sortable.
pub fn carved_name(offset: u64, created: Option<Timestamp>, ext: &str) -> String {
    let ext = sanitize_component(ext.trim_start_matches('.'));
    match created.filter(Timestamp::is_plausible) {
        Some(ts) => format!("f{offset:012x}_{}.{ext}", ts.compact()),
        None => format!("f{offset:012x}.{ext}"),
    }
}

/// Lowercase extension of a file name, without the dot.
pub fn extension_of(name: &str) -> Option<String> {
    let i = name.rfind('.')?;
    if i == 0 || i + 1 >= name.len() {
        return None;
    }
    Some(name[i + 1..].to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_hostile_names() {
        assert_eq!(sanitize_component("IMG_0001.JPG"), "IMG_0001.JPG");
        assert_eq!(
            sanitize_component("a/b\\c:d*e?f\"g<h>i|j"),
            "a_b_c_d_e_f_g_h_i_j"
        );
        assert_eq!(sanitize_component("bad\u{0}\u{1}name"), "bad__name");
        assert_eq!(sanitize_component("  trailing. . "), "trailing");
        assert_eq!(sanitize_component("CON"), "_CON");
        assert_eq!(sanitize_component("lpt1.txt"), "_lpt1.txt");
        assert_eq!(sanitize_component("CONSOLE.txt"), "CONSOLE.txt");
        assert_eq!(sanitize_component(".."), "_");
        assert_eq!(sanitize_component(""), "_");
        assert_eq!(sanitize_component("..."), "_");
        assert_eq!(sanitize_component("ünïcødé фото.jpg"), "ünïcødé фото.jpg");
    }

    #[test]
    fn truncates_on_char_boundary_keeping_extension() {
        let long = "é".repeat(300) + ".jpeg";
        let s = sanitize_component(&long);
        assert!(s.len() <= MAX_COMPONENT_BYTES);
        assert!(s.ends_with(".jpeg"));
        let no_ext = "x".repeat(500);
        assert_eq!(sanitize_component(&no_ext).len(), MAX_COMPONENT_BYTES);
    }

    #[test]
    fn relative_paths_cannot_escape() {
        assert_eq!(
            sanitize_relative_path("../../etc/passwd"),
            PathBuf::from("etc/passwd")
        );
        assert_eq!(
            sanitize_relative_path("/DCIM//100CANON/./IMG.JPG"),
            PathBuf::from("DCIM/100CANON/IMG.JPG")
        );
        assert_eq!(sanitize_relative_path(""), PathBuf::from("_"));
        assert_eq!(sanitize_relative_path("a\\..\\b"), PathBuf::from("a/b"));
    }

    #[test]
    fn unique_paths() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.jpg");
        assert_eq!(unique_path(&p).unwrap(), p);
        std::fs::write(&p, b"1").unwrap();
        let p1 = unique_path(&p).unwrap();
        assert_eq!(p1, dir.path().join("x (1).jpg"));
        std::fs::write(&p1, b"1").unwrap();
        assert_eq!(unique_path(&p).unwrap(), dir.path().join("x (2).jpg"));
        let q = dir.path().join("noext");
        std::fs::write(&q, b"1").unwrap();
        assert_eq!(unique_path(&q).unwrap(), dir.path().join("noext (1)"));
    }

    #[test]
    fn carved_names() {
        let ts = Timestamp::from_exif_str("2023:07:14 09:05:59").unwrap();
        assert_eq!(
            carved_name(0x1234, Some(ts), "jpg"),
            "f000000001234_20230714_090559.jpg"
        );
        assert_eq!(carved_name(0x1234, None, ".mp4"), "f000000001234.mp4");
        assert_eq!(
            carved_name(1, Some(Timestamp::from_unix(0)), "png"),
            "f000000000001.png"
        );
        assert_eq!(extension_of("a.B.JpG").as_deref(), Some("jpg"));
        assert_eq!(extension_of(".bashrc"), None);
    }
}
