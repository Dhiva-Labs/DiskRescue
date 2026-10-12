//! Opening the SOURCE argument: a device, a partition or an image file.
//!
//! The work is done by `diskrescue_app::source` (read-only, mapfile
//! attached automatically, safety checks against the output); this module
//! turns its errors into command-line errors.

#[cfg(test)]
use std::path::{Path, PathBuf};

pub use diskrescue_app::source::{default_map_path, Opened, SourceRequest};

use crate::error::CliError;

/// Open the source described by `req`.
pub fn open(req: &SourceRequest<'_>) -> Result<Opened, CliError> {
    diskrescue_app::source::open(req).map_err(CliError::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_map_is_appended() {
        assert_eq!(
            default_map_path(Path::new("/x/card.img")),
            PathBuf::from("/x/card.img.map")
        );
    }

    #[test]
    fn missing_source_gets_a_helpful_message() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("nothing.img");
        let req = SourceRequest::reading(&p);
        let err = open(&req).err().expect("must fail");
        let m = err.to_string();
        assert!(
            m.contains("does not exist") && m.contains("diskrescue list"),
            "{m}"
        );
    }

    #[test]
    fn folder_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let req = SourceRequest::reading(dir.path());
        let err = open(&req).err().expect("must fail");
        assert!(err.to_string().contains("is a folder"));
    }

    #[test]
    fn image_with_mapfile_knows_its_holes() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("a.img");
        std::fs::write(&img, vec![7u8; 4096]).unwrap();
        let map = default_map_path(&img);
        std::fs::write(
            &map,
            "# Mapfile. Created by test\n# current_pos  current_status  current_pass\n0x00000000  +  1\n#      pos        size  status\n0x00000000  0x00000800  +\n0x00000800  0x00000800  -\n",
        )
        .unwrap();
        let opened = open(&SourceRequest::reading(&img)).unwrap();
        assert_eq!(opened.mapfile.as_deref(), Some(map.as_path()));
        let un = opened.shared.unreadable().expect("unreadable ranges");
        assert_eq!(un.total_len(), 2048);
        assert_eq!(opened.warnings.len(), 1);
        assert!(opened.warnings[0].contains("could not be copied"));
        // Without map use the image is a plain file.
        let mut req = SourceRequest::reading(&img);
        req.use_map = false;
        let plain = open(&req).unwrap();
        assert!(plain.shared.unreadable().is_none());
    }

    #[test]
    fn explicit_missing_map_is_an_error_but_implicit_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("a.img");
        std::fs::write(&img, vec![0u8; 1024]).unwrap();
        assert!(open(&SourceRequest::reading(&img)).is_ok());
        let missing = dir.path().join("none.map");
        let mut req = SourceRequest::reading(&img);
        req.map = Some(&missing);
        assert!(open(&req).is_err());
    }

    #[test]
    fn broken_implicit_map_is_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("a.img");
        std::fs::write(&img, vec![0u8; 1024]).unwrap();
        std::fs::write(default_map_path(&img), "this is not a mapfile at all\n").unwrap();
        let o = open(&SourceRequest::reading(&img)).unwrap();
        assert!(o.mapfile.is_none());
        assert!(o.warnings.iter().any(|w| w.contains("could not be read")));
        let mut req = SourceRequest::reading(&img);
        let explicit = default_map_path(&img);
        req.map = Some(&explicit);
        assert!(open(&req).is_err());
    }

    #[test]
    fn output_equal_to_image_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("a.img");
        std::fs::write(&img, vec![0u8; 1024]).unwrap();
        let mut req = SourceRequest::reading(&img);
        req.output = Some(&img);
        let err = open(&req).err().expect("must be refused");
        assert_eq!(err.exit_code(), 3);
    }
}
