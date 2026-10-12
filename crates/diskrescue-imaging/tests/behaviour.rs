//! Profile knobs, limits, refusals, resume edge cases and a randomized
//! fault fuzz of the engine.

mod common;

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use common::*;
use diskrescue_core::source::{FileSource, MemSource, ReadError, ReadErrorKind, Source};
use diskrescue_core::{Error, Range, RangeSet, SharedSource};
use diskrescue_imaging::simulate::FaultySource;
use diskrescue_imaging::{
    bad_ranges_of, open_rescued_image, BlockStatus, Control, Event, Imager, ImagingOptions,
    MapFile, Phase,
};
use rand::{rngs::StdRng, Rng, SeedableRng};

fn new_imager(
    src: SharedSource,
    dir: &Path,
    opts: ImagingOptions,
) -> diskrescue_core::Result<Imager> {
    let (out, map) = paths(dir);
    Imager::new(src, &out, &map, opts)
}

#[test]
fn slow_area_is_skipped_in_pass_one_and_read_later() {
    let size = 8 * MIB;
    let slow = Range::new(2 * MIB, 3 * MIB);
    let data = pattern(size, 20);
    let src = Arc::new(
        FaultySource::builder(data.clone())
            .slow(slow, Duration::from_millis(20))
            .build(),
    );
    let mut p = test_profile();
    p.min_read_rate = Some(10 * MIB);
    p.slow_reads_before_skip = 2;
    let (run, _dir) = image(&src, test_opts(p));
    assert_bytes_eq(&run.output, &data);
    assert!(run.summary.finished);
    assert!(run.summary.slow_reads >= 2, "{:?}", run.summary);
    let log = src.read_log();
    let p2 = run
        .phase_marks
        .iter()
        .find(|(ph, _)| matches!(ph, Phase::Copying { pass: 2, .. }))
        .unwrap()
        .1;
    let pass1_bytes: u64 = log[..p2]
        .iter()
        .filter_map(|r| r.range().intersect(slow))
        .map(|r| r.len())
        .sum();
    assert!(
        pass1_bytes < slow.len(),
        "pass 1 must skip part of the slow area ({pass1_bytes} of {})",
        slow.len()
    );
    let later: u64 = log[p2..]
        .iter()
        .filter(|r| r.is_ok())
        .filter_map(|r| r.range().intersect(slow))
        .map(|r| r.len())
        .sum();
    assert!(later > 0);
    let last = run.last_progress.unwrap();
    assert_eq!(last.phase, Phase::Finished);
    assert!(last.average_rate > 0.0);
    assert_eq!(last.eta, Some(Duration::ZERO));
    assert_reads_sane(&src);
}

#[test]
fn max_slow_reads_stops_with_a_warning() {
    let size = 4 * MIB;
    let data = pattern(size, 21);
    let src = Arc::new(
        FaultySource::builder(data.clone())
            .slow(Range::new(MIB, 2 * MIB), Duration::from_millis(15))
            .build(),
    );
    let mut p = test_profile();
    p.min_read_rate = Some(10 * MIB);
    p.max_slow_reads = 3;
    let (run, _dir) = image(&src, test_opts(p));
    assert!(!run.summary.finished && !run.summary.cancelled);
    assert_eq!(run.summary.slow_reads, 3);
    assert!(run
        .summary
        .warnings
        .iter()
        .any(|w| w.contains("slow reads")));
    assert!(run.map.map.stats().non_tried > 0);
    for r in run.map.map.ranges_with(&[BlockStatus::Finished]).iter() {
        assert_bytes_eq(
            &run.output[r.start as usize..r.end as usize],
            &data[r.start as usize..r.end as usize],
        );
    }
}

#[test]
fn max_errors_stops_with_a_warning() {
    let size = 8 * MIB;
    let data = pattern(size, 22);
    let src = Arc::new(
        FaultySource::builder(data)
            .bad(Range::new(MIB, 3 * MIB))
            .build(),
    );
    let mut p = test_profile();
    p.max_errors = 3;
    let (run, _dir) = image(&src, test_opts(p));
    assert!(!run.summary.finished);
    assert_eq!(run.summary.errors, 3);
    assert!(run
        .summary
        .warnings
        .iter()
        .any(|w| w.contains("max_errors")));
    assert_eq!(
        run.map.current_status,
        diskrescue_imaging::CurrentStatus::Copying
    );
}

#[test]
fn pauses_and_reopen_after_errors() {
    let size = 2 * MIB;
    let data = pattern(size, 23);
    let src = Arc::new(
        FaultySource::builder(data.clone())
            .bad(Range::new(MIB, MIB + 2048))
            .build(),
    );
    let mut p = test_profile();
    p.pause_on_error = Duration::from_millis(2);
    p.pause_after_errors = Some(2);
    p.pause_duration = Duration::from_millis(3);
    p.reopen_after_errors = Some(2);
    let (run, _dir) = image(&src, test_opts(p));
    let pauses: Vec<Duration> = run
        .events
        .iter()
        .filter_map(|e| match e {
            Event::Paused(d) => Some(*d),
            _ => None,
        })
        .collect();
    let errors = run.summary.errors as usize;
    assert!(errors >= 3);
    assert_eq!(
        pauses
            .iter()
            .filter(|d| **d == Duration::from_millis(2))
            .count(),
        errors
    );
    assert!(pauses.contains(&Duration::from_millis(3)));
    assert!(src.counters().reopen_calls >= 1);
    assert_eq!(run.map.map.bad_ranges(), ranges(&[(MIB, MIB + 2048)]));
    assert_bytes_eq(&run.output, &expected(&data, &ranges(&[(MIB, MIB + 2048)])));
}

#[test]
fn device_that_does_not_come_back_leaves_nothing_marked_bad() {
    let size = 2 * MIB;
    let data = pattern(size, 24);
    let src = Arc::new(
        FaultySource::builder(data.clone())
            .vanish(5, 1_000_000)
            .build(),
    );
    let mut p = test_profile();
    p.wait_for_reattach = Some(Duration::from_millis(150));
    let mut opts = test_opts(p);
    opts.reattach_poll = Duration::from_millis(20);
    let (run, _dir) = image(&src, opts);
    assert!(!run.summary.finished && !run.summary.cancelled);
    assert!(run
        .summary
        .warnings
        .iter()
        .any(|w| w.contains("did not come back")));
    let st = run.map.map.stats();
    assert_eq!((st.non_trimmed, st.non_scraped, st.bad_sectors), (0, 0, 0));
    assert_eq!(st.rescued, 5 * 64 * KIB);
    assert!(src.counters().reopen_calls >= 3);
}

#[test]
fn max_reattaches_gives_up_after_repeated_losses() {
    let size = 2 * MIB;
    let data = pattern(size, 25);
    let src = Arc::new(
        FaultySource::builder(data.clone())
            .vanish(3, 1)
            .vanish(8, 1)
            .build(),
    );
    let mut p = test_profile();
    p.max_reattaches = 1;
    let (run, _dir) = image(&src, test_opts(p));
    assert_eq!(run.summary.reattaches, 1);
    assert!(!run.summary.finished);
    assert!(run.summary.warnings.iter().any(|w| w.contains("giving up")));
    assert_eq!(run.map.map.stats().errsize, 0);
    // Unlimited re-attaches: completes.
    let src = Arc::new(
        FaultySource::builder(data.clone())
            .vanish(3, 1)
            .vanish(8, 2)
            .build(),
    );
    let (run, _dir) = image(&src, test_opts(test_profile()));
    assert_eq!(run.summary.reattaches, 2);
    assert!(run.summary.finished);
    assert_bytes_eq(&run.output, &data);
}

/// A source that drops off after a number of reads and cannot be
/// re-opened (`reopen` keeps the trait default: unsupported). With
/// `eof_instead`, it returns 0 bytes like the fd of a removed disk.
struct DroppingSource {
    inner: MemSource,
    reads: AtomicU64,
    after: u64,
    eof_instead: bool,
}

impl Source for DroppingSource {
    fn size(&self) -> u64 {
        self.inner.size()
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, ReadError> {
        if self.reads.fetch_add(1, Ordering::SeqCst) >= self.after {
            if self.eof_instead {
                return Ok(0);
            }
            return Err(ReadError::new(
                offset,
                buf.len(),
                ReadErrorKind::DeviceGone,
                "gone",
            ));
        }
        self.inner.read_at(offset, buf)
    }
    fn describe(&self) -> String {
        "dropping".into()
    }
}

#[test]
fn source_that_cannot_reopen_stops_cleanly() {
    for eof_instead in [false, true] {
        let data = pattern(MIB, 26);
        let src: SharedSource = Arc::new(DroppingSource {
            inner: MemSource::new(data.clone()),
            reads: AtomicU64::new(0),
            after: 4,
            eof_instead,
        });
        let dir = tempfile::tempdir().unwrap();
        let mut im = new_imager(src, dir.path(), test_opts(test_profile())).unwrap();
        let mut lost = 0;
        let s = im
            .run(Control::new(), &mut |e| {
                if matches!(e, Event::DeviceLost { .. }) {
                    lost += 1;
                }
            })
            .unwrap();
        assert_eq!(lost, 1);
        assert!(!s.finished);
        assert!(s.warnings.iter().any(|w| w.contains("cannot be re-opened")));
        let map = MapFile::load(&paths(dir.path()).1).unwrap();
        assert_eq!(map.map.stats().errsize, 0, "nothing may be marked bad");
        assert_eq!(map.map.stats().rescued, 4 * 64 * KIB);
    }
}

#[test]
fn refusals() {
    let dir = tempfile::tempdir().unwrap();
    let (out, map) = paths(dir.path());
    // Output is the source file itself.
    let src_path = dir.path().join("source.img");
    std::fs::write(&src_path, pattern(MIB, 27)).unwrap();
    let fsrc: SharedSource = Arc::new(FileSource::open(&src_path).unwrap());
    let e = Imager::new(fsrc.clone(), &src_path, &map, test_opts(test_profile())).unwrap_err();
    assert!(matches!(e, Error::Refused(_)), "{e}");
    let e = Imager::new(fsrc.clone(), &out, &src_path, test_opts(test_profile())).unwrap_err();
    assert!(matches!(e, Error::Refused(_)), "{e}");
    let e = Imager::new(fsrc.clone(), &out, &out, test_opts(test_profile())).unwrap_err();
    assert!(matches!(e, Error::Refused(_)), "{e}");
    assert_eq!(std::fs::read(&src_path).unwrap(), pattern(MIB, 27));

    // An existing output without a mapfile is not overwritten by default.
    std::fs::write(&out, b"precious").unwrap();
    let e = Imager::new(fsrc.clone(), &out, &map, test_opts(test_profile())).unwrap_err();
    assert!(matches!(e, Error::Refused(_)), "{e}");
    assert_eq!(std::fs::read(&out).unwrap(), b"precious");
    assert!(!map.exists());

    // Zero-size source, empty domain.
    let empty: SharedSource = Arc::new(MemSource::new(Vec::new()));
    assert!(Imager::new(empty, &out, &map, test_opts(test_profile())).is_err());
    let mut o = test_opts(test_profile());
    o.domain = Some(Range::new(2 * MIB, 3 * MIB));
    assert!(Imager::new(fsrc, &dir.path().join("x.img"), &map, o).is_err());
}

#[test]
fn size_mismatch_and_inconsistent_outputs_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (out, map) = paths(dir.path());
    let data = pattern(2 * MIB, 28);
    let mk = || -> SharedSource { Arc::new(FaultySource::builder(data.clone()).build()) };
    // A mapfile for a 1 MiB device with rescued data.
    let mut mf = MapFile::new(MIB);
    mf.map.set(Range::new(0, 512 * KIB), BlockStatus::Finished);
    mf.save_atomic(
        &map,
        "t",
        "0",
        diskrescue_core::time::Timestamp::from_unix(0),
    )
    .unwrap();
    // Output missing although the map says data was rescued.
    let e = new_imager(mk(), dir.path(), test_opts(test_profile())).unwrap_err();
    assert!(
        e.to_string().contains("2097152") || matches!(e, Error::InvalidArgument(_)),
        "{e}"
    );
    let mut o = test_opts(test_profile());
    o.allow_size_mismatch = true;
    let e = new_imager(mk(), dir.path(), o.clone()).unwrap_err();
    assert!(matches!(e, Error::Refused(_)), "{e}");
    // Output shorter than the rescued data.
    std::fs::write(&out, vec![0u8; 1000]).unwrap();
    let e = new_imager(mk(), dir.path(), o.clone()).unwrap_err();
    assert!(e.to_string().contains("wrong output"), "{e}");
    // A plausible output: the map is grown and the rescue completes.
    let mut img = vec![0u8; MIB as usize];
    img[..512 * KIB as usize].copy_from_slice(&data[..512 * KIB as usize]);
    std::fs::write(&out, &img).unwrap();
    let src = Arc::new(FaultySource::builder(data.clone()).build());
    let run = image_in(dir.path(), &src, o, |_, _| {});
    assert!(run.summary.finished);
    assert_bytes_eq(&run.output, &data);
    assert!(run.summary.warnings.iter().any(|w| w.contains("adapted")));
    // The 512 KiB rescued before were not read again.
    assert!(src.read_log().iter().all(|r| r.offset >= 512 * KIB));

    // Shrinking below rescued data is refused even when allowed.
    let small: SharedSource = Arc::new(FaultySource::builder(pattern(256 * KIB, 1)).build());
    let mut o = test_opts(test_profile());
    o.allow_size_mismatch = true;
    assert!(new_imager(small, dir.path(), o).is_err());
}

#[test]
fn ddrescue_unknown_size_mapfile_is_adapted() {
    let dir = tempfile::tempdir().unwrap();
    let (out, map) = paths(dir.path());
    let data = pattern(MIB, 29);
    std::fs::write(
        &map,
        "0x10000 ? 1\n0x0 0x10000 +\n0x10000 0x7FFFFFFFFFFF0000 ?\n",
    )
    .unwrap();
    let mut img = vec![0u8; MIB as usize];
    img[..0x10000].copy_from_slice(&data[..0x10000]);
    std::fs::write(&out, &img).unwrap();
    let src = Arc::new(FaultySource::builder(data.clone()).build());
    let run = image_in(dir.path(), &src, test_opts(test_profile()), |_, _| {});
    assert!(run.summary.finished);
    assert_bytes_eq(&run.output, &data);
    assert_eq!(run.map.map.size(), MIB);
}

#[test]
fn short_reads_and_error_offsets() {
    let size = 2 * MIB;
    let bad = Range::new(MIB + 10 * 512, MIB + 11 * 512);
    let data = pattern(size, 30);
    let want = expected(&data, &ranges(&[(bad.start, bad.end)]));
    // Exact error offset: the good prefix of the failed cluster is read
    // again on its own right away.
    let src = Arc::new(FaultySource::builder(data.clone()).bad(bad).build());
    let (run, _d) = image(&src, test_opts(test_profile()));
    assert_bytes_eq(&run.output, &want);
    assert_eq!(run.map.map.bad_ranges(), ranges(&[(bad.start, bad.end)]));
    let log = src.read_log();
    let i = log.iter().position(|r| !r.is_ok()).unwrap();
    assert_eq!((log[i].offset, log[i].len as u64), (MIB, 64 * KIB));
    assert_eq!((log[i + 1].offset, log[i + 1].len as u64), (MIB, 10 * 512));
    assert!(log[i + 1].is_ok());
    // Short reads instead of errors.
    let src = Arc::new(
        FaultySource::builder(data.clone())
            .bad(bad)
            .short_reads(true)
            .build(),
    );
    let (run, _d) = image(&src, test_opts(test_profile()));
    assert_bytes_eq(&run.output, &want);
    assert_eq!(run.map.map.bad_ranges(), ranges(&[(bad.start, bad.end)]));
    // No offset information at all.
    let src = Arc::new(
        FaultySource::builder(data.clone())
            .bad(bad)
            .error_offset_granularity(0)
            .build(),
    );
    let (run, _d) = image(&src, test_opts(test_profile()));
    assert_bytes_eq(&run.output, &want);
    assert_eq!(run.map.map.bad_ranges(), ranges(&[(bad.start, bad.end)]));
    assert_reads_sane(&src);
}

#[test]
fn sparse_output_never_leaves_stale_bytes() {
    let size = 4 * MIB;
    let mut data = pattern(size, 31);
    data[MIB as usize..2 * MIB as usize].fill(0);
    let src = Arc::new(FaultySource::builder(data.clone()).build());
    // Existing output full of garbage: zeros must still be written.
    let dir = tempfile::tempdir().unwrap();
    let (out, _) = paths(dir.path());
    std::fs::write(&out, vec![0x5Au8; size as usize]).unwrap();
    let mut o = test_opts(test_profile());
    o.sparse_output = true;
    o.allow_existing_output = true;
    let run = image_in(dir.path(), &src, o.clone(), |_, _| {});
    assert_bytes_eq(&run.output, &data);
    // Fresh output: correct, and sparse where the filesystem supports holes.
    let src = Arc::new(FaultySource::builder(data.clone()).build());
    o.allow_existing_output = false;
    let dir = tempfile::tempdir().unwrap();
    let run = image_in(dir.path(), &src, o, |_, _| {});
    assert_bytes_eq(&run.output, &data);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let probe = dir.path().join("probe");
        let f = std::fs::File::create(&probe).unwrap();
        f.set_len(MIB).unwrap();
        if f.metadata().unwrap().blocks() == 0 {
            let allocated = std::fs::metadata(&run.out_path).unwrap().blocks() * 512;
            assert!(allocated <= size - 512 * KIB, "allocated {allocated}");
        } else {
            eprintln!("skipped: filesystem without sparse files");
        }
    }
}

#[test]
fn output_offset_and_preallocate() {
    let size = MIB;
    let data = pattern(size, 32);
    let src = Arc::new(FaultySource::builder(data.clone()).build());
    let mut o = test_opts(test_profile());
    o.output_offset = 4096;
    o.preallocate = true;
    let (run, _d) = image(&src, o);
    assert_eq!(run.output.len() as u64, size + 4096);
    assert!(run.output[..4096].iter().all(|&b| b == 0));
    assert_bytes_eq(&run.output[4096..], &data);
}

#[test]
fn reverse_start_reads_from_the_end() {
    let size = 2 * MIB;
    let data = pattern(size, 33);
    let src = Arc::new(
        FaultySource::builder(data.clone())
            .bad(Range::new(MIB, MIB + 512))
            .build(),
    );
    let mut o = test_opts(test_profile());
    o.start_reverse = true;
    let (run, _d) = image(&src, o);
    let log = src.read_log();
    assert_eq!(log[0].offset, size - 64 * KIB);
    assert!(run.has_phase(|p| *p
        == Phase::Copying {
            pass: 1,
            reverse: true
        }));
    assert_bytes_eq(&run.output, &expected(&data, &ranges(&[(MIB, MIB + 512)])));
}

#[test]
fn resume_inside_a_backward_pass_continues_downwards() {
    let size = 8 * MIB;
    let data = pattern(size, 34);
    let dir = tempfile::tempdir().unwrap();
    let (out, map) = paths(dir.path());
    let mut img = vec![0u8; size as usize];
    img[..MIB as usize].copy_from_slice(&data[..MIB as usize]);
    img[4 * MIB as usize..].copy_from_slice(&data[4 * MIB as usize..]);
    std::fs::write(&out, &img).unwrap();
    std::fs::write(
        &map,
        "0x300000 ? 2\n0x0 0x100000 +\n0x100000 0x300000 ?\n0x400000 0x400000 +\n",
    )
    .unwrap();
    let src = Arc::new(FaultySource::builder(data.clone()).build());
    let run = image_in(dir.path(), &src, test_opts(test_profile()), |_, _| {});
    assert!(run.summary.finished);
    assert_bytes_eq(&run.output, &data);
    let log = src.read_log();
    assert_eq!(
        log[0].offset,
        3 * MIB - 64 * KIB,
        "resumes below current_pos"
    );
    assert!(log[1].offset < log[0].offset);
    assert!(run.has_phase(|p| *p
        == Phase::Copying {
            pass: 2,
            reverse: true
        }));
}

#[test]
fn resume_inside_trimming_starts_at_current_pos_and_wraps() {
    let size = 4 * MIB;
    let data = pattern(size, 35);
    let dir = tempfile::tempdir().unwrap();
    let (out, map) = paths(dir.path());
    let mut img = data.clone();
    img[MIB as usize..(MIB + 64 * KIB) as usize].fill(0);
    img[2 * MIB as usize..(2 * MIB + 64 * KIB) as usize].fill(0);
    std::fs::write(&out, &img).unwrap();
    std::fs::write(
        &map,
        "0x200000 * 1\n0x0 0x100000 +\n0x100000 0x10000 *\n0x110000 0xF0000 +\n\
         0x200000 0x10000 *\n0x210000 0x1F0000 +\n",
    )
    .unwrap();
    let src = Arc::new(FaultySource::builder(data.clone()).build());
    let run = image_in(dir.path(), &src, test_opts(test_profile()), |_, _| {});
    assert!(run.summary.finished);
    assert_bytes_eq(&run.output, &data);
    let log = src.read_log();
    assert_eq!(log[0].offset, 2 * MIB);
    assert!(log.iter().any(|r| r.offset == MIB));
    assert!(log.iter().all(|r| r.len == 512));
    assert_eq!(run.phase_marks[0].0, Phase::Trimming);
}

#[test]
fn flash_profile_divides_instead_of_trimming() {
    let size = 4 * MIB;
    let bad = Range::new(MIB + 40 * KIB, MIB + 88 * KIB);
    let data = pattern(size, 36);
    let src = Arc::new(FaultySource::builder(data.clone()).bad(bad).build());
    let mut p = diskrescue_core::profile::ImagingProfile::ssd();
    p.timeout_copy = Duration::from_secs(10);
    p.timeout_scrape = Duration::from_secs(10);
    p.timeout_retry = Duration::from_secs(10);
    let (run, _d) = image(&src, test_opts(p));
    assert!(run.has_phase(|p| *p == Phase::Dividing));
    assert!(run.has_phase(|p| *p == Phase::Scraping));
    assert!(!run.has_phase(|p| *p == Phase::Trimming));
    assert_eq!(run.map.map.bad_ranges(), ranges(&[(bad.start, bad.end)]));
    assert_bytes_eq(
        &run.output,
        &expected(&data, &ranges(&[(bad.start, bad.end)])),
    );
    // Dividing reads are at most block_size / 8.
    let log = src.read_log();
    let d0 = run
        .phase_marks
        .iter()
        .find(|(p, _)| *p == Phase::Dividing)
        .unwrap()
        .1;
    let s0 = run
        .phase_marks
        .iter()
        .find(|(p, _)| *p == Phase::Scraping)
        .unwrap()
        .1;
    assert!(s0 > d0);
    assert!(log[d0..s0].iter().all(|r| r.len as u64 <= 32 * KIB));
    assert_reads_sane(&src);
}

#[test]
fn disabled_phases_leave_work_and_report_unfinished() {
    let size = 2 * MIB;
    let data = pattern(size, 37);
    let src = Arc::new(
        FaultySource::builder(data.clone())
            .bad(Range::new(MIB, MIB + 4 * KIB))
            .build(),
    );
    let mut p = test_profile();
    p.scrape = false;
    let (run, _d) = image(&src, test_opts(p));
    assert!(!run.summary.finished);
    assert_eq!(run.summary.stats.non_scraped, 3 * KIB);
    assert_eq!(run.summary.stats.bad_sectors, KIB);

    let src = Arc::new(
        FaultySource::builder(data.clone())
            .bad(Range::new(MIB, MIB + 512))
            .build(),
    );
    let mut o = test_opts(test_profile());
    o.max_copy_passes = 1;
    let (run, _d) = image(&src, o);
    assert!(!run.summary.finished);
    assert_eq!(run.summary.stats.non_tried, 64 * KIB);
}

#[test]
fn min_read_size_sets_the_scrape_unit() {
    let size = MIB;
    let data = pattern(size, 38);
    let bad = Range::new(512 * KIB + 2048, 512 * KIB + 2048 + 100);
    let src = Arc::new(FaultySource::builder(data.clone()).bad(bad).build());
    let mut p = test_profile();
    p.min_read_size = 2048;
    let (run, _d) = image(&src, test_opts(p));
    assert_eq!(
        run.map.map.bad_ranges(),
        ranges(&[(512 * KIB + 2048, 512 * KIB + 4096)])
    );
    for r in src.read_log() {
        assert!(r.len >= 2048, "{r:?}");
    }
}

#[test]
fn pause_resume_and_save_requests() {
    let size = 2 * MIB;
    let data = pattern(size, 39);
    let src = Arc::new(FaultySource::builder(data.clone()).build());
    let dir = tempfile::tempdir().unwrap();
    let mut o = test_opts(test_profile());
    o.progress_interval = Duration::ZERO;
    let mut paused_once = false;
    let mut saves_requested = 0;
    let run = image_in(dir.path(), &src, o, |ev, ctl| {
        if let Event::Progress(p) = ev {
            if !paused_once && p.stats.rescued > 0 {
                paused_once = true;
                ctl.pause(true);
                // Unpause from another thread, as a UI would.
                let ctl2 = Arc::clone(ctl);
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(100));
                    ctl2.pause(false);
                });
            } else if saves_requested < 2 && p.stats.rescued > MIB {
                saves_requested += 1;
                ctl.request_save();
            }
        }
    });
    assert!(run.summary.finished);
    let logs = run.logs();
    assert!(logs.iter().any(|m| m == "paused"));
    assert!(logs.iter().any(|m| m == "resumed"));
    let saves = run
        .events
        .iter()
        .filter(|e| matches!(e, Event::Saved))
        .count();
    assert!(saves >= 4, "{saves}");
    assert_bytes_eq(&run.output, &data);
}

#[test]
fn rescued_image_handoff() {
    let size = MIB;
    let bad = Range::new(100 * KIB, 100 * KIB + 512);
    let data = pattern(size, 40);
    let src = Arc::new(FaultySource::builder(data).bad(bad).build());
    let mut o = test_opts(test_profile());
    o.max_copy_passes = 1;
    o.profile.trim = false;
    o.profile.scrape = false;
    o.profile.retry_passes = 0;
    let (run, _d) = image(&src, o);
    let holes = bad_ranges_of(&run.map_path).unwrap();
    assert_eq!(holes, run.map.map.unfinished_ranges());
    assert!(holes.covers(bad));
    let img = open_rescued_image(&run.out_path, &run.map_path).unwrap();
    assert_eq!(img.size(), size);
    assert_eq!(img.unreadable(), Some(holes));
    assert!(bad_ranges_of(&run.map_path.with_extension("nope")).is_err());
    assert!(open_rescued_image(&run.out_path, &run.map_path.with_extension("nope")).is_err());
}

/// Randomized faults, profiles and domains. Whatever happens, the image
/// never holds wrong data for a finished byte, nothing outside the domain
/// is touched, every read is aligned and inside the source, and with all
/// phases enabled the bad sectors are exactly the faulty ones.
#[test]
fn randomized_fault_fuzz() {
    // FUZZ_ITERS / FUZZ_SEED widen the search when hunting bugs.
    let env = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<u64>().ok());
    let mut rng = StdRng::seed_from_u64(env("FUZZ_SEED").unwrap_or(0xD15C));
    for iter in 0..env("FUZZ_ITERS").unwrap_or(30) {
        let sector: u64 = if rng.gen_bool(0.3) { 4096 } else { 512 };
        let size = rng.gen_range(64 * KIB..2 * MIB) / 512 * 512 + rng.gen_range(0..2) * 300;
        let data = pattern(size, iter);
        let mut b = FaultySource::builder(data.clone()).sector_size(sector as u32);
        let mut bad = RangeSet::new();
        let mut flaky = RangeSet::new();
        for _ in 0..rng.gen_range(0..4) {
            let s = rng.gen_range(0..size);
            let e = (s + rng.gen_range(1..256 * KIB)).min(size);
            bad.insert(Range::new(s, e));
            b = b.bad(Range::new(s, e));
        }
        for _ in 0..rng.gen_range(0..2) {
            let s = rng.gen_range(0..size);
            let e = (s + rng.gen_range(1..16 * KIB)).min(size);
            flaky.insert(Range::new(s, e));
            b = b.flaky(Range::new(s, e), rng.gen_range(0.1..0.9), iter);
        }
        let check = rng.gen_bool(0.5);
        if check && rng.gen_bool(0.5) {
            b = b.stale_after_error(true);
        }
        if rng.gen_bool(0.3) {
            b = b.short_reads(true);
        }
        if rng.gen_bool(0.3) {
            b = b.error_offset_granularity(0);
        }
        // Hangs (timeouts), slow areas and a vanishing device.
        let mut slow_on = false;
        let mut extra_faults = Vec::new();
        if rng.gen_bool(0.2) {
            let s = rng.gen_range(0..size);
            extra_faults.push(format!("hang {s}+1024"));
            b = b.hang(
                Range::new(s, (s + 1024).min(size)),
                Duration::from_millis(30),
            );
        }
        if rng.gen_bool(0.2) {
            let s = rng.gen_range(0..size);
            extra_faults.push(format!("slow {s}+64K"));
            b = b.slow(
                Range::new(s, (s + 64 * KIB).min(size)),
                Duration::from_millis(3),
            );
            slow_on = true;
        }
        if rng.gen_bool(0.2) {
            let (after, reopens) = (rng.gen_range(0..40), rng.gen_range(0..3));
            extra_faults.push(format!("vanish {after}/{reopens}"));
            b = b.vanish(after, reopens);
        }
        let src = Arc::new(b.build());
        let mut p = test_profile();
        p.block_size = [4 * KIB, 16 * KIB, 64 * KIB, 128 * KIB][rng.gen_range(0..4)];
        p.timeout_copy = Duration::from_millis(10);
        if slow_on && rng.gen_bool(0.5) {
            p.min_read_rate = Some(if rng.gen_bool(0.5) { 0 } else { 50 * MIB });
            p.slow_reads_before_skip = rng.gen_range(1..4);
        }
        if rng.gen_bool(0.2) {
            p.reopen_after_errors = Some(rng.gen_range(1..4));
        }
        p.trim = rng.gen_bool(0.7);
        p.divide = rng.gen_bool(0.5);
        p.retry_passes = rng.gen_range(0..3);
        p.reverse_second_pass = rng.gen_bool(0.7);
        p.check_on_error = check;
        p.skip_initial = [0, 4 * KIB, 64 * KIB][rng.gen_range(0..3)];
        p.skip_initial_divisor = 0;
        let mut o = test_opts(p.clone());
        o.start_reverse = rng.gen_bool(0.3);
        let domain = if rng.gen_bool(0.5) {
            let a = rng.gen_range(0..size - 1);
            let z = rng.gen_range(a + 1..=size);
            Range::new(a, z)
        } else {
            Range::new(0, size)
        };
        o.domain = Some(domain);
        let (run, _d) = image(&src, o);
        let ctx = format!(
            "iter {iter}: sector {sector} size {size} domain {domain:?} bad {:?} flaky {:?} \
             other {extra_faults:?} profile {p:?}",
            bad.iter().collect::<Vec<_>>(),
            flaky.iter().collect::<Vec<_>>()
        );
        if std::env::var("FUZZ_DEBUG")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            == Some(iter)
        {
            eprintln!("{ctx}");
            for (i, r) in src.read_log().iter().enumerate() {
                eprintln!("{i}: {r:?}");
            }
            for e in &run.events {
                eprintln!("{e:?}");
            }
        }
        let map = &run.map.map;
        map.validate().unwrap();
        assert_eq!(map.size(), size, "{ctx}");
        assert_reads_sane(&src);
        // Finished bytes hold the source's bytes; no byte of the image is
        // ever wrong (it is the source's byte or still zero).
        for blk in map.iter() {
            let (s, e) = (blk.pos as usize, blk.end() as usize);
            if blk.status == BlockStatus::Finished && run.output[s..e] != data[s..e] {
                let first = (s..e).find(|&i| run.output[i] != data[i]).unwrap();
                panic!(
                    "{ctx}: finished block {blk:?} differs from the source at {first}; \
                     stale reads {}",
                    src.counters().stale
                );
            }
        }
        for (i, (&o, &d)) in run.output.iter().zip(&data).enumerate() {
            assert!(o == d || o == 0, "{ctx}: wrong byte at {i}");
        }
        // Outside the domain nothing changed.
        let outside = RangeSet::from_range(domain).complement_within(Range::new(0, size));
        for r in outside.iter() {
            assert!(
                run.output[r.start as usize..r.end as usize]
                    .iter()
                    .all(|&x| x == 0),
                "{ctx}: written outside the domain in {r:?}"
            );
            assert!(
                map.stats_in(r).non_tried == r.len(),
                "{ctx}: outside-domain status changed in {r:?}"
            );
        }
        // Sectors touching a hard fault are never finished.
        let round = |set: &RangeSet| -> RangeSet {
            set.iter()
                .map(|r| {
                    Range::new(
                        r.start / sector * sector,
                        (r.end.div_ceil(sector) * sector).min(size),
                    )
                })
                .collect::<RangeSet>()
                .intersection(&RangeSet::from_range(domain))
        };
        let hard = round(&bad);
        let finished = map.ranges_with(&[BlockStatus::Finished]);
        assert!(finished.intersection(&hard).is_empty(), "{ctx}");
        // All phases on: the unfinished bytes are exactly the hard faults
        // plus (possibly) flaky sectors.
        if p.scrape {
            let unfinished = map
                .unfinished_ranges()
                .intersection(&RangeSet::from_range(domain));
            let allowed = hard.union(&round(&flaky));
            let extra = allowed
                .complement_within(Range::new(0, size))
                .intersection(&unfinished);
            assert!(
                extra.is_empty(),
                "{ctx}: unexpected unfinished bytes {:?} (blocks there: {:?})",
                extra.iter().collect::<Vec<_>>(),
                extra
                    .iter()
                    .flat_map(|r| map.iter().filter(move |b| b.range().overlaps(r)))
                    .collect::<Vec<_>>()
            );
            for r in hard.iter() {
                assert!(map.bad_ranges().covers(r), "{ctx}: {r:?} not bad");
            }
            assert!(
                run.summary.finished,
                "{ctx}: not finished: {:?} warnings {:?} blocks {:?}",
                run.summary.stats,
                run.summary.warnings,
                map.blocks()
            );
        }
    }
}

/// Interrupt the rescue at random points (any phase), resume with a new
/// Imager on the same mapfile until done. The result must be exactly what
/// an uninterrupted rescue produces: the bad sectors are the faulty ones,
/// everything else is rescued, and the image is byte-exact.
#[test]
fn randomized_interrupt_and_resume() {
    let env = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<u64>().ok());
    let mut rng = StdRng::seed_from_u64(env("FUZZ_SEED").unwrap_or(0x5E5));
    for iter in 0..env("FUZZ_ITERS").unwrap_or(12) {
        let sector: u64 = if rng.gen_bool(0.3) { 4096 } else { 512 };
        let size = rng.gen_range(128 * KIB..1536 * KIB) / sector * sector;
        let data = pattern(size, 1000 + iter);
        let mut b = FaultySource::builder(data.clone()).sector_size(sector as u32);
        let mut bad = RangeSet::new();
        for _ in 0..rng.gen_range(1..4) {
            let s = rng.gen_range(0..size);
            let e = (s + rng.gen_range(1..128 * KIB)).min(size);
            bad.insert(Range::new(s, e));
            b = b.bad(Range::new(s, e));
        }
        if rng.gen_bool(0.5) {
            b = b.short_reads(true);
        }
        let src = Arc::new(b.build());
        let mut p = test_profile();
        p.block_size = [8 * KIB, 32 * KIB, 64 * KIB][rng.gen_range(0..3)];
        p.trim = rng.gen_bool(0.7);
        p.divide = rng.gen_bool(0.5);
        p.retry_passes = rng.gen_range(0..3);
        p.check_on_error = rng.gen_bool(0.5);
        p.skip_initial = [4 * KIB, 64 * KIB][rng.gen_range(0..2)];
        p.skip_initial_divisor = 0;
        let mut o = test_opts(p.clone());
        o.progress_interval = Duration::ZERO;
        o.start_reverse = rng.gen_bool(0.3);
        let dir = tempfile::tempdir().unwrap();
        let mut rounds = 0;
        let run = loop {
            rounds += 1;
            assert!(
                rounds < 60,
                "iter {iter}: no progress after {rounds} resumes"
            );
            let cut = if rounds < 8 {
                rng.gen_range(1..40u64)
            } else {
                u64::MAX
            };
            let mut seen = 0u64;
            let run = image_in(dir.path(), &src, o.clone(), |ev, ctl| {
                if let Event::Progress(_) = ev {
                    seen += 1;
                    if seen == cut {
                        ctl.cancel();
                    }
                }
            });
            run.map.map.validate().unwrap();
            if run.summary.finished {
                break run;
            }
            assert!(run.summary.cancelled, "iter {iter}: {:?}", run.summary);
        };
        let hard: RangeSet = bad
            .iter()
            .map(|r| Range::new(r.start / sector * sector, r.end.div_ceil(sector) * sector))
            .collect();
        let ctx = format!("iter {iter} after {rounds} runs, profile {p:?}");
        assert_eq!(run.map.map.bad_ranges(), hard, "{ctx}");
        assert_eq!(run.map.map.stats().pending(), 0, "{ctx}");
        assert_bytes_eq(&run.output, &expected(&data, &hard));
        assert_reads_sane(&src);
    }
}
