//! Give recovered files back to the user who ran `sudo` (or `pkexec`).
//!
//! The work is done by `diskrescue_app::ownership`: only entries that this
//! run created are changed (owned by root, inside the paths the command
//! writes to, modified since it started); symbolic links are never followed.

#[cfg(all(test, unix))]
use std::time::SystemTime;

pub use diskrescue_app::ownership::{hand_back, image_and_sidecars};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidecars_are_found() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("card.img");
        for name in [
            "card.img",
            "card.img.map",
            "card.img.map.bak",
            "card.img.map.device.json",
            "other.txt",
        ] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        let got = image_and_sidecars(&img, None);
        let names: Vec<String> = got
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        for want in [
            "card.img",
            "card.img.map",
            "card.img.map.bak",
            "card.img.map.device.json",
        ] {
            assert!(names.iter().any(|n| n == want), "{want} in {names:?}");
        }
        assert!(!names.iter().any(|n| n == "other.txt"));
    }

    #[cfg(unix)]
    #[test]
    fn hand_back_is_a_noop_when_not_root_under_sudo() {
        // The test suite never runs as root; this must simply do nothing.
        let dir = tempfile::tempdir().unwrap();
        hand_back(&[dir.path().to_path_buf()], SystemTime::now());
    }
}
