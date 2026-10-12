//! Metadata helpers shared by the container parsers: loose date parsing,
//! checksums and the few elementary-stream headers (MPEG-1/2 video sequence
//! header, H.264 and HEVC SPS) that give a container its picture size when
//! the container itself does not store one.

use diskrescue_core::time::{days_from_civil, Timestamp};
use diskrescue_core::util::bits::{unescape_nal, BitReader};

/// Key under which the per-file extension (`webm`, `ts`, `vob`, ...) is
/// stored in [`diskrescue_core::media::MediaMeta::extra`]. The carver uses it
/// instead of the format's static extension list when present.
pub const EXTENSION_KEY: &str = "extension";

/// Parse the date strings found in RIFF INFO, IDIT, Vorbis comments and
/// similar free-form fields. Accepts `YYYY-MM-DD`, `YYYY-MM-DD[T ]HH:MM:SS`
/// (with optional fraction or zone suffix), `YYYY:MM:DD HH:MM:SS` and the
/// `Www Mmm DD HH:MM:SS YYYY` form of AVI `IDIT`. A bare year or anything
/// implausible gives `None`.
pub fn parse_loose_date(s: &str) -> Option<Timestamp> {
    let s = s.trim_matches(|c: char| c == '\0' || c.is_whitespace());
    if s.len() < 8 {
        return None;
    }
    let ts = parse_numeric_date(s).or_else(|| parse_ctime_date(s))?;
    ts.is_plausible().then_some(ts)
}

fn parse_numeric_date(s: &str) -> Option<Timestamp> {
    let b = s.as_bytes();
    let num = |a: usize, l: usize| -> Option<i64> {
        let t = s.get(a..a + l)?;
        if !t.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        t.parse().ok()
    };
    let year = num(0, 4)?;
    let sep = *b.get(4)?;
    if sep != b'-' && sep != b':' && sep != b'/' {
        return None;
    }
    let month = num(5, 2)?;
    let day = num(8, 2)?;
    let (mut hh, mut mm, mut ss) = (0, 0, 0);
    if b.len() >= 19 && matches!(b.get(10), Some(b' ') | Some(b'T')) {
        hh = num(11, 2)?;
        mm = num(14, 2)?;
        ss = num(17, 2)?;
    } else if b.len() > 10 && !matches!(b.get(10), Some(b' ') | Some(b'T')) {
        return None;
    }
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    let days = days_from_civil(year, month as u32, day as u32);
    Some(Timestamp::from_unix(
        days * 86_400 + hh * 3600 + mm * 60 + ss,
    ))
}

fn parse_ctime_date(s: &str) -> Option<Timestamp> {
    // "Wed Jan 02 02:03:55 2008"
    let mut it = s.split_whitespace();
    let _weekday = it.next()?;
    let mon = it.next()?;
    let day: i64 = it.next()?.parse().ok()?;
    let time = it.next()?;
    let year: i64 = it.next()?.parse().ok()?;
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let m = MONTHS
        .iter()
        .position(|&x| mon.len() >= 3 && mon[..3].eq_ignore_ascii_case(x))? as u32
        + 1;
    let mut t = time.split(':');
    let hh: i64 = t.next()?.parse().ok()?;
    let mm: i64 = t.next()?.parse().ok()?;
    let ss: i64 = t.next()?.parse().ok()?;
    if !(1..=31).contains(&day) || hh > 23 || mm > 59 || ss > 60 || !(1980..=2200).contains(&year) {
        return None;
    }
    Some(Timestamp::from_unix(
        days_from_civil(year, m, day as u32) * 86_400 + hh * 3600 + mm * 60 + ss,
    ))
}

/// Trim a NUL-terminated or padded byte field into a clean string.
pub fn clean_string(b: &[u8]) -> Option<String> {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    let s = String::from_utf8_lossy(&b[..end]);
    let s = s.trim();
    if s.is_empty() {
        None
    } else {
        Some(s.chars().take(256).collect())
    }
}

// ---------------------------------------------------------------------------
// Checksums
// ---------------------------------------------------------------------------

/// CRC-8 with polynomial 0x07, initial value 0 (FLAC frame headers).
pub fn crc8(data: &[u8]) -> u8 {
    let mut crc = 0u8;
    for &b in data {
        crc ^= b;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 {
                (crc << 1) ^ 0x07
            } else {
                crc << 1
            };
        }
    }
    crc
}

const fn make_crc16_table() -> [[u16; 256]; 4] {
    let mut t = [[0u16; 256]; 4];
    let mut i = 0;
    while i < 256 {
        let mut c = (i as u16) << 8;
        let mut k = 0;
        while k < 8 {
            c = if c & 0x8000 != 0 {
                (c << 1) ^ 0x8005
            } else {
                c << 1
            };
            k += 1;
        }
        t[0][i] = c;
        i += 1;
    }
    // Slicing-by-4 tables: t[n][i] is the CRC of byte i followed by n zero
    // bytes.
    let mut n = 1;
    while n < 4 {
        let mut i = 0;
        while i < 256 {
            let prev = t[n - 1][i];
            t[n][i] = (prev << 8) ^ t[0][(prev >> 8) as usize];
            i += 1;
        }
        n += 1;
    }
    t
}

static CRC16: [[u16; 256]; 4] = make_crc16_table();

/// Running CRC-16 with polynomial 0x8005, no reflection, initial value 0
/// (FLAC frame footers). Pass the previous result as `crc` to continue.
pub fn crc16_update(mut crc: u16, data: &[u8]) -> u16 {
    let mut chunks = data.chunks_exact(4);
    for c in &mut chunks {
        let x = crc ^ u16::from_be_bytes([c[0], c[1]]);
        crc = CRC16[3][(x >> 8) as usize]
            ^ CRC16[2][(x & 0xFF) as usize]
            ^ CRC16[1][c[2] as usize]
            ^ CRC16[0][c[3] as usize];
    }
    for &b in chunks.remainder() {
        crc = (crc << 8) ^ CRC16[0][((crc >> 8) as u8 ^ b) as usize];
    }
    crc
}

/// CRC-32/MPEG-2 (polynomial 0x04C11DB7, initial value 0xFFFFFFFF, no
/// reflection, no final XOR) as used by PAT/PMT sections. The CRC of a
/// section including its trailing CRC field is zero.
pub fn crc32_mpeg(data: &[u8]) -> u32 {
    static TABLE: [u32; 256] = {
        let mut t = [0u32; 256];
        let mut i = 0;
        while i < 256 {
            let mut c = (i as u32) << 24;
            let mut k = 0;
            while k < 8 {
                c = if c & 0x8000_0000 != 0 {
                    (c << 1) ^ 0x04C1_1DB7
                } else {
                    c << 1
                };
                k += 1;
            }
            t[i] = c;
            i += 1;
        }
        t
    };
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc = (crc << 8) ^ TABLE[((crc >> 24) as u8 ^ b) as usize];
    }
    crc
}

// ---------------------------------------------------------------------------
// Elementary stream headers
// ---------------------------------------------------------------------------

/// Picture parameters from an elementary-stream header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PictureInfo {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

/// Frame rate in frames per second for an MPEG-1/2 `frame_rate_code`.
pub fn mpeg_frame_rate(code: u8) -> Option<f64> {
    Some(match code {
        1 => 24000.0 / 1001.0,
        2 => 24.0,
        3 => 25.0,
        4 => 30000.0 / 1001.0,
        5 => 30.0,
        6 => 50.0,
        7 => 60000.0 / 1001.0,
        8 => 60.0,
        _ => return None,
    })
}

/// What an MPEG-1/2 video sequence header says.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MpegSequence {
    /// Coded picture size.
    pub picture: PictureInfo,
    /// `frame_rate_code` (1..=8).
    pub rate_code: u8,
    /// A sequence extension follows: the stream is MPEG-2 video.
    pub mpeg2: bool,
}

/// Find the first MPEG-1/2 video sequence header (`00 00 01 B3`) in `data`.
pub fn find_mpeg_sequence_header(data: &[u8]) -> Option<MpegSequence> {
    let mut i = 0usize;
    while i + 12 <= data.len() {
        let rel = data[i..].windows(4).position(|w| w == [0, 0, 1, 0xB3])?;
        let p = i + rel;
        let h = data.get(p + 4..p + 12)?;
        let width = (u32::from(h[0]) << 4) | u32::from(h[1] >> 4);
        let height = (u32::from(h[1] & 0x0F) << 8) | u32::from(h[2]);
        let rate = h[3] & 0x0F;
        if (16..=4096).contains(&width) && (16..=4096).contains(&height) && rate != 0 && rate <= 8 {
            // The sequence extension follows at once, or after quantiser
            // matrices of 64 bytes each.
            let after = data.get(p + 12..).unwrap_or(&[]);
            let window = &after[..after.len().min(140)];
            let mpeg2 = window
                .windows(5)
                .any(|w| w[..4] == [0, 0, 1, 0xB5] && w[4] >> 4 == 1);
            return Some(MpegSequence {
                picture: PictureInfo { width, height },
                rate_code: rate,
                mpeg2,
            });
        }
        i = p + 1;
    }
    None
}

/// Iterate over the NAL units of an Annex B byte stream: returns the byte
/// ranges of each NAL unit (after the start code), at most `max` of them.
pub fn annexb_nals(data: &[u8], max: usize) -> Vec<(usize, usize)> {
    let mut starts: Vec<usize> = Vec::new();
    let mut i = 0usize;
    while i + 3 <= data.len() && starts.len() < max + 1 {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::new();
    for (k, &s) in starts.iter().enumerate() {
        let mut e = starts.get(k + 1).map_or(data.len(), |&n| n - 3);
        // Trailing zero bytes belong to the next start code or padding.
        while e > s && data[e - 1] == 0 {
            e -= 1;
        }
        if e > s {
            out.push((s, e));
        }
        if out.len() >= max {
            break;
        }
    }
    out
}

fn skip_scaling_list(r: &mut BitReader<'_>, size: usize) -> Option<()> {
    let mut last = 8i32;
    let mut next = 8i32;
    for _ in 0..size {
        if next != 0 {
            let delta = r.read_se()?;
            next = (last + delta + 256) % 256;
        }
        last = if next == 0 { last } else { next };
    }
    Some(())
}

/// Parse the picture size out of an H.264 SPS NAL unit (including its NAL
/// header byte).
pub fn parse_h264_sps(nal: &[u8]) -> Option<PictureInfo> {
    if nal.len() < 5 || nal[0] & 0x1F != 7 {
        return None;
    }
    let rbsp = unescape_nal(&nal[1..]);
    let mut r = BitReader::new(&rbsp);
    let profile = r.read_bits(8)?;
    r.read_bits(8)?; // constraint flags + reserved
    r.read_bits(8)?; // level
    r.read_ue()?; // sps id
    let mut chroma_format = 1u32;
    if matches!(
        profile,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        chroma_format = r.read_ue()?;
        if chroma_format == 3 {
            r.read_flag()?;
        }
        r.read_ue()?;
        r.read_ue()?;
        r.read_flag()?;
        if r.read_flag()? {
            let lists = if chroma_format != 3 { 8 } else { 12 };
            for i in 0..lists {
                if r.read_flag()? {
                    skip_scaling_list(&mut r, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    r.read_ue()?; // log2_max_frame_num_minus4
    let poc_type = r.read_ue()?;
    if poc_type == 0 {
        r.read_ue()?;
    } else if poc_type == 1 {
        r.read_flag()?;
        r.read_se()?;
        r.read_se()?;
        let n = r.read_ue()?;
        if n > 255 {
            return None;
        }
        for _ in 0..n {
            r.read_se()?;
        }
    } else if poc_type != 2 {
        return None;
    }
    r.read_ue()?; // max_num_ref_frames
    r.read_flag()?; // gaps
    let w_mbs = r.read_ue()?.checked_add(1)?;
    let h_map = r.read_ue()?.checked_add(1)?;
    let frame_mbs_only = r.read_flag()?;
    if !frame_mbs_only {
        r.read_flag()?; // mb_adaptive_frame_field
    }
    r.read_flag()?; // direct_8x8
    let (mut cl, mut cr, mut ct, mut cb) = (0u32, 0u32, 0u32, 0u32);
    if r.read_flag()? {
        cl = r.read_ue()?;
        cr = r.read_ue()?;
        ct = r.read_ue()?;
        cb = r.read_ue()?;
    }
    let (sub_w, sub_h) = match chroma_format {
        0 => (1, 1),
        1 => (2, 2),
        2 => (2, 1),
        _ => (1, 1),
    };
    let crop_unit_x = sub_w;
    let crop_unit_y = sub_h * if frame_mbs_only { 1 } else { 2 };
    let width = w_mbs
        .checked_mul(16)?
        .checked_sub(crop_unit_x * (cl + cr))?;
    let height = h_map
        .checked_mul(16)?
        .checked_mul(if frame_mbs_only { 1 } else { 2 })?
        .checked_sub(crop_unit_y * (ct + cb))?;
    if width == 0 || height == 0 || width > 16384 || height > 16384 {
        return None;
    }
    Some(PictureInfo { width, height })
}

/// Parse the picture size out of an HEVC SPS NAL unit (including its two
/// byte NAL header).
pub fn parse_hevc_sps(nal: &[u8]) -> Option<PictureInfo> {
    if nal.len() < 6 || (nal[0] >> 1) & 0x3F != 33 {
        return None;
    }
    let rbsp = unescape_nal(&nal[2..]);
    let mut r = BitReader::new(&rbsp);
    r.read_bits(4)?; // vps id
    let max_sub_layers_minus1 = r.read_bits(3)?;
    r.read_bits(1)?; // temporal id nesting
                     // profile_tier_level(1, max_sub_layers_minus1)
    r.skip(2 + 1 + 5 + 32 + 4 + 43 + 1)?; // general profile fields
    r.skip(8)?; // general_level_idc
    let mut profile_present = [false; 8];
    let mut level_present = [false; 8];
    for i in 0..max_sub_layers_minus1 as usize {
        profile_present[i] = r.read_flag()?;
        level_present[i] = r.read_flag()?;
    }
    if max_sub_layers_minus1 > 0 {
        for _ in max_sub_layers_minus1..8 {
            r.skip(2)?;
        }
    }
    for i in 0..max_sub_layers_minus1 as usize {
        if profile_present[i] {
            r.skip(88)?;
        }
        if level_present[i] {
            r.skip(8)?;
        }
    }
    r.read_ue()?; // sps id
    let chroma = r.read_ue()?;
    if chroma == 3 {
        r.read_flag()?;
    }
    let mut width = r.read_ue()?;
    let mut height = r.read_ue()?;
    if r.read_flag()? {
        let l = r.read_ue()?;
        let rr = r.read_ue()?;
        let t = r.read_ue()?;
        let b = r.read_ue()?;
        let (sx, sy) = match chroma {
            1 => (2u32, 2u32),
            2 => (2, 1),
            _ => (1, 1),
        };
        width = width.checked_sub(sx.checked_mul(l.checked_add(rr)?)?)?;
        height = height.checked_sub(sy.checked_mul(t.checked_add(b)?)?)?;
    }
    if width == 0 || height == 0 || width > 16384 || height > 16384 {
        return None;
    }
    Some(PictureInfo { width, height })
}

/// Look through an Annex B elementary stream prefix for the first SPS of the
/// given codec and return its picture size.
pub fn picture_from_annexb(data: &[u8], hevc: bool) -> Option<PictureInfo> {
    for (s, e) in annexb_nals(data, 64) {
        let nal = &data[s..e];
        if hevc {
            if let Some(p) = parse_hevc_sps(nal) {
                return Some(p);
            }
        } else if let Some(p) = parse_h264_sps(nal) {
            return Some(p);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loose_dates() {
        let t = parse_loose_date("2024-05-01").unwrap();
        assert_eq!(t.to_rfc3339(), "2024-05-01T00:00:00Z");
        let t = parse_loose_date("2023-07-14T09:05:59Z").unwrap();
        assert_eq!(t.to_rfc3339(), "2023-07-14T09:05:59Z");
        let t = parse_loose_date("2023:07:14 09:05:59").unwrap();
        assert_eq!(t.to_rfc3339(), "2023-07-14T09:05:59Z");
        let t = parse_loose_date("Wed Jan 02 02:03:55 2008\n\0").unwrap();
        assert_eq!(t.to_rfc3339(), "2008-01-02T02:03:55Z");
        assert!(parse_loose_date("2020").is_none());
        assert!(parse_loose_date("0000-00-00").is_none());
        assert!(parse_loose_date("garbage text here").is_none());
        assert!(parse_loose_date("1960-01-01").is_none());
    }

    #[test]
    fn checksums() {
        // FLAC header CRC-8 of the classic 'FF F8 69 18 00' prefix is arbitrary;
        // check the algorithms against well known check values instead.
        assert_eq!(crc8(b"123456789"), 0xF4);
        assert_eq!(crc16_update(0, b"123456789"), 0xFEE8);
        let mut c = crc16_update(0, b"1234");
        c = crc16_update(c, b"56789");
        assert_eq!(c, 0xFEE8);
        assert_eq!(crc32_mpeg(b"123456789"), 0x0376_E6E7);
    }

    #[test]
    fn crc16_slicing_matches_bytewise() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i * 31 % 251) as u8).collect();
        let mut slow = 0u16;
        for &b in &data {
            slow = (slow << 8) ^ CRC16[0][((slow >> 8) as u8 ^ b) as usize];
        }
        assert_eq!(crc16_update(0, &data), slow);
    }

    #[test]
    fn mpeg_sequence_header() {
        // 00 00 01 B3, 320x240, aspect 1, rate code 3 (25 fps)
        let mut d = vec![0xAA, 0xBB, 0, 0, 1, 0xB3];
        d.extend_from_slice(&[0x14, 0x00, 0xF0, 0x13, 0xFF, 0xFF, 0xE0, 0x18]);
        let s = find_mpeg_sequence_header(&d).unwrap();
        assert_eq!(
            (s.picture.width, s.picture.height, s.rate_code),
            (320, 240, 3)
        );
        assert!(!s.mpeg2);
        d.extend_from_slice(&[0, 0, 1, 0xB5, 0x14, 0x8A]);
        assert!(find_mpeg_sequence_header(&d).unwrap().mpeg2);
        assert!(find_mpeg_sequence_header(&[0, 0, 1, 0xB3, 0, 0]).is_none());
    }

    // SPS NAL units written by x264/x265 for synthetic test sources.
    const SPS_320X240: [u8; 22] = [
        0x67, 0x42, 0xC0, 0x0D, 0xDA, 0x05, 0x07, 0xEC, 0x04, 0x40, 0x00, 0x00, 0x03, 0x00, 0x40,
        0x00, 0x00, 0x0F, 0x03, 0xC5, 0x0A, 0xA8,
    ];
    const SPS_1080P: [u8; 25] = [
        0x67, 0x42, 0xC0, 0x28, 0xDA, 0x01, 0xE0, 0x08, 0x9F, 0x97, 0x01, 0x10, 0x00, 0x00, 0x03,
        0x00, 0x10, 0x00, 0x00, 0x03, 0x03, 0xC0, 0xF1, 0x83, 0x2A,
    ];
    const HEVC_SPS_320X240: [u8; 42] = [
        0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00, 0x03, 0x00, 0x00,
        0x03, 0x00, 0x3C, 0xA0, 0x0A, 0x08, 0x0F, 0x16, 0x59, 0x59, 0xA4, 0x93, 0x2B, 0xC0, 0x5A,
        0x02, 0x00, 0x00, 0x03, 0x00, 0x02, 0x00, 0x00, 0x03, 0x00, 0x3C, 0x10,
    ];

    #[test]
    fn h264_sps_sizes_and_cropping() {
        let p = parse_h264_sps(&SPS_320X240).unwrap();
        assert_eq!((p.width, p.height), (320, 240));
        // 1920x1088 coded, cropped to 1080 lines.
        let p = parse_h264_sps(&SPS_1080P).unwrap();
        assert_eq!((p.width, p.height), (1920, 1080));
        assert!(parse_h264_sps(&SPS_320X240[..6]).is_none());
        assert!(parse_h264_sps(&[0x68, 1, 2, 3, 4, 5]).is_none());
        assert!(parse_h264_sps(&[]).is_none());
    }

    #[test]
    fn hevc_sps_size() {
        let p = parse_hevc_sps(&HEVC_SPS_320X240).unwrap();
        assert_eq!((p.width, p.height), (320, 240));
        assert!(parse_hevc_sps(&HEVC_SPS_320X240[..8]).is_none());
        assert!(parse_hevc_sps(&SPS_320X240).is_none());
    }

    #[test]
    fn annexb_extraction() {
        let mut es = vec![0, 0, 0, 1, 0x09, 0xF0, 0, 0, 1];
        es.extend_from_slice(&SPS_320X240);
        es.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xCE, 0x3C, 0x80]);
        assert_eq!(picture_from_annexb(&es, false).unwrap().width, 320);
        assert!(picture_from_annexb(&es, true).is_none());
        assert!(picture_from_annexb(&[], false).is_none());
        let mut h = vec![0, 0, 1];
        h.extend_from_slice(&HEVC_SPS_320X240);
        assert_eq!(picture_from_annexb(&h, true).unwrap().height, 240);
    }
}
