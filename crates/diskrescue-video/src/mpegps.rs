//! MPEG program streams: `.mpg` (MPEG-1/2 system streams) and DVD `.vob`.
//!
//! # Algorithm
//!
//! The stream is a sequence of packs. A pack is `00 00 01 BA`, a pack header
//! (14 bytes plus stuffing for MPEG-2, 12 for MPEG-1; the marker bits are
//! checked), then packets that each carry a 16-bit length: the system header
//! (`BB`), program stream map (`BC`), private stream 1 (`BD`), padding
//! (`BE`), private stream 2 (`BF`, DVD navigation), audio (`C0..DF`), video
//! (`E0..EF`) and the other reserved streams. The walker follows the length
//! fields, never scanning inside a packet, and stops at:
//!
//! * `00 00 01 B9` (program end): `Complete`.
//! * The end of the data on an element boundary, or bytes that do not start
//!   an element after at least one complete packet: `UnknownLength` (neither
//!   ffmpeg's `vob` muxer nor most DVD rips write the end code, so the
//!   following bytes can be anything).
//! * A pack header with bad marker bits, a packet whose MPEG-2 header marker
//!   bits are wrong, or an element cut by the end of the data: `Truncated`.
//!   The length then ends at the start of the pack that holds the problem, so
//!   a cut VOB ends on its last complete 2048-byte pack.
//!
//! A stream is labelled `vob` (extension `vob` in `extra["extension"]`) when
//! at least 90% of the packs start on 2048-byte multiples and either a DVD
//! navigation packet (`BF`) or a private-stream-1 packet with an AC-3, DTS,
//! LPCM or subpicture sub-stream id is present; otherwise `mpg`.
//!
//! Duration is the difference between the SCR of the first and the last pack
//! header. The picture size comes from the first MPEG video sequence header
//! found in the first video packets.

use diskrescue_core::format::{
    Confidence, ExtentStatus, FileFormat, Measured, Signature, Validation, ValidationStatus,
};
use diskrescue_core::media::{MediaCategory, MediaMeta};
use diskrescue_core::{Result, SharedSource};

use crate::io::{note_unreadable, Window};
use crate::meta::{find_mpeg_sequence_header, mpeg_frame_rate, PictureInfo, EXTENSION_KEY};
use crate::mpa;

const MAX_SIZE: u64 = 64 << 30;
/// Video packets examined for a sequence header.
const VIDEO_PES_SCAN: u32 = 12;
/// Bytes of each such packet examined.
const VIDEO_SCAN_BYTES: usize = 6144;
/// Private-1 / audio packets examined for codec identification.
const AUDIO_PES_SCAN: u32 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    /// `00 00 01 B9`.
    ProgramEnd,
    /// Data ended at an element boundary.
    Eof,
    /// Limit reached.
    Limit,
    /// Not an element, after valid packets: the stream ended.
    NotAnElement,
    /// Pack header or packet header is damaged.
    Damaged,
    /// An element is cut by the end of the data.
    Partial,
    Fault,
}

#[derive(Debug)]
struct Walk {
    stop: Stop,
    /// Length of the part that can be kept.
    len: u64,
    /// Where the problem is.
    err_at: u64,
    packs: u64,
    aligned: u64,
    elements: u64,
    mpeg2: bool,
    nav: bool,
    dvd_substream: bool,
    first_scr: Option<u64>,
    last_scr: Option<u64>,
    picture: Option<PictureInfo>,
    fps_code: Option<u8>,
    video: bool,
    /// Whether the video elementary stream is MPEG-2 (from its extension).
    video_mpeg2: Option<bool>,
    audio_codec: Option<String>,
    private_codec: Option<String>,
}

fn scr_mpeg2(b: &[u8]) -> u64 {
    (u64::from((b[0] >> 3) & 7) << 30)
        | (u64::from(b[0] & 3) << 28)
        | (u64::from(b[1]) << 20)
        | (u64::from(b[2] >> 3) << 15)
        | (u64::from(b[2] & 3) << 13)
        | (u64::from(b[3]) << 5)
        | u64::from(b[4] >> 3)
}

fn scr_mpeg1(b: &[u8]) -> u64 {
    (u64::from((b[0] >> 1) & 7) << 30)
        | (u64::from(b[1]) << 22)
        | (u64::from(b[2] >> 1) << 15)
        | (u64::from(b[3]) << 7)
        | u64::from(b[4] >> 1)
}

/// Parse a pack header whose bytes start at `h[0..4] == 00 00 01 BA`.
/// Returns `(total length, scr, is_mpeg2)`.
fn parse_pack_header(h: &[u8]) -> Option<(usize, u64, bool)> {
    if h.len() < 12 || h[..4] != [0, 0, 1, 0xBA] {
        return None;
    }
    if h[4] & 0xC0 == 0x40 {
        if h.len() < 14 {
            return None;
        }
        // Marker bits.
        if h[4] & 0x04 == 0 || h[6] & 0x04 == 0 || h[8] & 0x04 == 0 || h[9] & 0x01 == 0 {
            return None;
        }
        if h[12] & 0x03 != 0x03 {
            return None;
        }
        let stuffing = usize::from(h[13] & 7);
        Some((14 + stuffing, scr_mpeg2(&h[4..9]), true))
    } else if h[4] & 0xF0 == 0x20 {
        if h[4] & 1 == 0 || h[6] & 1 == 0 || h[8] & 1 == 0 || h[9] & 0x80 == 0 || h[11] & 1 == 0 {
            return None;
        }
        Some((12, scr_mpeg1(&h[4..9]), false))
    } else {
        None
    }
}

/// Stream ids that start a length-prefixed packet.
fn is_packet_id(sid: u8) -> bool {
    sid >= 0xBB
}

/// Stream ids whose MPEG-2 packets carry the `10` flags byte.
fn has_pes_header(sid: u8) -> bool {
    matches!(sid, 0xBD | 0xC0..=0xEF)
}

fn private1_codec(sub: u8) -> Option<&'static str> {
    match sub {
        0x80..=0x87 => Some("ac3"),
        0x88..=0x8F => Some("dts"),
        0xA0..=0xA7 => Some("pcm"),
        0xC0..=0xCF => Some("ac3"),
        _ => None,
    }
}

fn walk(win: &mut Window<'_>) -> Option<Walk> {
    let avail = win.avail();
    let mut w = Walk {
        stop: Stop::Eof,
        len: 0,
        err_at: 0,
        packs: 0,
        aligned: 0,
        elements: 0,
        mpeg2: false,
        nav: false,
        dvd_substream: false,
        first_scr: None,
        last_scr: None,
        picture: None,
        fps_code: None,
        video: false,
        video_mpeg2: None,
        audio_codec: None,
        private_codec: None,
    };
    let mut pos = 0u64;
    let mut pack_start = 0u64;
    let mut in_pack = false;
    let mut pes_in_pack = 0u32;
    let mut video_seen = 0u32;
    let mut audio_seen = 0u32;
    let mut private_seen = 0u32;
    let mut iterations = 0u64;
    loop {
        iterations += 1;
        if iterations > avail / 4 + 8 {
            break;
        }
        if pos >= avail {
            w.stop = if win.limited() {
                Stop::Limit
            } else {
                Stop::Eof
            };
            w.len = if w.stop == Stop::Limit && in_pack && pes_in_pack == 0 {
                pack_start
            } else {
                pos
            };
            break;
        }
        let hdr = win.get(pos, 14);
        if hdr.len() < 4 {
            w.stop = if win.fault().is_some() {
                Stop::Fault
            } else if win.limited() {
                Stop::Limit
            } else {
                Stop::Partial
            };
            w.err_at = pos;
            w.len = if in_pack { pack_start } else { pos };
            break;
        }
        if hdr[..3] != [0, 0, 1] {
            // Not a start code.
            w.err_at = pos;
            if in_pack && pes_in_pack == 0 {
                // Garbage straight after a pack header: the pack is damaged.
                w.stop = Stop::Damaged;
                w.len = pack_start;
            } else {
                w.stop = Stop::NotAnElement;
                w.len = pos;
            }
            break;
        }
        let sid = hdr[3];
        if sid == 0xB9 {
            if w.packs == 0 {
                return None;
            }
            w.stop = Stop::ProgramEnd;
            w.len = pos + 4;
            break;
        }
        if sid == 0xBA {
            let hdr = hdr.to_vec();
            let Some((total, scr, mpeg2)) = parse_pack_header(&hdr) else {
                if hdr.len() < 14 {
                    w.stop = Stop::Partial;
                } else {
                    w.stop = Stop::Damaged;
                }
                w.err_at = pos;
                w.len = pos;
                if w.packs == 0 {
                    return None;
                }
                break;
            };
            if pos + total as u64 > avail {
                w.stop = if win.limited() {
                    Stop::Limit
                } else {
                    Stop::Partial
                };
                w.err_at = pos;
                w.len = pos;
                break;
            }
            if w.packs == 0 {
                w.mpeg2 = mpeg2;
                w.first_scr = Some(scr);
            }
            w.last_scr = Some(scr);
            w.packs += 1;
            if pos % 2048 == 0 {
                w.aligned += 1;
            }
            pack_start = pos;
            in_pack = true;
            pes_in_pack = 0;
            pos += total as u64;
            continue;
        }
        if !is_packet_id(sid) || w.packs == 0 {
            // Elementary stream start codes (00..B8) cannot appear between
            // packets; and the stream must open with a pack header.
            if w.packs == 0 {
                return None;
            }
            w.err_at = pos;
            if in_pack && pes_in_pack == 0 {
                w.stop = Stop::Damaged;
                w.len = pack_start;
            } else {
                w.stop = Stop::NotAnElement;
                w.len = pos;
            }
            break;
        }
        // Length-prefixed packet.
        if hdr.len() < 6 {
            w.stop = if win.limited() {
                Stop::Limit
            } else {
                Stop::Partial
            };
            w.err_at = pos;
            w.len = if in_pack { pack_start } else { pos };
            break;
        }
        let plen = u64::from(u16::from_be_bytes([hdr[4], hdr[5]]));
        let end = pos + 6 + plen;
        if plen == 0 && sid != 0xBE {
            w.err_at = pos;
            w.stop = Stop::Damaged;
            w.len = if in_pack { pack_start } else { pos };
            break;
        }
        if end > avail {
            w.stop = if win.limited() {
                Stop::Limit
            } else {
                Stop::Partial
            };
            w.err_at = pos;
            w.len = if in_pack { pack_start } else { pos };
            break;
        }
        if w.mpeg2 && has_pes_header(sid) && plen >= 3 && hdr.len() >= 7 && hdr[6] & 0xC0 != 0x80 {
            w.err_at = pos;
            w.stop = Stop::Damaged;
            w.len = if in_pack { pack_start } else { pos };
            break;
        }
        w.elements += 1;
        pes_in_pack += 1;
        match sid {
            0xBF => {
                // DVD navigation: PCI (substream 0) or DSI (substream 1).
                if let Some(b) = win.u8(pos + 6) {
                    if b <= 1 {
                        w.nav = true;
                    }
                }
            }
            0xE0..=0xEF => {
                w.video = true;
                if video_seen < VIDEO_PES_SCAN && w.picture.is_none() {
                    video_seen += 1;
                    let n = (plen as usize).min(VIDEO_SCAN_BYTES);
                    let data = win.get(pos + 6, n);
                    if let Some(seq) = find_mpeg_sequence_header(data) {
                        w.picture = Some(seq.picture);
                        w.fps_code = Some(seq.rate_code);
                        w.video_mpeg2 = Some(seq.mpeg2);
                    }
                }
            }
            0xC0..=0xDF => {
                if audio_seen < AUDIO_PES_SCAN && w.audio_codec.is_none() {
                    audio_seen += 1;
                    let n = (plen as usize).min(2048);
                    let data = win.get(pos + 6, n);
                    // Skip the PES header when it is MPEG-2.
                    let skip = if w.mpeg2 && data.len() > 3 {
                        3 + usize::from(data[2])
                    } else {
                        0
                    };
                    let payload = data.get(skip..).unwrap_or(&[]);
                    w.audio_codec = Some(
                        mpa::find_stream(payload)
                            .map_or("mpeg_audio", |h| h.codec())
                            .to_string(),
                    );
                }
            }
            0xBD if private_seen < AUDIO_PES_SCAN * 4 => {
                private_seen += 1;
                if w.mpeg2 {
                    let head = win.get(pos + 6, 16).to_vec();
                    if head.len() > 3 {
                        let off = 3 + usize::from(head[2]);
                        let sub = if off < head.len() {
                            Some(head[off])
                        } else {
                            win.u8(pos + 6 + off as u64)
                        };
                        if let Some(sub) = sub {
                            if let Some(c) = private1_codec(sub) {
                                w.dvd_substream = true;
                                w.private_codec.get_or_insert_with(|| c.to_string());
                            } else if (0x20..=0x3F).contains(&sub) {
                                w.dvd_substream = true;
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        pos = end;
    }
    if win.fault().is_some() && !matches!(w.stop, Stop::Fault) {
        // A read failure silently shortened a read somewhere.
        if matches!(w.stop, Stop::Partial) {
            w.stop = Stop::Fault;
        }
    }
    if w.packs == 0 {
        return None;
    }
    log::debug!(
        "MPEG-PS: {} packs, {} packets, stop {:?} at {}",
        w.packs,
        w.elements,
        w.stop,
        w.len
    );
    Some(w)
}

fn is_vob(w: &Walk) -> bool {
    w.packs >= 2 && w.aligned * 10 >= w.packs * 9 && (w.nav || w.dvd_substream) && w.mpeg2
}

fn duration_ms(w: &Walk) -> Option<u64> {
    let (a, b) = (w.first_scr?, w.last_scr?);
    const WRAP: u64 = 1 << 33;
    let d = if b >= a { b - a } else { b + WRAP - a };
    let ms = d / 90;
    (ms > 0 && ms < 48 * 3600 * 1000).then_some(ms)
}

fn build_meta(w: &Walk) -> MediaMeta {
    let mut m = MediaMeta::default();
    if let Some(p) = w.picture {
        m.width = Some(p.width);
        m.height = Some(p.height);
    }
    if w.video {
        let mpeg2 = w.video_mpeg2.unwrap_or(w.mpeg2);
        m.video_codec = Some(if mpeg2 { "mpeg2video" } else { "mpeg1video" }.to_string());
    }
    m.audio_codec = w.audio_codec.clone().or_else(|| w.private_codec.clone());
    m.duration_ms = duration_ms(w);
    m.extra.insert(
        EXTENSION_KEY.into(),
        if is_vob(w) { "vob" } else { "mpg" }.into(),
    );
    m.extra.insert("packs".into(), w.packs.to_string());
    m.extra.insert(
        "system".into(),
        if w.mpeg2 { "mpeg2-ps" } else { "mpeg1-ps" }.into(),
    );
    if let Some(f) = w.fps_code.and_then(mpeg_frame_rate) {
        m.extra.insert("fps".into(), format!("{f:.3}"));
    }
    m
}

/// Format handler for MPEG program streams.
#[derive(Debug, Default, Clone, Copy)]
pub struct MpegPsFormat;

static SIGNATURES: [Signature; 1] = [Signature::at_start(&[0x00, 0x00, 0x01, 0xBA])];

impl FileFormat for MpegPsFormat {
    fn id(&self) -> &'static str {
        "mpg"
    }

    fn name(&self) -> &'static str {
        "MPEG program stream (MPG / VOB)"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["mpg", "vob", "mpeg"]
    }

    fn category(&self) -> MediaCategory {
        MediaCategory::Video
    }

    fn signatures(&self) -> &'static [Signature] {
        &SIGNATURES
    }

    fn min_size(&self) -> u64 {
        2048
    }

    fn max_size(&self) -> u64 {
        MAX_SIZE
    }

    fn priority(&self) -> u8 {
        25
    }

    fn probe(&self, head: &[u8]) -> bool {
        let Some((total, _, _)) = parse_pack_header(head) else {
            return false;
        };
        // The next start code must be a packet (or the end code).
        match head.get(total..total + 4) {
            Some(n) => n[..3] == [0, 0, 1] && (n[3] >= 0xB9),
            None => true,
        }
    }

    fn measure(&self, src: &SharedSource, start: u64, limit: u64) -> Result<Option<Measured>> {
        let mut win = Window::new(&**src, start, limit);
        let Some(w) = walk(&mut win) else {
            return Ok(None);
        };
        if w.len < 2048.min(w.len.max(1)) && w.elements == 0 {
            return Ok(None);
        }
        if w.elements == 0 {
            return Ok(None);
        }
        let mut notes = Vec::new();
        let (status, err_offset, conf) = match w.stop {
            Stop::ProgramEnd => (ExtentStatus::Complete, None, Confidence::HIGH),
            Stop::Eof | Stop::NotAnElement => (
                ExtentStatus::UnknownLength,
                None,
                if w.packs >= 4 {
                    Confidence::MEDIUM
                } else {
                    Confidence::LOW
                },
            ),
            Stop::Limit => (ExtentStatus::Capped, None, Confidence::MEDIUM),
            Stop::Damaged | Stop::Partial | Stop::Fault => {
                notes.push(format!("stream damaged or cut at offset {}", w.err_at));
                (
                    ExtentStatus::Truncated,
                    Some(w.err_at.max(w.len)),
                    Confidence::MEDIUM,
                )
            }
        };
        if let Some(f) = win.fault() {
            notes.push(format!("read error near offset {}", f.offset));
        }
        let mut m = Measured {
            len: w.len,
            status,
            confidence: conf,
            err_offset,
            meta: build_meta(&w),
            nested: Vec::new(),
            notes,
        };
        note_unreadable(&**src, start, &mut m);
        Ok(Some(m))
    }

    fn validate(&self, src: &SharedSource) -> Result<Validation> {
        let mut win = Window::new(&**src, 0, u64::MAX);
        let size = win.avail();
        let Some(w) = walk(&mut win) else {
            return Ok(Validation::invalid(
                "not an MPEG program stream (no valid pack header)",
            ));
        };
        let mut problems = Vec::new();
        match w.stop {
            Stop::Damaged => problems.push(format!(
                "damaged pack or packet header at offset {}",
                w.err_at
            )),
            Stop::Partial => problems.push(format!(
                "the file ends inside a pack (cut at offset {})",
                w.err_at
            )),
            Stop::Fault => problems.push(format!("read error near offset {}", w.err_at)),
            Stop::NotAnElement => problems.push(format!(
                "{} bytes at offset {} are not part of the stream",
                size - w.len.min(size),
                w.len
            )),
            Stop::ProgramEnd | Stop::Eof | Stop::Limit => {}
        }
        if w.stop == Stop::ProgramEnd && w.len < size {
            problems.push(format!(
                "{} bytes follow the program end code",
                size - w.len
            ));
        }
        if w.elements == 0 {
            problems.push("no packets inside the packs".into());
        }
        let intact = if problems.is_empty() {
            1.0
        } else {
            (w.len as f64 / size.max(1) as f64).clamp(0.0, 1.0) as f32
        };
        Ok(Validation {
            status: if problems.is_empty() {
                ValidationStatus::Valid
            } else {
                ValidationStatus::Damaged
            },
            intact_fraction: Some(intact),
            problems,
            meta: build_meta(&w),
            repairable_by: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal MPEG-2 pack header with a given SCR base.
    fn pack(scr: u64) -> Vec<u8> {
        let mut h = vec![0, 0, 1, 0xBA];
        let b0 = 0x40 | (((scr >> 30) & 7) << 3) as u8 | 0x04 | ((scr >> 28) & 3) as u8;
        h.push(b0);
        h.push((scr >> 20) as u8);
        h.push((((scr >> 15) & 0x1F) << 3) as u8 | 0x04 | ((scr >> 13) & 3) as u8);
        h.push((scr >> 5) as u8);
        h.push(((scr & 0x1F) << 3) as u8 | 0x04);
        h.push(0x01); // ext 0 + marker
        h.extend_from_slice(&[0x00, 0x04, 0x03]); // mux rate + markers
        h.push(0xF8); // reserved + stuffing 0
        h
    }

    fn pes(sid: u8, payload: &[u8]) -> Vec<u8> {
        let mut p = vec![0, 0, 1, sid];
        let len = payload.len() + 3;
        p.extend_from_slice(&(len as u16).to_be_bytes());
        p.extend_from_slice(&[0x80, 0x00, 0x00]);
        p.extend_from_slice(payload);
        p
    }

    #[test]
    fn pack_header_roundtrip() {
        let h = pack(123_456_789);
        assert_eq!(h.len(), 14);
        let (total, scr, mpeg2) = parse_pack_header(&h).unwrap();
        assert_eq!((total, scr, mpeg2), (14, 123_456_789, true));
        let mut bad = h.clone();
        bad[6] &= !4;
        assert!(parse_pack_header(&bad).is_none());
        assert!(parse_pack_header(&h[..13]).is_none());
    }

    fn mem(d: Vec<u8>) -> SharedSource {
        std::sync::Arc::new(diskrescue_core::source::MemSource::new(d))
    }

    #[test]
    fn walks_packs_and_stops_cleanly() {
        let mut d = Vec::new();
        for i in 0..6u64 {
            d.extend_from_slice(&pack(i * 90_000));
            d.extend_from_slice(&pes(0xE0, &[1, 2, 3, 4]));
            d.extend_from_slice(&pes(0xC0, &[5, 6]));
        }
        let len = d.len() as u64;
        d.extend_from_slice(&[9, 9, 9, 9, 9, 9, 9, 9]);
        let m = MpegPsFormat
            .measure(&mem(d.clone()), 0, u64::MAX)
            .unwrap()
            .unwrap();
        assert_eq!(m.len, len);
        assert_eq!(m.status, ExtentStatus::UnknownLength);
        assert_eq!(m.meta.duration_ms, Some(5000));
        // Program end code => complete.
        let mut e = d[..len as usize].to_vec();
        e.extend_from_slice(&[0, 0, 1, 0xB9]);
        e.extend_from_slice(&[7; 100]);
        let m = MpegPsFormat.measure(&mem(e), 0, u64::MAX).unwrap().unwrap();
        assert_eq!(m.len, len + 4);
        assert_eq!(m.status, ExtentStatus::Complete);
        assert_eq!(m.meta.extra["extension"], "mpg");
    }

    #[test]
    fn cut_inside_a_pack_ends_at_the_previous_pack() {
        let mut d = Vec::new();
        for i in 0..6u64 {
            d.extend_from_slice(&pack(i * 90_000));
            d.extend_from_slice(&pes(0xE0, &[1; 40]));
        }
        let pack_len = d.len() / 6;
        let cut = &d[..pack_len * 4 + 30];
        let m = MpegPsFormat
            .measure(&mem(cut.to_vec()), 0, u64::MAX)
            .unwrap()
            .unwrap();
        assert_eq!(m.status, ExtentStatus::Truncated);
        assert_eq!(m.len, (pack_len * 4) as u64);
        assert!(m.err_offset.unwrap() >= m.len);
    }

    #[test]
    fn rejects_lone_pack_header_and_noise() {
        let d = pack(0);
        assert!(MpegPsFormat
            .measure(&mem(d), 0, u64::MAX)
            .unwrap()
            .is_none());
        assert!(MpegPsFormat
            .measure(
                &mem(vec![
                    0, 0, 1, 0xBA, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF
                ]),
                0,
                u64::MAX
            )
            .unwrap()
            .is_none());
    }

    /// A raw length-prefixed packet (no PES flags), as DVD navigation uses.
    fn raw(sid: u8, payload: &[u8]) -> Vec<u8> {
        let mut p = vec![0, 0, 1, sid];
        p.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        p.extend_from_slice(payload);
        p
    }

    /// A 2048-byte DVD-style pack: header, then packets, padded.
    fn dvd_pack(scr: u64, packets: &[Vec<u8>]) -> Vec<u8> {
        let mut d = pack(scr);
        for p in packets {
            d.extend_from_slice(p);
        }
        assert!(d.len() <= 2048 - 6);
        let pad = 2048 - d.len() - 6;
        d.extend_from_slice(&raw(0xBE, &vec![0xFF; pad]));
        assert_eq!(d.len(), 2048);
        d
    }

    #[test]
    fn dvd_navigation_packs_make_a_vob() {
        let mut d = Vec::new();
        d.extend_from_slice(&dvd_pack(
            0,
            &[
                raw(0xBB, &[0x80, 0x00, 0x01, 0x04, 0xE1, 0x7F]),
                raw(0xBF, &[0u8; 0x3D4]),
                raw(0xBF, &{
                    let mut v = vec![1u8];
                    v.resize(0x3FA, 0);
                    v
                }),
            ],
        ));
        for i in 1..8u64 {
            d.extend_from_slice(&dvd_pack(i * 3000, &[pes(0xE0, &[7u8; 1900])]));
        }
        let m = MpegPsFormat
            .measure(&mem(d.clone()), 0, u64::MAX)
            .unwrap()
            .unwrap();
        assert_eq!(m.len, 8 * 2048);
        assert_eq!(m.meta.extra["extension"], "vob");
        // The same packs without navigation are a plain MPEG file.
        let mut plain = Vec::new();
        for i in 0..8u64 {
            plain.extend_from_slice(&dvd_pack(i * 3000, &[pes(0xE0, &[7u8; 1900])]));
        }
        let m = MpegPsFormat
            .measure(&mem(plain), 0, u64::MAX)
            .unwrap()
            .unwrap();
        assert_eq!(m.meta.extra["extension"], "mpg");
        // Navigation packs in a stream that is not 2048-aligned stay mpg.
        let mut shifted = vec![0u8; 0];
        shifted.extend_from_slice(&d);
        let mut odd = pack(0);
        odd.extend_from_slice(&raw(0xBF, &[0u8; 100]));
        odd.extend_from_slice(&d);
        let m = MpegPsFormat
            .measure(&mem(odd), 0, u64::MAX)
            .unwrap()
            .unwrap();
        assert_eq!(m.meta.extra["extension"], "mpg");
    }
}
