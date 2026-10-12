//! Preflight checks: every blocker the command line refuses with, with its
//! code and override. Sources are regular files only.

mod common;

use std::path::PathBuf;

use diskrescue_app::imaging::{identity_path, write_identity};
use diskrescue_app::plan::{ImagePlan, RecoverPlan, RepairPlan, ScanPlan, VerifyPlan};
use diskrescue_app::preflight::{
    preflight_image, preflight_recover, preflight_repair, preflight_scan, preflight_verify,
    BlockerCode, Override,
};
use diskrescue_core::device::{DeviceInfo, DeviceKind, Transport};
use diskrescue_device::Fingerprint;

fn source_file(dir: &std::path::Path, len: usize) -> PathBuf {
    let p = dir.join("source.bin");
    std::fs::write(&p, common::noise(len, 7)).unwrap();
    p
}

/// A ddrescue mapfile: `rescued` bytes copied, the rest not tried.
fn mapfile(total: u64, rescued: u64) -> String {
    let mut s = String::from(
        "# Mapfile. Created by test\n# current_pos  current_status  current_pass\n0x00000000  ?  1\n#      pos        size  status\n",
    );
    if rescued > 0 {
        s.push_str(&format!("0x{:08X}  0x{:08X}  +\n", 0, rescued));
    }
    if total > rescued {
        s.push_str(&format!(
            "0x{:08X}  0x{:08X}  ?\n",
            rescued,
            total - rescued
        ));
    }
    s
}

#[test]
fn a_clean_plan_is_clear_and_resolved() {
    let dir = tempfile::tempdir().unwrap();
    let src = source_file(dir.path(), 1 << 20);
    let pf = preflight_image(&ImagePlan::new(&src, dir.path().join("out/copy.img")));
    assert!(pf.is_clear(), "{:?}", pf.blockers);
    let r = pf.resolved.unwrap();
    assert_eq!(r.source.kind, DeviceKind::ImageFile);
    assert_eq!(r.copy_size, 1 << 20);
    assert_eq!(r.profile.unwrap().name, "generic");
    assert!(!r.already_complete && r.resuming.is_none());
    assert_eq!(r.mapfile, dir.path().join("out/copy.img.map"));
}

#[test]
fn the_image_cannot_be_the_source() {
    let dir = tempfile::tempdir().unwrap();
    let src = source_file(dir.path(), 4096);
    let pf = preflight_image(&ImagePlan::new(&src, &src));
    let b = pf
        .blockers
        .iter()
        .find(|b| b.code == BlockerCode::ImageIsSource)
        .expect("image == source must block");
    assert_eq!(b.override_flag, None);
    assert!(b.message.contains("the source itself"), "{}", b.message);
    // The same plan with every override set is still refused.
    let mut plan = ImagePlan::new(&src, &src);
    plan.force = true;
    plan.allow_mounted = true;
    plan.no_space_check = true;
    assert!(preflight_image(&plan).has(BlockerCode::ImageIsSource));
}

#[test]
fn a_folder_is_not_an_image() {
    let dir = tempfile::tempdir().unwrap();
    let src = source_file(dir.path(), 4096);
    let pf = preflight_image(&ImagePlan::new(&src, dir.path()));
    let b = pf
        .blockers
        .iter()
        .find(|b| b.code == BlockerCode::ImageIsFolder)
        .expect("a folder must block");
    assert!(b.message.contains("is a folder"), "{}", b.message);
    assert_eq!(b.override_flag, None);
}

#[test]
fn an_existing_image_without_a_map_needs_force() {
    let dir = tempfile::tempdir().unwrap();
    let src = source_file(dir.path(), 4096);
    let img = dir.path().join("existing.img");
    std::fs::write(&img, b"precious data").unwrap();
    let mut plan = ImagePlan::new(&src, &img);
    let pf = preflight_image(&plan);
    let b = pf
        .blockers
        .iter()
        .find(|b| b.code == BlockerCode::ImageExists)
        .expect("an existing image must block");
    assert_eq!(b.override_flag, Some(Override::Force));
    assert!(b.message.contains("will not overwrite"), "{}", b.message);
    plan.force = true;
    let pf = preflight_image(&plan);
    assert!(pf.is_clear(), "{:?}", pf.blockers);
    assert!(pf.warnings.iter().any(|w| w.contains("will be replaced")));
    // Preflight never writes.
    assert_eq!(std::fs::read(&img).unwrap(), b"precious data");
}

#[test]
fn a_map_from_another_device_needs_force() {
    let dir = tempfile::tempdir().unwrap();
    let src = source_file(dir.path(), 1 << 20);
    let img = dir.path().join("card.img");
    std::fs::write(&img, vec![0u8; 1 << 20]).unwrap();
    let map = dir.path().join("card.img.map");
    std::fs::write(&map, mapfile(1 << 20, 4096)).unwrap();
    // The sidecar says the copy started on a 32 GB card with a serial number.
    let other = DeviceInfo {
        path: PathBuf::from("/dev/sdb"),
        name: "sdb".into(),
        kind: DeviceKind::SdCard,
        transport: Transport::Usb,
        size: 32_000_000_000,
        logical_sector: 512,
        physical_sector: 512,
        model: Some("Ultra".into()),
        vendor: Some("SanDisk".into()),
        serial: Some("SERIAL-ONE-1234".into()),
        firmware: None,
        stable_id: None,
        removable: true,
        rotational: false,
        read_only: false,
        partitions: vec![],
        mountpoints: vec![],
        health: None,
        warnings: vec![],
    };
    write_identity(&identity_path(&map), &Fingerprint::of(&other, None));
    let mut plan = ImagePlan::new(&src, &img);
    let pf = preflight_image(&plan);
    let b = pf
        .blockers
        .iter()
        .find(|b| b.code == BlockerCode::DeviceChanged)
        .expect("a different device must block");
    assert_eq!(b.override_flag, Some(Override::Force));
    assert!(b.message.contains("different device"), "{}", b.message);
    assert_eq!(pf.resolved.unwrap().resuming.unwrap().rescued, 4096);
    plan.force = true;
    let pf = preflight_image(&plan);
    assert!(!pf.has(BlockerCode::DeviceChanged), "{:?}", pf.blockers);
    assert!(pf.warnings.iter().any(|w| w.contains("different device")));
}

#[test]
fn map_problems_block() {
    let dir = tempfile::tempdir().unwrap();
    let src = source_file(dir.path(), 4096);
    // Map and image are the same file.
    let img = dir.path().join("a.img");
    let mut plan = ImagePlan::new(&src, &img);
    plan.map = Some(img.clone());
    assert!(preflight_image(&plan).has(BlockerCode::MapIsImage));
    // An unreadable map.
    let img = dir.path().join("b.img");
    std::fs::write(dir.path().join("b.img.map"), "not a mapfile\n").unwrap();
    assert!(preflight_image(&ImagePlan::new(&src, &img)).has(BlockerCode::MapUnreadable));
    // A map that records copied data without its image.
    let img = dir.path().join("c.img");
    std::fs::write(dir.path().join("c.img.map"), mapfile(4096, 2048)).unwrap();
    assert!(preflight_image(&ImagePlan::new(&src, &img)).has(BlockerCode::ImageMissing));
}

#[test]
fn source_problems_block() {
    let dir = tempfile::tempdir().unwrap();
    let pf = preflight_image(&ImagePlan::new(
        dir.path().join("nothing.img"),
        dir.path().join("x.img"),
    ));
    assert!(pf.has(BlockerCode::SourceMissing), "{:?}", pf.blockers);
    assert!(pf.resolved.is_none());
    let pf = preflight_image(&ImagePlan::new(dir.path(), dir.path().join("x.img")));
    assert!(pf.has(BlockerCode::SourceIsFolder), "{:?}", pf.blockers);
    let empty = dir.path().join("empty.img");
    std::fs::write(&empty, b"").unwrap();
    let pf = preflight_image(&ImagePlan::new(&empty, dir.path().join("x.img")));
    assert!(pf.has(BlockerCode::ZeroCapacity), "{:?}", pf.blockers);
}

#[cfg(unix)]
#[test]
fn an_unreadable_source_needs_privileges() {
    use std::os::unix::fs::PermissionsExt;
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipped: running as root");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let src = source_file(dir.path(), 4096);
    std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o000)).unwrap();
    assert!(diskrescue_app::helper::needs_privilege(&src));
    let pf = preflight_image(&ImagePlan::new(&src, dir.path().join("x.img")));
    let b = pf
        .blockers
        .iter()
        .find(|b| b.code == BlockerCode::PermissionDenied)
        .expect("permission denied must block");
    assert!(
        b.message.contains("sudo") || b.message.contains("ls -l"),
        "{}",
        b.message
    );
    std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(!diskrescue_app::helper::needs_privilege(&src));
}

#[test]
fn options_out_of_range_block() {
    let dir = tempfile::tempdir().unwrap();
    let src = source_file(dir.path(), 4096);
    let mut plan = ImagePlan::new(&src, dir.path().join("x.img"));
    plan.start = Some(1 << 20);
    assert!(preflight_image(&plan).has(BlockerCode::InvalidOption));
    let mut plan = ImagePlan::new(&src, dir.path().join("x.img"));
    plan.tuning.profile = Some("warp-drive".into());
    assert!(preflight_image(&plan).has(BlockerCode::InvalidOption));
}

#[test]
fn too_little_room_needs_the_space_check_off() {
    let dir = common::scratch("space-");
    // A sparse source far larger than any test disk.
    let src = dir.path().join("huge.bin");
    let f = std::fs::File::create(&src).unwrap();
    if f.set_len(8 << 40).is_err() {
        eprintln!("skipped: the filesystem cannot hold an 8 TiB sparse file");
        return;
    }
    drop(f);
    let mut plan = ImagePlan::new(&src, dir.path().join("huge.img"));
    let pf = preflight_image(&plan);
    let b = pf
        .blockers
        .iter()
        .find(|b| b.code == BlockerCode::NotEnoughSpace)
        .expect("too little room must block");
    assert_eq!(b.override_flag, Some(Override::SkipSpaceCheck));
    assert!(b.message.contains("not enough free space"), "{}", b.message);
    let r = pf.resolved.unwrap();
    assert_eq!(r.space_needed, 8 << 40);
    assert!(r.free_space.unwrap() < r.space_needed);
    plan.no_space_check = true;
    let pf = preflight_image(&plan);
    assert!(pf.is_clear(), "{:?}", pf.blockers);
    assert!(pf
        .warnings
        .iter()
        .any(|w| w.contains("free-space check is off")));
}

#[test]
fn recovery_into_the_image_itself_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let img = source_file(dir.path(), 4096);
    let pf = preflight_recover(&RecoverPlan::new(&img, &img));
    for code in [
        BlockerCode::OutputIsSource,
        BlockerCode::OutputOnSource,
        BlockerCode::OutputNotFolder,
    ] {
        assert!(pf.has(code), "{code:?} missing in {:?}", pf.blockers);
    }
    assert!(pf.blockers.iter().all(|b| b.override_flag.is_none()));
    // A folder next to it is fine.
    let pf = preflight_recover(&RecoverPlan::new(&img, dir.path().join("out")));
    assert!(pf.is_clear(), "{:?}", pf.blockers);
    assert!(!pf.resolved.unwrap().reading_device);
}

#[test]
fn recovery_checks_donors_maps_and_the_imaging_step() {
    let dir = tempfile::tempdir().unwrap();
    let img = source_file(dir.path(), 4096);
    let mut plan = RecoverPlan::new(&img, dir.path().join("out"));
    plan.repair.donors.push(dir.path().join("missing.mp4"));
    plan.map = Some(dir.path().join("missing.map"));
    let pf = preflight_recover(&plan);
    assert!(pf.has(BlockerCode::DonorMissing), "{:?}", pf.blockers);
    assert!(pf.has(BlockerCode::MapMissing), "{:?}", pf.blockers);

    // With an imaging step, the image's checks are included.
    let existing = dir.path().join("copy.img");
    std::fs::write(&existing, b"old").unwrap();
    let mut plan = RecoverPlan::new(&img, dir.path().join("out2"));
    plan.image_to = Some(existing);
    let pf = preflight_recover(&plan);
    assert!(pf.has(BlockerCode::ImageExists), "{:?}", pf.blockers);
    assert!(pf.resolved.unwrap().image.is_some());
}

#[test]
fn scan_repair_and_verify_inputs() {
    let dir = tempfile::tempdir().unwrap();
    let img = source_file(dir.path(), 4096);
    let pf = preflight_scan(&ScanPlan {
        source: img.clone(),
        ..ScanPlan::default()
    });
    assert!(pf.is_clear(), "{:?}", pf.blockers);
    let pf = preflight_scan(&ScanPlan {
        source: dir.path().join("none.img"),
        ..ScanPlan::default()
    });
    assert!(pf.has(BlockerCode::SourceMissing));

    let pf = preflight_verify(&VerifyPlan {
        files: vec![dir.path().join("*.jpg")],
    });
    assert!(pf.has(BlockerCode::InputInvalid));
    let pf = preflight_verify(&VerifyPlan {
        files: vec![img.clone()],
    });
    assert_eq!(pf.resolved.unwrap(), vec![img.clone()]);

    let pf = preflight_repair(&RepairPlan {
        files: vec![img.clone()],
        output: img.clone(),
        donors: vec![dir.path().join("nope.jpg")],
        aggressive: false,
    });
    assert!(pf.has(BlockerCode::OutputNotFolder) && pf.has(BlockerCode::DonorMissing));
}
