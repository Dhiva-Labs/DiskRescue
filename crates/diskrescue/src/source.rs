//! Opening the SOURCE argument: a device, a partition or an image file.
//!
//! Everything here is read-only. A device is described first (so safety
//! checks can run before it is opened), then opened with the device layer;
//! an image file gets its mapfile attached automatically so that parsers
//! know which areas could not be read from the original device.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use diskrescue_core::device::{DeviceInfo, DeviceKind};
use diskrescue_core::util::human;
use diskrescue_core::SharedSource;
use diskrescue_device::{DeviceSource, OpenOptions};
use diskrescue_imaging::{MapFile, Stats};
use diskrescue_recover::report::SourceRecord;

use crate::error::{explain_io, CliError};
use crate::signal::Interrupt;
use crate::util::percent;

/// How to open a source.
pub struct SourceRequest<'a> {
    /// The path given by the user.
    pub path: &'a Path,
    /// An explicit mapfile (`--map`).
    pub map: Option<&'a Path>,
    /// Attach the mapfile of an image file (reading phases). When false the
    /// image is read as a plain file (it is the source of an imaging run).
    pub use_map: bool,
    /// Treat a mounted source as acceptable (`--allow-mounted`, and always
    /// for read-only listing commands).
    pub allow_mounted: bool,
    /// Use direct I/O on devices (the imaging engine aligns its reads;
    /// parsers do not, so reading commands turn this off).
    pub direct: bool,
    /// Timeout of a single read on a device.
    pub timeout: Option<Duration>,
    /// Where results will be written: checked against the source before it
    /// is opened (refuses output on the source, mounted sources).
    pub output: Option<&'a Path>,
    /// Interrupt state, wired to the device's re-attach wait.
    pub interrupt: Option<&'a Interrupt>,
}

impl<'a> SourceRequest<'a> {
    /// Defaults for a reading command on `path`.
    pub fn reading(path: &'a Path) -> Self {
        SourceRequest {
            path,
            map: None,
            use_map: true,
            allow_mounted: true,
            direct: false,
            timeout: Some(Duration::from_secs(20)),
            output: None,
            interrupt: None,
        }
    }
}

/// An opened source.
pub struct Opened {
    /// Read-only handle (with the mapfile's unreadable ranges for images).
    pub shared: SharedSource,
    /// What is known about it.
    pub info: DeviceInfo,
    /// The device handle when the source is a block device.
    pub device: Option<Arc<DeviceSource>>,
    /// Mapfile in use, for image files.
    pub mapfile: Option<PathBuf>,
    /// Rescue statistics from that mapfile.
    pub map_stats: Option<Stats>,
    /// Things worth telling the user before work starts.
    pub warnings: Vec<String>,
}

impl Opened {
    /// The record for the recovery report.
    pub fn record(&self, origin: Option<&DeviceInfo>) -> SourceRecord {
        let info = &self.info;
        let mut description = if info.kind == DeviceKind::ImageFile {
            format!(
                "{} (image file, {})",
                info.path.display(),
                human::bytes(info.size)
            )
        } else {
            info.summary()
        };
        let mut model = info.model.clone();
        let mut serial = info.serial.clone();
        if let Some(o) = origin {
            description = format!(
                "{} (image of {}: {})",
                info.path.display(),
                o.path.display(),
                diskrescue_device::device_label(o)
            );
            model = model.or_else(|| o.model.clone());
            serial = serial.or_else(|| o.serial.clone());
        }
        SourceRecord {
            description,
            path: Some(info.path.clone()),
            kind: Some(info.kind),
            size: info.size,
            sector_size: info.logical_sector,
            model,
            serial,
            mapfile: self.mapfile.clone(),
            unreadable_bytes: 0,
        }
    }
}

/// `<image>.map` next to the image.
pub fn default_map_path(image: &Path) -> PathBuf {
    let mut s = image.as_os_str().to_os_string();
    s.push(".map");
    PathBuf::from(s)
}

fn looks_like_windows_device(path: &Path) -> bool {
    path.to_string_lossy().starts_with(r"\\.\")
}

fn not_found_message(path: &Path) -> String {
    format!(
        "{} does not exist. Check the spelling, or run 'diskrescue list' to see the disks, cards \
         and USB drives connected to this computer.",
        path.display()
    )
}

/// Describe a source without opening it: the first thing every command does.
pub fn describe(path: &Path) -> Result<DeviceInfo, CliError> {
    match std::fs::metadata(path) {
        Ok(m) if m.is_dir() => {
            return Err(CliError::failed(format!(
            "{} is a folder. Give a device (for example /dev/sdb), a partition or an image file.",
            path.display()
        )))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !looks_like_windows_device(path) => {
            return Err(CliError::failed(not_found_message(path)))
        }
        _ => {}
    }
    diskrescue_device::describe(path).map_err(|e| match e {
        diskrescue_core::Error::NotFound(_) => CliError::failed(not_found_message(path)),
        diskrescue_core::Error::Io(io) => CliError::failed(explain_io(&io, Some(path))),
        other => CliError::from(other),
    })
}

/// Open the source described by `req`. See the module documentation.
pub fn open(req: &SourceRequest<'_>) -> Result<Opened, CliError> {
    let path = req.path;
    let mut info = describe(path)?;
    let mut warnings: Vec<String> = Vec::new();

    if let Some(out) = req.output {
        let report = diskrescue_recover::safety::check(path, &info, out, req.allow_mounted);
        warnings.extend(report.into_result().map_err(|e| match e {
            diskrescue_core::Error::Refused(m) => CliError::refused(m),
            other => CliError::from(other),
        })?);
    } else if info.kind != DeviceKind::ImageFile && diskrescue_device::is_mounted(&info) {
        let mps: Vec<String> = info
            .mountpoints
            .iter()
            .map(|m| m.display().to_string())
            .collect();
        warnings.push(format!(
            "{} is mounted{}. Results may be incomplete while it is in use; unmount it for the best results.",
            path.display(),
            if mps.is_empty() {
                String::new()
            } else {
                format!(" at {}", mps.join(", "))
            }
        ));
    }

    let is_file = info.kind == DeviceKind::ImageFile;
    if !is_file && req.map.is_some() {
        warnings.push("--map only applies to image files; it is ignored for a device".to_string());
    }
    if is_file {
        let (shared, mapfile, map_stats, mut notes) = open_file(path, req)?;
        warnings.append(&mut notes);
        return Ok(Opened {
            shared,
            info,
            device: None,
            mapfile,
            map_stats,
            warnings,
        });
    }

    let opts = OpenOptions {
        direct: req.direct,
        default_timeout: req.timeout.unwrap_or(Duration::from_secs(30)),
        allow_mounted: true,
        fallback_buffered: true,
    };
    let dev = diskrescue_device::open_device(path, &opts).map_err(|e| match e {
        diskrescue_core::Error::Io(io) => CliError::failed(explain_io(&io, Some(path))),
        other => CliError::from(other),
    })?;
    let dev = Arc::new(dev);
    if let Some(i) = req.interrupt {
        dev.set_cancel_flag(i.flag());
    }
    info = dev.info();
    warnings.extend(
        info.warnings
            .iter()
            .filter(|w| !warnings.contains(w))
            .cloned()
            .collect::<Vec<_>>(),
    );
    let shared: SharedSource = dev.clone();
    Ok(Opened {
        shared,
        info,
        device: Some(dev),
        mapfile: None,
        map_stats: None,
        warnings,
    })
}

type FileOpen = (SharedSource, Option<PathBuf>, Option<Stats>, Vec<String>);

fn open_file(path: &Path, req: &SourceRequest<'_>) -> Result<FileOpen, CliError> {
    let plain = || -> Result<SharedSource, CliError> {
        let opts = OpenOptions {
            allow_mounted: true,
            ..OpenOptions::default()
        };
        let (shared, _) = diskrescue_device::open_any(path, &opts).map_err(|e| match e {
            diskrescue_core::Error::Io(io) => CliError::failed(explain_io(&io, Some(path))),
            other => CliError::from(other),
        })?;
        Ok(shared)
    };
    if !req.use_map {
        return Ok((plain()?, None, None, Vec::new()));
    }
    let explicit = req.map.is_some();
    let map_path = req
        .map
        .map(Path::to_path_buf)
        .unwrap_or_else(|| default_map_path(path));
    if !map_path.exists() {
        if explicit {
            return Err(CliError::failed(format!(
                "the mapfile {} does not exist",
                map_path.display()
            )));
        }
        return Ok((plain()?, None, None, Vec::new()));
    }
    let loaded = MapFile::load(&map_path).and_then(|mf| {
        let stats = mf.map.stats();
        diskrescue_imaging::open_rescued_image(path, &map_path).map(|s| (s, stats))
    });
    match loaded {
        Ok((src, stats)) => {
            let mut notes = Vec::new();
            if stats.pending() > 0 || stats.bad_sectors > 0 {
                let missing = stats.total().saturating_sub(stats.rescued);
                notes.push(format!(
                    "The mapfile {} shows that {} of the original {} could not be copied ({} rescued). \
                     Files stored in those areas will be reported as damaged.{}",
                    map_path.display(),
                    human::bytes(missing),
                    human::bytes(stats.total()),
                    percent(stats.rescued, stats.total()),
                    if stats.pending() > 0 {
                        " Imaging did not finish: run 'diskrescue image' again with the same device and image to copy the rest."
                    } else {
                        ""
                    }
                ));
            }
            let shared: SharedSource = Arc::new(src);
            Ok((shared, Some(map_path), Some(stats), notes))
        }
        Err(e) if explicit => Err(CliError::failed(format!(
            "cannot use the mapfile {}: {e}",
            map_path.display()
        ))),
        Err(e) => {
            let note = format!(
                "The mapfile {} could not be read ({e}), so unreadable areas of the image are not known. \
                 Continuing without it.",
                map_path.display()
            );
            Ok((plain()?, None, None, vec![note]))
        }
    }
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
