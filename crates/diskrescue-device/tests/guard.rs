//! Output guard (path_is_on_device) and re-identification by fingerprint.

#![cfg(target_os = "linux")]

mod common;

use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

use common::{vpd80, Disk, FakeSys};
use diskrescue_core::device::DeviceKind;
use diskrescue_device::{sysfs, Fingerprint, Match};

/// (major, minor) of the filesystem holding `p`.
fn st_dev(p: &std::path::Path) -> (u32, u32) {
    let d = std::fs::metadata(p).unwrap().dev() as libc::dev_t;
    (libc::major(d), libc::minor(d))
}

const ATA: &str = "devices/pci0000:00/0000:00:17.0";

#[test]
fn output_on_the_source_disk_is_detected() {
    let fs = FakeSys::new();
    let out_dir = tempfile::tempdir().unwrap();
    let here = st_dev(out_dir.path());
    // A fake disk whose dev number is the temp filesystem's st_dev...
    fs.add_disk(&Disk::scsi(
        "sdz",
        &format!("{ATA}/ata1/host0/target0:0:0/0:0:0:0"),
        here,
        1 << 21,
    ));
    // ...and another one that is not.
    fs.add_disk(&Disk::scsi(
        "sdy",
        &format!("{ATA}/ata2/host1/target1:0:0/1:0:0:0"),
        (8, 240),
        1 << 21,
    ));
    let sdz = sysfs::describe_name_with(&fs.roots, "sdz").unwrap();
    let sdy = sysfs::describe_name_with(&fs.roots, "sdy").unwrap();
    // The output directory does not exist yet: its nearest ancestor counts.
    let target = out_dir.path().join("recovered/photos");
    assert!(sysfs::path_is_on_device_with(&fs.roots, &target, &sdz).unwrap());
    assert!(sysfs::path_is_on_device_with(&fs.roots, out_dir.path(), &sdz).unwrap());
    assert!(!sysfs::path_is_on_device_with(&fs.roots, &target, &sdy).unwrap());
    // Image files never count as "the device".
    let img = diskrescue_device::image_file_info(&out_dir.path().join("x.img"), 0, false);
    assert!(!sysfs::path_is_on_device_with(&fs.roots, &target, &img).unwrap());
}

#[test]
fn output_on_a_partition_or_stacked_volume_is_detected() {
    let fs = FakeSys::new();
    let out_dir = tempfile::tempdir().unwrap();
    let here = st_dev(out_dir.path());

    // Output filesystem is on a partition of sdx.
    let sdx = fs.add_disk(&Disk::scsi(
        "sdx",
        &format!("{ATA}/ata1/host0/target0:0:0/0:0:0:0"),
        (8, 16),
        1 << 21,
    ));
    fs.add_partition(&sdx, "sdx1", 1, 2048, 1 << 20, here);
    let info = sysfs::describe_name_with(&fs.roots, "sdx").unwrap();
    assert!(sysfs::path_is_on_device_with(&fs.roots, out_dir.path(), &info).unwrap());
    // The source given as the *other* partition still guards the disk.
    let part_view = sysfs::describe_name_with(&fs.roots, "sdx1").unwrap();
    assert!(sysfs::path_is_on_device_with(&fs.roots, out_dir.path(), &part_view).unwrap());

    // Output on an LVM volume (holders walk from the source).
    let fs2 = FakeSys::new();
    let sdw = fs2.add_disk(&Disk::scsi(
        "sdw",
        &format!("{ATA}/ata1/host0/target0:0:0/0:0:0:0"),
        (8, 32),
        1 << 21,
    ));
    let sdw1 = fs2.add_partition(&sdw, "sdw1", 1, 2048, 1 << 20, (8, 33));
    let dm = fs2.add_disk(&Disk::virt("dm-7", here, 1 << 20));
    fs2.add_holder(&sdw1, "sdw1", "dm-7", &dm);
    let info = sysfs::describe_name_with(&fs2.roots, "sdw").unwrap();
    assert!(sysfs::path_is_on_device_with(&fs2.roots, out_dir.path(), &info).unwrap());

    // Only the slaves link exists (dm/md built on the source): found by
    // walking the output device's slaves.
    let fs3 = FakeSys::new();
    let sdv = fs3.add_disk(&Disk::scsi(
        "sdv",
        &format!("{ATA}/ata1/host0/target0:0:0/0:0:0:0"),
        (8, 48),
        1 << 21,
    ));
    let sdv1 = fs3.add_partition(&sdv, "sdv1", 1, 2048, 1 << 20, (8, 49));
    let md = fs3.add_disk(&Disk::virt("md9", here, 1 << 20));
    fs3.symlink(&format!("{md}/slaves/sdv1"), &sdv1);
    let info = sysfs::describe_name_with(&fs3.roots, "sdv").unwrap();
    assert!(sysfs::path_is_on_device_with(&fs3.roots, out_dir.path(), &info).unwrap());
    // An unrelated disk in that tree is not affected.
    fs3.add_disk(&Disk::scsi(
        "sdu",
        &format!("{ATA}/ata2/host1/target1:0:0/1:0:0:0"),
        (8, 64),
        1 << 21,
    ));
    let other = sysfs::describe_name_with(&fs3.roots, "sdu").unwrap();
    assert!(!sysfs::path_is_on_device_with(&fs3.roots, out_dir.path(), &other).unwrap());
}

#[test]
fn anonymous_device_numbers_use_the_mount_source() {
    // btrfs/fuse report 0:N; the mount's source names the real device.
    let fs = FakeSys::new();
    let out_dir = tempfile::tempdir().unwrap();
    let canon = std::fs::canonicalize(out_dir.path()).unwrap();
    let sdt = fs.add_disk(&Disk::scsi(
        "sdt",
        &format!("{ATA}/ata1/host0/target0:0:0/0:0:0:0"),
        (8, 80),
        1 << 21,
    ));
    fs.add_partition(&sdt, "sdt1", 1, 2048, 1 << 20, (8, 81));
    fs.mountinfo(&format!(
        "1 0 0:99 / / rw - ext4 /dev/nvme9n9 rw\n77 1 0:250 / {} rw - fuseblk /dev/sdt1 rw\n",
        canon.display()
    ));
    let info = sysfs::describe_name_with(&fs.roots, "sdt").unwrap();
    assert!(sysfs::path_is_on_device_with(&fs.roots, &canon.join("out"), &info).unwrap());
    // Mounted elsewhere: not on the device.
    fs.mountinfo("1 0 0:99 / / rw - ext4 /dev/nvme9n9 rw\n");
    assert!(!sysfs::path_is_on_device_with(&fs.roots, &canon.join("out"), &info).unwrap());
}

fn usb_stick(
    fs: &FakeSys,
    name: &str,
    port: &str,
    devt: (u32, u32),
    serial: Option<&str>,
    sectors: u64,
) -> String {
    let usb = format!("devices/pci0000:00/0000:00:14.0/usb1/{port}");
    let scsi = format!("{usb}/{port}:1.0/host6/target6:0:0/6:0:0:0");
    let mut d = Disk::scsi(name, &scsi, devt, sectors);
    d.removable = true;
    let dir = fs.add_disk(&d);
    fs.write(&format!("sys/{usb}/idVendor"), "0781\n");
    fs.write(&format!("sys/{scsi}/vendor"), "SanDisk \n");
    fs.write(&format!("sys/{scsi}/model"), "Cruzer Blade    \n");
    if let Some(s) = serial {
        fs.write(&format!("sys/{scsi}/vpd_pg80"), vpd80(s));
    }
    dir
}

#[test]
fn fingerprint_refinds_a_renamed_device() {
    let before = FakeSys::new();
    let dir = usb_stick(
        &before,
        "sdc",
        "1-3",
        (8, 32),
        Some("4C530001231012345678"),
        120_176_640,
    );
    before.add_partition(&dir, "sdc1", 1, 2048, 120_174_592, (8, 33));
    let info = sysfs::describe_name_with(&before.roots, "sdc").unwrap();
    let fp = Fingerprint::of(&info, None);
    let part = sysfs::describe_name_with(&before.roots, "sdc1").unwrap();
    let pfp = Fingerprint::of(&part, None);

    // After a USB reset the stick is sdf; another stick took sdc.
    let after = FakeSys::new();
    usb_stick(
        &after,
        "sdc",
        "1-4",
        (8, 32),
        Some("FFFF0001231012345678"),
        120_176_640,
    );
    let dir = usb_stick(
        &after,
        "sdf",
        "1-3",
        (8, 80),
        Some("4C530001231012345678"),
        120_176_640,
    );
    after.add_partition(&dir, "sdf1", 1, 2048, 120_174_592, (8, 81));
    let found = sysfs::find_by_fingerprint_with(&after.roots, &fp)
        .unwrap()
        .unwrap();
    assert_eq!(found.name, "sdf");
    assert_eq!(fp.matches(&found), Match::Exact);
    let found = sysfs::find_by_fingerprint_with(&after.roots, &pfp)
        .unwrap()
        .unwrap();
    assert_eq!(found.name, "sdf1");
    assert!(diskrescue_device::is_partition_view(&found));

    // The same serial with a collapsed capacity is a mismatch, not a match,
    // and a loop device of the right size is never taken for the stick.
    let broken = FakeSys::new();
    usb_stick(
        &broken,
        "sdf",
        "1-3",
        (8, 80),
        Some("4C530001231012345678"),
        0,
    );
    broken.add_disk(&Disk::virt("loop0", (7, 0), 120_176_640));
    assert!(sysfs::find_by_fingerprint_with(&broken.roots, &fp)
        .unwrap()
        .is_none());
    let b = sysfs::describe_name_with(&broken.roots, "sdf").unwrap();
    assert!(matches!(fp.matches(&b), Match::Mismatch(r) if r.contains("size changed")));
}

#[test]
fn weak_identities_need_an_unambiguous_candidate() {
    let before = FakeSys::new();
    usb_stick(&before, "sdc", "1-3", (8, 32), None, 15_000_000);
    let info = sysfs::describe_name_with(&before.roots, "sdc").unwrap();
    assert_eq!(info.kind, DeviceKind::UsbFlash);
    assert_eq!(info.serial, None);
    let fp = Fingerprint::of(&info, None);
    assert!(!fp.has_strong_serial());

    // Without a first-sector hash a weak identity is never guessed, not
    // even when only one device of that model and size is present: it may
    // be another stick of the same kind.
    let one = FakeSys::new();
    usb_stick(&one, "sdd", "1-3", (8, 48), None, 15_000_000);
    assert!(sysfs::find_by_fingerprint_with(&one.roots, &fp)
        .unwrap()
        .is_none());

    let two = FakeSys::new();
    usb_stick(&two, "sdd", "1-3", (8, 48), None, 15_000_000);
    usb_stick(&two, "sde", "1-4", (8, 64), None, 15_000_000);
    assert!(sysfs::find_by_fingerprint_with(&two.roots, &fp)
        .unwrap()
        .is_none());
    assert!(sysfs::find_by_fingerprint_with(&FakeSys::new().roots, &fp)
        .unwrap()
        .is_none());

    // With the hash of LBA 0 the right one of the two is found (the fake
    // device nodes are regular files holding the first sectors).
    let ours = vec![0x5Au8; 512];
    let theirs = vec![0xA5u8; 512];
    two.write("dev/sdd", &theirs);
    two.write("dev/sde", &ours);
    let fp = Fingerprint::of(&info, Some(&ours));
    let got = sysfs::find_by_fingerprint_with(&two.roots, &fp).unwrap();
    assert_eq!(got.map(|d| d.name), Some("sde".to_string()));
    // Nobody has our first sector: nothing is found.
    two.write("dev/sde", &theirs);
    assert!(sysfs::find_by_fingerprint_with(&two.roots, &fp)
        .unwrap()
        .is_none());
}

#[test]
fn cloned_serials_and_swapped_cards_are_not_taken_for_the_lost_device() {
    // Two sticks that ship the same "unique" serial number and size: the
    // first-sector hash decides, and without it nothing is chosen.
    let before = FakeSys::new();
    usb_stick(
        &before,
        "sdc",
        "1-3",
        (8, 32),
        Some("AA11BB22CC33DD44"),
        15_000_000,
    );
    let info = sysfs::describe_name_with(&before.roots, "sdc").unwrap();
    let ours = vec![0x11u8; 512];
    let other = vec![0x22u8; 512];
    let fp_nohash = Fingerprint::of(&info, None);
    let fp = Fingerprint::of(&info, Some(&ours));
    assert!(fp.has_strong_serial());

    let after = FakeSys::new();
    for (name, port, devt) in [("sdd", "1-3", (8, 48)), ("sde", "1-4", (8, 64))] {
        usb_stick(
            &after,
            name,
            port,
            devt,
            Some("AA11BB22CC33DD44"),
            15_000_000,
        );
    }
    after.write("dev/sdd", &other);
    after.write("dev/sde", &ours);
    assert!(sysfs::find_by_fingerprint_with(&after.roots, &fp_nohash)
        .unwrap()
        .is_none());
    let got = sysfs::find_by_fingerprint_with(&after.roots, &fp).unwrap();
    assert_eq!(got.map(|d| d.name), Some("sde".to_string()));

    // A single device with our serial (a card reader) but another card in
    // it: its first sector differs, so it is not ours.
    let swapped = FakeSys::new();
    usb_stick(
        &swapped,
        "sdd",
        "1-3",
        (8, 48),
        Some("AA11BB22CC33DD44"),
        15_000_000,
    );
    swapped.write("dev/sdd", &other);
    assert!(sysfs::find_by_fingerprint_with(&swapped.roots, &fp)
        .unwrap()
        .is_none());
    // The original card is back: found. An unreadable first sector does not
    // block a unique serial number.
    swapped.write("dev/sdd", &ours);
    assert!(sysfs::find_by_fingerprint_with(&swapped.roots, &fp)
        .unwrap()
        .is_some());
    std::fs::remove_file(swapped.base().join("dev/sdd")).unwrap();
    assert!(sysfs::find_by_fingerprint_with(&swapped.roots, &fp)
        .unwrap()
        .is_some());
}

#[test]
fn mountpoints_of_requires_a_device_or_file() {
    let fs = FakeSys::new();
    assert!(sysfs::mountpoints_of_with(&fs.roots, fs.base()).is_err());
    assert!(sysfs::mountpoints_of_with(&fs.roots, &fs.base().join("nope")).is_err());
    let real = diskrescue_device::mountpoints_of(&PathBuf::from("/nonexistent-device"));
    assert!(real.is_err());
}

#[test]
fn partition_mount_state_is_per_partition() {
    let fs = FakeSys::new();
    let dir = fs.add_disk(&Disk::scsi(
        "sdb",
        &format!("{ATA}/ata1/host0/target0:0:0/0:0:0:0"),
        (8, 16),
        1 << 22,
    ));
    let p1 = fs.add_partition(&dir, "sdb1", 1, 2048, 1 << 20, (8, 17));
    fs.add_partition(&dir, "sdb2", 2, 2048 + (1 << 20), 1 << 20, (8, 18));
    let crypt = fs.add_disk(&Disk::virt("dm-3", (253, 3), 1 << 20));
    fs.add_holder(&p1, "sdb1", "dm-3", &crypt);
    fs.mountinfo("40 1 8:18 / /mnt/two rw - ext4 /dev/sdb2 rw\n");
    let disk = sysfs::describe_name_with(&fs.roots, "sdb").unwrap();
    let mut one = sysfs::describe_name_with(&fs.roots, "sdb1").unwrap();
    let mut two = sysfs::describe_name_with(&fs.roots, "sdb2").unwrap();
    assert!(disk.is_mounted() && sysfs::is_mounted_with(&fs.roots, &disk));
    assert!(!one.is_mounted() && !sysfs::is_mounted_with(&fs.roots, &one));
    assert!(two.is_mounted());
    two.mountpoints.clear();
    assert!(sysfs::is_mounted_with(&fs.roots, &two));
    // The dm-crypt volume on sdb1 gets mounted later: re-checked live.
    fs.mountinfo("41 1 253:3 / /home rw - ext4 /dev/mapper/home rw\n");
    one.mountpoints.clear();
    assert!(sysfs::is_mounted_with(&fs.roots, &one));
}
