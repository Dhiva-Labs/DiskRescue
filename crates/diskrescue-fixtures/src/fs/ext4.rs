//! ext4 builder.
//!
//! The files are written into a staging directory (with their mtimes), then
//! `mkfs.ext4 -q -F -d <dir> -b <block> -U <fixed uuid> -E hash_seed=<fixed>`
//! populates a sparse image without root. Deletions use
//! `debugfs -w -f <script>` with `rm` / `rmdir` commands; like the real
//! `debugfs rm` they free the inode and the bitmap bits and set `dtime` but
//! keep the extent tree and the size in the inode, which makes the data
//! recoverable (the "optimistic" deleted inode; a kernel-deleted inode would
//! have its extents zeroed). Extents are read back with `debugfs -R "stat"`
//! before the deletion.
//!
//! Needs: `mkfs.ext4` (required), `debugfs` (needed for deletions and
//! extent reporting; without it extents are left empty and deletions fail).

use std::path::Path;
use std::time::{Duration, SystemTime};

use crate::error::{FixtureError, Result};
use crate::fs::plan::{Child, Plan};
use crate::fs::{create_sparse, BuildReport, FsSpec, PlacedFile};
use crate::tools;

const UUID: &str = "d15c0e5c-0e5c-4a11-8e5c-0d15c0e5c001";
const HASH_SEED: &str = "d15c0e5c-0e5c-4a11-8e5c-0d15c0e5c002";

fn to_system_time(secs: i64) -> SystemTime {
    if secs >= 0 {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs as u64)
    } else {
        SystemTime::UNIX_EPOCH - Duration::from_secs(secs.unsigned_abs())
    }
}

/// Set access and modification time of a file or directory.
fn set_times(path: &Path, secs: i64) -> Result<()> {
    let t = to_system_time(secs);
    let f = std::fs::File::open(path)?;
    f.set_times(std::fs::FileTimes::new().set_accessed(t).set_modified(t))?;
    Ok(())
}

pub(crate) fn build(spec: &FsSpec, out: &Path) -> Result<BuildReport> {
    tools::require("mkfs.ext4")?;
    let plan = Plan::new(spec, false)?;
    let mut report = BuildReport {
        tool: "mkfs.ext4".into(),
        ..Default::default()
    };
    for f in &spec.files {
        for c in f.path.split('/').filter(|c| !c.is_empty()) {
            if c.len() > 255 || c.contains('\0') {
                return Err(FixtureError::invalid(format!("bad ext4 name {c:?}")));
            }
        }
    }
    if spec.files.iter().any(|f| f.fragment.is_some()) {
        report
            .notes
            .push("fragmented placement is not supported on ext4 and was ignored".into());
    }
    let block = spec.cluster_size.unwrap_or(4096);
    if !matches!(block, 1024 | 2048 | 4096) {
        return Err(FixtureError::invalid(format!(
            "ext4 block size must be 1024, 2048 or 4096, got {block}"
        )));
    }

    let stage = tempfile::tempdir()?;
    let root = stage.path().join("root");
    std::fs::create_dir(&root)?;
    for &item in &plan.order {
        match item {
            Child::Dir(i) => std::fs::create_dir_all(root.join(&plan.dirs[i].path))?,
            Child::File(i) => {
                let n = &plan.files[i];
                let target = root.join(&n.path);
                if let Some(p) = target.parent() {
                    std::fs::create_dir_all(p)?;
                }
                std::fs::write(&target, &spec.files[n.spec].data)?;
                set_times(&target, n.mtime)?;
            }
        }
    }
    // Directory times last (writing into a directory changes them), deepest first.
    let mut by_depth: Vec<usize> = (0..plan.dirs.len()).collect();
    by_depth.sort_by_key(|&i| std::cmp::Reverse(plan.depth(i)));
    for i in by_depth {
        set_times(&root.join(&plan.dirs[i].path), plan.dirs[i].mtime)?;
    }

    drop(create_sparse(out, spec.size)?);
    let img = out.to_string_lossy().into_owned();
    let root_s = root.to_string_lossy().into_owned();
    let bs = block.to_string();
    let hash = format!("hash_seed={HASH_SEED}");
    let mut args = vec![
        "-q", "-F", "-b", &bs, "-U", UUID, "-E", &hash, "-d", &root_s,
    ];
    if let Some(l) = &spec.label {
        if l.len() > 16 {
            return Err(FixtureError::invalid("ext4 label is limited to 16 bytes"));
        }
        args.push("-L");
        args.push(l);
    }
    args.push(&img);
    // A fixed "now" makes the superblock and group timestamps reproducible.
    let fake_now = crate::fs::DEFAULT_MTIME.to_string();
    let env = [("E2FSPROGS_FAKE_TIME", fake_now.as_str())];
    tools::run_env("mkfs.ext4", &args, &env)?;
    report.cluster_size = block;
    report.data_start = 0;

    let have_debugfs = tools::have("debugfs");
    for n in &plan.files {
        let size = spec.files[n.spec].data.len() as u64;
        let extents = if have_debugfs && size > 0 {
            stat_extents(&img, &n.path, block as u64, size).unwrap_or_default()
        } else {
            Vec::new()
        };
        report.placed.push(PlacedFile {
            path: n.path.clone(),
            size,
            extents,
            deleted: n.deleted,
            mtime: n.mtime,
        });
    }
    if !have_debugfs {
        report
            .notes
            .push("debugfs missing: extents not recorded".into());
    }

    let any_deleted = plan.files.iter().any(|f| f.deleted) || plan.dirs.iter().any(|d| d.deleted);
    if any_deleted {
        tools::require("debugfs")?;
    }
    if have_debugfs {
        // The staging files' ctime is "now"; pin it to the mtime so that two
        // builds of the same spec are byte-identical.
        let mut script = String::from("sif <2> ctime @");
        script.push_str(&plan.dirs[0].mtime.to_string());
        script.push('\n');
        for d in plan.dirs.iter().skip(1) {
            script.push_str(&format!("sif \"/{}\" ctime @{}\n", d.path, d.mtime));
        }
        for n in &plan.files {
            script.push_str(&format!("sif \"/{}\" ctime @{}\n", n.path, n.mtime));
        }
        for n in plan.files.iter().filter(|n| n.deleted) {
            script.push_str(&format!("rm \"/{}\"\n", n.path));
        }
        let mut dirs: Vec<usize> = (1..plan.dirs.len())
            .filter(|&i| plan.dirs[i].deleted)
            .collect();
        dirs.sort_by_key(|&i| std::cmp::Reverse(plan.depth(i)));
        for i in dirs {
            script.push_str(&format!("rmdir \"/{}\"\n", plan.dirs[i].path));
        }
        let cmd = stage.path().join("debugfs.cmd");
        std::fs::write(&cmd, script)?;
        let cmd_s = cmd.to_string_lossy().into_owned();
        tools::run_env("debugfs", &["-w", "-f", &cmd_s, &img], &env)?;
    }
    report.deleted_supported = have_debugfs;
    Ok(report)
}

/// Run `debugfs -R "stat /path"` and turn the extent list into byte ranges.
fn stat_extents(img: &str, path: &str, block: u64, size: u64) -> Result<Vec<(u64, u64)>> {
    let req = format!("stat \"/{path}\"");
    let text = tools::run_stdout("debugfs", &["-R", &req, img])?;
    Ok(parse_stat_extents(&text, block, size))
}

/// Parse `(0-4):1581-1585, (5):1600` style lists from `debugfs stat`
/// (EXTENTS or BLOCKS sections). Indirect-block markers are skipped.
pub(crate) fn parse_stat_extents(text: &str, block: u64, size: u64) -> Vec<(u64, u64)> {
    let mut out: Vec<(u64, u64)> = Vec::new();
    let mut left = size;
    let mut in_list = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with("EXTENTS:") || t.starts_with("BLOCKS:") {
            in_list = true;
            continue;
        }
        if !in_list {
            continue;
        }
        if t.is_empty() || t.starts_with("TOTAL:") {
            break;
        }
        for item in t.split(',') {
            let item = item.trim();
            let Some(rest) = item.strip_prefix('(') else {
                continue;
            };
            let Some((logical, phys)) = rest.split_once("):") else {
                continue;
            };
            if logical.starts_with("IND")
                || logical.starts_with("DIND")
                || logical.starts_with("TIND")
            {
                continue;
            }
            let (l0, l1) = match logical.split_once('-') {
                Some((a, b)) => (a.parse::<u64>(), b.parse::<u64>()),
                None => (logical.parse::<u64>(), logical.parse::<u64>()),
            };
            let (p0, p1) = match phys.split_once('-') {
                Some((a, b)) => (a.parse::<u64>(), b.parse::<u64>()),
                None => (phys.parse::<u64>(), phys.parse::<u64>()),
            };
            let (Ok(l0), Ok(l1), Ok(p0), Ok(p1)) = (l0, l1, p0, p1) else {
                continue;
            };
            if l1 < l0 || p1 < p0 || left == 0 {
                continue;
            }
            let _ = l0;
            let blocks = p1 - p0 + 1;
            let take = (blocks * block).min(left);
            let off = p0 * block;
            match out.last_mut() {
                Some((o, l)) if *o + *l == off => *l += take,
                _ => out.push((off, take)),
            }
            left -= take;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_extent_lists() {
        let t = "Inode: 12\nEXTENTS:\n(0-1):100-101, (2-3):200-201\n";
        assert_eq!(
            parse_stat_extents(t, 4096, 4 * 4096 - 10),
            vec![(100 * 4096, 2 * 4096), (200 * 4096, 2 * 4096 - 10)]
        );
        let t = "BLOCKS:\n(0-11):1000-1011, (IND):1012, (12-13):1013-1014\nTOTAL: 15\n";
        assert_eq!(
            parse_stat_extents(t, 1024, 14 * 1024),
            vec![(1000 * 1024, 12 * 1024), (1013 * 1024, 2 * 1024)]
        );
        assert!(parse_stat_extents("nothing here", 4096, 10).is_empty());
        assert!(parse_stat_extents("EXTENTS:\n(a-b):x-y\n", 4096, 10).is_empty());
    }
}
