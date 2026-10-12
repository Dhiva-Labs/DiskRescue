//! Garbage, truncated and absurd sysfs/udev/mountinfo contents must never
//! panic or hang enumeration.

mod common;

use std::time::{Duration, Instant};

use common::{Disk, FakeSys, XorShift};
use diskrescue_core::device::DeviceKind;
use diskrescue_device::sysfs;

const SCSI: &str = "devices/pci0000:00/0000:00:14.0/usb1/1-3/1-3:1.0/host6/target6:0:0/6:0:0:0";

#[test]
fn random_attribute_contents() {
    let mut rng = XorShift(0x0123_4567_89AB_CDEF);
    for round in 0..40 {
        let fs = FakeSys::new();
        let dir = fs.add_disk(&Disk::scsi("sdb", SCSI, (8, 16), 1000));
        let part = fs.add_partition(&dir, "sdb1", 1, 2048, 100, (8, 17));
        let files = [
            format!("{dir}/size"),
            format!("{dir}/removable"),
            format!("{dir}/ro"),
            format!("{dir}/dev"),
            format!("{dir}/diskseq"),
            format!("{dir}/queue/rotational"),
            format!("{dir}/queue/logical_block_size"),
            format!("{dir}/queue/physical_block_size"),
            format!("{dir}/queue/zoned"),
            format!("{part}/partition"),
            format!("{part}/start"),
            format!("{part}/size"),
            format!("{part}/dev"),
            format!("sys/{SCSI}/vendor"),
            format!("sys/{SCSI}/model"),
            format!("sys/{SCSI}/type"),
            format!("sys/{SCSI}/state"),
            format!("sys/{SCSI}/vpd_pg80"),
            format!("sys/{SCSI}/vpd_pgb1"),
            "sys/devices/pci0000:00/0000:00:14.0/usb1/1-3/idVendor".to_string(),
            "sys/devices/pci0000:00/0000:00:14.0/usb1/1-3/serial".to_string(),
            "run/udev/data/b8:16".to_string(),
            "proc/self/mountinfo".to_string(),
            "proc/swaps".to_string(),
        ];
        for f in &files {
            let len = (rng.next() % 300) as usize;
            if round % 3 == 0 {
                // Valid-looking but absurd numbers.
                fs.write(f, format!("{}\n", u64::MAX - (rng.next() % 3)));
            } else {
                fs.write(f, rng.bytes(len));
            }
        }
        let devs = sysfs::scan_with(&fs.roots).unwrap();
        for d in devs {
            assert!(d.info.logical_sector >= 512 && d.info.logical_sector.is_power_of_two());
            assert!(d.info.physical_sector >= d.info.logical_sector);
            let _ = diskrescue_device::device_label(&d.info);
        }
        let _ = sysfs::describe_name_with(&fs.roots, "sdb1");
        let fp = diskrescue_device::Fingerprint::of(
            &sysfs::describe_name_with(&fs.roots, "sdb")
                .unwrap_or_else(|_| diskrescue_device::image_file_info(fs.base(), 0, true)),
            Some(&[1, 2, 3]),
        );
        let _ = sysfs::find_by_fingerprint_with(&fs.roots, &fp);
    }
}

#[test]
fn absurd_sizes_saturate() {
    let fs = FakeSys::new();
    let dir = fs.add_disk(&Disk::scsi("sdb", SCSI, (8, 16), 1));
    fs.write(&format!("{dir}/size"), "18446744073709551615\n");
    fs.write(&format!("{dir}/removable"), "1\n");
    fs.write(&format!("sys/{SCSI}/model"), "Flash Disk\n");
    fs.write(&format!("{dir}/queue/logical_block_size"), "3\n");
    let d = sysfs::describe_name_with(&fs.roots, "sdb").unwrap();
    assert_eq!(d.size, u64::MAX);
    assert_eq!(d.logical_sector, 512);
    assert_eq!(d.kind, DeviceKind::UsbFlash);
    assert!(d.warnings.iter().any(|w| w.contains("implausible")));
}

#[test]
fn symlink_and_holder_loops_terminate() {
    let fs = FakeSys::new();
    let a = fs.add_disk(&Disk::virt("dm-0", (253, 0), 100));
    let b = fs.add_disk(&Disk::virt("dm-1", (253, 1), 100));
    // dm-0 and dm-1 claim to hold each other.
    fs.add_holder(&a, "dm-0", "dm-1", &b);
    fs.add_holder(&b, "dm-1", "dm-0", &a);
    // by-id and /dev symlink loops.
    fs.raw_symlink("dev/disk/by-id/loop-a", "loop-b");
    fs.raw_symlink("dev/disk/by-id/loop-b", "loop-a");
    fs.raw_symlink("dev/x", "y");
    fs.raw_symlink("dev/y", "x");
    fs.mountinfo("1 0 0:5 / /mnt rw - fuse /dev/x rw\n");
    // A `device` link pointing outside the sysfs root.
    fs.raw_symlink("sys/devices/virtual/block/dm-1/device", "/");
    let t = Instant::now();
    let devs = sysfs::scan_with(&fs.roots).unwrap();
    assert_eq!(devs.len(), 2);
    assert!(t.elapsed() < Duration::from_secs(5));
    let out = tempfile::tempdir().unwrap();
    let info = sysfs::describe_name_with(&fs.roots, "dm-0").unwrap();
    let _ = sysfs::path_is_on_device_with(&fs.roots, out.path(), &info);
}

#[test]
fn entries_without_size_or_hidden_are_skipped() {
    let fs = FakeSys::new();
    let dir = fs.add_disk(&Disk::scsi("sdb", SCSI, (8, 16), 1000));
    std::fs::remove_file(fs.base().join(format!("{dir}/size"))).unwrap();
    let dir2 = fs.add_disk(&Disk::virt("nvme0c0n1", (259, 5), 10));
    fs.write(&format!("{dir2}/hidden"), "1\n");
    fs.raw_symlink("sys/block/dangling", "../nowhere");
    fs.write("sys/block/not-a-link", "x");
    assert!(sysfs::scan_with(&fs.roots).unwrap().is_empty());
}
