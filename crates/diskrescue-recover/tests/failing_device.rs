//! The whole workflow on a simulated dying pen drive: image it with the
//! USB-flash profile (a dead area, a flaky area, the device dropping off the
//! bus, stale data after errors), then recover from the rescued image and
//! compare every file with the ground truth.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use diskrescue_core::profile::ImagingProfile;
use diskrescue_core::ranges::{Range, RangeSet};
use diskrescue_core::source::FileSource;
use diskrescue_core::{SharedSource, Source};
use diskrescue_fixtures::sets::{self, Recoverability, Scenario};
use diskrescue_imaging::simulate::FaultySource;
use diskrescue_imaging::{
    bad_ranges_of, open_rescued_image, Control, Event, Imager, ImagingOptions,
};
use diskrescue_recover::report::{Origin, Outcome, SourceRecord};
use diskrescue_recover::{recover, NoopObserver, RecoverOptions};

fn build(dir: &Path) -> Option<Scenario> {
    match sets::camera_sd_card(dir) {
        Ok(s) => Some(s),
        Err(e) if e.is_missing() => {
            eprintln!("skipped: {e}");
            None
        }
        Err(e) => panic!("fixture: {e}"),
    }
}

fn file_extent(scn: &Scenario, suffix: &str) -> (u64, u64) {
    let t = scn
        .files
        .iter()
        .find(|t| t.path.ends_with(suffix))
        .expect("file in scenario");
    t.extents[0]
}

#[test]
fn dying_pen_drive_image_then_recover() {
    let dir = tempfile::tempdir().unwrap();
    let Some(scn) = build(&dir.path().join("scn")) else {
        return;
    };
    let disk = scn.disk_image.clone().unwrap();

    // Faults: a dead 4 KiB area inside IMG_0005, a flaky 256 KiB area over
    // IMG_0006 that usually reads on retry, the drive vanishing (twice per
    // loss) and a bridge that returns zeros right after an error.
    let (bad_file_start, bad_file_len) = file_extent(&scn, "IMG_0005.JPG");
    let dead = Range::with_len(bad_file_start + (bad_file_len / 2) / 4096 * 4096, 4096);
    let (flaky_start, _) = file_extent(&scn, "IMG_0006.JPG");
    let flaky = Range::with_len(flaky_start / 4096 * 4096, 256 * 1024);
    let faulty = Arc::new(
        FaultySource::builder_file(FileSource::open(&disk).unwrap())
            .name("simulated USB flash drive")
            .bad(dead)
            .flaky(flaky, 0.3, 7)
            .vanish(500, 2)
            .stale_after_error(true)
            .build(),
    );

    let mut profile = ImagingProfile::usb_flash();
    profile.retry_passes = 4;
    profile.pause_on_error = Duration::ZERO;
    profile.pause_after_errors = None;
    let mut opts = ImagingOptions::with_profile(profile);
    opts.reattach_poll = Duration::from_millis(10);
    let image = dir.path().join("card.img");
    let map = dir.path().join("card.img.map");
    let src: SharedSource = faulty.clone();
    let mut imager = Imager::new(src, &image, &map, opts).expect("imager");
    let mut lost = 0;
    let mut back = 0;
    let summary = imager
        .run(Control::new(), &mut |e| match e {
            Event::DeviceLost { .. } => lost += 1,
            Event::DeviceBack { .. } => back += 1,
            _ => {}
        })
        .expect("imaging");
    assert!(summary.finished, "imaging finished: {summary:?}");
    assert!(
        lost >= 1 && lost == back,
        "device vanished and came back ({lost}/{back})"
    );

    // Bad sectors: the whole dead area, plus at most a few flaky sectors that
    // failed every attempt; nothing anywhere else.
    let bad_map = bad_ranges_of(&map).expect("mapfile");
    assert!(
        bad_map.covers(dead),
        "the dead area is marked bad: {bad_map:?}"
    );
    let allowed = RangeSet::from_ranges([dead, flaky]);
    assert_eq!(
        bad_map.intersection(&allowed),
        bad_map,
        "bad sectors only where the faults are: {bad_map:?}"
    );
    assert!(
        bad_map.total_len() <= dead.len() + flaky.len() / 8,
        "retries recover most of the flaky area: {:?}",
        summary.stats
    );

    // The rescued image differs from the original only in sectors reported bad
    // (stale bridge data must never have been accepted).
    let orig = std::fs::read(&disk).unwrap();
    let got = std::fs::read(&image).unwrap();
    assert_eq!(orig.len(), got.len());
    let differs_outside = orig
        .iter()
        .zip(&got)
        .enumerate()
        .any(|(i, (a, b))| a != b && !bad_map.contains(i as u64));
    assert!(!differs_outside, "only unreadable sectors may differ");
    drop((orig, got));

    // Recover from the image; the mapfile tells the parsers where the holes are.
    let rescued: SharedSource =
        Arc::new(open_rescued_image(&image, &map).expect("open rescued image"));
    assert_eq!(
        rescued.unreadable().map(|u| u.total_len()),
        Some(bad_map.total_len())
    );
    let out = dir.path().join("out");
    let mut ropts = RecoverOptions::new(&out);
    ropts.workers = 4;
    let source = SourceRecord {
        description: "card.img".into(),
        size: rescued.size(),
        mapfile: Some(map.clone()),
        ..Default::default()
    };
    let report = recover(rescued, source, &ropts, &NoopObserver).expect("recover");
    assert_eq!(report.source.unreadable_bytes, bad_map.total_len());

    for t in scn
        .files
        .iter()
        .filter(|t| matches!(t.kind.as_str(), "jpeg" | "mp4" | "cr2"))
    {
        if !matches!(
            t.expect,
            Recoverability::Live | Recoverability::DeletedIntact
        ) {
            continue;
        }
        let lost: u64 = t
            .extents
            .iter()
            .map(|&(o, l)| {
                bad_map
                    .intersection_with_range(Range::with_len(o, l))
                    .total_len()
            })
            .sum();
        if lost > 0 {
            let rec = report
                .files
                .iter()
                .find(|f| matches!(&f.origin, Origin::Filesystem { path, .. } if path == &t.path))
                .unwrap_or_else(|| panic!("{} still listed", t.path));
            assert_eq!(rec.unreadable_bytes, lost, "{rec:?}");
            assert_ne!(
                rec.outcome,
                Outcome::Recovered,
                "a file with unreadable sectors is not reported intact"
            );
            assert!(rec.output.is_some(), "the readable part is still written");
            continue;
        }
        assert!(
            report
                .files
                .iter()
                .any(|f| f.blake3.as_deref() == Some(t.blake3.as_str()) && f.output.is_some()),
            "{} recovered byte for byte",
            t.path
        );
    }
}
