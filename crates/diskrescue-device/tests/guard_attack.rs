//! Attacks on the output guard (`path_is_on_device`): every way an output
//! path could end up on the source device must be caught, and hostile
//! sysfs trees must not make the check hang.

#![cfg(target_os = "linux")]

mod common;

use std::os::unix::fs::{symlink, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use common::{Disk, FakeSys};
use diskrescue_device::sysfs;

const ATA: &str = "devices/pci0000:00/0000:00:17.0";

/// (major, minor) of the filesystem holding `p`.
fn st_dev(p: &Path) -> (u32, u32) {
    let d = std::fs::metadata(p).unwrap().dev() as libc::dev_t;
    (libc::major(d), libc::minor(d))
}

/// A directory on a filesystem other than the one holding `/tmp`
/// (tempfile's default), or `None` when the machine has only one.
fn other_fs_dir() -> Option<(tempfile::TempDir, tempfile::TempDir)> {
    let a = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let b = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    if st_dev(a.path()) == st_dev(b.path()) {
        eprintln!("skipped: /tmp and the target directory are on the same filesystem");
        return None;
    }
    Some((a, b))
}

/// Run `f` on a thread and fail (instead of hanging) if it takes too long.
fn within<T: Send + 'static>(secs: u64, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(Duration::from_secs(secs))
        .expect("the check did not finish in time")
}

/// A fake SATA disk named `name` whose device number is `devt`.
fn disk(fs: &FakeSys, name: &str, host: u32, devt: (u32, u32)) -> String {
    fs.add_disk(&Disk::scsi(
        name,
        &format!("{ATA}/ata{host}/host{host}/target{host}:0:0/{host}:0:0:0"),
        devt,
        1 << 21,
    ))
}

#[test]
fn dangling_symlink_to_the_source_is_caught() {
    // The output (a mapfile, say) is a dangling link on a healthy disk
    // that points into the source's filesystem: creating the file creates
    // the link target, on the source. The nearest existing ancestor of the
    // link itself is on the healthy disk, which used to pass the check.
    let Some((src_fs, healthy)) = other_fs_dir() else {
        return;
    };
    let fs = FakeSys::new();
    disk(&fs, "sdz", 1, st_dev(src_fs.path()));
    let info = sysfs::describe_name_with(&fs.roots, "sdz").unwrap();
    let link = healthy.path().join("rescue.map");
    symlink(src_fs.path().join("not-yet/rescue.map"), &link).unwrap();
    assert!(sysfs::path_is_on_device_with(&fs.roots, &link, &info).unwrap());
    // A dangling link in the middle of a path that does not exist yet.
    let mid = healthy.path().join("out");
    symlink(src_fs.path().join("later"), &mid).unwrap();
    assert!(sysfs::path_is_on_device_with(&fs.roots, &mid.join("photos/a.jpg"), &info).unwrap());
    // A relative dangling link chain ending on the source.
    let hop = healthy.path().join("hop");
    symlink("rescue.map", &hop).unwrap();
    assert!(sysfs::path_is_on_device_with(&fs.roots, &hop, &info).unwrap());
    // A plain path on the healthy disk is fine.
    assert!(!sysfs::path_is_on_device_with(&fs.roots, &healthy.path().join("x/y"), &info).unwrap());
}

#[test]
fn dot_dot_after_missing_components_is_resolved() {
    let out = tempfile::tempdir().unwrap();
    let fs = FakeSys::new();
    disk(&fs, "sdz", 1, st_dev(out.path()));
    disk(&fs, "sdy", 2, (8, 240));
    let sdz = sysfs::describe_name_with(&fs.roots, "sdz").unwrap();
    let sdy = sysfs::describe_name_with(&fs.roots, "sdy").unwrap();
    // `new` does not exist; `new/../out` used to be an error, which the
    // recovery safety check only turns into a warning.
    let p = out.path().join("new/../out/photos");
    assert!(sysfs::path_is_on_device_with(&fs.roots, &p, &sdz).unwrap());
    assert!(!sysfs::path_is_on_device_with(&fs.roots, &p, &sdy).unwrap());
    let p = out.path().join("a/b/../../c/./d");
    assert!(sysfs::path_is_on_device_with(&fs.roots, &p, &sdz).unwrap());
    // `..` beyond the root stays at the root.
    let deep: PathBuf = std::iter::repeat_n("..", 64).collect();
    let p = out.path().join(deep).join("tmp");
    let _ = sysfs::path_is_on_device_with(&fs.roots, &p, &sdy).unwrap();
}

#[test]
fn symlink_loops_are_an_error_not_a_hang() {
    let out = tempfile::tempdir().unwrap();
    let fs = FakeSys::new();
    disk(&fs, "sdz", 1, st_dev(out.path()));
    let info = sysfs::describe_name_with(&fs.roots, "sdz").unwrap();
    symlink(out.path().join("b"), out.path().join("a")).unwrap();
    symlink(out.path().join("a"), out.path().join("b")).unwrap();
    let roots = fs.roots.clone();
    let p = out.path().join("a/x");
    let r = within(10, move || {
        sysfs::path_is_on_device_with(&roots, &p, &info).is_err()
    });
    assert!(r);
    drop(fs);
}

#[test]
fn output_on_a_loop_device_backed_by_a_file_on_the_source_is_caught() {
    // The output directory is inside a filesystem image mounted through
    // loop7; the image file itself lives on the source.
    let Some((src_fs, healthy)) = other_fs_dir() else {
        return;
    };
    let fs = FakeSys::new();
    disk(&fs, "sdz", 1, st_dev(src_fs.path()));
    let backing = src_fs.path().join("container.img");
    std::fs::write(&backing, b"x").unwrap();
    let lp = fs.add_disk(&Disk::virt("loop7", st_dev(healthy.path()), 16));
    fs.write(
        &format!("{lp}/loop/backing_file"),
        format!("{}\n", backing.display()),
    );
    let info = sysfs::describe_name_with(&fs.roots, "sdz").unwrap();
    assert!(sysfs::path_is_on_device_with(&fs.roots, &healthy.path().join("out"), &info).unwrap());
    // The same loop device backed by a file elsewhere: fine.
    let elsewhere = healthy.path().join("container.img");
    std::fs::write(&elsewhere, b"x").unwrap();
    fs.write(
        &format!("{lp}/loop/backing_file"),
        format!("{}\n", elsewhere.display()),
    );
    assert!(!sysfs::path_is_on_device_with(&fs.roots, &healthy.path().join("out"), &info).unwrap());
}

#[test]
fn image_file_sources_guard_their_loop_mounts_and_themselves() {
    let Some((img_fs, mnt)) = other_fs_dir() else {
        return;
    };
    let fs = FakeSys::new();
    let img = img_fs.path().join("card.img");
    std::fs::write(&img, vec![0u8; 4096]).unwrap();
    let info = diskrescue_device::image_file_info(&img, 4096, false);
    // Not loop-attached: writing next to the image is fine, onto it is not.
    assert!(!sysfs::path_is_on_device_with(&fs.roots, &img_fs.path().join("out"), &info).unwrap());
    assert!(sysfs::path_is_on_device_with(&fs.roots, &img, &info).unwrap());
    let link = img_fs.path().join("alias.img");
    symlink(&img, &link).unwrap();
    assert!(sysfs::path_is_on_device_with(&fs.roots, &link, &info).unwrap());
    // Loop-mounted at `mnt`: output inside the mounted image would modify it.
    let lp = fs.add_disk(&Disk::virt("loop3", st_dev(mnt.path()), 8));
    let canon = std::fs::canonicalize(&img).unwrap();
    fs.write(
        &format!("{lp}/loop/backing_file"),
        format!("{}\n", canon.display()),
    );
    assert!(
        sysfs::path_is_on_device_with(&fs.roots, &mnt.path().join("recovered"), &info).unwrap()
    );
    assert!(!sysfs::path_is_on_device_with(&fs.roots, &img_fs.path().join("out"), &info).unwrap());
}

#[test]
fn output_on_another_partition_under_a_stacked_source_is_caught() {
    // The source is dm-0 (dm-crypt on sdb1); the output is on sdb2, the
    // same physical disk.
    let out = tempfile::tempdir().unwrap();
    let fs = FakeSys::new();
    let sdb = disk(&fs, "sdb", 1, (8, 16));
    let sdb1 = fs.add_partition(&sdb, "sdb1", 1, 2048, 1 << 20, (8, 17));
    fs.add_partition(
        &sdb,
        "sdb2",
        2,
        2048 + (1 << 20),
        1 << 20,
        st_dev(out.path()),
    );
    let dm = fs.add_disk(&Disk::virt("dm-0", (253, 0), 1 << 20));
    fs.add_holder(&sdb1, "sdb1", "dm-0", &dm);
    let info = sysfs::describe_name_with(&fs.roots, "dm-0").unwrap();
    assert!(sysfs::path_is_on_device_with(&fs.roots, out.path(), &info).unwrap());
    // An md RAID source over two disks: output on either member disk.
    let fs2 = FakeSys::new();
    let sdc = disk(&fs2, "sdc", 1, (8, 32));
    let sdd = disk(&fs2, "sdd", 2, (8, 48));
    let sdc1 = fs2.add_partition(&sdc, "sdc1", 1, 2048, 1 << 20, (8, 33));
    fs2.add_partition(&sdd, "sdd9", 9, 2048, 1 << 20, st_dev(out.path()));
    let md = fs2.add_disk(&Disk::virt("md0", (9, 0), 1 << 20));
    fs2.add_holder(&sdc1, "sdc1", "md0", &md);
    fs2.symlink(&format!("{md}/slaves/sdd"), &sdd);
    let info = sysfs::describe_name_with(&fs2.roots, "md0").unwrap();
    assert!(sysfs::path_is_on_device_with(&fs2.roots, out.path(), &info).unwrap());
    // A loop source is not expanded to the disk holding its backing file.
    let lp = fs2.add_disk(&Disk::virt("loop1", (7, 1), 16));
    let _ = lp;
    let info = sysfs::describe_name_with(&fs2.roots, "loop1").unwrap();
    assert!(!sysfs::path_is_on_device_with(&fs2.roots, out.path(), &info).unwrap());
}

#[test]
fn output_on_a_partition_of_an_md_array_on_the_source_is_caught() {
    // md0p1 (a partition of an array built on the source disk) holds the
    // output filesystem.
    let out = tempfile::tempdir().unwrap();
    let fs = FakeSys::new();
    let sdb = disk(&fs, "sdb", 1, (8, 16));
    let sdb1 = fs.add_partition(&sdb, "sdb1", 1, 2048, 1 << 20, (8, 17));
    let md = fs.add_disk(&Disk::virt("md5", (9, 5), 1 << 20));
    fs.add_partition(&md, "md5p1", 1, 2048, 1 << 19, st_dev(out.path()));
    // Only the slaves link exists (no holders entry on the member).
    fs.symlink(&format!("{md}/slaves/sdb1"), &sdb1);
    let info = sysfs::describe_name_with(&fs.roots, "sdb").unwrap();
    assert!(sysfs::path_is_on_device_with(&fs.roots, out.path(), &info).unwrap());
}

#[test]
fn multi_device_btrfs_with_a_member_on_the_source_is_caught() {
    // btrfs reports an anonymous st_dev and names only one device as the
    // mount source; the source disk is another member of the filesystem.
    let out = tempfile::tempdir().unwrap();
    let canon = std::fs::canonicalize(out.path()).unwrap();
    let fs = FakeSys::new();
    let sdb = disk(&fs, "sdb", 1, (8, 16));
    fs.add_partition(&sdb, "sdb1", 1, 2048, 1 << 20, (8, 17));
    let sdq = disk(&fs, "sdq", 2, (65, 0));
    fs.add_partition(&sdq, "sdq1", 1, 2048, 1 << 20, (65, 1));
    fs.mountinfo(&format!(
        "1 0 0:99 / / rw - ext4 /dev/nvme9n9 rw\n80 1 0:61 / {} rw - btrfs /dev/sdq1 rw\n",
        canon.display()
    ));
    let uuid = "sys/fs/btrfs/0d3c9a5e-0000-4000-8000-000000000001/devices";
    fs.symlink(&format!("{uuid}/sdq1"), &format!("{sdq}/sdq1"));
    fs.symlink(&format!("{uuid}/sdb1"), &format!("{sdb}/sdb1"));
    let info = sysfs::describe_name_with(&fs.roots, "sdb").unwrap();
    assert!(sysfs::path_is_on_device_with(&fs.roots, &canon.join("out"), &info).unwrap());
    // Single-device btrfs on sdq only: fine.
    std::fs::remove_file(fs.base().join(format!("{uuid}/sdb1"))).unwrap();
    assert!(!sysfs::path_is_on_device_with(&fs.roots, &canon.join("out"), &info).unwrap());
}

#[test]
fn dense_slave_graphs_do_not_explode() {
    // 40 dm devices, each listing every other one as a slave: the old walk
    // was exponential in the depth limit (40^8 visits).
    let out = tempfile::tempdir().unwrap();
    let fs = FakeSys::new();
    disk(&fs, "sdz", 1, (8, 250));
    let n = 40u32;
    let dirs: Vec<String> = (0..n)
        .map(|i| {
            let devt = if i == 0 {
                st_dev(out.path())
            } else {
                (253, 100 + i)
            };
            fs.add_disk(&Disk::virt(&format!("dm-{i}"), devt, 1 << 10))
        })
        .collect();
    for (i, d) in dirs.iter().enumerate() {
        for (j, other) in dirs.iter().enumerate() {
            if i != j {
                fs.symlink(&format!("{d}/slaves/dm-{j}"), other);
            }
        }
    }
    let info = sysfs::describe_name_with(&fs.roots, "sdz").unwrap();
    let roots = fs.roots.clone();
    let p = out.path().to_path_buf();
    let on = within(20, move || {
        sysfs::path_is_on_device_with(&roots, &p, &info).unwrap()
    });
    assert!(!on);
    drop(fs);
}

#[test]
fn bind_mounted_and_symlinked_output_directories() {
    // A bind mount keeps st_dev, so it is caught like the original; a
    // symlinked directory resolves to its target.
    let Some((src_fs, healthy)) = other_fs_dir() else {
        return;
    };
    let fs = FakeSys::new();
    disk(&fs, "sdz", 1, st_dev(src_fs.path()));
    let info = sysfs::describe_name_with(&fs.roots, "sdz").unwrap();
    let real = src_fs.path().join("photos");
    std::fs::create_dir(&real).unwrap();
    let link = healthy.path().join("photos");
    symlink(&real, &link).unwrap();
    assert!(sysfs::path_is_on_device_with(&fs.roots, &link, &info).unwrap());
    assert!(sysfs::path_is_on_device_with(&fs.roots, &link.join("new/sub"), &info).unwrap());
    // `..` is applied after the link: healthy/photos/.. is src_fs.
    assert!(sysfs::path_is_on_device_with(&fs.roots, &link.join("../other"), &info).unwrap());
    // A relative path is resolved from the current directory.
    assert!(
        !sysfs::path_is_on_device_with(&fs.roots, Path::new("relative/out"), &info).unwrap()
            || st_dev(&std::env::current_dir().unwrap()) == st_dev(src_fs.path())
    );
}
