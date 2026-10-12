//! Enumeration and classification over fake sysfs trees.

mod common;

use std::path::PathBuf;

use common::{vpd80, vpdb1, Disk, FakeSys};
use diskrescue_core::device::{DeviceInfo, DeviceKind, Transport};
use diskrescue_device::sysfs::{self, SysDevice};
use diskrescue_device::Confidence;

const PCI: &str = "devices/pci0000:00";

/// One machine with every device class the layer must recognise.
fn world() -> FakeSys {
    let fs = FakeSys::new();

    // SATA HDD (WD Blue, VPD B1 7200 rpm, udev full model and by-id).
    let sda_dev = format!("{PCI}/0000:00:17.0/ata1/host0/target0:0:0/0:0:0:0");
    let mut d = Disk::scsi("sda", &sda_dev, (8, 0), 1_953_525_168);
    d.pbs = 4096;
    fs.add_disk(&d);
    fs.write(&format!("sys/{sda_dev}/vendor"), "ATA     \n");
    fs.write(&format!("sys/{sda_dev}/model"), "WDC WD10EZEX-08W\n");
    fs.write(&format!("sys/{sda_dev}/rev"), "1A01\n");
    fs.write(&format!("sys/{sda_dev}/type"), "0\n");
    fs.write(&format!("sys/{sda_dev}/state"), "running\n");
    fs.write(&format!("sys/{sda_dev}/timeout"), "30\n");
    fs.write(
        &format!("sys/{sda_dev}/vpd_pg80"),
        vpd80("     WD-WCC6Y1234567"),
    );
    fs.write(&format!("sys/{sda_dev}/vpd_pgb1"), vpdb1(7200, 0));
    fs.udev(
        (8, 0),
        "S:disk/by-id/ata-WDC_WD10EZEX-08WN4A0_WD-WCC6Y1234567\nE:ID_BUS=ata\nE:ID_MODEL=WDC_WD10EZEX-08WN4A0\nE:ID_MODEL_ENC=WDC\\x20WD10EZEX-08WN4A0\\x20\\x20\\x20\nE:ID_SERIAL=WDC_WD10EZEX-08WN4A0_WD-WCC6Y1234567\nE:ID_SERIAL_SHORT=WD-WCC6Y1234567\nE:ID_ATA_ROTATION_RATE_RPM=7200\n",
    );
    fs.by_id("ata-WDC_WD10EZEX-08WN4A0_WD-WCC6Y1234567", "sda");
    fs.by_id("wwn-0x50014ee2b1234567", "sda");

    // SATA SSD (rotational 0, VPD B1 non-rotating, no udev).
    let sdb_dev = format!("{PCI}/0000:00:17.0/ata2/host1/target1:0:0/1:0:0:0");
    let mut d = Disk::scsi("sdb", &sdb_dev, (8, 16), 976_773_168);
    d.rotational = Some(0);
    fs.add_disk(&d);
    fs.write(&format!("sys/{sdb_dev}/vendor"), "ATA     \n");
    fs.write(&format!("sys/{sdb_dev}/model"), "Samsung SSD 860 \n");
    fs.write(&format!("sys/{sdb_dev}/vpd_pg80"), vpd80("S3Z9NB0K123456X"));
    fs.write(&format!("sys/{sdb_dev}/vpd_pgb1"), vpdb1(1, 0));
    fs.write(&format!("sys/{sdb_dev}/state"), "running\n");

    // NVMe with partitions, mounts and swap.
    let ctrl = format!("{PCI}/0000:00:1d.0/0000:3d:00.0/nvme/nvme0");
    let nvme = Disk {
        name: "nvme0n1",
        dir: format!("{ctrl}/nvme0n1"),
        device: Some(ctrl.clone()),
        devt: (259, 0),
        size_sectors: 2_000_409_264,
        removable: false,
        ro: false,
        rotational: Some(0),
        lbs: 512,
        pbs: 512,
    };
    let nvme_dir = fs.add_disk(&nvme);
    fs.write(
        &format!("sys/{ctrl}/model"),
        "KXG60ZNV1T02 KIOXIA                     \n",
    );
    fs.write(&format!("sys/{ctrl}/serial"), "307F71PHFGQL        \n");
    fs.write(&format!("sys/{ctrl}/firmware_rev"), "AGHA4101\n");
    fs.write(&format!("sys/{ctrl}/state"), "live\n");
    fs.write(&format!("sys/{ctrl}/transport"), "pcie\n");
    fs.write(
        &format!("{nvme_dir}/wwid"),
        "eui.00000000000000018ce38e05002143c9\n",
    );
    fs.add_partition(&nvme_dir, "nvme0n1p1", 1, 2048, 1_048_576, (259, 1));
    fs.add_partition(&nvme_dir, "nvme0n1p2", 2, 1_050_624, 16_777_216, (259, 2));
    fs.add_partition(
        &nvme_dir,
        "nvme0n1p3",
        3,
        17_827_840,
        1_982_580_736,
        (259, 3),
    );
    fs.udev((259, 1), "E:ID_FS_TYPE=vfat\n");
    fs.udev((259, 3), "E:ID_FS_TYPE=ext4\n");
    fs.by_id("nvme-KXG60ZNV1T02_KIOXIA_307F71PHFGQL", "nvme0n1");
    fs.by_id("nvme-KXG60ZNV1T02_KIOXIA_307F71PHFGQL_1", "nvme0n1");
    fs.by_id("nvme-eui.00000000000000018ce38e05002143c9", "nvme0n1");
    fs.by_id("nvme-KXG60ZNV1T02_KIOXIA_307F71PHFGQL-part3", "nvme0n1p3");
    // Hidden per-path multipath disk: must be skipped.
    let mut hidden = Disk::virt("nvme0c0n1", (259, 9), 2_000_409_264);
    hidden.dir = format!("{ctrl}/nvme0c0n1");
    hidden.device = Some(ctrl.clone());
    let hdir = fs.add_disk(&hidden);
    fs.write(&format!("{hdir}/hidden"), "1\n");

    // USB flash: SanDisk Cruzer Blade with a serial (spec fixture).
    let usb = format!("{PCI}/0000:00:14.0/usb1/1-3");
    let iface = format!("{usb}/1-3:1.0");
    let scsi = format!("{iface}/host6/target6:0:0/6:0:0:0");
    let mut d = Disk::scsi("sdc", &scsi, (8, 32), 120_176_640);
    d.removable = true;
    let sdc_dir = fs.add_disk(&d);
    fs.write(&format!("sys/{usb}/idVendor"), "0781\n");
    fs.write(&format!("sys/{usb}/idProduct"), "5567\n");
    fs.write(&format!("sys/{usb}/serial"), "4C530001231012345678\n");
    fs.write(&format!("sys/{usb}/manufacturer"), "SanDisk\n");
    fs.write(&format!("sys/{usb}/product"), "Cruzer Blade\n");
    fs.write(&format!("sys/{usb}/speed"), "480\n");
    fs.write(&format!("sys/{usb}/devpath"), "3\n");
    fs.write(&format!("sys/{iface}/bInterfaceProtocol"), "50\n");
    fs.mkdir("sys/bus/usb/drivers/usb-storage");
    fs.symlink(
        &format!("sys/{iface}/driver"),
        "sys/bus/usb/drivers/usb-storage",
    );
    fs.write(&format!("sys/{scsi}/vendor"), "SanDisk \n");
    fs.write(&format!("sys/{scsi}/model"), "Cruzer Blade    \n");
    fs.write(&format!("sys/{scsi}/rev"), "1.26\n");
    fs.write(&format!("sys/{scsi}/type"), "0\n");
    fs.write(&format!("sys/{scsi}/state"), "running\n");
    fs.write(
        &format!("sys/{scsi}/vpd_pg80"),
        vpd80("4C530001231012345678"),
    );
    fs.add_partition(&sdc_dir, "sdc1", 1, 2048, 120_174_592, (8, 33));
    fs.udev(
        (8, 32),
        "S:disk/by-id/usb-SanDisk_Cruzer_Blade_4C530001231012345678-0:0\nE:ID_SERIAL_SHORT=4C530001231012345678\nE:ID_BUS=usb\nE:ID_USB_DRIVER=usb-storage\n",
    );
    fs.udev((8, 33), "E:ID_FS_TYPE=exfat\n");
    fs.by_id("usb-SanDisk_Cruzer_Blade_4C530001231012345678-0:0", "sdc");
    fs.by_id(
        "usb-SanDisk_Cruzer_Blade_4C530001231012345678-0:0-part1",
        "sdc1",
    );

    // USB HDD: Seagate Expansion (rotational 1, not removable, UAS) with
    // an LVM volume (dm-0) on its partition, mounted.
    let usb2 = format!("{PCI}/0000:00:14.0/usb2/2-1");
    let iface2 = format!("{usb2}/2-1:1.0");
    let scsi2 = format!("{iface2}/host7/target7:0:0/7:0:0:0");
    let d = Disk::scsi("sdd", &scsi2, (8, 48), 3_907_029_168);
    let sdd_dir = fs.add_disk(&d);
    fs.write(&format!("sys/{usb2}/idVendor"), "0bc2\n");
    fs.write(&format!("sys/{usb2}/idProduct"), "2322\n");
    fs.write(&format!("sys/{usb2}/serial"), "NAAB1234\n");
    fs.write(&format!("sys/{usb2}/product"), "Expansion HDD\n");
    fs.mkdir("sys/bus/usb/drivers/uas");
    fs.symlink(&format!("sys/{iface2}/driver"), "sys/bus/usb/drivers/uas");
    fs.write(&format!("sys/{scsi2}/vendor"), "Seagate \n");
    fs.write(&format!("sys/{scsi2}/model"), "Expansion HDD   \n");
    fs.write(&format!("sys/{scsi2}/type"), "0\n");
    fs.write(&format!("sys/{scsi2}/state"), "running\n");
    let sdd1 = fs.add_partition(&sdd_dir, "sdd1", 1, 2048, 3_907_026_944, (8, 49));
    let dm = fs.add_disk(&Disk::virt("dm-0", (253, 0), 3_907_000_000));
    fs.write(&format!("{dm}/dm/name"), "vg-data\n");
    fs.add_holder(&sdd1, "sdd1", "dm-0", &dm);
    fs.raw_symlink("dev/mapper/vg-data", "../dm-0");

    // SD card on a native MMC host.
    let card = "devices/platform/soc/fe320000.mmc/mmc_host/mmc0/mmc0:aaaa".to_string();
    let mut d = Disk::scsi("mmcblk0", &card, (179, 0), 62_333_952);
    d.rotational = Some(0);
    let mmc_dir = fs.add_disk(&d);
    fs.write(&format!("sys/{card}/type"), "SD\n");
    fs.write(&format!("sys/{card}/name"), "SL32G\n");
    fs.write(&format!("sys/{card}/serial"), "0x1b2c3d4e\n");
    fs.write(&format!("sys/{card}/manfid"), "0x000003\n");
    fs.write(&format!("sys/{card}/oemid"), "0x5344\n");
    fs.write(&format!("sys/{card}/fwrev"), "0x0\n");
    fs.write(
        &format!("sys/{card}/csd"),
        "400e00325b590000ee7f7f800a404000\n",
    );
    fs.add_partition(&mmc_dir, "mmcblk0p1", 1, 8192, 62_325_760, (179, 1));
    fs.by_id("mmc-SL32G_0x1b2c3d4e", "mmcblk0");

    // eMMC boot partition exposed as a disk: virtual, not a candidate.
    let emmc = "devices/platform/soc/fe330000.mmc/mmc_host/mmc1/mmc1:0001".to_string();
    let mut d = Disk::scsi("mmcblk1boot0", &emmc, (179, 32), 8192);
    d.ro = true;
    fs.add_disk(&d);
    fs.write(&format!("sys/{emmc}/type"), "MMC\n");

    // SD card in a USB reader (Genesys Logic, generic names, no serial).
    let usb3 = format!("{PCI}/0000:00:14.0/usb1/1-4");
    let iface3 = format!("{usb3}/1-4:1.0");
    let scsi3 = format!("{iface3}/host8/target8:0:0/8:0:0:0");
    let mut d = Disk::scsi("sde", &scsi3, (8, 64), 62_333_952);
    d.removable = true;
    fs.add_disk(&d);
    fs.write(&format!("sys/{usb3}/idVendor"), "05e3\n");
    fs.write(&format!("sys/{usb3}/idProduct"), "0751\n");
    fs.write(&format!("sys/{usb3}/product"), "USB Storage\n");
    fs.write(&format!("sys/{scsi3}/vendor"), "Generic \n");
    fs.write(&format!("sys/{scsi3}/model"), "STORAGE DEVICE  \n");
    fs.write(&format!("sys/{scsi3}/type"), "0\n");

    // Dead pen drive: zero capacity, generic identity and serial.
    let usb4 = format!("{PCI}/0000:00:14.0/usb1/1-5");
    let iface4 = format!("{usb4}/1-5:1.0");
    let scsi4 = format!("{iface4}/host9/target9:0:0/9:0:0:0");
    let mut d = Disk::scsi("sdg", &scsi4, (8, 96), 0);
    d.removable = true;
    fs.add_disk(&d);
    fs.write(&format!("sys/{usb4}/idVendor"), "1234\n");
    fs.write(&format!("sys/{usb4}/serial"), "0123456789ABCDEF\n");
    fs.write(&format!("sys/{scsi4}/vendor"), "General \n");
    fs.write(&format!("sys/{scsi4}/model"), "UDisk           \n");
    fs.write(&format!("sys/{scsi4}/state"), "running\n");

    // 4Kn SATA disk.
    let sdh_dev = format!("{PCI}/0000:00:17.0/ata4/host3/target3:0:0/3:0:0:0");
    let mut d = Disk::scsi("sdh", &sdh_dev, (8, 112), 7_814_037_168);
    d.lbs = 4096;
    d.pbs = 4096;
    fs.add_disk(&d);
    fs.write(&format!("sys/{sdh_dev}/vendor"), "ATA     \n");
    fs.write(&format!("sys/{sdh_dev}/model"), "ST4000NM0035-1V4\n");
    fs.write(&format!("sys/{sdh_dev}/vpd_pg80"), vpd80("ZC1ABCDE"));

    // Optical drive without a disc.
    let sr_dev = format!("{PCI}/0000:00:17.0/ata5/host4/target4:0:0/4:0:0:0");
    let mut d = Disk::scsi("sr0", &sr_dev, (11, 0), 0);
    d.lbs = 2048;
    d.pbs = 2048;
    d.removable = true;
    fs.add_disk(&d);
    fs.write(&format!("sys/{sr_dev}/type"), "5\n");
    fs.write(&format!("sys/{sr_dev}/vendor"), "HL-DT-ST\n");
    fs.write(&format!("sys/{sr_dev}/model"), "DVDRAM GH24NSD1 \n");

    // Loop device over an image file.
    let lp = fs.add_disk(&Disk::virt("loop0", (7, 0), 131_072));
    fs.write(&format!("{lp}/loop/backing_file"), "/home/u/card.img\n");

    // Mount table: NVMe root/EFI, LVM on the Seagate, the Cruzer through
    // ntfs-3g (anonymous device number, matched by source), tmpfs.
    fs.mountinfo(
        "22 1 259:3 / / rw,relatime shared:1 - ext4 /dev/nvme0n1p3 rw\n\
         23 22 259:1 / /boot/efi rw,relatime shared:2 - vfat /dev/nvme0n1p1 rw\n\
         40 22 253:0 / /mnt/data rw,relatime shared:3 - ext4 /dev/mapper/vg-data rw\n\
         41 22 0:45 / /media/u/My\\040Stick rw,nosuid shared:4 - fuseblk /dev/sdc1 rw\n\
         50 22 0:30 / /tmp rw shared:5 - tmpfs tmpfs rw\n",
    );
    fs.swaps("Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority\n/dev/nvme0n1p2                          partition\t8388604\t\t0\t\t-2\n");
    fs
}

fn scan(fs: &FakeSys) -> Vec<SysDevice> {
    sysfs::scan_with(&fs.roots).unwrap()
}

fn find<'a>(devs: &'a [SysDevice], name: &str) -> &'a SysDevice {
    devs.iter()
        .find(|d| d.info.name == name)
        .unwrap_or_else(|| panic!("{name} not enumerated"))
}

fn has_warning(info: &DeviceInfo, needle: &str) -> bool {
    info.warnings.iter().any(|w| w.contains(needle))
}

#[test]
fn enumerates_whole_disks_in_natural_order() {
    let fs = world();
    let names: Vec<String> = sysfs::enumerate_with(&fs.roots)
        .unwrap()
        .into_iter()
        .map(|d| d.name)
        .collect();
    assert_eq!(
        names,
        vec![
            "dm-0",
            "loop0",
            "mmcblk0",
            "mmcblk1boot0",
            "nvme0n1",
            "sda",
            "sdb",
            "sdc",
            "sdd",
            "sde",
            "sdg",
            "sdh",
            "sr0"
        ]
    );
}

#[test]
fn sata_hdd() {
    let fs = world();
    let devs = scan(&fs);
    let d = find(&devs, "sda");
    assert_eq!(d.info.kind, DeviceKind::Hdd);
    assert_eq!(d.info.transport, Transport::Ata);
    assert_eq!(d.classification.confidence, Confidence::High);
    assert_eq!(d.info.model.as_deref(), Some("WDC WD10EZEX-08WN4A0"));
    assert_eq!(d.info.vendor, None, "libata's 'ATA' is not a vendor");
    assert_eq!(d.info.serial.as_deref(), Some("WD-WCC6Y1234567"));
    assert_eq!(d.info.firmware.as_deref(), Some("1A01"));
    assert_eq!(
        d.info.stable_id.as_deref(),
        Some("ata-WDC_WD10EZEX-08WN4A0_WD-WCC6Y1234567")
    );
    assert_eq!(d.info.size, 1_953_525_168 * 512);
    assert_eq!((d.info.logical_sector, d.info.physical_sector), (512, 4096));
    assert!(d.info.rotational);
    assert_eq!(d.devt, Some((8, 0)));
    assert_eq!(d.diskseq, Some(7));
    assert_eq!(d.scsi_timeout_secs, Some(30));
    assert_eq!(d.max_sectors_kb, Some(120));
    assert_eq!(d.info.path, fs.roots.dev.join("sda"));
    assert!(d.info.warnings.is_empty(), "{:?}", d.info.warnings);
}

#[test]
fn sata_ssd() {
    let fs = world();
    let devs = scan(&fs);
    let d = find(&devs, "sdb");
    assert_eq!(d.info.kind, DeviceKind::SataSsd);
    assert_eq!(d.classification.confidence, Confidence::High);
    assert_eq!(d.info.model.as_deref(), Some("Samsung SSD 860"));
    assert_eq!(d.info.serial.as_deref(), Some("S3Z9NB0K123456X"));
    assert!(!d.info.rotational);
}

#[test]
fn nvme_with_partitions_mounts_and_swap() {
    let fs = world();
    let devs = scan(&fs);
    let d = find(&devs, "nvme0n1");
    assert_eq!(d.info.kind, DeviceKind::NvmeSsd);
    assert_eq!(d.info.transport, Transport::Nvme);
    assert_eq!(d.info.model.as_deref(), Some("KXG60ZNV1T02 KIOXIA"));
    assert_eq!(d.info.serial.as_deref(), Some("307F71PHFGQL"));
    assert_eq!(d.info.firmware.as_deref(), Some("AGHA4101"));
    assert_eq!(d.state.as_deref(), Some("live"));
    assert_eq!(
        d.wwid.as_deref(),
        Some("eui.00000000000000018ce38e05002143c9")
    );
    assert_eq!(
        d.info.stable_id.as_deref(),
        Some("nvme-KXG60ZNV1T02_KIOXIA_307F71PHFGQL")
    );
    let p = &d.info.partitions;
    assert_eq!(p.len(), 3);
    assert_eq!(p[0].number, 1);
    assert_eq!(p[0].start, 2048 * 512);
    assert_eq!(p[0].size, 1_048_576 * 512);
    assert_eq!(p[0].fs_type.as_deref(), Some("vfat"));
    assert_eq!(p[0].mountpoints, vec![PathBuf::from("/boot/efi")]);
    assert_eq!(p[1].mountpoints, vec![PathBuf::from("[SWAP]")]);
    assert_eq!(p[2].mountpoints, vec![PathBuf::from("/")]);
    assert_eq!(p[2].path, Some(fs.roots.dev.join("nvme0n1p3")));
    for m in ["/", "/boot/efi", "[SWAP]"] {
        assert!(d.info.mountpoints.contains(&PathBuf::from(m)), "{m}");
    }
    assert!(d.info.is_mounted());
    assert!(has_warning(&d.info, "mounted at"));
    // The hidden multipath path disk is not listed.
    assert!(!devs.iter().any(|x| x.info.name == "nvme0c0n1"));
}

#[test]
fn usb_flash_cruzer() {
    let fs = world();
    let devs = scan(&fs);
    let d = find(&devs, "sdc");
    assert_eq!(d.info.kind, DeviceKind::UsbFlash);
    assert_eq!(d.info.transport, Transport::Usb);
    assert_eq!(d.classification.confidence, Confidence::Medium);
    assert_eq!(d.info.vendor.as_deref(), Some("SanDisk"));
    assert_eq!(d.info.model.as_deref(), Some("Cruzer Blade"));
    assert_eq!(d.info.serial.as_deref(), Some("4C530001231012345678"));
    assert!(d.info.removable);
    assert_eq!(
        d.info.stable_id.as_deref(),
        Some("usb-SanDisk_Cruzer_Blade_4C530001231012345678-0:0")
    );
    let usb = d.usb.as_ref().unwrap();
    assert_eq!(usb.vendor_id.as_deref(), Some("0781"));
    assert_eq!(usb.product_id.as_deref(), Some("5567"));
    assert_eq!(usb.driver.as_deref(), Some("usb-storage"));
    assert_eq!(usb.interface_protocol.as_deref(), Some("50"));
    assert_eq!(usb.devpath.as_deref(), Some("3"));
    assert!(has_warning(&d.info, "misreport"));
    // Mounted through ntfs-3g/fuse: anonymous st_dev, matched by source.
    assert_eq!(
        d.info.partitions[0].mountpoints,
        vec![PathBuf::from("/media/u/My Stick")]
    );
    assert_eq!(d.info.partitions[0].fs_type.as_deref(), Some("exfat"));
    assert!(d.info.is_mounted());
}

#[test]
fn usb_hdd_seagate_with_lvm_holder() {
    let fs = world();
    let devs = scan(&fs);
    let d = find(&devs, "sdd");
    assert_eq!(d.info.kind, DeviceKind::UsbHdd);
    assert!(d.classification.confidence >= Confidence::Medium);
    assert_eq!(d.usb.as_ref().unwrap().driver.as_deref(), Some("uas"));
    assert_eq!(d.info.serial.as_deref(), Some("NAAB1234"));
    assert!(d.holders.contains(&"dm-0".to_string()), "{:?}", d.holders);
    // Mounted through the dm volume stacked on its partition.
    assert!(d.info.mountpoints.contains(&PathBuf::from("/mnt/data")));
    assert!(d.info.partitions[0]
        .mountpoints
        .contains(&PathBuf::from("/mnt/data")));
    let dm = find(&devs, "dm-0");
    assert_eq!(dm.info.kind, DeviceKind::Virtual);
    assert!(has_warning(&dm.info, "vg-data"));
    assert!(has_warning(&dm.info, "sdd1"));
    assert_eq!(dm.info.mountpoints, vec![PathBuf::from("/mnt/data")]);
}

#[test]
fn sd_card_native_mmc() {
    let fs = world();
    let devs = scan(&fs);
    let d = find(&devs, "mmcblk0");
    assert_eq!(d.info.kind, DeviceKind::SdCard);
    assert_eq!(d.info.transport, Transport::Mmc);
    assert_eq!(d.info.model.as_deref(), Some("SL32G"));
    assert_eq!(d.info.vendor.as_deref(), Some("SanDisk"));
    assert_eq!(d.info.serial.as_deref(), Some("0x1b2c3d4e"));
    assert_eq!(d.info.stable_id.as_deref(), Some("mmc-SL32G_0x1b2c3d4e"));
    assert_eq!(d.info.partitions.len(), 1);
    let boot = find(&devs, "mmcblk1boot0");
    assert_eq!(boot.info.kind, DeviceKind::Virtual);
    assert!(has_warning(&boot.info, "boot/RPMB"));
    assert!(has_warning(&boot.info, "read-only"));
}

#[test]
fn sd_card_in_usb_reader() {
    let fs = world();
    let devs = scan(&fs);
    let d = find(&devs, "sde");
    assert_eq!(d.info.kind, DeviceKind::UsbFlash);
    assert!(d.classification.card_reader);
    assert!(has_warning(&d.info, "card reader"));
    assert!(has_warning(&d.info, "no serial number"));
    assert_eq!(d.info.serial, None);
}

#[test]
fn dead_pen_drive_with_zero_capacity() {
    let fs = world();
    let devs = scan(&fs);
    let d = find(&devs, "sdg");
    assert_eq!(d.info.size, 0);
    assert_eq!(d.info.kind, DeviceKind::UsbFlash);
    assert!(has_warning(
        &d.info,
        "device reports no capacity: controller failure is likely"
    ));
    assert!(has_warning(&d.info, "looks generic"));
    assert!(has_warning(&d.info, "generic identity"));
}

#[test]
fn four_k_native_disk() {
    let fs = world();
    let devs = scan(&fs);
    let d = find(&devs, "sdh");
    assert_eq!(d.info.logical_sector, 4096);
    assert_eq!(d.info.physical_sector, 4096);
    assert_eq!(d.info.size, 7_814_037_168 * 512);
    assert_eq!(d.info.kind, DeviceKind::Hdd);
    assert_eq!(d.classification.confidence, Confidence::Medium);
}

#[test]
fn optical_loop_and_virtual() {
    let fs = world();
    let devs = scan(&fs);
    let sr = find(&devs, "sr0");
    assert_eq!(sr.info.kind, DeviceKind::Optical);
    assert_eq!(sr.info.logical_sector, 2048);
    assert!(has_warning(&sr.info, "no card/disc"));
    let lp = find(&devs, "loop0");
    assert_eq!(lp.info.kind, DeviceKind::Virtual);
    assert_eq!(lp.info.transport, Transport::Virtual);
    assert!(has_warning(&lp.info, "/home/u/card.img"));
}

#[test]
fn describe_partition_reports_parent_kind() {
    let fs = world();
    let p = sysfs::describe_name_with(&fs.roots, "sdc1").unwrap();
    assert_eq!(p.kind, DeviceKind::UsbFlash);
    assert_eq!(p.name, "sdc1");
    assert_eq!(p.path, fs.roots.dev.join("sdc1"));
    assert_eq!(p.size, 120_174_592 * 512);
    assert_eq!(p.serial.as_deref(), Some("4C530001231012345678"));
    assert_eq!(
        p.stable_id.as_deref(),
        Some("usb-SanDisk_Cruzer_Blade_4C530001231012345678-0:0-part1")
    );
    assert_eq!(p.partitions.len(), 1);
    assert!(diskrescue_device::is_partition_view(&p));
    assert_eq!(p.mountpoints, vec![PathBuf::from("/media/u/My Stick")]);
    assert!(has_warning(&p, "partition 1 of"));
    let disk = sysfs::describe_name_with(&fs.roots, "sdc").unwrap();
    assert!(!diskrescue_device::is_partition_view(&disk));
    assert!(sysfs::describe_name_with(&fs.roots, "sdq9").is_err());
    assert!(sysfs::describe_name_with(&fs.roots, "../etc").is_err());
    let label = diskrescue_device::device_label(&p);
    assert_eq!(
        label,
        "SanDisk Cruzer Blade 62 GB, USB flash drive, partition 1"
    );
}

#[test]
fn image_files_and_non_devices() {
    let fs = world();
    let img = fs.base().join("card.img");
    std::fs::write(&img, vec![0u8; 4096]).unwrap();
    let i = sysfs::describe_with(&fs.roots, &img).unwrap();
    assert_eq!(i.kind, DeviceKind::ImageFile);
    assert_eq!(i.size, 4096);
    assert!(sysfs::describe_with(&fs.roots, fs.base()).is_err());
    assert!(sysfs::describe_with(&fs.roots, &fs.base().join("missing")).is_err());
}

#[test]
fn no_udev_and_no_mounts_falls_back_to_sysfs() {
    let fs = world();
    std::fs::remove_dir_all(&fs.roots.udev_data).unwrap();
    std::fs::remove_file(&fs.roots.mountinfo).unwrap();
    std::fs::remove_file(fs.roots.swaps()).unwrap();
    std::fs::remove_dir_all(fs.roots.dev.join("disk")).unwrap();
    let devs = scan(&fs);
    let sda = find(&devs, "sda");
    assert_eq!(sda.info.model.as_deref(), Some("WDC WD10EZEX-08W"));
    assert_eq!(sda.info.serial.as_deref(), Some("WD-WCC6Y1234567"));
    assert_eq!(sda.info.kind, DeviceKind::Hdd);
    assert_eq!(sda.info.stable_id, None);
    let nvme = find(&devs, "nvme0n1");
    assert!(nvme.info.mountpoints.is_empty());
}

#[test]
fn override_and_mount_recheck() {
    let fs = world();
    let mut info = sysfs::describe_name_with(&fs.roots, "sdd").unwrap();
    diskrescue_device::override_kind(&mut info, DeviceKind::UsbSsd);
    assert_eq!(info.kind, DeviceKind::UsbSsd);
    assert!(has_warning(&info, "overridden"));
    // A stale description without mountpoints is re-checked live.
    info.mountpoints.clear();
    for p in &mut info.partitions {
        p.mountpoints.clear();
    }
    assert!(sysfs::is_mounted_with(&fs.roots, &info));
    let sdb = sysfs::describe_name_with(&fs.roots, "sdb").unwrap();
    assert!(!sysfs::is_mounted_with(&fs.roots, &sdb));
}

#[test]
fn missing_sysfs_is_an_error() {
    let fs = FakeSys::new();
    std::fs::remove_dir_all(fs.roots.sysfs.join("block")).unwrap();
    assert!(sysfs::enumerate_with(&fs.roots).is_err());
    let empty = FakeSys::new();
    assert!(sysfs::enumerate_with(&empty.roots).unwrap().is_empty());
}

#[test]
fn real_system_enumeration_does_not_fail() {
    // Reads the real /sys (never opens a device node).
    match diskrescue_device::enumerate() {
        Ok(devs) => {
            for d in devs {
                assert!(!d.name.is_empty());
                assert!(d.logical_sector >= 512);
            }
        }
        Err(e) => eprintln!("skipped: no sysfs here ({e})"),
    }
}

#[test]
fn usb_ssd_behind_a_lying_bridge_and_offline_state() {
    let fs = FakeSys::new();
    let usb = format!("{PCI}/0000:00:14.0/usb2/2-2");
    let scsi = format!("{usb}/2-2:1.0/host10/target10:0:0/10:0:0:0");
    let d = Disk::scsi("sdk", &scsi, (8, 160), 1_953_525_168);
    fs.add_disk(&d);
    fs.write(&format!("sys/{usb}/idVendor"), "152d\n");
    fs.write(&format!("sys/{scsi}/vendor"), "Samsung \n");
    fs.write(&format!("sys/{scsi}/model"), "Portable SSD T5 \n");
    fs.write(&format!("sys/{scsi}/state"), "offline\n");
    fs.write(&format!("sys/{scsi}/vpd_pg80"), vpd80("S49WNP0N123456"));
    let devs = scan(&fs);
    let d = find(&devs, "sdk");
    assert_eq!(d.info.kind, DeviceKind::UsbSsd);
    assert_eq!(d.classification.confidence, Confidence::Low);
    assert!(has_warning(&d.info, "looks like an SSD"));
    assert!(has_warning(&d.info, "state is 'offline'"));
    assert!(has_warning(&d.info, "misreport"));

    // The same bridge passing ATA IDENTIFY through: udev knows the truth.
    fs.udev((8, 160), "E:ID_ATA_ROTATION_RATE_RPM=0\n");
    let devs = scan(&fs);
    let d = find(&devs, "sdk");
    assert_eq!(d.info.kind, DeviceKind::UsbSsd);
    assert_eq!(d.classification.confidence, Confidence::High);
}

#[test]
fn nvme_multipath_head_is_not_virtual() {
    let fs = FakeSys::new();
    let ctrl = format!("{PCI}/0000:00:1d.0/0000:3d:00.0/nvme/nvme0");
    fs.write(&format!("sys/{ctrl}/state"), "live\n");
    fs.write(&format!("sys/{ctrl}/transport"), "pcie\n");
    let subsys = "devices/virtual/nvme-subsystem/nvme-subsys0".to_string();
    let head = Disk {
        name: "nvme0n1",
        dir: format!("{subsys}/nvme0n1"),
        device: Some(subsys.clone()),
        devt: (259, 0),
        size_sectors: 1000,
        removable: false,
        ro: false,
        rotational: Some(0),
        lbs: 512,
        pbs: 512,
    };
    fs.add_disk(&head);
    fs.write(&format!("sys/{subsys}/model"), "Multi NS SSD\n");
    fs.write(&format!("sys/{subsys}/serial"), "MPSERIAL0001\n");
    fs.symlink(&format!("sys/{subsys}/nvme0"), &format!("sys/{ctrl}"));
    let devs = scan(&fs);
    let d = find(&devs, "nvme0n1");
    assert_eq!(d.info.kind, DeviceKind::NvmeSsd);
    assert_eq!(d.info.transport, Transport::Nvme);
    assert_eq!(d.state.as_deref(), Some("live"));
    assert_eq!(d.info.serial.as_deref(), Some("MPSERIAL0001"));
}
