//! AAC raw data blocks as stored in MP4 `mp4a` samples (ISO/IEC 14496-3
//! subpart 4, `raw_data_block()`).
//!
//! An MP4 AAC sample has no length field and no sync word, so the moov
//! rebuild needs another way to find where a frame ends. This module walks
//! the complete bitstream *syntax* of a frame — element ids, `ics_info`,
//! section data, Huffman-coded scale factors, pulse/TNS data and the
//! Huffman-coded spectral data — without dequantising or transforming
//! anything. The walk ends at the `END` element; the frame length is the
//! byte-aligned bit position after it. Along the way every range rule a
//! decoder enforces is checked (reserved bit, `max_sfb` against the band
//! count, reserved codebook 12, scale factors within 0..=255, TNS order,
//! escape-sequence length, prediction not allowed in AAC-LC, element order
//! matching the channel configuration), so random bytes are rejected with
//! very high probability and a real frame yields its exact length.
//!
//! Supported: AAC Main, LC, LTP and the LC core of HE-AAC v1/v2 (SBR/PS
//! data lives in FIL elements and is skipped by its count), channel
//! configurations 0..=7, 1024- and 960-sample frames. Not supported (the
//! walker returns `None`): coupling channel elements (CCE), SSR gain
//! control, and the error-resilient object types (ER-LC, LD, ELD).
//! [`looks_like_raw_frame`] still gives a heuristic score for those.

mod tables;

use std::sync::OnceLock;

use diskrescue_core::util::bits::BitReader;

use tables::*;

/// Sampling rates by `samplingFrequencyIndex`.
pub const SAMPLE_RATES: [u32; 13] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
];

/// Parsed `AudioSpecificConfig`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AudioSpecificConfig {
    /// Core audio object type (2 = LC; for HE-AAC this is the LC core).
    pub object_type: u8,
    /// samplingFrequencyIndex of the core (derived from the rate when the
    /// config carries an explicit 24-bit rate).
    pub sample_rate_index: u8,
    /// Core sampling rate in Hz.
    pub sample_rate: u32,
    /// channelConfiguration (0 = defined by a PCE in the stream).
    pub channels: u8,
    /// Samples per frame: 1024 or 960.
    pub frame_length: u32,
    /// SBR signalled (explicitly or via the sync extension).
    pub sbr: bool,
    /// Parametric stereo signalled (AOT 29).
    pub ps: bool,
}

/// How plausible a byte string is as one AAC raw data block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Score {
    /// Not an AAC frame.
    Reject,
    /// Weak evidence (heuristic checks only, or the syntax ends early).
    Low,
    /// Heuristic checks pass but the frame could not be fully walked.
    Medium,
    /// The full syntax walk ends exactly at the end of the data.
    High,
}

fn rate_to_index(rate: u32) -> u8 {
    const T: [(u32, u8); 11] = [
        (92017, 0),
        (75132, 1),
        (55426, 2),
        (46009, 3),
        (37566, 4),
        (27713, 5),
        (23004, 6),
        (18783, 7),
        (13856, 8),
        (11502, 9),
        (9391, 10),
    ];
    T.iter()
        .find(|(min, _)| rate >= *min)
        .map(|x| x.1)
        .unwrap_or(11)
}

fn read_aot(r: &mut BitReader<'_>) -> Option<u8> {
    let a = r.read_bits(5)? as u8;
    if a == 31 {
        Some(32 + r.read_bits(6)? as u8)
    } else {
        Some(a)
    }
}

fn read_rate(r: &mut BitReader<'_>) -> Option<(u8, u32)> {
    let i = r.read_bits(4)? as u8;
    if i == 15 {
        let rate = r.read_bits(24)?;
        if rate == 0 {
            return None;
        }
        Some((rate_to_index(rate), rate))
    } else {
        Some((i, *SAMPLE_RATES.get(i as usize)?))
    }
}

/// Parse an `AudioSpecificConfig` (from `esds`). Returns `None` for an
/// empty or malformed config.
pub fn parse_audio_specific_config(bytes: &[u8]) -> Option<AudioSpecificConfig> {
    if bytes.len() < 2 {
        return None;
    }
    let mut r = BitReader::new(bytes);
    let mut aot = read_aot(&mut r)?;
    let (mut sri, mut rate) = read_rate(&mut r)?;
    let channels = r.read_bits(4)? as u8;
    let mut cfg = AudioSpecificConfig {
        channels,
        frame_length: 1024,
        ..Default::default()
    };
    if aot == 5 || aot == 29 {
        cfg.sbr = true;
        cfg.ps = aot == 29;
        let _ext = read_rate(&mut r)?;
        aot = read_aot(&mut r)?;
    }
    if matches!(aot, 1..=4 | 6 | 7 | 17 | 19..=23) {
        let flen = r.read_flag()?;
        cfg.frame_length = if flen { 960 } else { 1024 };
        if r.read_flag()? {
            r.skip(14)?;
        }
        let _ext = r.read_flag();
    }
    // Backward-compatible SBR signalling via the sync extension.
    if r.remaining() >= 16 && r.peek_bits(11) == Some(0x2B7) {
        r.skip(11)?;
        let ext = read_aot(&mut r)?;
        if ext == 5 {
            cfg.sbr = r.read_flag().unwrap_or(false);
        }
    }
    if rate == 0 || sri as usize >= SAMPLE_RATES.len() {
        // Explicit rates map to the nearest index for band tables.
        sri = rate_to_index(rate);
        rate = rate.max(1);
    }
    cfg.object_type = aot;
    cfg.sample_rate_index = sri;
    cfg.sample_rate = rate;
    Some(cfg)
}

// ---------------------------------------------------------------------------
// Huffman decoding
// ---------------------------------------------------------------------------

/// Binary decode tree: node -> [child0, child1]; a child `> 0` is a node
/// index, `< 0` is `-(symbol + 1)`, `0` is "no code".
struct Tree {
    nodes: Vec<[i32; 2]>,
}

impl Tree {
    fn build(codes: &[u32], bits: &[u8]) -> Tree {
        let mut nodes = vec![[0i32; 2]];
        for (sym, (&code, &len)) in codes.iter().zip(bits).enumerate() {
            let mut n = 0usize;
            for i in (0..len).rev() {
                let b = ((code >> i) & 1) as usize;
                if i == 0 {
                    nodes[n][b] = -(sym as i32 + 1);
                } else {
                    let next = nodes[n][b];
                    if next > 0 {
                        n = next as usize;
                    } else {
                        nodes.push([0, 0]);
                        let idx = nodes.len() - 1;
                        nodes[n][b] = idx as i32;
                        n = idx;
                    }
                }
            }
        }
        Tree { nodes }
    }

    #[inline]
    fn decode(&self, r: &mut BitReader<'_>) -> Option<usize> {
        let mut n = 0usize;
        for _ in 0..24 {
            let b = r.read_bit()? as usize;
            let next = self.nodes[n][b];
            if next < 0 {
                return Some((-next - 1) as usize);
            }
            if next == 0 {
                return None;
            }
            n = next as usize;
        }
        None
    }
}

/// Per-codebook decode data.
struct Codebook {
    tree: Tree,
    /// Number of sign bits that follow each symbol (unsigned books).
    signs: Vec<u8>,
    /// Number of escape sequences that follow each symbol (book 11).
    escapes: Vec<u8>,
    /// Values per symbol (4 or 2).
    dim: usize,
}

struct Books {
    sf: Tree,
    spec: Vec<Codebook>,
}

fn books() -> &'static Books {
    static B: OnceLock<Books> = OnceLock::new();
    B.get_or_init(|| {
        let sf = Tree::build(&SF_CODES, &SF_BITS);
        let widen = |c: &[u16]| c.iter().map(|&x| x as u32).collect::<Vec<_>>();
        let specs: [(&[u16], &[u8]); 11] = [
            (&SPEC_CODES_1, &SPEC_BITS_1),
            (&SPEC_CODES_2, &SPEC_BITS_2),
            (&SPEC_CODES_3, &SPEC_BITS_3),
            (&SPEC_CODES_4, &SPEC_BITS_4),
            (&SPEC_CODES_5, &SPEC_BITS_5),
            (&SPEC_CODES_6, &SPEC_BITS_6),
            (&SPEC_CODES_7, &SPEC_BITS_7),
            (&SPEC_CODES_8, &SPEC_BITS_8),
            (&SPEC_CODES_9, &SPEC_BITS_9),
            (&SPEC_CODES_10, &SPEC_BITS_10),
            (&SPEC_CODES_11, &SPEC_BITS_11),
        ];
        let mut spec = Vec::new();
        for (i, (c, b)) in specs.iter().enumerate() {
            let cb = i + 1;
            let n = c.len();
            let (dim, unsigned, base) = match cb {
                1 | 2 => (4, false, 3),
                3 | 4 => (4, true, 3),
                5 | 6 => (2, false, 9),
                7 | 8 => (2, true, 8),
                9 | 10 => (2, true, 13),
                _ => (2, true, 17),
            };
            let mut signs = vec![0u8; n];
            let mut escapes = vec![0u8; n];
            for idx in 0..n {
                let mut v = idx;
                let mut vals = [0usize; 4];
                for k in (0..dim).rev() {
                    vals[k] = v % base;
                    v /= base;
                }
                if unsigned {
                    signs[idx] = vals[..dim].iter().filter(|&&x| x != 0).count() as u8;
                }
                if cb == 11 {
                    escapes[idx] = vals[..dim].iter().filter(|&&x| x == 16).count() as u8;
                }
            }
            spec.push(Codebook {
                tree: Tree::build(&widen(c), b),
                signs,
                escapes,
                dim,
            });
        }
        Books { sf, spec }
    })
}

// ---------------------------------------------------------------------------
// Band tables
// ---------------------------------------------------------------------------

fn swb_long(sri: u8, frame_length: u32) -> Option<&'static [u16]> {
    Some(if frame_length == 960 {
        match sri {
            0 | 1 => &SWB_960_96[..],
            2 => &SWB_960_64[..],
            3..=5 => &SWB_960_48[..],
            6 | 7 => &SWB_960_24[..],
            8..=10 => &SWB_960_16[..],
            11 | 12 => &SWB_960_8[..],
            _ => return None,
        }
    } else {
        match sri {
            0 | 1 => &SWB_1024_96[..],
            2 => &SWB_1024_64[..],
            3 | 4 => &SWB_1024_48[..],
            5 => &SWB_1024_32[..],
            6 | 7 => &SWB_1024_24[..],
            8..=10 => &SWB_1024_16[..],
            11 | 12 => &SWB_1024_8[..],
            _ => return None,
        }
    })
}

fn swb_short(sri: u8, frame_length: u32) -> Option<&'static [u16]> {
    Some(if frame_length == 960 {
        match sri {
            0..=2 => &SWB_120_96[..],
            3..=5 => &SWB_120_48[..],
            6 | 7 => &SWB_120_24[..],
            8..=10 => &SWB_120_16[..],
            11 | 12 => &SWB_120_8[..],
            _ => return None,
        }
    } else {
        match sri {
            0..=2 => &SWB_128_96[..],
            3..=5 => &SWB_128_48[..],
            6 | 7 => &SWB_128_24[..],
            8..=10 => &SWB_128_16[..],
            11 | 12 => &SWB_128_8[..],
            _ => return None,
        }
    })
}

const PRED_SFB_MAX: [u8; 13] = [33, 33, 38, 40, 40, 40, 41, 41, 37, 37, 37, 34, 34];

// ---------------------------------------------------------------------------
// Syntax walk
// ---------------------------------------------------------------------------

const EIGHT_SHORT: u32 = 2;
const ZERO_HCB: u8 = 0;
const NOISE_HCB: u8 = 13;
const INTENSITY_HCB2: u8 = 14;
const INTENSITY_HCB: u8 = 15;

#[derive(Clone, Default)]
struct IcsInfo {
    window_sequence: u32,
    max_sfb: usize,
    num_window_groups: usize,
    group_len: [u8; 8],
    swb: &'static [u16],
    num_swb: usize,
    ltp_present: bool,
}

struct Walker<'a, 'b> {
    r: &'b mut BitReader<'a>,
    cfg: &'b AudioSpecificConfig,
}

impl Walker<'_, '_> {
    fn ics_info(&mut self) -> Option<IcsInfo> {
        let r = &mut *self.r;
        if r.read_flag()? {
            return None; // ics_reserved_bit
        }
        let mut ics = IcsInfo {
            window_sequence: r.read_bits(2)?,
            ..Default::default()
        };
        r.read_flag()?; // window_shape
        if ics.window_sequence == EIGHT_SHORT {
            ics.max_sfb = r.read_bits(4)? as usize;
            ics.num_window_groups = 1;
            ics.group_len[0] = 1;
            for _ in 0..7 {
                if r.read_flag()? {
                    ics.group_len[ics.num_window_groups - 1] += 1;
                } else {
                    ics.num_window_groups += 1;
                    ics.group_len[ics.num_window_groups - 1] = 1;
                }
            }
            ics.swb = swb_short(self.cfg.sample_rate_index, self.cfg.frame_length)?;
        } else {
            ics.max_sfb = r.read_bits(6)? as usize;
            ics.num_window_groups = 1;
            ics.group_len[0] = 1;
            ics.swb = swb_long(self.cfg.sample_rate_index, self.cfg.frame_length)?;
            if r.read_flag()? {
                // predictor_data_present
                match self.cfg.object_type {
                    1 => {
                        if r.read_flag()? {
                            let g = r.read_bits(5)?;
                            if g == 0 || g > 30 {
                                return None;
                            }
                        }
                        let n = ics.max_sfb.min(
                            PRED_SFB_MAX[self.cfg.sample_rate_index.min(12) as usize] as usize,
                        );
                        r.skip(n)?;
                    }
                    4 => {
                        ics.ltp_present = r.read_flag()?;
                        if ics.ltp_present {
                            Self::ltp(r, ics.max_sfb)?;
                        }
                    }
                    _ => return None, // prediction is not allowed in AAC-LC
                }
            }
        }
        ics.num_swb = ics.swb.len() - 1;
        if ics.max_sfb > ics.num_swb {
            return None;
        }
        Some(ics)
    }

    fn ltp(r: &mut BitReader<'_>, max_sfb: usize) -> Option<()> {
        r.skip(11 + 3)?;
        r.skip(max_sfb.min(40))
    }

    /// Section data: returns band types `[group][sfb]`.
    fn section_data(&mut self, ics: &IcsInfo, allow_is: bool) -> Option<[[u8; 64]; 8]> {
        let r = &mut *self.r;
        let mut band = [[0u8; 64]; 8];
        let bits = if ics.window_sequence == EIGHT_SHORT {
            3
        } else {
            5
        };
        let esc = (1u32 << bits) - 1;
        for g in band.iter_mut().take(ics.num_window_groups) {
            let mut k = 0usize;
            let mut sections = 0;
            while k < ics.max_sfb {
                sections += 1;
                if sections > 64 {
                    return None;
                }
                let cb = r.read_bits(4)? as u8;
                if cb == 12 {
                    return None;
                }
                if (cb == INTENSITY_HCB || cb == INTENSITY_HCB2) && !allow_is {
                    return None;
                }
                let mut len = 0usize;
                loop {
                    let inc = r.read_bits(bits)?;
                    len += inc as usize;
                    if inc != esc {
                        break;
                    }
                    if len > 64 {
                        return None;
                    }
                }
                if k + len > ics.max_sfb {
                    return None;
                }
                for x in &mut g[k..k + len] {
                    *x = cb;
                }
                k += len;
            }
        }
        Some(band)
    }

    fn scale_factors(
        &mut self,
        ics: &IcsInfo,
        band: &[[u8; 64]; 8],
        global_gain: u32,
    ) -> Option<()> {
        let b = books();
        let r = &mut *self.r;
        let mut sf = global_gain as i32;
        let mut noise_first = true;
        for g in band.iter().take(ics.num_window_groups) {
            for &cb in g.iter().take(ics.max_sfb) {
                match cb {
                    ZERO_HCB => {}
                    INTENSITY_HCB | INTENSITY_HCB2 => {
                        b.sf.decode(r)?;
                    }
                    NOISE_HCB => {
                        if noise_first {
                            r.skip(9)?;
                            noise_first = false;
                        } else {
                            b.sf.decode(r)?;
                        }
                    }
                    _ => {
                        let d = b.sf.decode(r)? as i32 - 60;
                        sf += d;
                        if !(0..=255).contains(&sf) {
                            return None;
                        }
                    }
                }
            }
        }
        Some(())
    }

    fn pulse(&mut self, ics: &IcsInfo) -> Option<()> {
        let r = &mut *self.r;
        let n = r.read_bits(2)? as usize + 1;
        let start = r.read_bits(6)? as usize;
        if start >= ics.num_swb {
            return None;
        }
        let mut pos = ics.swb[start] as u32 + r.read_bits(5)?;
        if pos >= ics.swb[ics.num_swb] as u32 {
            return None;
        }
        r.skip(4)?;
        for _ in 1..n {
            pos += r.read_bits(5)?;
            if pos > 1023 {
                return None;
            }
            r.skip(4)?;
        }
        Some(())
    }

    fn tns(&mut self, ics: &IcsInfo) -> Option<()> {
        let r = &mut *self.r;
        let short = ics.window_sequence == EIGHT_SHORT;
        let windows = if short { 8 } else { 1 };
        let max_order = if short {
            7
        } else if self.cfg.object_type == 1 {
            20
        } else {
            12
        };
        for _ in 0..windows {
            let n_filt = r.read_bits(if short { 1 } else { 2 })?;
            if n_filt == 0 {
                continue;
            }
            let coef_res = r.read_bits(1)?;
            for _ in 0..n_filt {
                r.skip(if short { 4 } else { 6 })?;
                let order = r.read_bits(if short { 3 } else { 5 })?;
                if order > max_order {
                    return None;
                }
                if order > 0 {
                    r.read_flag()?;
                    let compress = r.read_bits(1)?;
                    let len = coef_res + 3 - compress;
                    r.skip((order * len) as usize)?;
                }
            }
        }
        Some(())
    }

    fn spectral(&mut self, ics: &IcsInfo, band: &[[u8; 64]; 8]) -> Option<()> {
        let b = books();
        let r = &mut *self.r;
        for (g, gb) in band.iter().enumerate().take(ics.num_window_groups) {
            let glen = ics.group_len[g] as usize;
            for (sfb, &cb) in gb.iter().enumerate().take(ics.max_sfb) {
                if cb == ZERO_HCB || cb >= NOISE_HCB {
                    continue;
                }
                let book = &b.spec[cb as usize - 1];
                let width = (ics.swb[sfb + 1] - ics.swb[sfb]) as usize * glen;
                let mut k = 0;
                while k < width {
                    let idx = book.tree.decode(r)?;
                    let signs = book.signs[idx] as usize;
                    if signs > 0 {
                        r.skip(signs)?;
                    }
                    for _ in 0..book.escapes[idx] {
                        let mut n = 0u32;
                        while r.read_flag()? {
                            n += 1;
                            if n > 8 {
                                return None;
                            }
                        }
                        r.skip(n as usize + 4)?;
                    }
                    k += book.dim;
                }
            }
        }
        Some(())
    }

    /// individual_channel_stream(); `shared` is the CPE common-window info.
    fn ics(&mut self, shared: Option<&IcsInfo>, allow_is: bool) -> Option<()> {
        let global_gain = self.r.read_bits(8)?;
        let ics = match shared {
            Some(s) => s.clone(),
            None => self.ics_info()?,
        };
        let band = self.section_data(&ics, allow_is)?;
        self.scale_factors(&ics, &band, global_gain)?;
        if self.r.read_flag()? {
            if ics.window_sequence == EIGHT_SHORT {
                return None;
            }
            self.pulse(&ics)?;
        }
        if self.r.read_flag()? {
            self.tns(&ics)?;
        }
        if self.r.read_flag()? {
            return None; // gain control (SSR only)
        }
        self.spectral(&ics, &band)
    }

    fn cpe(&mut self) -> Option<()> {
        let common = self.r.read_flag()?;
        if common {
            let ics = self.ics_info()?;
            if self.cfg.object_type == 4 && ics.ltp_present {
                // second channel's LTP data
                if self.r.read_flag()? {
                    Self::ltp(self.r, ics.max_sfb)?;
                }
            }
            match self.r.read_bits(2)? {
                0 | 2 => {}
                1 => {
                    let n = ics.num_window_groups * ics.max_sfb;
                    self.r.skip(n)?;
                }
                _ => return None,
            }
            self.ics(Some(&ics), true)?;
            self.ics(Some(&ics), true)
        } else {
            self.ics(None, true)?;
            self.ics(None, true)
        }
    }

    fn pce(&mut self) -> Option<()> {
        let r = &mut *self.r;
        r.skip(4 + 2 + 4)?;
        let front = r.read_bits(4)?;
        let side = r.read_bits(4)?;
        let back = r.read_bits(4)?;
        let lfe = r.read_bits(2)?;
        let assoc = r.read_bits(3)?;
        let cc = r.read_bits(4)?;
        if r.read_flag()? {
            r.skip(4)?; // mono mixdown element
        }
        if r.read_flag()? {
            r.skip(4)?; // stereo mixdown element
        }
        if r.read_flag()? {
            r.skip(3)?; // matrix mixdown idx + pseudo surround
        }
        r.skip(((front + side + back) * 5 + lfe * 4 + assoc * 4 + cc * 5) as usize)?;
        r.align_byte();
        let comment = r.read_bits(8)?;
        r.skip(comment as usize * 8)
    }
}

/// Element ids.
const SCE: u32 = 0;
const CPE: u32 = 1;
const CCE: u32 = 2;
const LFE: u32 = 3;
const DSE: u32 = 4;
const PCE: u32 = 5;
const FIL: u32 = 6;
const END: u32 = 7;

fn expected_elements(channels: u8) -> Option<&'static [u32]> {
    Some(match channels {
        1 => &[SCE],
        2 => &[CPE],
        3 => &[SCE, CPE],
        4 => &[SCE, CPE, SCE],
        5 => &[SCE, CPE, CPE],
        6 => &[SCE, CPE, CPE, LFE],
        7 => &[SCE, CPE, CPE, CPE, LFE],
        _ => return None,
    })
}

/// True when the walker supports this configuration.
pub fn walk_supported(cfg: &AudioSpecificConfig) -> bool {
    matches!(cfg.object_type, 1 | 2 | 4)
        && (cfg.sample_rate_index as usize) < SAMPLE_RATES.len()
        && cfg.channels <= 7
}

/// Walk one raw data block from the start of `data` and return its exact
/// length in bytes, or `None` when the bytes are not a valid frame for
/// `cfg` (or the frame continues past `data`).
pub fn frame_length(data: &[u8], cfg: &AudioSpecificConfig) -> Option<usize> {
    if !walk_supported(cfg) || data.is_empty() {
        return None;
    }
    let max_bits = (6144usize * (cfg.channels.max(1) as usize) + 64).min(data.len() * 8);
    let data = &data[..data.len().min(max_bits.div_ceil(8) + 8)];
    let mut r = BitReader::new(data);
    let mut seen: Vec<u32> = Vec::with_capacity(8);
    let mut elements = 0;
    loop {
        elements += 1;
        if elements > 64 {
            return None;
        }
        let id = r.read_bits(3)?;
        let mut w = Walker { r: &mut r, cfg };
        match id {
            SCE | LFE => {
                w.r.skip(4)?;
                w.ics(None, false)?;
                seen.push(id);
            }
            CPE => {
                w.r.skip(4)?;
                w.cpe()?;
                seen.push(id);
            }
            CCE => return None,
            DSE => {
                w.r.skip(4)?;
                let align = w.r.read_flag()?;
                let mut count = w.r.read_bits(8)? as usize;
                if count == 255 {
                    count += w.r.read_bits(8)? as usize;
                }
                if align {
                    w.r.align_byte();
                }
                w.r.skip(count * 8)?;
            }
            PCE => {
                if cfg.channels != 0 {
                    return None;
                }
                w.pce()?;
            }
            FIL => {
                let mut count = w.r.read_bits(4)? as usize;
                if count == 15 {
                    count += w.r.read_bits(8)? as usize;
                    count -= 1;
                }
                w.r.skip(count * 8)?;
            }
            _ => {
                // END
                r.align_byte();
                break;
            }
        }
        if r.position() > max_bits {
            return None;
        }
    }
    if let Some(exp) = expected_elements(cfg.channels) {
        if seen.as_slice() != exp {
            return None;
        }
    } else if seen.is_empty() {
        return None;
    }
    let len = r.position() / 8;
    (len <= data.len() && len > 0).then_some(len)
}

/// True when the last byte of a frame is consistent with an `END` element
/// followed by zero alignment bits (spec §2.7 filter; ~30 % of random bytes
/// pass).
pub fn end_pattern(last: u8) -> bool {
    (0..=5).any(|k| (last >> k) & 7 == 7 && last & ((1u8 << k) - 1) == 0)
        || last == 0xC0
        || last == 0x80
}

/// Score `data` as one complete AAC raw data block for `config`.
///
/// When the configuration is supported by the syntax walker the result is
/// `High` only if the walk ends exactly at `data.len()`, `Low` if a valid
/// frame ends earlier, `Reject` otherwise. For unsupported configurations
/// a heuristic is used: first element id consistent with the channel
/// configuration, `element_instance_tag`, `ics_reserved_bit == 0`,
/// `max_sfb` within the band count, a sane `global_gain`, a size within
/// the plausible range, and the END pattern on the last byte.
pub fn looks_like_raw_frame(data: &[u8], config: &AudioSpecificConfig) -> Score {
    if data.is_empty() || data.len() > 768 * config.channels.max(1) as usize + 8 {
        return Score::Reject;
    }
    if walk_supported(config) {
        return match frame_length(data, config) {
            Some(n) if n == data.len() => Score::High,
            Some(_) => Score::Low,
            None => Score::Reject,
        };
    }
    let mut r = BitReader::new(data);
    let Some(id) = r.read_bits(3) else {
        return Score::Reject;
    };
    let ok_id = match config.channels {
        1 => matches!(id, SCE | FIL | DSE),
        2 => matches!(id, CPE | FIL | DSE),
        _ => id != END && id != CCE,
    };
    if !ok_id {
        return Score::Reject;
    }
    if id == SCE || id == CPE {
        let tag = r.read_bits(4).unwrap_or(15);
        let gain_or_cw = if id == SCE {
            r.read_bits(8).unwrap_or(0)
        } else {
            r.read_bits(1).unwrap_or(0) * 128 + 100
        };
        if tag != 0 || !(40..=250).contains(&gain_or_cw) {
            return Score::Low;
        }
        if id == SCE && r.read_flag() != Some(false) {
            return Score::Reject;
        }
    }
    if end_pattern(*data.last().unwrap_or(&0)) {
        Score::Medium
    } else {
        Score::Low
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kraft_and_prefix(codes: &[u32], bits: &[u8]) {
        // Kraft sum exactly 1 with 2^-len terms, using a common denominator.
        let max = *bits.iter().max().unwrap() as u32;
        let sum: u64 = bits.iter().map(|&b| 1u64 << (max - b as u32)).sum();
        assert_eq!(sum, 1u64 << max);
        let mut v: Vec<String> = codes
            .iter()
            .zip(bits)
            .map(|(&c, &b)| format!("{:0width$b}", c, width = b as usize))
            .collect();
        v.sort();
        for w in v.windows(2) {
            assert!(!w[1].starts_with(&w[0]));
        }
    }

    #[test]
    fn codebooks_are_complete_prefix_codes() {
        kraft_and_prefix(&SF_CODES, &SF_BITS);
        let books: [(&[u16], &[u8]); 11] = [
            (&SPEC_CODES_1, &SPEC_BITS_1),
            (&SPEC_CODES_2, &SPEC_BITS_2),
            (&SPEC_CODES_3, &SPEC_BITS_3),
            (&SPEC_CODES_4, &SPEC_BITS_4),
            (&SPEC_CODES_5, &SPEC_BITS_5),
            (&SPEC_CODES_6, &SPEC_BITS_6),
            (&SPEC_CODES_7, &SPEC_BITS_7),
            (&SPEC_CODES_8, &SPEC_BITS_8),
            (&SPEC_CODES_9, &SPEC_BITS_9),
            (&SPEC_CODES_10, &SPEC_BITS_10),
            (&SPEC_CODES_11, &SPEC_BITS_11),
        ];
        for (c, b) in books {
            let c: Vec<u32> = c.iter().map(|&x| x as u32).collect();
            kraft_and_prefix(&c, b);
        }
        for t in [
            &SWB_1024_96[..],
            &SWB_1024_64,
            &SWB_1024_48,
            &SWB_1024_32,
            &SWB_1024_24,
            &SWB_1024_16,
            &SWB_1024_8,
        ] {
            assert_eq!(*t.last().unwrap(), 1024);
            assert!(t.windows(2).all(|w| w[1] > w[0] && (w[1] - w[0]) % 4 == 0));
        }
    }

    #[test]
    fn asc_parsing() {
        let c = parse_audio_specific_config(&[0x11, 0x88, 0x56, 0xe5, 0x00]).unwrap();
        assert_eq!(c.object_type, 2);
        assert_eq!(c.sample_rate, 48000);
        assert_eq!(c.channels, 1);
        assert_eq!(c.frame_length, 1024);
        assert!(!c.sbr);
        let he = parse_audio_specific_config(&[0x2B, 0x92, 0x08, 0x00]).unwrap();
        assert!(he.sbr);
        assert_eq!(he.object_type, 2);
        assert!(parse_audio_specific_config(&[0x11]).is_none());
    }

    #[test]
    fn random_bytes_are_rejected_and_never_panic() {
        let cfg = parse_audio_specific_config(&[0x11, 0x88]).unwrap();
        let mut x = 0x1234_5678u32;
        let mut accepted = 0;
        for _ in 0..2000 {
            let mut buf = vec![0u8; 400];
            for b in buf.iter_mut() {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                *b = x as u8;
            }
            buf[0] &= 0x01; // make the first element an SCE with tag 0
            if frame_length(&buf, &cfg).is_some() {
                accepted += 1;
            }
            let _ = looks_like_raw_frame(&buf, &cfg);
        }
        assert!(accepted < 40, "{accepted} random frames accepted");
    }

    #[test]
    fn end_patterns() {
        assert!(end_pattern(0xA7) && end_pattern(0x38) && end_pattern(0xE0));
        assert!(!end_pattern(0x00));
    }
}
