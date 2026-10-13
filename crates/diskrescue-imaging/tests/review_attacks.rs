//! Adversarial tests written during the review of the imaging engine:
//! misbehaving sources (devices that keep vanishing, nonsense error
//! offsets, absurd sector sizes), output/mapfile aliasing, resume with
//! changed options, an output that must never shrink, and mapfile save
//! failures.

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use diskrescue_core::profile::ImagingProfile;
use diskrescue_core::source::{ReadError, ReadErrorKind, Source};
use diskrescue_core::{Range, SharedSource};
use diskrescue_imaging::simulate::FaultySource;
use diskrescue_imaging::{BlockStatus, Control, Event, Imager, ImagingOptions, MapFile, Summary};

/// Run `imager` with a watchdog that cancels it after `limit`; returns the
/// result and whether the watchdog fired.
fn run_guarded(
    imager: &mut Imager,
    control: Arc<Control>,
    limit: Duration,
    mut on_event: impl FnMut(&Event),
) -> (diskrescue_core::Result<Summary>, bool) {
    let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (c2, f2, d2) = (control.clone(), fired.clone(), done.clone());
    let watchdog = std::thread::spawn(move || {
        let t0 = Instant::now();
        while !d2.load(Ordering::SeqCst) {
            if t0.elapsed() >= limit {
                f2.store(true, Ordering::SeqCst);
                c2.cancel();
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    });
    let r = imager.run(control, &mut |ev: Event| on_event(&ev));
    done.store(true, Ordering::SeqCst);
    watchdog.join().unwrap();
    (r, fired.load(Ordering::SeqCst))
}

/// A device that comes back on every `reopen` but answers every read
/// after the first `good_reads` with `DeviceGone` (the node reappears but
/// the device behind it is dead).
struct Flapping {
    data: Vec<u8>,
    good_reads: u64,
    reads: AtomicU64,
    reopens: AtomicU64,
}

impl Source for Flapping {
    fn size(&self) -> u64 {
        self.data.len() as u64
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, ReadError> {
        let n = self.reads.fetch_add(1, Ordering::SeqCst);
        if n >= self.good_reads {
            return Err(ReadError::new(
                offset,
                buf.len(),
                ReadErrorKind::DeviceGone,
                "gone again",
            ));
        }
        let s = offset as usize;
        let k = buf.len().min(self.data.len() - s);
        buf[..k].copy_from_slice(&self.data[s..s + k]);
        Ok(k)
    }
    fn describe(&self) -> String {
        "flapping device".into()
    }
    fn reopen(&self) -> Result<(), ReadError> {
        self.reopens.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// A device that keeps disappearing right after every successful re-open
/// must not make the engine spin (reopen, save, read, reopen, ...) forever
/// with the usb-flash profile's "wait forever" setting: it gives up with a
/// resumable map and marks nothing bad.
#[test]
fn flapping_device_gives_up_instead_of_spinning() {
    let size = 1024 * KIB;
    let src = Arc::new(Flapping {
        data: pattern(size, 1),
        good_reads: 4,
        reads: AtomicU64::new(0),
        reopens: AtomicU64::new(0),
    });
    let dir = tempfile::tempdir().unwrap();
    let (out, map) = paths(dir.path());
    let mut p = ImagingProfile::usb_flash();
    p.pause_on_error = Duration::ZERO;
    assert!(p.wait_for_reattach.is_none() && p.max_reattaches == 0);
    let mut opts = test_opts(p);
    opts.reattach_poll = Duration::from_millis(5);
    let shared: SharedSource = src.clone();
    let mut im = Imager::new(shared, &out, &map, opts).unwrap();
    let control = Control::new();
    let mut lost = 0u64;
    let (r, fired) = run_guarded(&mut im, control, Duration::from_secs(20), |ev| {
        if matches!(ev, Event::DeviceLost { .. }) {
            lost += 1;
        }
    });
    let s = r.unwrap();
    assert!(
        !fired,
        "engine still spinning after 20 s: {} reads, {} reopens, {lost} losses",
        src.reads.load(Ordering::SeqCst),
        src.reopens.load(Ordering::SeqCst)
    );
    assert!(!s.cancelled && !s.finished, "{s:?}");
    assert!(lost <= 20, "{lost} device losses before giving up");
    let m = MapFile::load(&map).unwrap();
    assert_eq!(m.map.stats().errsize, 0, "nothing may be marked failed");
    // Four good reads; with check_on_error the first one of the run is
    // read twice (verified), so three blocks got rescued.
    assert_eq!(m.map.stats().rescued, 3 * 64 * KIB);
}

/// A device that vanishes exactly during the verification read that
/// check_on_error makes after an error must not get the verified block
/// marked as failed: a vanished device never marks anything bad.
#[test]
fn device_gone_during_a_verification_read_marks_nothing_failed() {
    let size = 512 * KIB;
    let mut data = pattern(size, 2);
    // Block 2 is all zeros: right after the error in block 1 it is
    // verified with a second read, and that read finds the device gone.
    data[(128 * KIB) as usize..(192 * KIB) as usize].fill(0);
    let src = Arc::new(
        FaultySource::builder(data.clone())
            .bad(Range::with_len(64 * KIB, 512))
            // reads: 1 block0, 2 block1 (fails), 3 check of block0,
            // 4 block2 (zeros), 5 = verification of block2 -> gone.
            .vanish(4, 1)
            .build(),
    );
    let mut p = test_profile();
    p.check_on_error = true;
    p.skip_initial = 0;
    p.skip_initial_divisor = 0;
    p.trim = false;
    p.divide = false;
    p.scrape = false;
    p.retry_passes = 0;
    let (run, _dir) = image(&src, test_opts(p));
    assert!(run
        .events
        .iter()
        .any(|e| matches!(e, Event::DeviceLost { .. })));
    // Only block 1 failed; block 2 must be finished with the right bytes.
    assert_eq!(
        run.status_ranges(BlockStatus::NonTrimmed),
        vec![Range::with_len(64 * KIB, 64 * KIB)],
        "{:?}",
        run.map.map.blocks()
    );
    let fin = run.map.map.ranges_with(&[BlockStatus::Finished]);
    for r in fin.iter() {
        assert_bytes_eq(
            &run.output[r.start as usize..r.end as usize],
            &data[r.start as usize..r.end as usize],
        );
    }
}

/// A source whose errors name an offset outside the request (garbage, or
/// the end of the request) must not make the engine read the whole
/// failing block a second time.
struct BadOffsets {
    data: Vec<u8>,
    bad: Range,
    report: u64,
    reads: Mutex<Vec<(u64, usize)>>,
}

impl Source for BadOffsets {
    fn size(&self) -> u64 {
        self.data.len() as u64
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, ReadError> {
        self.reads.lock().unwrap().push((offset, buf.len()));
        let r = Range::with_len(offset, buf.len() as u64);
        if r.overlaps(self.bad) {
            return Err(ReadError::new(
                self.report,
                buf.len(),
                ReadErrorKind::Io,
                "nonsense offset",
            ));
        }
        let s = offset as usize;
        let k = buf.len().min(self.data.len() - s);
        buf[..k].copy_from_slice(&self.data[s..s + k]);
        Ok(k)
    }
    fn describe(&self) -> String {
        "bad offsets".into()
    }
}

#[test]
fn nonsense_error_offsets_do_not_double_read_failed_blocks() {
    for report in [u64::MAX, 128 * KIB, 64 * KIB + 64 * KIB - 1] {
        let size = 256 * KIB;
        let data = pattern(size, 3);
        let src = Arc::new(BadOffsets {
            data: data.clone(),
            bad: Range::with_len(64 * KIB + 4096, 512),
            report,
            reads: Mutex::new(Vec::new()),
        });
        let dir = tempfile::tempdir().unwrap();
        let (out, map) = paths(dir.path());
        let mut p = test_profile();
        p.trim = false;
        p.divide = false;
        p.scrape = false;
        p.retry_passes = 0;
        let shared: SharedSource = src.clone();
        let mut im = Imager::new(shared, &out, &map, test_opts(p)).unwrap();
        let s = im.run(Control::new(), &mut |_| {}).unwrap();
        let reads = src.reads.lock().unwrap().clone();
        let full_block_reads = reads
            .iter()
            .filter(|&&(o, l)| o == 64 * KIB && l as u64 == 64 * KIB)
            .count();
        assert_eq!(
            full_block_reads, 1,
            "report {report}: the failing block was read {full_block_reads} times: {reads:?}"
        );
        assert_eq!(s.stats.non_trimmed, 64 * KIB, "report {report}");
        let output = std::fs::read(&out).unwrap();
        let fin = MapFile::load(&map).unwrap();
        for r in fin.map.ranges_with(&[BlockStatus::Finished]).iter() {
            assert_bytes_eq(
                &output[r.start as usize..r.end as usize],
                &data[r.start as usize..r.end as usize],
            );
        }
    }
}

/// A device reporting an absurd sector size is refused up front instead
/// of making the engine allocate gigabytes per read.
struct HugeSector;

impl Source for HugeSector {
    fn size(&self) -> u64 {
        1 << 40
    }
    fn sector_size(&self) -> u32 {
        u32::MAX
    }
    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> Result<usize, ReadError> {
        panic!("must not be read");
    }
    fn describe(&self) -> String {
        "huge sectors".into()
    }
}

#[test]
fn absurd_sector_size_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (out, map) = paths(dir.path());
    let src: SharedSource = Arc::new(HugeSector);
    let err = Imager::new(src, &out, &map, test_opts(test_profile())).unwrap_err();
    assert!(err.to_string().contains("sector size"), "{err}");
}

/// The engine writes `<map>.tmp`, `<map>.bak`, `<map>.dr.json` and
/// `<map>.dr.json.tmp` next to the mapfile (and deletes the old `.bak`).
/// An output image or a source file at one of those paths would be
/// truncated, replaced or deleted: refuse.
#[test]
fn output_or_source_at_a_mapfile_side_path_is_refused() {
    for suffix in [".tmp", ".bak", ".dr.json", ".dr.json.tmp"] {
        let dir = tempfile::tempdir().unwrap();
        let map = dir.path().join("disk.map");
        let out = dir.path().join(format!("disk.map{suffix}"));
        let data = pattern(256 * KIB, 4);
        let src: SharedSource = Arc::new(FaultySource::builder(data.clone()).build());
        let r = Imager::new(src, &out, &map, test_opts(test_profile()));
        match r {
            Err(e) => assert!(e.to_string().contains("refused"), "{suffix}: {e}"),
            Ok(mut im) => {
                // Show the damage the aliasing does.
                let _ = im.run(Control::new(), &mut |_| {});
                let got = std::fs::read(&out).unwrap_or_default();
                panic!(
                    "output {} accepted; after the run it holds {} bytes, source bytes intact: {}",
                    out.display(),
                    got.len(),
                    got == data
                );
            }
        }
        // A source image file at a side path is refused as well.
        let src_path = dir.path().join(format!("src.map{suffix}"));
        std::fs::write(&src_path, &data).unwrap();
        let fs: SharedSource =
            Arc::new(diskrescue_core::source::FileSource::open(&src_path).unwrap());
        let r = Imager::new(
            fs,
            &dir.path().join("x.img"),
            &dir.path().join("src.map"),
            test_opts(test_profile()),
        );
        assert!(r.is_err(), "source at side path {suffix} accepted");
        assert_eq!(std::fs::read(&src_path).unwrap(), data);
    }
}

/// Resuming with a different `output_offset` would leave every finished
/// block's data at the old offsets while the map says it is rescued: the
/// new offset must be refused.
#[test]
fn resume_with_a_different_output_offset_is_refused() {
    let data = pattern(512 * KIB, 5);
    let src = Arc::new(FaultySource::builder(data).build());
    let dir = tempfile::tempdir().unwrap();
    let mut opts = test_opts(test_profile());
    opts.progress_interval = Duration::ZERO;
    let _ = image_in(dir.path(), &src, opts.clone(), |ev, ctl| {
        if let Event::Progress(p) = ev {
            if p.stats.rescued >= 128 * KIB {
                ctl.cancel();
            }
        }
    });
    let (out, map) = paths(dir.path());
    let mut o2 = opts.clone();
    o2.output_offset = 4096;
    let shared: SharedSource = src.clone();
    let r = Imager::new(shared, &out, &map, o2);
    assert!(r.is_err(), "resume with a changed output_offset accepted");
    // The original offset still resumes.
    let shared: SharedSource = src.clone();
    assert!(Imager::new(shared, &out, &map, opts).is_ok());
}

/// The output is never truncated or shrunk: not when it is longer than
/// the source, not on resume, not with preallocation, and bytes past the
/// rescue's extent are never touched.
#[test]
fn output_is_never_shrunk_or_touched_past_the_extent() {
    let size = 512 * KIB;
    let data = pattern(size, 6);
    let src = Arc::new(
        FaultySource::builder(data.clone())
            .bad(Range::with_len(200 * KIB, 4096))
            .build(),
    );
    let dir = tempfile::tempdir().unwrap();
    let (out, map) = paths(dir.path());
    let big = (size + 300 * KIB) as usize;
    std::fs::write(&out, vec![0xAAu8; big]).unwrap();
    let mut opts = test_opts(test_profile());
    opts.allow_existing_output = true;
    opts.preallocate = true;
    opts.progress_interval = Duration::ZERO;
    for round in 0..20 {
        let shared: SharedSource = src.clone();
        let mut im = Imager::new(shared, &out, &map, opts.clone()).unwrap();
        let control = Control::new();
        let ctl = control.clone();
        let mut seen = 0;
        let s = im
            .run(control, &mut |ev| {
                if let Event::Progress(_) = ev {
                    seen += 1;
                    if seen == 3 + round {
                        ctl.cancel();
                    }
                }
            })
            .unwrap();
        let got = std::fs::read(&out).unwrap();
        assert_eq!(got.len(), big, "round {round}: output length changed");
        assert!(got[size as usize..].iter().all(|&b| b == 0xAA));
        if s.finished {
            let m = MapFile::load(&map).unwrap();
            for b in m.map.iter() {
                let (a, z) = (b.pos as usize, b.end() as usize);
                match b.status {
                    BlockStatus::Finished => assert_bytes_eq(&got[a..z], &data[a..z]),
                    // Never written: the old bytes are still there.
                    _ => assert!(got[a..z].iter().all(|&x| x == 0xAA)),
                }
            }
            return;
        }
    }
    panic!("did not finish");
}

#[cfg(unix)]
fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

#[cfg(unix)]
fn set_mode(p: &std::path::Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// The mapfile's directory turns read-only during the run (stands in for
/// ENOSPC/EIO on the mapfile's filesystem). The engine keeps rescuing,
/// reports the failure, never leaves a map on disk that claims data the
/// output does not hold, keeps the in-memory map so the caller can save it
/// elsewhere, and retries the save sooner than a full save interval.
#[cfg(unix)]
#[test]
fn mapfile_save_failures_lose_nothing() {
    if is_root() {
        eprintln!("skipped: running as root, permissions are not enforced");
        return;
    }
    let size = 2 * MIB;
    let data = pattern(size, 7);
    let src = Arc::new(
        FaultySource::builder(data.clone())
            .bad(Range::with_len(MIB, 4096))
            .slow(Range::new(0, size), Duration::from_millis(2))
            .build(),
    );
    let root = tempfile::tempdir().unwrap();
    let mapdir = root.path().join("maps");
    std::fs::create_dir(&mapdir).unwrap();
    let out = root.path().join("disk.img");
    let map = mapdir.join("disk.map");
    let mut opts = test_opts(test_profile());
    opts.save_interval = Duration::from_secs(3600);
    let shared: SharedSource = src.clone();
    let mut im = Imager::new(shared, &out, &map, opts.clone()).unwrap();
    set_mode(&mapdir, 0o555);
    let mut warnings = 0;
    let r = im.run(Control::new(), &mut |ev| {
        if let Event::Log(log::Level::Warn, m) = &ev {
            if m.contains("could not save the mapfile") {
                warnings += 1;
            }
        }
    });
    set_mode(&mapdir, 0o755);
    assert!(warnings > 0, "save failures must be reported");
    // The final save failed, so the run reports an error...
    let err = r.expect_err("final mapfile save failed; run must not report success");
    assert!(err.to_string().contains("disk.map"), "{err}");
    // ...but the in-memory map is complete and can be saved elsewhere.
    let mem = im.mapfile().clone();
    assert_eq!(mem.map.stats().pending(), 0);
    let alt = root.path().join("emergency.map");
    im.save_mapfile_to(&alt).unwrap();
    assert_eq!(MapFile::load(&alt).unwrap().map, mem.map);
    // The map on disk is the initial one: it claims nothing.
    let on_disk = MapFile::load(&map).unwrap();
    assert_eq!(on_disk.map.stats().rescued, 0);
    // Everything the in-memory map calls finished is in the output.
    let output = std::fs::read(&out).unwrap();
    for b in mem.map.iter().filter(|b| b.status == BlockStatus::Finished) {
        let (a, z) = (b.pos as usize, b.end() as usize);
        assert_bytes_eq(&output[a..z], &data[a..z]);
    }
    // Resuming from the alternative map needs no reads of rescued data.
    std::fs::copy(&alt, &map).unwrap();
    let before = src.read_log_len();
    let shared: SharedSource = src.clone();
    let mut im2 = Imager::new(shared, &out, &map, opts).unwrap();
    let s2 = im2.run(Control::new(), &mut |_| {}).unwrap();
    assert!(s2.finished);
    for r in &src.read_log()[before..] {
        assert!(
            !mem.map
                .ranges_with(&[BlockStatus::Finished])
                .intersects(r.range()),
            "{r:?}"
        );
    }
}

/// A failed periodic save is retried well before the next full save
/// interval.
#[cfg(unix)]
#[test]
fn failed_save_is_retried_soon() {
    if is_root() {
        eprintln!("skipped: running as root, permissions are not enforced");
        return;
    }
    // 512 reads of 20 ms: the run takes over 10 s unless cancelled.
    let size = 32 * MIB;
    let data = pattern(size, 8);
    let src = Arc::new(
        FaultySource::builder(data)
            .slow(Range::new(0, size), Duration::from_millis(20))
            .build(),
    );
    let root = tempfile::tempdir().unwrap();
    let mapdir = root.path().join("maps");
    std::fs::create_dir(&mapdir).unwrap();
    let out = root.path().join("disk.img");
    let map = mapdir.join("disk.map");
    let mut opts = test_opts(test_profile());
    opts.save_interval = Duration::from_secs(3600);
    let shared: SharedSource = src.clone();
    let mut im = Imager::new(shared, &out, &map, opts).unwrap();
    let control = Control::new();
    let ctl = control.clone();
    let md = mapdir.clone();
    // 0 waiting for the pass, 1 read-only + save requested, 2 save failed
    // (writable again), 3 saved again.
    let mut state = 0;
    let mut phase_seen = false;
    let mut failed_at = Instant::now();
    let mut retried_after = None;
    let r = im.run(control, &mut |ev| match ev {
        Event::Phase(_) => phase_seen = true,
        Event::Progress(_) if phase_seen && state == 0 => {
            set_mode(&md, 0o555);
            ctl.request_save();
            state = 1;
        }
        Event::Log(log::Level::Warn, m) if state == 1 && m.contains("could not save") => {
            set_mode(&md, 0o755);
            failed_at = Instant::now();
            state = 2;
        }
        Event::Saved if state == 2 => {
            retried_after = Some(failed_at.elapsed());
            state = 3;
            ctl.cancel();
        }
        _ => {}
    });
    set_mode(&mapdir, 0o755);
    let s = r.unwrap();
    assert_eq!(state, 3, "{s:?}");
    let after = retried_after.unwrap();
    assert!(
        s.cancelled && after < Duration::from_secs(8),
        "a failed save was retried only after {after:?}"
    );
}

/// Calling `run` again on the same Imager after a cancel continues where
/// the first run stopped (same phase, pass and position) instead of
/// starting the copying passes over.
#[test]
fn second_run_on_the_same_imager_resumes() {
    let size = 2 * MIB;
    let data = pattern(size, 9);
    let src = Arc::new(
        FaultySource::builder(data.clone())
            .bad(Range::with_len(MIB, 4096))
            .build(),
    );
    let dir = tempfile::tempdir().unwrap();
    let (out, map) = paths(dir.path());
    let mut opts = test_opts(test_profile());
    opts.progress_interval = Duration::ZERO;
    let shared: SharedSource = src.clone();
    let mut im = Imager::new(shared, &out, &map, opts).unwrap();
    let control = Control::new();
    let ctl = control.clone();
    let s1 = im
        .run(control, &mut |ev| {
            if let Event::Phase(diskrescue_imaging::Phase::Copying { pass: 2, .. }) = ev {
                ctl.cancel();
            }
        })
        .unwrap();
    assert!(s1.cancelled);
    assert_eq!(
        im.mapfile().current_status,
        diskrescue_imaging::CurrentStatus::Copying
    );
    assert_eq!(im.mapfile().current_pass, 2);
    let mut phases = Vec::new();
    let s2 = im
        .run(Control::new(), &mut |ev| {
            if let Event::Phase(p) = ev {
                phases.push(p);
            }
        })
        .unwrap();
    assert!(s2.finished);
    assert!(
        matches!(
            phases.first(),
            Some(diskrescue_imaging::Phase::Copying { pass: 2, .. })
        ),
        "{phases:?}"
    );
    let output = std::fs::read(&out).unwrap();
    assert_bytes_eq(&output, &expected(&data, &ranges(&[(MIB, MIB + 4096)])));
}

/// ddrescue always writes a `# Command line:` header line, even when the
/// command line is empty.
#[test]
fn writer_always_emits_the_command_line_header() {
    let m = MapFile::new(4096);
    let t = m.to_text(
        "DiskRescue",
        "0.1.0",
        None,
        diskrescue_core::time::Timestamp::from_unix(0),
    );
    let lines: Vec<&str> = t.lines().collect();
    assert_eq!(lines[0], "# Mapfile. Created by DiskRescue version 0.1.0");
    assert_eq!(lines[1], "# Command line: ");
    assert_eq!(lines[2], "# Start time:   1970-01-01 00:00:00");
    assert_eq!(lines[3], "# Current time: 1970-01-01 00:00:00");
    assert_eq!(lines[4], "# current_pos  current_status  current_pass");
    assert_eq!(lines[5], "0x00000000     ?               1");
    assert_eq!(lines[6], "#      pos        size  status");
    assert_eq!(lines[7], "0x00000000  0x00001000  ?");
    let back = MapFile::parse(&t).unwrap();
    assert_eq!(back.command_line, None);
    assert_eq!(back.map, m.map);
}

/// Hostile mapfiles: many blocks, very long lines, NUL bytes, numbers at
/// the limits. Parsing never panics, stays fast, and whatever parses
/// round-trips through the writer.
#[test]
fn hostile_mapfiles() {
    // 200 000 alternating blocks (a badly damaged disk) parse quickly,
    // round-trip exactly and survive sub-sector joining for 4 KiB sectors
    // without losing a finished byte.
    let n = 200_000u64;
    let mut t = String::from("# big\n0x0 * 2\n");
    for i in 0..n {
        let st = ['+', '-', '/', '*', '?'][(i % 5) as usize];
        t.push_str(&format!("0x{:08X}  0x00000200  {st}\n", i * 512));
    }
    let t0 = Instant::now();
    let m = MapFile::parse(&t).unwrap();
    assert_eq!(m.map.len() as u64, n);
    let text = m.to_text(
        "t",
        "v",
        None,
        diskrescue_core::time::Timestamp::from_unix(0),
    );
    let back = MapFile::parse(&text).unwrap();
    assert_eq!(back.map, m.map);
    let mut j = m.map.clone();
    j.join_subsectors(4096);
    j.validate().unwrap();
    assert_eq!(
        j.ranges_with(&[BlockStatus::Finished]),
        m.map.ranges_with(&[BlockStatus::Finished])
    );
    assert!(t0.elapsed() < Duration::from_secs(20), "{:?}", t0.elapsed());

    let cases: Vec<String> = vec![
        format!("0 ? 1\n0 {} +\n", "9".repeat(100_000)),
        format!("{}\n", "#".repeat(1 << 20)),
        "0 ? 1\n0\0 0x10 +\n".into(),
        "\0\0\0".into(),
        "0x7FFFFFFFFFFFFFFF ? 1\n0x7FFFFFFFFFFFFFFE 5 +\n".into(),
        "0 ? 1\n0x7FFFFFFFFFFFFFFF 1 +\n".into(),
        "0 ? 2147483648\n".into(),
        "0 ? -1\n".into(),
        "-0 ? 1\n".into(),
        "0 ? 1\n0 1 + extra tokens are ignored\n1 -1 ?\n".into(),
        "0 ?? 1\n".into(),
        "0 ? 1\n0 1Ei +\n0x1000000000000000 7Ei ?\n".into(),
    ];
    for c in &cases {
        if let Ok(m) = MapFile::parse(c) {
            m.map.validate().unwrap();
            let again = MapFile::parse(&m.to_text(
                "t",
                "v",
                None,
                diskrescue_core::time::Timestamp::from_unix(0),
            ))
            .unwrap();
            assert_eq!(again.map, m.map, "{:?}", &c[..c.len().min(60)]);
        }
    }
}

/// A rescue written at an output offset hands the parsers the holes at
/// the offset where the data actually is in the image.
#[test]
fn rescued_image_handoff_honours_the_output_offset() {
    let size = 512 * KIB;
    let off = 64 * KIB;
    let bad = Range::with_len(100 * KIB, 4096);
    let src = Arc::new(FaultySource::builder(pattern(size, 12)).bad(bad).build());
    let mut opts = test_opts(test_profile());
    opts.output_offset = off;
    let (run, _dir) = image(&src, opts);
    assert!(run.summary.finished);
    let img = diskrescue_imaging::open_rescued_image(&run.out_path, &run.map_path).unwrap();
    let holes = img.unreadable().unwrap();
    assert_eq!(
        holes.iter().collect::<Vec<_>>(),
        vec![Range::new(bad.start + off, bad.end + off)]
    );
}

/// ImagingOptions used with every profile must at least construct.
#[test]
fn every_named_profile_constructs() {
    for name in ImagingProfile::names() {
        let p = ImagingProfile::by_name(name).unwrap();
        let o = ImagingOptions::with_profile(p);
        let dir = tempfile::tempdir().unwrap();
        let (out, map) = paths(dir.path());
        let src: SharedSource = Arc::new(FaultySource::builder(pattern(64 * KIB, 1)).build());
        Imager::new(src, &out, &map, o).unwrap();
    }
}
