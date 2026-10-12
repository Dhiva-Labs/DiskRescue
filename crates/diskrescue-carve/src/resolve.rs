//! The overlap and nesting resolver.
//!
//! Measured candidates arrive strictly in ascending start order (the scanner
//! numbers them and a reorder buffer releases them in sequence). Because
//! every byte may belong to at most one top-level file, at most one file is
//! "open" at a time: the last accepted one. It stays open until a candidate
//! starting at or after its end arrives (or the scan ends), because a later
//! candidate inside it may still cut it short. Files that are closed are
//! final and handed back to the caller.
//!
//! Rules for a candidate `c` that starts inside the open file `p` (spec
//! section 3.3, adapted):
//!
//! * same start: the higher priority wins, then the higher confidence (R1;
//!   normally impossible because all formats matching one offset are
//!   measured in a single job that already applies this rule);
//! * inside a range `p` reported as nested: suppressed (the embedded object
//!   belongs to `p`; promotion is decided when `p` is emitted);
//! * `p` is `Complete`, `c` is on the grid, confident (>= 80 after
//!   environment penalties) and runs past the end of `p` (P2): an embedded
//!   object never extends beyond its container, so one of the two lengths is
//!   wrong. `p` is cut at `c` and loses [`PENALTY_SPLIT`] points: every byte
//!   still lands in an output file, whereas dropping `c` would lose all of
//!   it beyond `p`'s end;
//! * `p` is `Complete`, `c` is on the grid, confident and lies inside `p`,
//!   and `p` cannot contain `c`'s format (R2c, a strengthened R2'): `p`'s
//!   length field claims the bytes of an unrelated, self-consistent file.
//!   That is what an overwritten or truncated file whose size field
//!   survived looks like (a deleted video whose clusters were reused for a
//!   new photo). `c` is recorded as a *conflict* child of `p`, always
//!   emitted as a file of its own with `parent` set, and `p` keeps its
//!   extent (nothing is lost if `p` was in fact intact) but gets a note and
//!   loses [`PENALTY_CONFLICT`](crate::PENALTY_CONFLICT) points when emitted;
//! * `p` is *strong* (status `Complete`, confidence >= 80): any other `c` is
//!   suppressed. With `promote_nested`, a `c` that `p` may contain and that
//!   lies fully inside is recorded as nested;
//! * same run: `p` has a weak length and `c` has the same format and ends
//!   exactly where `p` ends. Run-based formats (transport streams, program
//!   streams, bare audio frames) repeat their "header" every packet, so
//!   such a `c` is a suffix of `p`, not a new file: suppressed, and `p`
//!   enters *run mode*, in which every further same-format candidate inside
//!   it is suppressed without looking at its extent;
//! * `p` has a weak length (`Truncated`, `Capped`, `UnknownLength`) and `c`
//!   is on the allocation grid: `p` ends where `c` starts (R3);
//! * `c` is on the grid and more than 20 points more confident than `p`:
//!   `p` is split at `c` and loses [`PENALTY_SPLIT`] points (R4);
//! * `c` lies fully inside `p`: recorded as nested when `p` may contain its
//!   format (R2), as a conflict child when it is confident, on the grid and
//!   `p` is `Complete` (R2c), otherwise ignored with a note on `p` (R2');
//! * `c` overlaps the end of `p`: the more confident of the two keeps the
//!   bytes; when `c` wins, `p` is cut at `c` (it keeps its valid prefix).
//!
//! The scanner may drop candidates before measuring them when they start
//! inside a strong open file off the grid (or inside one of its nested
//! ranges), or have the open file's format while it is in run mode, as long
//! as no earlier, still unmeasured candidate inside the open file could
//! change it. The rules above make that shortcut produce exactly the same
//! output as measuring them, so results do not depend on thread timing.
//! The run-mode shortcut is what keeps a multi-gigabyte stream from being
//! measured once per aligned packet (quadratic work). Candidates on the grid
//! inside a strong file are always measured, because P2 and R2c may apply.

use diskrescue_core::format::{Confidence, ExtentStatus, Measured, Nested, NestedRole};
use diskrescue_core::media::{MediaCategory, MediaMeta};
use diskrescue_core::{Range, RangeSet};

use crate::scan::Fmt;
use crate::types::PENALTY_SPLIT;

/// Confidence at which a complete file can no longer be split by R4, and
/// at which a candidate on the grid is trusted against an enclosing file's
/// length (P2, R2c).
pub(crate) const STRONG: u8 = 80;

/// Upper bound on nested entries a single file keeps.
pub(crate) const MAX_NESTED: usize = 65_536;

/// Upper bound on notes per file (further ones are summarised).
const MAX_NOTES: usize = 32;

/// A nested object that was measured with its own format (promotion
/// candidate).
#[derive(Debug, Clone)]
pub(crate) struct Child {
    pub start: u64,
    pub fmt: usize,
    pub role: NestedRole,
    pub m: Measured,
    /// Found by R2c: a confident file of a format the parent cannot hold.
    /// Always emitted.
    pub conflict: bool,
}

/// A measured candidate ready for resolution.
#[derive(Debug, Clone)]
pub(crate) struct Candidate {
    pub start: u64,
    pub fmt: usize,
    pub m: Measured,
    pub aligned: bool,
    /// Environment penalty for the measured extent (unreadable bytes,
    /// allocated space, off-grid header), from deterministic inputs only.
    pub env_penalty: u8,
    pub children: Vec<Child>,
    pub children_measured: bool,
}

/// A file held by the resolver (open, or closed and handed out).
#[derive(Debug, Clone)]
pub(crate) struct Open {
    pub start: u64,
    pub len: u64,
    pub fmt: usize,
    pub status: ExtentStatus,
    pub conf_raw: Confidence,
    pub res_penalty: u8,
    pub env_penalty: u8,
    pub err_offset: Option<u64>,
    pub meta: MediaMeta,
    pub nested: Vec<Nested>,
    pub nested_set: RangeSet,
    pub notes: Vec<String>,
    pub aligned: bool,
    pub children: Vec<Child>,
    pub children_measured: bool,
    /// A same-format suffix was seen: further same-format candidates inside
    /// are part of this run.
    pub run_mode: bool,
    /// Conflict children recorded by R2c.
    pub conflicts: u32,
    dropped_notes: usize,
}

impl Open {
    fn from_candidate(c: Candidate) -> Self {
        let mut nested = c.m.nested;
        nested.truncate(MAX_NESTED);
        let mut o = Open {
            start: c.start,
            len: c.m.len,
            fmt: c.fmt,
            status: c.m.status,
            conf_raw: c.m.confidence,
            res_penalty: 0,
            env_penalty: c.env_penalty,
            err_offset: c.m.err_offset,
            meta: c.m.meta,
            nested,
            nested_set: RangeSet::new(),
            notes: c.m.notes,
            aligned: c.aligned,
            children: c.children,
            children_measured: c.children_measured,
            run_mode: false,
            conflicts: 0,
            dropped_notes: 0,
        };
        o.rebuild_nested_set();
        o
    }

    /// One past the last byte.
    pub(crate) fn end(&self) -> u64 {
        self.start.saturating_add(self.len)
    }

    /// Confidence after resolver and environment penalties.
    pub(crate) fn effective(&self) -> Confidence {
        self.conf_raw
            .lowered(self.res_penalty.saturating_add(self.env_penalty))
    }

    /// Complete and confident enough that nothing can cut it.
    pub(crate) fn strong(&self) -> bool {
        self.status == ExtentStatus::Complete && self.effective().value() >= STRONG
    }

    pub(crate) fn weak_length(&self) -> bool {
        matches!(
            self.status,
            ExtentStatus::Truncated | ExtentStatus::Capped | ExtentStatus::UnknownLength
        )
    }

    fn rebuild_nested_set(&mut self) {
        let start = self.start;
        let len = self.len;
        self.nested_set = self
            .nested
            .iter()
            .filter(|n| n.range.start < len)
            .map(|n| Range::new(start + n.range.start, start + n.range.end.min(len)))
            .collect();
    }

    fn note(&mut self, s: String) {
        if self.notes.len() < MAX_NOTES {
            self.notes.push(s);
        } else {
            self.dropped_notes += 1;
        }
    }

    /// Notes with a summary of the ones that did not fit.
    pub(crate) fn take_notes(&mut self) -> Vec<String> {
        let mut v = std::mem::take(&mut self.notes);
        if self.dropped_notes > 0 {
            v.push(format!("{} further notes omitted", self.dropped_notes));
        }
        v
    }

    /// Cut the file at absolute offset `cut` (status becomes `Truncated`).
    fn truncate(&mut self, cut: u64, note: String, env: &dyn Fn(u64, u64) -> u8) {
        if cut <= self.start || cut >= self.end() {
            return;
        }
        let new_len = cut - self.start;
        self.len = new_len;
        self.status = ExtentStatus::Truncated;
        self.err_offset = Some(self.err_offset.map_or(new_len, |e| e.min(new_len)));
        self.nested.retain(|n| n.range.end <= new_len);
        self.children
            .retain(|c| c.start.saturating_add(c.m.len) <= cut);
        self.rebuild_nested_set();
        self.note(note);
        self.env_penalty = env(self.start, self.len);
    }

    fn add_child(&mut self, c: Candidate, fmts: &[Fmt]) {
        if self.nested.len() >= MAX_NESTED {
            self.note(format!(
                "more than {MAX_NESTED} embedded objects; later ones not recorded"
            ));
            return;
        }
        let role = guess_role(fmts[self.fmt].category, fmts[c.fmt].category);
        let rel = c.start - self.start;
        self.nested.push(Nested {
            range: Range::with_len(rel, c.m.len),
            format: Some(fmts[c.fmt].id.to_string()),
            role,
        });
        let end = c.start.saturating_add(c.m.len).min(self.end());
        self.nested_set.insert(Range::new(c.start, end));
        self.children.push(Child {
            start: c.start,
            fmt: c.fmt,
            role,
            m: c.m,
            conflict: false,
        });
    }

    /// R2c: record a confident file of a format this one cannot contain,
    /// found inside it. It is emitted as a file of its own; this file keeps
    /// its extent.
    fn add_conflict(&mut self, c: Candidate, fmts: &[Fmt]) {
        let cid = fmts[c.fmt].id;
        let rel = c.start - self.start;
        if self.nested.len() >= MAX_NESTED {
            // Cannot happen before 65536 conflicts; keep the bytes with this
            // file rather than grow without bound.
            self.note(format!(
                "more than {MAX_NESTED} embedded objects; a {cid} file at +{rel} was not recorded"
            ));
            return;
        }
        self.conflicts += 1;
        self.note(format!(
            "a {cid} file (confidence {}) starts at +{rel}, inside this file's extent, and a {} file cannot contain it: this file is probably damaged or overwritten from there on; the {cid} file is reported separately",
            c.m.confidence.value(),
            fmts[self.fmt].id
        ));
        self.nested.push(Nested {
            range: Range::with_len(rel, c.m.len),
            format: Some(cid.to_string()),
            role: NestedRole::Container,
        });
        let end = c.start.saturating_add(c.m.len).min(self.end());
        self.nested_set.insert(Range::new(c.start, end));
        self.children.push(Child {
            start: c.start,
            fmt: c.fmt,
            role: NestedRole::Container,
            m: c.m,
            conflict: true,
        });
    }
}

/// Role of an object of category `inner` found inside one of `outer`.
pub(crate) fn guess_role(outer: MediaCategory, inner: MediaCategory) -> NestedRole {
    use MediaCategory::*;
    match (outer, inner) {
        (Photo, Photo) => NestedRole::Thumbnail,
        (RawPhoto, Photo) | (RawPhoto, RawPhoto) => NestedRole::Preview,
        (Video | Audio, Photo) => NestedRole::CoverArt,
        (Photo, Video) => NestedRole::Trailer,
        _ => NestedRole::Container,
    }
}

/// Settings the resolver needs.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ResolverCfg {
    pub min_confidence: Confidence,
    pub promote_nested: bool,
}

/// What the scanner knows about unresolved (dispatched, not yet resolved)
/// candidates when it asks [`Resolver::prefilter_drops`].
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct PendingState {
    /// One starts inside the open file, outside its nested ranges: it could
    /// still cut the open file or add to it.
    pub inside: bool,
    /// One has a format other than the open file's (it could end a run).
    pub other_format: bool,
}

/// Sweep-line resolver over candidates in start order.
#[derive(Debug, Default)]
pub(crate) struct Resolver {
    open: Option<Open>,
    /// Candidates swallowed by an enclosing file.
    pub suppressed: u64,
}

impl Resolver {
    /// The open file, if any.
    pub(crate) fn open(&self) -> Option<&Open> {
        self.open.as_ref()
    }

    /// Whether the scanner may drop a candidate at `start` (of any of
    /// `fmts`; `aligned` when `start` is on the allocation grid) without
    /// measuring it, because the resolver would suppress it without changing
    /// any state.
    ///
    /// The shortcuts are only exact when no unresolved earlier candidate
    /// can still change the open file (see [`PendingState`]).
    pub(crate) fn prefilter_drops(
        &self,
        start: u64,
        aligned: bool,
        fmts: &[usize],
        all: &[Fmt],
        cfg: &ResolverCfg,
        pending: PendingState,
    ) -> bool {
        let Some(p) = &self.open else {
            return false;
        };
        if start <= p.start || start >= p.end() {
            return false;
        }
        if p.strong() {
            if pending.inside {
                return false;
            }
            if p.nested_set.contains(start) {
                return true;
            }
            if aligned {
                // P2 or R2c may apply: only a measurement can tell.
                return false;
            }
            return !(cfg.promote_nested
                && fmts.iter().any(|&f| all[p.fmt].f.may_contain(all[f].id)));
        }
        p.run_mode && p.weak_length() && !pending.other_format && fmts == [p.fmt]
    }

    /// Resolve one candidate. Files that become final are appended to `out`.
    pub(crate) fn push(
        &mut self,
        c: Candidate,
        fmts: &[Fmt],
        cfg: &ResolverCfg,
        env: &dyn Fn(u64, u64) -> u8,
        out: &mut Vec<Open>,
    ) {
        let Some(p) = self.open.as_mut() else {
            self.open = Some(Open::from_candidate(c));
            return;
        };
        if c.start >= p.end() {
            out.extend(self.open.take());
            self.open = Some(Open::from_candidate(c));
            return;
        }
        let c_eff = c.m.confidence.lowered(c.env_penalty);
        let c_end = c.start.saturating_add(c.m.len);
        let cid = fmts[c.fmt].id;
        if c.start <= p.start {
            // R1: same start (or an out-of-order arrival, which the scanner
            // never produces): priority, then confidence.
            let (pp, cp) = (fmts[p.fmt].priority, fmts[c.fmt].priority);
            if cp > pp || (cp == pp && c_eff > p.effective()) {
                self.suppressed += 1;
                self.open = Some(Open::from_candidate(c));
            } else {
                self.suppressed += 1;
            }
            return;
        }
        if p.nested_set.contains(c.start) {
            self.suppressed += 1;
            return;
        }
        let inside = c_end <= p.end();
        let may = fmts[p.fmt].f.may_contain(cid);
        let rel = c.start - p.start;
        // A confident header on the grid is evidence in its own right: its
        // bytes are not handed to an enclosing file on that file's length
        // field alone.
        let c_trusted = c.aligned && c_eff.value() >= STRONG;
        let p_complete = p.status == ExtentStatus::Complete;
        if p_complete && c_trusted && !inside {
            // P2: an embedded object never runs past its container.
            p.truncate(
                c.start,
                format!(
                    "cut at +{rel}: a {cid} file (confidence {}) starts there and runs past this file's end",
                    c_eff.value()
                ),
                env,
            );
            p.res_penalty = p.res_penalty.saturating_add(PENALTY_SPLIT);
            self.close_cut(out);
            self.open = Some(Open::from_candidate(c));
            return;
        }
        if p.strong() {
            if c_trusted && !may {
                // R2c.
                p.add_conflict(c, fmts);
                return;
            }
            if cfg.promote_nested && may && inside {
                p.add_child(c, fmts);
            }
            self.suppressed += 1;
            return;
        }
        if p.weak_length() && c.fmt == p.fmt && (p.run_mode || c_end == p.end()) {
            // Same run: a suffix of `p` measured as its own file.
            p.run_mode = true;
            self.suppressed += 1;
            return;
        }
        if p.weak_length() && c.aligned {
            // R3: a new header on the grid ends a weak-length file.
            p.truncate(
                c.start,
                format!("ended at the next header ({cid} at offset {})", c.start),
                env,
            );
            out.extend(self.open.take());
            self.open = Some(Open::from_candidate(c));
            return;
        }
        if c.aligned && c_eff.value() > p.effective().value().saturating_add(20) {
            // R4: a much stronger header on the grid splits the file.
            p.truncate(
                c.start,
                format!("cut at +{rel}: a more confident {cid} file starts there"),
                env,
            );
            p.res_penalty = p.res_penalty.saturating_add(PENALTY_SPLIT);
            self.close_cut(out);
            self.open = Some(Open::from_candidate(c));
            return;
        }
        if inside {
            if may {
                p.add_child(c, fmts);
            } else if c_trusted && p_complete {
                // R2c for a complete file R4 could not split.
                p.add_conflict(c, fmts);
                return;
            } else {
                p.note(format!("ignored a conflicting {cid} header at +{rel}"));
            }
            self.suppressed += 1;
            return;
        }
        if c_eff > p.effective() {
            p.truncate(
                c.start,
                format!("cut at +{rel}: overlaps a more confident {cid} file"),
                env,
            );
            p.res_penalty = p.res_penalty.saturating_add(PENALTY_SPLIT);
            self.close_cut(out);
            self.open = Some(Open::from_candidate(c));
        } else {
            p.note(format!(
                "ignored an overlapping, less confident {cid} candidate at +{rel}"
            ));
            self.suppressed += 1;
        }
    }

    /// Close the open file after a cut. It is kept whatever its confidence
    /// after the split penalty: its prefix is data the format accepted, and
    /// dropping it would leave those bytes in no output at all.
    fn close_cut(&mut self, out: &mut Vec<Open>) {
        out.extend(self.open.take());
    }

    /// Close the open file (end of scan).
    pub(crate) fn finish(&mut self, out: &mut Vec<Open>) {
        out.extend(self.open.take());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskrescue_core::format::{FileFormat, Signature, Validation};
    use diskrescue_core::{Result, SharedSource};
    use std::sync::Arc;

    struct F {
        id: &'static str,
        contains: &'static [&'static str],
    }

    impl FileFormat for F {
        fn id(&self) -> &'static str {
            self.id
        }
        fn name(&self) -> &'static str {
            self.id
        }
        fn extensions(&self) -> &'static [&'static str] {
            &["x"]
        }
        fn category(&self) -> MediaCategory {
            MediaCategory::Photo
        }
        fn signatures(&self) -> &'static [Signature] {
            &[]
        }
        fn min_size(&self) -> u64 {
            1
        }
        fn max_size(&self) -> u64 {
            u64::MAX
        }
        fn may_contain(&self, inner: &str) -> bool {
            self.contains.contains(&inner)
        }
        fn measure(&self, _: &SharedSource, _: u64, _: u64) -> Result<Option<Measured>> {
            Ok(None)
        }
        fn validate(&self, _: &SharedSource) -> Result<Validation> {
            Ok(Validation::invalid("x"))
        }
    }

    fn fmts() -> Vec<Fmt> {
        let mk = |id, contains, priority| Fmt {
            f: Arc::new(F { id, contains }),
            id,
            ext: "x",
            category: MediaCategory::Photo,
            priority,
            min_size: 1,
            max_size: u64::MAX,
        };
        vec![
            mk("box", &["pic"], 70),
            mk("pic", &["pic"], 50),
            mk("raw", &[], 20),
        ]
    }

    fn cand(start: u64, fmt: usize, len: u64, status: ExtentStatus, conf: u8) -> Candidate {
        let mut m = Measured::complete(len, Confidence(conf));
        m.status = status;
        Candidate {
            start,
            fmt,
            m,
            aligned: start % 512 == 0,
            env_penalty: 0,
            children: Vec::new(),
            children_measured: false,
        }
    }

    const NONE: PendingState = PendingState {
        inside: false,
        other_format: false,
    };

    const CFG: ResolverCfg = ResolverCfg {
        min_confidence: Confidence::LOW,
        promote_nested: false,
    };

    fn run(cands: Vec<Candidate>) -> (Vec<Open>, Resolver) {
        let f = fmts();
        let mut r = Resolver::default();
        let mut out = Vec::new();
        for c in cands {
            r.push(c, &f, &CFG, &|_, _| 0, &mut out);
        }
        r.finish(&mut out);
        (out, r)
    }

    fn spans(v: &[Open]) -> Vec<(u64, u64, ExtentStatus)> {
        v.iter().map(|o| (o.start, o.len, o.status)).collect()
    }

    #[test]
    fn disjoint_files_pass_through() {
        let (out, _) = run(vec![
            cand(0, 1, 1000, ExtentStatus::Complete, 90),
            cand(1024, 1, 100, ExtentStatus::Complete, 90),
        ]);
        assert_eq!(
            spans(&out),
            vec![
                (0, 1000, ExtentStatus::Complete),
                (1024, 100, ExtentStatus::Complete)
            ]
        );
    }

    #[test]
    fn r3_weak_parent_ends_at_aligned_header() {
        let (out, _) = run(vec![
            cand(0, 2, 1 << 20, ExtentStatus::UnknownLength, 40),
            cand(4096, 1, 1000, ExtentStatus::Complete, 90),
        ]);
        assert_eq!(
            spans(&out),
            vec![
                (0, 4096, ExtentStatus::Truncated),
                (4096, 1000, ExtentStatus::Complete)
            ]
        );
        assert_eq!(out[0].err_offset, Some(4096));
    }

    #[test]
    fn r3_ignores_unaligned_headers() {
        let (out, r) = run(vec![
            cand(0, 2, 1 << 20, ExtentStatus::Capped, 40),
            cand(4097, 1, 1000, ExtentStatus::Complete, 50),
        ]);
        assert_eq!(spans(&out), vec![(0, 1 << 20, ExtentStatus::Capped)]);
        assert_eq!(r.suppressed, 1);
        assert!(out[0].notes.iter().any(|n| n.contains("conflicting pic")));
    }

    #[test]
    fn r2_nested_in_strong_parent_is_suppressed() {
        let (out, r) = run(vec![
            cand(0, 0, 100_000, ExtentStatus::Complete, 90),
            cand(4096, 1, 1000, ExtentStatus::Complete, 90),
        ]);
        assert_eq!(spans(&out), vec![(0, 100_000, ExtentStatus::Complete)]);
        assert_eq!(r.suppressed, 1);
        assert!(out[0].nested.is_empty(), "strong parents stay unchanged");
    }

    #[test]
    fn r2_nested_in_medium_parent_is_recorded() {
        let (out, _) = run(vec![
            cand(0, 0, 100_000, ExtentStatus::Complete, 60),
            cand(4096, 1, 1000, ExtentStatus::Complete, 70),
        ]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].nested.len(), 1);
        assert_eq!(out[0].nested[0].range, Range::new(4096, 5096));
        assert_eq!(out[0].children.len(), 1);
    }

    #[test]
    fn r4_much_stronger_header_splits_complete_parent() {
        let (out, _) = run(vec![
            cand(0, 2, 100_000, ExtentStatus::Complete, 60),
            cand(8192, 1, 1000, ExtentStatus::Complete, 95),
        ]);
        assert_eq!(
            spans(&out),
            vec![
                (0, 8192, ExtentStatus::Truncated),
                (8192, 1000, ExtentStatus::Complete)
            ]
        );
        assert_eq!(out[0].effective(), Confidence(60 - PENALTY_SPLIT));
    }

    #[test]
    fn r4_split_below_minimum_keeps_the_parent_prefix() {
        // The split penalty takes the parent below the minimum, but its
        // prefix is data the format accepted: it stays, with the low score.
        let (out, _) = run(vec![
            cand(0, 2, 100_000, ExtentStatus::Complete, 45),
            cand(8192, 1, 1000, ExtentStatus::Complete, 95),
        ]);
        assert_eq!(
            spans(&out),
            vec![
                (0, 8192, ExtentStatus::Truncated),
                (8192, 1000, ExtentStatus::Complete)
            ]
        );
        assert_eq!(out[0].effective(), Confidence(45 - PENALTY_SPLIT));
    }

    #[test]
    fn partial_overlap_higher_confidence_wins() {
        // Unaligned so neither R3 nor R4 applies.
        let mut c = cand(5000, 1, 10_000, ExtentStatus::Complete, 70);
        c.aligned = false;
        let (out, _) = run(vec![cand(0, 0, 8000, ExtentStatus::Complete, 60), c]);
        assert_eq!(
            spans(&out),
            vec![
                (0, 5000, ExtentStatus::Truncated),
                (5000, 10_000, ExtentStatus::Complete)
            ]
        );
        let mut c = cand(5000, 1, 10_000, ExtentStatus::Complete, 55);
        c.aligned = false;
        let (out, r) = run(vec![cand(0, 0, 8000, ExtentStatus::Complete, 60), c]);
        assert_eq!(spans(&out), vec![(0, 8000, ExtentStatus::Complete)]);
        assert_eq!(r.suppressed, 1);
    }

    #[test]
    fn reported_nested_ranges_suppress_candidates() {
        let mut p = cand(0, 2, 1 << 20, ExtentStatus::Truncated, 40);
        p.m.nested.push(Nested {
            range: Range::new(4096, 9000),
            format: Some("pic".into()),
            role: NestedRole::Thumbnail,
        });
        // Aligned, inside a weak parent, but inside its nested range: no R3.
        let (out, r) = run(vec![p, cand(4096, 1, 4904, ExtentStatus::Complete, 90)]);
        assert_eq!(spans(&out), vec![(0, 1 << 20, ExtentStatus::Truncated)]);
        assert_eq!(r.suppressed, 1);
    }

    #[test]
    fn same_start_prefers_priority() {
        let (out, _) = run(vec![
            cand(0, 1, 100, ExtentStatus::Complete, 90),
            cand(0, 0, 200, ExtentStatus::Complete, 60),
        ]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].fmt, 0);
    }

    #[test]
    fn prefilter_agrees_with_resolution() {
        let f = fmts();
        let mut r = Resolver::default();
        let mut out = Vec::new();
        r.push(
            cand(0, 0, 100_000, ExtentStatus::Complete, 90),
            &f,
            &CFG,
            &|_, _| 0,
            &mut out,
        );
        // Off the grid inside a strong file: dropped.
        assert!(r.prefilter_drops(4097, false, &[1], &f, &CFG, NONE));
        // On the grid: P2 or R2c may apply, so it must be measured.
        assert!(!r.prefilter_drops(4096, true, &[1], &f, &CFG, NONE));
        assert!(!r.prefilter_drops(4096, true, &[2], &f, &CFG, NONE));
        // An earlier unmeasured candidate inside the file may still cut it.
        let inside = PendingState {
            inside: true,
            other_format: false,
        };
        assert!(!r.prefilter_drops(4097, false, &[1], &f, &CFG, inside));
        assert!(!r.prefilter_drops(100_000, false, &[1], &f, &CFG, NONE));
        let promote = ResolverCfg {
            promote_nested: true,
            ..CFG
        };
        assert!(!r.prefilter_drops(4097, false, &[1], &f, &promote, NONE));
        assert!(r.prefilter_drops(4097, false, &[2], &f, &promote, NONE));
    }

    #[test]
    fn p2_confident_file_running_past_a_complete_file_cuts_it() {
        // `raw` cannot be held by `box`; `pic` could, but an embedded object
        // never runs past its container either.
        for (fmt, len) in [(2usize, 100_000u64), (1, 50_000)] {
            let (out, r) = run(vec![
                cand(0, 0, 100_000, ExtentStatus::Complete, 90),
                cand(65_536, fmt, len, ExtentStatus::Complete, 90),
            ]);
            assert_eq!(
                spans(&out),
                vec![
                    (0, 65_536, ExtentStatus::Truncated),
                    (65_536, len, ExtentStatus::Complete)
                ],
                "format {fmt}"
            );
            assert_eq!(out[0].effective(), Confidence(90 - PENALTY_SPLIT));
            assert_eq!(r.suppressed, 0);
        }
        // Off the grid, or not confident: the strong file keeps its bytes.
        let mut c = cand(65_537, 2, 100_000, ExtentStatus::Complete, 90);
        c.aligned = false;
        let (out, _) = run(vec![cand(0, 0, 100_000, ExtentStatus::Complete, 90), c]);
        assert_eq!(spans(&out), vec![(0, 100_000, ExtentStatus::Complete)]);
        let (out, _) = run(vec![
            cand(0, 0, 100_000, ExtentStatus::Complete, 90),
            cand(65_536, 2, 100_000, ExtentStatus::Complete, 79),
        ]);
        assert_eq!(spans(&out), vec![(0, 100_000, ExtentStatus::Complete)]);
    }

    #[test]
    fn r2c_confident_foreign_file_inside_a_complete_file_is_a_conflict_child() {
        let (out, r) = run(vec![
            // `pic` may only contain `pic`.
            cand(0, 1, 100_000, ExtentStatus::Complete, 90),
            cand(8192, 2, 4096, ExtentStatus::Complete, 85),
            // Inside the conflict child: belongs to it.
            cand(8704, 0, 1000, ExtentStatus::Complete, 95),
            // Weak, or off the grid: no evidence against the parent.
            cand(16_384, 2, 4096, ExtentStatus::Truncated, 40),
            {
                let mut c = cand(20_000, 2, 4096, ExtentStatus::Complete, 95);
                c.aligned = false;
                c
            },
            // A second conflict.
            cand(32_768, 2, 1000, ExtentStatus::UnknownLength, 90),
        ]);
        assert_eq!(spans(&out), vec![(0, 100_000, ExtentStatus::Complete)]);
        let p = &out[0];
        assert_eq!(p.conflicts, 2);
        let kids: Vec<(u64, u64, bool)> = p
            .children
            .iter()
            .map(|c| (c.start, c.m.len, c.conflict))
            .collect();
        assert_eq!(kids, vec![(8192, 4096, true), (32_768, 1000, true)]);
        assert!(p.nested.iter().any(|n| n.range == Range::new(8192, 12_288)
            && n.role == NestedRole::Container
            && n.format.as_deref() == Some("raw")));
        assert!(p.notes.iter().any(|n| n.contains("probably damaged")));
        assert_eq!(r.suppressed, 3);

        // A complete parent R4 cannot split (not 20 points weaker).
        let (out, _) = run(vec![
            cand(0, 2, 100_000, ExtentStatus::Complete, 70),
            cand(8192, 1, 4096, ExtentStatus::Complete, 85),
        ]);
        assert_eq!(spans(&out), vec![(0, 100_000, ExtentStatus::Complete)]);
        assert_eq!(out[0].conflicts, 1);
        assert!(out[0].children[0].conflict);

        // A weak parent is cut by R3 instead.
        let (out, _) = run(vec![
            cand(0, 1, 100_000, ExtentStatus::Capped, 50),
            cand(8192, 2, 4096, ExtentStatus::Complete, 85),
        ]);
        assert_eq!(
            spans(&out),
            vec![
                (0, 8192, ExtentStatus::Truncated),
                (8192, 4096, ExtentStatus::Complete)
            ]
        );
        assert_eq!(out[0].conflicts, 0);
    }

    #[test]
    fn conflict_children_survive_later_cuts_of_the_parent() {
        let (out, _) = run(vec![
            cand(0, 1, 100_000, ExtentStatus::Complete, 90),
            cand(8192, 2, 4096, ExtentStatus::Complete, 85),
            // P2 cuts the parent after the conflict child.
            cand(65_536, 2, 100_000, ExtentStatus::Complete, 90),
        ]);
        assert_eq!(
            spans(&out),
            vec![
                (0, 65_536, ExtentStatus::Truncated),
                (65_536, 100_000, ExtentStatus::Complete)
            ]
        );
        assert_eq!(out[0].children.len(), 1);
        assert!(out[0].children[0].conflict);
    }

    #[test]
    fn same_format_suffix_of_a_run_is_not_a_new_file() {
        let f = fmts();
        let mut r = Resolver::default();
        let mut out = Vec::new();
        let env = |_: u64, _: u64| 0u8;
        r.push(
            cand(0, 2, 20_480, ExtentStatus::UnknownLength, 40),
            &f,
            &CFG,
            &env,
            &mut out,
        );
        assert!(
            !r.prefilter_drops(512, true, &[2], &f, &CFG, NONE),
            "not yet known as a run"
        );
        // A packet of the same stream: same end.
        r.push(
            cand(512, 2, 19_968, ExtentStatus::UnknownLength, 40),
            &f,
            &CFG,
            &env,
            &mut out,
        );
        assert!(r.open().unwrap().run_mode);
        assert!(r.prefilter_drops(1024, true, &[2], &f, &CFG, NONE));
        assert!(
            !r.prefilter_drops(
                1024,
                true,
                &[2],
                &f,
                &CFG,
                PendingState {
                    inside: false,
                    other_format: true
                }
            ),
            "another format may cut it"
        );
        assert!(!r.prefilter_drops(1024, true, &[1, 2], &f, &CFG, NONE));
        // In run mode the extent of further packets does not matter.
        r.push(
            cand(1024, 2, 100, ExtentStatus::UnknownLength, 40),
            &f,
            &CFG,
            &env,
            &mut out,
        );
        // A different format on the grid still ends the run (R3).
        r.push(
            cand(4096, 1, 1000, ExtentStatus::Complete, 90),
            &f,
            &CFG,
            &env,
            &mut out,
        );
        r.finish(&mut out);
        assert_eq!(
            spans(&out),
            vec![
                (0, 4096, ExtentStatus::Truncated),
                (4096, 1000, ExtentStatus::Complete)
            ]
        );
        assert_eq!(r.suppressed, 2);
    }

    #[test]
    fn same_format_with_its_own_end_cuts_a_weak_file() {
        // A broken file of a single-header format that claims everything
        // to its cap, followed by a new file of the same format.
        let (out, _) = run(vec![
            cand(0, 1, 1 << 20, ExtentStatus::Capped, 40),
            cand(65_536, 1, 5000, ExtentStatus::Complete, 90),
        ]);
        assert_eq!(
            spans(&out),
            vec![
                (0, 65_536, ExtentStatus::Truncated),
                (65_536, 5000, ExtentStatus::Complete)
            ]
        );
    }

    #[test]
    fn truncation_drops_nested_beyond_cut() {
        let mut p = cand(0, 2, 1 << 20, ExtentStatus::UnknownLength, 40);
        p.m.nested.push(Nested {
            range: Range::new(100_000, 110_000),
            format: Some("pic".into()),
            role: NestedRole::Thumbnail,
        });
        let (out, _) = run(vec![p, cand(65_536, 1, 1000, ExtentStatus::Complete, 90)]);
        assert!(out[0].nested.is_empty());
        assert_eq!(out[0].len, 65_536);
    }
}
