//! Every named imaging profile on a simulated device that mixes hard bad
//! areas, flaky sectors, hangs (timeouts), slow areas, repeated device
//! loss and (for the profiles that check for it) stale data after errors;
//! with 512- and 4096-byte sectors, the whole device and an unaligned
//! domain subset; and interrupted at the start of every phase and resumed.
//!
//! Invariants checked on every run: the image never holds a wrong byte,
//! finished bytes are the source's bytes, nothing outside the domain is
//! read into the output or changes status, every read is sector aligned
//! and inside the source, hard faults never end up finished, the output
//! never changes length, and the mapfile on disk never claims data the
//! output does not hold.

mod common;

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use diskrescue_core::profile::ImagingProfile;
use diskrescue_core::source::{ReadError, Source};
use diskrescue_core::{Range, RangeSet, SharedSource};
use diskrescue_imaging::simulate::FaultySource;
use diskrescue_imaging::{
    BlockStatus, Control, CurrentStatus, Event, Imager, ImagingOptions, MapFile, Phase, Summary,
};

const PROFILES: [&str; 8] = [
    "hdd",
    "ssd",
    "nvme",
    "usb-flash",
    "sd",
    "usb-hdd",
    "usb-ssd",
    "generic",
];

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// The named profile with its waits shrunk for tests (the strategy is
/// unchanged: same block sizes, skips, phases, checks and limits).
fn scaled(name: &str) -> ImagingProfile {
    let mut p = ImagingProfile::by_name(name).unwrap();
    p.timeout_copy = ms(30);
    p.timeout_scrape = ms(150);
    p.timeout_retry = ms(200);
    if !p.pause_on_error.is_zero() {
        p.pause_on_error = ms(1);
    }
    p.pause_duration = ms(1);
    if p.wait_for_reattach.is_some() {
        p.wait_for_reattach = Some(Duration::from_secs(5));
    }
    p
}

/// Forwards to a FaultySource and cancels `control` once `cancel_at`
/// reads went through this wrapper.
struct CancelAfter {
    inner: Arc<FaultySource>,
    reads: AtomicU64,
    cancel_at: u64,
    control: Mutex<Option<Arc<Control>>>,
}

impl CancelAfter {
    fn tick(&self) {
        let n = self.reads.fetch_add(1, Ordering::SeqCst) + 1;
        if n == self.cancel_at {
            if let Some(c) = self.control.lock().unwrap().as_ref() {
                c.cancel();
            }
        }
    }
}

impl Source for CancelAfter {
    fn size(&self) -> u64 {
        self.inner.size()
    }
    fn sector_size(&self) -> u32 {
        self.inner.sector_size()
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, ReadError> {
        self.tick();
        self.inner.read_at(offset, buf)
    }
    fn read_at_timeout(
        &self,
        offset: u64,
        buf: &mut [u8],
        timeout: Duration,
    ) -> Result<usize, ReadError> {
        self.tick();
        self.inner.read_at_timeout(offset, buf, timeout)
    }
    fn describe(&self) -> String {
        self.inner.describe()
    }
    fn is_device(&self) -> bool {
        true
    }
    fn requires_alignment(&self) -> bool {
        self.inner.requires_alignment()
    }
    fn reopen(&self) -> Result<(), ReadError> {
        self.inner.reopen()
    }
}

struct Outcome {
    summary: Summary,
    watchdog_fired: bool,
    phases: Vec<(Phase, u64)>,
    lost: usize,
    back: usize,
}

/// One run of a fresh Imager over `src` (cancelled after `cancel_at`
/// reads, 0 = never), guarded by a watchdog.
fn run_once(
    src: &Arc<FaultySource>,
    dir: &std::path::Path,
    opts: &ImagingOptions,
    cancel_at: u64,
    limit: Duration,
) -> Outcome {
    let (out, map) = paths(dir);
    let control = Control::new();
    let wrapper = Arc::new(CancelAfter {
        inner: src.clone(),
        reads: AtomicU64::new(0),
        cancel_at,
        control: Mutex::new(Some(control.clone())),
    });
    let shared: SharedSource = wrapper.clone();
    let mut im = Imager::new(shared, &out, &map, opts.clone()).expect("imager");
    let fired = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let (c2, f2, d2) = (control.clone(), fired.clone(), done.clone());
    let wd = std::thread::spawn(move || {
        let t0 = Instant::now();
        while !d2.load(Ordering::SeqCst) {
            if t0.elapsed() >= limit {
                f2.store(true, Ordering::SeqCst);
                c2.cancel();
                return;
            }
            std::thread::sleep(ms(5));
        }
    });
    let mut phases = Vec::new();
    let (mut lost, mut back) = (0, 0);
    let w2 = wrapper.clone();
    let summary = im
        .run(control, &mut |ev| match ev {
            Event::Phase(p) => phases.push((p, w2.reads.load(Ordering::SeqCst))),
            Event::DeviceLost { .. } => lost += 1,
            Event::DeviceBack { .. } => back += 1,
            _ => {}
        })
        .expect("run");
    done.store(true, Ordering::SeqCst);
    wd.join().unwrap();
    Outcome {
        summary,
        watchdog_fired: fired.load(Ordering::SeqCst),
        phases,
        lost,
        back,
    }
}

fn round_to(set: &RangeSet, sector: u64, size: u64) -> RangeSet {
    set.iter()
        .map(|r| {
            Range::new(
                r.start / sector * sector,
                (r.end.div_ceil(sector) * sector).min(size),
            )
        })
        .collect()
}

/// The invariants that must hold after any run, finished or not.
fn check_state(
    ctx: &str,
    dir: &std::path::Path,
    data: &[u8],
    domain: Range,
    hard: &RangeSet,
    src: &FaultySource,
) -> MapFile {
    let (out, map) = paths(dir);
    let m = MapFile::load(&map).unwrap_or_else(|e| panic!("{ctx}: {e}"));
    m.map.validate().unwrap();
    let size = data.len() as u64;
    assert_eq!(m.map.size(), size, "{ctx}");
    let output = std::fs::read(&out).unwrap();
    assert_eq!(output.len() as u64, size, "{ctx}: output length changed");
    for b in m.map.iter() {
        if b.status == BlockStatus::Finished {
            let (a, z) = (b.pos as usize, b.end() as usize);
            if output[a..z] != data[a..z] {
                let i = (a..z).find(|&i| output[i] != data[i]).unwrap();
                panic!("{ctx}: finished {b:?} not in the output (first wrong byte {i})");
            }
        }
    }
    for (i, (&o, &d)) in output.iter().zip(data).enumerate() {
        assert!(o == d || o == 0, "{ctx}: wrong byte at {i}");
    }
    let outside = RangeSet::from_range(domain).complement_within(Range::new(0, size));
    for r in outside.iter() {
        assert!(
            output[r.start as usize..r.end as usize]
                .iter()
                .all(|&x| x == 0),
            "{ctx}: output written outside the domain in {r:?}"
        );
        assert_eq!(m.map.stats_in(r).non_tried, r.len(), "{ctx}: {r:?}");
    }
    let fin = m.map.ranges_with(&[BlockStatus::Finished]);
    assert!(
        fin.intersection(hard).is_empty(),
        "{ctx}: a hard fault is finished: {:?}",
        fin.intersection(hard).iter().collect::<Vec<_>>()
    );
    assert_reads_sane(src);
    m
}

struct Scenario {
    data: Vec<u8>,
    src: Arc<FaultySource>,
    hard: RangeSet,
    flaky: RangeSet,
}

/// The fault layout. `small` halves the device and shrinks the damage so
/// the many interrupted runs of the resume test stay quick; the layout is
/// otherwise the same (a single bad sector, a bad area with a good island
/// and a zero-filled stretch behind it, an unaligned bad area, flaky
/// sectors, a hang and a slow area).
fn scenario(
    name: &str,
    sector: u64,
    seed: u64,
    stale: bool,
    vanish: bool,
    small: bool,
) -> Scenario {
    let k = if small { 1 } else { 2 };
    let size = k * MIB;
    let at = |kib: u64| kib * KIB * k / 2;
    let mut data = pattern(size, seed);
    let mut hard = RangeSet::new();
    hard.insert(Range::with_len(at(300), 512));
    if small {
        hard.insert(Range::new(at(700), at(700) + 8 * KIB));
        hard.insert(Range::new(at(700) + 12 * KIB, at(700) + 20 * KIB));
    } else {
        hard.insert(Range::new(at(700), at(800)));
        hard.insert(Range::new(at(808), at(1000)));
    }
    // A zero-filled stretch (free space) right after a bad area: what a
    // bridge's stale zeros look like.
    let z = hard.iter().nth(2).unwrap().end as usize;
    data[z..z + 64 * KIB as usize].fill(0);
    hard.insert(Range::new(at(1500) + 100, at(1500) + 5000));
    let flaky = RangeSet::from_range(Range::with_len(at(1700), 8 * KIB));
    let mut b = FaultySource::builder(data.clone())
        .sector_size(sector as u32)
        .name(format!("{name} sim"));
    for r in hard.iter() {
        b = b.bad(r);
    }
    for r in flaky.iter() {
        b = b.flaky(r, 0.4, seed);
    }
    let (hang_len, hang_ms) = if small { (512, 40) } else { (2 * KIB, 60) };
    b = b
        .hang(Range::with_len(at(1200), hang_len), ms(hang_ms))
        .slow(
            Range::with_len(at(1300), 128 * KIB),
            ms(if small { 5 } else { 100 }),
        );
    if vanish {
        b = b.vanish(9, 2).vanish(40, 1);
    }
    if stale {
        b = b.stale_after_error(true);
    }
    Scenario {
        data,
        src: Arc::new(b.build()),
        hard: round_to(&hard, sector, size),
        flaky: round_to(&flaky, sector, size),
    }
}

/// Every profile, both sector sizes, whole device and an unaligned domain.
#[test]
fn every_profile_survives_a_mixed_fault_device() {
    let size = 2 * MIB;
    for (pi, name) in PROFILES.iter().enumerate() {
        for sector in [512u64, 4096] {
            for (di, domain) in [
                Range::new(0, size),
                Range::new(100 * KIB + 700, 1900 * KIB + 300),
            ]
            .into_iter()
            .enumerate()
            {
                let p = scaled(name);
                let seed = (pi * 10 + di) as u64 + sector;
                let sc = scenario(name, sector, seed, p.check_on_error, true, false);
                let mut o = test_opts(p.clone());
                o.domain = Some(domain);
                let dir = tempfile::tempdir().unwrap();
                let ctx = format!("{name} sector {sector} domain {domain:?}");
                let r = run_once(&sc.src, dir.path(), &o, 0, Duration::from_secs(60));
                assert!(!r.watchdog_fired, "{ctx}: hung: {:?}", r.summary);
                assert!(!r.summary.cancelled, "{ctx}");
                assert_eq!(r.lost, r.back, "{ctx}: device lost but not back");
                let m = check_state(&ctx, dir.path(), &sc.data, domain, &sc.hard, &sc.src);
                // Every phase ran (all profiles scrape): only hard faults
                // and flaky sectors may be left unfinished.
                assert!(r.summary.finished, "{ctx}: {:?}", r.summary);
                let dom = RangeSet::from_range(domain);
                let unfinished = m.map.unfinished_ranges().intersection(&dom);
                let allowed = sc.hard.union(&sc.flaky);
                let extra = allowed
                    .complement_within(Range::new(0, size))
                    .intersection(&unfinished);
                assert!(
                    extra.is_empty(),
                    "{ctx}: unexpected unfinished {:?}; warnings {:?}",
                    extra.iter().collect::<Vec<_>>(),
                    r.summary.warnings
                );
                assert!(
                    m.map.bad_ranges().covers_all(&sc.hard.intersection(&dom)),
                    "{ctx}: hard faults not all bad"
                );
                assert_eq!(m.current_status, CurrentStatus::Finished, "{ctx}");
            }
        }
    }
}

trait CoversAll {
    fn covers_all(&self, other: &RangeSet) -> bool;
}

impl CoversAll for RangeSet {
    fn covers_all(&self, other: &RangeSet) -> bool {
        other.iter().all(|r| self.covers(r))
    }
}

/// Interrupt every profile right after the first, second and a middle read
/// of every phase of an uninterrupted reference run; resume with new
/// Imagers (interrupting a few more times) until done. After every
/// interruption the saved map and the output agree, and the end result is
/// the reference result.
///
/// By default each profile runs with one sector size (alternating) and is
/// cut right after the first read of every phase plus once in the middle
/// of its longest phase; `IMAGING_FULL_SWEEP=1` runs both sector sizes and
/// three cuts per phase (several minutes).
#[test]
fn every_profile_resumes_after_cancel_in_every_phase() {
    let full = std::env::var("IMAGING_FULL_SWEEP").is_ok_and(|v| v == "1");
    for (pi, name) in PROFILES.iter().enumerate() {
        let sectors: &[u64] = if full {
            &[512, 4096]
        } else if pi % 2 == 0 {
            &[512]
        } else {
            &[4096]
        };
        for &sector in sectors {
            let t_start = Instant::now();
            let mut p = scaled(name);
            // Slow-skipping depends on timing; keep the phase layout
            // deterministic so the cancel points land where intended.
            p.min_read_rate = None;
            // Pauses only cost time here; the mixed-device test keeps them.
            p.pause_on_error = Duration::ZERO;
            p.pause_duration = Duration::ZERO;
            let seed = 100 + pi as u64 + sector;
            let o = {
                let mut o = test_opts(p.clone());
                o.progress_interval = Duration::ZERO;
                o
            };
            // Reference run.
            let refsc = scenario(name, sector, seed, p.check_on_error, true, true);
            let refdir = tempfile::tempdir().unwrap();
            let size = refsc.data.len() as u64;
            let whole = Range::new(0, size);
            let r = run_once(&refsc.src, refdir.path(), &o, 0, Duration::from_secs(60));
            assert!(r.summary.finished && !r.watchdog_fired, "{name}: reference");
            let reference = check_state(
                &format!("{name} reference"),
                refdir.path(),
                &refsc.data,
                whole,
                &refsc.hard,
                &refsc.src,
            );
            let total = refsc.src.read_log_len() as u64;
            let mut cuts = BTreeSet::new();
            let starts: Vec<u64> = r.phases.iter().map(|(_, n)| *n).collect();
            for (i, s) in starts.iter().enumerate() {
                let end = starts.get(i + 1).copied().unwrap_or(total);
                let mid = (s + end) / 2;
                let picks: &[u64] = if full { &[s + 1, s + 2, mid] } else { &[s + 1] };
                for &c in picks {
                    if c >= 1 && c < total {
                        cuts.insert(c);
                    }
                }
            }
            if !full {
                // One cut in the middle of the longest phase as well.
                if let Some((s, e)) = starts
                    .iter()
                    .zip(starts.iter().skip(1).chain(std::iter::once(&total)))
                    .max_by_key(|(s, e)| **e - **s)
                {
                    cuts.insert((s + e) / 2);
                }
            }
            let mut seen_status = BTreeSet::new();
            for cut in cuts {
                let sc = scenario(name, sector, seed, p.check_on_error, true, true);
                let dir = tempfile::tempdir().unwrap();
                let ctx = format!("{name} sector {sector} cut {cut}/{total}");
                let mut next = cut;
                let mut rounds = 0;
                loop {
                    rounds += 1;
                    assert!(rounds < 40, "{ctx}: no progress");
                    let r = run_once(&sc.src, dir.path(), &o, next, Duration::from_secs(60));
                    assert!(!r.watchdog_fired, "{ctx}: hung");
                    let m = check_state(&ctx, dir.path(), &sc.data, whole, &sc.hard, &sc.src);
                    if r.summary.finished {
                        // Flaky sectors may come out either way; the rest
                        // must match the uninterrupted run exactly.
                        let solid = sc.flaky.complement_within(whole);
                        assert_eq!(
                            m.map.bad_ranges().intersection(&solid),
                            reference.map.bad_ranges().intersection(&solid),
                            "{ctx}: result differs from the uninterrupted run"
                        );
                        assert!(m
                            .map
                            .bad_ranges()
                            .intersection(&sc.flaky.complement_within(whole))
                            .iter()
                            .all(|r| sc.hard.covers(r)));
                        assert_eq!(m.map.stats().pending(), 0, "{ctx}");
                        break;
                    }
                    assert!(r.summary.cancelled, "{ctx}: {:?}", r.summary);
                    seen_status.insert(format!("{:?}", m.current_status));
                    // Interrupt again a few reads later, then let it finish.
                    next = if rounds < 3 { 3 + rounds as u64 * 5 } else { 0 };
                }
            }
            let p2 = &p;
            let mut want = vec!["Copying"];
            if p2.trim {
                want.push("Trimming");
            }
            if p2.divide || p2.scrape {
                want.push("Scraping");
            }
            if p2.retry_passes > 0 {
                want.push("Retrying");
            }
            for w in want {
                assert!(
                    seen_status.contains(w),
                    "{name} sector {sector}: never interrupted while {w}: {seen_status:?}"
                );
            }
            eprintln!(
                "{name} sector {sector}: {} reads, {:?}",
                total,
                t_start.elapsed()
            );
        }
    }
}
