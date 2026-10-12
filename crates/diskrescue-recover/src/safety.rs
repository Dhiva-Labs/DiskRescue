//! Safety checks that run before any recovery writes a byte.
//!
//! * The output directory must not live on the device being recovered:
//!   every byte written there could overwrite a deleted photo.
//! * A mounted source keeps changing underneath us and the OS may write to
//!   it; the user must unmount it or explicitly accept the risk.
//! * An image file must not be written into its own output.

use std::path::{Path, PathBuf};

use diskrescue_core::device::{DeviceInfo, DeviceKind};
use diskrescue_core::{Error, Result};

/// Findings of [`check`].
#[derive(Debug, Clone, Default)]
pub struct SafetyReport {
    /// Problems that must stop the run (unless overridden where allowed).
    pub blockers: Vec<String>,
    /// Things to tell the user.
    pub warnings: Vec<String>,
}

impl SafetyReport {
    /// Turn blockers into an error.
    pub fn into_result(self) -> Result<Vec<String>> {
        if self.blockers.is_empty() {
            Ok(self.warnings)
        } else {
            Err(Error::Refused(self.blockers.join("; ")))
        }
    }
}

/// Check that it is safe to recover from `source` (described by `info`) into
/// `output`. `allow_mounted` turns the mounted-source blocker into a warning.
pub fn check(source: &Path, info: &DeviceInfo, output: &Path, allow_mounted: bool) -> SafetyReport {
    let mut r = SafetyReport::default();
    let out_abs = absolute_existing_prefix(output);

    if info.kind == DeviceKind::ImageFile {
        if let (Ok(src), Some(out)) = (source.canonicalize(), out_abs.as_ref()) {
            if out == &src {
                r.blockers.push("the output directory is the image file itself".into());
            }
        }
    } else {
        match diskrescue_device::path_is_on_device(output, info) {
            Ok(true) => r.blockers.push(format!(
                "the output directory {} is on {}, the device being recovered. Writing there can overwrite the \
                 very files you want back. Choose a directory on another disk.",
                output.display(),
                info.path.display()
            )),
            Ok(false) => {}
            Err(e) => r.warnings.push(format!(
                "could not verify that the output is on a different device ({e}); make sure it is"
            )),
        }
        if info.is_mounted() {
            let mps: Vec<String> = info.mountpoints.iter().map(|m| m.display().to_string()).collect();
            let msg = format!(
                "{} is mounted at {}. Unmount it first so nothing writes to it while recovering \
                 (or pass --allow-mounted to continue anyway).",
                info.path.display(),
                mps.join(", ")
            );
            if allow_mounted {
                r.warnings.push(msg);
            } else {
                r.blockers.push(msg);
            }
        }
        if info.kind.is_flash() && !info.kind.is_card() && !info.kind.is_usb() {
            r.warnings.push(
                "this is an SSD: if the files were deleted, TRIM has usually erased them already. \
                 Files that still exist, and files on a failing SSD, can be recovered."
                    .into(),
            );
        }
        if info.size == 0 {
            r.blockers.push(format!(
                "{} reports a capacity of 0 bytes. This usually means the device's controller has failed; \
                 software cannot read it. A data recovery lab can often still recover the data.",
                info.path.display()
            ));
        }
        r.warnings.extend(info.warnings.iter().cloned());
    }
    r
}

/// The deepest existing ancestor of `p`, canonicalised (the output directory
/// may not exist yet).
fn absolute_existing_prefix(p: &Path) -> Option<PathBuf> {
    let mut cur = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(p)
    };
    loop {
        if let Ok(c) = cur.canonicalize() {
            return Some(c);
        }
        if !cur.pop() {
            return None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskrescue_core::device::Transport;

    fn image_info(path: &Path) -> DeviceInfo {
        DeviceInfo {
            path: path.to_path_buf(),
            name: "img".into(),
            kind: DeviceKind::ImageFile,
            transport: Transport::Unknown,
            size: 1024,
            logical_sector: 512,
            physical_sector: 512,
            model: None,
            vendor: None,
            serial: None,
            firmware: None,
            stable_id: None,
            removable: false,
            rotational: false,
            read_only: false,
            partitions: vec![],
            mountpoints: vec![],
            health: None,
            warnings: vec![],
        }
    }

    #[test]
    fn image_file_rules() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("card.img");
        std::fs::write(&img, [0u8; 1024]).unwrap();
        let info = image_info(&img);
        assert!(check(&img, &info, &dir.path().join("out"), false).blockers.is_empty());
        assert!(!check(&img, &info, &img, false).blockers.is_empty());
    }

    #[test]
    fn existing_prefix_handles_missing_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let p = absolute_existing_prefix(&dir.path().join("a/b/c")).unwrap();
        assert_eq!(p, dir.path().canonicalize().unwrap());
    }
}
