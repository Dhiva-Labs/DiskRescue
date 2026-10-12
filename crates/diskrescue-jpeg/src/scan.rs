//! Huffman scan analyzer: decodes the entropy-coded data of every scan
//! symbol by symbol (no IDCT, no colour conversion) and pinpoints where it
//! stops making sense.
//!
//! # Algorithm
//!
//! * **Bit reader.** Bytes are unstuffed on the fly (`FF 00` is a literal
//!   `FF`, `FF FF` is fill) so every bit can be traced back to its file
//!   offset; a small ring buffer remembers the offset of each byte in the
//!   64-bit accumulator. Any other `FF xx` stops the stream: the reader
//!   then feeds virtual zero bits, and consuming one of them means the
//!   decoder ran into the marker in the middle of an MCU.
//! * **Huffman tables** come from the DHT segments (a 9-bit lookup plus the
//!   T.81 MAXCODE slow path). When a scan references table 0/1 and no DHT
//!   defined it, the Annex K tables are used (Motion-JPEG convention) and
//!   [`ScanAnalysis::used_default_tables`] is set.
//! * **Sequential scans** decode DC categories and AC run/size symbols for
//!   every block of every MCU (interleaved per the sampling factors, or one
//!   block per MCU in non-interleaved scans), tracking per-component DC
//!   predictors that reset at restart markers.
//! * **Progressive scans** are decoded per T.81 G.1.2: DC first (category +
//!   bits), DC refinement (1 bit per block), AC first (with EOBRUN) and AC
//!   refinement, which needs to know which coefficients are already
//!   non-zero; one presence bit per coefficient (8 bytes per block) is kept
//!   instead of the coefficients themselves.
//! * **Restart intervals** must end exactly on the expected `RSTn`. A wrong
//!   or missing marker is an error; decoding then resynchronises on the
//!   next RST marker (its number tells which interval it starts), so one
//!   damaged interval does not hide the rest of the image.
//!
//! Corruption signatures: an invalid Huffman code, a run past the end of
//! the block, an undefined symbol (including a ZRL not followed by a
//! coefficient, which no encoder emits), a marker inside an MCU, a
//! misplaced RST, and a coefficient no image of this precision could
//! produce. The last one is the most sensitive: with quantisation step
//! `Q[k]`, an 8-bit image cannot have a quantised coefficient above about
//! `1024 / Q[k]`, and a desynchronised stream (DC is differential) leaves
//! that range quickly. Because a file whose DQT does not belong to its data
//! (a donor header, a damaged table) would violate it everywhere, the
//! check runs in counting mode over the first MCU row of each scan and is
//! switched off when that warm-up already shows violations.
//!
//! Limits: arithmetic-coded, lossless, hierarchical and DNL (height 0)
//! files are walked structurally only (markers, no symbols). Without
//! restart markers a damaged stream can only be located approximately: the
//! decoder may consume garbage for a while before an error is certain.

use diskrescue_core::source::Source;
use diskrescue_core::{Error, Range, RangeSet, Result};

use crate::cursor::{Cursor, SliceSource, SCAN_CHUNK};
use crate::huffman::HuffDecoder;
use crate::markers::{
    self, is_rst, parse_dht, parse_sos, FrameInfo, Headers, ScanInfo, DHT, DNL, DQT, DRI, EOI, SOS,
};
use crate::tables::standard_table;

/// Most scans analysed in one file.
pub const MAX_SCANS: usize = 128;
/// Largest number of blocks for which progressive AC refinement is
/// validated (presence bits cost 8 bytes per block: 64 MiB).
pub const MAX_PRESENCE_BLOCKS: u64 = 8 << 20;

/// What went wrong in the entropy-coded data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum ScanErrorKind {
    /// No Huffman code matched, or the symbol is not defined for this
    /// position (DC category too large, AC size too large, undefined run).
    InvalidCode,
    /// A run of zero coefficients went past the end of the block/band.
    RunOverflow,
    /// A marker other than RSTn/EOI appeared inside an MCU or between
    /// scans where it does not belong (corruption or the next file).
    MarkerInScan,
    /// An RST marker came early or with the wrong number.
    UnexpectedRst,
    /// A restart interval ended without its RST marker.
    MissingRst,
    /// The data ended (EOI, end of input, read error) before the expected
    /// number of MCUs: a truncated file.
    Eof,
    /// All MCUs were decoded but entropy-coded data continues where the
    /// end of the scan (EOI or the next marker) belongs.
    DataAfterEoi,
    /// A DC predictor or AC coefficient outside what the image's
    /// precision and quantisation tables can produce: the stream
    /// desynchronised here.
    ValueOutOfRange,
    /// The MCU uses bytes the source reported as unreadable (a bad-sector
    /// hole, typically zero-filled in a rescued image).
    Unreadable,
}

impl ScanErrorKind {
    /// True for kinds that mean "the data is damaged" rather than "the
    /// data ends early".
    pub fn is_corruption(self) -> bool {
        !matches!(self, ScanErrorKind::Eof)
    }
}

/// Where and how decoding failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ScanError {
    /// File-relative offset of the byte being decoded when the error was
    /// detected (for marker errors: the marker's `FF`).
    pub byte_offset: u64,
    /// MCU index within the scan.
    pub mcu_index: u64,
    /// What happened.
    pub kind: ScanErrorKind,
    /// Index of the scan (0 = first).
    pub scan: u32,
}

/// A position in the entropy-coded data at an MCU boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct McuPosition {
    /// File-relative offset of the byte holding the next unread bit (for a
    /// stuffed `FF 00` pair, the offset of the `FF`).
    pub byte_offset: u64,
    /// Bits of that byte already consumed (0..=7).
    pub bit: u8,
    /// Value of that byte after unstuffing.
    pub byte_value: u8,
    /// Number of MCUs fully decoded before this position.
    pub mcu_index: u64,
}

/// A clean cut point: right after a decoded MCU, with what is needed to
/// continue the bit stream from there (repair completes cut scans with
/// empty MCUs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CutPoint {
    /// Scan index (0 = first).
    pub scan: u32,
    /// Bit-exact position after the MCU.
    pub position: McuPosition,
    /// DC predictors of the scan components at that point (in units of
    /// the scan's point transform).
    pub preds: [i32; 4],
    /// The MCU starts a restart interval and its RST marker is already
    /// behind `position` (the cut is right after the marker).
    pub after_restart: bool,
}

/// Restart intervals that failed to decode and were skipped when decoding
/// resynchronised on a later RST marker (or ran to the end of the scan).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DamagedRange {
    /// Scan index (0 = first).
    pub scan: u32,
    /// First MCU of the first damaged interval (an interval boundary).
    pub first_mcu: u64,
    /// MCU where decoding resumed (the scan's MCU count when it did not).
    pub end_mcu: u64,
    /// File-relative offset of the first entropy-coded byte of the first
    /// damaged interval (right after the RST marker that precedes it).
    pub byte_start: u64,
    /// File-relative offset of the RST marker where decoding resumed, or
    /// of whatever ended the scan.
    pub byte_end: u64,
}

/// Per-scan result.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ScanReport {
    /// Scan index (0 = first).
    pub index: u32,
    /// File-relative offset of the SOS marker.
    pub offset: u64,
    /// First entropy-coded byte.
    pub data_offset: u64,
    /// Offset of the marker that ended the scan, when found.
    pub end_offset: Option<u64>,
    /// Component ids in the scan.
    pub components: Vec<u8>,
    /// Spectral selection start.
    pub ss: u8,
    /// Spectral selection end.
    pub se: u8,
    /// Successive approximation high.
    pub ah: u8,
    /// Successive approximation low.
    pub al: u8,
    /// MCUs the scan should contain.
    pub mcus_total: u64,
    /// MCUs decoded without error.
    pub mcus_decoded: u64,
    /// MCU rows in this scan's geometry.
    pub rows_total: u32,
    /// Rows decoded before the first error of this scan.
    pub rows_clean: u32,
    /// All MCUs decoded and the scan ended on a marker.
    pub complete: bool,
    /// Symbols were decoded (false: walked structurally only).
    pub validated: bool,
    /// First error in this scan.
    pub error: Option<ScanError>,
}

/// How deeply the entropy-coded data was checked.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum AnalysisDepth {
    /// Every scan was Huffman-decoded.
    Full,
    /// Decoding stopped at a budget; the rest was walked marker by marker.
    Sampled,
    /// Only markers were walked (arithmetic coding, DNL, missing tables...).
    #[default]
    Structural,
}

/// Result of [`analyze`].
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ScanAnalysis {
    /// Sequential (SOF0/SOF1) Huffman file.
    pub baseline: bool,
    /// Progressive file.
    pub progressive: bool,
    /// MCUs expected over the scans seen.
    pub mcus_total: u64,
    /// MCUs decoded without error over the scans seen.
    pub mcus_decoded: u64,
    /// MCU rows of the image (frame geometry).
    pub mcu_rows_total: u32,
    /// MCU rows decodable from the top before the first error (for
    /// progressive files: rows covered by the DC scans).
    pub mcu_rows_decoded: u32,
    /// The first error, if any.
    pub first_error: Option<ScanError>,
    /// RST markers passed correctly.
    pub restart_intervals_seen: u32,
    /// Last restart boundary reached cleanly before the first error:
    /// `(offset of the RST marker, index of the MCU it starts)`. Cutting
    /// the file at that offset keeps only verified intervals.
    pub last_good_restart: Option<(u64, u64)>,
    /// Like `last_good_restart`, but only boundaries that start an MCU row.
    pub last_good_row_restart: Option<(u64, u64)>,
    /// Position right after the last MCU decoded before the first error.
    pub last_good_mcu: Option<McuPosition>,
    /// `last_good_mcu` with its scan and DC predictors.
    pub last_good_cut: Option<CutPoint>,
    /// When the data ran out inside a scan: the position after the last
    /// MCU decoded there (also after earlier, resynchronised damage).
    pub tail_cut: Option<CutPoint>,
    /// Restart intervals skipped by resynchronisation, in file order.
    pub damaged_ranges: Vec<DamagedRange>,
    /// Offset of the marker that follows the last completely decoded scan
    /// (progressive repair cuts here).
    pub last_complete_scan_end: Option<u64>,
    /// File-relative offset of the EOI marker.
    pub eoi_offset: Option<u64>,
    /// Bytes after the EOI.
    pub trailing_bytes: u64,
    /// Where the walk of the entropy-coded data stopped: the EOI offset,
    /// the offending marker, or the end of the readable data.
    pub data_end: u64,
    /// Restart interval in MCUs (0 = none).
    pub restart_interval: u32,
    /// MCUs per row of the first scan.
    pub mcus_per_row: u32,
    /// Restart intervals damaged (skipped by resynchronisation).
    pub corrupt_intervals: u32,
    /// The Annex K tables were used because DHT was missing.
    pub used_default_tables: bool,
    /// How deeply the data was checked.
    pub depth: AnalysisDepth,
    /// Per-scan details.
    pub scans: Vec<ScanReport>,
    /// Observations.
    pub notes: Vec<String>,
}

impl ScanAnalysis {
    /// True when every scan decoded, nothing is wrong and EOI was found.
    pub fn is_clean(&self) -> bool {
        self.first_error.is_none()
            && self.eoi_offset.is_some()
            && self.scans.iter().all(|s| s.complete)
    }

    /// Fraction of MCU rows decodable (0.0..=1.0).
    pub fn row_fraction(&self) -> f32 {
        if self.mcu_rows_total == 0 {
            return 0.0;
        }
        (self.mcu_rows_decoded as f32 / self.mcu_rows_total as f32).clamp(0.0, 1.0)
    }

    /// Fraction of MCUs decodable, counting intervals recovered after a
    /// resynchronisation (0.0..=1.0).
    pub fn mcu_fraction(&self) -> f32 {
        if self.mcus_total == 0 {
            return 0.0;
        }
        (self.mcus_decoded as f32 / self.mcus_total as f32).clamp(0.0, 1.0)
    }
}

/// Knobs for [`analyze_source`].
#[derive(Debug, Clone)]
pub struct AnalyzeOptions {
    /// Stop Huffman decoding after this many bytes of entropy-coded data
    /// and walk the rest marker by marker (measure uses 32 MiB).
    pub max_decode_bytes: Option<u64>,
    /// Stop completely after this many MCUs (candidate scoring).
    pub max_mcus: Option<u64>,
    /// Use the Annex K tables when a referenced table 0/1 is undefined.
    pub default_tables: bool,
    /// Treat coefficients beyond what the quantisation tables allow as
    /// corruption (after a warm-up that detects mismatched tables).
    pub range_checks: bool,
    /// File-relative byte ranges known to be unreadable (bad sectors read
    /// as zeros). An MCU that uses them is reported as
    /// [`ScanErrorKind::Unreadable`] at the start of the hole.
    pub unreadable: Option<RangeSet>,
}

impl Default for AnalyzeOptions {
    fn default() -> Self {
        AnalyzeOptions {
            max_decode_bytes: None,
            max_mcus: None,
            default_tables: true,
            range_checks: true,
            unreadable: None,
        }
    }
}

/// Analyze a complete JPEG held in memory (`data[0]` is the SOI).
pub fn analyze(data: &[u8]) -> Result<ScanAnalysis> {
    analyze_with(data, &AnalyzeOptions::default())
}

/// [`analyze`] with options.
pub fn analyze_with(data: &[u8], opts: &AnalyzeOptions) -> Result<ScanAnalysis> {
    let src = SliceSource(data);
    analyze_source(&src, 0, data.len() as u64, opts).map(|(_, a)| a)
}

/// Walk the headers and analyze the scans of the JPEG at `start` in `src`
/// (streaming; at most `limit` bytes are read). Fails when there is no SOI
/// or the headers never reach a scan.
pub fn analyze_source(
    src: &dyn Source,
    start: u64,
    limit: u64,
    opts: &AnalyzeOptions,
) -> Result<(Headers, ScanAnalysis)> {
    let mut cur = Cursor::new(src, start, limit, SCAN_CHUNK);
    let h = markers::walk(&mut cur)?;
    if !h.reached_scan() {
        return Err(Error::corrupt(
            "JPEG header",
            format!("headers end without SOF and SOS ({:?})", h.end),
        ));
    }
    let a = analyze_after_headers(&mut cur, &h, opts);
    Ok((h, a))
}

/// Analyze the scans that follow already-walked headers.
pub(crate) fn analyze_after_headers(
    cur: &mut Cursor<'_>,
    h: &Headers,
    opts: &AnalyzeOptions,
) -> ScanAnalysis {
    let (Some(frame), Some(scan), Some(data)) = (&h.sof, h.scans.first(), h.scan_data_offset)
    else {
        let mut a = ScanAnalysis::default();
        a.notes.push("no scan to analyze".into());
        return a;
    };
    let mut an = Analyzer::new(frame.clone(), h, opts, data);
    an.run(cur, scan.clone(), data);
    an.finish()
}

// ---------------------------------------------------------------------
// Bit reader
// ---------------------------------------------------------------------

/// A marker that ended (or interrupted) the entropy-coded data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MarkerHit {
    /// First `FF` (start of any fill run).
    pub(crate) offset: u64,
    /// Offset after the marker code byte.
    pub(crate) next: u64,
    /// Marker code, `None` at the end of the readable data.
    pub(crate) code: Option<u8>,
}

struct Bits {
    pos: u64,
    acc: u64,
    nbits: u32,
    virt: u32,
    marker: Option<MarkerHit>,
    ring: [(u64, u8); 16],
    loaded: u64,
    consumed: u64,
}

impl Bits {
    fn new(pos: u64) -> Self {
        Bits {
            pos,
            acc: 0,
            nbits: 0,
            virt: 0,
            marker: None,
            ring: [(0, 0); 16],
            loaded: 0,
            consumed: 0,
        }
    }

    fn reset(&mut self, pos: u64) {
        *self = Bits::new(pos);
    }

    #[inline]
    fn push(&mut self, b: u8, off: u64) {
        self.acc |= (b as u64) << (56 - self.nbits);
        self.nbits += 8;
        self.ring[(self.loaded & 15) as usize] = (off, b);
        self.loaded += 1;
    }

    fn refill(&mut self, cur: &mut Cursor<'_>) {
        while self.nbits <= 56 {
            if self.marker.is_some() {
                self.nbits += 8;
                self.virt += 8;
                continue;
            }
            let Some(b) = cur.byte(self.pos) else {
                self.marker = Some(MarkerHit {
                    offset: self.pos,
                    next: self.pos,
                    code: None,
                });
                continue;
            };
            if b != 0xFF {
                self.push(b, self.pos);
                self.pos += 1;
                continue;
            }
            let mut p = self.pos + 1;
            let mut nb = cur.byte(p);
            while nb == Some(0xFF) {
                p += 1;
                nb = cur.byte(p);
            }
            match nb {
                Some(0x00) => {
                    self.push(0xFF, self.pos);
                    self.pos = p + 1;
                }
                Some(code) => {
                    self.marker = Some(MarkerHit {
                        offset: self.pos,
                        next: p + 1,
                        code: Some(code),
                    });
                }
                None => {
                    self.marker = Some(MarkerHit {
                        offset: self.pos,
                        next: p,
                        code: None,
                    });
                }
            }
        }
    }

    #[inline]
    fn peek16(&self) -> u32 {
        (self.acc >> 48) as u32
    }

    #[inline]
    fn consume(&mut self, n: u32) {
        self.acc = if n >= 64 { 0 } else { self.acc << n };
        self.nbits = self.nbits.saturating_sub(n);
        self.consumed += n as u64;
    }

    #[inline]
    fn overrun(&self) -> bool {
        self.nbits < self.virt
    }

    #[inline]
    fn real_bits(&self) -> u32 {
        self.nbits.saturating_sub(self.virt)
    }

    /// Offset of the byte holding the next unread bit.
    fn current_offset(&self) -> u64 {
        let idx = self.consumed / 8;
        if idx < self.loaded {
            self.ring[(idx & 15) as usize].0
        } else {
            self.marker.map(|m| m.offset).unwrap_or(self.pos)
        }
    }

    fn position(&self, mcu_index: u64) -> McuPosition {
        let idx = self.consumed / 8;
        let bit = (self.consumed % 8) as u8;
        if idx < self.loaded {
            let (off, val) = self.ring[(idx & 15) as usize];
            McuPosition {
                byte_offset: off,
                bit,
                byte_value: val,
                mcu_index,
            }
        } else {
            McuPosition {
                byte_offset: self.marker.map(|m| m.offset).unwrap_or(self.pos),
                bit: 0,
                byte_value: 0,
                mcu_index,
            }
        }
    }

    /// Skip the padding bits of the current byte and report what follows:
    /// `Ok(marker)` when the next thing is a marker, `Err(offset)` when
    /// entropy-coded data continues.
    fn align_to_marker(&mut self, cur: &mut Cursor<'_>) -> std::result::Result<MarkerHit, u64> {
        let drop = ((8 - (self.consumed % 8)) % 8) as u32;
        if drop > 0 && drop <= self.real_bits() {
            self.consume(drop);
        }
        if self.real_bits() == 0 && self.marker.is_none() {
            self.refill(cur);
        }
        if self.real_bits() > 0 {
            return Err(self.current_offset());
        }
        match self.marker {
            Some(m) => Ok(m),
            None => Err(self.current_offset()),
        }
    }
}

/// Markers that legitimately end a scan: EOI, SOI (the next file) and
/// every segment marker. Anything else inside entropy-coded data
/// (reserved codes, TEM) is damage.
fn ends_scan(code: u8) -> bool {
    code == EOI || code == markers::SOI || markers::is_header_segment(code)
}

/// The next RST marker from `from`, stepping over damaged marker codes
/// (bounded); stops at a marker that ends the scan.
fn next_rst(cur: &mut Cursor<'_>, from: u64) -> MarkerHit {
    let mut h = next_marker(cur, from, true);
    for _ in 0..256 {
        match h.code {
            Some(c) if !is_rst(c) && !ends_scan(c) => h = next_marker(cur, h.next, true),
            _ => break,
        }
    }
    h
}

/// Skip entropy-coded bytes from `from` to the next marker that is not a
/// stuffed byte or fill; RSTn are skipped too unless `stop_at_rst`.
pub(crate) fn next_marker(cur: &mut Cursor<'_>, from: u64, stop_at_rst: bool) -> MarkerHit {
    let mut p = from;
    loop {
        let Some(f) = cur.find_ff(p, cur.limit()) else {
            let end = cur.limit();
            return MarkerHit {
                offset: end,
                next: end,
                code: None,
            };
        };
        let mut q = f + 1;
        let mut c = cur.byte(q);
        while c == Some(0xFF) {
            q += 1;
            c = cur.byte(q);
        }
        match c {
            None => {
                return MarkerHit {
                    offset: f,
                    next: q,
                    code: None,
                }
            }
            Some(0x00) => p = q + 1,
            Some(m) if is_rst(m) && !stop_at_rst => p = q + 1,
            Some(m) => {
                return MarkerHit {
                    offset: f,
                    next: q + 1,
                    code: Some(m),
                }
            }
        }
    }
}

// ---------------------------------------------------------------------
// Symbol decoding
// ---------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Limits {
    max_dc_cat: u32,
    max_ac_size: u32,
    hard_dc: i32,
}

/// What to do when a coefficient exceeds what the quantisation table
/// allows (see the module docs).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CheckMode {
    /// Checks disabled (tables do not match the data).
    Off,
    /// Warm-up: report violations to the caller without failing.
    Count,
    /// A violation is corruption.
    Error,
}

/// Apply `mode` to a range violation; `Ok(true)` asks the caller to note it.
#[inline]
fn violation(mode: CheckMode) -> std::result::Result<bool, ScanErrorKind> {
    match mode {
        CheckMode::Error => Err(ScanErrorKind::ValueOutOfRange),
        CheckMode::Count => Ok(true),
        CheckMode::Off => Ok(false),
    }
}

#[inline]
fn decode_sym(
    bits: &mut Bits,
    cur: &mut Cursor<'_>,
    t: &HuffDecoder,
) -> std::result::Result<u8, ScanErrorKind> {
    if bits.nbits < 32 {
        bits.refill(cur);
    }
    match t.decode(bits.peek16()) {
        Some((len, v)) => {
            bits.consume(len);
            Ok(v)
        }
        None => Err(ScanErrorKind::InvalidCode),
    }
}

#[inline]
fn raw_bits(bits: &mut Bits, cur: &mut Cursor<'_>, n: u32) -> u32 {
    if n == 0 {
        return 0;
    }
    if bits.nbits < n {
        bits.refill(cur);
    }
    let v = (bits.acc >> (64 - n)) as u32;
    bits.consume(n);
    v
}

#[inline]
fn receive_extend(bits: &mut Bits, cur: &mut Cursor<'_>, s: u32) -> i32 {
    if s == 0 {
        return 0;
    }
    let v = raw_bits(bits, cur, s) as i32;
    if v < (1 << (s - 1)) {
        v - (1 << s) + 1
    } else {
        v
    }
}

/// Decode one sequential block (DC difference + AC run/size symbols).
/// `qb[k]` is the largest quantised magnitude coefficient `k` can reach.
#[allow(clippy::too_many_arguments)]
fn seq_block(
    bits: &mut Bits,
    cur: &mut Cursor<'_>,
    dc: &HuffDecoder,
    ac: &HuffDecoder,
    pred: &mut i32,
    q: &QuantCheck,
    mode: CheckMode,
    lim: Limits,
) -> std::result::Result<bool, ScanErrorKind> {
    let t = decode_sym(bits, cur, dc)? as u32;
    if t > lim.max_dc_cat {
        return Err(ScanErrorKind::InvalidCode);
    }
    *pred += receive_extend(bits, cur, t);
    if pred.abs() > lim.hard_dc {
        return Err(ScanErrorKind::ValueOutOfRange);
    }
    let qb = &q.bound;
    let mut noted = false;
    if pred.abs() > qb[0] {
        noted |= violation(mode)?;
    }
    let dc = (*pred as i64) * q.q[0] as i64;
    let mut energy = dc * dc;
    let mut k = 1u32;
    let mut after_zrl = false;
    while k < 64 {
        let rs = decode_sym(bits, cur, ac)?;
        let (r, s) = ((rs >> 4) as u32, (rs & 15) as u32);
        if s == 0 {
            if r == 15 {
                k += 16;
                if k > 63 {
                    return Err(ScanErrorKind::RunOverflow);
                }
                after_zrl = true;
                continue;
            }
            // Only EOB (0x00) is defined, and encoders never emit a ZRL
            // that is not followed by a coefficient.
            if r != 0 || after_zrl {
                return Err(ScanErrorKind::InvalidCode);
            }
            break;
        }
        k += r;
        if k > 63 {
            return Err(ScanErrorKind::RunOverflow);
        }
        if s > lim.max_ac_size {
            return Err(ScanErrorKind::InvalidCode);
        }
        let v = receive_extend(bits, cur, s);
        if (1i32 << (s - 1)) > qb[k as usize] {
            noted |= violation(mode)?;
        }
        let e = v as i64 * q.q[k as usize] as i64;
        energy += e * e;
        after_zrl = false;
        k += 1;
    }
    if energy > q.max_energy {
        noted |= violation(mode)?;
    }
    Ok(noted)
}

#[allow(clippy::too_many_arguments)]
fn dc_first_block(
    bits: &mut Bits,
    cur: &mut Cursor<'_>,
    dc: &HuffDecoder,
    pred: &mut i32,
    al: u32,
    q: &QuantCheck,
    mode: CheckMode,
    lim: Limits,
) -> std::result::Result<bool, ScanErrorKind> {
    let qb = &q.bound;
    let t = decode_sym(bits, cur, dc)? as u32;
    if t > lim.max_dc_cat {
        return Err(ScanErrorKind::InvalidCode);
    }
    *pred += receive_extend(bits, cur, t);
    let coef = ((*pred as i64) << al).abs();
    if coef > lim.hard_dc as i64 {
        return Err(ScanErrorKind::ValueOutOfRange);
    }
    if coef > qb[0] as i64 {
        return violation(mode);
    }
    Ok(false)
}

#[allow(clippy::too_many_arguments)]
fn ac_first_block(
    bits: &mut Bits,
    cur: &mut Cursor<'_>,
    ac: &HuffDecoder,
    st: &Setup,
    eobrun: &mut u32,
    pres: Option<&mut u64>,
    q: &QuantCheck,
    mode: CheckMode,
    lim: Limits,
) -> std::result::Result<bool, ScanErrorKind> {
    let qb = &q.bound;
    if *eobrun > 0 {
        *eobrun -= 1;
        return Ok(false);
    }
    let (ss, se, al) = (st.ss, st.se, st.al);
    let mut mask = 0u64;
    let mut noted = false;
    let mut k = ss;
    let mut after_zrl = false;
    while k <= se {
        let rs = decode_sym(bits, cur, ac)?;
        let (r, s) = ((rs >> 4) as u32, (rs & 15) as u32);
        if s == 0 {
            if r < 15 {
                if after_zrl {
                    return Err(ScanErrorKind::InvalidCode);
                }
                let run = (1u32 << r) + raw_bits(bits, cur, r);
                *eobrun = run - 1;
                break;
            }
            k += 16;
            if k > se {
                return Err(ScanErrorKind::RunOverflow);
            }
            after_zrl = true;
            continue;
        }
        k += r;
        if k > se {
            return Err(ScanErrorKind::RunOverflow);
        }
        if s > lim.max_ac_size {
            return Err(ScanErrorKind::InvalidCode);
        }
        raw_bits(bits, cur, s);
        if ((1i64 << (s - 1)) << al) > qb[k as usize] as i64 {
            noted |= violation(mode)?;
        }
        mask |= 1u64 << k;
        after_zrl = false;
        k += 1;
    }
    if let Some(p) = pres {
        *p |= mask;
    }
    Ok(noted)
}

fn ac_refine_block(
    bits: &mut Bits,
    cur: &mut Cursor<'_>,
    ac: &HuffDecoder,
    ss: u32,
    se: u32,
    eobrun: &mut u32,
    pres: &mut u64,
) -> std::result::Result<(), ScanErrorKind> {
    let mut k = ss;
    if *eobrun == 0 {
        while k <= se {
            let rs = decode_sym(bits, cur, ac)?;
            let (mut r, s) = ((rs >> 4) as u32, (rs & 15) as u32);
            let mut newcoef = false;
            if s == 0 {
                if r < 15 {
                    *eobrun = (1u32 << r) + raw_bits(bits, cur, r);
                    break;
                }
            } else {
                if s != 1 {
                    return Err(ScanErrorKind::InvalidCode);
                }
                raw_bits(bits, cur, 1);
                newcoef = true;
            }
            while k <= se {
                if *pres & (1u64 << k) != 0 {
                    raw_bits(bits, cur, 1);
                } else {
                    if r == 0 {
                        break;
                    }
                    r -= 1;
                }
                k += 1;
            }
            if newcoef {
                if k > se {
                    return Err(ScanErrorKind::RunOverflow);
                }
                *pres |= 1u64 << k;
            }
            k += 1;
        }
    }
    if *eobrun > 0 {
        while k <= se {
            if *pres & (1u64 << k) != 0 {
                raw_bits(bits, cur, 1);
            }
            k += 1;
        }
        *eobrun -= 1;
    }
    Ok(())
}

// ---------------------------------------------------------------------
// Analyzer
// ---------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Pass {
    Sequential,
    DcFirst,
    DcRefine,
    AcFirst,
    AcRefine,
}

/// Limits derived from a component's quantisation table.
struct QuantCheck {
    /// Quantisation steps in zigzag order (1 when unknown).
    q: [i32; 64],
    /// Largest quantised magnitude each coefficient can have.
    bound: [i32; 64],
    /// Largest block energy `sum((c_k * Q_k)^2)` (Parseval: the JPEG DCT is
    /// orthonormal, so this cannot exceed the energy of the level-shifted
    /// samples, plus quantisation slack).
    max_energy: i64,
}

impl QuantCheck {
    fn new(table: Option<[u16; 64]>, range: i32) -> Self {
        let mut q = [1i32; 64];
        if let Some(t) = table {
            for (d, &v) in q.iter_mut().zip(t.iter()) {
                *d = (v as i32).max(1);
            }
        }
        let mut bound = [0i32; 64];
        for k in 0..64 {
            bound[k] = (range + q[k] / 2 + 8) / q[k] + 1;
        }
        let slack: f64 = q
            .iter()
            .map(|&v| (v as f64 / 2.0).powi(2))
            .sum::<f64>()
            .sqrt();
        let r = range as f64 + slack + 16.0 * (range as f64 / 1024.0);
        QuantCheck {
            q,
            bound,
            max_energy: (r * r).ceil() as i64,
        }
    }
}

struct ScanComp {
    fi: usize,
    dc: usize,
    ac: usize,
    h: u32,
    v: u32,
    blocks_x: u32,
    pad_w: u32,
    qc: Box<QuantCheck>,
}

struct Setup {
    comps: Vec<ScanComp>,
    pass: Pass,
    interleaved: bool,
    total: u64,
    per_row: u64,
    rows: u32,
    ss: u32,
    se: u32,
    al: u32,
}

struct Analyzer<'o> {
    opts: &'o AnalyzeOptions,
    frame: FrameInfo,
    dc: [Option<HuffDecoder>; 4],
    ac: [Option<HuffDecoder>; 4],
    q_tables: [Option<[u16; 64]>; 4],
    quant_trust: bool,
    ri: u32,
    presence: Vec<Option<Vec<u64>>>,
    lim: Limits,
    dc_range: i32,
    a: ScanAnalysis,
    decode_origin: u64,
    sampling: bool,
    stop_all: bool,
    mcus_budget_used: u64,
    error_seen: bool,
    holes: Vec<Range>,
}

fn quant_values(q: &markers::QuantTable) -> Option<[u16; 64]> {
    let mut t = [0u16; 64];
    if q.values.len() < 64 {
        return None;
    }
    t.copy_from_slice(&q.values[..64]);
    Some(t)
}

impl<'o> Analyzer<'o> {
    fn new(frame: FrameInfo, h: &Headers, opts: &'o AnalyzeOptions, origin: u64) -> Self {
        let mut dc: [Option<HuffDecoder>; 4] = Default::default();
        let mut ac: [Option<HuffDecoder>; 4] = Default::default();
        for t in &h.dht {
            if let Ok(d) = HuffDecoder::new(&t.bits, &t.values) {
                let slot = if t.class == 0 { &mut dc } else { &mut ac };
                slot[(t.id & 3) as usize] = Some(d);
            }
        }
        let mut q_tables = [None; 4];
        for q in &h.dqt {
            q_tables[(q.id & 3) as usize] = quant_values(q);
        }
        let p = frame.precision.clamp(8, 12) as u32;
        let dc_range = 1024i32 << (p - 8);
        let lim = Limits {
            max_dc_cat: if p > 8 { 15 } else { 11 },
            max_ac_size: if p > 8 { 14 } else { 10 },
            hard_dc: dc_range * 2,
        };
        let mut a = ScanAnalysis {
            baseline: frame.is_huffman_dct() && !frame.progressive,
            progressive: frame.progressive,
            mcu_rows_total: frame.mcus_y(),
            mcus_per_row: frame.mcus_x(),
            restart_interval: h.dri.unwrap_or(0) as u32,
            depth: AnalysisDepth::Full,
            ..Default::default()
        };
        if !frame.is_huffman_dct() {
            a.notes.push(format!(
                "{} coding: entropy data walked structurally only",
                frame.process()
            ));
        }
        Analyzer {
            opts,
            presence: (0..frame.components.len()).map(|_| None).collect(),
            frame,
            dc,
            ac,
            q_tables,
            quant_trust: opts.range_checks,
            ri: h.dri.unwrap_or(0) as u32,
            lim,
            dc_range,
            a,
            decode_origin: origin,
            sampling: false,
            stop_all: false,
            mcus_budget_used: 0,
            error_seen: false,
            holes: opts
                .unreadable
                .as_ref()
                .map(|s| s.iter().collect())
                .unwrap_or_default(),
        }
    }

    /// First known-unreadable range that overlaps `from..to`.
    fn hole_in(&self, from: u64, to: u64) -> Option<Range> {
        if self.holes.is_empty() {
            return None;
        }
        let i = self.holes.partition_point(|r| r.end <= from);
        self.holes
            .get(i)
            .copied()
            .filter(|r| r.overlaps(Range::new(from, to)))
    }

    fn record_error(&mut self, report: &mut ScanReport, e: ScanError) {
        if report.error.is_none() {
            report.error = Some(e);
        }
        if self.a.first_error.is_none() {
            self.a.first_error = Some(e);
        }
        self.error_seen = true;
    }

    /// Walk all scans and the segments between them until EOI.
    fn run(&mut self, cur: &mut Cursor<'_>, first: ScanInfo, first_data: u64) {
        let mut scan = first;
        let mut data = first_data;
        let mut index = 0u32;
        loop {
            let hit = self.decode_scan(cur, &scan, data, index);
            if self.stop_all {
                self.a.data_end = hit.offset;
                return;
            }
            // Segments between scans, up to the next SOS or EOI.
            let mut m = hit;
            let next_scan = loop {
                match m.code {
                    None => {
                        self.end_of_data(m.offset, index);
                        return;
                    }
                    Some(EOI) => {
                        self.a.eoi_offset = Some(m.next - 2);
                        self.a.data_end = m.next - 2;
                        self.a.trailing_bytes = cur.limit().saturating_sub(m.next);
                        return;
                    }
                    Some(c) if is_rst(c) => {
                        // An RST outside any interval: note it and walk on.
                        self.a
                            .notes
                            .push(format!("stray RST marker at offset {}", m.offset));
                        m = next_marker(cur, m.next, false);
                    }
                    Some(c) if markers::is_header_segment(c) => {
                        let seg = m.next - 2;
                        let Some(len) = cur.u16be(seg + 2) else {
                            self.end_of_data(seg, index);
                            return;
                        };
                        let end = seg + 2 + len as u64;
                        if len < 2 || end > cur.limit() {
                            self.end_of_data(seg, index);
                            return;
                        }
                        let payload = cur.read_vec(seg + 4, len as usize - 2);
                        if payload.len() + 2 < len as usize {
                            self.end_of_data(seg, index);
                            return;
                        }
                        match c {
                            DHT => {
                                let (tables, err) = parse_dht(&payload, seg);
                                for t in tables {
                                    if let Ok(d) = HuffDecoder::new(&t.bits, &t.values) {
                                        let slot = if t.class == 0 {
                                            &mut self.dc
                                        } else {
                                            &mut self.ac
                                        };
                                        slot[(t.id & 3) as usize] = Some(d);
                                    }
                                }
                                if let Some(e) = err {
                                    self.a
                                        .notes
                                        .push(format!("bad DHT between scans at {seg}: {e}"));
                                }
                            }
                            DRI => {
                                if payload.len() >= 2 {
                                    self.ri = u16::from_be_bytes([payload[0], payload[1]]) as u32;
                                }
                            }
                            DQT => {
                                let (tables, _) = markers::parse_dqt(&payload, seg);
                                for q in tables {
                                    self.q_tables[(q.id & 3) as usize] = quant_values(&q);
                                }
                            }
                            DNL => self.a.notes.push(format!("DNL segment at offset {seg}")),
                            SOS => match parse_sos(&payload, seg, end) {
                                Ok(s) => break (s, end),
                                Err(e) => {
                                    let err = ScanError {
                                        byte_offset: seg,
                                        mcu_index: 0,
                                        kind: ScanErrorKind::MarkerInScan,
                                        scan: index + 1,
                                    };
                                    let mut dummy = ScanReport::default();
                                    self.record_error(&mut dummy, err);
                                    self.a.notes.push(format!("bad SOS at {seg}: {e}"));
                                    self.a.data_end = seg;
                                    return;
                                }
                            },
                            _ => {}
                        }
                        // The next thing after a segment must be a marker.
                        let mut p = end;
                        while cur.byte(p) == Some(0xFF) && cur.byte(p + 1) == Some(0xFF) {
                            p += 1;
                        }
                        match (cur.byte(p), cur.byte(p + 1)) {
                            (Some(0xFF), Some(code)) => {
                                m = MarkerHit {
                                    offset: p,
                                    next: p + 2,
                                    code: Some(code),
                                }
                            }
                            (None, _) | (Some(0xFF), None) => {
                                self.end_of_data(p, index);
                                return;
                            }
                            _ => {
                                self.foreign(p, index);
                                return;
                            }
                        }
                    }
                    Some(_) => {
                        self.foreign(m.offset, index);
                        return;
                    }
                }
            };
            index += 1;
            if index as usize >= MAX_SCANS {
                self.a.notes.push(format!(
                    "more than {MAX_SCANS} scans; rest walked structurally"
                ));
                self.sampling = true;
            }
            scan = next_scan.0;
            data = next_scan.1;
        }
    }

    /// The data ended (source end, limit, read error, truncated segment).
    fn end_of_data(&mut self, offset: u64, scan: u32) {
        self.a.data_end = offset;
        let complete = self.a.scans.last().is_some_and(|s| s.complete);
        if complete && !self.a.progressive {
            self.a.notes.push(format!(
                "EOI missing: data ends at offset {offset} after the last MCU"
            ));
            return;
        }
        if self.a.first_error.is_none() {
            self.a.first_error = Some(ScanError {
                byte_offset: offset,
                mcu_index: 0,
                kind: ScanErrorKind::Eof,
                scan,
            });
            self.a
                .notes
                .push(format!("data ends at offset {offset} without EOI"));
        }
    }

    /// Something that does not belong to a JPEG follows the scan data.
    fn foreign(&mut self, offset: u64, scan: u32) {
        self.a.data_end = offset;
        if self.a.first_error.is_none() {
            // After complete scans of a sequential file this is only a
            // missing EOI; otherwise the image is cut short.
            let complete = self.a.scans.last().is_some_and(|s| s.complete);
            if !(complete && !self.a.progressive) {
                let kind = if complete {
                    ScanErrorKind::Eof
                } else {
                    ScanErrorKind::MarkerInScan
                };
                self.a.first_error = Some(ScanError {
                    byte_offset: offset,
                    mcu_index: 0,
                    kind,
                    scan,
                });
            }
        }
        self.a.notes.push(format!(
            "unexpected data at offset {offset} where EOI belongs"
        ));
    }

    fn setup(&mut self, scan: &ScanInfo) -> std::result::Result<Setup, String> {
        let f = self.frame.clone();
        if !f.is_huffman_dct() {
            return Err(format!("{} coding", f.process()));
        }
        if f.height == 0 {
            return Err("height defined by DNL".into());
        }
        if f.check().is_err() {
            return Err("invalid frame header".into());
        }
        let ns = scan.components.len();
        if ns == 0 || ns > 4 {
            return Err(format!("{ns} components in scan"));
        }
        let (ss, se, ah, al) = (
            scan.ss as u32,
            scan.se as u32,
            scan.ah as u32,
            scan.al as u32,
        );
        let pass = if !f.progressive {
            if ss != 0 || se != 63 || ah != 0 || al != 0 {
                return Err(format!(
                    "sequential scan with Ss={ss} Se={se} Ah={ah} Al={al}"
                ));
            }
            Pass::Sequential
        } else if ss == 0 {
            if se != 0 {
                return Err("progressive DC scan with Se != 0".into());
            }
            if ah == 0 {
                Pass::DcFirst
            } else {
                Pass::DcRefine
            }
        } else {
            if se > 63 || ss > se || ns != 1 {
                return Err(format!("progressive AC scan Ss={ss} Se={se} Ns={ns}"));
            }
            if ah == 0 {
                Pass::AcFirst
            } else {
                Pass::AcRefine
            }
        };
        if al > 13 {
            return Err(format!("Al = {al}"));
        }
        let (hmax, vmax) = (f.hmax() as u32, f.vmax() as u32);
        let (mx, my) = (f.mcus_x(), f.mcus_y());
        let mut comps: Vec<ScanComp> = Vec::with_capacity(ns);
        let mut blocks_per_mcu = 0u32;
        for sc in &scan.components {
            let fi = f
                .component_index(sc.selector)
                .ok_or_else(|| format!("scan component {} not in frame", sc.selector))?;
            if comps.iter().any(|c| c.fi == fi) {
                return Err(format!("component {} twice in scan", sc.selector));
            }
            if sc.dc_table > 3 || sc.ac_table > 3 {
                return Err("table selector above 3".into());
            }
            let c = f.components[fi];
            let (h, v) = (c.h as u32, c.v as u32);
            blocks_per_mcu += h * v;
            let need_dc = matches!(pass, Pass::Sequential | Pass::DcFirst);
            let need_ac = matches!(pass, Pass::Sequential | Pass::AcFirst | Pass::AcRefine);
            let (td, ta) = (sc.dc_table as usize, sc.ac_table as usize);
            if need_dc && self.dc[td].is_none() {
                self.default_table(0, td)?;
            }
            if need_ac && self.ac[ta].is_none() {
                self.default_table(1, ta)?;
            }
            let qc = Box::new(QuantCheck::new(
                self.q_tables[(c.tq & 3) as usize],
                self.dc_range,
            ));
            let width_c = ((f.width as u32) * h).div_ceil(hmax);
            comps.push(ScanComp {
                fi,
                dc: td,
                ac: ta,
                h,
                v,
                blocks_x: width_c.div_ceil(8),
                pad_w: mx * h,
                qc,
            });
        }
        let interleaved = ns > 1;
        if interleaved && blocks_per_mcu > 10 {
            return Err(format!("{blocks_per_mcu} blocks per MCU"));
        }
        let (total, per_row, rows) = if interleaved {
            (mx as u64 * my as u64, mx as u64, my)
        } else {
            let fc = f.components[comps[0].fi];
            let bx = ((f.width as u32) * fc.h as u32).div_ceil(hmax).div_ceil(8);
            let by = ((f.height as u32) * fc.v as u32).div_ceil(vmax).div_ceil(8);
            (bx as u64 * by as u64, bx as u64, by)
        };
        if matches!(pass, Pass::AcFirst | Pass::AcRefine) {
            let c = &comps[0];
            if self.presence[c.fi].is_none() {
                let blocks = c.pad_w as u64 * (my as u64 * c.v as u64);
                if blocks > MAX_PRESENCE_BLOCKS {
                    return Err(format!("{blocks} blocks: too large for AC validation"));
                }
                self.presence[c.fi] = Some(vec![0u64; blocks as usize]);
            }
        }
        Ok(Setup {
            comps,
            pass,
            interleaved,
            total,
            per_row: per_row.max(1),
            rows,
            ss,
            se,
            al,
        })
    }

    fn default_table(&mut self, class: u8, id: usize) -> std::result::Result<(), String> {
        if !self.opts.default_tables {
            return Err(format!("Huffman table {class}/{id} not defined"));
        }
        let t = standard_table(class, id as u8)
            .ok_or_else(|| format!("Huffman table {class}/{id} not defined"))?;
        let d = HuffDecoder::new(&t.bits, &t.values).map_err(|e| e.to_string())?;
        if class == 0 {
            self.dc[id] = Some(d);
        } else {
            self.ac[id] = Some(d);
        }
        if !self.a.used_default_tables {
            self.a.used_default_tables = true;
            self.a
                .notes
                .push("DHT missing: using the standard (Annex K) Huffman tables".into());
        }
        Ok(())
    }

    /// Decode one scan; returns the marker that ends it.
    fn decode_scan(
        &mut self,
        cur: &mut Cursor<'_>,
        scan: &ScanInfo,
        data: u64,
        index: u32,
    ) -> MarkerHit {
        let mut report = ScanReport {
            index,
            offset: scan.offset,
            data_offset: data,
            components: scan.components.iter().map(|c| c.selector).collect(),
            ss: scan.ss,
            se: scan.se,
            ah: scan.ah,
            al: scan.al,
            ..Default::default()
        };
        let setup = match self.setup(scan) {
            Ok(s) => s,
            Err(why) => {
                if self.a.depth == AnalysisDepth::Full {
                    self.a.depth = AnalysisDepth::Structural;
                }
                self.a
                    .notes
                    .push(format!("scan {index} not decoded: {why}"));
                let hit = next_marker(cur, data, false);
                report.end_offset = Some(hit.offset);
                self.a.scans.push(report);
                return hit;
            }
        };
        report.mcus_total = setup.total;
        report.rows_total = setup.rows;
        if index == 0 {
            self.a.mcus_per_row = setup.per_row as u32;
        }
        if self.sampling {
            let hit = next_marker(cur, data, false);
            report.end_offset = Some(hit.offset);
            self.a.scans.push(report);
            return hit;
        }
        report.validated = true;
        let hit = self.decode_mcus(cur, &setup, data, index, &mut report);
        report.end_offset = Some(hit.offset);
        if report.complete && !self.error_seen {
            self.a.last_complete_scan_end = Some(hit.offset);
        }
        self.a.scans.push(report);
        hit
    }

    fn error_at(&self, kind: ScanErrorKind, offset: u64, mcu: u64, scan: u32) -> ScanError {
        ScanError {
            byte_offset: offset,
            mcu_index: mcu,
            kind,
            scan,
        }
    }

    #[allow(clippy::too_many_lines)]
    fn decode_mcus(
        &mut self,
        cur: &mut Cursor<'_>,
        st: &Setup,
        data: u64,
        index: u32,
        report: &mut ScanReport,
    ) -> MarkerHit {
        let ri = self.ri as u64;
        let mut bits = Bits::new(data);
        let mut preds = [0i32; 4];
        let mut eobrun = 0u32;
        let mut expected_rst = 0u8;
        let mut m = 0u64;
        let mut clean = !self.error_seen;
        let budget_end = self
            .opts
            .max_decode_bytes
            .map(|b| self.decode_origin.saturating_add(b));
        // Range checks start after a warm-up: violations right at the start
        // mean the quantisation tables do not belong to this data.
        let warm = st.per_row.max(16).min(st.total);
        let mut resynced_at: Option<u64> = None;
        // First entropy-coded byte of the current restart interval.
        let mut interval_start = data;
        while m < st.total {
            if ri > 0 && m > 0 && m % ri == 0 && resynced_at != Some(m) {
                let interval = m / ri;
                let at_boundary = CutPoint {
                    scan: index,
                    position: bits.position(m),
                    preds,
                    after_restart: false,
                };
                let err = match bits.align_to_marker(cur) {
                    Ok(hit) => match hit.code {
                        Some(c) if is_rst(c) && (c - 0xD0) == expected_rst => {
                            if clean {
                                self.a.last_good_restart = Some((hit.offset, m));
                                if m % st.per_row == 0 {
                                    self.a.last_good_row_restart = Some((hit.offset, m));
                                }
                            }
                            self.a.restart_intervals_seen += 1;
                            bits.reset(hit.next);
                            interval_start = hit.next;
                            preds = [0; 4];
                            eobrun = 0;
                            expected_rst = (expected_rst + 1) & 7;
                            None
                        }
                        Some(c) if is_rst(c) => {
                            Some((ScanErrorKind::UnexpectedRst, hit.offset, Some(hit)))
                        }
                        Some(EOI) | None => Some((ScanErrorKind::Eof, hit.offset, Some(hit))),
                        Some(_) => Some((ScanErrorKind::MarkerInScan, hit.offset, Some(hit))),
                    },
                    Err(off) => Some((ScanErrorKind::MissingRst, off, None)),
                };
                if let Some((kind, off, hit)) = err {
                    let e = self.error_at(kind, off, m, index);
                    self.record_error(report, e);
                    clean = false;
                    if kind == ScanErrorKind::Eof {
                        // The data ends exactly on an interval boundary.
                        self.a.tail_cut = Some(at_boundary);
                        return hit.unwrap_or_else(|| next_marker(cur, off, false));
                    }
                    let damaged = interval.saturating_sub(1);
                    let span = (index, damaged * ri, interval_start);
                    match self.resync(cur, &mut bits, hit, off, damaged, ri, st.total, span) {
                        Some((new_m, exp)) => {
                            m = new_m;
                            interval_start = bits.pos;
                            preds = [0; 4];
                            eobrun = 0;
                            expected_rst = exp;
                            resynced_at = Some(new_m);
                            continue;
                        }
                        None => {
                            let end = next_marker(cur, off, false);
                            self.damage_to_end(span, st.total, end.offset);
                            return end;
                        }
                    }
                }
            }
            if let Some(end) = budget_end {
                if bits.pos > end {
                    self.sampling = true;
                    self.a.depth = AnalysisDepth::Sampled;
                    self.a.notes.push(format!(
                        "Huffman-validated the first {} bytes of scan data",
                        end - self.decode_origin
                    ));
                    return next_marker(cur, bits.current_offset(), false);
                }
            }
            // Fast path: a run of empty bands in an AC first scan.
            if st.pass == Pass::AcFirst && eobrun > 0 {
                let next_boundary = m
                    .checked_div(ri)
                    .map_or(st.total, |interval| (interval + 1) * ri);
                let n = (eobrun as u64).min(next_boundary.min(st.total) - m);
                eobrun -= n as u32;
                m += n;
                report.mcus_decoded += n;
                if clean {
                    report.rows_clean = (m / st.per_row) as u32;
                }
                continue;
            }
            let mode = if !self.quant_trust {
                CheckMode::Off
            } else if m < warm {
                CheckMode::Count
            } else {
                CheckMode::Error
            };
            let before = bits.position(m);
            let before_cut = CutPoint {
                scan: index,
                position: before,
                preds,
                after_restart: ri > 0 && m > 0 && m % ri == 0,
            };
            let r = self.decode_one(cur, &mut bits, st, m, &mut preds, &mut eobrun, mode);
            let r = if bits.overrun() {
                Err(ScanErrorKind::Eof)
            } else {
                r
            };
            let r = match (
                r,
                self.hole_in(before.byte_offset, bits.current_offset() + 1),
            ) {
                (_, Some(h)) if !self.holes.is_empty() => Err((ScanErrorKind::Unreadable, h)),
                (Ok(v), _) => Ok(v),
                (Err(k), _) => Err((k, Range::new(0, 0))),
            };
            match r {
                Ok(noted) => {
                    if noted {
                        self.quant_trust = false;
                        self.a.notes.push(format!(
                            "coefficients exceed the quantisation tables' range near the start \
                             of scan {index}; range checks disabled (tables may not match the data)"
                        ));
                    }
                }
                Err((ScanErrorKind::Unreadable, hole)) => {
                    let e = self.error_at(ScanErrorKind::Unreadable, hole.start, m, index);
                    if clean {
                        self.a.last_good_mcu = Some(before);
                        self.a.last_good_cut = Some(before_cut);
                    }
                    self.record_error(report, e);
                    clean = false;
                    if let Some(interval) = m.checked_div(ri) {
                        let span = (index, interval * ri, interval_start);
                        match self
                            .resync(cur, &mut bits, None, hole.end, interval, ri, st.total, span)
                        {
                            Some((new_m, exp)) => {
                                m = new_m;
                                interval_start = bits.pos;
                                preds = [0; 4];
                                eobrun = 0;
                                expected_rst = exp;
                                resynced_at = Some(new_m);
                                continue;
                            }
                            None => {
                                let end = next_marker(cur, hole.end, false);
                                self.damage_to_end(span, st.total, end.offset);
                                return end;
                            }
                        }
                    }
                    return next_marker(cur, hole.end, false);
                }
                Err((kind, _)) => {
                    let (kind, off, hit) = if bits.overrun() {
                        let h = bits.marker.unwrap_or(MarkerHit {
                            offset: bits.pos,
                            next: bits.pos,
                            code: None,
                        });
                        let k = match h.code {
                            None | Some(EOI) => ScanErrorKind::Eof,
                            Some(c) if is_rst(c) => ScanErrorKind::UnexpectedRst,
                            Some(_) => ScanErrorKind::MarkerInScan,
                        };
                        (k, h.offset, Some(h))
                    } else {
                        (kind, bits.current_offset(), None)
                    };
                    let e = self.error_at(kind, off, m, index);
                    if clean {
                        self.a.last_good_mcu = Some(before);
                        self.a.last_good_cut = Some(before_cut);
                    }
                    self.record_error(report, e);
                    clean = false;
                    if kind == ScanErrorKind::Eof {
                        self.a.tail_cut = Some(before_cut);
                        return hit.unwrap_or_else(|| next_marker(cur, off, false));
                    }
                    if let Some(interval) = m.checked_div(ri) {
                        let from = hit.map(|h| h.offset).unwrap_or(off);
                        let span = (index, interval * ri, interval_start);
                        match self.resync(cur, &mut bits, hit, from, interval, ri, st.total, span) {
                            Some((new_m, exp)) => {
                                m = new_m;
                                interval_start = bits.pos;
                                preds = [0; 4];
                                eobrun = 0;
                                expected_rst = exp;
                                resynced_at = Some(new_m);
                                continue;
                            }
                            None => {
                                let end = next_marker(cur, from, false);
                                self.damage_to_end(span, st.total, end.offset);
                                return end;
                            }
                        }
                    }
                    // No restart markers: the rest of this scan cannot be
                    // located; walk to the next marker that is not RST.
                    return match hit {
                        Some(h) if h.code.is_some_and(is_rst) => next_marker(cur, h.next, false),
                        Some(h) => h,
                        None => next_marker(cur, off, false),
                    };
                }
            }
            m += 1;
            report.mcus_decoded += 1;
            self.mcus_budget_used += 1;
            if clean {
                if m % st.per_row == 0 {
                    report.rows_clean = (m / st.per_row) as u32;
                }
                let p = bits.position(m);
                self.a.last_good_mcu = Some(p);
                self.a.last_good_cut = Some(CutPoint {
                    scan: index,
                    position: p,
                    preds,
                    after_restart: false,
                });
            }
            if let Some(max) = self.opts.max_mcus {
                if self.mcus_budget_used >= max {
                    self.stop_all = true;
                    self.a.depth = AnalysisDepth::Sampled;
                    let at = bits.current_offset();
                    return MarkerHit {
                        offset: at,
                        next: at,
                        code: None,
                    };
                }
            }
        }
        // All MCUs of the scan decoded: the scan must end on a marker.
        match bits.align_to_marker(cur) {
            Ok(hit) => {
                if clean {
                    report.rows_clean = st.rows;
                }
                match hit.code {
                    Some(c) if is_rst(c) => {
                        // Harmless right before the next marker; with
                        // entropy-coded data after it, the scan holds more
                        // MCUs than the frame (a wrong header, a donor of
                        // the wrong size).
                        let marker_follows = cur.byte(hit.next) == Some(0xFF)
                            && cur.byte(hit.next + 1).is_some_and(|b| b != 0x00);
                        if marker_follows {
                            self.a.notes.push(format!(
                                "restart marker after the last MCU at offset {}",
                                hit.offset
                            ));
                        } else {
                            let e = self.error_at(
                                ScanErrorKind::DataAfterEoi,
                                hit.next,
                                st.total,
                                index,
                            );
                            self.record_error(report, e);
                        }
                        report.complete = report.error.is_none();
                        next_marker(cur, hit.next, false)
                    }
                    _ => {
                        report.complete = report.error.is_none();
                        hit
                    }
                }
            }
            Err(off) => {
                let e = self.error_at(ScanErrorKind::DataAfterEoi, off, st.total, index);
                self.record_error(report, e);
                if clean {
                    report.rows_clean = st.rows;
                }
                next_marker(cur, off, false)
            }
        }
    }

    /// Record that the intervals from `span` (scan, first MCU, first byte)
    /// up to the end of the scan could not be decoded.
    fn damage_to_end(&mut self, span: (u32, u64, u64), total: u64, end: u64) {
        let (scan, first_mcu, byte_start) = span;
        if first_mcu < total && self.a.damaged_ranges.len() < 1 << 16 {
            self.a.damaged_ranges.push(DamagedRange {
                scan,
                first_mcu,
                end_mcu: total,
                byte_start,
                byte_end: end.max(byte_start),
            });
        }
    }

    /// Resynchronise on the next RST marker after an error in interval
    /// `interval`. Returns the MCU index to continue from and the RST
    /// number expected next. `span` is `(scan, first MCU, first byte)` of
    /// the damaged interval, recorded in [`ScanAnalysis::damaged_ranges`]
    /// when decoding resumes.
    #[allow(clippy::too_many_arguments)]
    fn resync(
        &mut self,
        cur: &mut Cursor<'_>,
        bits: &mut Bits,
        hit: Option<MarkerHit>,
        from: u64,
        interval: u64,
        ri: u64,
        total: u64,
        span: (u32, u64, u64),
    ) -> Option<(u64, u8)> {
        let h = match hit {
            Some(h) if h.code.is_some_and(is_rst) => h,
            // A code no encoder writes (a damaged RST marker, say): keep
            // looking for the next restart marker after it.
            Some(h) if h.code.is_some_and(|c| !ends_scan(c)) => next_rst(cur, h.next),
            // A real marker (EOI, another segment) or the end of the data:
            // nothing to resynchronise on.
            Some(_) => return None,
            None => next_rst(cur, from),
        };
        let c = h.code.filter(|&c| is_rst(c))?;
        let n = (c - 0xD0) as i64;
        // RST n starts interval j with (j - 1) % 8 == n.
        let j = interval as i64 + 1 + (n - interval as i64).rem_euclid(8);
        let new_m = j as u64 * ri;
        if new_m >= total {
            return None;
        }
        let lost = (j as u64).saturating_sub(interval) as u32;
        self.a.corrupt_intervals += lost.max(1);
        if self.a.damaged_ranges.len() < 1 << 16 {
            let (scan, first_mcu, byte_start) = span;
            self.a.damaged_ranges.push(DamagedRange {
                scan,
                first_mcu,
                end_mcu: new_m,
                byte_start,
                byte_end: h.offset.max(byte_start),
            });
        }
        bits.reset(h.next);
        Some((new_m, ((n + 1) & 7) as u8))
    }

    #[allow(clippy::too_many_arguments)]
    fn decode_one(
        &mut self,
        cur: &mut Cursor<'_>,
        bits: &mut Bits,
        st: &Setup,
        m: u64,
        preds: &mut [i32; 4],
        eobrun: &mut u32,
        mode: CheckMode,
    ) -> std::result::Result<bool, ScanErrorKind> {
        let lim = self.lim;
        let mut noted = false;
        match st.pass {
            Pass::Sequential => {
                for (ci, c) in st.comps.iter().enumerate() {
                    let n = if st.interleaved { c.h * c.v } else { 1 };
                    let (Some(dc), Some(ac)) = (self.dc[c.dc].as_ref(), self.ac[c.ac].as_ref())
                    else {
                        return Err(ScanErrorKind::InvalidCode);
                    };
                    for _ in 0..n {
                        noted |= seq_block(bits, cur, dc, ac, &mut preds[ci], &c.qc, mode, lim)?;
                        if bits.overrun() {
                            return Err(ScanErrorKind::Eof);
                        }
                    }
                }
            }
            Pass::DcFirst => {
                for (ci, c) in st.comps.iter().enumerate() {
                    let n = if st.interleaved { c.h * c.v } else { 1 };
                    let Some(dc) = self.dc[c.dc].as_ref() else {
                        return Err(ScanErrorKind::InvalidCode);
                    };
                    for _ in 0..n {
                        noted |=
                            dc_first_block(bits, cur, dc, &mut preds[ci], st.al, &c.qc, mode, lim)?;
                        if bits.overrun() {
                            return Err(ScanErrorKind::Eof);
                        }
                    }
                }
            }
            Pass::DcRefine => {
                let n: u32 = if st.interleaved {
                    st.comps.iter().map(|c| c.h * c.v).sum()
                } else {
                    1
                };
                for _ in 0..n {
                    raw_bits(bits, cur, 1);
                }
            }
            Pass::AcFirst | Pass::AcRefine => {
                let c = &st.comps[0];
                let Some(ac) = self.ac[c.ac].as_ref() else {
                    return Err(ScanErrorKind::InvalidCode);
                };
                let bx = c.blocks_x.max(1) as u64;
                let idx = (m / bx) * c.pad_w as u64 + (m % bx);
                let pres = self.presence[c.fi]
                    .as_mut()
                    .and_then(|v| v.get_mut(idx as usize));
                if st.pass == Pass::AcFirst {
                    noted |= ac_first_block(bits, cur, ac, st, eobrun, pres, &c.qc, mode, lim)?;
                } else {
                    let mut scratch = 0u64;
                    let p = match pres {
                        Some(p) => p,
                        None => &mut scratch,
                    };
                    ac_refine_block(bits, cur, ac, st.ss, st.se, eobrun, p)?;
                }
            }
        }
        if bits.overrun() {
            return Err(ScanErrorKind::Eof);
        }
        Ok(noted)
    }

    fn finish(mut self) -> ScanAnalysis {
        let a = &mut self.a;
        a.mcus_total = a.scans.iter().map(|s| s.mcus_total).sum();
        a.mcus_decoded = a.scans.iter().map(|s| s.mcus_decoded).sum();
        if a.scans.iter().all(|s| !s.validated) {
            a.depth = AnalysisDepth::Structural;
        }
        let total_rows = a.mcu_rows_total;
        let scale = |clean: u32, rows: u32| -> u32 {
            if rows == 0 {
                0
            } else {
                ((clean as u64 * total_rows as u64) / rows as u64).min(total_rows as u64) as u32
            }
        };
        if a.progressive {
            // Rows covered by DC first scans, per frame component.
            let mut rows = total_rows;
            for c in &self.frame.components {
                let best = a
                    .scans
                    .iter()
                    .filter(|s| s.ss == 0 && s.ah == 0 && s.components.contains(&c.id))
                    .map(|s| {
                        if s.complete && s.error.is_none() {
                            total_rows
                        } else {
                            scale(s.rows_clean, s.rows_total)
                        }
                    })
                    .max()
                    .unwrap_or(0);
                rows = rows.min(best);
            }
            a.mcu_rows_decoded = rows;
        } else if let Some(s) = a.scans.first() {
            a.mcu_rows_decoded = if s.complete && s.error.is_none() {
                total_rows
            } else {
                scale(s.rows_clean, s.rows_total)
            };
        }
        self.a
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markers::tests::tiny_jpeg;

    #[test]
    fn tiny_file_decodes_cleanly() {
        let d = tiny_jpeg();
        let a = analyze(&d).unwrap();
        assert!(a.is_clean(), "{a:?}");
        assert_eq!(a.mcus_total, 1);
        assert_eq!(a.mcus_decoded, 1);
        assert_eq!(a.mcu_rows_total, 1);
        assert_eq!(a.mcu_rows_decoded, 1);
        assert_eq!(a.eoi_offset, Some(d.len() as u64 - 2));
        assert_eq!(a.trailing_bytes, 0);
        assert_eq!(a.depth, AnalysisDepth::Full);
    }

    #[test]
    fn truncated_tiny_file_is_eof() {
        let d = tiny_jpeg();
        let cut = &d[..d.len() - 3];
        let a = analyze(cut).unwrap();
        assert_eq!(a.first_error.map(|e| e.kind), Some(ScanErrorKind::Eof));
        assert!(a.eoi_offset.is_none());
    }

    #[test]
    fn trailing_bytes_are_counted() {
        let mut d = tiny_jpeg();
        d.extend_from_slice(&[1, 2, 3, 4, 5]);
        let a = analyze(&d).unwrap();
        assert!(a.is_clean());
        assert_eq!(a.trailing_bytes, 5);
    }

    #[test]
    fn invalid_symbol_is_reported() {
        let mut d = tiny_jpeg();
        let n = d.len();
        // DC "00" then sixteen 1-bits: no AC code is all ones.
        d.splice(n - 3..n - 2, [0x3F, 0xFF, 0x00, 0xFF, 0x00]);
        let a = analyze(&d).unwrap();
        let e = a.first_error.unwrap();
        assert_eq!(e.kind, ScanErrorKind::InvalidCode, "{a:?}");
        assert_eq!(e.mcu_index, 0);
    }

    #[test]
    fn missing_dht_uses_default_tables() {
        let d = tiny_jpeg();
        let i = d.windows(2).position(|w| w == [0xFF, 0xC4]).unwrap();
        let mut x = d[..i].to_vec();
        x.extend_from_slice(&d[i + crate::tables::STANDARD_DHT_LEN..]);
        let a = analyze(&x).unwrap();
        assert!(a.used_default_tables);
        assert!(a.is_clean());
        let strict = AnalyzeOptions {
            default_tables: false,
            ..Default::default()
        };
        let b = analyze_with(&x, &strict).unwrap();
        assert_eq!(b.depth, AnalysisDepth::Structural);
    }

    #[test]
    fn hostile_inputs_do_not_panic() {
        let base = tiny_jpeg();
        let mut x = 0xDEAD_BEEFu32;
        for round in 0..5000 {
            let mut d = base.clone();
            for _ in 0..(1 + round % 6) {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                let p = 2 + (x as usize) % (d.len() - 2);
                d[p] = (x >> 9) as u8;
            }
            let _ = analyze(&d);
            let cut = (x as usize >> 4) % d.len();
            let _ = analyze(&d[..cut]);
        }
        // Absurd frame sizes with almost no data.
        let mut d = base.clone();
        let sof = d.windows(2).position(|w| w == [0xFF, 0xC0]).unwrap();
        d[sof + 5] = 0x40;
        d[sof + 7] = 0x40;
        let a = analyze(&d).unwrap();
        assert_eq!(a.first_error.map(|e| e.kind), Some(ScanErrorKind::Eof));
        assert_eq!(a.mcus_total, 2049 * 2049);
        // Impossible frames are only walked structurally.
        d[sof + 5] = 0xFF;
        d[sof + 7] = 0xFF;
        let a = analyze(&d).unwrap();
        assert_eq!(a.depth, AnalysisDepth::Structural);
        assert!(analyze(b"").is_err());
        assert!(analyze(&[0xFF, 0xD8, 0xFF]).is_err());
    }
}
