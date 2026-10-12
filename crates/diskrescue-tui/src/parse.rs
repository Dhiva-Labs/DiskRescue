//! Reading numbers typed into forms.

use std::time::Duration;

/// Parse a size such as `4096`, `64k`, `1M`, `2.5G`, `512KiB` or `1MB`.
///
/// Bare suffixes (`k`, `M`, `G`) and the `iB` forms are binary (1024);
/// suffixes ending in `B` without the `i` (`kB`, `MB`) are decimal (1000),
/// as printed on product labels.
pub fn parse_size(s: &str) -> Result<u64, String> {
    let bad = || {
        format!(
            "'{s}' is not a size. Use a number with an optional suffix, such as 4096, 64k, 1M or 2G."
        )
    };
    let t = s.trim();
    if t.is_empty() {
        return Err(bad());
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
        "kb" => 1_000,
        "mb" => 1_000_000,
        "gb" => 1_000_000_000,
        _ => return Err(bad()),
    };
    let num = num.replace('_', "");
    if let Ok(n) = num.parse::<u64>() {
        return n
            .checked_mul(mult)
            .ok_or_else(|| format!("'{s}' is too large."));
    }
    let f: f64 = num.parse().map_err(|_| bad())?;
    let v = f * mult as f64;
    if !v.is_finite() || v < 0.0 || v >= u64::MAX as f64 {
        return Err(format!("'{s}' is too large."));
    }
    Ok(v as u64)
}

/// Parse a number of seconds (`30`, `2.5`, `10s`).
pub fn parse_seconds(s: &str) -> Result<Duration, String> {
    let t = s.trim().trim_end_matches(['s', 'S']).trim();
    let v: f64 = t
        .parse()
        .map_err(|_| format!("'{s}' is not a number of seconds. Use for example 30."))?;
    if !v.is_finite() || v <= 0.0 || v > 86_400.0 {
        return Err(format!(
            "'{s}' is not a sensible number of seconds. Use a value between 0.1 and 86400."
        ));
    }
    Ok(Duration::from_secs_f64(v))
}

/// Parse a count of retry passes.
pub fn parse_count(s: &str) -> Result<u32, String> {
    s.trim()
        .parse::<u32>()
        .map_err(|_| format!("'{s}' is not a whole number. Use for example 3."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("4096").unwrap(), 4096);
        assert_eq!(parse_size("64k").unwrap(), 65_536);
        assert_eq!(parse_size("1M").unwrap(), 1 << 20);
        assert_eq!(parse_size("2.5G").unwrap(), 2_684_354_560);
        assert_eq!(parse_size("1MB").unwrap(), 1_000_000);
        assert_eq!(parse_size("512 KiB").unwrap(), 512 << 10);
        assert!(parse_size("").is_err());
        assert!(parse_size("fast").is_err());
        assert!(parse_size("5x").is_err());
        assert!(parse_size("99999999999999999999G").is_err());
    }

    #[test]
    fn seconds_and_counts() {
        assert_eq!(parse_seconds("30").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_seconds("2.5s").unwrap(), Duration::from_millis(2500));
        assert!(parse_seconds("0").is_err());
        assert!(parse_seconds("soon").is_err());
        assert_eq!(parse_count(" 3 ").unwrap(), 3);
        assert!(parse_count("-1").is_err());
    }
}
