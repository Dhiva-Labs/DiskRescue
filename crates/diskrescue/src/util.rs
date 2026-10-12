//! Small helpers: size parsing and the command line as typed. Number
//! formatting, wildcard expansion and free-space lookup live in
//! `diskrescue_app` and are re-exported here.

use std::time::Duration;

pub use diskrescue_app::text::{fraction, percent, plural, shell_quote};
#[cfg(test)]
pub use diskrescue_app::util::{expand_file_args, free_space, wildcard_match};

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
