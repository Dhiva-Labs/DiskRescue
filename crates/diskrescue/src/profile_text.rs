//! Plain-language descriptions of the imaging profiles (shared with the
//! other front-ends through `diskrescue_app::text`).

#[cfg(test)]
use diskrescue_core::profile::ImagingProfile;

pub use diskrescue_app::text::{profile_blurb as blurb, profile_details as details};

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
