//! Lenient PNG decoding: salvage every scanline that can be trusted.
//!
//! A strict decoder gives up at the first error and returns nothing. This
//! decoder concatenates the payload of every IDAT chunk (including chunks
//! whose CRC does not match), inflates it through the stream-oriented
//! [`flate2::Decompress`] API so that output produced before an error is kept,
//! unfilters scanline after scanline and stops at the first row it cannot
//! trust.
//!
//! # What "trust" means
//!
//! PNG has no per-row checksum. After a flipped bit the deflate stream usually
//! keeps producing plausible-looking garbage for a while before it hits an
//! invalid code, so a decoder that simply keeps every row inflated before the
//! error would hand out damaged rows. This decoder therefore
//!
//! * stops at the first row whose filter-type byte is not in `0..=4` (random
//!   garbage fails that test 98 % of the time per row), and
//! * when it stops because of corruption (a bad filter byte or an inflate
//!   error) it also discards the last [`SUSPECT_ROWS`] complete rows, because
//!   the row in which the garbage started is complete but partly wrong.
//!
//! When the data merely *ends* (truncation, an unreadable sector, the end of
//! the file) nothing is discarded: every complete row before the end is real.
//!
//! # Chunk CRCs as evidence
//!
//! A flipped bit often leaves the deflate stream decodable (Huffman codes
//! resynchronise within a few symbols), so the damage may show up hundreds of
//! rows later or only in the final Adler-32. The IDAT chunk CRCs localise it:
//! when the image data spans several IDAT chunks and one fails its CRC, only
//! the rows completed from the chunks *before* it are reported
//! ([`LenientImage::rows_recovered`]); the rows that still decode afterwards
//! are counted in [`LenientImage::rows_unverified`] but neither returned nor
//! trusted. A stream that decodes completely with a valid Adler-32 overrides
//! this (then only the stored CRC was damaged). With a single IDAT chunk the
//! CRC cannot localise anything and the inflater's own error position is the
//! only evidence, so a silent glitch before it can slip through.
//!
//! # Limits
//!
//! Damage that leaves the deflate stream decodable (for example a flipped bit
//! inside a stored block, or zero-filled sectors that decode as valid symbols)
//! cannot be detected row by row; only the final Adler-32 check notices it,
//! and it does not say where. Sources that know their unreadable ranges
//! ([`diskrescue_core::Source::unreadable`]) are honoured: the image data is
//! treated as ending at the first hole.
//!
//! # Interlaced images
//!
//! Adam7 passes are decoded in order. [`LenientImage::pixels`] then holds the
//! full image buffer with recovered pixels scattered into place;
//! [`LenientImage::pass_rows_recovered`] says how far each pass got and
//! [`LenientImage::pixel_known`] tells which pixels are real.

use std::collections::VecDeque;

use diskrescue_core::ranges::{Range, RangeSet};
use diskrescue_core::source::Source;
use diskrescue_core::{Error, Result};
use flate2::{Decompress, FlushDecompress, Status};

use crate::chunks::{self, ChunkWalk};
use crate::filter::unfilter;
use crate::ihdr::{Ihdr, Pass, MAX_ROW_BYTES};
use crate::io::{SliceSource, BLOCK};

/// Number of most recent complete rows that are discarded when decoding stops
/// because of corruption (see the module documentation).
pub const SUSPECT_ROWS: usize = 2;

/// Inflated bytes beyond the last row that are tolerated before giving up.
const MAX_EXCESS: u64 = 16 << 20;

/// Largest image (inflated size) the decoder will work through. Real photos
/// stay far below; a damaged header claiming petabytes must not keep the
/// inflater busy for hours.
pub const MAX_IMAGE_BYTES: u64 = 8 << 30;

/// Most scanlines the decoder will process per image.
pub const MAX_ROWS: u64 = 1 << 24;

/// Options for [`decode_with`] and [`decode_source`].
#[derive(Debug, Clone)]
pub struct LenientOptions {
    /// Keep the decoded pixels. When false only the statistics are
    /// computed and `pixels` stays empty, so arbitrarily large images can be
    /// analysed in constant memory.
    pub keep_pixels: bool,
    /// Refuse to allocate more than this many bytes for pixels.
    pub max_pixel_bytes: u64,
}

impl Default for LenientOptions {
    fn default() -> Self {
        LenientOptions {
            keep_pixels: true,
            max_pixel_bytes: 1 << 30,
        }
    }
}

/// The salvaged part of a PNG image.
#[derive(Debug, Clone, PartialEq)]
pub struct LenientImage {
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// IHDR colour type.
    pub color_type: u8,
    /// IHDR bit depth.
    pub bit_depth: u8,
    /// True for Adam7 interlaced images.
    pub interlaced: bool,
    /// Scanlines recovered (summed over all passes for interlaced images).
    pub rows_recovered: u32,
    /// Further rows that decoded structurally but follow an IDAT chunk whose
    /// CRC failed without the inflater noticing; they are not trusted and not
    /// part of `pixels`.
    pub rows_unverified: u32,
    /// Scanlines in the whole image (summed over all passes for interlaced
    /// images).
    pub rows_total: u32,
    /// Bytes the inflater produced.
    pub bytes_inflated: u64,
    /// Why decoding stopped early or what is wrong with the stream; `None`
    /// when the zlib stream was complete, its checksum matched and it held
    /// exactly the expected number of bytes.
    pub inflate_error: Option<String>,
    /// Unfiltered scanlines without the filter byte, `row_bytes()` each.
    /// Non-interlaced images hold exactly the recovered rows
    /// (`rows_recovered * row_bytes()` bytes). Interlaced images hold the
    /// whole `height * row_bytes()` buffer with unrecovered pixels zeroed.
    /// Empty when [`LenientOptions::keep_pixels`] is false.
    pub pixels: Vec<u8>,
    /// One entry per image row: true when every pixel of the row was
    /// recovered. Empty when pixels were not kept.
    pub recovered_mask_rows: Vec<bool>,
    /// The header this image was decoded with.
    pub ihdr: Ihdr,
    /// Rows recovered per Adam7 pass (index 0 only for non-interlaced
    /// images).
    pub pass_rows_recovered: [u32; 7],
    /// Size a complete image inflates to.
    pub expected_bytes: u64,
    /// True when the zlib stream reached its end marker with a valid
    /// checksum.
    pub stream_complete: bool,
    /// Inflated bytes beyond the last row.
    pub excess_bytes: u64,
    /// Complete rows discarded because they preceded detected corruption.
    pub dropped_rows: u32,
    /// False when the zlib header (CMF/FLG) was invalid and the stream was
    /// inflated as raw deflate from the third byte.
    pub zlib_header_ok: bool,
    /// True when every byte of image data inflated but the zlib Adler-32
    /// checksum did not match: some pixels are wrong, but there is no way to
    /// tell which (flipped bits often leave the deflate stream decodable).
    pub checksum_failed: bool,
    /// True when decoding was cut off at an IDAT chunk whose CRC failed
    /// (see [`LenientImage::rows_unverified`]).
    pub crc_capped: bool,
}

impl LenientImage {
    /// Bytes in one scanline (without the filter byte).
    pub fn row_bytes(&self) -> usize {
        self.ihdr.row_bytes() as usize
    }

    /// True when every row was recovered and the stream is clean.
    pub fn is_complete(&self) -> bool {
        self.rows_recovered == self.rows_total && self.inflate_error.is_none()
    }

    /// Fraction of the image that was recovered: rows for non-interlaced
    /// images, pixels for interlaced ones (a pass of an Adam7 image covers
    /// far fewer pixels than rows).
    pub fn intact_fraction(&self) -> f32 {
        if self.rows_total == 0 {
            return 0.0;
        }
        if !self.interlaced {
            return self.rows_recovered as f32 / self.rows_total as f32;
        }
        let mut have = 0u64;
        let mut all = 0u64;
        for p in self.ihdr.passes() {
            let px = p.width as u64;
            all += px * p.height as u64;
            have += px * self.pass_rows_recovered[p.index].min(p.height) as u64;
        }
        if all == 0 {
            0.0
        } else {
            (have as f64 / all as f64) as f32
        }
    }

    /// The recovered scanline `y` of a non-interlaced image, or of an
    /// interlaced image when `pixels` was kept.
    pub fn row(&self, y: u32) -> Option<&[u8]> {
        let rb = self.row_bytes();
        let start = (y as usize).checked_mul(rb)?;
        self.pixels.get(start..start.checked_add(rb)?)
    }

    /// True when pixel `(x, y)` holds recovered data.
    pub fn pixel_known(&self, x: u32, y: u32) -> bool {
        if x >= self.width || y >= self.height {
            return false;
        }
        if !self.interlaced {
            return y < self.rows_recovered;
        }
        pixel_known(&self.ihdr.passes(), &self.pass_rows_recovered, x, y)
    }
}

/// True when the pixel belongs to a recovered pass row.
fn pixel_known(passes: &[Pass], pass_rows: &[u32; 7], x: u32, y: u32) -> bool {
    for p in passes {
        if x >= p.x0 && y >= p.y0 && (x - p.x0) % p.dx == 0 && (y - p.y0) % p.dy == 0 {
            return (y - p.y0) / p.dy < pass_rows[p.index];
        }
    }
    false
}

/// True when every pass row contributing to image row `y` was recovered.
fn row_fully_known(passes: &[Pass], pass_rows: &[u32; 7], y: u32) -> bool {
    passes.iter().all(|p| {
        if y < p.y0 || (y - p.y0) % p.dy != 0 {
            return true;
        }
        (y - p.y0) / p.dy < pass_rows[p.index]
    })
}

/// A scanline handed to the sink of [`scan_rows`].
pub(crate) struct RowEvent<'a> {
    /// Adam7 pass number (0 for non-interlaced images).
    pub pass: usize,
    /// Row within the pass.
    pub row: u32,
    /// Unfiltered bytes of the row (no filter byte).
    pub data: &'a [u8],
}

/// Statistics of a scan.
pub(crate) struct ScanOutcome {
    pub rows_recovered: u32,
    pub rows_unverified: u32,
    pub rows_total: u32,
    pub pass_rows: [u32; 7],
    pub bytes_inflated: u64,
    pub error: Option<String>,
    pub stream_end: bool,
    pub excess_bytes: u64,
    pub dropped_rows: u32,
    pub zlib_header_ok: bool,
    pub checksum_failed: bool,
    pub crc_capped: bool,
    pub expected: u64,
}

struct Halt {
    msg: String,
    /// Corruption (discard the suspect rows) as opposed to a benign stop.
    corrupt: bool,
}

struct Pending {
    pass: usize,
    row: u32,
    data: Vec<u8>,
    trusted: bool,
}

type Sink<'s> = dyn FnMut(RowEvent<'_>) -> Result<()> + 's;

/// Turns a stream of inflated bytes into unfiltered rows, holding back the
/// last [`SUSPECT_ROWS`] rows until later rows confirm them.
struct Assembler {
    passes: Vec<Pass>,
    pi: usize,
    row: u32,
    row_bytes: usize,
    stride: usize,
    bpp: usize,
    cur: Vec<u8>,
    fill: usize,
    /// Previous unfiltered row of the current pass (zeros at its start).
    prev: Vec<u8>,
    pending: VecDeque<Pending>,
    pool: Vec<Vec<u8>>,
    rows_emitted: u32,
    rows_unverified: u32,
    rows_seen: u64,
    pass_rows: [u32; 7],
    excess: u64,
    dropped: u32,
    /// Rows completing now are covered by verified input.
    trusted: bool,
    ihdr: Ihdr,
}

impl Assembler {
    fn new(ihdr: &Ihdr) -> Result<Self> {
        let passes = ihdr.passes();
        if ihdr.row_bytes() > MAX_ROW_BYTES {
            return Err(Error::unsupported(format!(
                "PNG scanlines of {} bytes are too large to decode",
                ihdr.row_bytes()
            )));
        }
        if ihdr.rows_total() > MAX_ROWS || ihdr.expected_inflated_len() > MAX_IMAGE_BYTES {
            return Err(Error::unsupported(format!(
                "a {}x{} PNG is too large to decode",
                ihdr.width, ihdr.height
            )));
        }
        let mut a = Assembler {
            passes,
            pi: 0,
            row: 0,
            row_bytes: 0,
            stride: 1,
            bpp: ihdr.filter_bpp(),
            cur: Vec::new(),
            fill: 0,
            prev: Vec::new(),
            pending: VecDeque::new(),
            pool: Vec::new(),
            rows_emitted: 0,
            rows_unverified: 0,
            rows_seen: 0,
            pass_rows: [0; 7],
            excess: 0,
            dropped: 0,
            trusted: true,
            ihdr: *ihdr,
        };
        a.set_pass();
        Ok(a)
    }

    fn done(&self) -> bool {
        self.pi >= self.passes.len()
    }

    fn set_pass(&mut self) {
        if let Some(p) = self.passes.get(self.pi) {
            self.row_bytes = p.row_bytes(&self.ihdr) as usize;
            self.stride = 1 + self.row_bytes;
            self.cur.resize(self.stride, 0);
            self.prev.clear();
            self.prev.resize(self.row_bytes, 0);
        }
    }

    fn feed(&mut self, mut data: &[u8], sink: &mut Sink<'_>) -> Result<Option<Halt>> {
        while !data.is_empty() {
            if self.done() {
                self.excess += data.len() as u64;
                if self.excess > MAX_EXCESS {
                    return Ok(Some(Halt {
                        msg: format!(
                            "more than {} MiB of image data after the last row",
                            MAX_EXCESS >> 20
                        ),
                        corrupt: false,
                    }));
                }
                return Ok(None);
            }
            let take = (self.stride - self.fill).min(data.len());
            self.cur[self.fill..self.fill + take].copy_from_slice(&data[..take]);
            self.fill += take;
            data = &data[take..];
            if self.fill == self.stride {
                self.fill = 0;
                if let Some(h) = self.row_done(sink)? {
                    return Ok(Some(h));
                }
            }
        }
        Ok(None)
    }

    fn row_done(&mut self, sink: &mut Sink<'_>) -> Result<Option<Halt>> {
        let ftype = self.cur[0];
        if ftype > 4 {
            return Ok(Some(Halt {
                msg: format!(
                    "invalid filter type {ftype} at row {} (damaged image data)",
                    self.rows_seen
                ),
                corrupt: true,
            }));
        }
        let mut recon = self.pool.pop().unwrap_or_default();
        recon.clear();
        recon.resize(self.row_bytes, 0);
        unfilter(ftype, &self.cur[1..], &self.prev, &mut recon, self.bpp);
        self.prev.copy_from_slice(&recon);
        let pass = self.passes[self.pi].index;
        self.pending.push_back(Pending {
            pass,
            row: self.row,
            data: recon,
            trusted: self.trusted,
        });
        self.rows_seen += 1;
        while self.pending.len() > SUSPECT_ROWS {
            if let Some(p) = self.pending.pop_front() {
                self.emit(p, sink)?;
            }
        }
        self.row += 1;
        if self.row >= self.passes[self.pi].height {
            self.pi += 1;
            self.row = 0;
            self.set_pass();
        }
        Ok(None)
    }

    fn emit(&mut self, p: Pending, sink: &mut Sink<'_>) -> Result<()> {
        if p.trusted {
            sink(RowEvent {
                pass: p.pass,
                row: p.row,
                data: &p.data,
            })?;
            self.rows_emitted += 1;
            self.pass_rows[p.pass] += 1;
        } else {
            self.rows_unverified += 1;
        }
        self.pool.push(p.data);
        Ok(())
    }

    /// Input covered by a verified CRC has been used up: the rows completed
    /// so far are certain, whatever follows is not.
    fn close_trust(&mut self, sink: &mut Sink<'_>) -> Result<()> {
        while let Some(p) = self.pending.pop_front() {
            self.emit(p, sink)?;
        }
        self.trusted = false;
        Ok(())
    }

    /// Release the held-back rows: confirmed when the data ended cleanly,
    /// discarded when corruption was detected.
    fn finish(&mut self, clean: bool, sink: &mut Sink<'_>) -> Result<()> {
        if clean {
            while let Some(p) = self.pending.pop_front() {
                self.emit(p, sink)?;
            }
        } else {
            self.dropped = self.pending.iter().filter(|p| p.trusted).count() as u32;
            self.rows_unverified += self.pending.iter().filter(|p| !p.trusted).count() as u32;
            self.pending.clear();
        }
        Ok(())
    }
}

/// Reads the concatenated IDAT payloads from a source.
struct Feed<'a> {
    src: &'a dyn Source,
    base: u64,
    ranges: &'a [Range],
    idx: usize,
    off: u64,
    /// Stream offset of the next byte to deliver.
    delivered: u64,
    /// Do not deliver bytes at or beyond this stream offset.
    limit: Option<u64>,
    block: usize,
    holes: Option<RangeSet>,
    stop_msg: Option<String>,
}

impl<'a> Feed<'a> {
    fn new(src: &'a dyn Source, base: u64, ranges: &'a [Range], block: usize) -> Self {
        Feed {
            src,
            base,
            ranges,
            idx: 0,
            off: 0,
            delivered: 0,
            limit: None,
            block,
            holes: src.unreadable().filter(|s| !s.is_empty()),
            stop_msg: None,
        }
    }

    /// Append up to one block of bytes to `buf`; returns how many were
    /// added (0 at the end of the data or at the limit).
    fn fill(&mut self, buf: &mut Vec<u8>) -> usize {
        let start_len = buf.len();
        while buf.len() < self.block && self.idx < self.ranges.len() && self.stop_msg.is_none() {
            let r = self.ranges[self.idx];
            if self.off >= r.len() {
                self.idx += 1;
                self.off = 0;
                continue;
            }
            let abs = self.base + r.start + self.off;
            let mut remaining = r.len() - self.off;
            if let Some(limit) = self.limit {
                if self.delivered >= limit {
                    break;
                }
                remaining = remaining.min(limit - self.delivered);
            }
            if let Some(holes) = &self.holes {
                if let Some(hit) = holes.first_intersecting(Range::new(abs, abs + remaining)) {
                    if hit.start <= abs {
                        self.stop_msg = Some(format!(
                            "image data is unreadable from offset {}",
                            abs - self.base
                        ));
                        break;
                    }
                    remaining = hit.start - abs;
                }
            }
            let n = remaining.min((self.block - buf.len()) as u64) as usize;
            let at = buf.len();
            buf.resize(at + n, 0);
            match self.src.read_at(abs, &mut buf[at..]) {
                Ok(0) => {
                    buf.truncate(at);
                    self.stop_msg = Some("image data ends early (source is shorter)".to_string());
                }
                Ok(got) => {
                    buf.truncate(at + got);
                    self.off += got as u64;
                    self.delivered += got as u64;
                }
                Err(e) => {
                    buf.truncate(at);
                    self.stop_msg = Some(format!(
                        "image data is unreadable from offset {}",
                        e.offset.saturating_sub(self.base)
                    ));
                }
            }
        }
        buf.len() - start_len
    }
}

/// True when the two bytes form a valid zlib header for deflate.
fn zlib_header_valid(cmf: u8, flg: u8) -> bool {
    (cmf & 0x0F) == 8
        && (cmf >> 4) <= 7
        && ((cmf as u16) << 8 | flg as u16) % 31 == 0
        && flg & 0x20 == 0
}

/// Inflate the image data of `walk` and feed the unfiltered rows to `sink`
/// as they become trustworthy. See the module documentation for what is
/// discarded.
///
/// Chunk CRCs add evidence the inflater cannot supply: a flipped bit often
/// leaves the deflate stream decodable (Huffman codes resynchronise), so the
/// damage shows up late or only in the Adler-32. When the image data is split
/// over several IDAT chunks and one of them fails its CRC, the rows completed
/// *before* that chunk are the only ones vouched for (every earlier chunk
/// matched its CRC, so everything decoded from them is real). The exception
/// is a stream that decodes completely with a valid Adler-32: then only the
/// stored CRC was damaged.
pub(crate) fn scan_rows(
    src: &dyn Source,
    walk: &ChunkWalk,
    ihdr: &Ihdr,
    sink: &mut Sink<'_>,
) -> Result<ScanOutcome> {
    let first_bad = walk
        .idat_status
        .iter()
        .position(|&(ok, complete)| !ok && complete);
    if let (Some(k), true) = (first_bad, walk.idat_ranges.len() > 1) {
        let before: u64 = walk.idat_ranges[..k].iter().map(Range::len).sum();
        // Dry run: does the whole stream decode with a matching checksum?
        let mut null = |_: RowEvent<'_>| -> Result<()> { Ok(()) };
        let pre = scan_inner(src, walk, ihdr, None, &mut null)?;
        let clean = pre.stream_end && pre.error.is_none();
        if !clean {
            let mut out = scan_inner(src, walk, ihdr, Some(before), sink)?;
            out.crc_capped = true;
            return Ok(out);
        }
    }
    scan_inner(src, walk, ihdr, None, sink)
}

fn scan_inner(
    src: &dyn Source,
    walk: &ChunkWalk,
    ihdr: &Ihdr,
    boundary: Option<u64>,
    sink: &mut Sink<'_>,
) -> Result<ScanOutcome> {
    let block = BLOCK;
    let rows_total = u32::try_from(ihdr.rows_total()).unwrap_or(u32::MAX);
    let expected = ihdr.expected_inflated_len();
    let mut asm = Assembler::new(ihdr)?;
    let mut feed = Feed::new(src, walk.start, &walk.idat_ranges, block);
    feed.limit = boundary;
    let mut inbuf: Vec<u8> = Vec::with_capacity(block);
    let mut inpos = 0usize;
    let mut input_done = false;
    if feed.fill(&mut inbuf) == 0 {
        input_done = true;
    }

    let mut zlib_header_ok = true;
    let use_zlib = if walk.cgbi {
        false
    } else if inbuf.len() >= 2 {
        if zlib_header_valid(inbuf[0], inbuf[1]) {
            true
        } else {
            zlib_header_ok = false;
            inpos = 2;
            false
        }
    } else {
        true
    };
    let mut dec = Decompress::new(use_zlib);
    let mut stage = vec![0u8; BLOCK];
    let mut error: Option<String> = None;
    let mut stream_end = false;
    let mut corrupt = false;
    let mut checksum_failed = false;
    let mut trust_closed = boundary.is_none();

    loop {
        if inpos >= inbuf.len() && !input_done {
            inbuf.clear();
            inpos = 0;
            if feed.fill(&mut inbuf) == 0 {
                input_done = true;
            }
        }
        let before_in = dec.total_in();
        let before_out = dec.total_out();
        let res = dec.decompress(&inbuf[inpos..], &mut stage, FlushDecompress::None);
        let used = (dec.total_in() - before_in) as usize;
        let made = (dec.total_out() - before_out) as usize;
        inpos += used;
        if made > 0 {
            if let Some(h) = asm.feed(&stage[..made], sink)? {
                error = Some(h.msg);
                corrupt = h.corrupt;
                break;
            }
        }
        match res {
            Ok(Status::StreamEnd) => {
                stream_end = true;
                break;
            }
            Ok(_) => {
                if used == 0 && made == 0 {
                    if inpos >= inbuf.len() {
                        if input_done {
                            if !trust_closed {
                                // Everything before the suspect chunk is used
                                // up; carry on past it.
                                asm.close_trust(sink)?;
                                trust_closed = true;
                                feed.limit = None;
                                input_done = false;
                                continue;
                            }
                            break;
                        }
                        continue;
                    }
                    error = Some("inflate stalled on damaged data".to_string());
                    corrupt = true;
                    break;
                }
            }
            Err(e) => {
                if dec.total_out() >= expected && asm.done() {
                    // Every byte was produced; only the checksum failed.
                    checksum_failed = true;
                    error = Some(
                        "zlib checksum (Adler-32) mismatch: some pixel data is corrupt".to_string(),
                    );
                } else {
                    error = Some(format!(
                        "corrupt deflate stream after {} bytes of image data ({e})",
                        dec.total_out()
                    ));
                    corrupt = true;
                }
                break;
            }
        }
        // Reached the suspect chunk exactly: the decoder has digested all
        // verified input.
        if !trust_closed && inpos >= inbuf.len() && feed.delivered >= boundary.unwrap_or(u64::MAX) {
            asm.close_trust(sink)?;
            trust_closed = true;
            feed.limit = None;
            input_done = false;
        }
    }

    asm.finish(!corrupt, sink)?;

    let rows_recovered = asm.rows_emitted;
    let decoded = rows_recovered + asm.rows_unverified;
    if error.is_none() {
        if let Some(m) = feed.stop_msg.take().filter(|_| !stream_end) {
            error = Some(m);
        } else if !stream_end {
            error = Some(if decoded == rows_total {
                "zlib stream is cut short (missing end marker or checksum)".to_string()
            } else {
                format!("image data ends early: {decoded} of {rows_total} rows")
            });
        } else if decoded < rows_total {
            error = Some(format!(
                "image data ends early: {decoded} of {rows_total} rows"
            ));
        } else if asm.excess > 0 {
            error = Some(format!(
                "{} extra bytes of image data after the last row",
                asm.excess
            ));
        }
    }
    if !zlib_header_ok {
        let msg = "invalid zlib header (inflated as raw deflate)".to_string();
        error = Some(match error {
            Some(e) => format!("{msg}; {e}"),
            None => msg,
        });
    }

    Ok(ScanOutcome {
        rows_recovered,
        rows_unverified: asm.rows_unverified,
        rows_total,
        pass_rows: asm.pass_rows,
        bytes_inflated: dec.total_out(),
        error,
        stream_end,
        excess_bytes: asm.excess,
        dropped_rows: asm.dropped,
        zlib_header_ok,
        checksum_failed,
        crc_capped: false,
        expected,
    })
}

/// Decode a PNG held in memory, salvaging every trustworthy row. Fails only
/// when there is no usable IHDR or the image is too large to buffer.
pub fn decode(data: &[u8]) -> Result<LenientImage> {
    decode_with(data, &LenientOptions::default())
}

/// [`decode`] with explicit options.
pub fn decode_with(data: &[u8], opts: &LenientOptions) -> Result<LenientImage> {
    let src = SliceSource(data);
    let walk = chunks::walk(&src, 0, data.len() as u64)?;
    decode_source(&src, &walk, opts)
}

/// Decode the PNG described by `walk` from `src`.
pub fn decode_source(
    src: &dyn Source,
    walk: &ChunkWalk,
    opts: &LenientOptions,
) -> Result<LenientImage> {
    let ihdr = walk
        .ihdr
        .ok_or_else(|| Error::corrupt("PNG IHDR", "no IHDR chunk found"))?;
    decode_with_ihdr(src, walk, &ihdr, opts)
}

/// Statistics-only decode (no pixel buffer): how much of the image data is
/// intact. Constant memory regardless of image size.
pub fn analyze(src: &dyn Source, walk: &ChunkWalk) -> Result<LenientImage> {
    decode_source(
        src,
        walk,
        &LenientOptions {
            keep_pixels: false,
            ..Default::default()
        },
    )
}

/// Like [`analyze`] but with an explicit header (for example one borrowed
/// from a donor file).
pub(crate) fn analyze_with(
    src: &dyn Source,
    walk: &ChunkWalk,
    ihdr: &Ihdr,
) -> Result<LenientImage> {
    decode_with_ihdr(
        src,
        walk,
        ihdr,
        &LenientOptions {
            keep_pixels: false,
            ..Default::default()
        },
    )
}

pub(crate) fn decode_with_ihdr(
    src: &dyn Source,
    walk: &ChunkWalk,
    ihdr: &Ihdr,
    opts: &LenientOptions,
) -> Result<LenientImage> {
    ihdr.check().map_err(|e| Error::corrupt("PNG IHDR", e))?;
    let row_bytes = ihdr.row_bytes();
    if row_bytes > MAX_ROW_BYTES {
        return Err(Error::unsupported(format!(
            "PNG scanlines of {row_bytes} bytes are too large to decode"
        )));
    }
    let interlaced = ihdr.is_interlaced();
    let full_bytes = row_bytes.saturating_mul(ihdr.height as u64);
    if opts.keep_pixels && full_bytes > opts.max_pixel_bytes {
        return Err(Error::unsupported(format!(
            "a {}x{} PNG needs {} bytes of pixel buffer (limit {})",
            ihdr.width, ihdr.height, full_bytes, opts.max_pixel_bytes
        )));
    }
    let mut pixels: Vec<u8> = Vec::new();
    if opts.keep_pixels {
        if interlaced {
            pixels = vec![0u8; full_bytes as usize];
        } else {
            pixels.reserve(full_bytes.min(16 << 20) as usize);
        }
    }
    let passes = ihdr.passes();
    let keep = opts.keep_pixels;
    let rb = row_bytes as usize;
    let mut sink = |ev: RowEvent<'_>| -> Result<()> {
        if !keep {
            return Ok(());
        }
        if !interlaced {
            pixels.extend_from_slice(ev.data);
        } else if let Some(p) = passes.iter().find(|p| p.index == ev.pass) {
            scatter(&mut pixels, ihdr, rb, p, ev.row, ev.data);
        }
        Ok(())
    };
    let out = scan_rows(src, walk, ihdr, &mut sink)?;

    let mask = if keep {
        if interlaced {
            (0..ihdr.height)
                .map(|y| row_fully_known(&passes, &out.pass_rows, y))
                .collect()
        } else {
            (0..ihdr.height).map(|y| y < out.rows_recovered).collect()
        }
    } else {
        Vec::new()
    };
    Ok(LenientImage {
        width: ihdr.width,
        height: ihdr.height,
        color_type: ihdr.color_type,
        bit_depth: ihdr.bit_depth,
        interlaced,
        rows_recovered: out.rows_recovered,
        rows_unverified: out.rows_unverified,
        rows_total: out.rows_total,
        bytes_inflated: out.bytes_inflated,
        inflate_error: out.error,
        pixels,
        recovered_mask_rows: mask,
        ihdr: *ihdr,
        pass_rows_recovered: out.pass_rows,
        expected_bytes: out.expected,
        stream_complete: out.stream_end
            && out.zlib_header_ok
            && out.excess_bytes == 0
            && out.rows_recovered == out.rows_total,
        excess_bytes: out.excess_bytes,
        dropped_rows: out.dropped_rows,
        zlib_header_ok: out.zlib_header_ok,
        checksum_failed: out.checksum_failed,
        crc_capped: out.crc_capped,
    })
}

/// Read pixel `x` of a packed row as raw bytes (sample bits for depths under
/// 8 are returned in the low bits of byte 0).
pub(crate) fn get_pixel(row: &[u8], x: usize, bits: u32) -> [u8; 8] {
    let mut v = [0u8; 8];
    if bits >= 8 {
        let n = (bits / 8) as usize;
        if let Some(s) = row.get(x * n..x * n + n) {
            v[..n].copy_from_slice(s);
        }
    } else {
        let bit = x * bits as usize;
        if let Some(&b) = row.get(bit / 8) {
            let shift = 8 - bits as usize - (bit % 8);
            v[0] = (b >> shift) & ((1u16 << bits) - 1) as u8;
        }
    }
    v
}

/// Write pixel `x` of a packed row (inverse of [`get_pixel`]).
pub(crate) fn set_pixel(row: &mut [u8], x: usize, bits: u32, v: &[u8; 8]) {
    if bits >= 8 {
        let n = (bits / 8) as usize;
        if let Some(s) = row.get_mut(x * n..x * n + n) {
            s.copy_from_slice(&v[..n]);
        }
    } else {
        let bit = x * bits as usize;
        if let Some(b) = row.get_mut(bit / 8) {
            let shift = 8 - bits as usize - (bit % 8);
            let mask = (((1u16 << bits) - 1) as u8) << shift;
            *b = (*b & !mask) | ((v[0] << shift) & mask);
        }
    }
}

/// Place one decoded pass row into the full-image buffer.
fn scatter(pixels: &mut [u8], ihdr: &Ihdr, row_bytes: usize, p: &Pass, row: u32, data: &[u8]) {
    let y = p.y0 as usize + row as usize * p.dy as usize;
    let Some(dst) = pixels.get_mut(y * row_bytes..(y + 1) * row_bytes) else {
        return;
    };
    let bits = ihdr.bits_per_pixel();
    for i in 0..p.width as usize {
        let x = p.x0 as usize + i * p.dx as usize;
        let v = get_pixel(data, i, bits);
        set_pixel(dst, x, bits, &v);
    }
}

/// Display cell `(width, height)` of an Adam7 pass pixel: the block of the
/// final image a pixel of that pass stands in for until later passes refine
/// it (the "blocky" progressive display of libpng).
const ADAM7_CELLS: [(usize, usize); 7] = [(8, 8), (4, 8), (4, 4), (2, 4), (2, 2), (1, 2), (1, 1)];

/// Turn the recovered passes of an interlaced image into a full-frame
/// preview: every recovered pass pixel is replicated over its display cell
/// (later passes overwrite the cells of earlier ones), everything not covered
/// by a recovered pass row gets `fill`. Operates in place on `img.pixels`,
/// which must have been kept.
pub(crate) fn upsample_interlaced(img: &mut LenientImage, fill: &[u8; 8]) {
    let ihdr = img.ihdr;
    let bits = ihdr.bits_per_pixel();
    let rb = img.row_bytes();
    let (w, h) = (img.width as usize, img.height as usize);
    if img.pixels.len() < rb * h {
        return;
    }
    let mut out = vec![0u8; rb * h];
    if bits >= 8 {
        // Whole-byte pixels: plain slice copies.
        let n = (bits / 8) as usize;
        for row in out.chunks_mut(rb) {
            for px in row.chunks_exact_mut(n).take(w) {
                px.copy_from_slice(&fill[..n]);
            }
        }
        for p in ihdr.passes() {
            let (cw, ch) = ADAM7_CELLS[p.index];
            let rows = img.pass_rows_recovered[p.index].min(p.height) as usize;
            for r in 0..rows {
                let y = p.y0 as usize + r * p.dy as usize;
                for i in 0..p.width as usize {
                    let x = p.x0 as usize + i * p.dx as usize;
                    let mut v = [0u8; 8];
                    v[..n].copy_from_slice(&img.pixels[y * rb + x * n..y * rb + x * n + n]);
                    for yy in y..(y + ch).min(h) {
                        for xx in x..(x + cw).min(w) {
                            out[yy * rb + xx * n..yy * rb + xx * n + n].copy_from_slice(&v[..n]);
                        }
                    }
                }
            }
        }
    } else {
        for y in 0..h {
            let row = &mut out[y * rb..(y + 1) * rb];
            for x in 0..w {
                set_pixel(row, x, bits, fill);
            }
        }
        for p in ihdr.passes() {
            let (cw, ch) = ADAM7_CELLS[p.index];
            let rows = img.pass_rows_recovered[p.index].min(p.height) as usize;
            for r in 0..rows {
                let y = p.y0 as usize + r * p.dy as usize;
                for i in 0..p.width as usize {
                    let x = p.x0 as usize + i * p.dx as usize;
                    let v = get_pixel(&img.pixels[y * rb..(y + 1) * rb], x, bits);
                    for yy in y..(y + ch).min(h) {
                        let row = &mut out[yy * rb..(yy + 1) * rb];
                        for xx in x..(x + cw).min(w) {
                            set_pixel(row, xx, bits, &v);
                        }
                    }
                }
            }
        }
    }
    img.pixels = out;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn ihdr(w: u32, h: u32, depth: u8, color: u8, interlace: u8) -> Ihdr {
        Ihdr {
            width: w,
            height: h,
            bit_depth: depth,
            color_type: color,
            compression: 0,
            filter: 0,
            interlace,
        }
    }

    /// Build a minimal non-interlaced 8-bit gray PNG with filter type 0.
    fn gray_png(w: u32, h: u32) -> (Vec<u8>, Vec<u8>) {
        let mut raw = Vec::new();
        let mut pix = Vec::new();
        for y in 0..h {
            raw.push(0u8);
            for x in 0..w {
                let v = (x * 7 + y * 13) as u8;
                raw.push(v);
                pix.push(v);
            }
        }
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::new(6));
        z.write_all(&raw).unwrap();
        let z = z.finish().unwrap();
        let mut out = crate::chunks::SIGNATURE.to_vec();
        let mut put = |kind: &[u8; 4], data: &[u8]| {
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            out.extend_from_slice(kind);
            out.extend_from_slice(data);
            out.extend_from_slice(&crate::io::chunk_crc(kind, data).to_be_bytes());
        };
        put(b"IHDR", &ihdr(w, h, 8, 0, 0).to_bytes());
        put(b"IDAT", &z);
        put(b"IEND", &[]);
        (out, pix)
    }

    #[test]
    fn decodes_a_clean_image() {
        let (png, pix) = gray_png(37, 29);
        let img = decode(&png).unwrap();
        assert_eq!(img.rows_recovered, 29);
        assert_eq!(img.rows_total, 29);
        assert!(img.inflate_error.is_none(), "{:?}", img.inflate_error);
        assert!(img.stream_complete);
        assert_eq!(img.pixels, pix);
        assert!(img.recovered_mask_rows.iter().all(|&b| b));
        assert_eq!(img.intact_fraction(), 1.0);
    }

    #[test]
    fn clean_truncation_keeps_every_complete_row() {
        let (png, pix) = gray_png(64, 64);
        // Cut inside the IDAT payload: header 8 + IHDR 25 + IDAT header 8.
        let idat_len = png.len() - 8 - 25 - 12 - 12;
        let cut = 8 + 25 + 8 + idat_len / 2;
        let img = decode(&png[..cut]).unwrap();
        assert!(img.rows_recovered > 0 && img.rows_recovered < 64);
        assert!(img.inflate_error.is_some());
        assert_eq!(img.dropped_rows, 0);
        let n = img.rows_recovered as usize * 64;
        assert_eq!(img.pixels.len(), n);
        assert_eq!(&img.pixels[..], &pix[..n]);
    }

    #[test]
    fn corruption_drops_the_suspect_rows() {
        // Stored-block deflate: damage a filter byte directly.
        let w = 20u32;
        let h = 12u32;
        let mut raw = Vec::new();
        for y in 0..h {
            raw.push(0u8);
            for x in 0..w {
                raw.push((x + y) as u8);
            }
        }
        // Row 7's filter byte becomes invalid.
        let stride = 1 + w as usize;
        raw[7 * stride] = 9;
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::none());
        z.write_all(&raw).unwrap();
        let z = z.finish().unwrap();
        let mut png = crate::chunks::SIGNATURE.to_vec();
        for (kind, data) in [
            (b"IHDR", ihdr(w, h, 8, 0, 0).to_bytes().to_vec()),
            (b"IDAT", z),
            (b"IEND", Vec::new()),
        ] {
            png.extend_from_slice(&(data.len() as u32).to_be_bytes());
            png.extend_from_slice(kind);
            png.extend_from_slice(&data);
            png.extend_from_slice(&crate::io::chunk_crc(kind, &data).to_be_bytes());
        }
        let img = decode(&png).unwrap();
        // Rows 0..=6 decode; the last SUSPECT_ROWS of them are dropped.
        assert_eq!(img.rows_recovered as usize, 7 - SUSPECT_ROWS);
        assert_eq!(img.dropped_rows as usize, SUSPECT_ROWS);
        assert!(img.inflate_error.as_ref().unwrap().contains("filter"));
    }

    #[test]
    fn no_ihdr_is_an_error() {
        assert!(decode(b"").is_err());
        assert!(decode(b"not a png at all").is_err());
        assert!(decode(&crate::chunks::SIGNATURE).is_err());
    }

    #[test]
    fn absurd_dimensions_are_refused_not_allocated() {
        let mut png = crate::chunks::SIGNATURE.to_vec();
        let h = ihdr(60_000, 60_000, 16, 6, 0).to_bytes();
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&h);
        png.extend_from_slice(&crate::io::chunk_crc(b"IHDR", &h).to_be_bytes());
        assert!(decode(&png).is_err());
        // Analysis refuses such sizes up front too (no hour-long inflate).
        let src = SliceSource(&png);
        let walk = chunks::walk(&src, 0, png.len() as u64).unwrap();
        assert!(analyze(&src, &walk).is_err());
    }

    #[test]
    fn pixel_helpers_roundtrip() {
        for bits in [1u32, 2, 4, 8, 16, 24, 32, 48, 64] {
            let n = 13usize;
            let mut row = vec![0u8; (n * bits as usize).div_ceil(8)];
            let max = if bits < 8 { (1u16 << bits) - 1 } else { 255 };
            for x in 0..n {
                let mut v = [0u8; 8];
                if bits < 8 {
                    v[0] = (x as u16 % (max + 1)) as u8;
                } else {
                    for (i, b) in v.iter_mut().enumerate().take((bits / 8) as usize) {
                        *b = (x * 11 + i * 5) as u8;
                    }
                }
                set_pixel(&mut row, x, bits, &v);
                assert_eq!(get_pixel(&row, x, bits), v, "bits {bits} x {x}");
            }
        }
    }
}
