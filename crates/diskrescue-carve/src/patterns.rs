//! The signature table and the multi-pattern search over a chunk buffer.
//!
//! Every [`Signature`] of every registered format contributes its primary
//! `bytes`. Identical byte strings are merged, so the automaton holds each
//! distinct pattern once and every pattern remembers all `(format,
//! signature, offset)` triples that use it. One Aho-Corasick automaton
//! (leftmost-first, DFA, prefilter on) finds the leftmost position where any
//! pattern matches; the search then resumes one byte later, so every
//! position with at least one match is visited. At such a position all
//! patterns that start with the same byte are compared directly, which
//! recovers overlapping and same-position matches that leftmost-first
//! semantics would otherwise hide (a TIFF magic that is a prefix of a CR2
//! magic, for example).
//!
//! A hit at absolute position `p` of a pattern used with offset `o` names
//! the candidate file start `p - o`. Candidates are kept only when that start
//! lies in the caller's "owned" range, is a multiple of the alignment and the
//! complete signature (including its `also` pattern) matches the bytes at
//! the start.
//!
//! Zero skip: a 4 KiB block that is entirely zero cannot contain any byte of
//! a pattern that has a non-zero byte, but a pattern may straddle the block
//! edge (an ISO BMFF size `00 00 00 18` before `ftyp`). Non-zero runs of
//! blocks are therefore searched with a margin of `longest pattern - 1` bytes
//! into the neighbouring zero blocks. Zero skip is disabled when a pattern
//! is all zero bytes or longer than half a block.

use std::sync::Arc;

use aho_corasick::{AhoCorasick, AhoCorasickKind, Input, MatchKind};
use diskrescue_core::format::{FileFormat, Signature};
use diskrescue_core::{Error, Result};

use crate::types::ZERO_BLOCK;

/// Signatures whose probe length exceeds this are refused.
const MAX_PROBE_LEN: u64 = 16 << 20;

/// One use of a pattern by a format signature.
#[derive(Debug, Clone)]
struct SigRef {
    fmt: usize,
    sig: Signature,
    offset: u64,
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
    /// Candidate starts must be multiples of this (>= 1).
    pub alignment: u64,
    /// Skip all-zero 4 KiB blocks.
    pub zero_skip: bool,
}

/// The compiled signature table.
pub(crate) struct PatternSet {
    ac: AhoCorasick,
    pats: Vec<&'static [u8]>,
    refs: Vec<Vec<SigRef>>,
    by_first: Vec<Vec<u32>>,
    max_len: usize,
    max_tail: u64,
    max_probe: u64,
    zero_safe: bool,
}

impl PatternSet {
    /// Compile the signatures of `formats` (index = format index).
    pub(crate) fn build(formats: &[Arc<dyn FileFormat>]) -> Result<Self> {
        let mut pats: Vec<&'static [u8]> = Vec::new();
        let mut refs: Vec<Vec<SigRef>> = Vec::new();
        let mut max_tail = 0u64;
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
                max_tail = max_tail.max(s.offset + s.bytes.len() as u64);
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
                });
            }
        }
        if pats.is_empty() {
            return Err(Error::InvalidArgument(
                "no formats registered for carving".into(),
            ));
        }
        let mut by_first = vec![Vec::new(); 256];
        for (i, p) in pats.iter().enumerate() {
            by_first[p[0] as usize].push(i as u32);
        }
        let max_len = pats.iter().map(|p| p.len()).max().unwrap_or(1);
        let zero_safe = pats.iter().all(|p| p.iter().any(|&b| b != 0)) && max_len <= ZERO_BLOCK / 2;
        let ac = AhoCorasick::builder()
            .match_kind(MatchKind::LeftmostFirst)
            .kind(Some(AhoCorasickKind::DFA))
            .prefilter(true)
            .build(&pats)
            .or_else(|e| {
                log::debug!("DFA build failed ({e}); falling back to automatic automaton");
                AhoCorasick::builder()
                    .match_kind(MatchKind::LeftmostFirst)
                    .build(&pats)
            })
            .map_err(|e| Error::other(format!("cannot build signature automaton: {e}")))?;
        log::debug!(
            "signature table: {} formats, {} distinct patterns, longest {} bytes, probe length {}, zero skip {}",
            formats.len(),
            pats.len(),
            max_len,
            max_probe,
            if zero_safe { "possible" } else { "disabled" }
        );
        Ok(PatternSet {
            ac,
            pats,
            refs,
            by_first,
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

    /// Search `buf[lo..hi]` for pattern occurrences fully inside it.
    fn search_span(&self, buf: &[u8], lo: usize, hi: usize, area: &SearchArea, out: &mut Vec<Hit>) {
        let hay = &buf[..hi];
        let mut pos = lo;
        while pos < hi {
            let Some(m) = self.ac.find(Input::new(hay).span(pos..hi)) else {
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
                if start < area.own_start || start >= area.own_end || start % area.alignment != 0 {
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

    fn set() -> (Vec<Arc<dyn FileFormat>>, PatternSet) {
        let f: Vec<Arc<dyn FileFormat>> = vec![
            Arc::new(Sigs("tiff", TIFF)),
            Arc::new(Sigs("cr2", CR2)),
            Arc::new(Sigs("mp4", MP4)),
            Arc::new(Sigs("heic", HEIC)),
        ];
        let p = PatternSet::build(&f).unwrap();
        (f, p)
    }

    fn area(len: usize, alignment: u64, zero_skip: bool) -> SearchArea {
        SearchArea {
            buf_base: 0,
            own_start: 0,
            own_end: len as u64,
            alignment,
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

    #[test]
    fn overlapping_and_offset_signatures_are_all_found() {
        let (_, p) = set();
        let mut buf = vec![0x55u8; 4096];
        buf[512..524].copy_from_slice(b"II*\0\x10\0\0\0CR\x02\0");
        buf[1024..1036].copy_from_slice(b"\0\0\0\x18ftypheic");
        let got = hits(&p, &buf, area(buf.len(), 512, false));
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
        assert_eq!(hits(&p, &buf, area(buf.len(), 512, false)).len(), 4);
        assert_eq!(hits(&p, &buf, area(buf.len(), 1, false)).len(), 5);
    }

    #[test]
    fn zero_skip_keeps_patterns_straddling_zero_blocks() {
        let (_, p) = set();
        // Block 0 random-ish, block 1 all zero except the size field of a
        // box whose `ftyp` begins block 2.
        let mut buf = vec![0x77u8; 3 * 4096];
        buf[4096..8192].fill(0);
        buf[8192 - 4..8192].copy_from_slice(&[0, 0, 0, 0]);
        buf[8192..8196].copy_from_slice(b"ftyp");
        let with = hits(&p, &buf, area(buf.len(), 4, true));
        let without = hits(&p, &buf, area(buf.len(), 4, false));
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
        let (_, p) = set();
        let mut buf = vec![0x11u8; 8192];
        buf[1000..1004].copy_from_slice(b"II*\0");
        buf[5000..5004].copy_from_slice(b"ftyp"); // start 4996
        let a = SearchArea {
            buf_base: 0,
            own_start: 1000,
            own_end: 4996,
            alignment: 1,
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
        let (f, p) = set();
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let mut buf: Vec<u8> = (0..200_000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                // Small alphabet so patterns occur by chance.
                [0u8, b'I', b'*', b'f', b't', b'y', b'p', 0x10][(x % 8) as usize]
            })
            .collect();
        buf[98_304..110_592].fill(0);
        let mut naive = Vec::new();
        for start in 0..buf.len() {
            for (fi, fmt) in f.iter().enumerate() {
                if fmt.signatures().iter().any(|s| s.matches(&buf[start..])) {
                    naive.push(Hit {
                        start: start as u64,
                        fmt: fi,
                    });
                }
            }
        }
        for zs in [false, true] {
            assert_eq!(
                hits(&p, &buf, area(buf.len(), 1, zs)),
                naive,
                "zero_skip={zs}"
            );
        }
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
        assert!(PatternSet::build(&f).is_err());
        let f: Vec<Arc<dyn FileFormat>> = vec![Arc::new(Sigs("far", FAR))];
        assert!(PatternSet::build(&f).is_err());
        static ZEROS: &[Signature] = &[Signature::at_start(b"\0\0\0\0")];
        let f: Vec<Arc<dyn FileFormat>> = vec![Arc::new(Sigs("z", ZEROS))];
        assert!(!PatternSet::build(&f).unwrap().zero_safe());
    }
}
