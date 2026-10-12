//! Small helpers: size parsing, wildcard expansion, command-line quoting,
//! free-space lookup.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// Parse a size such as `4096`, `64k`, `1M`, `2.5G`, `0x10000`, `512KiB` or `1MB`.
///
/// Bare suffixes (`k`, `M`, `G`, `T`) and the `iB` forms (`KiB`, `MiB`) are
/// binary (1024). Suffixes ending in `B` without the `i` (`kB`, `MB`, `GB`)
/// are decimal (1000), as printed on product labels.
pub fn parse_size(s: &str) -> Result<u64, String> {
    let bad = || {
        format!(
            "'{s}' is not a size. Use a number with an optional suffix, such as 4096, 64k, 1M or 2G"
        )
    };
    let t = s.trim();
    if t.is_empty() {
        return Err(bad());
    }
    if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        return u64::from_str_radix(&hex.replace('_', ""), 16).map_err(|_| bad());
    }
    let split = t
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '_'))
        .unwrap_or(t.len());
    let (num, suffix) = t.split_at(split);
    if num.is_empty() {
        return Err(bad());
    }
    let mult: u64 = match suffix.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "ki" | "kib" => 1 << 10,
        "m" | "mi" | "mib" => 1 << 20,
        "g" | "gi" | "gib" => 1 << 30,
        "t" | "ti" | "tib" => 1 << 40,
        "kb" => 1_000,
        "mb" => 1_000_000,
        "gb" => 1_000_000_000,
        "tb" => 1_000_000_000_000,
        _ => return Err(bad()),
    };
    let num = num.replace('_', "");
    if let Ok(n) = num.parse::<u64>() {
        return n
            .checked_mul(mult)
            .ok_or_else(|| format!("'{s}' is too large"));
    }
    let f: f64 = num.parse().map_err(|_| bad())?;
    let v = f * mult as f64;
    if !v.is_finite() || v < 0.0 || v >= u64::MAX as f64 {
        return Err(format!("'{s}' is too large"));
    }
    Ok(v as u64)
}

/// Parse a number of seconds (`10`, `2.5`) into a duration between 1 ms and 1 day.
pub fn parse_secs(s: &str) -> Result<Duration, String> {
    let v: f64 = s
        .trim()
        .parse()
        .map_err(|_| format!("'{s}' is not a number of seconds (for example 10 or 2.5)"))?;
    if !v.is_finite() || v <= 0.0 {
        return Err("the timeout must be greater than zero".into());
    }
    if v > 86_400.0 {
        return Err("the timeout cannot be longer than one day (86400 seconds)".into());
    }
    Ok(Duration::from_secs_f64(v))
}

/// Quote one command-line argument for display in a copy-and-paste command.
pub fn shell_quote(arg: &str) -> String {
    let safe = !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@%+,\\".contains(c));
    if safe {
        arg.to_string()
    } else if cfg!(windows) {
        // cmd.exe and PowerShell both understand double quotes.
        format!("\"{}\"", arg.replace('"', "\\\""))
    } else {
        format!("'{}'", arg.replace('\'', "'\\''"))
    }
}

/// The command line as typed (`diskrescue ...`), for mapfile headers and reports.
pub fn command_line() -> String {
    let mut parts = vec!["diskrescue".to_string()];
    parts.extend(
        std::env::args_os()
            .skip(1)
            .map(|a| shell_quote(&a.to_string_lossy())),
    );
    parts.join(" ")
}

/// Percentage with one decimal, never rounding up to 100.0 unless complete.
pub fn percent(part: u64, total: u64) -> String {
    if total == 0 {
        return "100%".into();
    }
    if part >= total {
        return "100%".into();
    }
    let p = part as f64 * 100.0 / total as f64;
    let p = (p * 10.0).floor() / 10.0;
    format!("{p:.1}%")
}

/// Fraction 0..=1 for a gauge.
pub fn fraction(part: u64, total: u64) -> f64 {
    if total == 0 {
        1.0
    } else {
        (part as f64 / total as f64).clamp(0.0, 1.0)
    }
}

/// Plural helper: `1 file`, `2 files`.
pub fn plural(n: u64, one: &str, many: &str) -> String {
    format!(
        "{} {}",
        diskrescue_core::util::human::count(n),
        if n == 1 { one } else { many }
    )
}

/// Case-insensitive `*` / `?` wildcard match on a file name.
pub fn wildcard_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let n: Vec<char> = name.to_lowercase().chars().collect();
    let (mut pi, mut ni) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ni;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Expand `*` and `?` in file names that the shell left alone (Windows `cmd`
/// does not expand them) and check that every input exists and is a file.
pub fn expand_file_args(args: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    let mut out: Vec<PathBuf> = Vec::new();
    for a in args {
        let name = a
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !a.exists() && (name.contains('*') || name.contains('?')) {
            let dir = match a.parent() {
                Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
                _ => PathBuf::from("."),
            };
            let mut found: Vec<PathBuf> = std::fs::read_dir(&dir)
                .map_err(|e| format!("cannot read the folder {}: {e}", dir.display()))?
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.file_type().map(|t| t.is_file()).unwrap_or(false)
                        && wildcard_match(&name, &e.file_name().to_string_lossy())
                })
                .map(|e| e.path())
                .collect();
            found.sort();
            if found.is_empty() {
                return Err(format!("no files match {}", a.display()));
            }
            out.extend(found);
            continue;
        }
        match std::fs::metadata(a) {
            Ok(m) if m.is_file() => out.push(a.clone()),
            Ok(m) if m.is_dir() => {
                return Err(format!(
                    "{} is a folder; give the files inside it (for example {}/*.jpg)",
                    a.display(),
                    a.display()
                ))
            }
            Ok(_) => {
                return Err(format!(
                    "{} is a device, not a file. This command works on files; \
                     to get files back from a device use 'diskrescue recover'",
                    a.display()
                ))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(format!("{} does not exist", a.display()))
            }
            Err(e) => return Err(format!("cannot read {}: {e}", a.display())),
        }
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|p| seen.insert(p.clone()));
    Ok(out)
}

/// Free space, in bytes, on the filesystem that holds `path` (or its nearest
/// existing ancestor). `None` when it cannot be determined.
pub fn free_space(path: &Path) -> Option<u64> {
    let mut cur = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    while !cur.exists() {
        if !cur.pop() {
            return None;
        }
    }
    free_space_of_existing(&cur)
}

#[cfg(unix)]
fn free_space_of_existing(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `c` is a valid NUL-terminated string and `st` is a properly
    // sized, writable statvfs buffer that statvfs fully initialises on success.
    unsafe {
        let mut st: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c.as_ptr(), &mut st) != 0 {
            return None;
        }
        #[allow(clippy::unnecessary_cast)]
        Some((st.f_bavail as u64).saturating_mul(st.f_frsize as u64))
    }
}

#[cfg(windows)]
fn free_space_of_existing(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut avail: u64 = 0;
    let mut total: u64 = 0;
    let mut free: u64 = 0;
    // SAFETY: `wide` is NUL terminated and the three out-pointers refer to
    // live u64 variables for the duration of the call.
    let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut avail, &mut total, &mut free) };
    (ok != 0).then_some(avail)
}

#[cfg(not(any(unix, windows)))]
fn free_space_of_existing(_path: &Path) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("4096").unwrap(), 4096);
        assert_eq!(parse_size("64k").unwrap(), 65_536);
        assert_eq!(parse_size("64K").unwrap(), 65_536);
        assert_eq!(parse_size("1M").unwrap(), 1 << 20);
        assert_eq!(parse_size("2G").unwrap(), 2 << 30);
        assert_eq!(parse_size("1.5k").unwrap(), 1536);
        assert_eq!(parse_size("20KiB").unwrap(), 20 * 1024);
        assert_eq!(parse_size("1MB").unwrap(), 1_000_000);
        assert_eq!(parse_size("1kB").unwrap(), 1000);
        assert_eq!(parse_size("0x10000").unwrap(), 65_536);
        assert_eq!(parse_size(" 7 ").unwrap(), 7);
        assert_eq!(parse_size("1_000").unwrap(), 1000);
        assert!(parse_size("").is_err());
        assert!(parse_size("abc").is_err());
        assert!(parse_size("12x").is_err());
        assert!(parse_size("-5").is_err());
        assert!(parse_size("99999999999T").is_err());
        assert!(parse_size("0xZZ").is_err());
        let e = parse_size("fast").unwrap_err();
        assert!(e.contains("64k"), "{e}");
    }

    #[test]
    fn seconds() {
        assert_eq!(parse_secs("10").unwrap(), Duration::from_secs(10));
        assert_eq!(parse_secs("0.5").unwrap(), Duration::from_millis(500));
        assert!(parse_secs("0").is_err());
        assert!(parse_secs("-1").is_err());
        assert!(parse_secs("nan").is_err());
        assert!(parse_secs("1e12").is_err());
        assert!(parse_secs("soon").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn quoting() {
        assert_eq!(shell_quote("/dev/sdb"), "/dev/sdb");
        assert_eq!(shell_quote("my card.img"), "'my card.img'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote(""), "''");
    }

    #[cfg(windows)]
    #[test]
    fn quoting() {
        assert_eq!(shell_quote(r"\\.\PhysicalDrive2"), r"\\.\PhysicalDrive2");
        assert_eq!(shell_quote("my card.img"), "\"my card.img\"");
    }

    #[test]
    fn percentages() {
        assert_eq!(percent(0, 100), "0.0%");
        assert_eq!(percent(50, 100), "50.0%");
        assert_eq!(percent(100, 100), "100%");
        assert_eq!(percent(0, 0), "100%");
        // Almost done must not round up to 100.
        assert_eq!(percent(99_999, 100_000), "99.9%");
        assert_eq!(percent(1, 3), "33.3%");
    }

    #[test]
    fn wildcards() {
        assert!(wildcard_match("*.jpg", "IMG_0001.JPG"));
        assert!(wildcard_match("IMG_00??.jpg", "img_0042.jpg"));
        assert!(!wildcard_match("*.jpg", "a.png"));
        assert!(wildcard_match("*", "anything"));
        assert!(wildcard_match("a*b*c", "aXXbYYc"));
        assert!(!wildcard_match("a*b*c", "aXXbYY"));
        assert!(wildcard_match("", ""));
        assert!(!wildcard_match("", "x"));
    }

    #[test]
    fn expands_inputs() {
        let dir = tempfile::tempdir().unwrap();
        for n in ["a.jpg", "b.jpg", "c.png"] {
            std::fs::write(dir.path().join(n), b"x").unwrap();
        }
        let got = expand_file_args(&[dir.path().join("*.jpg")]).unwrap();
        assert_eq!(got.len(), 2);
        assert!(expand_file_args(&[dir.path().join("*.gif")]).is_err());
        assert!(expand_file_args(&[dir.path().join("missing.jpg")]).is_err());
        let e = expand_file_args(&[dir.path().to_path_buf()]).unwrap_err();
        assert!(e.contains("folder"), "{e}");
        // Duplicates collapse.
        let one = dir.path().join("a.jpg");
        assert_eq!(
            expand_file_args(&[one.clone(), one.clone()]).unwrap(),
            vec![one]
        );
    }

    #[test]
    fn free_space_is_known_for_temp_dir() {
        let dir = tempfile::tempdir().unwrap();
        #[cfg(any(unix, windows))]
        assert!(free_space(&dir.path().join("not/yet/there")).is_some());
        let _ = dir;
    }
}
