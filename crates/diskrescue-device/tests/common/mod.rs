//! Fake `/sys`, `/dev`, mountinfo and udev trees for tests.
//!
//! Directories and symlinks mimic the kernel's layout (relative symlinks
//! from `/sys/block`, `/sys/class/block` and `/sys/dev/block` into
//! `/sys/devices/...`, a `device` link from the disk to its parent). No
//! device nodes are created; nothing here needs root.

#![allow(dead_code)]

#[cfg(target_os = "linux")]
use std::fs;
#[cfg(target_os = "linux")]
use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
use diskrescue_device::sysfs::Roots;

#[cfg(target_os = "linux")]
pub struct FakeSys {
    pub dir: tempfile::TempDir,
    pub roots: Roots,
}

/// A whole disk to add.
#[cfg(target_os = "linux")]
pub struct Disk<'a> {
    pub name: &'a str,
    /// Disk directory relative to `sys/` (e.g.
    /// `devices/.../0:0:0:0/block/sda` or `devices/virtual/block/loop0`).
    pub dir: String,
    /// Target of the `device` link relative to `sys/`, if any.
    pub device: Option<String>,
    pub devt: (u32, u32),
    pub size_sectors: u64,
    pub removable: bool,
    pub ro: bool,
    pub rotational: Option<u8>,
    pub lbs: u32,
    pub pbs: u32,
}

#[cfg(target_os = "linux")]
impl<'a> Disk<'a> {
    /// A SCSI-style disk below `parent` (the scsi_device directory relative
    /// to `sys/`).
    pub fn scsi(name: &'a str, parent: &str, devt: (u32, u32), size_sectors: u64) -> Self {
        Disk {
            name,
            dir: format!("{parent}/block/{name}"),
            device: Some(parent.to_string()),
            devt,
            size_sectors,
            removable: false,
            ro: false,
            rotational: Some(1),
            lbs: 512,
            pbs: 512,
        }
    }

    /// A virtual disk under `devices/virtual/block`.
    pub fn virt(name: &'a str, devt: (u32, u32), size_sectors: u64) -> Self {
        Disk {
            name,
            dir: format!("devices/virtual/block/{name}"),
            device: None,
            devt,
            size_sectors,
            removable: false,
            ro: false,
            rotational: Some(0),
            lbs: 512,
            pbs: 512,
        }
    }
}

#[cfg(target_os = "linux")]
impl FakeSys {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let roots = Roots::under(dir.path());
        for d in [
            "sys/block",
            "sys/class/block",
            "sys/dev/block",
            "dev/disk/by-id",
            "run/udev/data",
            "proc/self",
        ] {
            fs::create_dir_all(dir.path().join(d)).unwrap();
        }
        fs::write(&roots.mountinfo, "").unwrap();
        FakeSys { dir, roots }
    }

    pub fn base(&self) -> &Path {
        self.dir.path()
    }

    /// Write `content` to `rel` (relative to the fake root), creating parents.
    pub fn write(&self, rel: &str, content: impl AsRef<[u8]>) {
        let p = self.base().join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }

    pub fn mkdir(&self, rel: &str) {
        fs::create_dir_all(self.base().join(rel)).unwrap();
    }

    /// Create a relative symlink at `link` (relative to the fake root)
    /// pointing at `target` (also relative to the fake root).
    pub fn symlink(&self, link: &str, target: &str) {
        let lp = self.base().join(link);
        fs::create_dir_all(lp.parent().unwrap()).unwrap();
        let depth = Path::new(link)
            .parent()
            .map(|p| p.components().count())
            .unwrap_or(0);
        let rel: PathBuf = std::iter::repeat_n("..", depth)
            .collect::<PathBuf>()
            .join(target);
        let _ = fs::remove_file(&lp);
        std::os::unix::fs::symlink(rel, lp).unwrap();
    }

    /// Create a symlink with a literal target.
    pub fn raw_symlink(&self, link: &str, target: &str) {
        let lp = self.base().join(link);
        fs::create_dir_all(lp.parent().unwrap()).unwrap();
        let _ = fs::remove_file(&lp);
        std::os::unix::fs::symlink(target, lp).unwrap();
    }

    /// Add a whole disk; returns its directory relative to the fake root.
    pub fn add_disk(&self, d: &Disk<'_>) -> String {
        let dir = format!("sys/{}", d.dir);
        self.write(&format!("{dir}/size"), format!("{}\n", d.size_sectors));
        self.write(
            &format!("{dir}/removable"),
            format!("{}\n", d.removable as u8),
        );
        self.write(&format!("{dir}/ro"), format!("{}\n", d.ro as u8));
        self.write(&format!("{dir}/hidden"), "0\n");
        self.write(
            &format!("{dir}/dev"),
            format!("{}:{}\n", d.devt.0, d.devt.1),
        );
        self.write(&format!("{dir}/diskseq"), "7\n");
        if let Some(r) = d.rotational {
            self.write(&format!("{dir}/queue/rotational"), format!("{r}\n"));
        }
        self.write(
            &format!("{dir}/queue/logical_block_size"),
            format!("{}\n", d.lbs),
        );
        self.write(
            &format!("{dir}/queue/physical_block_size"),
            format!("{}\n", d.pbs),
        );
        self.write(&format!("{dir}/queue/max_sectors_kb"), "120\n");
        self.write(&format!("{dir}/queue/zoned"), "none\n");
        self.mkdir(&format!("{dir}/holders"));
        self.mkdir(&format!("{dir}/slaves"));
        if let Some(dev) = &d.device {
            self.symlink(&format!("{dir}/device"), &format!("sys/{dev}"));
        }
        self.symlink(&format!("sys/block/{}", d.name), &dir);
        self.symlink(&format!("sys/class/block/{}", d.name), &dir);
        self.symlink(&format!("sys/dev/block/{}:{}", d.devt.0, d.devt.1), &dir);
        dir
    }

    /// Add a partition to a disk directory returned by [`FakeSys::add_disk`].
    pub fn add_partition(
        &self,
        disk_dir: &str,
        name: &str,
        number: u32,
        start: u64,
        size: u64,
        devt: (u32, u32),
    ) -> String {
        let dir = format!("{disk_dir}/{name}");
        self.write(&format!("{dir}/partition"), format!("{number}\n"));
        self.write(&format!("{dir}/start"), format!("{start}\n"));
        self.write(&format!("{dir}/size"), format!("{size}\n"));
        self.write(&format!("{dir}/dev"), format!("{}:{}\n", devt.0, devt.1));
        self.mkdir(&format!("{dir}/holders"));
        self.symlink(&format!("sys/class/block/{name}"), &dir);
        self.symlink(&format!("sys/dev/block/{}:{}", devt.0, devt.1), &dir);
        dir
    }

    /// Stack `holder` (a disk added with [`FakeSys::add_disk`]) on `member_dir`.
    pub fn add_holder(&self, member_dir: &str, member_name: &str, holder: &str, holder_dir: &str) {
        self.symlink(&format!("{member_dir}/holders/{holder}"), holder_dir);
        self.symlink(&format!("{holder_dir}/slaves/{member_name}"), member_dir);
    }

    pub fn udev(&self, devt: (u32, u32), text: &str) {
        self.write(&format!("run/udev/data/b{}:{}", devt.0, devt.1), text);
    }

    pub fn by_id(&self, link: &str, name: &str) {
        self.raw_symlink(&format!("dev/disk/by-id/{link}"), &format!("../../{name}"));
    }

    pub fn mountinfo(&self, text: &str) {
        fs::write(&self.roots.mountinfo, text).unwrap();
    }

    pub fn swaps(&self, text: &str) {
        fs::write(self.roots.swaps(), text).unwrap();
    }
}

/// A SCSI VPD page 0x80 carrying `serial`.
pub fn vpd80(serial: &str) -> Vec<u8> {
    let mut v = vec![0u8, 0x80, 0, serial.len() as u8];
    v.extend_from_slice(serial.as_bytes());
    v
}

/// A SCSI VPD page 0xB1 with the given rotation rate and product type.
pub fn vpdb1(rate: u16, product_type: u8) -> Vec<u8> {
    let mut v = vec![0u8; 64];
    v[1] = 0xB1;
    v[3] = 0x3C;
    v[4..6].copy_from_slice(&rate.to_be_bytes());
    v[6] = product_type;
    v
}

/// Tiny deterministic PRNG for fuzz-style tests.
pub struct XorShift(pub u64);

impl XorShift {
    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    pub fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
}
