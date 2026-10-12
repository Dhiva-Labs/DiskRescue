//! The signature table and the multi-pattern search over a chunk buffer.
//!
//! Every [`Signature`] of every registered format contributes its primary
//! `bytes`. Identical byte strings are merged, so each distinct pattern is
//! held once and remembers all `(format, signature, offset)` triples that
//! use it.
//!
//! Two search paths cover the patterns:
//!
//! * **Automaton.** One Aho-Corasick automaton (leftmost-first, DFA,
//!   prefilter on) finds the leftmost position where any of its patterns
//!   matches; the search then resumes one byte later, so every position with
//!   at least one match is visited. At such a position all automaton
//!   patterns that start with the same byte are compared directly, which
//!   recovers overlapping and same-position matches that leftmost-first
//!   semantics would otherwise hide (a TIFF magic that is a prefix of a CR2
//!   magic, for example).
//! * **Grid probes.** A pattern of one or two bytes (the MPEG-TS sync byte,
//!   MP3 frame sync, `BM`) matches every few hundred bytes of random data;
//!   in the automaton it would stop the search constantly and disable its
//!   prefilter (one such pattern cut the scan rate from 3.5 GB/s to under
//!   0.5 GB/s). When every signature using such a pattern is only accepted
//!   on a grid of at least [`DIRECT_MIN_STEP`] bytes, the pattern stays out
//!   of the automaton and is compared directly at each grid position (two
//!   loads per sector for MPEG-TS, as the carving spec suggests).
//!
//! Weak signatures (fewer than [`WEAK_SIG_BYTES`] fixed bytes, primary plus
//! `also`) are accepted only at sector-aligned starts whatever the alignment
//! option, as the carving spec requires for TS, M2TS, bare MP3 frames and
//! BMP (spec 3.1, `needs_sector_align`): a deep scan (`alignment` 1) does
//! not try them at every byte.
//!
//! A hit at absolute position `p` of a pattern used with offset `o` names
//! the candidate file start `p - o`. Candidates are kept only when that start
//! lies in the caller's "owned" range, is on the signature's grid and the
//! complete signature (including its `also` pattern) matches the bytes at
//! the start.
//!
//! Zero skip: a 4 KiB block that is entirely zero cannot contain any byte of
//! a pattern that has a non-zero byte, but a pattern may straddle the block
//! edge (an ISO BMFF size `00 00 00 18` before `ftyp`). Non-zero runs of
//! blocks are therefore searched with a margin of `longest pattern - 1` bytes
//! into the neighbouring zero blocks. Zero skip is disabled when an
//! automaton pattern is all zero bytes or longer than half a block. Grid
//! probes are cheap and also run over zero blocks.

use std::sync::Arc;

use aho_corasick::{AhoCorasick, AhoCorasickKind, Input, MatchKind};
use diskrescue_core::format::{FileFormat, Signature};
use diskrescue_core::{Error, Result};

use crate::types::ZERO_BLOCK;

/// Signatures whose probe length exceeds this are refused.
pub(crate) const MAX_PROBE_LEN: u64 = 16 << 20;

/// Signatures with fewer fixed bytes than this (primary plus `also`) are
/// weak: accepted only at multiples of [`WEAK_SIG_ALIGN`].
pub(crate) const WEAK_SIG_BYTES: usize = 3;

/// Sector grid that weak signatures must start on.
pub(crate) const WEAK_SIG_ALIGN: u64 = 512;

/// Patterns of at most this many bytes are probed on their grid instead of
/// being put in the automaton (when the grid is coarse enough).
const SHORT_PATTERN: usize = 2;

/// Smallest grid step for which grid probes are cheaper than the automaton.
pub(crate) const DIRECT_MIN_STEP: u64 = 16;

/// One use of a pattern by a format signature.
#[derive(Debug, Clone)]
struct SigRef {
    fmt: usize,
    sig: Signature,
    offset: u64,
    /// Candidate starts must be multiples of this.
    step: u64,
}

/// A grid-probed use of a short pattern.
#[derive(Debug, Clone)]
struct DirectRef {
    pat: &'static [u8],
    r: SigRef,
}

/// Greatest common divisor.
fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// Least common multiple of two non-zero values, saturating.
fn lcm(a: u64, b: u64) -> u64 {
    let (a, b) = (a.max(1), b.max(1));
    (a / gcd(a, b)).saturating_mul(b)
}

/// Grid a signature's candidate starts must lie on, given the alignment
/// option (absolute source offsets).
pub(crate) fn sig_step(sig: &Signature, alignment: u64) -> u64 {
    let alignment = alignment.max(1);
    let fixed = sig.bytes.len() + sig.also.map_or(0, |(b, _)| b.len());
    if fixed < WEAK_SIG_BYTES {
        lcm(alignment, WEAK_SIG_ALIGN)
    } else {
        alignment
    }
}

/// A candidate found by the search: a file of format `fmt` may start at
/// `start`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Hit {
    pub start: u64,
    pub fmt: usize,
}

/// Where to search and what to keep.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SearchArea {
    /// Absolute offset of `buf[0]`.
    pub buf_base: u64,
    /// Candidate starts in `[own_start, own_end)` are reported.
    pub own_start: u64,
    /// See `own_start`.
    pub own_end: u64,
    /// Skip all-zero 4 KiB blocks.
    pub zero_skip: bool,
}

/// The compiled signature table.
pub(crate) struct PatternSet {
    /// Automaton over `pats` (none when every pattern is grid-probed).
    ac: Option<AhoCorasick>,
    pats: Vec<&'static [u8]>,
    refs: Vec<Vec<SigRef>>,
    by_first: Vec<Vec<u32>>,
    /// Grid-probed signature uses, grouped by grid step.
    direct: Vec<(u64, Vec<DirectRef>)>,
    max_len: usize,
    max_tail: u64,
    max_probe: u64,
    zero_safe: bool,
}

impl PatternSet {
    /// Compile the signatures of `formats` (index = format index) for scans
    /// with the given alignment option (0 is treated as 1).
    pub(crate) fn build(formats: &[Arc<dyn FileFormat>], alignment: u64) -> Result<Self> {
        let mut pats: Vec<&'static [u8]> = Vec::new();
        let mut refs: Vec<Vec<SigRef>> = Vec::new();
        let mut max_probe = 0u64;
        for (fi, f) in formats.iter().enumerate() {
            let sigs = f.signatures();
            if sigs.is_empty() {
                return Err(Error::InvalidArgument(format!(
                    "format {} has no signatures and cannot be carved",
                    f.id()
                )));
            }
            for s in sigs {
                if s.bytes.is_empty() {
                    return Err(Error::InvalidArgument(format!(
                        "format {} has an empty signature",
                        f.id()
                    )));
                }
                let probe = s.probe_len();
                if probe > MAX_PROBE_LEN || s.offset > MAX_PROBE_LEN {
                    return Err(Error::InvalidArgument(format!(
                        "format {} has a signature {probe} bytes into the file; at most {MAX_PROBE_LEN} is supported",
                        f.id()
                    )));
                }
                max_probe = max_probe.max(probe);
                let idx = match pats.iter().position(|p| *p == s.bytes) {
                    Some(i) => i,
                    None => {
                        pats.push(s.bytes);
                        refs.push(Vec::new());
                        pats.len() - 1
                    }
                };
                refs[idx].push(SigRef {
                    fmt: fi,
                    sig: s.clone(),
                    offset: s.offset,
                    step: sig_step(s, alignment),
                });
            }
        }
        if pats.is_empty() {
            return Err(Error::InvalidArgument(
                "no formats registered for carving".into(),
            ));
        }
        // Short patterns whose every use sits on a coarse grid are probed
        // on that grid; everything else goes in the automaton.
        let mut ac_pats: Vec<&'static [u8]> = Vec::new();
        let mut ac_refs: Vec<Vec<SigRef>> = Vec::new();
        let mut direct: Vec<(u64, Vec<DirectRef>)> = Vec::new();
        for (p, rs) in pats.into_iter().zip(refs) {
            let grid = p.len() <= SHORT_PATTERN && rs.iter().all(|r| r.step >= DIRECT_MIN_STEP);
            if !grid {
                ac_pats.push(p);
                ac_refs.push(rs);
                continue;
            }
            for r in rs {
                let step = r.step;
                let d = DirectRef { pat: p, r };
                match direct.iter_mut().find(|(st, _)| *st == step) {
                    Some((_, v)) => v.push(d),
                    None => direct.push((step, vec![d])),
                }
            }
        }
        let mut by_first = vec![Vec::new(); 256];
        for (i, p) in ac_pats.iter().enumerate() {
            by_first[p[0] as usize].push(i as u32);
        }
        let max_len = ac_pats.iter().map(|p| p.len()).max().unwrap_or(1);
        let max_tail = ac_refs
            .iter()
            .flatten()
            .map(|r| r.offset + r.sig.bytes.len() as u64)
            .max()
            .unwrap_or(0);
        let zero_safe =
            ac_pats.iter().all(|p| p.iter().any(|&b| b != 0)) && max_len <= ZERO_BLOCK / 2;
        let ac = if ac_pats.is_empty() {
            None
        } else {
            let built = AhoCorasick::builder()
                .match_kind(MatchKind::LeftmostFirst)
                .kind(Some(AhoCorasickKind::DFA))
                .prefilter(true)
                .build(&ac_pats)
                .or_else(|e| {
                    log::debug!("DFA build failed ({e}); falling back to automatic automaton");
                    AhoCorasick::builder()
                        .match_kind(MatchKind::LeftmostFirst)
                        .build(&ac_pats)
                })
                .map_err(|e| Error::other(format!("cannot build signature automaton: {e}")))?;
            Some(built)
        };
        log::debug!(
            "signature table: {} formats, {} automaton patterns (longest {} bytes), {} grid-probed uses, probe length {}, zero skip {}",
            formats.len(),
            ac_pats.len(),
            max_len,
            direct.iter().map(|(_, v)| v.len()).sum::<usize>(),
            max_probe,
            if zero_safe { "possible" } else { "disabled" }
        );
        Ok(PatternSet {
            ac,
            pats: ac_pats,
            refs: ac_refs,
            by_first,
            direct,
            max_len,
            max_tail,
            max_probe,
            zero_safe,
        })
    }

    /// Longest signature probe length (bytes from the file start needed to
    /// check every signature, `also` patterns included).
    pub(crate) fn max_probe_len(&self) -> u64 {
        self.max_probe
    }

    /// Whether zero skip can be used with this table.
    pub(crate) fn zero_safe(&self) -> bool {
        self.zero_safe
    }

    /// Append every candidate whose start is owned by `area` to `out`, in
    /// no particular order and possibly with duplicates.
    pub(crate) fn search(&self, buf: &[u8], area: SearchArea, out: &mut Vec<Hit>) {
        let buf_end = area.buf_base + buf.len() as u64;
        if area.own_start >= area.own_end
            || area.own_start < area.buf_base
            || area.own_start >= buf_end
        {
            return;
        }
        self.search_grid(buf, &area, out);
        if self.ac.is_none() {
            return;
        }
        let region_end = buf_end.min(area.own_end.saturating_add(self.max_tail));
        let lo = (area.own_start - area.buf_base) as usize;
        let hi = (region_end - area.buf_base) as usize;
        if !(area.zero_skip && self.zero_safe) {
            self.search_span(buf, lo, hi, &area, out);
            return;
        }
        let margin = self.max_len.saturating_sub(1);
        let block = ZERO_BLOCK as u64;
        let mut run: Option<usize> = None;
        let mut i = lo;
        while i < hi {
            let abs = area.buf_base + i as u64;
            let block_end = ((abs / block + 1) * block - area.buf_base) as usize;
            let j = block_end.min(hi);
            let zero = j - i == ZERO_BLOCK && is_zero(&buf[i..j]);
            if zero {
                if let Some(rs) = run.take() {
                    self.search_span(
                        buf,
                        rs.saturating_sub(margin).max(lo),
                        (i + margin).min(hi),
                        &area,
                        out,
                    );
                }
            } else if run.is_none() {
                run = Some(i);
            }
            i = j;
        }
        if let Some(rs) = run {
            self.search_span(buf, rs.saturating_sub(margin).max(lo), hi, &area, out);
        }
    }

    /// Probe every grid position of the owned range for the grid-probed
    /// patterns.
    fn search_grid(&self, buf: &[u8], area: &SearchArea, out: &mut Vec<Hit>) {
        let buf_end = area.buf_base + buf.len() as u64;
        let end = area.own_end.min(buf_end);
        for (step, refs) in &self.direct {
            let step = *step;
            let Some(mut s) = area.own_start.div_ceil(step).checked_mul(step) else {
                continue;
            };
            while s < end {
                let rel = (s - area.buf_base) as usize;
                for d in refs {
                    let p = rel.saturating_add(d.r.offset as usize);
                    if buf.get(p..p.saturating_add(d.pat.len())) == Some(d.pat)
                        && d.r.sig.matches(&buf[rel..])
                    {
                        out.push(Hit {
                            start: s,
                            fmt: d.r.fmt,
                        });
                    }
                }
                let Some(next) = s.checked_add(step) else {
                    break;
                };
                s = next;
            }
        }
    }

    /// Search `buf[lo..hi]` for automaton pattern occurrences fully inside
    /// it.
    fn search_span(&self, buf: &[u8], lo: usize, hi: usize, area: &SearchArea, out: &mut Vec<Hit>) {
        let Some(ac) = &self.ac else {
            return;
        };
        let hay = &buf[..hi];
        let mut pos = lo;
        while pos < hi {
            let Some(m) = ac.find(Input::new(hay).span(pos..hi)) else {
                break;
            };
            let p = m.start();
            self.expand(buf, p, area, out);
            pos = p + 1;
        }
    }

    /// Every signature whose pattern occurs at `buf[p]`.
    fn expand(&self, buf: &[u8], p: usize, area: &SearchArea, out: &mut Vec<Hit>) {
        let tail = &buf[p..];
        let abs_p = area.buf_base + p as u64;
        for &q in &self.by_first[tail[0] as usize] {
            let q = q as usize;
            if !tail.starts_with(self.pats[q]) {
                continue;
            }
            for r in &self.refs[q] {
                let Some(start) = abs_p.checked_sub(r.offset) else {
                    continue;
                };
                if start < area.own_start || start >= area.own_end || start % r.step != 0 {
                    continue;
                }
                let head = &buf[(start - area.buf_base) as usize..];
                if r.sig.matches(head) {
                    out.push(Hit { start, fmt: r.fmt });
                }
            }
        }
    }
}

/// A block of zeros to compare against.
static ZEROS: [u8; ZERO_BLOCK] = [0; ZERO_BLOCK];

/// True when every byte of `b` is zero. Compares against a static zero
/// block (`memcmp`, which stops at the first difference).
pub(crate) fn is_zero(b: &[u8]) -> bool {
    b.chunks(ZERO_BLOCK).all(|c| c == &ZEROS[..c.len()])
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskrescue_core::format::{Measured, Validation};
    use diskrescue_core::media::MediaCategory;
    use diskrescue_core::SharedSource;

    struct Sigs(&'static str, &'static [Signature]);

    impl FileFormat for Sigs {
        fn id(&self) -> &'static str {
            self.0
        }
        fn name(&self) -> &'static str {
            self.0
        }
        fn extensions(&self) -> &'static [&'static str] {
            &["bin"]
        }
        fn category(&self) -> MediaCategory {
            MediaCategory::Other
        }
        fn signatures(&self) -> &'static [Signature] {
            self.1
        }
        fn min_size(&self) -> u64 {
            1
        }
        fn max_size(&self) -> u64 {
            1 << 20
        }
        fn measure(&self, _: &SharedSource, _: u64, _: u64) -> Result<Option<Measured>> {
            Ok(None)
        }
        fn validate(&self, _: &SharedSource) -> Result<Validation> {
            Ok(Validation::invalid("test"))
        }
    }

    static TIFF: &[Signature] = &[Signature::at_start(b"II*\0")];
    static CR2: &[Signature] = &[Signature::at_start(b"II*\0\x10\0\0\0CR")];
    static MP4: &[Signature] = &[Signature::at(b"ftyp", 4)];
    static HEIC: &[Signature] = &[Signature::at(b"ftyp", 4).with_also(b"heic", 8)];
    // The real MPEG-TS / M2TS and bare-MP3 / BMP shapes: one- and two-byte
    // patterns (weak signatures).
    static TS: &[Signature] = &[
        Signature::at_start(b"G").with_also(b"G", 188),
        Signature::at(b"G", 4).with_also(b"G", 196),
    ];
    static MP3: &[Signature] = &[Signature::at_start(b"\xFF\xFB")];
    static BMP: &[Signature] = &[Signature::at_start(b"BM")];
    // A short pattern with a long `also`: not weak, follows the alignment.
    static SHORT_STRONG: &[Signature] = &[Signature::at_start(b"Gy").with_also(b"tp*I", 6)];

    fn formats() -> Vec<Arc<dyn FileFormat>> {
        vec![
            Arc::new(Sigs("tiff", TIFF)),
            Arc::new(Sigs("cr2", CR2)),
            Arc::new(Sigs("mp4", MP4)),
            Arc::new(Sigs("heic", HEIC)),
        ]
    }

    fn set(alignment: u64) -> (Vec<Arc<dyn FileFormat>>, PatternSet) {
        let f = formats();
        let p = PatternSet::build(&f, alignment).unwrap();
        (f, p)
    }

    fn area(len: usize, zero_skip: bool) -> SearchArea {
        SearchArea {
            buf_base: 0,
            own_start: 0,
            own_end: len as u64,
            zero_skip,
        }
    }

    fn hits(p: &PatternSet, buf: &[u8], a: SearchArea) -> Vec<Hit> {
        let mut v = Vec::new();
        p.search(buf, a, &mut v);
        v.sort();
        v.dedup();
        v
    }

    /// Every (start, format) a brute-force search accepts at `alignment`.
    fn naive(f: &[Arc<dyn FileFormat>], buf: &[u8], alignment: u64) -> Vec<Hit> {
        let mut out = Vec::new();
        for start in 0..buf.len() {
            for (fi, fmt) in f.iter().enumerate() {
                if fmt
                    .signatures()
                    .iter()
                    .any(|s| start as u64 % sig_step(s, alignment) == 0 && s.matches(&buf[start..]))
                {
                    out.push(Hit {
                        start: start as u64,
                        fmt: fi,
                    });
                }
            }
        }
        out
    }

    fn small_alphabet(len: usize, alphabet: &[u8], mut x: u64) -> Vec<u8> {
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                alphabet[(x % alphabet.len() as u64) as usize]
            })
            .collect()
    }

    #[test]
    fn overlapping_and_offset_signatures_are_all_found() {
        let (_, p) = set(512);
        let mut buf = vec![0x55u8; 4096];
        buf[512..524].copy_from_slice(b"II*\0\x10\0\0\0CR\x02\0");
        buf[1024..1036].copy_from_slice(b"\0\0\0\x18ftypheic");
        let got = hits(&p, &buf, area(buf.len(), false));
        assert_eq!(
            got,
            vec![
                Hit { start: 512, fmt: 0 },
                Hit { start: 512, fmt: 1 },
                Hit {
                    start: 1024,
                    fmt: 2
                },
                Hit {
                    start: 1024,
                    fmt: 3
                },
            ]
        );
        // Unaligned copies are filtered unless alignment is 1.
        buf[2001..2005].copy_from_slice(b"II*\0");
        assert_eq!(hits(&p, &buf, area(buf.len(), false)).len(), 4);
        let (_, p1) = set(1);
        assert_eq!(hits(&p1, &buf, area(buf.len(), false)).len(), 5);
    }

    #[test]
    fn zero_skip_keeps_patterns_straddling_zero_blocks() {
        let (_, p) = set(4);
        // Block 0 random-ish, block 1 all zero except the size field of a
        // box whose `ftyp` begins block 2.
        let mut buf = vec![0x77u8; 3 * 4096];
        buf[4096..8192].fill(0);
        buf[8192 - 4..8192].copy_from_slice(&[0, 0, 0, 0]);
        buf[8192..8196].copy_from_slice(b"ftyp");
        let with = hits(&p, &buf, area(buf.len(), true));
        let without = hits(&p, &buf, area(buf.len(), false));
        assert_eq!(with, without);
        assert_eq!(
            with,
            vec![Hit {
                start: 8188,
                fmt: 2
            }]
        );
    }

    #[test]
    fn ownership_window_is_respected() {
        let (_, p) = set(1);
        let mut buf = vec![0x11u8; 8192];
        buf[1000..1004].copy_from_slice(b"II*\0");
        buf[5000..5004].copy_from_slice(b"ftyp"); // start 4996
        let a = SearchArea {
            buf_base: 0,
            own_start: 1000,
            own_end: 4996,
            zero_skip: true,
        };
        assert_eq!(
            hits(&p, &buf, a),
            vec![Hit {
                start: 1000,
                fmt: 0
            }]
        );
        let a = SearchArea {
            own_start: 4996,
            own_end: 8192,
            ..a
        };
        assert_eq!(
            hits(&p, &buf, a),
            vec![Hit {
                start: 4996,
                fmt: 2
            }]
        );
    }

    #[test]
    fn matches_naive_search_on_pseudo_random_data() {
        let (f, p) = set(1);
        let mut buf = small_alphabet(
            200_000,
            &[0u8, b'I', b'*', b'f', b't', b'y', b'p', 0x10],
            0x9E37_79B9_7F4A_7C15,
        );
        buf[98_304..110_592].fill(0);
        let want = naive(&f, &buf, 1);
        for zs in [false, true] {
            assert_eq!(hits(&p, &buf, area(buf.len(), zs)), want, "zero_skip={zs}");
        }
    }

    #[test]
    fn weak_short_signatures_sit_on_the_sector_grid_and_match_naive_search() {
        let mut f = formats();
        f.push(Arc::new(Sigs("mts", TS)));
        f.push(Arc::new(Sigs("mp3", MP3)));
        f.push(Arc::new(Sigs("bmp", BMP)));
        f.push(Arc::new(Sigs("short", SHORT_STRONG)));
        // `G` everywhere, so TS candidates occur at many offsets; the rest
        // of the alphabet makes the other signatures occur too.
        let mut buf = small_alphabet(
            300_000,
            &[
                b'G', b'G', b'G', 0xFF, 0xFB, b'B', b'M', b'y', b't', b'p', b'*', b'I', 0,
            ],
            0x1234_5678_9ABC_DEF1,
        );
        buf[40_960..49_152].fill(0);
        for at in [0usize, 4096, 5000, 65_536 - 3, 131_072, 200_001] {
            buf[at..at + 10].copy_from_slice(b"Gy\0\0\0\0tp*I");
        }
        for alignment in [1u64, 2, 16, 512, 4096] {
            let p = PatternSet::build(&f, alignment).unwrap();
            let want = naive(&f, &buf, alignment);
            assert!(
                want.iter()
                    .all(|h| f[h.fmt].id() != "mts" || h.start % 512 == 0),
                "weak signatures stay on the sector grid"
            );
            assert!(want.iter().any(|h| f[h.fmt].id() == "mts"));
            assert!(want.iter().any(|h| f[h.fmt].id() == "short"));
            for zs in [false, true] {
                assert_eq!(
                    hits(&p, &buf, area(buf.len(), zs)),
                    want,
                    "alignment={alignment} zero_skip={zs}"
                );
            }
            // Split ownership the way the scanner does (buffer with a base
            // and an overlap) and check that nothing is lost or doubled.
            let mut got = Vec::new();
            let step = 65_536usize;
            let mut a = 0usize;
            while a < buf.len() {
                let b = (a + step).min(buf.len());
                let base = a.saturating_sub(1000) / 512 * 512;
                let end = (b + 70_000).min(buf.len());
                p.search(
                    &buf[base..end],
                    SearchArea {
                        buf_base: base as u64,
                        own_start: a as u64,
                        own_end: b as u64,
                        zero_skip: true,
                    },
                    &mut got,
                );
                a = b;
            }
            got.sort();
            got.dedup();
            assert_eq!(got, want, "alignment={alignment}, windowed");
        }
        // With a coarse alignment every short pattern is grid-probed and the
        // automaton only holds the long ones.
        let p = PatternSet::build(&f, 512).unwrap();
        assert!(p.pats.iter().all(|x| x.len() > SHORT_PATTERN));
        assert!(!p.direct.is_empty());
        // With alignment 1 the non-weak short pattern needs the automaton.
        let p = PatternSet::build(&f, 1).unwrap();
        assert!(p.pats.contains(&&b"Gy"[..]));
    }

    #[test]
    fn grid_steps() {
        let weak = Signature::at_start(b"BM");
        let strong = Signature::at_start(b"\xFF\xD8\xFF");
        assert_eq!(sig_step(&weak, 1), 512);
        assert_eq!(sig_step(&weak, 0), 512);
        assert_eq!(sig_step(&weak, 4096), 4096);
        assert_eq!(sig_step(&weak, 1000), 64_000);
        assert_eq!(sig_step(&weak, u64::MAX), u64::MAX);
        assert_eq!(sig_step(&strong, 1), 1);
        assert_eq!(sig_step(&strong, 0), 1);
        assert_eq!(sig_step(&strong, 4096), 4096);
        assert_eq!(
            sig_step(&Signature::at_start(b"G").with_also(b"G", 188), 4),
            512
        );
    }

    #[test]
    fn zero_detection() {
        assert!(is_zero(&[0u8; 4096]));
        let mut b = [0u8; 4096];
        b[4095] = 1;
        assert!(!is_zero(&b));
        b[4095] = 0;
        b[100] = 1;
        assert!(!is_zero(&b));
        assert!(is_zero(&[]));
    }

    #[test]
    fn rejects_empty_and_absurd_signatures() {
        static EMPTY: &[Signature] = &[Signature::at_start(b"")];
        static FAR: &[Signature] = &[Signature::at(b"ab", 1 << 40)];
        let f: Vec<Arc<dyn FileFormat>> = vec![Arc::new(Sigs("e", EMPTY))];
        assert!(PatternSet::build(&f, 512).is_err());
        let f: Vec<Arc<dyn FileFormat>> = vec![Arc::new(Sigs("far", FAR))];
        assert!(PatternSet::build(&f, 512).is_err());
        static ZEROS: &[Signature] = &[Signature::at_start(b"\0\0\0\0")];
        let f: Vec<Arc<dyn FileFormat>> = vec![Arc::new(Sigs("z", ZEROS))];
        assert!(!PatternSet::build(&f, 512).unwrap().zero_safe());
        // A table of grid-probed patterns only has no automaton.
        let f: Vec<Arc<dyn FileFormat>> = vec![Arc::new(Sigs("bmp", BMP))];
        let p = PatternSet::build(&f, 512).unwrap();
        assert!(p.ac.is_none());
        let mut buf = vec![0u8; 2048];
        buf[1024..1026].copy_from_slice(b"BM");
        buf[1500..1502].copy_from_slice(b"BM");
        assert_eq!(
            hits(&p, &buf, area(buf.len(), true)),
            vec![Hit {
                start: 1024,
                fmt: 0
            }]
        );
    }
}
