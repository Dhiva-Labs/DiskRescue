//! What we know about a physical device.
//!
//! Filled in by the `diskrescue-device` crate on each platform; consumed by
//! the imaging engine (to pick a profile), the CLI (to list devices) and the
//! report.

use std::path::PathBuf;

/// Physical class of a device. Drives the imaging strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceKind {
    /// Spinning hard disk on SATA/SAS/IDE.
    Hdd,
    /// Solid-state disk on SATA/SAS.
    SataSsd,
    /// NVMe solid-state disk.
    NvmeSsd,
    /// USB flash drive (pen drive / thumb drive) or a USB card reader.
    UsbFlash,
    /// Spinning hard disk behind a USB bridge.
    UsbHdd,
    /// Solid-state disk behind a USB bridge (detected best-effort).
    UsbSsd,
    /// SD / SDHC / SDXC card on a native (MMC) host controller.
    SdCard,
    /// eMMC / MMC storage.
    Mmc,
    /// CD/DVD/BD drive.
    Optical,
    /// Loop, device-mapper, RAID, zram and other virtual devices.
    Virtual,
    /// A regular file used as an image.
    ImageFile,
    /// Could not be classified.
    Unknown,
}

impl DeviceKind {
    /// Human-readable name.
    pub fn display_name(&self) -> &'static str {
        match self {
            DeviceKind::Hdd => "hard disk (HDD)",
            DeviceKind::SataSsd => "SATA SSD",
            DeviceKind::NvmeSsd => "NVMe SSD",
            DeviceKind::UsbFlash => "USB flash drive",
            DeviceKind::UsbHdd => "USB hard disk",
            DeviceKind::UsbSsd => "USB SSD",
            DeviceKind::SdCard => "SD card",
            DeviceKind::Mmc => "eMMC/MMC",
            DeviceKind::Optical => "optical drive",
            DeviceKind::Virtual => "virtual device",
            DeviceKind::ImageFile => "image file",
            DeviceKind::Unknown => "unknown device",
        }
    }

    /// NAND-flash based media: controller lock-ups and bus drops are the
    /// typical failure mode rather than clustered bad sectors.
    pub fn is_flash(&self) -> bool {
        matches!(
            self,
            DeviceKind::SataSsd
                | DeviceKind::NvmeSsd
                | DeviceKind::UsbFlash
                | DeviceKind::UsbSsd
                | DeviceKind::SdCard
                | DeviceKind::Mmc
        )
    }

    /// Spinning media: head seeks are expensive and bad areas cluster.
    pub fn is_rotational(&self) -> bool {
        matches!(self, DeviceKind::Hdd | DeviceKind::UsbHdd)
    }

    /// Attached over USB: may drop off the bus and reappear under another
    /// name, so the source must be re-found by serial number.
    pub fn is_usb(&self) -> bool {
        matches!(
            self,
            DeviceKind::UsbFlash | DeviceKind::UsbHdd | DeviceKind::UsbSsd
        )
    }

    /// Removable card media.
    pub fn is_card(&self) -> bool {
        matches!(self, DeviceKind::SdCard | DeviceKind::Mmc)
    }
}

impl std::fmt::Display for DeviceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.display_name())
    }
}

/// How the device is attached to the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    Ata,
    Sas,
    Nvme,
    Usb,
    Mmc,
    Virtio,
    Scsi,
    Firewire,
    Thunderbolt,
    Virtual,
    Unknown,
}

/// A partition as reported by the OS (not parsed by us).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PartitionInfo {
    /// Device path of the partition node (`/dev/sdb1`), if the OS exposes one.
    pub path: Option<PathBuf>,
    /// Partition number as the OS counts it.
    pub number: u32,
    /// Byte offset from the start of the parent device.
    pub start: u64,
    /// Size in bytes.
    pub size: u64,
    /// Filesystem type name as reported by the OS, if known.
    pub fs_type: Option<String>,
    /// Where it is mounted, if it is.
    pub mountpoints: Vec<PathBuf>,
}

/// Optional health summary from SMART, when a tool to read it is present.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HealthInfo {
    /// Overall self-assessment passed, when reported.
    pub passed: Option<bool>,
    /// Reallocated sector count (attribute 5).
    pub reallocated_sectors: Option<u64>,
    /// Current pending sectors (attribute 197).
    pub pending_sectors: Option<u64>,
    /// Offline uncorrectable sectors (attribute 198).
    pub uncorrectable_sectors: Option<u64>,
    /// Temperature in Celsius.
    pub temperature_c: Option<i32>,
    /// Power-on hours.
    pub power_on_hours: Option<u64>,
    /// Percentage of rated life used (NVMe) or wear indicator.
    pub percentage_used: Option<u8>,
    /// Free-form notes ("media errors: 3", "read-only mode").
    pub notes: Vec<String>,
}

/// Everything known about a block device.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DeviceInfo {
    /// Device node path (`/dev/sdb`, `\\.\PhysicalDrive2`).
    pub path: PathBuf,
    /// Short name (`sdb`, `PhysicalDrive2`).
    pub name: String,
    /// Classification.
    pub kind: DeviceKind,
    /// Attachment.
    pub transport: Transport,
    /// Size in bytes (0 when the device reports none: typical for a flash
    /// drive whose controller died).
    pub size: u64,
    /// Logical sector size.
    pub logical_sector: u32,
    /// Physical sector size.
    pub physical_sector: u32,
    /// Model string, trimmed.
    pub model: Option<String>,
    /// Vendor string, trimmed.
    pub vendor: Option<String>,
    /// Serial number, trimmed. Used to re-find USB devices after a reset.
    pub serial: Option<String>,
    /// Firmware revision.
    pub firmware: Option<String>,
    /// Stable identifier such as a `/dev/disk/by-id/...` name.
    pub stable_id: Option<String>,
    /// OS says removable.
    pub removable: bool,
    /// OS says rotational.
    pub rotational: bool,
    /// OS has the device marked read-only.
    pub read_only: bool,
    /// Partitions the OS knows about.
    pub partitions: Vec<PartitionInfo>,
    /// Mountpoints of the whole device (rare) plus all of its partitions.
    pub mountpoints: Vec<PathBuf>,
    /// SMART summary, when available.
    pub health: Option<HealthInfo>,
    /// Warnings worth showing ("reports 0 bytes: controller failure",
    /// "USB bridge may misreport rotational flag").
    pub warnings: Vec<String>,
}

impl DeviceInfo {
    /// True when any partition or the device itself is mounted.
    pub fn is_mounted(&self) -> bool {
        !self.mountpoints.is_empty()
    }

    /// One-line summary for listings.
    pub fn summary(&self) -> String {
        let model = self.model.clone().unwrap_or_else(|| "unknown model".into());
        format!(
            "{} {} {} ({})",
            self.path.display(),
            crate::util::human::bytes(self.size),
            model,
            self.kind.display_name()
        )
    }
}
