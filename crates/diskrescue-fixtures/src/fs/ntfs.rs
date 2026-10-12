//! NTFS builder.
//!
//! * `mkntfs -F -Q -q -s 512 -c <cluster>` creates the empty volume
//!   (required).
//! * Files that live in the root directory, have no explicit timestamp and
//!   no named streams are copied in with `ntfscp` (no mount needed).
//! * Everything else (subdirectories, deletions, timestamps, named streams
//!   such as `photo.jpg:Zone.Identifier`) needs an `ntfs-3g` FUSE mount:
//!   the image is mounted at a path inside a fresh temporary directory
//!   (`ntfs-3g -o no_detach,...`), populated with ordinary `std::fs` calls
//!   and unmounted with `fusermount3 -u`; a drop guard unmounts and reaps the
//!   helper process even on errors and panics. Deletions are performed in a
//!   second mount session so that the extents of the doomed files can be read
//!   with `ntfsinfo` first.
//! * Without a usable mount (`ntfs-3g`, `fusermount3` or `/dev/fuse` missing)
//!   the builder falls back to `ntfscp` into the root directory, with `/`
//!   in paths replaced by `_`, skips deletions and reports
//!   `deleted_supported = false` and a note.
//!
//! Extents come from `ntfsinfo -v -F` (the runlist of the unnamed `$DATA`
//! stream); files kept resident in the MFT record have none.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use crate::error::{FixtureError, Result};
use crate::fs::plan::{Child as PChild, Plan};
use crate::fs::{create_sparse, BuildReport, FsSpec, PlacedFile};
use crate::tools;

/// True when an `ntfs-3g` FUSE mount can work on this machine.
pub fn can_mount() -> bool {
    cfg!(unix)
        && tools::have("ntfs-3g")
        && tools::have("fusermount3")
        && Path::new("/dev/fuse").exists()
}

/// A mounted NTFS image. Unmounts on drop.
struct Mount {
    child: Option<Child>,
    mnt: PathBuf,
    mounted: bool,
}

impl Mount {
    fn start(image: &Path, mnt: &Path, log: &Path) -> Result<Mount> {
        let exe = tools::require("ntfs-3g")?;
        let logf = std::fs::File::create(log)?;
        let child = Command::new(exe)
            .args([
                "-o",
                "no_detach,noatime,windows_names,streams_interface=windows,umask=0",
            ])
            .arg(image)
            .arg(mnt)
            .stdin(Stdio::null())
            .stdout(Stdio::from(logf.try_clone()?))
            .stderr(Stdio::from(logf))
            .spawn()?;
        let mut m = Mount {
            child: Some(child),
            mnt: mnt.to_path_buf(),
            mounted: false,
        };
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(c) = m.child.as_mut() {
                if let Some(status) = c.try_wait()? {
                    m.child = None;
                    let text = std::fs::read_to_string(log).unwrap_or_default();
                    return Err(FixtureError::Tool {
                        tool: "ntfs-3g".into(),
                        status: status.to_string(),
                        stderr: text.trim().to_string(),
                    });
                }
            }
            if is_mount_point(mnt) {
                m.mounted = true;
                return Ok(m);
            }
            if Instant::now() > deadline {
                return Err(FixtureError::Tool {
                    tool: "ntfs-3g".into(),
                    status: "timeout".into(),
                    stderr: "mount did not appear within 20 s".into(),
                });
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn fusermount_u(&self) -> bool {
        Command::new("fusermount3")
            .arg("-u")
            .arg(&self.mnt)
            .stdin(Stdio::null())
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn reap(&mut self, limit: Duration) -> bool {
        let Some(c) = self.child.as_mut() else {
            return true;
        };
        let deadline = Instant::now() + limit;
        loop {
            match c.try_wait() {
                Ok(Some(_)) => {
                    self.child = None;
                    return true;
                }
                Ok(None) => {}
                Err(_) => break,
            }
            if Instant::now() > deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    /// Unmount and wait for the FUSE daemon to finish writing the volume.
    fn unmount(&mut self) -> Result<()> {
        let mut ok = false;
        for _ in 0..25 {
            if self.fusermount_u() {
                ok = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        self.mounted = false;
        if !ok {
            return Err(FixtureError::Tool {
                tool: "fusermount3".into(),
                status: "failed".into(),
                stderr: format!("could not unmount {}", self.mnt.display()),
            });
        }
        if !self.reap(Duration::from_secs(30)) {
            if let Some(c) = self.child.as_mut() {
                let _ = c.kill();
                let _ = c.wait();
            }
            self.child = None;
            return Err(FixtureError::Tool {
                tool: "ntfs-3g".into(),
                status: "timeout".into(),
                stderr: "daemon did not exit after unmount".into(),
            });
        }
        Ok(())
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        if self.mounted {
            for _ in 0..10 {
                if self.fusermount_u() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            self.mounted = false;
        }
        if !self.reap(Duration::from_secs(10)) {
            if let Some(c) = self.child.as_mut() {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
    }
}

#[cfg(unix)]
fn is_mount_point(p: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(m) = std::fs::metadata(p) else {
        return false;
    };
    let Some(parent) = p.parent() else {
        return false;
    };
    let Ok(pm) = std::fs::metadata(parent) else {
        return false;
    };
    m.dev() != pm.dev()
}

#[cfg(not(unix))]
fn is_mount_point(_p: &Path) -> bool {
    false
}

fn stream_split(path: &str) -> Option<(String, String)> {
    let last = path.rsplit('/').next().unwrap_or(path);
    let i = last.find(':')?;
    let base_len = path.len() - last.len() + i;
    Some((path[..base_len].to_string(), last[i + 1..].to_string()))
}

fn check_ntfs_name(c: &str) -> Result<()> {
    if c.chars()
        .any(|ch| (ch as u32) < 0x20 || "\"*:<>?\\|".contains(ch))
        || c.ends_with(' ')
        || c.ends_with('.')
    {
        return Err(FixtureError::invalid(format!(
            "name not allowed on NTFS: {c:?}"
        )));
    }
    Ok(())
}

fn set_mtime(path: &Path, secs: i64) -> Result<()> {
    let t = if secs >= 0 {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs as u64)
    } else {
        SystemTime::UNIX_EPOCH - Duration::from_secs(secs.unsigned_abs())
    };
    let f = std::fs::OpenOptions::new().write(true).open(path)?;
    f.set_modified(t)?;
    Ok(())
}

pub(crate) fn build(spec: &FsSpec, out: &Path) -> Result<BuildReport> {
    tools::require("mkntfs")?;
    let mut report = BuildReport {
        tool: "mkntfs".into(),
        ..Default::default()
    };

    // Separate named streams from regular files.
    let mut base = spec.clone();
    base.files.clear();
    let mut streams: Vec<(String, String, Vec<u8>)> = Vec::new();
    for f in &spec.files {
        match stream_split(&f.path) {
            Some((file, stream)) => streams.push((file, stream, f.data.clone())),
            None => base.files.push(f.clone()),
        }
    }
    for f in &base.files {
        for c in f.path.split('/').filter(|c| !c.is_empty()) {
            check_ntfs_name(c)?;
        }
    }
    for d in &base.dirs {
        for c in d.split('/').filter(|c| !c.is_empty()) {
            check_ntfs_name(c)?;
        }
    }
    if base.files.iter().any(|f| f.fragment.is_some()) {
        report
            .notes
            .push("fragmented placement is not supported on NTFS and was ignored".into());
    }
    let plan = Plan::new(&base, true)?;

    drop(create_sparse(out, spec.size)?);
    let path = out.to_string_lossy().into_owned();
    let cs = spec.cluster_size.map(|c| c.to_string());
    if let Some(c) = spec.cluster_size {
        if !(512..=65536).contains(&c) || !c.is_power_of_two() {
            return Err(FixtureError::invalid(format!("bad NTFS cluster size {c}")));
        }
    }
    let mut args = vec!["-F", "-Q", "-q", "-s", "512"];
    if let Some(c) = &cs {
        args.push("-c");
        args.push(c);
    }
    if let Some(l) = &spec.label {
        args.push("-L");
        args.push(l);
    }
    args.push(&path);
    tools::run("mkntfs", &args)?;

    let needs_mount = !streams.is_empty()
        || plan.dirs.len() > 1
        || !spec.deleted.is_empty()
        || base.files.iter().any(|f| f.mtime.is_some());

    let boot = {
        use std::io::Read;
        let mut f = std::fs::File::open(out)?;
        let mut b = [0u8; 512];
        f.read_exact(&mut b)?;
        b
    };
    let bps = u16::from_le_bytes([boot[11], boot[12]]) as u64;
    let spc = boot[13] as u64;
    if &boot[3..11] != b"NTFS    " || bps == 0 || spc == 0 {
        return Err(FixtureError::corrupt(
            "NTFS boot sector",
            "mkntfs produced no NTFS",
        ));
    }
    let cluster = bps * spc;
    report.cluster_size = cluster as u32;

    // Try the mount first; any failure before the volume was touched falls
    // back to the flat ntfscp path.
    let mut session = None;
    if needs_mount && !spec.no_mount && can_mount() {
        let tmp = tempfile::tempdir()?;
        let mnt = tmp.path().join("mnt");
        std::fs::create_dir(&mnt)?;
        let log = tmp.path().join("ntfs-3g.log");
        match Mount::start(out, &mnt, &log) {
            Ok(m) => session = Some((tmp, mnt, log, m)),
            Err(e) => report.notes.push(format!(
                "ntfs-3g mount failed ({e}); using the ntfscp fallback"
            )),
        }
    }
    if let Some((_tmp, mnt, log, first)) = session {
        report.tool = "mkntfs+ntfs-3g".into();
        let sess = Session {
            image: out,
            mnt: &mnt,
            log: &log,
        };
        populate_mounted(spec, &base, &plan, &streams, &sess, first, &mut report)?;
        report.deleted_supported = true;
    } else {
        if needs_mount {
            report.notes.push(
                "ntfs-3g mount unavailable: files copied with ntfscp into the root directory \
                 (paths flattened with '_'); subdirectories, timestamps, streams and \
                 deletions were skipped"
                    .into(),
            );
        } else {
            report.tool = "mkntfs+ntfscp".into();
        }
        tools::require("ntfscp")?;
        let dir = tempfile::tempdir()?;
        for (i, f) in base.files.iter().enumerate() {
            let flat = f.path.trim_matches('/').replace('/', "_");
            let src = dir.path().join(format!("src{i}"));
            std::fs::write(&src, &f.data)?;
            let src_s = src.to_string_lossy().into_owned();
            let dst = format!("/{flat}");
            tools::run("ntfscp", &[path.as_str(), src_s.as_str(), dst.as_str()])?;
            report.placed.push(PlacedFile {
                path: flat,
                size: f.data.len() as u64,
                extents: Vec::new(),
                deleted: false,
                mtime: f.mtime.unwrap_or(super::DEFAULT_MTIME),
            });
        }
        report.deleted_supported = false;
    }
    fill_extents(&mut report, out, cluster)?;
    Ok(report)
}

/// Where the image is and where it gets mounted.
struct Session<'a> {
    image: &'a Path,
    mnt: &'a Path,
    log: &'a Path,
}

/// Populate through the first mount session, then (if needed) delete in a
/// second one.
fn populate_mounted(
    spec: &FsSpec,
    base: &FsSpec,
    plan: &Plan,
    streams: &[(String, String, Vec<u8>)],
    sess: &Session,
    mut m: Mount,
    report: &mut BuildReport,
) -> Result<()> {
    let Session { image, mnt, log } = *sess;
    {
        for &item in &plan.order {
            match item {
                PChild::Dir(i) => {
                    std::fs::create_dir_all(mnt.join(&plan.dirs[i].path))?;
                }
                PChild::File(i) => {
                    let n = &plan.files[i];
                    let target = mnt.join(&n.path);
                    if let Some(p) = target.parent() {
                        std::fs::create_dir_all(p)?;
                    }
                    std::fs::write(&target, &base.files[n.spec].data)?;
                    set_mtime(&target, n.mtime)?;
                }
            }
        }
        for (file, stream, data) in streams {
            let file = file.trim_matches('/');
            let target = mnt.join(format!("{file}:{stream}"));
            std::fs::write(&target, data).map_err(|e| {
                FixtureError::Io(std::io::Error::new(
                    e.kind(),
                    format!("writing stream {file}:{stream}: {e}"),
                ))
            })?;
        }
        m.unmount()?;
    }

    for n in &plan.files {
        report.placed.push(PlacedFile {
            path: n.path.clone(),
            size: base.files[n.spec].data.len() as u64,
            extents: Vec::new(),
            deleted: n.deleted,
            mtime: n.mtime,
        });
    }

    if !spec.deleted.is_empty() {
        // Extents of the doomed files must be read before deletion.
        let cluster = report.cluster_size as u64;
        fill_extents(report, image, cluster)?;
        let mut m = Mount::start(image, mnt, log)?;
        for d in &spec.deleted {
            let p = mnt.join(d.trim_matches('/'));
            let md = std::fs::symlink_metadata(&p)?;
            if md.is_dir() {
                std::fs::remove_dir_all(&p)?;
            } else {
                std::fs::remove_file(&p)?;
            }
        }
        m.unmount()?;
    }
    Ok(())
}

/// Fill `extents` of every placed, non-resident file from `ntfsinfo`.
fn fill_extents(report: &mut BuildReport, image: &Path, cluster: u64) -> Result<()> {
    if !tools::have("ntfsinfo") {
        report
            .notes
            .push("ntfsinfo missing: extents not recorded".into());
        return Ok(());
    }
    let img = image.to_string_lossy().into_owned();
    for f in &mut report.placed {
        if !f.extents.is_empty() || f.size == 0 {
            continue;
        }
        let abs = format!("/{}", f.path);
        // A deleted file is no longer in the directory; ignore the failure.
        let Ok(text) = tools::run_stdout("ntfsinfo", &["-v", "-F", &abs, &img]) else {
            continue;
        };
        f.extents = parse_runlist(&text, cluster, f.size);
    }
    Ok(())
}

/// Parse the unnamed `$DATA` runlist printed by `ntfsinfo -v`.
pub(crate) fn parse_runlist(text: &str, cluster: u64, size: u64) -> Vec<(u64, u64)> {
    let mut in_data = false;
    let mut in_runs = false;
    let mut out: Vec<(u64, u64)> = Vec::new();
    let mut left = size;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with("Dumping attribute $DATA") {
            if in_data {
                break; // only the first $DATA (the unnamed stream)
            }
            in_data = true;
            continue;
        }
        if !in_data {
            continue;
        }
        if t.starts_with("Dumping attribute") {
            break;
        }
        if t.starts_with("Runlist:") {
            in_runs = true;
            continue;
        }
        if in_runs {
            let parts: Vec<&str> = t.split_whitespace().collect();
            if parts.len() == 3 && parts[0].starts_with("0x") {
                let lcn = parts[1]
                    .strip_prefix("0x")
                    .and_then(|h| u64::from_str_radix(h, 16).ok());
                let len = parts[2]
                    .strip_prefix("0x")
                    .and_then(|h| u64::from_str_radix(h, 16).ok());
                if let (Some(lcn), Some(len)) = (lcn, len) {
                    let take = (len * cluster).min(left);
                    let off = lcn * cluster;
                    match out.last_mut() {
                        Some((o, l)) if *o + *l == off => *l += take,
                        _ => out.push((off, take)),
                    }
                    left -= take;
                }
            } else if t.starts_with("End of") || t.starts_with("Total runs") {
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_syntax() {
        assert_eq!(
            stream_split("DCIM/a.jpg:Zone.Identifier"),
            Some(("DCIM/a.jpg".into(), "Zone.Identifier".into()))
        );
        assert_eq!(stream_split("a/b.txt"), None);
        assert_eq!(stream_split("a:b/c.txt"), None);
    }

    #[test]
    fn runlist_parser() {
        let t = "Dumping attribute $DATA (0x80) from mft record 66 (0x42)\n\
                 \tResident: \t\t No\n\
                 \tRunlist:\tVCN\t\tLCN\t\tLength\n\
                 \t\t\t0x0\t\t0x1200\t\t0x2\n\
                 \t\t\t0x2\t\t0x1202\t\t0x3\n\
                 \t\t\t0x5\t\t0x2000\t\t0x4\n\
                 End of inode reached\n";
        let r = parse_runlist(t, 4096, 5 * 4096 + 100);
        assert_eq!(r, vec![(0x1200 * 4096, 5 * 4096), (0x2000 * 4096, 100)]);
        assert!(parse_runlist("nothing", 4096, 10).is_empty());
        assert!(
            parse_runlist("Dumping attribute $DATA\nRunlist: x\n0x0 -1 0x4", 4096, 10).is_empty()
        );
    }

    #[test]
    fn mount_of_a_non_ntfs_image_fails_fast_and_cleanly() {
        if !can_mount() {
            eprintln!("skipped: ntfs-3g mount unavailable");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("zero.img");
        std::fs::write(&img, vec![0u8; 1 << 20]).unwrap();
        let mnt = dir.path().join("mnt");
        std::fs::create_dir(&mnt).unwrap();
        let started = Instant::now();
        match Mount::start(&img, &mnt, &dir.path().join("log")) {
            Err(FixtureError::Tool { tool, .. }) => assert_eq!(tool, "ntfs-3g"),
            Err(e) => panic!("unexpected error {e}"),
            Ok(_) => panic!("a zero image cannot be mounted"),
        }
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(!is_mount_point(&mnt));
    }

    #[test]
    fn name_rules() {
        assert!(check_ntfs_name("good name.jpg").is_ok());
        assert!(check_ntfs_name("bad|name").is_err());
        assert!(check_ntfs_name("dot.").is_err());
    }
}
