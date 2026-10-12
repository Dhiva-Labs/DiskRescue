//! Shared helpers for the integration tests.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use diskrescue_core::profile::ImagingProfile;
use diskrescue_core::{Range, RangeSet, SharedSource};
use diskrescue_imaging::simulate::{FaultySource, SimOutcome};
use diskrescue_imaging::{
    BlockStatus, Control, Event, Imager, ImagingOptions, MapFile, Phase, Progress, Summary,
};

pub const KIB: u64 = 1024;
pub const MIB: u64 = 1024 * KIB;

/// Deterministic pseudo-random content (no all-zero sectors in practice).
pub fn pattern(size: u64, seed: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(size as usize);
    let mut x = seed ^ 0x9E37_79B9_7F4A_7C15;
    while (out.len() as u64) < size {
        x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = x;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        out.extend_from_slice(&z.to_le_bytes());
    }
    out.truncate(size as usize);
    out
}

/// `data` with the bytes in `holes` zeroed: what a fresh image must hold.
pub fn expected(data: &[u8], holes: &RangeSet) -> Vec<u8> {
    let mut v = data.to_vec();
    for r in holes.iter() {
        v[r.start as usize..r.end as usize].fill(0);
    }
    v
}

/// The generic profile with test-friendly waits.
pub fn test_profile() -> ImagingProfile {
    let mut p = ImagingProfile::generic();
    p.timeout_copy = Duration::from_secs(10);
    p.timeout_scrape = Duration::from_secs(10);
    p.timeout_retry = Duration::from_secs(10);
    p.wait_for_reattach = Some(Duration::from_secs(10));
    p
}

pub fn test_opts(profile: ImagingProfile) -> ImagingOptions {
    ImagingOptions {
        profile,
        reattach_poll: Duration::from_millis(10),
        ..ImagingOptions::default()
    }
}

/// Everything a test wants to look at after a run.
pub struct Run {
    pub summary: Summary,
    /// All events except progress.
    pub events: Vec<Event>,
    /// Read-log length when each phase started.
    pub phase_marks: Vec<(Phase, usize)>,
    pub last_progress: Option<Progress>,
    pub progress_count: usize,
    pub output: Vec<u8>,
    /// The mapfile as saved on disk.
    pub map: MapFile,
    pub out_path: PathBuf,
    pub map_path: PathBuf,
}

impl Run {
    pub fn logs(&self) -> Vec<String> {
        self.events
            .iter()
            .filter_map(|e| match e {
                Event::Log(_, m) => Some(m.clone()),
                _ => None,
            })
            .collect()
    }

    pub fn has_phase(&self, f: impl Fn(&Phase) -> bool) -> bool {
        self.phase_marks.iter().any(|(p, _)| f(p))
    }

    pub fn status_ranges(&self, st: BlockStatus) -> Vec<Range> {
        self.map.map.ranges_with(&[st]).iter().collect()
    }
}

pub fn paths(dir: &Path) -> (PathBuf, PathBuf) {
    (dir.join("disk.img"), dir.join("disk.map"))
}

/// Run an imager over `src` with output and mapfile in `dir`. `hook` sees
/// every event (including progress) and the control block.
pub fn image_in(
    dir: &Path,
    src: &Arc<FaultySource>,
    opts: ImagingOptions,
    mut hook: impl FnMut(&Event, &Arc<Control>),
) -> Run {
    let (out_path, map_path) = paths(dir);
    let shared: SharedSource = src.clone();
    let mut imager = Imager::new(shared, &out_path, &map_path, opts).expect("imager");
    let control = Control::new();
    let mut events = Vec::new();
    let mut phase_marks = Vec::new();
    let mut last_progress = None;
    let mut progress_count = 0usize;
    let ctl = control.clone();
    let summary = imager
        .run(control.clone(), &mut |ev: Event| {
            hook(&ev, &ctl);
            match ev {
                Event::Progress(p) => {
                    progress_count += 1;
                    last_progress = Some(p);
                }
                Event::Phase(ph) => {
                    phase_marks.push((ph, src.read_log_len()));
                    events.push(Event::Phase(ph));
                }
                other => events.push(other),
            }
        })
        .expect("run");
    let map = MapFile::load(&map_path).expect("mapfile");
    assert_eq!(
        &map.map,
        &imager.mapfile().map,
        "saved map == in-memory map"
    );
    map.map.validate().unwrap();
    let output = std::fs::read(&out_path).unwrap();
    Run {
        summary,
        events,
        phase_marks,
        last_progress,
        progress_count,
        output,
        map,
        out_path,
        map_path,
    }
}

pub fn image(src: &Arc<FaultySource>, opts: ImagingOptions) -> (Run, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let run = image_in(dir.path(), src, opts, |_, _| {});
    (run, dir)
}

/// Every read the engine issued was sector aligned and inside the source.
pub fn assert_reads_sane(src: &FaultySource) {
    let c = src.counters();
    assert_eq!(c.unaligned, 0, "unaligned reads: {c:?}");
    assert_eq!(c.out_of_range, 0, "reads past the end: {c:?}");
    let sector = diskrescue_core::Source::sector_size(src) as u64;
    for r in src.read_log() {
        assert_eq!(r.offset % sector, 0, "{r:?}");
        assert!(r.len > 0, "{r:?}");
    }
}

pub fn failed_reads(src: &FaultySource) -> Vec<(u64, usize)> {
    src.read_log()
        .iter()
        .filter(|r| matches!(r.outcome, SimOutcome::Error { .. }))
        .map(|r| (r.offset, r.len))
        .collect()
}

/// Compare two images without dumping megabytes on failure.
#[track_caller]
pub fn assert_bytes_eq(got: &[u8], want: &[u8]) {
    assert_eq!(got.len(), want.len(), "length differs");
    if let Some(i) = got.iter().zip(want).position(|(a, b)| a != b) {
        let n = got[i..]
            .iter()
            .zip(&want[i..])
            .take_while(|(a, b)| a != b)
            .count();
        panic!(
            "images differ at byte {i} (0x{i:X}) for {n} bytes: got {:02x?}.. want {:02x?}..",
            &got[i..(i + 8).min(got.len())],
            &want[i..(i + 8).min(want.len())]
        );
    }
}

pub fn ranges(v: &[(u64, u64)]) -> RangeSet {
    v.iter().map(|&(s, e)| Range::new(s, e)).collect()
}
