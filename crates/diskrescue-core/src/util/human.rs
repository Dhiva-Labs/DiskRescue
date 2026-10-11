//! Human-readable formatting.

use std::time::Duration;

/// Bytes as `1.50 GiB`-style text (binary units).
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 7] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if v >= 100.0 {
        format!("{v:.0} {}", UNITS[i])
    } else if v >= 10.0 {
        format!("{v:.1} {}", UNITS[i])
    } else {
        format!("{v:.2} {}", UNITS[i])
    }
}

/// Bytes as decimal `1.50 GB` text, matching what vendors print on labels.
pub fn bytes_decimal(n: u64) -> String {
    const UNITS: [&str; 7] = ["B", "kB", "MB", "GB", "TB", "PB", "EB"];
    if n < 1000 {
        return format!("{n} B");
    }
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1000.0 && i < UNITS.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if v >= 100.0 {
        format!("{v:.0} {}", UNITS[i])
    } else if v >= 10.0 {
        format!("{v:.1} {}", UNITS[i])
    } else {
        format!("{v:.2} {}", UNITS[i])
    }
}

/// Rate as `12.3 MiB/s`.
pub fn rate(bytes_per_sec: f64) -> String {
    if bytes_per_sec <= 0.0 || !bytes_per_sec.is_finite() {
        return "0 B/s".into();
    }
    format!("{}/s", bytes(bytes_per_sec as u64))
}

/// Duration as `1h 02m 03s`, `4m 05s` or `12s`.
pub fn duration(d: Duration) -> String {
    let s = d.as_secs();
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}h {m:02}m {sec:02}s")
    } else if m > 0 {
        format!("{m}m {sec:02}s")
    } else {
        format!("{sec}s")
    }
}

/// Count with thousands separators.
pub fn count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(1023), "1023 B");
        assert_eq!(bytes(1024), "1.00 KiB");
        assert_eq!(bytes(1536), "1.50 KiB");
        assert_eq!(bytes(10 * 1024 * 1024), "10.0 MiB");
        assert_eq!(bytes(500 * 1024 * 1024 * 1024), "500 GiB");
        assert_eq!(bytes_decimal(500_107_862_016), "500 GB");
        assert_eq!(rate(2.5 * 1024.0 * 1024.0), "2.50 MiB/s");
        assert_eq!(duration(Duration::from_secs(3723)), "1h 02m 03s");
        assert_eq!(duration(Duration::from_secs(245)), "4m 05s");
        assert_eq!(duration(Duration::from_secs(12)), "12s");
        assert_eq!(count(1234567), "1,234,567");
        assert_eq!(count(12), "12");
    }
}
