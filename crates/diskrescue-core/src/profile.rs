//! Imaging strategy per device kind.
//!
//! A profile is a bag of parameters consumed by the imaging engine. The
//! defaults encode what works on real failing media (see
//! `docs/specs/imaging-engine.md`, section 3.9); users override single
//! fields from the command line.

use std::time::Duration;

use crate::device::DeviceKind;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;

/// Parameters of the imaging engine for one kind of device.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ImagingProfile {
    /// Name shown in the UI (`hdd`, `ssd`, `usb-flash`, ...).
    pub name: String,
    /// Read size ("cluster") for the copying passes, in bytes. The engine
    /// rounds it to the sector size and clamps it to what the device
    /// accepts (`max_sectors_kb`).
    pub block_size: u64,
    /// Smallest read used when scraping a bad area. 0 means one sector.
    pub min_read_size: u64,
    /// Absolute lower bound for the initial skip after a failed read.
    pub skip_initial: u64,
    /// When non-zero, the initial skip is `max(skip_initial, size / divisor)`
    /// (ddrescue uses 100000, so a 2 TB disk skips 20 MiB on its first error).
    pub skip_initial_divisor: u64,
    /// Upper bound for the skip distance as a percentage of the source size
    /// (ddrescue: 1 %).
    pub skip_max_percent: f32,
    /// Absolute upper bound for the skip distance (applies together with
    /// `skip_max_percent`; the smaller wins).
    pub skip_max: u64,
    /// Multiplier applied to the skip distance on consecutive errors
    /// (1.0 disables growth). Reset after a successful read.
    pub skip_growth: f32,
    /// Per-read timeout in the copying passes.
    pub timeout_copy: Duration,
    /// Per-read timeout while trimming / dividing / scraping.
    pub timeout_scrape: Duration,
    /// Per-read timeout in the retry passes.
    pub timeout_retry: Duration,
    /// A read slower than this (bytes/second) is "slow"; after
    /// `slow_reads_before_skip` of them in a row the engine skips ahead as
    /// if an error occurred. `None` disables slow-skipping. `Some(0)` means
    /// automatic (a tenth of the average rate so far).
    pub min_read_rate: Option<u64>,
    /// See `min_read_rate`.
    pub slow_reads_before_skip: u32,
    /// Number of retry passes over bad sectors after scraping. Directions
    /// alternate between passes.
    pub retry_passes: u32,
    /// Run the second copying pass backwards (ddrescue default).
    pub reverse_second_pass: bool,
    /// Perform the trimming pass (sector-by-sector from both ends of each
    /// bad area). Off for flash media where failures are page sized.
    pub trim: bool,
    /// Before scraping, re-read each bad area in `block_size / 8` pieces to
    /// locate the dead page quickly (OpenSuperClone's "dividing").
    pub divide: bool,
    /// Perform the scraping pass (one sector at a time).
    pub scrape: bool,
    /// After every read error, re-read the last good block to detect
    /// bridges and controllers that return stale or zeroed data after a
    /// fault (ddrescue `-J`).
    pub check_on_error: bool,
    /// Re-open the device after this many consecutive errors (USB bridges
    /// and flash controllers often need it). `None` disables.
    pub reopen_after_errors: Option<u32>,
    /// How long to wait for a vanished device to come back before giving up.
    /// `None` waits indefinitely (the user can always cancel).
    pub wait_for_reattach: Option<Duration>,
    /// Give up (with advice to try another port/cable/reader) after this
    /// many re-attaches. 0 = unlimited.
    pub max_reattaches: u32,
    /// Sleep this long after every read error (lets a bridge recover).
    pub pause_on_error: Duration,
    /// Pause for `pause_duration` after every `pause_after_errors`
    /// consecutive errors to let an overheating or struggling drive recover.
    pub pause_after_errors: Option<u32>,
    /// See `pause_after_errors`.
    pub pause_duration: Duration,
    /// Use direct I/O (bypass the page cache). Required to see real device
    /// errors instead of cached data; the engine falls back automatically
    /// when the OS refuses.
    pub direct_io: bool,
    /// Stop imaging after this many errors in the copying passes (0 = never).
    /// A safety valve for devices that are getting worse while read.
    pub max_errors: u64,
    /// Stop after this many slow reads in total (0 = never). Useful for
    /// drives that recover after a power cycle.
    pub max_slow_reads: u64,
}

impl ImagingProfile {
    /// Profile recommended for `kind`.
    pub fn for_kind(kind: DeviceKind) -> Self {
        match kind {
            DeviceKind::Hdd => ImagingProfile::hdd(),
            DeviceKind::UsbHdd => ImagingProfile::usb_hdd(),
            DeviceKind::SataSsd => ImagingProfile::ssd(),
            DeviceKind::NvmeSsd => ImagingProfile::nvme(),
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
            "ssd" | "sata-ssd" => ImagingProfile::ssd(),
            "nvme" | "nvme-ssd" => ImagingProfile::nvme(),
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
            "nvme",
            "usb-ssd",
            "usb-flash",
            "sd",
            "optical",
            "generic",
        ]
    }

    /// Initial skip distance for a source of `size` bytes.
    pub fn initial_skip(&self, size: u64) -> u64 {
        let rel = if self.skip_initial_divisor > 0 {
            size / self.skip_initial_divisor
        } else {
            0
        };
        self.skip_initial.max(rel)
    }

    /// Maximum skip distance for a source of `size` bytes.
    pub fn max_skip(&self, size: u64) -> u64 {
        let pct = ((size as f64) * (self.skip_max_percent as f64) / 100.0) as u64;
        let cap = if pct == 0 {
            self.skip_max
        } else {
            pct.min(self.skip_max)
        };
        cap.max(self.skip_initial)
    }

    /// Spinning disk: errors cluster (scratches, weak heads), every read in
    /// a bad zone costs seconds and physical wear. Small clusters so errors
    /// are localised, exponential skipping, second pass backwards, fine
    /// work (trim/scrape) last and once.
    pub fn hdd() -> Self {
        ImagingProfile {
            name: "hdd".into(),
            block_size: 64 * KIB,
            min_read_size: 0,
            skip_initial: 64 * KIB,
            skip_initial_divisor: 100_000,
            skip_max_percent: 1.0,
            skip_max: 4 * 1024 * MIB,
            skip_growth: 2.0,
            timeout_copy: Duration::from_secs(15),
            timeout_scrape: Duration::from_secs(30),
            timeout_retry: Duration::from_secs(45),
            min_read_rate: Some(0),
            slow_reads_before_skip: 3,
            retry_passes: 2,
            reverse_second_pass: true,
            trim: true,
            divide: false,
            scrape: true,
            check_on_error: false,
            reopen_after_errors: None,
            wait_for_reattach: Some(Duration::from_secs(600)),
            max_reattaches: 0,
            pause_on_error: Duration::ZERO,
            pause_after_errors: Some(50),
            pause_duration: Duration::from_secs(5),
            direct_io: true,
            max_errors: 0,
            max_slow_reads: 0,
        }
    }

    /// HDD behind a USB bridge: like HDD but the bridge may wedge or reset.
    pub fn usb_hdd() -> Self {
        ImagingProfile {
            name: "usb-hdd".into(),
            block_size: 64 * KIB,
            timeout_copy: Duration::from_secs(20),
            check_on_error: true,
            reopen_after_errors: Some(4),
            wait_for_reattach: None,
            max_reattaches: 0,
            pause_on_error: Duration::from_millis(500),
            ..ImagingProfile::hdd()
        }
    }

    /// SATA SSD: failures are page/block granular and scattered, seeks are
    /// free, slowness is ECC retry that still returns good data. Large
    /// reads, no trimming (divide instead), bounded retries.
    pub fn ssd() -> Self {
        ImagingProfile {
            name: "ssd".into(),
            block_size: 256 * KIB,
            min_read_size: 0,
            skip_initial: MIB,
            skip_initial_divisor: 0,
            skip_max_percent: 1.0,
            skip_max: 4 * 1024 * MIB,
            skip_growth: 2.0,
            timeout_copy: Duration::from_secs(5),
            timeout_scrape: Duration::from_secs(10),
            timeout_retry: Duration::from_secs(20),
            min_read_rate: None,
            slow_reads_before_skip: 3,
            retry_passes: 3,
            reverse_second_pass: false,
            trim: false,
            divide: true,
            scrape: true,
            check_on_error: false,
            reopen_after_errors: None,
            wait_for_reattach: Some(Duration::from_secs(600)),
            max_reattaches: 0,
            pause_on_error: Duration::ZERO,
            pause_after_errors: None,
            pause_duration: Duration::from_secs(2),
            direct_io: true,
            max_errors: 0,
            max_slow_reads: 0,
        }
    }

    /// NVMe SSD: as SATA SSD with larger reads and shorter timeouts.
    pub fn nvme() -> Self {
        ImagingProfile {
            name: "nvme".into(),
            block_size: 512 * KIB,
            timeout_copy: Duration::from_secs(3),
            ..ImagingProfile::ssd()
        }
    }

    /// SSD behind a USB bridge.
    pub fn usb_ssd() -> Self {
        ImagingProfile {
            name: "usb-ssd".into(),
            block_size: 64 * KIB,
            timeout_copy: Duration::from_secs(10),
            timeout_scrape: Duration::from_secs(20),
            timeout_retry: Duration::from_secs(30),
            check_on_error: true,
            reopen_after_errors: Some(4),
            wait_for_reattach: None,
            pause_on_error: Duration::from_millis(500),
            ..ImagingProfile::ssd()
        }
    }

    /// USB pen drive: the controller or bridge is the usual failure. Small
    /// transfers, short timeouts, slow-skip, re-open on error bursts, wait
    /// for re-enumeration, verify data after errors.
    pub fn usb_flash() -> Self {
        ImagingProfile {
            name: "usb-flash".into(),
            block_size: 64 * KIB,
            min_read_size: 0,
            skip_initial: MIB,
            skip_initial_divisor: 0,
            skip_max_percent: 1.0,
            skip_max: 1024 * MIB,
            skip_growth: 2.0,
            timeout_copy: Duration::from_secs(10),
            timeout_scrape: Duration::from_secs(20),
            timeout_retry: Duration::from_secs(30),
            min_read_rate: Some(MIB),
            slow_reads_before_skip: 3,
            retry_passes: 1,
            reverse_second_pass: false,
            trim: true,
            divide: false,
            scrape: true,
            check_on_error: true,
            reopen_after_errors: Some(2),
            wait_for_reattach: None,
            max_reattaches: 0,
            pause_on_error: Duration::from_millis(500),
            pause_after_errors: Some(20),
            pause_duration: Duration::from_secs(3),
            direct_io: true,
            max_errors: 0,
            max_slow_reads: 0,
        }
    }

    /// SD/MMC card on a native host or a USB reader: readers cap transfers
    /// at 64 KiB and cards time out internally on long reads.
    pub fn sd_card() -> Self {
        ImagingProfile {
            name: "sd".into(),
            block_size: 32 * KIB,
            skip_initial: 512 * KIB,
            min_read_rate: Some(512 * KIB),
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
            skip_initial_divisor: 0,
            skip_max_percent: 1.0,
            skip_max: 32 * MIB,
            skip_growth: 2.0,
            timeout_copy: Duration::from_secs(60),
            timeout_scrape: Duration::from_secs(90),
            timeout_retry: Duration::from_secs(120),
            min_read_rate: None,
            slow_reads_before_skip: 3,
            retry_passes: 1,
            reverse_second_pass: false,
            trim: true,
            divide: false,
            scrape: true,
            check_on_error: false,
            reopen_after_errors: None,
            wait_for_reattach: Some(Duration::from_secs(60)),
            max_reattaches: 0,
            pause_on_error: Duration::ZERO,
            pause_after_errors: None,
            pause_duration: Duration::from_secs(1),
            direct_io: false,
            max_errors: 0,
            max_slow_reads: 0,
        }
    }

    /// Image files and unknown devices: ddrescue defaults, no device tricks.
    pub fn generic() -> Self {
        ImagingProfile {
            name: "generic".into(),
            block_size: 64 * KIB,
            min_read_size: 0,
            skip_initial: 64 * KIB,
            skip_initial_divisor: 100_000,
            skip_max_percent: 1.0,
            skip_max: 4 * 1024 * MIB,
            skip_growth: 2.0,
            timeout_copy: Duration::from_secs(120),
            timeout_scrape: Duration::from_secs(120),
            timeout_retry: Duration::from_secs(120),
            min_read_rate: None,
            slow_reads_before_skip: 3,
            retry_passes: 1,
            reverse_second_pass: true,
            trim: true,
            divide: false,
            scrape: true,
            check_on_error: false,
            reopen_after_errors: None,
            wait_for_reattach: Some(Duration::from_secs(60)),
            max_reattaches: 0,
            pause_on_error: Duration::ZERO,
            pause_after_errors: None,
            pause_duration: Duration::from_secs(1),
            direct_io: false,
            max_errors: 0,
            max_slow_reads: 0,
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
            assert!(p.max_skip(1 << 40) >= p.initial_skip(1 << 40));
        }
        for n in ImagingProfile::names() {
            assert!(ImagingProfile::by_name(n).is_some(), "{n}");
        }
        assert!(ImagingProfile::by_name("floppy").is_none());
    }

    #[test]
    fn skip_arithmetic_follows_ddrescue() {
        let p = ImagingProfile::hdd();
        // 2 TB disk: initial skip = size / 100000 = 20 MiB-ish, max = 1 %.
        let size = 2_000_000_000_000u64;
        assert_eq!(p.initial_skip(size), size / 100_000);
        assert_eq!(p.max_skip(size), size / 100);
        // Tiny image: lower bounds apply.
        assert_eq!(p.initial_skip(1 << 20), 64 * KIB);
        assert_eq!(p.max_skip(1 << 20), 64 * KIB);
        let s = ImagingProfile::ssd();
        assert_eq!(s.initial_skip(size), MIB);
        assert!(!s.trim && s.divide);
        assert!(ImagingProfile::usb_flash().wait_for_reattach.is_none());
    }
}
