//! Engine behaviour on simulated failing devices (the test list of the
//! imaging brief, numbered as there).

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use diskrescue_core::Range;
use diskrescue_imaging::simulate::{FaultySource, SimOutcome};
use diskrescue_imaging::{BlockStatus, CurrentStatus, Event, Imager, MapFile, Phase};

/// Brief test 1: Clean source: identical output, one finished block, finished summary.
#[test]
fn t01_clean_source_is_copied_exactly() {
    let size = 4 * MIB;
    let data = pattern(size, 1);
    let src = Arc::new(FaultySource::builder(data.clone()).build());
    let (run, _dir) = image(&src, test_opts(test_profile()));
    assert_bytes_eq(&run.output, &data);
    let blocks: Vec<_> = run.map.map.iter().collect();
    assert_eq!(blocks.len(), 1);
    assert_eq!(
        (blocks[0].pos, blocks[0].size, blocks[0].status),
        (0, size, BlockStatus::Finished)
    );
    assert_eq!(run.map.current_status, CurrentStatus::Finished);
    assert!(run.summary.finished && !run.summary.cancelled);
    assert_eq!(run.summary.errors, 0);
    assert_eq!(run.summary.stats.rescued, size);
    assert_reads_sane(&src);
    // One read per 64 KiB cluster, nothing read twice.
    let log = src.read_log();
    assert_eq!(log.len() as u64, size / (64 * KIB));
    assert!(log.iter().all(|r| r.len as u64 == 64 * KIB && r.is_ok()));
    assert!(run.events.iter().any(|e| matches!(e, Event::Finished(_))));
    assert!(run.events.iter().any(|e| matches!(e, Event::Saved)));
    let p = run.last_progress.unwrap();
    assert_eq!(p.stats.rescued, size);
    assert_eq!(p.size, size);
    // Sidecar exists and is JSON.
    let sc = std::fs::read(run.map_path.with_extension("map.dr.json")).unwrap();
    let v: serde_json::Value = serde_json::from_slice(&sc).unwrap();
    assert_eq!(v["format"], 1);
}

/// Brief test 2: One bad sector: exactly that sector is bad, zero in the output.
#[test]
fn t02_single_bad_sector_is_isolated_exactly() {
    let size = 4 * MIB;
    let bad = Range::new(2 * MIB + 7 * 512, 2 * MIB + 8 * 512);
    let data = pattern(size, 2);
    let src = Arc::new(FaultySource::builder(data.clone()).bad(bad).build());
    let (run, _dir) = image(&src, test_opts(test_profile()));
    assert_eq!(run.status_ranges(BlockStatus::BadSector), vec![bad]);
    assert_eq!(run.map.map.len(), 3);
    assert_bytes_eq(
        &run.output,
        &expected(&data, &ranges(&[(bad.start, bad.end)])),
    );
    assert!(run.output[bad.start as usize..bad.end as usize]
        .iter()
        .all(|&b| b == 0));
    let st = run.summary.stats;
    assert_eq!(
        (st.bad_sectors, st.bad_areas, st.rescued),
        (512, 1, size - 512)
    );
    assert!(run.summary.finished);
    assert!(run.has_phase(|p| *p == Phase::Trimming));
    assert_reads_sane(&src);
}

/// Brief test 3: A 3 MiB bad area with a 64 KiB good island at an unaligned position:
/// the island is rescued by trimming/scraping, two exact bad runs remain.
#[test]
fn t03_good_island_inside_bad_area_is_rescued() {
    let size = 16 * MIB;
    let island = Range::new(5 * MIB + 12 * KIB, 5 * MIB + 76 * KIB);
    let data = pattern(size, 3);
    let src = Arc::new(
        FaultySource::builder(data.clone())
            .bad(Range::new(4 * MIB, island.start))
            .bad(Range::new(island.end, 7 * MIB))
            .build(),
    );
    let (run, _dir) = image(&src, test_opts(test_profile()));
    assert_eq!(
        run.status_ranges(BlockStatus::BadSector),
        vec![
            Range::new(4 * MIB, island.start),
            Range::new(island.end, 7 * MIB)
        ]
    );
    assert_eq!(run.summary.stats.bad_areas, 2);
    assert_eq!(run.map.map.get(island.start), Some(BlockStatus::Finished));
    let holes = ranges(&[(4 * MIB, island.start), (island.end, 7 * MIB)]);
    assert_bytes_eq(&run.output, &expected(&data, &holes));
    assert!(run.summary.finished);
    assert_reads_sane(&src);
}

/// Brief test 4: Skip growth: pass 1 jumps through a long bad region with doubling
/// skips; pass 2 approaches the region from the other side.
#[test]
fn t04_skips_grow_exponentially_and_pass_two_comes_back() {
    let size = 32 * MIB;
    let bad = Range::new(4 * MIB, 20 * MIB);
    let data = pattern(size, 4);
    let src = Arc::new(FaultySource::builder(data.clone()).bad(bad).build());
    let mut p = test_profile();
    p.block_size = 64 * KIB;
    p.skip_initial = 64 * KIB;
    p.skip_initial_divisor = 0;
    p.skip_max_percent = 100.0;
    p.skip_max = 8 * MIB;
    p.skip_growth = 2.0;
    p.retry_passes = 0;
    let (run, _dir) = image(&src, test_opts(p));
    let log = src.read_log();
    let mark = |pred: &dyn Fn(&Phase) -> bool| {
        run.phase_marks
            .iter()
            .find(|(ph, _)| pred(ph))
            .map(|(_, i)| *i)
            .unwrap()
    };
    let p2 = mark(&|ph| matches!(ph, Phase::Copying { pass: 2, .. }));
    let p3 = mark(&|ph| matches!(ph, Phase::Copying { pass: 3, .. }));
    // Pass 1: the failed reads are separated by block + skip, and the skip
    // doubles from 64 KiB up to 8 MiB.
    let fails: Vec<u64> = log[..p2]
        .iter()
        .filter(|r| !r.is_ok())
        .map(|r| r.offset)
        .collect();
    assert_eq!(fails[0], bad.start);
    let skips: Vec<u64> = fails.windows(2).map(|w| w[1] - w[0] - 64 * KIB).collect();
    assert_eq!(
        skips,
        (0..7).map(|k| (64 * KIB) << k).collect::<Vec<_>>(),
        "fails at {fails:x?}"
    );
    // After the last failure the 8 MiB skip lands beyond the bad region
    // and pass 1 reads on from there.
    let resume_at = fails.last().unwrap() + 64 * KIB + 8 * MIB;
    assert!(resume_at > bad.end);
    assert!(log[..p2].iter().any(|r| r.offset == resume_at && r.is_ok()));
    // Pass 2 runs backwards: strictly descending offsets, successful down to
    // the end of the bad region, first failure just below it.
    let pass2 = &log[p2..p3];
    assert!(!pass2.is_empty());
    assert!(pass2.windows(2).all(|w| w[1].offset < w[0].offset));
    let first_fail = pass2.iter().position(|r| !r.is_ok()).unwrap();
    assert_eq!(pass2[first_fail].offset, bad.end - 64 * KIB);
    assert!(pass2[..first_fail].iter().all(|r| r.offset >= bad.end));
    assert_eq!(pass2[first_fail - 1].offset, bad.end);
    // Final map: exactly the bad region is bad.
    assert_eq!(run.status_ranges(BlockStatus::BadSector), vec![bad]);
    assert_bytes_eq(
        &run.output,
        &expected(&data, &ranges(&[(bad.start, bad.end)])),
    );
    assert_reads_sane(&src);
}

/// Brief test 5: Hanging reads: time out in the copying passes (short timeout), are
/// counted, and are rescued by trimming with its longer timeout.
#[test]
fn t05_timeouts_are_counted_and_retried_with_longer_timeout() {
    let size = 4 * MIB;
    let hang = Range::new(MIB, MIB + 1024);
    let data = pattern(size, 5);
    let src = Arc::new(
        FaultySource::builder(data.clone())
            .hang(hang, Duration::from_millis(300))
            .build(),
    );
    let mut p = test_profile();
    p.timeout_copy = Duration::from_millis(100);
    p.timeout_scrape = Duration::from_secs(5);
    p.timeout_retry = Duration::from_secs(5);
    let (run, _dir) = image(&src, test_opts(p));
    assert!(run.summary.timeouts >= 1, "{:?}", run.summary);
    assert_eq!(run.summary.timeouts, src.counters().timeouts);
    assert_eq!(
        run.last_progress.as_ref().unwrap().timeouts,
        run.summary.timeouts
    );
    assert_bytes_eq(&run.output, &data);
    assert_eq!(run.map.map.len(), 1);
    assert!(run.summary.finished);
    let log = src.read_log();
    assert!(log.iter().any(|r| matches!(
        r.outcome,
        SimOutcome::Error {
            kind: diskrescue_core::ReadErrorKind::Timeout,
            ..
        }
    ) && r.timeout == Some(Duration::from_millis(100))));
    // The hanging sectors were read successfully with the scrape timeout.
    assert!(log.iter().any(|r| r.is_ok()
        && r.range().overlaps(hang)
        && r.timeout == Some(Duration::from_secs(5))));
    assert_reads_sane(&src);
}

/// Brief test 6: The device vanishes after N reads and needs two re-opens: DeviceLost
/// and DeviceBack are reported, nothing is marked bad, output complete.
#[test]
fn t06_vanished_device_is_waited_for_and_nothing_is_marked_bad() {
    let size = 4 * MIB;
    let data = pattern(size, 6);
    let src = Arc::new(FaultySource::builder(data.clone()).vanish(10, 2).build());
    let (run, _dir) = image(&src, test_opts(test_profile()));
    let lost = run
        .events
        .iter()
        .position(|e| matches!(e, Event::DeviceLost { .. }))
        .expect("DeviceLost");
    let back = run
        .events
        .iter()
        .position(|e| matches!(e, Event::DeviceBack { .. }))
        .expect("DeviceBack");
    assert!(lost < back);
    if let Event::DeviceLost { at } = run.events[lost] {
        assert_eq!(at, 10 * 64 * KIB);
    }
    assert_eq!(run.summary.reattaches, 1);
    assert_eq!(run.summary.errors, 0);
    assert_eq!(src.counters().reopen_calls, 2);
    assert_bytes_eq(&run.output, &data);
    assert_eq!(run.map.map.len(), 1);
    assert!(run.summary.finished);
    assert_reads_sane(&src);
}

/// Brief test 7: Resume: cancel at ~40 %, a new Imager on the same mapfile finishes;
/// the output is identical and rescued data is not read again.
#[test]
fn t07_cancel_and_resume_gives_identical_output() {
    let size = 8 * MIB;
    let bad = [(3 * MIB, 3 * MIB + 512), (6 * MIB + 1536, 6 * MIB + 2048)];
    let data = pattern(size, 7);
    let mut b = FaultySource::builder(data.clone());
    for (s, e) in bad {
        b = b.bad(Range::new(s, e));
    }
    let src = Arc::new(b.build());
    let dir = tempfile::tempdir().unwrap();
    let mut opts = test_opts(test_profile());
    opts.progress_interval = Duration::ZERO;
    let run1 = image_in(dir.path(), &src, opts.clone(), |ev, ctl| {
        if let Event::Progress(p) = ev {
            if p.stats.rescued * 10 >= size * 4 {
                ctl.cancel();
            }
        }
    });
    assert!(run1.summary.cancelled && !run1.summary.finished);
    let frac = run1.summary.stats.rescued as f64 / size as f64;
    assert!((0.39..0.6).contains(&frac), "{frac}");
    assert!(run1.map.map.stats().non_tried > 0);
    assert_eq!(run1.map.current_status, CurrentStatus::Copying);
    // The saved text parses back to the same map, and writing it again
    // reproduces the same text apart from the timestamps.
    let text = std::fs::read_to_string(&run1.map_path).unwrap();
    let parsed = MapFile::parse(&text).unwrap();
    assert_eq!(parsed.map, run1.map.map);
    let finished_after_run1 = run1.map.map.ranges_with(&[BlockStatus::Finished]);
    let reads_before = src.read_log_len();

    let run2 = image_in(dir.path(), &src, opts, |_, _| {});
    assert!(run2.summary.finished && !run2.summary.cancelled);
    let holes = ranges(&bad);
    assert_bytes_eq(&run2.output, &expected(&data, &holes));
    assert_eq!(run2.map.map.bad_ranges(), holes);
    // No successful read of run 2 re-read data rescued by run 1.
    for r in &src.read_log()[reads_before..] {
        if r.is_ok() {
            assert!(!finished_after_run1.intersects(r.range()), "{r:?}");
        }
    }
    assert_reads_sane(&src);
}

/// Brief test 9: Flaky sectors (p = 0.5): the retry passes recover everything.
///
/// The simulator's outcome is a pure function of (seed, sector, attempt),
/// so the run is reproducible; seed 3 leaves bad sectors that need all
/// three retry passes (forwards, backwards, forwards).
#[test]
fn t09_flaky_sectors_are_recovered_by_retries() {
    let size = 2 * MIB;
    let flaky = Range::new(MIB, MIB + 4 * KIB);
    let data = pattern(size, 9);
    let src = Arc::new(
        FaultySource::builder(data.clone())
            .flaky(flaky, 0.5, 3)
            .build(),
    );
    let mut p = test_profile();
    p.retry_passes = 3;
    let (run, _dir) = image(&src, test_opts(p));
    assert!(
        run.has_phase(|p| matches!(p, Phase::Retrying { .. })),
        "the seed must leave bad sectors for the retry passes"
    );
    assert_bytes_eq(&run.output, &data);
    assert_eq!(run.map.map.len(), 1, "{:?}", run.map.map.blocks());
    assert!(run.summary.finished);
    // Retry passes alternate direction.
    let dirs: Vec<bool> = run
        .phase_marks
        .iter()
        .filter_map(|(p, _)| match p {
            Phase::Retrying { reverse, .. } => Some(*reverse),
            _ => None,
        })
        .collect();
    assert_eq!(dirs, vec![false, true, false]);
    assert_reads_sane(&src);
}

/// Brief test 9b: Whatever the luck, flaky sectors never produce wrong data: over many
/// seeds every finished byte equals the source, only flaky sectors can be
/// left bad, and retries never lose anything.
#[test]
fn t09b_flaky_outcomes_are_always_correct() {
    let size = 512 * KIB;
    let flaky = Range::new(128 * KIB, 128 * KIB + 8 * KIB);
    let data = pattern(size, 91);
    for seed in 0..40 {
        let src = Arc::new(
            FaultySource::builder(data.clone())
                .flaky(flaky, 0.5, seed)
                .build(),
        );
        let mut p = test_profile();
        p.retry_passes = 3;
        let (run, _dir) = image(&src, test_opts(p));
        let map = &run.map.map;
        for r in map.ranges_with(&[BlockStatus::Finished]).iter() {
            let (s, e) = (r.start as usize, r.end as usize);
            assert_bytes_eq(&run.output[s..e], &data[s..e]);
        }
        for r in map.bad_ranges().iter() {
            assert!(
                r.start >= flaky.start && r.end <= flaky.end,
                "seed {seed}: {r:?}"
            );
            assert!(run.output[r.start as usize..r.end as usize]
                .iter()
                .all(|&b| b == 0));
        }
        assert_eq!(map.stats().pending(), 0, "seed {seed}");
        assert!(run.summary.finished);
        assert_reads_sane(&src);
    }
}

/// Brief test 10: A bridge that returns zeros for the read after an error: with
/// check_on_error the stale data is never accepted as rescued.
#[test]
fn t10_stale_data_after_error_is_not_accepted() {
    let size = 4 * MIB;
    let bad = Range::new(2 * MIB + 1024, 2 * MIB + 1536);
    let data = pattern(size, 10);
    let mk = || {
        Arc::new(
            FaultySource::builder(data.clone())
                .bad(bad)
                .stale_after_error(true)
                .build(),
        )
    };
    let holes = ranges(&[(bad.start, bad.end)]);
    // Without the check the simulated bridge corrupts the image: this is
    // the hazard the option exists for.
    let src = mk();
    let mut p = test_profile();
    p.check_on_error = false;
    let (run, _d1) = image(&src, test_opts(p.clone()));
    assert!(src.counters().stale > 0);
    assert!(
        run.output != expected(&data, &holes),
        "the stale read must corrupt the unchecked image"
    );

    let src = mk();
    p.check_on_error = true;
    let (run, _d2) = image(&src, test_opts(p));
    assert!(src.counters().stale > 0);
    assert_bytes_eq(&run.output, &expected(&data, &holes));
    assert_eq!(run.map.map.bad_ranges(), holes);
    assert!(run
        .logs()
        .iter()
        .any(|m| m.contains("returned different data")));
    // Every finished byte holds the source's bytes.
    for r in run.map.map.ranges_with(&[BlockStatus::Finished]).iter() {
        let (s, e) = (r.start as usize, r.end as usize);
        assert_eq!(run.output[s..e], data[s..e]);
    }
    assert_reads_sane(&src);
}

/// Brief test 11: Domain subset: only the domain is read, written and changed.
#[test]
fn t11_domain_subset_touches_only_the_domain() {
    let size = 8 * MIB;
    let domain = Range::new(2 * MIB, 5 * MIB + 1536);
    let bad_in = Range::new(3 * MIB, 3 * MIB + 512);
    let bad_out = Range::new(6 * MIB, 6 * MIB + 512);
    let data = pattern(size, 11);
    let src = Arc::new(
        FaultySource::builder(data.clone())
            .bad(bad_in)
            .bad(bad_out)
            .build(),
    );
    let dir = tempfile::tempdir().unwrap();
    let (out, _) = paths(dir.path());
    std::fs::write(&out, vec![0xAAu8; size as usize]).unwrap();
    let mut opts = test_opts(test_profile());
    opts.domain = Some(domain);
    opts.allow_existing_output = true;
    let run = image_in(dir.path(), &src, opts, |_, _| {});
    let (ds, de) = (domain.start as usize, domain.end as usize);
    assert!(run.output[..ds].iter().all(|&b| b == 0xAA));
    assert!(run.output[de..].iter().all(|&b| b == 0xAA));
    for (i, (&o, &d)) in run.output[ds..de].iter().zip(&data[ds..de]).enumerate() {
        let pos = (ds + i) as u64;
        if bad_in.contains(pos) {
            assert_eq!(o, 0xAA, "bad sector must not be written");
        } else {
            assert_eq!(o, d, "byte {pos}");
        }
    }
    assert_eq!(run.map.map.get(0), Some(BlockStatus::NonTried));
    assert_eq!(run.map.map.get(domain.end), Some(BlockStatus::NonTried));
    assert_eq!(
        run.map.map.find_next(
            0,
            BlockStatus::NonTried,
            diskrescue_imaging::Direction::Forward
        ),
        Some(Range::new(0, domain.start))
    );
    assert_eq!(run.status_ranges(BlockStatus::BadSector), vec![bad_in]);
    assert_eq!(run.summary.stats.total(), domain.len());
    assert!(run.summary.finished);
    for r in src.read_log() {
        assert!(
            r.offset >= domain.start && r.offset + r.len as u64 <= domain.end,
            "read outside the domain: {r:?}"
        );
    }
    assert_reads_sane(&src);
}

/// Brief test 12: 4096-byte sectors with an unaligned domain: every read is 4096
/// aligned (reads are widened to whole sectors), writes stay in the domain.
#[test]
fn t12_4096_byte_sectors_get_aligned_reads_only() {
    let size = 8 * MIB;
    let domain = Range::new(MIB + 1000, 7 * MIB - 300);
    let bad = Range::new(3 * MIB + 8 * KIB, 3 * MIB + 12 * KIB);
    let data = pattern(size, 12);
    let src = Arc::new(
        FaultySource::builder(data.clone())
            .sector_size(4096)
            .bad(bad)
            .build(),
    );
    let mut opts = test_opts(test_profile());
    opts.domain = Some(domain);
    let (run, _dir) = image(&src, opts);
    assert_eq!(src.counters().unaligned, 0);
    for r in src.read_log() {
        assert_eq!(r.offset % 4096, 0, "{r:?}");
        assert_eq!(r.len % 4096, 0, "{r:?}");
    }
    assert_eq!(run.status_ranges(BlockStatus::BadSector), vec![bad]);
    let mut want = vec![0u8; size as usize];
    want[domain.start as usize..domain.end as usize]
        .copy_from_slice(&data[domain.start as usize..domain.end as usize]);
    want[bad.start as usize..bad.end as usize].fill(0);
    assert_bytes_eq(&run.output, &want);
    assert_eq!(
        run.map.map.ranges_with(&[BlockStatus::Finished]),
        ranges(&[(domain.start, bad.start), (bad.end, domain.end)])
    );
    assert_reads_sane(&src);
}

/// Brief test 12b: A mapfile with boundaries that are not multiples of a 4096-byte
/// sector (written by a 512-byte tool) is resumed with aligned reads only,
/// and finished data from it is kept.
#[test]
fn t12b_unaligned_mapfile_boundaries_are_joined() {
    let size = 2 * MIB;
    let data = pattern(size, 121);
    let src = Arc::new(
        FaultySource::builder(data.clone())
            .sector_size(4096)
            .build(),
    );
    let dir = tempfile::tempdir().unwrap();
    let (out, map) = paths(dir.path());
    // The first 1 KiB + 512 were rescued earlier; a bad sector follows.
    let mut img = vec![0u8; size as usize];
    img[..1536].copy_from_slice(&data[..1536]);
    std::fs::write(&out, &img).unwrap();
    std::fs::write(
        &map,
        "0x600 ? 1\n0x0 0x600 +\n0x600 0x200 -\n0x800 0x1FF800 ?\n",
    )
    .unwrap();
    let run = image_in(dir.path(), &src, test_opts(test_profile()), |_, _| {});
    assert_bytes_eq(&run.output, &data);
    assert_eq!(run.map.map.len(), 1);
    assert_eq!(src.counters().unaligned, 0);
    for r in src.read_log() {
        assert_eq!(r.offset % 4096, 0, "{r:?}");
    }
}

/// Resume an existing Imager twice: a finished map issues no further reads.
#[test]
fn finished_map_needs_no_reads() {
    let size = MIB;
    let data = pattern(size, 13);
    let src = Arc::new(FaultySource::builder(data.clone()).build());
    let dir = tempfile::tempdir().unwrap();
    let run1 = image_in(dir.path(), &src, test_opts(test_profile()), |_, _| {});
    assert!(run1.summary.finished);
    let n = src.read_log_len();
    let run2 = image_in(dir.path(), &src, test_opts(test_profile()), |_, _| {});
    assert!(run2.summary.finished);
    assert_eq!(src.read_log_len(), n);
    assert_bytes_eq(&run2.output, &data);
}

/// The imager can also be driven directly (no helper).
#[test]
fn direct_api_use() {
    let data = pattern(256 * KIB, 14);
    let src = Arc::new(FaultySource::builder(data.clone()).build());
    let dir = tempfile::tempdir().unwrap();
    let mut im = Imager::new(
        src,
        &dir.path().join("a.img"),
        &dir.path().join("a.map"),
        test_opts(test_profile()),
    )
    .unwrap();
    assert!(!im.is_resumed());
    assert_eq!(im.geometry().block_size, 64 * KIB);
    assert_eq!(im.domain(), Range::new(0, 256 * KIB));
    let s = im
        .run(diskrescue_imaging::Control::new(), &mut |_| {})
        .unwrap();
    assert!(s.finished);
    assert_eq!(std::fs::read(dir.path().join("a.img")).unwrap(), data);
}
