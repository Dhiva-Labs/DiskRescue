//! Device classification: which [`DeviceKind`] a block device is.
//!
//! This is the decision table of `docs/specs/device-layer.md` §3.2 as a
//! pure function over the facts the platform layer collected (transport,
//! rotational flag, removable flag, size, SCSI peripheral type, VPD page
//! 0xB1, udev's `ID_ATA_ROTATION_RATE_RPM`, MMC card type, USB identity).
//! Every branch records a human-readable reason and a [`Confidence`], so the
//! UI can show *why* a device was classified the way it was and the imaging
//! engine can treat a low-confidence class conservatively.
//!
//! # USB
//!
//! USB is where classification is hard. Bulk-only bridges do not forward
//! VPD page 0xB1, so the kernel's default `rotational=1` is reported for
//! pen drives and SSDs alike; `rotational=1` on USB is therefore *not*
//! evidence of a spinning disk. The order of evidence is:
//!
//! 1. card-reader hints (VPD 0xB1 product type, known reader vendor IDs,
//!    "card reader" / "SD/MMC" product strings) -> USB flash (card reader);
//! 2. explicit rotation information that made it through the bridge
//!    (VPD 0xB1, udev ATA rotation rate, SMART) -> SSD/flash or HDD;
//! 3. `rotational=0` (the kernel saw VPD 0xB1 = non-rotating): removable and
//!    at most 512 GiB, or a flash-like model name -> USB flash, else USB SSD;
//! 4. otherwise (`rotational=1` or unknown): flash-like model names and
//!    removable media up to 512 GiB -> USB flash; non-removable -> USB HDD.
//!
//! Limits: a USB-SATA SSD behind a bridge that hides everything lands in
//! `UsbHdd` with low confidence, and an old enclosure that sets the
//! removable bit on a small hard disk lands in `UsbFlash`. The caller can
//! override the class ([`crate::override_kind`]); the profiles for these
//! classes are all safe for either medium, just not optimal.

use diskrescue_core::device::{DeviceKind, Transport};

use crate::parse::{Rotation, VpdB1};

const GIB: u64 = 1 << 30;

/// Size limit (inclusive) below which a removable USB device is assumed to
/// be a flash drive rather than an external SSD.
pub const USB_FLASH_MAX_SIZE: u64 = 512 * GIB;

/// USB vendor IDs of common card-reader chip makers (Genesys Logic, Alcor
/// Micro, Realtek).
pub const CARD_READER_VIDS: &[(&str, &str)] = &[
    ("05e3", "Genesys Logic"),
    ("058f", "Alcor Micro"),
    ("0bda", "Realtek"),
];

/// How sure the classifier is.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    /// A guess from weak signals (size, removable bit).
    Low,
    /// Consistent indirect evidence.
    Medium,
    /// Direct evidence (transport, rotation rate reported by the device).
    High,
}

impl std::fmt::Display for Confidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Confidence::Low => "low",
            Confidence::Medium => "medium",
            Confidence::High => "high",
        })
    }
}

/// Result of [`classify`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Classification {
    /// The device class.
    pub kind: DeviceKind,
    /// How sure we are.
    pub confidence: Confidence,
    /// Why (one entry per rule that fired).
    pub reasons: Vec<String>,
    /// The device is a USB card reader with (or waiting for) a memory card.
    pub card_reader: bool,
}

impl Classification {
    fn new(kind: DeviceKind, confidence: Confidence, reason: impl Into<String>) -> Self {
        Classification {
            kind,
            confidence,
            reasons: vec![reason.into()],
            card_reader: false,
        }
    }

    fn reader(confidence: Confidence, reason: impl Into<String>) -> Self {
        Classification {
            kind: DeviceKind::UsbFlash,
            confidence,
            reasons: vec![reason.into()],
            card_reader: true,
        }
    }
}

/// Facts the classifier looks at. Every field is optional evidence; leave
/// unknown things at their default.
#[derive(Debug, Clone)]
pub struct ClassifyInput<'a> {
    /// Kernel name (`sdb`, `sr0`, `mmcblk0`, `loop3`).
    pub name: &'a str,
    /// Attachment.
    pub transport: Transport,
    /// `queue/rotational` (`None` when the file is missing).
    pub rotational: Option<bool>,
    /// `removable` attribute (the INQUIRY RMB bit for SCSI/USB).
    pub removable: bool,
    /// Size in bytes.
    pub size: u64,
    /// SCSI peripheral device type (`device/type`): 0 disk, 5 CD/DVD.
    pub scsi_type: Option<u8>,
    /// Vendor string (INQUIRY vendor).
    pub vendor: Option<&'a str>,
    /// Model string (INQUIRY product, NVMe/MMC model).
    pub model: Option<&'a str>,
    /// USB `idVendor` (lowercase hex, `0781`).
    pub usb_vendor_id: Option<&'a str>,
    /// USB `product` string descriptor.
    pub usb_product: Option<&'a str>,
    /// USB storage driver (`usb-storage` or `uas`).
    pub usb_driver: Option<&'a str>,
    /// udev `ID_ATA_ROTATION_RATE_RPM` (0 = non-rotating).
    pub udev_ata_rpm: Option<u32>,
    /// Decoded VPD page 0xB1, when the device answered it.
    pub vpd_b1: Option<VpdB1>,
    /// MMC card type (`SD`, `MMC`, `SDIO`, `SDcombo`).
    pub mmc_type: Option<&'a str>,
    /// `rotation_rate` reported by `smartctl -j` (0 = SSD).
    pub smart_rotation_rate: Option<u32>,
}

impl<'a> ClassifyInput<'a> {
    /// Input with only a name and a transport set.
    pub fn new(name: &'a str, transport: Transport) -> Self {
        ClassifyInput {
            name,
            transport,
            rotational: None,
            removable: false,
            size: 0,
            scsi_type: None,
            vendor: None,
            model: None,
            usb_vendor_id: None,
            usb_product: None,
            usb_driver: None,
            udev_ata_rpm: None,
            vpd_b1: None,
            mmc_type: None,
            smart_rotation_rate: None,
        }
    }
}

/// True for kernel names of virtual block devices (loop, device-mapper,
/// md RAID, zram, ram disks, network block devices).
pub fn is_virtual_name(name: &str) -> bool {
    let digit_after = |p: &str| {
        name.strip_prefix(p)
            .and_then(|r| r.chars().next())
            .is_some_and(|c| c.is_ascii_digit())
    };
    name.starts_with("loop")
        || name.starts_with("dm-")
        || name.starts_with("zram")
        || name.starts_with("nbd")
        || digit_after("ram")
        || digit_after("md")
        || name.starts_with("md_")
}

/// True for the eMMC hardware partitions the kernel exposes as separate
/// disks (`mmcblk0boot0`, `mmcblk0boot1`, `mmcblk0rpmb`).
pub fn is_emmc_hw_partition(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("mmcblk") else {
        return false;
    };
    let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 {
        return false;
    }
    let tail = &rest[digits..];
    tail == "rpmb"
        || tail
            .strip_prefix("boot")
            .is_some_and(|d| !d.is_empty() && d.chars().all(|c| c.is_ascii_digit()))
}

fn lc(s: Option<&str>) -> String {
    s.unwrap_or("").to_ascii_lowercase()
}

/// Model strings typical of USB pen drives.
pub fn looks_like_flash_model(vendor: Option<&str>, model: Option<&str>) -> bool {
    let m = lc(model);
    let v = lc(vendor);
    const HINTS: &[&str] = &[
        "cruzer",
        "flash",
        "datatraveler",
        "jumpdrive",
        "udisk",
        "u disk",
        "usb disk",
        "pen drive",
        "pendrive",
        "thumb",
        "stick",
        "ixpand",
        "traveler",
        "store n go",
    ];
    if m.is_empty() {
        return false;
    }
    if looks_like_ssd_model(model) {
        return false;
    }
    HINTS.iter().any(|h| m.contains(h))
        || (v.contains("sandisk") && (m.starts_with("ultra") || m.contains(" ultra")))
}

/// Model strings typical of SSDs (internal or portable).
pub fn looks_like_ssd_model(model: Option<&str>) -> bool {
    let m = lc(model);
    if m.is_empty() {
        return false;
    }
    m.contains("ssd")
        || m.contains("nvme")
        || m.starts_with("pssd")
        || m.contains("solid state")
        || m.contains("extreme portable")
}

/// Model strings typical of external hard disks.
pub fn looks_like_hdd_model(model: Option<&str>) -> bool {
    let m = lc(model);
    const HINTS: &[&str] = &[
        "expansion",
        "backup plus",
        "my passport",
        "my book",
        "elements",
        "game drive",
        "one touch",
        "canvio",
        "hdd",
        "portable drive",
        "desktop drive",
        "touro",
    ];
    !m.is_empty() && HINTS.iter().any(|h| m.contains(h))
}

fn card_reader_hint(input: &ClassifyInput<'_>) -> Option<Classification> {
    if let Some(b1) = input.vpd_b1 {
        if b1.is_memory_card() {
            return Some(Classification::reader(
                Confidence::High,
                format!(
                    "VPD page 0xB1 product type {} (memory card behind a USB reader)",
                    b1.product_type
                ),
            ));
        }
    }
    let product = format!("{} {}", lc(input.usb_product), lc(input.model));
    let readerish = product.contains("card")
        || product.contains("reader")
        || product.contains("sd/mmc")
        || product.contains("ms/sd")
        || product.contains("microsd")
        || product
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|w| w == "sd");
    // Alcor and Genesys also make pen-drive controllers ("058f:6387 Flash
    // Disk"): a flash-drive name wins over the vendor id.
    let flash_named = looks_like_flash_model(input.vendor, input.model)
        || looks_like_flash_model(input.vendor, input.usb_product);
    if let Some(vid) = input.usb_vendor_id {
        let vid = vid.trim().to_ascii_lowercase();
        if let Some((_, maker)) = CARD_READER_VIDS.iter().find(|(v, _)| *v == vid) {
            if readerish || (input.removable && !flash_named) {
                return Some(Classification::reader(
                    Confidence::Medium,
                    format!(
                        "USB vendor {vid} ({maker}) makes card readers and the medium is removable"
                    ),
                ));
            }
        }
    }
    if product.contains("card reader")
        || product.contains("cardreader")
        || product.contains("sd/mmc")
        || product.contains("ms/sd")
        || product.contains("multi-card")
        || product.contains("multicard")
    {
        return Some(Classification::reader(
            Confidence::Medium,
            format!(
                "USB product string '{}' names a card reader",
                product.trim()
            ),
        ));
    }
    None
}

fn classify_usb(input: &ClassifyInput<'_>) -> Classification {
    if let Some(c) = card_reader_hint(input) {
        return c;
    }
    let flash_name = looks_like_flash_model(input.vendor, input.model);
    let ssd_name = looks_like_ssd_model(input.model);
    let hdd_name = looks_like_hdd_model(input.model);
    let size_gib = input.size as f64 / GIB as f64;

    // Explicit rotation information that made it through the bridge.
    let non_rot = input
        .vpd_b1
        .and_then(|b| b.rotation)
        .map(|r| r == Rotation::NonRotating)
        .or(input.udev_ata_rpm.map(|r| r == 0))
        .or(input.smart_rotation_rate.map(|r| r == 0));
    let reason_src = if input.vpd_b1.and_then(|b| b.rotation).is_some() {
        "VPD page 0xB1 rotation rate"
    } else if input.udev_ata_rpm.is_some() {
        "udev ID_ATA_ROTATION_RATE_RPM"
    } else {
        "SMART rotation_rate"
    };
    match non_rot {
        Some(true) => {
            return if input.removable || flash_name {
                Classification::new(
                    DeviceKind::UsbFlash,
                    Confidence::Medium,
                    format!("{reason_src} says non-rotating and the device is removable or named like a flash drive"),
                )
            } else {
                Classification::new(
                    DeviceKind::UsbSsd,
                    Confidence::High,
                    format!("{reason_src} says non-rotating and the medium is not removable"),
                )
            };
        }
        Some(false) => {
            return Classification::new(
                DeviceKind::UsbHdd,
                Confidence::High,
                format!("{reason_src} reports a spinning disk"),
            );
        }
        None => {}
    }

    // The kernel only reports rotational=0 on USB when the bridge passed
    // VPD 0xB1 with "non-rotating": that is real evidence.
    if input.rotational == Some(false) {
        return if (input.removable && input.size <= USB_FLASH_MAX_SIZE) || flash_name {
            Classification::new(
                DeviceKind::UsbFlash,
                Confidence::Medium,
                format!(
                    "rotational=0 and {}",
                    if flash_name {
                        "a flash-drive model name".to_string()
                    } else {
                        format!(
                            "removable={} with size {size_gib:.1} GiB",
                            input.removable as u8
                        )
                    }
                ),
            )
        } else {
            Classification::new(
                DeviceKind::UsbSsd,
                Confidence::Medium,
                "rotational=0 on a non-removable or large USB device",
            )
        };
    }

    // rotational=1 (the kernel default for USB bridges) or unknown.
    if ssd_name && !input.removable {
        return Classification::new(
            DeviceKind::UsbSsd,
            Confidence::Low,
            format!(
                "model '{}' looks like an SSD although the bridge reports rotational=1",
                input.model.unwrap_or("")
            ),
        );
    }
    if flash_name {
        return Classification::new(
            DeviceKind::UsbFlash,
            Confidence::Medium,
            format!(
                "model '{}' is a flash-drive name",
                input.model.unwrap_or("")
            ),
        );
    }
    if input.removable {
        return if input.size <= USB_FLASH_MAX_SIZE {
            Classification::new(
                DeviceKind::UsbFlash,
                Confidence::Medium,
                format!("removable=1 with size {size_gib:.1} GiB (rotational=1 on USB is only the kernel default)"),
            )
        } else {
            Classification::new(
                DeviceKind::UsbFlash,
                Confidence::Low,
                format!("removable=1 but size {size_gib:.1} GiB is large for a flash drive"),
            )
        };
    }
    if hdd_name {
        return Classification::new(
            DeviceKind::UsbHdd,
            Confidence::Medium,
            format!(
                "non-removable with an external hard disk model name '{}'",
                input.model.unwrap_or("")
            ),
        );
    }
    let driver = input.usb_driver.unwrap_or("unknown driver");
    Classification::new(
        DeviceKind::UsbHdd,
        Confidence::Low,
        format!("non-removable USB disk ({driver}) without rotation information"),
    )
}

fn classify_ata(input: &ClassifyInput<'_>) -> Classification {
    // first_some(vpd_b1.rotation_rate, udev ata rpm, rotational)
    let from_b1 = input.vpd_b1.and_then(|b| b.rotation);
    if let Some(r) = from_b1 {
        return match r {
            Rotation::NonRotating => Classification::new(
                DeviceKind::SataSsd,
                Confidence::High,
                "VPD page 0xB1 rotation rate = 1 (non-rotating)",
            ),
            Rotation::Rpm(rpm) => Classification::new(
                DeviceKind::Hdd,
                Confidence::High,
                format!("VPD page 0xB1 rotation rate = {rpm} rpm"),
            ),
        };
    }
    if let Some(rpm) = input.udev_ata_rpm {
        return if rpm == 0 {
            Classification::new(
                DeviceKind::SataSsd,
                Confidence::High,
                "udev ID_ATA_ROTATION_RATE_RPM=0",
            )
        } else {
            Classification::new(
                DeviceKind::Hdd,
                Confidence::High,
                format!("udev ID_ATA_ROTATION_RATE_RPM={rpm}"),
            )
        };
    }
    match input.rotational {
        Some(false) => Classification::new(
            DeviceKind::SataSsd,
            Confidence::High,
            "ATA device with queue/rotational=0",
        ),
        Some(true) => Classification::new(
            DeviceKind::Hdd,
            Confidence::Medium,
            "ATA device with queue/rotational=1 (rotation rate not reported)",
        ),
        None => Classification::new(
            DeviceKind::Hdd,
            Confidence::Low,
            "ATA device without rotation information: assuming a hard disk",
        ),
    }
}

/// Classify a device. See the module documentation for the rules.
pub fn classify(input: &ClassifyInput<'_>) -> Classification {
    if input.transport == Transport::Virtual || is_virtual_name(input.name) {
        return Classification::new(
            DeviceKind::Virtual,
            Confidence::High,
            format!("virtual block device ({})", input.name),
        );
    }
    if is_emmc_hw_partition(input.name) {
        return Classification::new(
            DeviceKind::Virtual,
            Confidence::High,
            "eMMC hardware boot/RPMB partition (not a data area)",
        );
    }
    if input.name.starts_with("sr") || input.scsi_type == Some(5) {
        return Classification::new(
            DeviceKind::Optical,
            Confidence::High,
            if input.scsi_type == Some(5) {
                "SCSI peripheral type 5 (CD/DVD)"
            } else {
                "sr* device name (CD/DVD)"
            },
        );
    }
    match input.transport {
        Transport::Mmc => {
            let t = input.mmc_type.unwrap_or("").trim();
            if t.eq_ignore_ascii_case("MMC") {
                Classification::new(
                    DeviceKind::Mmc,
                    Confidence::High,
                    "MMC host, card type MMC (eMMC)",
                )
            } else {
                Classification::new(
                    DeviceKind::SdCard,
                    Confidence::High,
                    format!(
                        "MMC host, card type {}",
                        if t.is_empty() {
                            "unknown (assuming SD)"
                        } else {
                            t
                        }
                    ),
                )
            }
        }
        Transport::Nvme => {
            Classification::new(DeviceKind::NvmeSsd, Confidence::High, "NVMe transport")
        }
        Transport::Ata => classify_ata(input),
        Transport::Usb => classify_usb(input),
        Transport::Scsi | Transport::Sas => match input.rotational {
            Some(false) => Classification::new(
                DeviceKind::SataSsd,
                Confidence::Medium,
                "SCSI/SAS device with queue/rotational=0",
            ),
            Some(true) => Classification::new(
                DeviceKind::Hdd,
                Confidence::Medium,
                "SCSI/SAS device with queue/rotational=1",
            ),
            None => Classification::new(
                DeviceKind::Unknown,
                Confidence::Low,
                "SCSI device without rotation information",
            ),
        },
        other => Classification::new(
            DeviceKind::Unknown,
            Confidence::Low,
            format!("transport {other:?}: no classification rule"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usb<'a>(model: &'a str, removable: bool, rot: Option<bool>, size: u64) -> ClassifyInput<'a> {
        let mut i = ClassifyInput::new("sdb", Transport::Usb);
        i.model = Some(model);
        i.removable = removable;
        i.rotational = rot;
        i.size = size;
        i
    }

    #[test]
    fn names() {
        for n in ["loop0", "dm-3", "md127", "md_d0", "zram0", "ram15", "nbd2"] {
            assert!(is_virtual_name(n), "{n}");
        }
        for n in ["sda", "mmcblk0", "nvme0n1", "ramdisk", "mdisk", "sr0"] {
            assert!(!is_virtual_name(n), "{n}");
        }
        assert!(is_emmc_hw_partition("mmcblk0boot0"));
        assert!(is_emmc_hw_partition("mmcblk12rpmb"));
        assert!(!is_emmc_hw_partition("mmcblk0"));
        assert!(!is_emmc_hw_partition("mmcblk0p1"));
        assert!(!is_emmc_hw_partition("mmcblkboot0"));
    }

    #[test]
    fn usb_rules() {
        let c = classify(&usb("Cruzer Blade", true, Some(true), 61 * GIB));
        assert_eq!(c.kind, DeviceKind::UsbFlash);
        assert_eq!(c.confidence, Confidence::Medium);
        let c = classify(&usb("Expansion", false, Some(true), 2000 * GIB));
        assert_eq!(c.kind, DeviceKind::UsbHdd);
        let c = classify(&usb("Some Disk", false, Some(true), 2000 * GIB));
        assert_eq!(
            (c.kind, c.confidence),
            (DeviceKind::UsbHdd, Confidence::Low)
        );
        let c = classify(&usb("PSSD T7", false, Some(false), 1000 * GIB));
        assert_eq!(c.kind, DeviceKind::UsbSsd);
        let c = classify(&usb("Thing", true, Some(false), 32 * GIB));
        assert_eq!(c.kind, DeviceKind::UsbFlash);
        let c = classify(&usb("Portable SSD", false, Some(true), 500 * GIB));
        assert_eq!(
            (c.kind, c.confidence),
            (DeviceKind::UsbSsd, Confidence::Low)
        );
        let c = classify(&usb("Big", true, Some(true), 1000 * GIB));
        assert_eq!(
            (c.kind, c.confidence),
            (DeviceKind::UsbFlash, Confidence::Low)
        );
        // A dead stick reporting 0 bytes is still a flash drive.
        let c = classify(&usb("", true, Some(true), 0));
        assert_eq!(c.kind, DeviceKind::UsbFlash);
        // udev ATA rotation rate through the bridge.
        let mut i = usb("X", false, Some(true), 1000 * GIB);
        i.udev_ata_rpm = Some(0);
        assert_eq!(classify(&i).kind, DeviceKind::UsbSsd);
        i.udev_ata_rpm = Some(5400);
        assert_eq!(
            (classify(&i).kind, classify(&i).confidence),
            (DeviceKind::UsbHdd, Confidence::High)
        );
    }

    #[test]
    fn card_readers() {
        let mut i = usb("STORAGE DEVICE", true, Some(true), 0);
        i.usb_vendor_id = Some("05e3");
        let c = classify(&i);
        assert!(c.card_reader);
        assert_eq!(c.kind, DeviceKind::UsbFlash);
        let i = usb("SD/MMC", true, Some(true), 32 * GIB);
        assert!(classify(&i).card_reader);
        let mut i = usb("Mass Storage", false, Some(true), 32 * GIB);
        let mut b1 = VpdB1 {
            rotation: None,
            product_type: 5,
            form_factor: 0,
            zoned: 0,
        };
        i.vpd_b1 = Some(b1);
        let c = classify(&i);
        assert!(c.card_reader && c.confidence == Confidence::High);
        b1.product_type = 0;
        i.vpd_b1 = Some(b1);
        assert!(!classify(&i).card_reader);
        // An Alcor-controller pen drive is not a card reader.
        let mut i = usb("Flash Disk", true, Some(true), 8 * GIB);
        i.usb_vendor_id = Some("058f");
        i.usb_product = Some("Mass Storage Device");
        let c = classify(&i);
        assert!(!c.card_reader, "{c:?}");
        assert_eq!(c.kind, DeviceKind::UsbFlash);
        // ...while an Alcor reader still is.
        let mut i = usb("Mass-Storage", true, Some(true), 0);
        i.usb_vendor_id = Some("058f");
        assert!(classify(&i).card_reader);
    }

    #[test]
    fn other_transports() {
        let mut i = ClassifyInput::new("sda", Transport::Ata);
        i.rotational = Some(true);
        assert_eq!(classify(&i).kind, DeviceKind::Hdd);
        i.rotational = Some(false);
        assert_eq!(classify(&i).kind, DeviceKind::SataSsd);
        i.vpd_b1 = Some(VpdB1 {
            rotation: Some(Rotation::Rpm(7200)),
            product_type: 0,
            form_factor: 2,
            zoned: 0,
        });
        assert_eq!(classify(&i).kind, DeviceKind::Hdd);
        let i = ClassifyInput::new("nvme0n1", Transport::Nvme);
        assert_eq!(classify(&i).kind, DeviceKind::NvmeSsd);
        let mut i = ClassifyInput::new("mmcblk0", Transport::Mmc);
        i.mmc_type = Some("SD");
        assert_eq!(classify(&i).kind, DeviceKind::SdCard);
        i.mmc_type = Some("MMC");
        assert_eq!(classify(&i).kind, DeviceKind::Mmc);
        let i = ClassifyInput::new("mmcblk0boot0", Transport::Mmc);
        assert_eq!(classify(&i).kind, DeviceKind::Virtual);
        let i = ClassifyInput::new("sr0", Transport::Ata);
        assert_eq!(classify(&i).kind, DeviceKind::Optical);
        let mut i = ClassifyInput::new("sdc", Transport::Usb);
        i.scsi_type = Some(5);
        assert_eq!(classify(&i).kind, DeviceKind::Optical);
        let i = ClassifyInput::new("loop0", Transport::Unknown);
        assert_eq!(classify(&i).kind, DeviceKind::Virtual);
        let i = ClassifyInput::new("vda", Transport::Virtio);
        assert_eq!(classify(&i).kind, DeviceKind::Unknown);
    }

    #[test]
    fn model_hints() {
        assert!(looks_like_flash_model(Some("SanDisk"), Some("Ultra")));
        assert!(!looks_like_flash_model(
            Some("WD"),
            Some("My Passport Ultra")
        ));
        assert!(looks_like_flash_model(None, Some("DataTraveler 3.0")));
        assert!(!looks_like_flash_model(None, Some("Extreme Portable SSD")));
        assert!(looks_like_ssd_model(Some("Samsung PSSD T7")));
        assert!(looks_like_hdd_model(Some("Expansion HDD")));
        assert!(!looks_like_hdd_model(None));
    }
}
