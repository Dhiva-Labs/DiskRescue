//! MPEG audio frame headers (layers I, II, III; MPEG-1, 2 and 2.5), shared by
//! the MP3 walker and the program/transport stream parsers that want to tell
//! MP2 from MP3.

/// A parsed 4-byte MPEG audio frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MpaHeader {
    /// 1 = MPEG-1, 2 = MPEG-2, 25 = MPEG-2.5.
    pub version: u8,
    /// 1, 2 or 3.
    pub layer: u8,
    /// Bitrate in bit/s.
    pub bitrate: u32,
    /// Sample rate in Hz.
    pub sample_rate: u32,
    pub padding: bool,
    pub crc_protected: bool,
    /// 1 for mono, 2 otherwise.
    pub channels: u8,
    /// Channel mode bits (0 stereo, 1 joint, 2 dual, 3 mono).
    pub mode: u8,
    /// Frame length in bytes, header included.
    pub frame_len: usize,
    /// PCM samples per channel in one frame.
    pub samples: u32,
}

const BITRATES_V1: [[u32; 14]; 3] = [
    [
        32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448,
    ],
    [
        32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384,
    ],
    [
        32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
    ],
];
const BITRATES_V2: [[u32; 14]; 3] = [
    [
        32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256,
    ],
    [8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160],
    [8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160],
];

/// Parse a frame header. `None` for anything that is not a valid header
/// (bad sync, reserved version/layer, free-format or invalid bitrate index,
/// reserved sample rate or emphasis).
pub(crate) fn parse_header(h: &[u8]) -> Option<MpaHeader> {
    if h.len() < 4 || h[0] != 0xFF || h[1] & 0xE0 != 0xE0 {
        return None;
    }
    let version = match (h[1] >> 3) & 3 {
        0 => 25,
        2 => 2,
        3 => 1,
        _ => return None,
    };
    let layer = match (h[1] >> 1) & 3 {
        1 => 3,
        2 => 2,
        3 => 1,
        _ => return None,
    };
    let br_idx = usize::from(h[2] >> 4);
    if br_idx == 0 || br_idx == 15 {
        return None;
    }
    let sr_idx = usize::from((h[2] >> 2) & 3);
    if sr_idx == 3 {
        return None;
    }
    if h[3] & 3 == 2 {
        return None; // reserved emphasis
    }
    let sample_rate = match (version, sr_idx) {
        (1, 0) => 44_100,
        (1, 1) => 48_000,
        (1, _) => 32_000,
        (2, 0) => 22_050,
        (2, 1) => 24_000,
        (2, _) => 16_000,
        (_, 0) => 11_025,
        (_, 1) => 12_000,
        (_, _) => 8_000,
    };
    let table = if version == 1 {
        &BITRATES_V1
    } else {
        &BITRATES_V2
    };
    let bitrate = table[layer as usize - 1][br_idx - 1] * 1000;
    let padding = (h[2] >> 1) & 1 == 1;
    let (frame_len, samples) = match (layer, version) {
        (1, _) => (
            (12 * bitrate / sample_rate + u32::from(padding)) as usize * 4,
            384,
        ),
        (2, _) => (
            (144 * bitrate / sample_rate + u32::from(padding)) as usize,
            1152,
        ),
        (_, 1) => (
            (144 * bitrate / sample_rate + u32::from(padding)) as usize,
            1152,
        ),
        _ => (
            (72 * bitrate / sample_rate + u32::from(padding)) as usize,
            576,
        ),
    };
    let mode = h[3] >> 6;
    Some(MpaHeader {
        version,
        layer,
        bitrate,
        sample_rate,
        padding,
        crc_protected: h[1] & 1 == 0,
        channels: if mode == 3 { 1 } else { 2 },
        mode,
        frame_len,
        samples,
    })
}

impl MpaHeader {
    /// Frames in one stream must agree on these.
    pub fn same_stream(&self, other: &MpaHeader) -> bool {
        self.version == other.version
            && self.layer == other.layer
            && self.sample_rate == other.sample_rate
    }

    /// Codec name (`mp1`, `mp2`, `mp3`).
    pub fn codec(&self) -> &'static str {
        match self.layer {
            1 => "mp1",
            2 => "mp2",
            _ => "mp3",
        }
    }

    /// Side information size (for locating Xing/Info/VBRI).
    pub fn side_info_len(&self) -> usize {
        match (self.version, self.channels) {
            (1, 1) => 17,
            (1, _) => 32,
            (_, 1) => 9,
            (_, _) => 17,
        }
    }
}

/// Find the first run of two consecutive valid, consistent frames in `data`
/// and return the first header. Used to name the audio codec of a PES.
pub(crate) fn find_stream(data: &[u8]) -> Option<MpaHeader> {
    let mut i = 0;
    while i + 4 <= data.len() {
        let rel = data[i..].iter().position(|&b| b == 0xFF)?;
        let p = i + rel;
        if let Some(h) = data.get(p..p + 4).and_then(parse_header) {
            match data
                .get(p + h.frame_len..p + h.frame_len + 4)
                .and_then(parse_header)
            {
                Some(n) if n.same_stream(&h) => return Some(h),
                None if p + h.frame_len + 4 > data.len() && p == 0 => return Some(h),
                _ => {}
            }
        }
        i = p + 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typical_mp3_header() {
        // 0xFF 0xFB 0x90 0x64: MPEG-1 layer III, 128 kbit/s, 44.1 kHz, joint stereo
        let h = parse_header(&[0xFF, 0xFB, 0x90, 0x64]).unwrap();
        assert_eq!((h.version, h.layer), (1, 3));
        assert_eq!(h.bitrate, 128_000);
        assert_eq!(h.sample_rate, 44_100);
        assert_eq!(h.frame_len, 417);
        assert_eq!(h.samples, 1152);
        assert_eq!(h.channels, 2);
        assert_eq!(h.codec(), "mp3");
        let padded = parse_header(&[0xFF, 0xFB, 0x92, 0x64]).unwrap();
        assert_eq!(padded.frame_len, 418);
    }

    #[test]
    fn other_versions_and_layers() {
        // MPEG-2 layer III, 64 kbit/s, 22.05 kHz mono
        let h = parse_header(&[0xFF, 0xF3, 0x80, 0xC4]).unwrap();
        assert_eq!((h.version, h.layer, h.channels), (2, 3, 1));
        assert_eq!(h.sample_rate, 22_050);
        assert_eq!(h.samples, 576);
        assert_eq!(h.frame_len, 72 * 64_000 / 22_050);
        // MPEG-1 layer II, 192 kbit/s, 48 kHz
        let h = parse_header(&[0xFF, 0xFD, 0xA4, 0x00]).unwrap();
        assert_eq!((h.version, h.layer, h.bitrate), (1, 2, 192_000));
        assert_eq!(h.codec(), "mp2");
        assert_eq!(h.frame_len, 144 * 192_000 / 48_000);
        // MPEG-1 layer I
        let h = parse_header(&[0xFF, 0xFF, 0x50, 0x00]).unwrap();
        assert_eq!((h.layer, h.samples), (1, 384));
    }

    #[test]
    fn invalid_headers() {
        assert!(parse_header(&[0xFF, 0xFB, 0x00, 0x00]).is_none()); // free format
        assert!(parse_header(&[0xFF, 0xFB, 0xF0, 0x00]).is_none()); // bitrate 15
        assert!(parse_header(&[0xFF, 0xFB, 0x9C, 0x00]).is_none()); // sample rate 3
        assert!(parse_header(&[0xFF, 0xE9, 0x90, 0x00]).is_none()); // reserved version
        assert!(parse_header(&[0xFF, 0xF9, 0x90, 0x00]).is_none()); // reserved layer
        assert!(parse_header(&[0xFF, 0xFB, 0x90, 0x02]).is_none()); // emphasis
        assert!(parse_header(&[0xFF, 0xFF, 0xFF, 0xFF]).is_none());
        assert!(parse_header(&[0xFE, 0xFB, 0x90, 0x00]).is_none());
        assert!(parse_header(&[0xFF]).is_none());
    }
}
