//! Timestamps and the conversions each on-disk format needs.
//!
//! Everything is normalised to [`Timestamp`] (seconds since the Unix epoch
//! plus nanoseconds). Conversions for FAT, NTFS, ext4 and ISO BMFF live here
//! so every parser produces identical values.

use std::fmt;

/// A point in time, UTC.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct Timestamp {
    /// Seconds since 1970-01-01T00:00:00Z (may be negative).
    pub unix_secs: i64,
    /// Nanoseconds within the second (0..1e9).
    pub nanos: u32,
}

impl Timestamp {
    /// Build from Unix seconds.
    pub fn from_unix(secs: i64) -> Self {
        Timestamp {
            unix_secs: secs,
            nanos: 0,
        }
    }

    /// Build from Unix seconds and nanoseconds.
    pub fn from_unix_nanos(secs: i64, nanos: u32) -> Self {
        Timestamp {
            unix_secs: secs,
            nanos: nanos % 1_000_000_000,
        }
    }

    /// The current time.
    pub fn now() -> Self {
        let d = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        Timestamp::from_unix_nanos(d.as_secs() as i64, d.subsec_nanos())
    }

    /// FAT date/time: `date` and `time` are the packed 16-bit fields, `tenths`
    /// is the optional 10 ms field (0..=199). Returns `None` for the all-zero
    /// "not set" value or an impossible date. FAT times are local time with
    /// no zone information; they are interpreted as UTC here.
    pub fn from_fat(date: u16, time: u16, tenths: u8) -> Option<Self> {
        if date == 0 {
            return None;
        }
        let day = (date & 0x1F) as i64;
        let month = ((date >> 5) & 0x0F) as i64;
        let year = 1980 + ((date >> 9) & 0x7F) as i64;
        let sec2 = (time & 0x1F) as i64;
        let min = ((time >> 5) & 0x3F) as i64;
        let hour = ((time >> 11) & 0x1F) as i64;
        if !(1..=12).contains(&month)
            || !(1..=31).contains(&day)
            || hour > 23
            || min > 59
            || sec2 > 29
        {
            return None;
        }
        let days = days_from_civil(year, month as u32, day as u32);
        let extra_secs = (tenths as i64) / 100;
        let nanos = ((tenths as u32) % 100) * 10_000_000;
        Some(Timestamp::from_unix_nanos(
            days * 86_400 + hour * 3600 + min * 60 + sec2 * 2 + extra_secs,
            nanos,
        ))
    }

    /// Windows FILETIME: 100 ns intervals since 1601-01-01 UTC (NTFS, exFAT
    /// uses its own encoding, see [`Timestamp::from_exfat`]).
    pub fn from_filetime(ft: u64) -> Option<Self> {
        if ft == 0 {
            return None;
        }
        const EPOCH_DIFF: i64 = 11_644_473_600; // seconds between 1601 and 1970
        let secs = (ft / 10_000_000) as i64 - EPOCH_DIFF;
        let nanos = ((ft % 10_000_000) * 100) as u32;
        Some(Timestamp::from_unix_nanos(secs, nanos))
    }

    /// exFAT timestamp: same packed layout as FAT (date in the high 16 bits,
    /// time in the low 16 bits of a 32-bit value), with a 10 ms increment
    /// field and a UTC offset byte (bit 7 = valid, bits 0..6 = signed offset in
    /// 15-minute units).
    pub fn from_exfat(packed: u32, ten_ms: u8, utc_offset: u8) -> Option<Self> {
        let date = (packed >> 16) as u16;
        let time = (packed & 0xFFFF) as u16;
        let mut ts = Timestamp::from_fat(date, time, ten_ms)?;
        if utc_offset & 0x80 != 0 {
            // Signed 7-bit value in 15 minute units; the local time stored on
            // disk is local = UTC + offset, so UTC = local - offset.
            let raw = (utc_offset & 0x7F) as i8;
            let signed = if raw & 0x40 != 0 {
                (raw | 0x80u8 as i8) as i64
            } else {
                raw as i64
            };
            ts.unix_secs -= signed * 15 * 60;
        }
        Some(ts)
    }

    /// ext4 inode time: `secs` is the 32-bit seconds field (signed), `extra`
    /// the optional `i_*time_extra` word (low 2 bits extend the epoch, the
    /// rest are nanoseconds).
    pub fn from_ext4(secs: u32, extra: Option<u32>) -> Option<Self> {
        if secs == 0 && extra.unwrap_or(0) == 0 {
            return None;
        }
        let mut s = secs as i32 as i64;
        let mut nanos = 0u32;
        if let Some(x) = extra {
            let epoch_bits = (x & 0x3) as i64;
            s += epoch_bits << 32;
            nanos = x >> 2;
        }
        Some(Timestamp::from_unix_nanos(s, nanos))
    }

    /// ISO BMFF (MP4/MOV) time: seconds since 1904-01-01 UTC.
    pub fn from_mp4(secs: u64) -> Option<Self> {
        if secs == 0 {
            return None;
        }
        const EPOCH_DIFF: i64 = 2_082_844_800;
        Some(Timestamp::from_unix(secs as i64 - EPOCH_DIFF))
    }

    /// EXIF `YYYY:MM:DD HH:MM:SS` (optionally with trailing sub-seconds or
    /// zone, which are ignored). Returns `None` for blanks or garbage.
    pub fn from_exif_str(s: &str) -> Option<Self> {
        let s = s.trim_end_matches('\0').trim();
        if s.len() < 19 {
            return None;
        }
        let b = s.as_bytes();
        let num = |a: usize, l: usize| -> Option<i64> {
            let t = std::str::from_utf8(&b[a..a + l]).ok()?;
            t.parse::<i64>().ok()
        };
        let year = num(0, 4)?;
        let month = num(5, 2)?;
        let day = num(8, 2)?;
        let hour = num(11, 2)?;
        let min = num(14, 2)?;
        let sec = num(17, 2)?;
        if !(1..=12).contains(&month)
            || !(1..=31).contains(&day)
            || hour > 23
            || min > 59
            || sec > 60
            || year < 1900
        {
            return None;
        }
        let days = days_from_civil(year, month as u32, day as u32);
        Some(Timestamp::from_unix(
            days * 86_400 + hour * 3600 + min * 60 + sec,
        ))
    }

    /// Calendar fields `(year, month, day, hour, minute, second)` in UTC.
    pub fn civil(&self) -> (i64, u32, u32, u32, u32, u32) {
        let days = self.unix_secs.div_euclid(86_400);
        let rem = self.unix_secs.rem_euclid(86_400);
        let (y, m, d) = civil_from_days(days);
        (
            y,
            m,
            d,
            (rem / 3600) as u32,
            ((rem % 3600) / 60) as u32,
            (rem % 60) as u32,
        )
    }

    /// RFC 3339 string, e.g. `2024-05-01T12:30:00Z`.
    pub fn to_rfc3339(&self) -> String {
        let (y, m, d, hh, mm, ss) = self.civil();
        format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
    }

    /// `YYYY/MM` path component used to organise recovered files by date.
    pub fn year_month_path(&self) -> String {
        let (y, m, _, _, _, _) = self.civil();
        format!("{y:04}/{m:02}")
    }

    /// Compact filename-safe form `YYYYMMDD_HHMMSS`.
    pub fn compact(&self) -> String {
        let (y, m, d, hh, mm, ss) = self.civil();
        format!("{y:04}{m:02}{d:02}_{hh:02}{mm:02}{ss:02}")
    }

    /// True when the timestamp is within a plausible range for a photo or
    /// video file (1980..=2100). Used to discard garbage metadata.
    pub fn is_plausible(&self) -> bool {
        (315_532_800..=4_102_444_800).contains(&self.unix_secs)
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_rfc3339())
    }
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's
/// algorithm).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Inverse of [`days_from_civil`].
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_roundtrip() {
        for &(y, m, d) in &[
            (1970, 1, 1),
            (2000, 2, 29),
            (2024, 12, 31),
            (1601, 1, 1),
            (1904, 1, 1),
            (2100, 3, 1),
        ] {
            let days = days_from_civil(y, m, d);
            assert_eq!(civil_from_days(days), (y, m, d));
        }
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 1, 1), 10_957);
    }

    #[test]
    fn fat_time() {
        // 2024-05-01 12:30:10 => date = (44<<9)|(5<<5)|1, time = (12<<11)|(30<<5)|5
        let date = (44u16 << 9) | (5 << 5) | 1;
        let time = (12u16 << 11) | (30 << 5) | 5;
        let ts = Timestamp::from_fat(date, time, 150).unwrap();
        assert_eq!(ts.to_rfc3339(), "2024-05-01T12:30:11Z");
        assert_eq!(ts.nanos, 500_000_000);
        assert!(Timestamp::from_fat(0, 0, 0).is_none());
        assert!(Timestamp::from_fat((44 << 9) | (13 << 5) | 1, 0, 0).is_none());
    }

    #[test]
    fn filetime_and_mp4() {
        // 2024-01-01T00:00:00Z as FILETIME: (unix 1704067200 + 11644473600) * 1e7
        let ft = (1_704_067_200u64 + 11_644_473_600) * 10_000_000 + 1_234_567;
        let ts = Timestamp::from_filetime(ft).unwrap();
        assert_eq!(ts.unix_secs, 1_704_067_200);
        assert_eq!(ts.nanos, 123_456_700);
        assert!(Timestamp::from_filetime(0).is_none());
        let mp4 = Timestamp::from_mp4(1_704_067_200 + 2_082_844_800).unwrap();
        assert_eq!(mp4.to_rfc3339(), "2024-01-01T00:00:00Z");
    }

    #[test]
    fn exfat_offset_and_ext4_extra() {
        let date = (44u16 << 9) | (5 << 5) | 1;
        let time = (12u16 << 11) | (30 << 5);
        let packed = ((date as u32) << 16) | time as u32;
        // UTC+05:30 = 22 quarter hours, valid bit set.
        let ts = Timestamp::from_exfat(packed, 0, 0x80 | 22).unwrap();
        assert_eq!(ts.to_rfc3339(), "2024-05-01T07:00:00Z");
        // Negative offset: -04:00 = -16 => 7-bit two's complement 0x70.
        let ts2 = Timestamp::from_exfat(packed, 0, 0x80 | 0x70).unwrap();
        assert_eq!(ts2.to_rfc3339(), "2024-05-01T16:30:00Z");
        let e = Timestamp::from_ext4(1_704_067_200, Some((123 << 2) | 1)).unwrap();
        assert_eq!(e.unix_secs, 1_704_067_200 + (1i64 << 32));
        assert_eq!(e.nanos, 123);
        assert!(Timestamp::from_ext4(0, None).is_none());
    }

    #[test]
    fn exif_strings() {
        let ts = Timestamp::from_exif_str("2023:07:14 09:05:59\0").unwrap();
        assert_eq!(ts.to_rfc3339(), "2023-07-14T09:05:59Z");
        assert_eq!(ts.year_month_path(), "2023/07");
        assert_eq!(ts.compact(), "20230714_090559");
        assert!(Timestamp::from_exif_str("    :  :     :  :  ").is_none());
        assert!(Timestamp::from_exif_str("0000:00:00 00:00:00").is_none());
        assert!(ts.is_plausible());
        assert!(!Timestamp::from_unix(0).is_plausible());
    }
}
