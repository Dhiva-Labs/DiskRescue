//! AVI header structures: `LIST hdrl` (avih, strl: strh/strf/indx, odml/dmlh)
//! and `LIST INFO`, plus codec naming.
//!
//! The parser records the file offsets of every field a repair may need to
//! patch (frame counts, stream lengths), so repairs never have to re-parse.

use diskrescue_core::util::bytes;

use crate::io::{fourcc_string, Window};
use crate::meta::{clean_string, parse_loose_date};

/// Hard bounds that keep a hostile `hdrl` from costing more than a little time.
const MAX_HDRL_CHILDREN: usize = 512;
const MAX_STREAMS: usize = 64;
const MAX_STRL_CHILDREN: usize = 64;

/// `avih`: the main AVI header.
#[derive(Debug, Clone, Default)]
pub(crate) struct Avih {
    /// File offset of the chunk data.
    pub pos: u64,
    pub usec_per_frame: u32,
    pub flags: u32,
    pub total_frames: u32,
    pub width: u32,
    pub height: u32,
}

/// What the `strf` chunk says about a stream.
#[derive(Debug, Clone)]
pub(crate) enum StreamFormat {
    Video {
        width: i32,
        height: i32,
        compression: [u8; 4],
    },
    Audio {
        tag: u16,
        channels: u16,
        sample_rate: u32,
        bits: u16,
    },
    Other,
}

/// One `strl` list.
#[derive(Debug, Clone)]
pub(crate) struct StreamHdr {
    /// File offset of the `strh` chunk data.
    pub strh_pos: u64,
    pub fcc_type: [u8; 4],
    pub handler: [u8; 4],
    pub scale: u32,
    pub rate: u32,
    pub length: u32,
    pub sample_size: u32,
    pub format: StreamFormat,
    /// File offset of an `indx` chunk header (OpenDML super index), if any.
    pub indx_pos: Option<u64>,
}

impl StreamHdr {
    pub fn is_video(&self) -> bool {
        &self.fcc_type == b"vids"
    }
    pub fn is_audio(&self) -> bool {
        &self.fcc_type == b"auds"
    }
}

/// Parsed `LIST hdrl`.
#[derive(Debug, Clone, Default)]
pub(crate) struct Hdrl {
    /// File offset of the `LIST` header.
    pub list_pos: u64,
    pub avih: Option<Avih>,
    pub streams: Vec<StreamHdr>,
    /// `dmlh` total frame count: (file offset of the value, value).
    pub dmlh: Option<(u64, u32)>,
    /// `IDIT` digitisation time string, when present.
    pub idit: Option<String>,
}

/// Strings from `LIST INFO`.
#[derive(Debug, Clone, Default)]
pub(crate) struct Info {
    pub software: Option<String>,
    pub created: Option<String>,
    pub title: Option<String>,
}

/// Header of one RIFF chunk.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ChunkHdr {
    pub id: [u8; 4],
    pub size: u32,
    /// File offset of the chunk header.
    pub pos: u64,
}

impl ChunkHdr {
    /// File offset of the chunk data.
    pub fn data(&self) -> u64 {
        self.pos + 8
    }
    /// File offset just past the (even-padded) chunk, saturating.
    pub fn next(&self) -> u64 {
        self.pos
            .saturating_add(8)
            .saturating_add(u64::from(self.size))
            .saturating_add(u64::from(self.size & 1))
    }
}

/// Read a chunk header at `pos`.
pub(crate) fn read_chunk(win: &mut Window<'_>, pos: u64) -> Option<ChunkHdr> {
    let s = win.exact(pos, 8)?;
    Some(ChunkHdr {
        id: [s[0], s[1], s[2], s[3]],
        size: bytes::u32le(s, 4)?,
        pos,
    })
}

/// Parse `LIST hdrl` whose `LIST` header is at `list_pos` and whose payload
/// ends at `end`.
pub(crate) fn parse_hdrl(win: &mut Window<'_>, list_pos: u64, end: u64) -> Hdrl {
    let mut h = Hdrl {
        list_pos,
        ..Hdrl::default()
    };
    let end = end.min(win.avail());
    let mut pos = list_pos + 12;
    let mut n = 0;
    while pos + 8 <= end && n < MAX_HDRL_CHILDREN {
        n += 1;
        let Some(c) = read_chunk(win, pos) else { break };
        let c_end = c.pos + 8 + u64::from(c.size);
        if c_end > end {
            break;
        }
        match &c.id {
            b"avih" if c.size >= 40 => {
                if let Some(d) = win.vec(c.data(), 40) {
                    h.avih = Some(Avih {
                        pos: c.data(),
                        usec_per_frame: bytes::u32le(&d, 0).unwrap_or(0),
                        flags: bytes::u32le(&d, 12).unwrap_or(0),
                        total_frames: bytes::u32le(&d, 16).unwrap_or(0),
                        width: bytes::u32le(&d, 32).unwrap_or(0),
                        height: bytes::u32le(&d, 36).unwrap_or(0),
                    });
                }
            }
            b"LIST" => match win.fourcc(c.data()) {
                Some(t) if &t == b"strl" => {
                    if h.streams.len() < MAX_STREAMS {
                        if let Some(s) = parse_strl(win, c.pos, c_end) {
                            h.streams.push(s);
                        }
                    }
                }
                Some(t) if &t == b"odml" => {
                    let mut p = c.pos + 12;
                    let mut k = 0;
                    while p + 8 <= c_end && k < 8 {
                        k += 1;
                        let Some(d) = read_chunk(win, p) else { break };
                        if &d.id == b"dmlh" && d.size >= 4 {
                            if let Some(v) = win.u32le(d.data()) {
                                h.dmlh = Some((d.data(), v));
                            }
                        }
                        p = d.next();
                    }
                }
                _ => {}
            },
            b"IDIT" => {
                let len = (c.size as usize).min(64);
                if let Some(d) = win.vec(c.data(), len) {
                    h.idit = clean_string(&d);
                }
            }
            _ => {}
        }
        pos = c.next();
    }
    h
}

fn parse_strl(win: &mut Window<'_>, list_pos: u64, end: u64) -> Option<StreamHdr> {
    let mut strh: Option<StreamHdr> = None;
    let mut fmt = StreamFormat::Other;
    let mut indx_pos = None;
    let mut pos = list_pos + 12;
    let mut n = 0;
    while pos + 8 <= end && n < MAX_STRL_CHILDREN {
        n += 1;
        let c = read_chunk(win, pos)?;
        let c_end = c.pos + 8 + u64::from(c.size);
        if c_end > end {
            break;
        }
        match &c.id {
            b"strh" if c.size >= 48 => {
                let d = win.vec(c.data(), 48)?;
                strh = Some(StreamHdr {
                    strh_pos: c.data(),
                    fcc_type: [d[0], d[1], d[2], d[3]],
                    handler: [d[4], d[5], d[6], d[7]],
                    scale: bytes::u32le(&d, 20)?,
                    rate: bytes::u32le(&d, 24)?,
                    length: bytes::u32le(&d, 32)?,
                    sample_size: bytes::u32le(&d, 44)?,
                    format: StreamFormat::Other,
                    indx_pos: None,
                });
            }
            b"strf" => {
                let len = (c.size as usize).min(64);
                if let Some(d) = win.vec(c.data(), len) {
                    let is_video = strh.as_ref().is_some_and(|s| s.is_video());
                    let is_audio = strh.as_ref().is_some_and(|s| s.is_audio());
                    if is_video && d.len() >= 20 {
                        fmt = StreamFormat::Video {
                            width: bytes::i32le(&d, 4).unwrap_or(0),
                            height: bytes::i32le(&d, 8).unwrap_or(0),
                            compression: [d[16], d[17], d[18], d[19]],
                        };
                    } else if is_audio && d.len() >= 16 {
                        fmt = StreamFormat::Audio {
                            tag: bytes::u16le(&d, 0).unwrap_or(0),
                            channels: bytes::u16le(&d, 2).unwrap_or(0),
                            sample_rate: bytes::u32le(&d, 4).unwrap_or(0),
                            bits: bytes::u16le(&d, 14).unwrap_or(0),
                        };
                        // WAVE_FORMAT_EXTENSIBLE: the real tag is the first
                        // two bytes of the SubFormat GUID.
                        if let StreamFormat::Audio { tag, .. } = &mut fmt {
                            if *tag == 0xFFFE && d.len() >= 26 {
                                *tag = bytes::u16le(&d, 24).unwrap_or(0xFFFE);
                            }
                        }
                    }
                }
            }
            b"indx" => indx_pos = Some(c.pos),
            _ => {}
        }
        pos = c.next();
    }
    let mut s = strh?;
    s.format = fmt;
    s.indx_pos = indx_pos;
    Some(s)
}

/// Parse `LIST INFO` whose header is at `list_pos`.
pub(crate) fn parse_info(win: &mut Window<'_>, list_pos: u64, end: u64) -> Info {
    let mut info = Info::default();
    let end = end.min(win.avail());
    let mut pos = list_pos + 12;
    let mut n = 0;
    while pos + 8 <= end && n < 64 {
        n += 1;
        let Some(c) = read_chunk(win, pos) else { break };
        if c.pos + 8 + u64::from(c.size) > end {
            break;
        }
        let len = (c.size as usize).min(256);
        match &c.id {
            b"ISFT" => info.software = win_text(win, c.data(), len),
            b"ICRD" => info.created = win_text(win, c.data(), len),
            b"INAM" => info.title = win_text(win, c.data(), len),
            _ => {}
        }
        pos = c.next();
    }
    info
}

fn win_text(win: &mut Window<'_>, pos: u64, len: usize) -> Option<String> {
    let d = win.vec(pos, len)?;
    clean_string(&d)
}

/// Creation time from `ICRD`/`IDIT` strings.
pub(crate) fn created_from(info: &Info, hdrl: &Hdrl) -> Option<diskrescue_core::time::Timestamp> {
    info.created
        .as_deref()
        .and_then(parse_loose_date)
        .or_else(|| hdrl.idit.as_deref().and_then(parse_loose_date))
}

/// How a video codec's chunks are classified for keyframe detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VideoClass {
    /// Every frame is independently decodable.
    AllKey,
    /// MPEG-4 part 2 (DivX, Xvid, FMP4): I-VOP detection.
    Mpeg4,
    /// Microsoft MPEG-4 v3 (DIV3, MP43): picture type in the first bits.
    MsMpeg4V3,
    /// H.264.
    H264,
    /// HEVC.
    Hevc,
    /// Unknown codec; all frames are marked key.
    Unknown,
}

/// Canonical codec name and keyframe class for a video stream.
pub(crate) fn video_codec(s: &StreamHdr) -> (String, VideoClass) {
    let comp = match &s.format {
        StreamFormat::Video { compression, .. } => *compression,
        _ => [0; 4],
    };
    let printable = |c: [u8; 4]| c.iter().all(|&b| (0x20..0x7F).contains(&b)) && c != [0x20; 4];
    // Prefer the strf compression; fall back to the strh handler. Some
    // writers leave one of them zero or generic.
    let cc = if printable(comp) {
        comp
    } else if printable(s.handler) {
        s.handler
    } else {
        [0; 4]
    };
    classify_video_fourcc(cc)
}

fn classify_video_fourcc(cc: [u8; 4]) -> (String, VideoClass) {
    use VideoClass::*;
    let up: Vec<u8> = cc.iter().map(|b| b.to_ascii_uppercase()).collect();
    let name = match &up[..] {
        b"MJPG" | b"MJPEG" | b"JPEG" | b"AVRN" | b"LJPG" | b"MJPA" | b"MJPB" => {
            return ("mjpeg".into(), AllKey)
        }
        b"DVSD" | b"DVHD" | b"DVSL" | b"DV25" | b"DV50" | b"CDVC" | b"DVC " | b"DVCP" => {
            return ("dvvideo".into(), AllKey)
        }
        b"FMP4" | b"XVID" | b"DIVX" | b"DX50" | b"MP4V" | b"3IV2" | b"DXGM" | b"RMP4" | b"SEDG"
        | b"BLZ0" | b"HDX4" | b"M4S2" | b"FVFW" => return ("mpeg4".into(), Mpeg4),
        b"DIV3" | b"MP43" | b"DIV4" | b"DIV5" | b"DIV6" | b"MPG3" | b"COL1" | b"AP41" => {
            return ("msmpeg4v3".into(), MsMpeg4V3)
        }
        b"MP42" | b"DIV2" | b"MPG4" | b"MP41" | b"DIV1" => return ("msmpeg4v2".into(), Unknown),
        b"H264" | b"X264" | b"AVC1" | b"DAVC" | b"V264" => return ("h264".into(), H264),
        b"HEVC" | b"H265" | b"HVC1" | b"X265" => return ("hevc".into(), Hevc),
        b"VP80" => return ("vp8".into(), Unknown),
        b"VP90" => return ("vp9".into(), Unknown),
        b"WMV1" => "wmv1",
        b"WMV2" => "wmv2",
        b"WMV3" => "wmv3",
        b"WVC1" => "vc1",
        b"MPG1" | b"MPEG" => "mpeg1video",
        b"MPG2" | b"MP2V" => "mpeg2video",
        b"CVID" => "cinepak",
        b"IV50" | b"IV41" | b"IV31" | b"IV32" => "indeo",
        b"HFYU" | b"FFVH" => return ("huffyuv".into(), AllKey),
        b"FFV1" => return ("ffv1".into(), AllKey),
        b"I420" | b"YV12" | b"YUY2" | b"UYVY" | b"YVYU" | b"IYUV" | b"NV12" | b"Y800" => {
            return ("rawvideo".into(), AllKey)
        }
        [0, 0, 0, 0] => return ("rawvideo".into(), AllKey),
        _ => "",
    };
    if name.is_empty() {
        (fourcc_string(cc).to_ascii_lowercase(), Unknown)
    } else {
        (name.to_string(), Unknown)
    }
}

/// Canonical audio codec name for a WAVEFORMATEX tag.
pub(crate) fn audio_codec(tag: u16, bits: u16) -> String {
    match tag {
        0x0001 => match bits {
            8 => "pcm_u8".into(),
            16 => "pcm_s16le".into(),
            24 => "pcm_s24le".into(),
            32 => "pcm_s32le".into(),
            _ => "pcm".into(),
        },
        0x0003 => match bits {
            64 => "pcm_f64le".into(),
            _ => "pcm_f32le".into(),
        },
        0x0002 => "adpcm_ms".into(),
        0x0006 => "pcm_alaw".into(),
        0x0007 => "pcm_mulaw".into(),
        0x0011 => "adpcm_ima_wav".into(),
        0x0050 => "mp2".into(),
        0x0055 => "mp3".into(),
        0x00FF | 0x1610 | 0x706D | 0x4143 => "aac".into(),
        0x0160 => "wmav1".into(),
        0x0161 => "wmav2".into(),
        0x0162 => "wmapro".into(),
        0x2000 => "ac3".into(),
        0x2001 => "dts".into(),
        0x674F..=0x6751 => "vorbis".into(),
        0xF1AC => "flac".into(),
        t => format!("wav_0x{t:04x}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_fourcc_classification() {
        assert_eq!(
            classify_video_fourcc(*b"FMP4"),
            ("mpeg4".to_string(), VideoClass::Mpeg4)
        );
        assert_eq!(
            classify_video_fourcc(*b"mjpg"),
            ("mjpeg".to_string(), VideoClass::AllKey)
        );
        assert_eq!(classify_video_fourcc(*b"H264").1, VideoClass::H264);
        assert_eq!(classify_video_fourcc(*b"DIV3").1, VideoClass::MsMpeg4V3);
        let (n, c) = classify_video_fourcc(*b"ZZZ9");
        assert_eq!((n.as_str(), c), ("zzz9", VideoClass::Unknown));
        assert_eq!(classify_video_fourcc([0; 4]).0, "rawvideo");
    }

    #[test]
    fn audio_names() {
        assert_eq!(audio_codec(1, 16), "pcm_s16le");
        assert_eq!(audio_codec(0x55, 0), "mp3");
        assert_eq!(audio_codec(0x2000, 0), "ac3");
        assert_eq!(audio_codec(0x1234, 0), "wav_0x1234");
    }
}
