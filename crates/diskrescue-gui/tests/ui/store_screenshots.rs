//! Screenshots of the desktop app for the Microsoft Store listing: the same
//! five screens as `docs/screenshots`, with the drives, paths and files of a
//! Windows PC, at exactly 1920 x 1080 (a 1745 x 982 window at 110 % scale, so
//! that all four drives of the first screen fit and the text is still larger
//! than on a 1920 x 1080 screen at 100 %).
//!
//! Nothing runs, and nothing is written, unless `DISKRESCUE_STORE_SCREENSHOTS`
//! is set, so that ordinary test runs (and CI) are unchanged. To make the
//! pictures, from the repository root:
//!
//! ```text
//! env -u DISPLAY -u WAYLAND_DISPLAY DISKRESCUE_STORE_SCREENSHOTS=1 \
//!     cargo test -p diskrescue-gui --test ui store_screenshots
//! ```
//!
//! The PNG files go to `docs/store-screenshots/`: `01-drives.png`,
//! `02-wizard-review.png`, `03-progress.png`, `04-results.png` and
//! `05-repair.png`. They tell one story: Alex rescues the 64 GB SD card of a
//! camera (the first three), looks through the pictures that came back, and
//! repairs the four that are damaged.
//!
//! Everything on screen is made up: the drives are described the way the
//! Windows backend describes them (`\\.\PhysicalDriveN`, the disk's own model
//! string, drive letters for the volumes on it), the photos are drawn by
//! `store_images`, and no real device is read. A run on Linux cannot make
//! Windows paths by itself, so a few things differ from a run on Windows
//! (where they cost nothing):
//!
//! * `system::keep_drive_paths` stops `C:\...` from being made absolute below
//!   the current folder (the wizard's summary does that);
//! * the repair page shows `shown_folder` instead of the real output folder;
//! * the system disk gets the Unix mount point `/` as well, because only a
//!   Windows build knows `C:\` as the system drive (the app shows that mount
//!   point nowhere unless the card's "More about this drive" is opened);
//! * the in-use note of a drive with a drive letter is given as a warning of
//!   the drive, with the text a Windows build writes, because the same
//!   sentence made from the drive letter is worded for Linux here.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::common::*;
use crate::store_images::{self, Scene};
use diskrescue_app::jobs::{RecoverState, RecoverStatus};
use diskrescue_app::preflight::Preflight;
use diskrescue_app::JobOutcome;
use diskrescue_core::device::{DeviceInfo, DeviceKind, HealthInfo, Transport};
use diskrescue_core::format::ValidationStatus;
use diskrescue_core::fs::{EntryConfidence, FsKind};
use diskrescue_core::media::{MediaCategory, MediaMeta};
use diskrescue_core::time::Timestamp;
use diskrescue_core::Range;
use diskrescue_gui::progress::{ActiveJob, JobKind, Snapshot};
use diskrescue_gui::{DiskRescueApp, Page};
use diskrescue_imaging::{BlockStatus, Phase, Progress, StatusMap};
use diskrescue_recover::report::{FileRecord, Origin, Outcome, Report, ValidationRecord};
use egui_kittest::kittest::Queryable;
use egui_kittest::Harness;

/// The size of the pictures, and the scale of the window.
const SHOT: (u32, u32) = (1920, 1080);
const SCALE: f32 = 1.1;

/// Where the rescue of the SD card (the story of these screens) saves to.
const RECOVERED: &str = r"C:\Users\Alex\Pictures\Recovered";
const REPAIRED: &str = r"C:\Users\Alex\Pictures\Repaired";
const IMAGE: &str = r"D:\Rescue\card.img";
const TITLE: &str = "Rescue of SD card";

const LIMIT: Duration = Duration::from_secs(30);

/// The pictures are only made when asked for.
fn wanted() -> bool {
    std::env::var_os("DISKRESCUE_STORE_SCREENSHOTS").is_some()
}

fn out_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/store-screenshots")
}

/// The window, in points: its size in pixels (`SHOT`) divided by the scale.
fn window() -> egui::Vec2 {
    egui::vec2(SHOT.0 as f32 / SCALE, SHOT.1 as f32 / SCALE)
}

fn store_harness(app: DiskRescueApp) -> Harness<'static, DiskRescueApp> {
    diskrescue_gui::system::keep_drive_paths(true);
    Harness::builder()
        .with_size(window())
        .with_pixels_per_point(SCALE)
        .with_theme(egui::Theme::Light)
        .with_max_steps(64)
        .build_eframe(move |_cc| app)
}

/// True when some text on the screen contains `text` (unlike
/// `has_label_containing`, several places may).
fn shows(h: &Harness<'_, DiskRescueApp>, text: &str) -> bool {
    h.query_all_by_label_contains(text).next().is_some()
}

/// No path or name of a Linux system may be on the screen.
fn assert_windows_only(h: &Harness<'_, DiskRescueApp>) {
    for bad in [
        "/dev/", "/media/", "/home/", "/tmp/", "/mnt/", "/run/", "/target/", ".tmp",
    ] {
        assert!(!shows(h, bad), "{bad:?} is on the screen");
    }
}

/// Render the last frame and save it as `docs/store-screenshots/<name>.png`
/// (true colour: these are not kept in the repository's history).
fn store_shot(mut h: Harness<'static, DiskRescueApp>, name: &str) {
    let _g = render_lock();
    assert_windows_only(&h);
    h.remove_cursor();
    for _ in 0..3 {
        h.step();
    }
    let img = h.render().expect("headless rendering");
    assert_eq!((img.width(), img.height()), SHOT, "{name}");
    let dir = out_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let file =
        std::io::BufWriter::new(std::fs::File::create(dir.join(format!("{name}.png"))).unwrap());
    let mut enc = png::Encoder::new(file, img.width(), img.height());
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::Best);
    enc.set_adaptive_filter(png::AdaptiveFilterType::Adaptive);
    let rgb: Vec<u8> = img
        .as_raw()
        .chunks_exact(4)
        .flat_map(|p| [p[0], p[1], p[2]])
        .collect();
    enc.write_header().unwrap().write_image_data(&rgb).unwrap();
    drop(h);
}

// ------------------------------------------------------------------ drives

fn physical_drive(
    n: u32,
    kind: DeviceKind,
    transport: Transport,
    vendor: &str,
    model: &str,
    size: u64,
) -> DeviceInfo {
    DeviceInfo {
        path: PathBuf::from(format!(r"\\.\PhysicalDrive{n}")),
        name: format!("PhysicalDrive{n}"),
        kind,
        transport,
        size,
        logical_sector: 512,
        physical_sector: if kind.is_rotational() { 4096 } else { 512 },
        model: (!model.is_empty()).then(|| model.to_string()),
        vendor: (!vendor.is_empty()).then(|| vendor.to_string()),
        serial: None,
        firmware: None,
        stable_id: None,
        removable: kind.is_usb() && !kind.is_rotational() || kind.is_card(),
        rotational: kind.is_rotational(),
        read_only: false,
        partitions: vec![],
        mountpoints: vec![],
        health: None,
        warnings: vec![],
    }
}

/// What a Windows build says about a volume that is in use (the same text as
/// its drive card when the volume has a drive letter).
fn in_use(letter: &str) -> String {
    format!(
        "Windows uses it as {letter}. Take it out of use before copying it, so that Windows does \
         not write to it: in Disk Management, choose Offline for the disk (for a USB stick or \
         card, remove the drive letter of its volume)."
    )
}

/// The drives of a Windows PC, in the order of their paths: the system SSD
/// (C:), a USB stick (E:), the SD card of a camera in a card reader (F:) and
/// a failing USB hard disk (G:).
fn windows_devices() -> Vec<DeviceInfo> {
    use DeviceKind::*;
    let mut system = physical_drive(
        0,
        SataSsd,
        Transport::Ata,
        "",
        "Samsung SSD 870 EVO 500GB",
        500_107_862_016,
    );
    system.mountpoints = vec![PathBuf::from(r"C:\")];
    if cfg!(not(windows)) {
        // Only a Windows build counts `C:\` as the system drive.
        system.mountpoints.push(PathBuf::from("/"));
    }

    let mut stick = physical_drive(
        1,
        UsbFlash,
        Transport::Usb,
        "SanDisk",
        "Ultra USB 3.0",
        32_017_047_552,
    );
    stick.warnings = vec![in_use(r"E:\")];

    // A card reader that gives no name: the card is described by its kind.
    let mut card = physical_drive(2, SdCard, Transport::Mmc, "", "", 63_864_569_856);
    card.warnings = vec![in_use(r"F:\")];

    let mut hdd = physical_drive(
        3,
        UsbHdd,
        Transport::Usb,
        "WD",
        "Elements 25A2",
        1_000_204_886_016,
    );
    hdd.health = Some(HealthInfo {
        passed: Some(false),
        reallocated_sectors: Some(310),
        pending_sectors: Some(24),
        ..HealthInfo::default()
    });
    vec![system, stick, card, hdd]
}

#[test]
fn store_screenshots_01_drives() {
    if !wanted() {
        return;
    }
    let (app, _) = fake_app(windows_devices());
    let mut h = store_harness(app);
    wait_for(&mut h, LIMIT, "the drive list", |h| {
        has_label(h, "Rescue SD card")
    });
    assert!(shows(&h, r"\\.\PhysicalDrive3"));
    assert!(has_label(&h, "Failing"));
    assert!(has_label(&h, "This is the disk your computer runs from"));
    assert!(!enabled(&h, "Rescue Samsung SSD 870 EVO 500GB"));
    store_shot(h, "01-drives");
}

// ------------------------------------------------------------------ wizard

#[test]
fn store_screenshots_02_wizard_review() {
    if !wanted() {
        return;
    }
    let (app, backend) = fake_app(windows_devices());
    // Every check passes.
    *backend.preflight.lock().unwrap() = Some(Preflight {
        blockers: vec![],
        warnings: vec![],
        resolved: None,
    });
    let mut h = store_harness(app);
    wait_for(&mut h, LIMIT, "the drive list", |h| {
        has_label(h, "Rescue SD card")
    });
    click(&mut h, "Rescue SD card");
    assert_eq!(h.state().page, Page::Wizard);
    {
        let w = h.state_mut().wizard.as_mut().unwrap();
        w.output = RECOVERED.into();
        w.image_path = IMAGE.into();
        w.step = 2;
    }
    wait_for(&mut h, LIMIT, "the safety checks", |h| {
        enabled(h, "Start rescue")
    });
    assert!(has_label(&h, "Everything is ready"));
    assert!(shows(&h, RECOVERED));
    store_shot(h, "02-wizard-review");
}

// ---------------------------------------------------------------- progress

/// The copy of the 64 GB card, two thirds done, with a few bad areas.
fn card_snapshot() -> Snapshot {
    let mut snap = diskrescue_gui::progress::preview_snapshot();
    let mut img = snap.image.take().expect("the preview copies a drive");
    let size: u64 = 63_864_569_856;
    let mb = 1u64 << 20;
    let mut map = StatusMap::new(size);
    map.set(Range::new(0, 33_800 * mb), BlockStatus::Finished);
    map.set(Range::new(7_700 * mb, 7_764 * mb), BlockStatus::NonTrimmed);
    map.set(Range::new(14_300 * mb, 14_301 * mb), BlockStatus::BadSector);
    map.set(
        Range::new(21_000 * mb, 21_090 * mb),
        BlockStatus::NonScraped,
    );
    map.set(Range::new(21_090 * mb, 21_092 * mb), BlockStatus::BadSector);
    map.set(Range::new(41_000 * mb, 43_900 * mb), BlockStatus::Finished);
    map.set(Range::new(56_906 * mb, size), BlockStatus::Finished);
    let stats = map.stats();
    let mut info = img.source.take().expect("a source");
    info.path = PathBuf::from(r"\\.\PhysicalDrive2");
    info.name = "PhysicalDrive2".into();
    info.kind = DeviceKind::SdCard;
    info.transport = Transport::Mmc;
    info.size = size;
    info.model = None;
    info.vendor = None;
    img.source = Some(info);
    img.image = PathBuf::from(IMAGE);
    img.mapfile = PathBuf::from(format!("{IMAGE}.map"));
    img.copy_size = size;
    img.progress = Some(Progress {
        phase: Phase::Copying {
            pass: 1,
            reverse: false,
        },
        stats,
        current_pos: 33_800 * mb,
        current_rate: 22.3e6,
        average_rate: 20.9e6,
        elapsed: Duration::from_secs(28 * 60 + 31),
        eta: Some(Duration::from_secs(16 * 60 + 40)),
        errors: 5,
        slow_reads: 3,
        timeouts: 1,
        reattaches: 0,
        last_error: Some("read error at 14.3 GB: Data error (cyclic redundancy check)".into()),
        size,
    });
    img.map = Some(Arc::new(map));
    snap.recover.as_mut().expect("a recovery").image = Some(img.clone());
    snap.image = Some(img);
    snap
}

#[test]
fn store_screenshots_03_progress() {
    if !wanted() {
        return;
    }
    let (app, _) = fake_app(windows_devices());
    let mut h = store_harness(app);
    h.state_mut().show_preview_progress(TITLE, card_snapshot());
    {
        let job = h.state_mut().job.as_mut().unwrap();
        job.output = PathBuf::from(RECOVERED);
        job.image_path = Some(PathBuf::from(IMAGE));
    }
    h.step();
    h.step();
    assert_eq!(h.state().page, Page::Progress);
    assert!(shows(&h, "Saving to C:"));
    store_shot(h, "03-progress");
}

// ----------------------------------------------------------------- results

/// What the rescued card holds, in the order of the pictures on it.
struct Roll {
    count: usize,
}

impl Roll {
    /// Canon numbers stills and movies together.
    fn number(i: usize) -> usize {
        4801 + i
    }

    fn is_video(i: usize) -> bool {
        i % 13 == 5
    }

    fn is_deleted(i: usize) -> bool {
        i % 14 == 10
    }

    /// Duplicates and files that could not be read, far down the list.
    fn is_duplicate(i: usize) -> bool {
        i > 200 && i % 207 == 0
    }

    fn is_failed(i: usize) -> bool {
        i == 777 || i == 1105
    }

    fn is_damaged(i: usize) -> bool {
        matches!(i, 9 | 18 | 20 | 31)
    }

    fn name(i: usize) -> String {
        if Self::is_video(i) {
            format!("MVI_{}.MP4", Self::number(i))
        } else {
            format!("IMG_{}.JPG", Self::number(i))
        }
    }

    fn portrait(i: usize) -> bool {
        i % 9 == 4
    }
}

/// The pictures that are really on disk: a screenful and a bit more.
const DRAWN: usize = 48;

/// Draw and write the first pictures (in parallel: drawing is the slow part).
fn write_photos(dir: &Path, roll: &Roll) {
    let todo: Vec<usize> = (0..roll.count.min(DRAWN * 2))
        .filter(|&i| !Roll::is_video(i))
        .take(DRAWN)
        .collect();
    let threads = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .min(8);
    std::thread::scope(|s| {
        for chunk in todo.chunks(todo.len().div_ceil(threads)) {
            s.spawn(move || {
                for &i in chunk {
                    let (w, h) = if Roll::portrait(i) {
                        (320, 480)
                    } else {
                        (480, 320)
                    };
                    let scene = Scene::ALL[i % Scene::ALL.len()];
                    let rgb =
                        store_images::photo(scene, (i / 10 + 1) as u32 + (i as u32) * 7, w, h);
                    let jpg = store_images::jpeg(&rgb, w, h, 88);
                    std::fs::write(dir.join(Roll::name(i)), jpg).unwrap();
                }
            });
        }
    });
}

fn record(i: usize, deleted_override: bool) -> FileRecord {
    let name = Roll::name(i);
    let video = Roll::is_video(i);
    let jitter = (i as i64 * 7919) % 1100;
    let taken = Timestamp::from_unix(1_774_689_240 + i as i64 * 1_500 + jitter);
    let deleted = Roll::is_deleted(i) || deleted_override;
    let size = if video {
        (60 + (i as u64 * 37) % 700) << 20
    } else {
        (5_500_000 + (i as u64 * 104_729) % 5_800_000) + 1_000_000
    };
    let mut rec = FileRecord::new(
        Origin::Filesystem {
            volume: 1,
            fs: FsKind::ExFat,
            path: format!("DCIM/100CANON/{name}"),
            deleted,
            confidence: if deleted {
                EntryConfidence::DeletedIntact
            } else {
                EntryConfidence::Allocated
            },
        },
        if video {
            MediaCategory::Video
        } else {
            MediaCategory::Photo
        },
        size,
    );
    rec.output = Some(PathBuf::from(&name));
    rec.format = Some(if video { "mp4" } else { "jpeg" }.into());
    rec.outcome = Outcome::Recovered;
    rec.meta = MediaMeta {
        make: Some("Canon".into()),
        model: Some("Canon EOS R10".into()),
        created: Some(taken),
        ..MediaMeta::default()
    };
    if video {
        rec.meta.width = Some(3840);
        rec.meta.height = Some(2160);
        rec.meta.duration_ms = Some(9_000 + (i as u64 * 4_333) % 110_000);
        rec.meta.video_codec = Some("h264".into());
        rec.meta.audio_codec = Some("aac".into());
    } else {
        let (w, h) = if Roll::portrait(i) {
            (4000, 6000)
        } else {
            (6000, 4000)
        };
        rec.meta.width = Some(w);
        rec.meta.height = Some(h);
        rec.meta.orientation = Some(1);
    }
    if deleted {
        rec.notes.push(
            "deleted file: its location on disk was reconstructed and may be incomplete if it was \
             fragmented"
                .into(),
        );
    }
    if Roll::is_damaged(i) {
        rec.outcome = Outcome::Damaged;
        rec.validation = Some(ValidationRecord {
            status: ValidationStatus::Damaged,
            intact_fraction: Some(0.6),
            problems: vec![if video {
                "the index of the video (moov) is missing, so it will not play".into()
            } else {
                "the picture data ends early: the rest of the picture is missing".into()
            }],
        });
    }
    rec
}

/// The report of the rescue of the camera's card: 1,240 pictures and movies.
fn card_report() -> Report {
    let mut report = Report::new(None);
    let count = 1_240;
    for i in 0..count {
        let mut rec = record(i, false);
        if Roll::is_duplicate(i) {
            rec.outcome = Outcome::Duplicate;
            rec.output = None;
        }
        if Roll::is_failed(i) {
            rec.outcome = Outcome::Failed;
            rec.output = None;
        }
        report.add_file(rec);
    }
    report.finalize();
    report
}

/// The banner of a finished rescue.
fn finished_job() -> ActiveJob {
    let snap = Snapshot {
        outcome: Some(JobOutcome::Finished),
        recover: Some(RecoverState {
            status: RecoverStatus::Done,
            ..RecoverState::default()
        }),
        ..Snapshot::default()
    };
    ActiveJob {
        title: TITLE.into(),
        output: PathBuf::from(RECOVERED),
        image_path: Some(PathBuf::from(IMAGE)),
        kind: JobKind::Preview(Box::new(snap)),
        confirm_stop: false,
        results_taken: true,
    }
}

#[test]
fn store_screenshots_04_results() {
    if !wanted() {
        return;
    }
    let report = card_report();
    let roll = Roll {
        count: report.files.len(),
    };
    let dir = scratch("store-results-");
    write_photos(dir.path(), &roll);

    let (app, _) = fake_app(windows_devices());
    let mut h = store_harness(app);
    h.state_mut().job = Some(finished_job());
    h.state_mut()
        .results
        .set_report(dir.path(), Arc::new(report));
    h.state_mut().go(Page::Results);
    // A deleted photo that was recovered whole.
    let chosen = (0..roll.count)
        .find(|&i| Roll::is_deleted(i) && !Roll::is_video(i))
        .unwrap();
    h.state_mut().results.selected = Some(chosen);
    h.state_mut().results.shown_folder = Some(RECOVERED.into());
    h.step();
    h.step();
    wait_for(&mut h, LIMIT, "the thumbnails", |h| {
        !h.state().thumbnails_busy()
    });
    assert!(shows(&h, &Roll::name(chosen)));
    assert!(shows(&h, "(it had been deleted)"));
    assert!(shows(&h, "Videos ("));
    store_shot(h, "04-results");
}

// ------------------------------------------------------------------ repair

#[test]
fn store_screenshots_05_repair() {
    if !wanted() {
        return;
    }
    let Some(sc) = damaged_media() else {
        eprintln!("skipped: the repair screen needs the damaged-media fixture");
        return;
    };
    let dir = scratch("store-repair-");
    let damaged = dir.path().join("Damaged");
    let good = dir.path().join("Good");
    std::fs::create_dir_all(&damaged).unwrap();
    std::fs::create_dir_all(&good).unwrap();
    // The damaged files of the rescue, under the names of the card.
    let mut inputs: Vec<PathBuf> = Vec::new();
    for (from, to) in [
        ("zeroed_header.jpg", "IMG_4810.JPG"),
        ("no_moov.mp4", "MVI_4819.MP4"),
        ("truncated.jpg", "IMG_4821.JPG"),
        ("truncated.mp4", "MVI_4832.MP4"),
    ] {
        let p = damaged.join(to);
        std::fs::copy(sc.root.join(from), &p).unwrap();
        inputs.push(p);
    }
    let mut donors: Vec<PathBuf> = Vec::new();
    for (from, to) in [("donor.jpg", "IMG_4802.JPG"), ("donor.mp4", "MVI_4806.MP4")] {
        let p = good.join(to);
        std::fs::copy(sc.root.join("donors").join(from), &p).unwrap();
        donors.push(p);
    }
    let out = dir.path().join("Repaired");

    let (app, _) = fake_app(windows_devices());
    let mut h = store_harness(app);
    {
        let r = &mut h.state_mut().repair;
        r.add_files(inputs.clone());
        r.add_donors(donors.clone());
        r.start(inputs, donors, out);
    }
    h.state_mut().go(Page::Repair);
    wait_for(&mut h, Duration::from_secs(300), "the repair", |h| {
        shows(h, "Done: ")
    });
    assert!(shows(&h, "Done: "));
    {
        let r = &mut h.state_mut().repair;
        r.shown_folder = Some(REPAIRED.into());
        r.output = REPAIRED.into();
        // The lists below the results name folders of this Linux machine.
        r.files.clear();
        r.donors.clear();
    }
    h.step();
    h.step();
    assert!(shows(&h, REPAIRED));
    store_shot(h, "05-repair");
}
