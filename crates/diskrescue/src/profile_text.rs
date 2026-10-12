//! Plain-language descriptions of the imaging profiles.

use diskrescue_core::profile::ImagingProfile;
use diskrescue_core::util::human;

/// What a profile is for and does, in one sentence (matches `docs/USAGE.md`).
pub fn blurb(name: &str) -> &'static str {
    match name {
        "hdd" => "64 KiB reads, exponential skipping over bad areas, second pass backwards, sector-level scraping last",
        "usb-hdd" => "as hdd, plus re-opening the device when the USB bridge stalls",
        "ssd" | "nvme" => "large reads, short timeouts, no trimming (flash fails in whole pages), more retry passes",
        "usb-ssd" => "as ssd, with bridge handling",
        "usb-flash" => "small reads, 10 s timeouts, re-opens and waits for the drive to come back after it drops off the bus, re-checks data after errors",
        "sd" => "as usb-flash with 32 KiB reads",
        "optical" => "2 KiB sectors, long timeouts, for CD, DVD and Blu-ray drives",
        _ => "GNU ddrescue's defaults",
    }
}

fn secs(d: std::time::Duration) -> String {
    if d.subsec_millis() == 0 {
        format!("{} s", d.as_secs())
    } else {
        format!("{:.1} s", d.as_secs_f64())
    }
}

/// The numbers that matter, for `info` and the start of an imaging run.
pub fn details(p: &ImagingProfile) -> String {
    let mut parts = vec![
        format!("reads of {}", human::bytes(p.block_size)),
        format!(
            "timeouts {} / {} / {} (copy / scrape / retry)",
            secs(p.timeout_copy),
            secs(p.timeout_scrape),
            secs(p.timeout_retry)
        ),
        format!("{} retry pass(es)", p.retry_passes),
    ];
    parts.push(if p.trim {
        "trimming on".into()
    } else {
        "trimming off".into()
    });
    parts.push(if p.scrape {
        "scraping on".into()
    } else {
        "scraping off".into()
    });
    if p.reopen_after_errors.is_some() {
        parts.push("re-opens the device after repeated errors".into());
    }
    parts.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_profile_has_text() {
        for n in ImagingProfile::names() {
            let p = ImagingProfile::by_name(n).unwrap();
            assert!(!blurb(n).is_empty());
            let d = details(&p);
            assert!(d.contains("reads of") && d.contains("timeouts"), "{d}");
        }
    }

    #[test]
    fn flash_profile_mentions_reopen() {
        assert!(details(&ImagingProfile::usb_flash()).contains("re-opens"));
        assert!(!details(&ImagingProfile::hdd()).contains("re-opens"));
    }
}
