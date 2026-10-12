//! Rebuilding a lost `moov` from a healthy reference recording.
//!
//! Phones and cameras write the media data (`mdat`) while recording and the
//! index (`moov`) only when recording stops; a crash, a pulled card or a
//! flat battery leaves an `mdat` with no index. The samples are still
//! there, back to back, in the order the encoder produced them. This module
//! finds their boundaries again, the way untrunc / recoverMP4 do, using a
//! healthy clip from the same device as the source of codec parameters,
//! sample descriptions and timing.
//!
//! # Algorithm
//!
//! 1. [`learn`] parses the reference's `moov`: the first video track
//!    (H.264 `avc1`/`avc3`, HEVC `hvc1`/`hev1`) and the first audio track
//!    (AAC `mp4a`, or PCM `sowt`/`twos`/`lpcm`/`in24`...), their parameter
//!    sets, the frame durations (mode of the `stts` deltas), the reorder
//!    delay (first composition offset), sample-size and chunk statistics,
//!    and the chunk interleave order. Other tracks (timecode, GoPro
//!    telemetry, DJI/Sony metadata) are dropped and listed.
//! 2. The media region of the damaged file is located: the payload of the
//!    first `mdat` (to the end of the file when its size is 0 or wrong), or
//!    everything after the recognisable leading boxes when the `mdat`
//!    header is gone.
//! 3. The region is walked sample by sample. At each position both
//!    matchers are tried:
//!    * video: `nal_length_size`-byte big-endian NAL lengths, NAL header
//!      validity (forbidden bit, allowed type, `nal_ref_idc` rules),
//!      structural checks of SEI / AUD / filler / parameter-set NALs, and
//!      the slice header parsed with the reference's SPS/PPS. Consecutive
//!      NALs are grouped into one access unit until a NAL starts the next
//!      one (AUD, SEI or parameter set after a slice, or a slice that
//!      [`crate::h264::is_new_picture`] / the HEVC first-slice flag says is
//!      a new picture). H.264 `frame_num` continuity is tracked; a break
//!      makes the candidate "weak".
//!    * audio: AAC frames are delimited exactly by walking their syntax
//!      ([`crate::aac::frame_length`]); PCM has no structure and is placed
//!      as the bytes between video samples (a multiple of the frame size).
//!
//!    When both match, or the only match is weak, the candidate whose end
//!    is followed by another valid sample (or the end of the data) wins;
//!    remaining ties follow the reference's chunk pattern, then video.
//!    When nothing matches, the bytes are unclassified and the scanner
//!    resynchronises: it searches forward (up to 1 MiB per step) for a
//!    position where a sample starts *and* is followed by another valid
//!    sample. The scan stops when an unclassified run exceeds
//!    [`RebuildOptions::stop_at_first_unclassified_mb`], at the
//!    duration limit, or at the end of the data. Container structure found
//!    inside the region (`free`/`skip`/`wide` boxes, a nested `mdat`
//!    header, a stale `moov`) is skipped.
//! 4. Composition offsets: each video sample's picture order count is
//!    derived from its slice header (H.264 §8.2.1, HEVC §8.3.1) and an
//!    "epoch" counter that increases at every IDR (HEVC: every IRAP with
//!    NoRaslOutputFlag) and after every gap. Display rank = position in the
//!    stream sorted by (epoch, POC). `cts = (rank + shift) * frame
//!    duration`, `dts = decode index * frame duration`, with one global
//!    `shift` (the largest reorder depth, at least the reference's) so
//!    every offset is non-negative and presentation order is monotonic. An
//!    edit list with `media_time = shift * frame duration` makes the first
//!    displayed frame start at 0. No `ctts` is written when every offset is
//!    zero.
//! 5. The output is `ftyp` (copied from the reference) + a new `moov` +
//!    an `mdat` that holds exactly the classified bytes in their original
//!    order; unclassified gaps are dropped and every chunk offset points
//!    into the new `mdat`.
//!
//! # Limits
//!
//! * Video codecs other than H.264/HEVC and audio codecs other than AAC
//!   (Main/LC/LTP and the LC core of HE-AAC) and PCM are not rebuilt; such
//!   tracks are dropped with a note.
//! * Constant frame rate is assumed (the reference's dominant frame
//!   duration). When the reference itself is variable-frame-rate and the
//!   rebuilt video and audio durations differ by more than 0.5 s, the video
//!   frame duration is stretched to match the audio.
//! * Interlaced H.264 field pairs are grouped into one sample when the two
//!   fields share `frame_num` and have opposite parity.

use std::collections::BTreeMap;
use std::io::Write;

use diskrescue_core::{Error, Range, Result, SharedSource, Source, SourceExt};

use crate::aac::{self, AudioSpecificConfig};
use crate::boxes::{self, is_pcm_kind, parse_moov, sample_table, walk_top_level, Elst, Moov, Trak};
use crate::writer::{assemble, MovieOut, OutSample, SpliceSource, TrackOut};
use crate::{h264, hevc};

/// Largest NAL unit accepted while scanning.
pub const MAX_NAL_SIZE: u64 = 8 << 20;

/// Samples-per-chunk statistics of a reference track.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ChunkStats {
    /// Smallest chunk (samples).
    pub min: u32,
    /// Largest chunk (samples).
    pub max: u32,
    /// Most common chunk size (samples).
    pub mode: u32,
    /// Mean chunk size (samples).
    pub mean: f64,
}

/// Sample-size statistics of a reference track.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SizeStats {
    /// Smallest sample.
    pub min: u32,
    /// Largest sample.
    pub max: u32,
    /// Median sample.
    pub median: u32,
    /// 99th percentile.
    pub p99: u32,
}

/// What the scanner needs to know about a track's codec.
#[derive(Debug, Clone, PartialEq)]
pub enum Codec {
    /// H.264 with length-prefixed NAL units.
    Avc {
        /// NAL length prefix size.
        nal_length_size: u8,
        /// Parameter sets from `avcC`.
        sps: h264::SpsMap,
        /// Picture parameter sets from `avcC`.
        pps: h264::PpsMap,
    },
    /// HEVC with length-prefixed NAL units.
    Hevc {
        /// NAL length prefix size.
        nal_length_size: u8,
        /// Video parameter sets.
        vps: hevc::VpsMap,
        /// Sequence parameter sets.
        sps: hevc::SpsMap,
        /// Picture parameter sets.
        pps: hevc::PpsMap,
    },
    /// AAC raw data blocks.
    Aac {
        /// Parsed AudioSpecificConfig.
        config: AudioSpecificConfig,
    },
    /// Uncompressed PCM.
    Pcm {
        /// Bytes per PCM frame (all channels).
        bytes_per_frame: u32,
    },
}

/// A reference track usable as a rebuild template.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackTemplate {
    /// The reference track (sample description, headers).
    pub trak: Trak,
    /// Codec parameters for the scanner.
    pub codec: Codec,
    /// Most common `stts` delta.
    pub sample_delta: u32,
    /// Sample sizes.
    pub sizes: SizeStats,
    /// Samples per chunk.
    pub chunk: ChunkStats,
    /// Reorder delay in frames (first composition offset / delta).
    pub reorder_delay: u32,
    /// The reference has more than one distinct frame duration.
    pub variable_rate: bool,
    /// Edit list media time (AAC priming, video delay), when present.
    pub edit_media_time: Option<i64>,
}

/// Everything learnt from a healthy reference recording.
#[derive(Debug, Clone, PartialEq)]
pub struct Reference {
    /// The reference's parsed `moov`.
    pub moov: Moov,
    /// The reference's `ftyp` box (header included), empty when absent.
    pub ftyp: Vec<u8>,
    /// Video template.
    pub video: Option<TrackTemplate>,
    /// Audio template.
    pub audio: Option<TrackTemplate>,
    /// Handler (or sample entry) types of tracks that will not be rebuilt.
    pub other_tracks_dropped: Vec<[u8; 4]>,
    /// Video samples per chunk.
    pub typical_video_chunk: ChunkStats,
    /// Audio samples per chunk.
    pub typical_audio_chunk: ChunkStats,
    /// Video frame duration (mode of the `stts` deltas), media ticks.
    pub frame_duration: u32,
    /// Audio frame duration, media ticks.
    pub audio_frame_duration: u32,
    /// The reference's chunk order: `(track_id, samples)` per chunk in file
    /// order (first 256 chunks).
    pub interleave_pattern: Vec<(u32, usize)>,
    /// Observations.
    pub notes: Vec<String>,
}

/// Options for [`rebuild`].
#[derive(Debug, Clone, PartialEq)]
pub struct RebuildOptions {
    /// Stop scanning when one contiguous unclassified run reaches this many
    /// MiB (0 = never stop early).
    pub stop_at_first_unclassified_mb: u64,
    /// Use SPS/PPS (and VPS) found inside the media data when the
    /// reference lacks a set with that id.
    pub allow_in_band_parameter_sets: bool,
    /// Stop once the rebuilt movie reaches this duration.
    pub max_duration_s: Option<u64>,
}

impl Default for RebuildOptions {
    fn default() -> Self {
        RebuildOptions {
            stop_at_first_unclassified_mb: 16,
            allow_in_band_parameter_sets: true,
            max_duration_s: None,
        }
    }
}

/// What a rebuild found and wrote.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct RebuildReport {
    /// Video samples written.
    pub video_samples: u64,
    /// Audio samples written (PCM: frames).
    pub audio_samples: u64,
    /// Video keyframes.
    pub keyframes: u64,
    /// Duration of the longest rebuilt track.
    pub duration_ms: u64,
    /// Bytes assigned to samples.
    pub bytes_classified: u64,
    /// Bytes that matched nothing (dropped).
    pub bytes_unclassified: u64,
    /// The unclassified ranges (absolute offsets in the damaged file).
    pub unclassified_ranges: Vec<Range>,
    /// Where scanning stopped early, if it did.
    pub stopped_at: Option<u64>,
    /// Bytes written (0 until the output is written).
    pub output_len: u64,
    /// Observations.
    pub notes: Vec<String>,
}

// ---------------------------------------------------------------------------
// learn()
// ---------------------------------------------------------------------------

fn mode_of(counts: impl Iterator<Item = (u32, u64)>) -> u32 {
    let mut m: BTreeMap<u32, u64> = BTreeMap::new();
    for (v, n) in counts {
        *m.entry(v).or_default() += n;
    }
    m.into_iter()
        .max_by_key(|&(v, n)| (n, std::cmp::Reverse(v)))
        .map(|x| x.0)
        .unwrap_or(0)
}

fn size_stats(mut sizes: Vec<u32>) -> SizeStats {
    if sizes.is_empty() {
        return SizeStats::default();
    }
    sizes.sort_unstable();
    let n = sizes.len();
    SizeStats {
        min: sizes[0],
        max: sizes[n - 1],
        median: sizes[n / 2],
        p99: sizes[((n * 99) / 100).min(n - 1)],
    }
}

fn chunk_stats(trak: &Trak) -> ChunkStats {
    let n = trak.stco.len();
    if n == 0 || trak.stsc.is_empty() {
        return ChunkStats::default();
    }
    let mut per: Vec<u32> = Vec::with_capacity(n.min(1 << 20));
    let mut i = 0usize;
    for c in 0..n.min(1 << 20) {
        let chunk_no = c as u32 + 1;
        while i + 1 < trak.stsc.len() && trak.stsc[i + 1].0 <= chunk_no {
            i += 1;
        }
        per.push(trak.stsc[i].1);
    }
    let min = *per.iter().min().unwrap_or(&0);
    let max = *per.iter().max().unwrap_or(&0);
    let mean = per.iter().map(|&x| x as f64).sum::<f64>() / per.len() as f64;
    ChunkStats {
        min,
        max,
        mode: mode_of(per.iter().map(|&x| (x, 1))),
        mean,
    }
}

fn template_for(trak: &Trak, notes: &mut Vec<String>) -> Option<TrackTemplate> {
    let entry = trak.entry()?;
    let codec = match &entry.kind {
        b"avc1" | b"avc3" => {
            let mut sps = h264::SpsMap::new();
            let mut pps = h264::PpsMap::new();
            let mut l = 4u8;
            if let Some(c) = &entry.avcc {
                l = c.nal_length_size;
                for n in &c.sps {
                    match h264::parse_sps(n) {
                        Some(s) => {
                            sps.insert(s.sps_id, s);
                        }
                        None => notes.push("reference avcC SPS does not parse".into()),
                    }
                }
                for n in &c.pps {
                    match h264::parse_pps(n) {
                        Some(p) => {
                            pps.insert(p.pps_id, p);
                        }
                        None => notes.push("reference avcC PPS does not parse".into()),
                    }
                }
            }
            if l == 3 {
                notes.push("avcC declares 3-byte NAL lengths".into());
            }
            Codec::Avc {
                nal_length_size: l,
                sps,
                pps,
            }
        }
        b"hvc1" | b"hev1" | b"dvh1" | b"dvhe" => {
            let mut vps = hevc::VpsMap::new();
            let mut sps = hevc::SpsMap::new();
            let mut pps = hevc::PpsMap::new();
            let mut l = 4u8;
            if let Some(c) = &entry.hvcc {
                l = c.nal_length_size;
                for n in &c.vps {
                    if let Some(v) = hevc::parse_vps(n) {
                        vps.insert(v.vps_id, v);
                    }
                }
                for n in &c.sps {
                    match hevc::parse_sps(n) {
                        Some(s) => {
                            sps.insert(s.sps_id, s);
                        }
                        None => notes.push("reference hvcC SPS does not parse".into()),
                    }
                }
                for n in &c.pps {
                    match hevc::parse_pps(n) {
                        Some(p) => {
                            pps.insert(p.pps_id, p);
                        }
                        None => notes.push("reference hvcC PPS does not parse".into()),
                    }
                }
            }
            Codec::Hevc {
                nal_length_size: l,
                vps,
                sps,
                pps,
            }
        }
        b"mp4a" => {
            let esds = entry.esds.as_ref()?;
            let config = aac::parse_audio_specific_config(&esds.audio_specific_config)?;
            if !aac::walk_supported(&config) {
                notes.push(format!(
                    "AAC object type {} / {} channels is not supported by the frame walker",
                    config.object_type, config.channels
                ));
                return None;
            }
            Codec::Aac { config }
        }
        k if is_pcm_kind(k) => {
            let bpf = match trak.stsz {
                boxes::SampleSizes::Constant { size, .. } if size > 0 && size <= 4096 => size,
                _ => entry.bytes_per_frame?,
            };
            Codec::Pcm {
                bytes_per_frame: bpf,
            }
        }
        _ => return None,
    };
    let samples = sample_table(trak).ok()?;
    let sample_delta = mode_of(trak.stts.iter().map(|&(n, d)| (d, n as u64)));
    let distinct: std::collections::BTreeSet<u32> = trak
        .stts
        .iter()
        .take(trak.stts.len().saturating_sub(1).max(1))
        .map(|e| e.1)
        .collect();
    let reorder_delay = match (samples.first(), sample_delta) {
        (Some(s), d) if d > 0 && s.cts_offset > 0 => (s.cts_offset as u32) / d,
        _ => 0,
    };
    let edit_media_time = trak
        .elst
        .as_ref()
        .and_then(|e| e.iter().find(|x| x.media_time >= 0))
        .map(|x| x.media_time);
    Some(TrackTemplate {
        trak: trak.clone(),
        codec,
        sample_delta: sample_delta.max(1),
        sizes: size_stats(samples.iter().map(|s| s.size).collect()),
        chunk: chunk_stats(trak),
        reorder_delay,
        variable_rate: distinct.len() > 1,
        edit_media_time,
    })
}

/// Learn the rebuild template from a healthy reference recording (only its
/// `ftyp` and `moov` are read).
pub fn learn(reference: &SharedSource) -> Result<Reference> {
    let src: &dyn Source = &**reference;
    let size = src.size();
    let (tops, _) = walk_top_level(src, 0, size, &|k| boxes::is_printable_fourcc(k))?;
    let moov_box = tops
        .iter()
        .find(|t| t.header.is(b"moov"))
        .ok_or_else(|| Error::NotFound("reference has no moov box".into()))?;
    let moov = parse_moov(src, &moov_box.header)?;
    let ftyp = match tops.first() {
        Some(t) if t.header.is(b"ftyp") && t.header.size <= 4096 && !t.truncated => {
            src.read_exact_vec(t.header.offset, t.header.size as usize)?
        }
        _ => Vec::new(),
    };
    let mut notes = Vec::new();
    let mut video: Option<TrackTemplate> = None;
    let mut audio: Option<TrackTemplate> = None;
    let mut dropped = Vec::new();
    for t in &moov.tracks {
        let kind = t.entry().map(|e| e.kind).unwrap_or(t.handler);
        if t.is_video() && video.is_none() {
            if let Some(tt) = template_for(t, &mut notes) {
                video = Some(tt);
                continue;
            }
            notes.push(format!(
                "video codec '{}' cannot be rebuilt",
                boxes::fourcc_str(&kind)
            ));
        } else if t.is_audio() && audio.is_none() {
            if let Some(tt) = template_for(t, &mut notes) {
                audio = Some(tt);
                continue;
            }
            notes.push(format!(
                "audio codec '{}' cannot be rebuilt",
                boxes::fourcc_str(&kind)
            ));
        }
        dropped.push(if t.handler == [0; 4] { kind } else { t.handler });
    }
    if video.is_none() && audio.is_none() {
        return Err(Error::unsupported(
            "reference has no H.264/HEVC video or AAC/PCM audio track",
        ));
    }
    // Interleave pattern: chunks of the kept tracks in file order.
    let mut chunks: Vec<(u64, u32, usize)> = Vec::new();
    for tt in video.iter().chain(audio.iter()) {
        let id = tt.trak.tkhd.track_id;
        let mut i = 0usize;
        for (c, &off) in tt.trak.stco.iter().enumerate().take(4096) {
            let chunk_no = c as u32 + 1;
            while i + 1 < tt.trak.stsc.len() && tt.trak.stsc[i + 1].0 <= chunk_no {
                i += 1;
            }
            let spc = tt.trak.stsc.get(i).map(|e| e.1).unwrap_or(1) as usize;
            chunks.push((off, id, spc));
        }
    }
    chunks.sort_by_key(|c| c.0);
    let interleave_pattern = chunks.iter().take(256).map(|c| (c.1, c.2)).collect();
    Ok(Reference {
        typical_video_chunk: video.as_ref().map(|v| v.chunk).unwrap_or_default(),
        typical_audio_chunk: audio.as_ref().map(|a| a.chunk).unwrap_or_default(),
        frame_duration: video.as_ref().map(|v| v.sample_delta).unwrap_or(0),
        audio_frame_duration: audio.as_ref().map(|a| a.sample_delta).unwrap_or(0),
        moov,
        ftyp,
        video,
        audio,
        other_tracks_dropped: dropped,
        interleave_pattern,
        notes,
    })
}

// ---------------------------------------------------------------------------
// Buffered window over the damaged source
// ---------------------------------------------------------------------------

const WINDOW: usize = 8 << 20;

struct Window<'a> {
    src: &'a dyn Source,
    start: u64,
    buf: Vec<u8>,
    end: u64,
    /// The buffer stops early because of a read error or short read.
    short: bool,
}

impl<'a> Window<'a> {
    fn new(src: &'a dyn Source, end: u64) -> Self {
        Window {
            src,
            start: 0,
            buf: Vec::new(),
            end: end.min(src.size()),
            short: false,
        }
    }

    /// Up to `len` bytes at `pos` (shorter at the end of the region or at
    /// an unreadable byte; empty when `pos` itself is unreadable).
    fn get(&mut self, pos: u64, len: usize) -> &[u8] {
        if pos >= self.end {
            return &[];
        }
        let want_end = pos.saturating_add(len as u64).min(self.end);
        let buf_end = self.start + self.buf.len() as u64;
        let inside = pos >= self.start && pos < buf_end;
        let have = inside && (want_end <= buf_end || self.short);
        if !have {
            let read_len = (want_end - pos).max(WINDOW as u64).min(self.end - pos) as usize;
            self.start = pos;
            self.buf.resize(read_len, 0);
            let mut done = 0usize;
            while done < read_len {
                match self.src.read_at(pos + done as u64, &mut self.buf[done..]) {
                    Ok(0) => break,
                    Ok(n) => done += n,
                    Err(_) => break,
                }
            }
            self.short = done < read_len;
            self.buf.truncate(done);
        }
        let a = (pos - self.start) as usize;
        let b = ((want_end - self.start) as usize).min(self.buf.len());
        if a >= b {
            return &[];
        }
        &self.buf[a..b]
    }
}

fn be_uint(b: &[u8]) -> u64 {
    b.iter().fold(0u64, |a, &x| (a << 8) | x as u64)
}

// ---------------------------------------------------------------------------
// Video matching
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum SliceRef {
    Avc(h264::SliceHeader),
    Hevc(hevc::SliceSegmentHeader),
}

#[derive(Debug, Clone)]
struct VideoCand {
    len: u64,
    keyframe: bool,
    /// The primary picture is intra coded (a decoder can start here).
    intra: bool,
    slice: SliceRef,
    params: Vec<Vec<u8>>,
}

enum VideoState {
    Avc {
        l: usize,
        sps: h264::SpsMap,
        pps: h264::PpsMap,
        poc: h264::PocState,
        prev_ref_frame_num: Option<u32>,
    },
    Hevc {
        l: usize,
        vps: hevc::VpsMap,
        sps: hevc::SpsMap,
        pps: hevc::PpsMap,
        poc: hevc::PocState,
    },
}

/// Check a filler-data payload: 0xFF bytes then the 0x80 trailing byte.
fn filler_ok(payload: &[u8]) -> bool {
    match payload.split_last() {
        Some((&last, rest)) => last == 0x80 && rest.iter().all(|&b| b == 0xFF),
        None => false,
    }
}

const HEAD: usize = 112;

impl VideoState {
    fn new(codec: &Codec) -> Option<VideoState> {
        match codec {
            Codec::Avc {
                nal_length_size,
                sps,
                pps,
            } => Some(VideoState::Avc {
                l: *nal_length_size as usize,
                sps: sps.clone(),
                pps: pps.clone(),
                poc: h264::PocState::new(),
                prev_ref_frame_num: None,
            }),
            Codec::Hevc {
                nal_length_size,
                vps,
                sps,
                pps,
            } => Some(VideoState::Hevc {
                l: *nal_length_size as usize,
                vps: vps.clone(),
                sps: sps.clone(),
                pps: pps.clone(),
                poc: hevc::PocState::new(),
            }),
            _ => None,
        }
    }

    fn nal_len_size(&self) -> usize {
        match self {
            VideoState::Avc { l, .. } | VideoState::Hevc { l, .. } => *l,
        }
    }

    /// Cheap test used while resynchronising: could an access unit start at
    /// the beginning of `b`?
    fn quick_start(&self, b: &[u8], room: u64) -> bool {
        let l = self.nal_len_size();
        if b.len() < l + 2 {
            return false;
        }
        let n = be_uint(&b[..l]);
        if !(2..=MAX_NAL_SIZE).contains(&n) || n + l as u64 > room {
            return false;
        }
        let h = b[l];
        if h & 0x80 != 0 {
            return false;
        }
        match self {
            VideoState::Avc { .. } => {
                let t = h & 0x1F;
                let r = h >> 5;
                match t {
                    1 => true,
                    5 | 7 | 8 => r != 0,
                    6 | 9 => r == 0,
                    _ => false,
                }
            }
            VideoState::Hevc { .. } => match hevc::parse_nal_header(&b[l..]) {
                Some(nh) => hevc::is_slice_type(nh.nal_type) || matches!(nh.nal_type, 32..=35 | 39),
                None => false,
            },
        }
    }

    fn try_at(&self, win: &mut Window<'_>, pos: u64, end: u64, in_band: bool) -> Option<VideoCand> {
        match self {
            VideoState::Avc { l, sps, pps, .. } => try_avc(win, pos, end, *l, sps, pps, in_band),
            VideoState::Hevc { l, sps, pps, .. } => try_hevc(win, pos, end, *l, sps, pps, in_band),
        }
    }

    /// H.264 frame_num continuity (7.4.3); HEVC has no equivalent check.
    fn continuity_ok(&self, c: &VideoCand) -> bool {
        match (self, &c.slice) {
            (
                VideoState::Avc {
                    sps,
                    prev_ref_frame_num,
                    ..
                },
                SliceRef::Avc(h),
            ) => {
                if h.is_idr() {
                    return true;
                }
                let Some(prev) = prev_ref_frame_num else {
                    return true;
                };
                let Some(s) = sps.get(&h.sps_id) else {
                    return false;
                };
                if s.gaps_in_frame_num_allowed {
                    return true;
                }
                let max = s.max_frame_num();
                h.frame_num == *prev || h.frame_num == (prev + 1) % max
            }
            _ => true,
        }
    }

    /// Commit an accepted candidate: returns (poc, starts_new_epoch).
    fn accept(&mut self, c: &VideoCand, in_band: bool) -> (i32, bool) {
        match (self, &c.slice) {
            (
                VideoState::Avc {
                    sps,
                    pps,
                    poc,
                    prev_ref_frame_num,
                    ..
                },
                SliceRef::Avc(h),
            ) => {
                if in_band {
                    for p in &c.params {
                        match h264::nal_type(p[0]) {
                            7 => {
                                if let Some(s) = h264::parse_sps(p) {
                                    sps.entry(s.sps_id).or_insert(s);
                                }
                            }
                            8 => {
                                if let Some(x) = h264::parse_pps(p) {
                                    pps.entry(x.pps_id).or_insert(x);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                let value = match sps.get(&h.sps_id) {
                    Some(s) => h264::poc(s, h, poc),
                    None => 0,
                };
                if h.nal_ref_idc != 0 {
                    *prev_ref_frame_num = Some(h.frame_num);
                }
                (value, h.is_idr())
            }
            (
                VideoState::Hevc {
                    vps, sps, pps, poc, ..
                },
                SliceRef::Hevc(h),
            ) => {
                if in_band {
                    for p in &c.params {
                        match hevc::nal_type(p[0]) {
                            32 => {
                                if let Some(v) = hevc::parse_vps(p) {
                                    vps.entry(v.vps_id).or_insert(v);
                                }
                            }
                            33 => {
                                if let Some(s) = hevc::parse_sps(p) {
                                    sps.entry(s.sps_id).or_insert(s);
                                }
                            }
                            34 => {
                                if let Some(x) = hevc::parse_pps(p) {
                                    pps.entry(x.pps_id).or_insert(x);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                let new_epoch = hevc::is_irap(h.nal_type) && hevc::no_rasl_output(h.nal_type, poc);
                let sps_for = pps.get(&h.pps_id).and_then(|p| sps.get(&p.sps_id));
                let value = match sps_for {
                    Some(s) => hevc::poc(s, h, poc),
                    None => 0,
                };
                (value, new_epoch)
            }
            _ => (0, false),
        }
    }

    /// Forget continuity after a gap.
    fn reset_after_gap(&mut self) {
        match self {
            VideoState::Avc {
                poc,
                prev_ref_frame_num,
                ..
            } => {
                *poc = h264::PocState::new();
                *prev_ref_frame_num = None;
            }
            VideoState::Hevc { poc, .. } => {
                *poc = hevc::PocState::new();
            }
        }
    }
}

/// Read the NAL at `p`: returns (payload length, header bytes copy).
fn read_nal(win: &mut Window<'_>, p: u64, end: u64, l: usize) -> Option<(u64, Vec<u8>)> {
    let b = win.get(p, l + HEAD);
    if b.len() < l + 1 {
        return None;
    }
    let n = be_uint(&b[..l]);
    if n == 0 || n > MAX_NAL_SIZE || p + l as u64 + n > end {
        return None;
    }
    let take = (n as usize).min(b.len() - l);
    Some((n, b[l..l + take].to_vec()))
}

fn full_nal(win: &mut Window<'_>, p: u64, l: usize, n: u64, cap: u64) -> Option<Vec<u8>> {
    if n > cap {
        return None;
    }
    let b = win.get(p + l as u64, n as usize);
    (b.len() as u64 == n).then(|| b.to_vec())
}

fn try_avc(
    win: &mut Window<'_>,
    pos: u64,
    end: u64,
    l: usize,
    sps_ref: &h264::SpsMap,
    pps_ref: &h264::PpsMap,
    in_band: bool,
) -> Option<VideoCand> {
    let mut p = pos;
    let mut count = 0u32;
    let mut first: Option<h264::SliceHeader> = None;
    let mut prev: Option<h264::SliceHeader> = None;
    let mut key = false;
    let mut fields = 0u32;
    let mut params: Vec<Vec<u8>> = Vec::new();
    let mut local: Option<(h264::SpsMap, h264::PpsMap)> = None;
    macro_rules! stop {
        () => {{
            if count == 0 {
                return None;
            }
            break;
        }};
    }
    while p < end && count < 4096 {
        let Some((n, head)) = read_nal(win, p, end, l) else {
            stop!()
        };
        let h = head[0];
        if h & 0x80 != 0 {
            stop!()
        }
        let t = h & 0x1F;
        let r = h >> 5;
        let mut last = false;
        match t {
            1 | 2 | 5 => {
                let (sps, pps) = match &local {
                    Some((s, q)) => (s, q),
                    None => (sps_ref, pps_ref),
                };
                let Some(sh) = h264::parse_slice_header(&head, sps, pps) else {
                    stop!()
                };
                match &prev {
                    Some(pv) => {
                        let s = sps.get(&sh.sps_id)?;
                        let second_field = pv.field_pic
                            && sh.field_pic
                            && pv.bottom_field != sh.bottom_field
                            && pv.frame_num == sh.frame_num
                            && fields == 1
                            && sh.first_mb_in_slice == 0;
                        if second_field {
                            fields += 1;
                        } else if h264::is_new_picture(pv, &sh, s)
                            || sh.first_mb_in_slice <= pv.first_mb_in_slice
                        {
                            break;
                        }
                    }
                    None => {
                        if sh.first_mb_in_slice != 0 {
                            stop!()
                        }
                        fields = 1;
                        first = Some(sh.clone());
                    }
                }
                key |= t == 5;
                prev = Some(sh);
            }
            3 | 4 => {
                if prev.is_none() {
                    stop!()
                }
            }
            6 => {
                if prev.is_some() {
                    break;
                }
                if r != 0 {
                    stop!()
                }
                let Some(full) = full_nal(win, p, l, n, 1 << 20) else {
                    stop!()
                };
                if !h264::sei_well_formed(&full) {
                    stop!()
                }
            }
            7 | 8 => {
                if prev.is_some() {
                    break;
                }
                let Some(full) = full_nal(win, p, l, n, 4096) else {
                    stop!()
                };
                if t == 7 {
                    let Some(s) = h264::parse_sps(&full) else {
                        stop!()
                    };
                    if in_band && !sps_ref.contains_key(&s.sps_id) {
                        let e = local.get_or_insert_with(|| (sps_ref.clone(), pps_ref.clone()));
                        e.0.insert(s.sps_id, s);
                    }
                } else {
                    let sm = local.as_ref().map(|x| &x.0).unwrap_or(sps_ref);
                    let Some(x) = h264::parse_pps(&full) else {
                        stop!()
                    };
                    if !sm.contains_key(&x.sps_id) {
                        stop!()
                    }
                    if in_band && !pps_ref.contains_key(&x.pps_id) {
                        let e = local.get_or_insert_with(|| (sps_ref.clone(), pps_ref.clone()));
                        e.1.insert(x.pps_id, x);
                    }
                }
                params.push(full);
            }
            9 => {
                if count > 0 || r != 0 || n != 2 {
                    stop!()
                }
            }
            10 | 11 => {
                if prev.is_none() || r != 0 || n != 1 {
                    stop!()
                }
                last = true;
            }
            12 => {
                if r != 0 {
                    stop!()
                }
                let Some(full) = full_nal(win, p, l, n, 1 << 20) else {
                    stop!()
                };
                if !filler_ok(&full[1..]) {
                    stop!()
                }
            }
            13..=15 => {
                if prev.is_some() {
                    break;
                }
            }
            19 | 20 => {
                if prev.is_none() {
                    stop!()
                }
            }
            _ => stop!(),
        }
        count += 1;
        p += l as u64 + n;
        if last {
            break;
        }
    }
    let first = first?;
    Some(VideoCand {
        len: p - pos,
        keyframe: key,
        intra: matches!(first.kind(), 2 | 4),
        slice: SliceRef::Avc(first),
        params,
    })
}

fn try_hevc(
    win: &mut Window<'_>,
    pos: u64,
    end: u64,
    l: usize,
    sps_ref: &hevc::SpsMap,
    pps_ref: &hevc::PpsMap,
    in_band: bool,
) -> Option<VideoCand> {
    let mut p = pos;
    let mut count = 0u32;
    let mut first: Option<hevc::SliceSegmentHeader> = None;
    let mut prev: Option<hevc::SliceSegmentHeader> = None;
    let mut key = false;
    let mut params: Vec<Vec<u8>> = Vec::new();
    let mut local: Option<(hevc::SpsMap, hevc::PpsMap)> = None;
    macro_rules! stop {
        () => {{
            if count == 0 {
                return None;
            }
            break;
        }};
    }
    while p < end && count < 4096 {
        let Some((n, head)) = read_nal(win, p, end, l) else {
            stop!()
        };
        if n < 2 {
            stop!()
        }
        let Some(nh) = hevc::parse_nal_header(&head) else {
            stop!()
        };
        let t = nh.nal_type;
        let mut last = false;
        match t {
            0..=9 | 16..=21 => {
                let (sps, pps) = match &local {
                    Some((s, q)) => (s, q),
                    None => (sps_ref, pps_ref),
                };
                let Some(sh) = hevc::parse_slice_header(&head, sps, pps) else {
                    stop!()
                };
                match &prev {
                    Some(pv) => {
                        if sh.layer_id == 0 && sh.first_slice_segment_in_pic {
                            break;
                        }
                        if sh.layer_id == pv.layer_id
                            && (sh.temporal_id != pv.temporal_id
                                || (!sh.first_slice_segment_in_pic
                                    && sh.segment_address <= pv.segment_address))
                        {
                            break;
                        }
                    }
                    None => {
                        if !sh.first_slice_segment_in_pic || sh.layer_id != 0 {
                            stop!()
                        }
                        first = Some(sh.clone());
                    }
                }
                key |= hevc::is_keyframe_type(t);
                prev = Some(sh);
            }
            32..=34 => {
                if prev.is_some() {
                    break;
                }
                let Some(full) = full_nal(win, p, l, n, 4096) else {
                    stop!()
                };
                match t {
                    32 => {
                        if hevc::parse_vps(&full).is_none() {
                            stop!()
                        }
                    }
                    33 => {
                        let Some(s) = hevc::parse_sps(&full) else {
                            stop!()
                        };
                        if in_band && !sps_ref.contains_key(&s.sps_id) {
                            let e = local.get_or_insert_with(|| (sps_ref.clone(), pps_ref.clone()));
                            e.0.insert(s.sps_id, s);
                        }
                    }
                    _ => {
                        let Some(x) = hevc::parse_pps(&full) else {
                            stop!()
                        };
                        if in_band && !pps_ref.contains_key(&x.pps_id) {
                            let e = local.get_or_insert_with(|| (sps_ref.clone(), pps_ref.clone()));
                            e.1.insert(x.pps_id, x);
                        }
                    }
                }
                params.push(full);
            }
            35 => {
                if count > 0 || n != 3 {
                    stop!()
                }
            }
            36 | 37 => {
                if prev.is_none() || n != 2 {
                    stop!()
                }
                last = true;
            }
            38 => {
                let Some(full) = full_nal(win, p, l, n, 1 << 20) else {
                    stop!()
                };
                if !filler_ok(&full[2..]) {
                    stop!()
                }
            }
            39 | 40 => {
                if t == 39 && prev.is_some() {
                    break;
                }
                if t == 40 && prev.is_none() {
                    stop!()
                }
                let Some(full) = full_nal(win, p, l, n, 1 << 20) else {
                    stop!()
                };
                let rbsp = diskrescue_core::util::bits::unescape_nal(&full[2..]);
                if !h264::sei_messages_fit(&rbsp) {
                    stop!()
                }
            }
            _ => stop!(),
        }
        count += 1;
        p += l as u64 + n;
        if last {
            break;
        }
    }
    let first = first?;
    Some(VideoCand {
        len: p - pos,
        keyframe: key,
        intra: first.slice_type == Some(2),
        slice: SliceRef::Hevc(first),
        params,
    })
}

// ---------------------------------------------------------------------------
// Audio matching
// ---------------------------------------------------------------------------

enum AudioState {
    Aac {
        config: AudioSpecificConfig,
        max_len: usize,
    },
    Pcm {
        bpf: u32,
    },
}

impl AudioState {
    fn new(codec: &Codec) -> Option<AudioState> {
        match codec {
            Codec::Aac { config } => Some(AudioState::Aac {
                config: *config,
                max_len: 768 * config.channels.max(1) as usize + 16,
            }),
            Codec::Pcm { bytes_per_frame } => Some(AudioState::Pcm {
                bpf: (*bytes_per_frame).max(1),
            }),
            _ => None,
        }
    }

    fn quick_start(&self, b: &[u8]) -> bool {
        match self {
            AudioState::Aac { config, .. } => {
                let Some(&b0) = b.first() else { return false };
                let id = b0 >> 5;
                match config.channels {
                    1 => matches!(id, 0 | 4 | 6),
                    2 => matches!(id, 1 | 4 | 6),
                    0 => matches!(id, 4..=6),
                    _ => matches!(id, 0 | 1 | 3 | 4 | 6),
                }
            }
            AudioState::Pcm { .. } => false,
        }
    }

    fn try_at(&self, win: &mut Window<'_>, pos: u64, end: u64) -> Option<u64> {
        match self {
            AudioState::Aac { config, max_len } => {
                let b = win.get(pos, *max_len);
                if !self.quick_start(b) {
                    return None;
                }
                let n = aac::frame_length(b, config)? as u64;
                (pos + n <= end).then_some(n)
            }
            AudioState::Pcm { .. } => None,
        }
    }
}

// ---------------------------------------------------------------------------
// The scan
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct VSample {
    offset: u64,
    len: u32,
    key: bool,
    intra: bool,
    poc: i32,
    epoch: u32,
}

#[derive(Debug, Clone, Copy)]
struct ASample {
    offset: u64,
    len: u32,
    count: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Track {
    Video,
    Audio,
}

struct Scanner<'a> {
    win: Window<'a>,
    end: u64,
    video: Option<VideoState>,
    audio: Option<AudioState>,
    in_band: bool,
    vs: Vec<VSample>,
    aus: Vec<ASample>,
    epoch: u32,
    gap_pending: bool,
    unclassified: Vec<Range>,
    run: Option<(Track, u32)>,
    video_chunk_max: u32,
    audio_chunk_max: u32,
    notes: Vec<String>,
    skipped_structure: u64,
}

/// A candidate classification at one position.
#[derive(Clone)]
enum Pick {
    Video(VideoCand),
    Audio(u64),
}

impl Scanner<'_> {
    /// Container structure at `pos` that should be skipped: returns its
    /// length (`None` when nothing recognisable is there).
    fn structure_at(&mut self, pos: u64) -> Option<u64> {
        let end = self.end;
        let b = self.win.get(pos, 32);
        let h = boxes::parse_header(b, pos)?;
        let skip_whole: &[&[u8; 4]] = &[
            b"free", b"skip", b"wide", b"moov", b"udta", b"uuid", b"junk", b"pnot", b"meta",
            b"PICT", b"sidx",
        ];
        if h.is(b"mdat") {
            return Some(h.full_header_len());
        }
        if skip_whole.iter().any(|k| h.is(k)) {
            let len = if h.size == 0 { end - pos } else { h.size };
            if pos + len <= end && len >= 8 {
                return Some(len);
            }
        }
        None
    }

    fn video_at(&mut self, pos: u64) -> Option<VideoCand> {
        let end = self.end;
        let in_band = self.in_band;
        let v = self.video.as_ref()?;
        v.try_at(&mut self.win, pos, end, in_band)
    }

    fn audio_at(&mut self, pos: u64) -> Option<u64> {
        let end = self.end;
        let a = self.audio.as_ref()?;
        a.try_at(&mut self.win, pos, end)
    }

    /// Is something valid at `pos` (a sample start, structure, or the end)?
    fn valid_next(&mut self, pos: u64) -> bool {
        if pos >= self.end {
            return true;
        }
        if let Some(v) = self.video_at(pos) {
            if self.video.as_ref().is_some_and(|s| s.continuity_ok(&v)) || v.keyframe {
                return true;
            }
        }
        if self.audio_at(pos).is_some() {
            return true;
        }
        self.structure_at(pos).is_some()
    }

    fn expected(&self) -> Track {
        match self.run {
            Some((Track::Audio, n)) if n < self.audio_chunk_max.max(1) => Track::Audio,
            Some((Track::Audio, _)) => Track::Video,
            Some((Track::Video, n)) if n < self.video_chunk_max.max(1) => Track::Video,
            Some((Track::Video, _)) => Track::Audio,
            None => Track::Video,
        }
    }

    fn classify(&mut self, pos: u64) -> Option<Pick> {
        let v = self.video_at(pos);
        let a = self.audio_at(pos);
        match (v, a) {
            (Some(v), Some(a)) => {
                let lv = self.valid_next(pos + v.len);
                let la = self.valid_next(pos + a);
                match (lv, la) {
                    (true, false) => Some(Pick::Video(v)),
                    (false, true) => Some(Pick::Audio(a)),
                    _ => {
                        if self.expected() == Track::Audio {
                            Some(Pick::Audio(a))
                        } else {
                            Some(Pick::Video(v))
                        }
                    }
                }
            }
            (Some(v), None) => {
                let strong =
                    !self.gap_pending && self.video.as_ref().is_some_and(|s| s.continuity_ok(&v));
                if strong || self.valid_next(pos + v.len) {
                    Some(Pick::Video(v))
                } else {
                    None
                }
            }
            (None, Some(a)) => {
                if !self.gap_pending || self.valid_next(pos + a) {
                    Some(Pick::Audio(a))
                } else {
                    None
                }
            }
            (None, None) => None,
        }
    }

    fn bump_run(&mut self, t: Track, n: u32) {
        self.run = match self.run {
            Some((rt, c)) if rt == t => Some((t, c + n)),
            _ => Some((t, n)),
        };
    }

    fn accept(&mut self, pos: u64, pick: Pick) -> u64 {
        match pick {
            Pick::Video(c) => {
                let in_band = self.in_band;
                let gap = std::mem::take(&mut self.gap_pending);
                let v = self.video.as_mut().expect("video state");
                if gap {
                    v.reset_after_gap();
                }
                let (poc, new_epoch) = v.accept(&c, in_band);
                if (new_epoch || gap) && !self.vs.is_empty() {
                    self.epoch += 1;
                }
                self.vs.push(VSample {
                    offset: pos,
                    len: c.len as u32,
                    key: c.keyframe,
                    intra: c.intra,
                    poc,
                    epoch: self.epoch,
                });
                self.bump_run(Track::Video, 1);
                c.len
            }
            Pick::Audio(n) => {
                self.gap_pending = false;
                self.aus.push(ASample {
                    offset: pos,
                    len: n as u32,
                    count: 1,
                });
                self.bump_run(Track::Audio, 1);
                n
            }
        }
    }

    /// Search forward from `from` for a confirmed sample start, examining
    /// at most `limit` bytes. Returns the position found.
    fn resync(&mut self, from: u64, limit: u64) -> Option<u64> {
        let stop = from.saturating_add(limit).min(self.end);
        let mut q = from;
        while q < stop {
            // Fetch a block and prefilter cheaply.
            let block: Vec<u8> = self.win.get(q, 4096 + 16).to_vec();
            if block.is_empty() {
                // Unreadable byte: skip a sector.
                q = (q / 512 + 1) * 512;
                continue;
            }
            let usable = block.len().saturating_sub(16).max(1).min(block.len());
            for i in 0..usable {
                let at = q + i as u64;
                if at >= stop {
                    return None;
                }
                let b = &block[i..];
                let room = self.end - at;
                let vq = self.video.as_ref().is_some_and(|v| v.quick_start(b, room));
                let aq = self.audio.as_ref().is_some_and(|a| a.quick_start(b));
                if !vq && !aq {
                    if b.len() >= 8 && boxes::is_top_level_type(&[b[4], b[5], b[6], b[7]]) {
                        if let Some(skip) = self.structure_at(at) {
                            if skip > 0 {
                                return Some(at);
                            }
                        }
                    }
                    continue;
                }
                if vq {
                    if let Some(v) = self.video_at(at) {
                        if self.valid_next(at + v.len) {
                            return Some(at);
                        }
                    }
                }
                if aq {
                    if let Some(n) = self.audio_at(at) {
                        if self.valid_next(at + n) {
                            return Some(at);
                        }
                    }
                }
            }
            q += usable as u64;
        }
        None
    }

    /// Next confirmed *video* start within `limit` bytes (PCM placement).
    fn next_video(&mut self, from: u64, limit: u64) -> Option<u64> {
        let stop = from.saturating_add(limit).min(self.end);
        let mut q = from;
        while q < stop {
            let block: Vec<u8> = self.win.get(q, 4096 + 16).to_vec();
            if block.is_empty() {
                return None;
            }
            let usable = block.len().saturating_sub(16).max(1).min(block.len());
            for i in 0..usable {
                let at = q + i as u64;
                if at >= stop {
                    return None;
                }
                let room = self.end - at;
                if !self
                    .video
                    .as_ref()
                    .is_some_and(|v| v.quick_start(&block[i..], room))
                {
                    continue;
                }
                if let Some(v) = self.video_at(at) {
                    let ok = self.video.as_ref().is_some_and(|s| s.continuity_ok(&v));
                    if ok || v.keyframe {
                        return Some(at);
                    }
                }
            }
            q += usable as u64;
        }
        None
    }
}

/// Locate the media region of the damaged file: `(start, end, notes)`.
fn media_region(src: &dyn Source) -> Result<(u64, u64, Vec<String>)> {
    let size = src.size();
    let mut notes = Vec::new();
    let (tops, stop) = walk_top_level(src, 0, size, &|k| boxes::is_top_level_type(k))?;
    if let Some(m) = tops.iter().find(|t| t.header.is(b"mdat")) {
        let start = m.header.payload_offset();
        let claimed_ok = m.header.size != 0 && !m.truncated;
        if !claimed_ok {
            notes.push(if m.header.size == 0 {
                "mdat size is 0: scanning to the end of the file".to_string()
            } else {
                "mdat runs past the end of the file (truncated recording)".to_string()
            });
            return Ok((start, size, notes));
        }
        // A size that ends exactly at the file end or at another box is
        // trusted; anything else (zeroed/garbage size) scans to the end.
        let end = m.end;
        let next_ok = end == size
            || tops
                .iter()
                .any(|t| t.header.offset == end && !t.header.is(b"mdat"));
        if next_ok {
            return Ok((start, end, notes));
        }
        notes.push("mdat size is inconsistent: scanning to the end of the file".into());
        return Ok((start, size, notes));
    }
    // No mdat header: start after the recognisable leading boxes.
    let start = match stop {
        boxes::TopStop::Garbage(p) | boxes::TopStop::UnknownType(p) => p,
        boxes::TopStop::Truncated => tops.last().map(|t| t.header.payload_offset()).unwrap_or(0),
        _ => tops.last().map(|t| t.end).unwrap_or(0),
    };
    notes.push(if tops.is_empty() {
        "no box structure: treating the whole file as media data".to_string()
    } else {
        "no mdat header: media data assumed after the leading boxes".to_string()
    });
    Ok((start.min(size), size, notes))
}

/// Scan the damaged file and describe the rebuilt movie without writing it.
pub fn rebuild_to_splice(
    damaged: &SharedSource,
    reference: &Reference,
    opts: &RebuildOptions,
) -> Result<(SpliceSource, RebuildReport)> {
    let src: &dyn Source = &**damaged;
    let (start, end, mut notes) = media_region(src)?;
    let video = reference
        .video
        .as_ref()
        .and_then(|v| VideoState::new(&v.codec));
    let audio = reference
        .audio
        .as_ref()
        .and_then(|a| AudioState::new(&a.codec));
    let mut sc = Scanner {
        win: Window::new(src, end),
        end,
        video,
        audio,
        in_band: opts.allow_in_band_parameter_sets,
        vs: Vec::new(),
        aus: Vec::new(),
        epoch: 0,
        gap_pending: false,
        unclassified: Vec::new(),
        run: None,
        video_chunk_max: reference.typical_video_chunk.max,
        audio_chunk_max: reference.typical_audio_chunk.max,
        notes: Vec::new(),
        skipped_structure: 0,
    };
    let stop_limit = if opts.stop_at_first_unclassified_mb == 0 {
        u64::MAX
    } else {
        opts.stop_at_first_unclassified_mb.saturating_mul(1 << 20)
    };
    let max_video_ticks = opts.max_duration_s.and_then(|s| {
        reference
            .video
            .as_ref()
            .map(|v| s.saturating_mul(v.trak.timescale as u64))
    });
    let max_audio_ticks = opts.max_duration_s.and_then(|s| {
        reference
            .audio
            .as_ref()
            .map(|a| s.saturating_mul(a.trak.timescale as u64))
    });
    let vdur = reference.frame_duration.max(1) as u64;
    let adur = reference.audio_frame_duration.max(1) as u64;
    let is_pcm = matches!(sc.audio, Some(AudioState::Pcm { .. }));
    let pcm_bpf = match sc.audio {
        Some(AudioState::Pcm { bpf }) => bpf as u64,
        _ => 0,
    };
    let pcm_window = reference
        .audio
        .as_ref()
        .map(|a| {
            let chunk_bytes = a.chunk.max.max(1) as u64 * pcm_bpf.max(1);
            (chunk_bytes * 4).clamp(1 << 20, 64 << 20)
        })
        .unwrap_or(1 << 20);
    let mut stopped_at = None;
    let mut pos = start;
    let mut unclassified_run_start: Option<u64> = None;
    while pos < end {
        if let (Some(m), true) = (max_video_ticks, !sc.vs.is_empty()) {
            if sc.vs.len() as u64 * vdur >= m {
                notes.push("stopped at the duration limit".into());
                break;
            }
        }
        if let (Some(m), true) = (max_audio_ticks, sc.vs.is_empty()) {
            let ticks: u64 = sc.aus.iter().map(|a| a.count as u64).sum::<u64>() * adur;
            if ticks >= m && !sc.aus.is_empty() {
                notes.push("stopped at the duration limit".into());
                break;
            }
        }
        if let Some(pick) = sc.classify(pos) {
            if let Some(us) = unclassified_run_start.take() {
                sc.unclassified.push(Range::new(us, pos));
            }
            pos += sc.accept(pos, pick);
            continue;
        }
        if let Some(skip) = sc.structure_at(pos) {
            if let Some(us) = unclassified_run_start.take() {
                sc.unclassified.push(Range::new(us, pos));
            }
            sc.skipped_structure += skip;
            pos += skip;
            continue;
        }
        if is_pcm {
            // PCM: the bytes up to the next video sample are audio.
            let next = if sc.video.is_some() {
                sc.next_video(pos + 1, pcm_window)
            } else {
                Some(end)
            };
            let next = match next {
                Some(n) => Some(n),
                None if sc.video.is_some() && end - pos <= pcm_window => Some(end),
                None => None,
            };
            if let Some(n) = next {
                let frames = (n - pos) / pcm_bpf;
                if frames > 0 && frames <= u32::MAX as u64 {
                    if let Some(us) = unclassified_run_start.take() {
                        sc.unclassified.push(Range::new(us, pos));
                    }
                    let len = frames * pcm_bpf;
                    // Split very long runs so sizes stay within u32.
                    let mut left = frames;
                    let mut at = pos;
                    while left > 0 {
                        let take = left.min((u32::MAX as u64 / pcm_bpf).min(1 << 20));
                        sc.aus.push(ASample {
                            offset: at,
                            len: (take * pcm_bpf) as u32,
                            count: take as u32,
                        });
                        at += take * pcm_bpf;
                        left -= take;
                    }
                    sc.bump_run(Track::Audio, frames as u32);
                    sc.gap_pending = false;
                    if pos + len < n {
                        sc.unclassified.push(Range::new(pos + len, n));
                    }
                    pos = n;
                    continue;
                }
            }
        }
        // Nothing here: resynchronise.
        let run_start = *unclassified_run_start.get_or_insert(pos);
        sc.gap_pending = true;
        let mut found = None;
        let mut q = pos + 1;
        while q < end {
            if q - run_start >= stop_limit {
                break;
            }
            let step = (1u64 << 20).min(stop_limit.saturating_sub(q - run_start));
            if let Some(n) = sc.resync(q, step) {
                found = Some(n);
                break;
            }
            q = q.saturating_add(step);
        }
        match found {
            Some(n) => {
                pos = n;
            }
            None => {
                if q < end {
                    stopped_at = Some(run_start);
                    notes.push(format!(
                        "stopped: {} MiB of unclassifiable data at offset {run_start}",
                        opts.stop_at_first_unclassified_mb
                    ));
                }
                pos = end;
            }
        }
    }
    if let Some(us) = unclassified_run_start.take() {
        sc.unclassified
            .push(Range::new(us, stopped_at.map_or(end, |_| end)));
    }
    for r in sc.unclassified.iter().take(16) {
        log::warn!(
            "rebuild: {} unclassified bytes at {}..{} dropped",
            r.len(),
            r.start,
            r.end
        );
    }
    for n in &notes {
        log::debug!("rebuild: {n}");
    }
    notes.append(&mut sc.notes);
    if sc.skipped_structure > 0 {
        notes.push(format!(
            "skipped {} bytes of container structure inside the media data",
            sc.skipped_structure
        ));
    }
    if sc.vs.is_empty() && sc.aus.is_empty() {
        return Err(Error::corrupt(
            "mdat",
            "no sample matched the reference (different camera, codec or settings?)",
        ));
    }
    if reference.video.is_some() && sc.vs.is_empty() {
        notes.push(
            "no video sample parsed with the reference's parameter sets: the reference may come from different settings"
                .into(),
        );
    }
    let (movie, mut report) = build_movie(reference, &sc, &mut notes)?;
    report.unclassified_ranges = sc.unclassified.clone();
    report.bytes_unclassified = sc.unclassified.iter().map(|r| r.len()).sum();
    report.stopped_at = stopped_at;
    report.notes = notes;
    let splice = assemble(&movie, damaged.clone())?;
    Ok((splice, report))
}

fn build_movie(
    reference: &Reference,
    sc: &Scanner<'_>,
    notes: &mut Vec<String>,
) -> Result<(MovieOut, RebuildReport)> {
    let mut report = RebuildReport::default();
    let mut tracks = Vec::new();
    let mut next_id = 1u32;
    let mut video_dur_s = 0f64;
    let mut audio_dur_s = 0f64;
    // A decoder cannot start before the first random-access picture:
    // drop the video samples before it (head of the recording lost) and
    // delay the video with an empty edit so audio stays in sync.
    let first_rap = sc.vs.iter().position(|s| s.key || s.intra);
    let lead = first_rap.unwrap_or(sc.vs.len());
    if lead > 0 && !sc.vs.is_empty() {
        notes.push(format!(
            "dropped {lead} video sample(s) before the first decodable picture"
        ));
    }
    let vs: Vec<VSample> = sc.vs[lead..].to_vec();
    if let (Some(vt), false) = (&reference.video, vs.is_empty()) {
        let n = vs.len();
        // Display ranks by (epoch, poc), ties by decode order.
        let mut order: Vec<usize> = (0..n).collect();
        order.sort_by_key(|&i| (vs[i].epoch, vs[i].poc, i));
        let mut rank = vec![0i64; n];
        for (r, &i) in order.iter().enumerate() {
            rank[i] = r as i64;
        }
        let computed_shift = (0..n).map(|i| i as i64 - rank[i]).max().unwrap_or(0).max(0);
        let reorders = (0..n).any(|i| rank[i] != i as i64);
        let shift = if reorders {
            computed_shift.max(vt.reorder_delay as i64)
        } else {
            0
        };
        let mut dur = vt.sample_delta;
        let ts = vt.trak.timescale.max(1) as f64;
        // Variable-frame-rate references: stretch video to the audio length.
        if vt.variable_rate {
            if let Some(at) = &reference.audio {
                let audio_ticks: u64 =
                    sc.aus.iter().map(|a| a.count as u64).sum::<u64>() * at.sample_delta as u64;
                let audio_s = audio_ticks as f64 / at.trak.timescale.max(1) as f64;
                let video_s = n as f64 * dur as f64 / ts;
                if audio_s > 0.0 && (audio_s - video_s).abs() > 0.5 {
                    let d = (audio_s * ts / n as f64).round() as u32;
                    if d > 0 {
                        notes.push(format!(
                            "variable frame rate reference: video frame duration stretched from {dur} to {d} ticks to match the audio"
                        ));
                        dur = d;
                    }
                }
            }
        }
        let mut samples: Vec<OutSample> = (0..n)
            .map(|i| {
                let s = &vs[i];
                let off = (rank[i] - i as i64 + shift) * dur as i64;
                OutSample {
                    src_offset: s.offset,
                    size: s.len,
                    duration: dur,
                    cts_offset: off.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
                    keyframe: s.key,
                    count: 1,
                }
            })
            .collect();
        if let Some(f) = samples.first_mut() {
            // The first kept picture is where decoding starts.
            f.keyframe = true;
        }
        report.video_samples = n as u64;
        report.keyframes = samples.iter().filter(|s| s.keyframe).count() as u64;
        if report.keyframes == 0 {
            notes.push("no keyframe found in the recovered video".into());
        }
        report.bytes_classified += samples.iter().map(|s| s.size as u64).sum::<u64>();
        video_dur_s = n as f64 * dur as f64 / ts;
        let mut elst: Vec<Elst> = Vec::new();
        if lead > 0 {
            let movie_ts = reference.moov.mvhd.timescale.max(1) as u128;
            let lead_ticks = lead as u128 * dur as u128;
            elst.push(Elst {
                segment_duration: (lead_ticks * movie_ts / ts as u128) as u64,
                media_time: -1,
                rate_integer: 1,
                rate_fraction: 0,
            });
        }
        if shift > 0 || lead > 0 {
            elst.push(Elst {
                segment_duration: 0,
                media_time: shift * dur as i64,
                rate_integer: 1,
                rate_fraction: 0,
            });
        }
        let elst = (!elst.is_empty()).then_some(elst);
        tracks.push(TrackOut {
            template: vt.trak.clone(),
            track_id: next_id,
            samples,
            elst,
            fit_elst: true,
            constant_size: None,
        });
        next_id += 1;
    }
    if let (Some(at), false) = (&reference.audio, sc.aus.is_empty()) {
        let dur = at.sample_delta;
        let pcm = matches!(at.codec, Codec::Pcm { .. });
        let bpf = match at.codec {
            Codec::Pcm { bytes_per_frame } => Some(bytes_per_frame),
            _ => None,
        };
        let samples: Vec<OutSample> = sc
            .aus
            .iter()
            .map(|a| OutSample {
                src_offset: a.offset,
                size: a.len,
                duration: dur,
                cts_offset: 0,
                keyframe: true,
                count: a.count,
            })
            .collect();
        let frames: u64 = sc.aus.iter().map(|a| a.count as u64).sum();
        report.audio_samples = frames;
        report.bytes_classified += samples.iter().map(|s| s.size as u64).sum::<u64>();
        audio_dur_s = frames as f64 * dur as f64 / at.trak.timescale.max(1) as f64;
        let elst = at.edit_media_time.filter(|&m| m > 0).map(|m| {
            vec![Elst {
                segment_duration: 0,
                media_time: m,
                rate_integer: 1,
                rate_fraction: 0,
            }]
        });
        tracks.push(TrackOut {
            template: at.trak.clone(),
            track_id: next_id,
            samples,
            elst,
            fit_elst: true,
            constant_size: if pcm { bpf } else { None },
        });
    }
    if tracks.is_empty() {
        return Err(Error::corrupt("mdat", "no samples recovered"));
    }
    if !reference.other_tracks_dropped.is_empty() {
        notes.push(format!(
            "reference tracks not rebuilt: {}",
            reference
                .other_tracks_dropped
                .iter()
                .map(boxes::fourcc_str)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    report.duration_ms = (video_dur_s.max(audio_dur_s) * 1000.0).round() as u64;
    let mut mvhd = reference.moov.mvhd.clone();
    mvhd.creation_time = 0;
    mvhd.modification_time = 0;
    if mvhd.timescale == 0 {
        mvhd.timescale = 1000;
    }
    let ftyp = if reference.ftyp.is_empty() {
        synth_ftyp()
    } else {
        reference.ftyp.clone()
    };
    Ok((
        MovieOut {
            ftyp,
            mvhd,
            tracks,
            user_boxes: Vec::new(),
            max_chunk_samples: 1024,
        },
        report,
    ))
}

fn synth_ftyp() -> Vec<u8> {
    let mut b = crate::writer::BoxBuf::new();
    b.start(b"ftyp");
    b.bytes(b"isom");
    b.u32(0x200);
    for k in [b"isom", b"iso2", b"mp41"] {
        b.bytes(k);
    }
    b.end();
    b.buf
}

/// Rebuild `damaged` (whose `moov` is missing or unusable) with the
/// template learnt from `reference`, writing a complete file to `out`.
pub fn rebuild(
    damaged: &SharedSource,
    reference: &Reference,
    out: &mut dyn Write,
    opts: &RebuildOptions,
) -> Result<RebuildReport> {
    let (splice, mut report) = rebuild_to_splice(damaged, reference, opts)?;
    report.output_len = splice.write_to(out)?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskrescue_core::source::MemSource;
    use std::sync::Arc;

    #[test]
    fn modes_and_stats() {
        assert_eq!(mode_of([(1024, 141u64), (640, 1)].into_iter()), 1024);
        let s = size_stats(vec![5, 1, 3, 2, 4]);
        assert_eq!((s.min, s.max, s.median), (1, 5, 3));
    }

    #[test]
    fn learn_rejects_garbage() {
        let src: SharedSource = Arc::new(MemSource::new(vec![0xAB; 4096]));
        assert!(learn(&src).is_err());
        let empty: SharedSource = Arc::new(MemSource::new(Vec::new()));
        assert!(learn(&empty).is_err());
    }

    #[test]
    fn filler() {
        assert!(filler_ok(&[0xFF, 0xFF, 0x80]));
        assert!(!filler_ok(&[0xFF, 0x00, 0x80]));
    }
}
