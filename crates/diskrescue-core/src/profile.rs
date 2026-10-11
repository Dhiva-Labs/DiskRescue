//! Imaging strategy per device kind.
//!
//! A profile is a bag of parameters consumed by the imaging engine. The
//! defaults encode what works on real failing media; users override single
//! fields from the CLI.

use std::time::Duration;

use crate::device::DeviceKind;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;

/// Parameters of the imaging engine for one kind of device.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ImagingProfile {
    /// Name shown in the UI (`hdd`, `ssd`, `usb-flash`, ...).
    pub name: String,
    /// Read size for the copying pass, in bytes. Always rounded to the
    /// sector size by the engine.
    pub block_size: u64,
    /// Smallest read used when scraping a bad area. 0 means one sector.
    pub min_read_size: u64,
    /// Bytes to skip ahead after a failed read in the copying pass.
    pub skip_initial: u64,
    /// Upper bound for the skip distance.
    pub skip_max: u64,
    /// Multiplier applied to the skip distance on consecutive errors
    /// (1.0 disables growth). Reset after a successful read.
    pub skip_growth: f32,
    /// Give up on a single read after this long and treat it as an error.
    pub read_timeout: Duration,
    /// A read slower than this (bytes/second) is counted as "slow"; after
    /// `slow_reads_before_skip` of them in a row the engine skips ahead as
    /// if an error occurred. `None` disables.
    pub slow_read_rate: Option<u64>,
    /// See `slow_read_rate`.
    pub slow_reads_before_skip: u32,
    /// Number of extra retry passes over bad sectors after scraping.
    pub retry_passes: u32,
    /// Run the second copying pass backwards (helps drives whose heads
    /// struggle in one direction and avoids re-hitting the same bad area
    /// from the same side).
    pub reverse_second_pass: bool,
    /// Perform the trimming pass.
    pub trim: bool,
    /// Perform the scraping pass.
    pub scrape: bool,
    /// Re-open the device after this many consecutive errors (USB bridges
    /// and flash controllers often need it). `None` disables.
    pub reopen_after_errors: Option<u32>,
    /// How long to wait for a vanished device to come back before giving up.
    pub wait_for_reattach: Duration,
    /// Pause for this long after every `pause_after_errors` consecutive errors
    /// to let an overheating or struggling drive recover.
    pub pause_after_errors: Option<u32>,
    /// See `pause_after_errors`.
    pub pause_duration: Duration,
    /// Use direct I/O (bypass the page cache). Required to see real device
    /// errors instead of cached data; falls back automatically when the OS
    /// refuses.
    pub direct_io: bool,
    /// Stop imaging after this many errors in the copying pass (0 = never).
    /// A safety valve for devices that are getting worse while read.
    pub max_errors: u64,
}

impl ImagingProfile {
    /// Profile recommended for `kind`.
    pub fn for_kind(kind: DeviceKind) -> Self {
        match kind {
            DeviceKind::Hdd => ImagingProfile::hdd(),
            DeviceKind::UsbHdd => ImagingProfile::usb_hdd(),
            DeviceKind::SataSsd | DeviceKind::NvmeSsd => ImagingProfile::ssd(),
            DeviceKind::UsbSsd => ImagingProfile::usb_ssd(),
            DeviceKind::UsbFlash => ImagingProfile::usb_flash(),
            DeviceKind::SdCard | DeviceKind::Mmc => ImagingProfile::sd_card(),
            DeviceKind::Optical => ImagingProfile::optical(),
            DeviceKind::Virtual | DeviceKind::ImageFile | DeviceKind::Unknown => {
                ImagingProfile::generic()
            }
        }
    }

    /// Look up a profile by its name as typed on the command line.
    pub fn by_name(name: &str) -> Option<Self> {
        Some(match name.to_ascii_lowercase().as_str() {
            "hdd" => ImagingProfile::hdd(),
            "usb-hdd" | "usbhdd" => ImagingProfile::usb_hdd(),
            "ssd" => ImagingProfile::ssd(),
            "usb-ssd" | "usbssd" => ImagingProfile::usb_ssd(),
            "usb-flash" | "usb" | "pendrive" | "flash" => ImagingProfile::usb_flash(),
            "sd" | "sd-card" | "sdcard" | "mmc" => ImagingProfile::sd_card(),
            "optical" | "cd" | "dvd" => ImagingProfile::optical(),
            "generic" | "file" | "image" => ImagingProfile::generic(),
            _ => return None,
        })
    }

    /// All profile names, for help text.
    pub fn names() -> &'static [&'static str] {
        &[
            "hdd",
            "usb-hdd",
            "ssd",
            "usb-ssd",
            "usb-flash",
            "sd",
            "optical",
            "generic",
        ]
    }

    /// Spinning disk: large sequential reads, aggressive skipping so the
    /// first pass grabs everything healthy before the drive degrades, a
    /// reverse pass, then fine-grained scraping.
    pub fn hdd() -> Self {
        ImagingProfile {
            name: "hdd".into(),
            block_size: MIB,
            min_read_size: 0,
            skip_initial: 64 * KIB,
            skip_max: 256 * MIB,
            skip_growth: 2.0,
            read_timeout: Duration::from_secs(30),
            slow_read_rate: None,
            slow_reads_before_skip: 3,
            retry_passes: 1,
            reverse_second_pass: true,
            trim: true,
            scrape: true,
            reopen_after_errors: None,
            wait_for_reattach: Duration::from_secs(60),
            pause_after_errors: Some(50),
            pause_duration: Duration::from_secs(5),
            direct_io: true,
            max_errors: 0,
        }
    }

    /// HDD behind a USB bridge: like HDD but the bridge may reset.
    pub fn usb_hdd() -> Self {
        ImagingProfile {
            name: "usb-hdd".into(),
            reopen_after_errors: Some(8),
            wait_for_reattach: Duration::from_secs(120),
            ..ImagingProfile::hdd()
        }
    }

    /// SATA/NVMe SSD: failures are page-sized and scattered, seeks are free,
    /// so skip little and retry more; short timeouts.
    pub fn ssd() -> Self {
        ImagingProfile {
            name: "ssd".into(),
            block_size: MIB,
            min_read_size: 0,
            skip_initial: 64 * KIB,
            skip_max: 4 * MIB,
            skip_growth: 1.5,
            read_timeout: Duration::from_secs(8),
            slow_read_rate: None,
            slow_reads_before_skip: 3,
            retry_passes: 2,
            reverse_second_pass: false,
            trim: true,
            scrape: true,
            reopen_after_errors: None,
            wait_for_reattach: Duration::from_secs(60),
            pause_after_errors: None,
            pause_duration: Duration::from_secs(2),
            direct_io: true,
            max_errors: 0,
        }
    }

    /// SSD behind a USB bridge.
    pub fn usb_ssd() -> Self {
        ImagingProfile {
            name: "usb-ssd".into(),
            reopen_after_errors: Some(5),
            wait_for_reattach: Duration::from_secs(120),
            ..ImagingProfile::ssd()
        }
    }

    /// USB pen drive: small transfers, short timeouts, re-open the device
    /// after a burst of errors, wait patiently for it to re-enumerate.
    pub fn usb_flash() -> Self {
        ImagingProfile {
            name: "usb-flash".into(),
            block_size: 128 * KIB,
            min_read_size: 0,
            skip_initial: 128 * KIB,
            skip_max: 16 * MIB,
            skip_growth: 2.0,
            read_timeout: Duration::from_secs(10),
            slow_read_rate: None,
            slow_reads_before_skip: 3,
            retry_passes: 1,
            reverse_second_pass: false,
            trim: true,
            scrape: true,
            reopen_after_errors: Some(3),
            wait_for_reattach: Duration::from_secs(180),
            pause_after_errors: Some(20),
            pause_duration: Duration::from_secs(3),
            direct_io: true,
            max_errors: 0,
        }
    }

    /// SD/MMC card on a native host or reader.
    pub fn sd_card() -> Self {
        ImagingProfile {
            name: "sd".into(),
            ..ImagingProfile::usb_flash()
        }
    }

    /// Optical media: 2048-byte sectors, long timeouts.
    pub fn optical() -> Self {
        ImagingProfile {
            name: "optical".into(),
            block_size: 64 * KIB,
            min_read_size: 2048,
            skip_initial: 64 * KIB,
            skip_max: 32 * MIB,
            skip_growth: 2.0,
            read_timeout: Duration::from_secs(60),
            slow_read_rate: None,
            slow_reads_before_skip: 3,
            retry_passes: 1,
            reverse_second_pass: false,
            trim: true,
            scrape: true,
            reopen_after_errors: None,
            wait_for_reattach: Duration::from_secs(30),
            pause_after_errors: None,
            pause_duration: Duration::from_secs(1),
            direct_io: false,
            max_errors: 0,
        }
    }

    /// Image files and unknown devices: no timeouts worth speaking of.
    pub fn generic() -> Self {
        ImagingProfile {
            name: "generic".into(),
            block_size: MIB,
            min_read_size: 0,
            skip_initial: 64 * KIB,
            skip_max: 64 * MIB,
            skip_growth: 2.0,
            read_timeout: Duration::from_secs(120),
            slow_read_rate: None,
            slow_reads_before_skip: 3,
            retry_passes: 1,
            reverse_second_pass: false,
            trim: true,
            scrape: true,
            reopen_after_errors: None,
            wait_for_reattach: Duration::from_secs(30),
            pause_after_errors: None,
            pause_duration: Duration::from_secs(1),
            direct_io: false,
            max_errors: 0,
        }
    }
}

impl Default for ImagingProfile {
    fn default() -> Self {
        ImagingProfile::generic()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_has_a_profile_and_names_resolve() {
        for kind in [
            DeviceKind::Hdd,
            DeviceKind::SataSsd,
            DeviceKind::NvmeSsd,
            DeviceKind::UsbFlash,
            DeviceKind::UsbHdd,
            DeviceKind::UsbSsd,
            DeviceKind::SdCard,
            DeviceKind::Mmc,
            DeviceKind::Optical,
            DeviceKind::Virtual,
            DeviceKind::ImageFile,
            DeviceKind::Unknown,
        ] {
            let p = ImagingProfile::for_kind(kind);
            assert!(p.block_size >= 512);
            assert!(p.skip_initial > 0);
            assert!(p.skip_max >= p.skip_initial);
        }
        for n in ImagingProfile::names() {
            assert!(ImagingProfile::by_name(n).is_some(), "{n}");
        }
        assert!(ImagingProfile::by_name("floppy").is_none());
    }
}
