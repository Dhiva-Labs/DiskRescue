//! Linux enumeration and classification from sysfs, udev and mountinfo.
//!
//! Everything is parameterised by [`Roots`] so tests can build fake
//! `/sys`, `/dev`, `/proc/self/mountinfo` and `/run/udev/data` trees in a
//! temporary directory; [`Roots::system`] points at the real ones. Nothing
//! here opens a device node, except [`open_device_with`] (and first-sector
//! verification in [`find_by_fingerprint_with`]), which open read-only.
//!
//! # Algorithm (device-layer spec §3.1-§3.4)
//!
//! For every entry of `/sys/block` (whole disks only; partitions are
//! sub-directories):
//!
//! 1. skip `hidden=1` (NVMe multipath per-path disks) and entries whose
//!    `size` cannot be read (removed while enumerating);
//! 2. read `size` (512-byte units regardless of the sector size),
//!    `removable`, `ro`, `dev`, `diskseq` and the `queue/` attributes;
//! 3. derive the transport from the *resolved* sysfs path, matching path
//!    components in this order: `nvme-subsystem` (NVMe multipath head),
//!    `devices/virtual/block` (virtual), `usbN`, `ataN`, `nvme`,
//!    `mmc_host`, `virtioN`, `ccw` (unknown), `hostN` (SCSI); kernel names
//!    `loop*`, `dm-*`, `md*`, `zram*`, `ram*`, `nbd*` are always virtual;
//! 4. read identity per transport: NVMe `device/{model,serial,firmware_rev}`
//!    (state/transport from the controller, also through a multipath
//!    subsystem), SCSI/ATA/USB `device/{vendor,model,rev,type,wwid}` plus
//!    the serial from VPD page 0x80, udev `ID_SERIAL_SHORT` or the USB
//!    descriptor; for USB walk up to the `usb_device` directory (the one
//!    with `idVendor`) for `idVendor`, `idProduct`, `serial`,
//!    `manufacturer`, `product` and the interface driver; MMC
//!    `device/{name,serial,cid,csd,manfid,oemid,type,fwrev}`;
//! 5. classify ([`crate::classify`]);
//! 6. list partitions (`<disk>/<part>/partition` present), their
//!    mountpoints and filesystem type (udev `ID_FS_TYPE`);
//! 7. mountpoints: a mountinfo line belongs to the device when its
//!    `MAJ:MIN` equals the `dev` of the disk, a partition or anything
//!    stacked on them (`holders/`, recursively: LVM, dm-crypt, md), or when
//!    its source resolves (through `/dev` symlinks) to one of their names
//!    (btrfs, fuse and others report anonymous `0:N` numbers); swap areas
//!    from `/proc/swaps` (read from `<mountinfo>/../../swaps`) count as
//!    mounted (`[SWAP]`);
//! 8. stable id: the best `/dev/disk/by-id` link resolving to the node
//!    (serial-bearing names first), else udev's `S:disk/by-id/` lines;
//! 9. warnings: no capacity, read-only, offline state, USB misreport note,
//!    SSD-looking model behind `rotational=1`, generic serial, implausible
//!    capacity, clone-card hints, eMMC wear, zoned media, mounted.
//!
//! Every sysfs read is bounded (4 KiB per attribute, a few thousand
//! directory entries, depth-limited symlink and holder walks). A missing
//! attribute is a normal condition and never an error.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};

use diskrescue_core::device::{DeviceInfo, DeviceKind, PartitionInfo, Transport};
use diskrescue_core::{Error, Result};

use crate::classify::{
    classify, is_emmc_hw_partition, is_virtual_name, looks_like_ssd_model, Classification,
    ClassifyInput, Confidence,
};
use crate::device_source::{
    resolve_selection, select_candidate, Ctx, DeviceSource, Lost, OpenOptions,
};
use crate::fingerprint::{is_generic_serial, Fingerprint};
use crate::parse::{
    csd_structure, decode_udev_enc, normalize_lexically, parse_hex_u32, parse_majmin,
    parse_mountinfo, parse_swaps, parse_udev_db, vpd80_serial, vpd_b1, MountEntry, UdevRecord,
    VpdB1,
};
use crate::platform::sane_sector;

/// Largest attribute read from sysfs.
const ATTR_LIMIT: u64 = 4096;
/// Largest text file read whole (mountinfo, udev records, swaps).
const TEXT_LIMIT: u64 = 4 << 20;
/// Directory entries looked at per directory.
const MAX_ENTRIES: usize = 4096;
/// Depth of holder/slave walks.
const MAX_DEPTH: usize = 8;
/// Members collected per device set.
const MAX_MEMBERS: usize = 512;

const TIB: u64 = 1 << 40;

/// Where the kernel interfaces live. Tests point these into a temporary
/// directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Roots {
    /// The sysfs mount (`/sys`).
    pub sysfs: PathBuf,
    /// The device directory (`/dev`), used for node paths and by-id links.
    pub dev: PathBuf,
    /// The mount table (`/proc/self/mountinfo`). `/proc/swaps` is read from
    /// `<mountinfo>/../../swaps`.
    pub mountinfo: PathBuf,
    /// The udev database (`/run/udev/data`).
    pub udev_data: PathBuf,
}

impl Roots {
    /// The real system locations.
    pub fn system() -> Self {
        Roots {
            sysfs: PathBuf::from("/sys"),
            dev: PathBuf::from("/dev"),
            mountinfo: PathBuf::from("/proc/self/mountinfo"),
            udev_data: PathBuf::from("/run/udev/data"),
        }
    }

    /// The standard layout below `base` (`base/sys`, `base/dev`,
    /// `base/proc/self/mountinfo`, `base/run/udev/data`).
    pub fn under(base: &Path) -> Self {
        Roots {
            sysfs: base.join("sys"),
            dev: base.join("dev"),
            mountinfo: base.join("proc/self/mountinfo"),
            udev_data: base.join("run/udev/data"),
        }
    }

    /// Location of the swap table derived from [`Roots::mountinfo`].
    pub fn swaps(&self) -> PathBuf {
        self.mountinfo
            .parent()
            .and_then(Path::parent)
            .map(|p| p.join("swaps"))
            .unwrap_or_else(|| PathBuf::from("/proc/swaps"))
    }
}

impl Default for Roots {
    fn default() -> Self {
        Roots::system()
    }
}

/// USB identity of a device behind a USB bridge or card reader.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UsbIdentity {
    /// `idVendor` (lowercase hex).
    pub vendor_id: Option<String>,
    /// `idProduct` (lowercase hex).
    pub product_id: Option<String>,
    /// iSerial string descriptor (absent on many cheap sticks and readers).
    pub serial: Option<String>,
    /// iManufacturer string.
    pub manufacturer: Option<String>,
    /// iProduct string.
    pub product: Option<String>,
    /// Interface driver (`usb-storage` or `uas`).
    pub driver: Option<String>,
    /// `bInterfaceProtocol` (`50` bulk-only, `62` UAS).
    pub interface_protocol: Option<String>,
    /// Link speed in Mbit/s as printed by the kernel (`480`, `5000`).
    pub speed: Option<String>,
    /// Physical port path (`1.3`), stable across re-plugs into the same port.
    pub devpath: Option<String>,
    /// Bus number.
    pub busnum: Option<String>,
}

/// Everything enumeration learned about one disk, including facts that
/// [`DeviceInfo`] has no field for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SysDevice {
    /// The portable description.
    pub info: DeviceInfo,
    /// `MAJ:MIN` of the disk.
    pub devt: Option<(u32, u32)>,
    /// Resolved sysfs directory.
    pub sysfs_path: PathBuf,
    /// Per-attach sequence number (changes on every re-plug).
    pub diskseq: Option<u64>,
    /// Classification with confidence and reasons.
    pub classification: Classification,
    /// Largest request the kernel issues (`queue/max_sectors_kb`).
    pub max_sectors_kb: Option<u32>,
    /// SCSI command timeout in seconds (`device/timeout`).
    pub scsi_timeout_secs: Option<u32>,
    /// `device/state` (`running`, `offline`, NVMe `live`, ...).
    pub state: Option<String>,
    /// `queue/zoned` (`none`, `host-aware`, `host-managed`).
    pub zoned: Option<String>,
    /// World-wide id (`naa.`, `t10.`, `eui.`).
    pub wwid: Option<String>,
    /// USB identity, for devices behind USB.
    pub usb: Option<UsbIdentity>,
    /// Devices stacked on the disk or its partitions (`dm-0`, `md0`).
    pub holders: Vec<String>,
}

// ---------------------------------------------------------------------------
// Bounded attribute readers.

fn read_raw(p: &Path, limit: u64) -> Option<Vec<u8>> {
    let f = fs::File::open(p).ok()?;
    let mut v = Vec::new();
    f.take(limit).read_to_end(&mut v).ok()?;
    Some(v)
}

fn read_str(p: &Path) -> Option<String> {
    let b = read_raw(p, ATTR_LIMIT)?;
    let s = String::from_utf8_lossy(&b);
    Some(
        s.trim_matches(|c: char| c.is_whitespace() || c == '\0')
            .to_string(),
    )
}

fn read_text(p: &Path) -> Option<String> {
    read_str(p).filter(|s| !s.is_empty())
}

fn read_u64(p: &Path) -> Option<u64> {
    read_str(p)?.parse().ok()
}

fn read_u32(p: &Path) -> Option<u32> {
    read_str(p)?.parse().ok()
}

fn read_flag(p: &Path) -> Option<bool> {
    match read_str(p)?.as_str() {
        "0" => Some(false),
        "1" => Some(true),
        _ => None,
    }
}

fn read_file_text(p: &Path) -> Option<String> {
    read_raw(p, TEXT_LIMIT).map(|b| String::from_utf8_lossy(&b).into_owned())
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/') && !name.contains('\0')
}

fn list_dir(p: &Path) -> Vec<fs::DirEntry> {
    match fs::read_dir(p) {
        Ok(rd) => rd.take(MAX_ENTRIES).filter_map(|e| e.ok()).collect(),
        Err(_) => Vec::new(),
    }
}

fn entry_name(e: &fs::DirEntry) -> Option<String> {
    let n = e.file_name().to_string_lossy().into_owned();
    valid_name(&n).then_some(n)
}

/// Transport from the resolved sysfs path of a disk (relative to the sysfs
/// root, e.g. `/devices/pci0000:00/.../usb1/1-3/.../block/sdb`) and its
/// kernel name. See the module docs for the matching order.
pub fn transport_from_sysfs_path(rel: &str, name: &str) -> Transport {
    if is_virtual_name(name) {
        return Transport::Virtual;
    }
    let comps: Vec<&str> = rel.split('/').filter(|c| !c.is_empty()).collect();
    let numbered = |prefix: &str| {
        comps.iter().any(|c| {
            c.strip_prefix(prefix)
                .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
        })
    };
    if comps.contains(&"nvme-subsystem") {
        return Transport::Nvme;
    }
    if comps
        .windows(3)
        .any(|w| w == ["devices", "virtual", "block"])
    {
        return Transport::Virtual;
    }
    if numbered("usb") {
        return Transport::Usb;
    }
    if numbered("ata") {
        return Transport::Ata;
    }
    if comps.contains(&"nvme") {
        return Transport::Nvme;
    }
    if comps.contains(&"mmc_host") {
        return Transport::Mmc;
    }
    if numbered("virtio") {
        return Transport::Virtio;
    }
    if comps.contains(&"ccw") {
        return Transport::Unknown;
    }
    if numbered("host") {
        return Transport::Scsi;
    }
    Transport::Unknown
}

/// Vendor name for an SD/MMC CID manufacturer id (informational).
pub fn mmc_vendor(manfid: u32) -> Option<&'static str> {
    Some(match manfid {
        0x02 => "Toshiba",
        0x03 => "SanDisk",
        0x1b => "Samsung",
        0x27 => "Phison",
        0x28 => "Lexar",
        0x41 => "Kingston",
        0x74 => "Transcend",
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Scan context: things read once per scan.

#[derive(Debug, Clone)]
struct Member {
    name: String,
    devt: Option<(u32, u32)>,
}

struct ScanCtx<'a> {
    roots: &'a Roots,
    sys_canon: PathBuf,
    dev_norm: PathBuf,
    mounts: Vec<MountEntry>,
    mount_names: Vec<Option<String>>,
    swap_names: Vec<Option<String>>,
    by_id: BTreeMap<String, Vec<String>>,
}

impl<'a> ScanCtx<'a> {
    fn new(roots: &'a Roots) -> Self {
        let sys_canon = fs::canonicalize(&roots.sysfs).unwrap_or_else(|_| roots.sysfs.clone());
        let dev_norm = normalize_lexically(&roots.dev);
        let mut ctx = ScanCtx {
            roots,
            sys_canon,
            dev_norm,
            mounts: Vec::new(),
            mount_names: Vec::new(),
            swap_names: Vec::new(),
            by_id: BTreeMap::new(),
        };
        ctx.mounts = read_file_text(&roots.mountinfo)
            .map(|t| parse_mountinfo(&t))
            .unwrap_or_default();
        ctx.mount_names = ctx
            .mounts
            .iter()
            .map(|m| ctx.source_name(&m.source))
            .collect();
        ctx.swap_names = read_file_text(&roots.swaps())
            .map(|t| parse_swaps(&t))
            .unwrap_or_default()
            .iter()
            .map(|s| ctx.source_name(s))
            .collect();
        ctx.by_id = ctx.build_by_id();
        ctx
    }

    /// Resolve a `/dev/...` path (following symlinks lexically inside
    /// [`Roots::dev`]) to a kernel device name.
    fn source_name(&self, source: &str) -> Option<String> {
        let rel = source.strip_prefix("/dev/")?;
        if rel.is_empty() {
            return None;
        }
        let mut p = normalize_lexically(&self.roots.dev.join(rel));
        for _ in 0..8 {
            let Ok(t) = fs::read_link(&p) else { break };
            let abs = if t.is_absolute() {
                match t.strip_prefix("/dev") {
                    Ok(r) => self.roots.dev.join(r),
                    Err(_) => t,
                }
            } else {
                p.parent().map(|d| d.join(&t)).unwrap_or(t)
            };
            p = normalize_lexically(&abs);
        }
        if !p.starts_with(&self.dev_norm) {
            return None;
        }
        p.file_name().map(|n| n.to_string_lossy().into_owned())
    }

    fn build_by_id(&self) -> BTreeMap<String, Vec<String>> {
        let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for e in list_dir(&self.roots.dev.join("disk/by-id")) {
            let Some(link_name) = entry_name(&e) else {
                continue;
            };
            let link = e.path();
            let Ok(target) = fs::read_link(&link) else {
                continue;
            };
            let abs = if target.is_absolute() {
                match target.strip_prefix("/dev") {
                    Ok(r) => self.roots.dev.join(r),
                    Err(_) => target,
                }
            } else {
                link.parent().map(|d| d.join(&target)).unwrap_or(target)
            };
            let norm = normalize_lexically(&abs);
            if norm.parent() != Some(self.dev_norm.as_path()) {
                continue;
            }
            if let Some(n) = norm.file_name() {
                map.entry(n.to_string_lossy().into_owned())
                    .or_default()
                    .push(link_name);
            }
        }
        map
    }

    fn rel(&self, real: &Path) -> String {
        match real.strip_prefix(&self.sys_canon) {
            Ok(r) => format!("/{}", r.display()),
            Err(_) => real.display().to_string(),
        }
    }

    fn udev(&self, devt: (u32, u32)) -> Option<UdevRecord> {
        let p = self.roots.udev_data.join(format!("b{}:{}", devt.0, devt.1));
        read_file_text(&p).map(|t| parse_udev_db(&t))
    }

    fn stable_id(&self, name: &str, udev: Option<&UdevRecord>) -> Option<String> {
        let mut links = self.by_id.get(name).cloned().unwrap_or_default();
        if links.is_empty() {
            if let Some(u) = udev {
                links = u.by_id_links();
            }
        }
        links.sort_by_key(|l| (link_priority(l), l.len(), l.clone()));
        links.into_iter().next()
    }

    /// Directory of a block device by kernel name (disk or partition).
    fn block_dir(&self, name: &str) -> Option<PathBuf> {
        if !valid_name(name) {
            return None;
        }
        for base in ["class/block", "block"] {
            let p = self.roots.sysfs.join(base).join(name);
            if p.exists() {
                return Some(p);
            }
        }
        // Minimal trees: a partition directory inside a disk directory.
        for e in list_dir(&self.roots.sysfs.join("block")) {
            let p = e.path().join(name);
            if p.join("partition").is_file() {
                return Some(p);
            }
        }
        None
    }

    fn partition_dirs(&self, dir: &Path) -> Vec<(String, PathBuf)> {
        let mut out = Vec::new();
        for e in list_dir(dir) {
            let Ok(ft) = e.file_type() else { continue };
            if !ft.is_dir() {
                continue;
            }
            let Some(n) = entry_name(&e) else { continue };
            let p = e.path();
            if p.join("partition").is_file() {
                out.push((n, p));
            }
        }
        out
    }

    /// Everything stacked on `dir` (holders, recursively, and their
    /// partitions).
    fn holders(
        &self,
        dir: &Path,
        out: &mut Vec<Member>,
        seen: &mut BTreeSet<String>,
        depth: usize,
    ) {
        if depth > MAX_DEPTH || out.len() >= MAX_MEMBERS {
            return;
        }
        for e in list_dir(&dir.join("holders")) {
            let Some(name) = entry_name(&e) else { continue };
            if !seen.insert(name.clone()) {
                continue;
            }
            let hdir = self.block_dir(&name);
            let devt = hdir
                .as_ref()
                .and_then(|d| read_str(&d.join("dev")))
                .and_then(|s| parse_majmin(&s));
            out.push(Member {
                name: name.clone(),
                devt,
            });
            if let Some(h) = hdir {
                self.holders(&h, out, seen, depth + 1);
                for (pn, pdir) in self.partition_dirs(&h) {
                    if seen.insert(pn.clone()) {
                        let devt = read_str(&pdir.join("dev")).and_then(|s| parse_majmin(&s));
                        out.push(Member { name: pn, devt });
                        self.holders(&pdir, out, seen, depth + 1);
                    }
                }
            }
            if out.len() >= MAX_MEMBERS {
                return;
            }
        }
    }

    /// The disk, its partitions and everything stacked on them.
    fn device_set(&self, disk_dir: &Path, disk_name: &str) -> Vec<Member> {
        let mut seen = BTreeSet::new();
        seen.insert(disk_name.to_string());
        let mut out = vec![Member {
            name: disk_name.to_string(),
            devt: read_str(&disk_dir.join("dev")).and_then(|s| parse_majmin(&s)),
        }];
        self.holders(disk_dir, &mut out, &mut seen, 0);
        for (pn, pdir) in self.partition_dirs(disk_dir) {
            if seen.insert(pn.clone()) {
                out.push(Member {
                    name: pn,
                    devt: read_str(&pdir.join("dev")).and_then(|s| parse_majmin(&s)),
                });
                self.holders(&pdir, &mut out, &mut seen, 0);
            }
        }
        out
    }

    fn mounts_for(&self, members: &[Member]) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = Vec::new();
        for (m, sname) in self.mounts.iter().zip(&self.mount_names) {
            let hit = members
                .iter()
                .any(|mem| mem.devt == Some(m.devt) || sname.as_deref() == Some(mem.name.as_str()));
            if hit && !out.contains(&m.mount_point) {
                out.push(m.mount_point.clone());
            }
        }
        let swap = self
            .swap_names
            .iter()
            .flatten()
            .any(|s| members.iter().any(|m| &m.name == s));
        if swap {
            out.push(PathBuf::from("[SWAP]"));
        }
        out
    }
}

fn link_priority(l: &str) -> u8 {
    const WEAK: &[&str] = &["nvme-eui.", "nvme-nguid.", "nvme-uuid.", "wwn-"];
    const STRONG: &[&str] = &[
        "ata-",
        "nvme-",
        "usb-",
        "mmc-",
        "scsi-",
        "virtio-",
        "ieee1394-",
        "memstick-",
    ];
    if WEAK.iter().any(|p| l.starts_with(p)) {
        1
    } else if STRONG.iter().any(|p| l.starts_with(p)) {
        0
    } else {
        2
    }
}

// ---------------------------------------------------------------------------
// Identity per transport.

#[derive(Debug, Default)]
struct Identity {
    vendor: Option<String>,
    model: Option<String>,
    serial: Option<String>,
    firmware: Option<String>,
    wwid: Option<String>,
    scsi_type: Option<u8>,
    vpd_b1: Option<VpdB1>,
    state: Option<String>,
    scsi_timeout: Option<u32>,
    mmc_type: Option<String>,
    mmc_manfid: Option<u32>,
    mmc_oemid: Option<u32>,
    mmc_pre_eol: Option<u32>,
    csd_structure: Option<u8>,
    notes: Vec<String>,
}

fn nvme_identity(base: &Path, dev: Option<&Path>, id: &mut Identity) {
    id.wwid = read_text(&base.join("wwid"));
    let Some(dev) = dev else { return };
    id.model = read_text(&dev.join("model"));
    id.serial = read_text(&dev.join("serial"));
    id.firmware = read_text(&dev.join("firmware_rev"));
    id.state = read_text(&dev.join("state"));
    if id.state.is_none() || read_text(&dev.join("transport")).is_none() {
        // Multipath head: the subsystem device has no state/transport; the
        // controllers are children named nvme<N>.
        for e in list_dir(dev) {
            let Some(n) = entry_name(&e) else { continue };
            let is_ctrl = n
                .strip_prefix("nvme")
                .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()));
            if is_ctrl {
                if id.state.is_none() {
                    id.state = read_text(&e.path().join("state"));
                }
                break;
            }
        }
    }
}

fn mmc_identity(dev: Option<&Path>, id: &mut Identity) {
    let Some(dev) = dev else { return };
    id.model = read_text(&dev.join("name"));
    id.serial = read_text(&dev.join("serial"));
    id.firmware = read_text(&dev.join("fwrev"));
    id.mmc_type = read_text(&dev.join("type"));
    id.mmc_manfid = read_text(&dev.join("manfid")).and_then(|s| parse_hex_u32(&s));
    id.mmc_oemid = read_text(&dev.join("oemid")).and_then(|s| parse_hex_u32(&s));
    id.mmc_pre_eol = read_text(&dev.join("pre_eol_info")).and_then(|s| parse_hex_u32(&s));
    id.csd_structure = read_text(&dev.join("csd")).and_then(|s| csd_structure(&s));
    id.vendor = id.mmc_manfid.map(|m| {
        mmc_vendor(m)
            .map(String::from)
            .unwrap_or_else(|| format!("manufacturer 0x{m:02x}"))
    });
}

fn scsi_identity(dev: Option<&Path>, id: &mut Identity) {
    let Some(dev) = dev else { return };
    id.vendor = read_text(&dev.join("vendor"));
    id.model = read_text(&dev.join("model"));
    id.firmware = read_text(&dev.join("rev"));
    id.scsi_type = read_u32(&dev.join("type"))
        .filter(|t| *t < 32)
        .map(|t| t as u8);
    id.serial = read_raw(&dev.join("vpd_pg80"), ATTR_LIMIT).and_then(|b| vpd80_serial(&b));
    id.wwid = read_text(&dev.join("wwid"));
    id.state = read_text(&dev.join("state"));
    id.scsi_timeout = read_u32(&dev.join("timeout"));
    id.vpd_b1 = read_raw(&dev.join("vpd_pgb1"), ATTR_LIMIT).and_then(|b| vpd_b1(&b));
}

fn virtual_identity(base: &Path, name: &str, id: &mut Identity) {
    let slaves: Vec<String> = list_dir(&base.join("slaves"))
        .iter()
        .filter_map(entry_name)
        .collect();
    let built_on = if slaves.is_empty() {
        String::new()
    } else {
        format!(" built on {}", slaves.join(", "))
    };
    if name.starts_with("loop") {
        match read_text(&base.join("loop/backing_file")) {
            Some(f) => id.notes.push(format!(
                "loop device backed by {f}: image or scan that file directly instead"
            )),
            None => id.notes.push("unbound loop device".into()),
        }
    } else if name.starts_with("dm-") {
        let dm = read_text(&base.join("dm/name")).unwrap_or_else(|| name.to_string());
        id.model = Some(format!("device-mapper {dm}"));
        id.notes.push(format!(
            "device-mapper volume '{dm}'{built_on}: prefer imaging the underlying disk"
        ));
    } else if name.starts_with("md") {
        let level = read_text(&base.join("md/level")).unwrap_or_else(|| "md".into());
        id.model = Some(format!("software RAID ({level})"));
        id.notes.push(format!("md RAID array ({level}){built_on}"));
    } else if is_emmc_hw_partition(name) {
        id.notes.push(
            "eMMC hardware boot/RPMB partition: not a data area, not offered for imaging by default"
                .into(),
        );
    }
}

fn usb_identity(ctx: &ScanCtx<'_>, dev: &Path) -> Option<UsbIdentity> {
    let mut cur = dev.to_path_buf();
    let mut child: Option<PathBuf> = None;
    let mut found = false;
    for _ in 0..16 {
        if cur.join("idVendor").is_file() {
            found = true;
            break;
        }
        if cur == ctx.sys_canon || !cur.starts_with(&ctx.sys_canon) {
            break;
        }
        let parent = cur.parent()?.to_path_buf();
        child = Some(cur);
        cur = parent;
    }
    if !found {
        return None;
    }
    let lower = |s: Option<String>| s.map(|v| v.to_ascii_lowercase());
    let driver = child.as_ref().and_then(|i| {
        fs::read_link(i.join("driver"))
            .ok()
            .and_then(|l| l.file_name().map(|n| n.to_string_lossy().into_owned()))
    });
    Some(UsbIdentity {
        vendor_id: lower(read_text(&cur.join("idVendor"))),
        product_id: lower(read_text(&cur.join("idProduct"))),
        serial: read_text(&cur.join("serial")),
        manufacturer: read_text(&cur.join("manufacturer")),
        product: read_text(&cur.join("product")),
        driver,
        interface_protocol: child
            .as_ref()
            .and_then(|i| read_text(&i.join("bInterfaceProtocol"))),
        speed: read_text(&cur.join("speed")),
        devpath: read_text(&cur.join("devpath")),
        busnum: read_text(&cur.join("busnum")),
    })
}

// ---------------------------------------------------------------------------
// One disk.

fn scan_disk(ctx: &ScanCtx<'_>, name: &str) -> Option<SysDevice> {
    if !valid_name(name) {
        return None;
    }
    let base = ctx.roots.sysfs.join("block").join(name);
    if read_flag(&base.join("hidden")) == Some(true) {
        log::debug!("{name}: hidden, skipped");
        return None;
    }
    let Some(size_sectors) = read_u64(&base.join("size")) else {
        log::debug!("{name}: no readable size (removed while enumerating?), skipped");
        return None;
    };
    let size = size_sectors.saturating_mul(512);
    let real = fs::canonicalize(&base).unwrap_or_else(|_| base.clone());
    let rel = ctx.rel(&real);
    let devt = read_str(&base.join("dev")).and_then(|s| parse_majmin(&s));
    let removable = read_flag(&base.join("removable")).unwrap_or(false);
    let read_only = read_flag(&base.join("ro")).unwrap_or(false);
    let diskseq = read_u64(&base.join("diskseq"));
    let q = base.join("queue");
    let lbs_raw = read_u64(&q.join("logical_block_size"));
    let logical_sector = lbs_raw.and_then(sane_sector).unwrap_or(512);
    let physical_sector = read_u64(&q.join("physical_block_size"))
        .and_then(sane_sector)
        .filter(|p| *p >= logical_sector)
        .unwrap_or(logical_sector);
    let rotational = read_flag(&q.join("rotational"));
    let max_sectors_kb = read_u32(&q.join("max_sectors_kb"));
    let zoned = read_text(&q.join("zoned"));
    let transport = transport_from_sysfs_path(&rel, name);
    let dev_dir = fs::canonicalize(base.join("device"))
        .ok()
        .filter(|d| d.starts_with(&ctx.sys_canon));
    let udev = devt.and_then(|d| ctx.udev(d));

    let mut id = Identity::default();
    match transport {
        Transport::Nvme => nvme_identity(&base, dev_dir.as_deref(), &mut id),
        Transport::Mmc => mmc_identity(dev_dir.as_deref(), &mut id),
        Transport::Virtual => virtual_identity(&base, name, &mut id),
        Transport::Virtio => {
            id.serial = read_text(&base.join("serial"))
                .or_else(|| dev_dir.as_ref().and_then(|d| read_text(&d.join("serial"))));
        }
        _ => scsi_identity(dev_dir.as_deref(), &mut id),
    }
    if is_emmc_hw_partition(name) {
        virtual_identity(&base, name, &mut id);
    }
    let usb = if transport == Transport::Usb {
        dev_dir.as_deref().and_then(|d| usb_identity(ctx, d))
    } else {
        None
    };
    if id.serial.is_none() {
        id.serial = udev
            .as_ref()
            .and_then(|u| u.prop("ID_SERIAL_SHORT"))
            .map(|s| s.trim().to_string());
    }
    if id.serial.is_none() {
        id.serial = usb.as_ref().and_then(|u| u.serial.clone());
    }
    if transport == Transport::Ata {
        if let Some(m) = udev
            .as_ref()
            .and_then(|u| u.prop("ID_MODEL_ENC"))
            .map(decode_udev_enc)
            .filter(|m| !m.is_empty())
        {
            id.model = Some(m);
        }
    }
    if id
        .vendor
        .as_deref()
        .is_some_and(|v| v.eq_ignore_ascii_case("ATA"))
    {
        id.vendor = None;
    }
    if let Some(u) = &usb {
        if id.model.is_none() {
            id.model = u.product.clone();
        }
        if id.vendor.is_none() {
            id.vendor = u.manufacturer.clone();
        }
    }

    let udev_rpm = udev.as_ref().and_then(|u| u.ata_rotation_rpm());
    let mut input = ClassifyInput::new(name, transport);
    input.rotational = rotational;
    input.removable = removable;
    input.size = size;
    input.scsi_type = id.scsi_type;
    input.vendor = id.vendor.as_deref();
    input.model = id.model.as_deref();
    input.usb_vendor_id = usb.as_ref().and_then(|u| u.vendor_id.as_deref());
    input.usb_product = usb.as_ref().and_then(|u| u.product.as_deref());
    input.usb_driver = usb.as_ref().and_then(|u| u.driver.as_deref());
    input.udev_ata_rpm = udev_rpm;
    input.vpd_b1 = id.vpd_b1;
    input.mmc_type = id.mmc_type.as_deref();
    let classification = classify(&input);
    let kind = classification.kind;

    // Partitions, holders and mounts.
    let mut partitions = Vec::new();
    for (pname, pdir) in ctx.partition_dirs(&base) {
        let Some(number) = read_u32(&pdir.join("partition")) else {
            continue;
        };
        let pdevt = read_str(&pdir.join("dev")).and_then(|s| parse_majmin(&s));
        let mut members = vec![Member {
            name: pname.clone(),
            devt: pdevt,
        }];
        let mut seen = BTreeSet::new();
        seen.insert(pname.clone());
        ctx.holders(&pdir, &mut members, &mut seen, 0);
        partitions.push(PartitionInfo {
            path: Some(ctx.roots.dev.join(&pname)),
            number,
            start: read_u64(&pdir.join("start"))
                .unwrap_or(0)
                .saturating_mul(512),
            size: read_u64(&pdir.join("size"))
                .unwrap_or(0)
                .saturating_mul(512),
            fs_type: pdevt
                .and_then(|d| ctx.udev(d))
                .and_then(|u| u.prop("ID_FS_TYPE").map(String::from)),
            mountpoints: ctx.mounts_for(&members),
        });
    }
    partitions.sort_by_key(|p| (p.number, p.start));
    let set = ctx.device_set(&base, name);
    let part_names: BTreeSet<String> = partitions
        .iter()
        .filter_map(|p| p.path.as_ref().and_then(|x| x.file_name()))
        .map(|f| f.to_string_lossy().into_owned())
        .collect();
    let holders: Vec<String> = set
        .iter()
        .skip(1)
        .filter(|m| !part_names.contains(&m.name))
        .map(|m| m.name.clone())
        .collect();
    let mountpoints = ctx.mounts_for(&set);
    let stable_id = ctx.stable_id(name, udev.as_ref());

    // Warnings.
    let mut warnings: Vec<String> = Vec::new();
    if size == 0 {
        let mut w = String::from("device reports no capacity: controller failure is likely");
        if removable || kind == DeviceKind::Optical || classification.card_reader {
            w.push_str(" (or no card/disc is inserted)");
        }
        warnings.push(w);
    }
    if let Some(l) = lbs_raw {
        if sane_sector(l).is_none() {
            warnings.push(format!(
                "implausible logical block size {l}; assuming 512 bytes"
            ));
        }
    }
    if read_only {
        warnings.push(
            "device is marked read-only by the OS (write-protect switch, or a worn-out card/drive in \
             permanent read-only mode); reading still works"
                .into(),
        );
    }
    if let Some(st) = &id.state {
        let ok = matches!(st.as_str(), "running" | "live");
        if !ok {
            warnings.push(format!(
                "device state is '{st}': the kernel considers it unusable right now (reads will fail \
                 until it recovers or is re-attached)"
            ));
        }
    }
    if transport == Transport::Usb {
        warnings.push(format!(
            "USB bridges often misreport the medium type: detected as {} ({} confidence: {}); \
             override the device class if this is wrong",
            kind.display_name(),
            classification.confidence,
            classification.reasons.join("; ")
        ));
        if classification.card_reader {
            warnings.push(
                "memory card in a USB card reader: if reads keep failing, try another reader before \
                 giving up on the card"
                    .into(),
            );
        }
        if rotational == Some(true) && looks_like_ssd_model(id.model.as_deref()) {
            warnings.push(format!(
                "the USB bridge reports rotational=1 but the model '{}' looks like an SSD",
                id.model.as_deref().unwrap_or("")
            ));
        }
    } else if classification.confidence == Confidence::Low && kind != DeviceKind::Virtual {
        warnings.push(format!(
            "classification is a guess ({}): override the device class if this is wrong",
            classification.reasons.join("; ")
        ));
    }
    match &id.serial {
        Some(s) if is_generic_serial(s) && kind != DeviceKind::Virtual => warnings.push(format!(
            "serial number '{s}' looks generic (shared by many devices): after a reset the device is \
             re-identified by model, size and first sector"
        )),
        None if transport == Transport::Usb => warnings.push(
            "device reports no serial number: after a reset it is re-identified by model, size and \
             first sector"
                .into(),
        ),
        _ => {}
    }
    let generic_names = [
        "general",
        "generic",
        "mass storage",
        "usb flash disk",
        "udisk",
    ];
    if transport == Transport::Usb && !classification.card_reader {
        let v = id.vendor.as_deref().unwrap_or("").to_ascii_lowercase();
        let m = id.model.as_deref().unwrap_or("").to_ascii_lowercase();
        if generic_names
            .iter()
            .any(|g| v.trim() == *g || m.trim() == *g || m.contains(g))
        {
            warnings.push(format!(
                "generic identity '{} {}': no-name flash may be counterfeit (capacity larger than the real \
                 memory); validate recovered files",
                id.vendor.as_deref().unwrap_or(""),
                id.model.as_deref().unwrap_or("")
            ));
        }
    }
    if kind == DeviceKind::UsbFlash && size > 128 * TIB {
        warnings.push(format!(
            "reported capacity {size} bytes is implausible for a flash drive: the controller may be in a \
             failure mode; consider limiting the size to read"
        ));
    }
    if transport == Transport::Mmc {
        if id.mmc_manfid == Some(0) || id.mmc_oemid == Some(0) {
            warnings.push(
                "card reports a zero manufacturer/OEM id: possibly a clone or counterfeit card"
                    .into(),
            );
        }
        match id.mmc_pre_eol {
            Some(2) => {
                warnings.push("eMMC reports pre-EOL warning (80% of reserved blocks used)".into())
            }
            Some(3) => warnings
                .push("eMMC reports pre-EOL urgent (reserved blocks nearly exhausted)".into()),
            _ => {}
        }
        if let Some(c) = id.csd_structure {
            log::debug!(
                "{name}: CSD structure {c} ({})",
                match c {
                    0 => "SDSC",
                    1 => "SDHC/SDXC",
                    2 => "SDUC",
                    _ => "reserved",
                }
            );
        }
    }
    if let Some(z) = &zoned {
        if z != "none" {
            warnings.push(format!(
                "zoned (SMR) device ({z}): reads are unaffected but latency varies"
            ));
        }
    }
    warnings.extend(id.notes.iter().cloned());
    if !mountpoints.is_empty() {
        let list: Vec<String> = mountpoints
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        warnings.push(format!(
            "mounted at {}: unmount before imaging (a mounted filesystem may write to the device, and \
             reads can be inconsistent)",
            list.join(", ")
        ));
    }

    let info = DeviceInfo {
        path: ctx.roots.dev.join(name),
        name: name.to_string(),
        kind,
        transport,
        size,
        logical_sector,
        physical_sector,
        model: id.model.clone(),
        vendor: id.vendor.clone(),
        serial: id.serial.clone(),
        firmware: id.firmware.clone(),
        stable_id,
        removable,
        rotational: rotational.unwrap_or(false),
        read_only,
        partitions,
        mountpoints,
        health: None,
        warnings,
    };
    Some(SysDevice {
        info,
        devt,
        sysfs_path: real,
        diskseq,
        classification,
        max_sectors_kb,
        scsi_timeout_secs: id.scsi_timeout,
        state: id.state,
        zoned,
        wwid: id.wwid,
        usb,
        holders,
    })
}

// ---------------------------------------------------------------------------
// Public entry points.

/// Scan every whole disk under `roots` with all collected details.
pub fn scan_with(roots: &Roots) -> Result<Vec<SysDevice>> {
    let block = roots.sysfs.join("block");
    let rd = fs::read_dir(&block).map_err(|e| {
        Error::Io(std::io::Error::new(
            e.kind(),
            format!("cannot list {} (is sysfs mounted?): {e}", block.display()),
        ))
    })?;
    let mut names: Vec<String> = rd
        .take(MAX_ENTRIES)
        .filter_map(|e| e.ok())
        .filter_map(|e| entry_name(&e))
        .collect();
    names.sort_by_cached_key(|a| natural_key(a));
    let ctx = ScanCtx::new(roots);
    Ok(names.iter().filter_map(|n| scan_disk(&ctx, n)).collect())
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Chunk {
    Text(usize, String),
    Num(u64),
}

/// Sort key so that `sdb < sdaa` and `nvme2n1 < nvme10n1`: names are
/// grouped by their first two characters; inside a group digit runs
/// compare numerically and letter runs by length, then text.
fn natural_key(s: &str) -> (String, Vec<Chunk>) {
    let group: String = s.chars().take(2).collect();
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut digits = false;
    for c in s.chars() {
        let d = c.is_ascii_digit();
        if !cur.is_empty() && d != digits {
            out.push(chunk(&cur, digits));
            cur.clear();
        }
        digits = d;
        cur.push(c);
    }
    if !cur.is_empty() {
        out.push(chunk(&cur, digits));
    }
    (group, out)
}

fn chunk(s: &str, digits: bool) -> Chunk {
    if digits {
        Chunk::Num(s.parse().unwrap_or(u64::MAX))
    } else {
        Chunk::Text(s.len(), s.to_string())
    }
}

/// Enumerate whole disks (partitions are inside each [`DeviceInfo`]).
/// Virtual devices are included and marked [`DeviceKind::Virtual`].
pub fn enumerate_with(roots: &Roots) -> Result<Vec<DeviceInfo>> {
    Ok(scan_with(roots)?.into_iter().map(|d| d.info).collect())
}

/// Full details of one device by kernel name: a whole disk, or a partition
/// (reported as a partition view of its disk, see [`crate::partition_view`]).
pub fn describe_name_with(roots: &Roots, name: &str) -> Result<DeviceInfo> {
    if !valid_name(name) {
        return Err(Error::InvalidArgument(format!(
            "invalid device name '{name}'"
        )));
    }
    let ctx = ScanCtx::new(roots);
    if roots.sysfs.join("block").join(name).exists() {
        return scan_disk(&ctx, name).map(|d| d.info).ok_or_else(|| {
            Error::NotFound(format!("{name}: no usable sysfs entry (hidden or removed)"))
        });
    }
    let disk = parent_disk_of(&ctx, name).ok_or_else(|| {
        Error::NotFound(format!(
            "no block device named '{name}' in {}",
            roots.sysfs.display()
        ))
    })?;
    let d = scan_disk(&ctx, &disk)
        .ok_or_else(|| Error::NotFound(format!("{disk}: no usable sysfs entry")))?;
    let part = d
        .info
        .partitions
        .iter()
        .find(|p| {
            p.path
                .as_ref()
                .and_then(|x| x.file_name())
                .is_some_and(|f| f.to_string_lossy() == name)
        })
        .cloned()
        .ok_or_else(|| Error::NotFound(format!("partition {name} vanished")))?;
    let sid = ctx.stable_id(name, None);
    Ok(crate::partition_view(&d.info, &part, sid))
}

fn parent_disk_of(ctx: &ScanCtx<'_>, part: &str) -> Option<String> {
    let class = ctx.roots.sysfs.join("class/block").join(part);
    if let Ok(real) = fs::canonicalize(&class) {
        if real.join("partition").is_file() {
            if let Some(parent) = real.parent().and_then(|p| p.file_name()) {
                let n = parent.to_string_lossy().into_owned();
                if ctx.roots.sysfs.join("block").join(&n).exists() {
                    return Some(n);
                }
            }
        }
    }
    for e in list_dir(&ctx.roots.sysfs.join("block")) {
        if e.path().join(part).join("partition").is_file() {
            return entry_name(&e);
        }
    }
    None
}

fn name_for_devt(ctx: &ScanCtx<'_>, devt: (u32, u32)) -> Option<String> {
    let link = ctx
        .roots
        .sysfs
        .join("dev/block")
        .join(format!("{}:{}", devt.0, devt.1));
    if let Ok(real) = fs::canonicalize(&link) {
        if let Some(n) = real.file_name() {
            return Some(n.to_string_lossy().into_owned());
        }
    }
    for e in list_dir(&ctx.roots.sysfs.join("block")) {
        let Some(n) = entry_name(&e) else { continue };
        let p = e.path();
        if read_str(&p.join("dev")).and_then(|s| parse_majmin(&s)) == Some(devt) {
            return Some(n);
        }
        for (pn, pdir) in ctx.partition_dirs(&p) {
            if read_str(&pdir.join("dev")).and_then(|s| parse_majmin(&s)) == Some(devt) {
                return Some(pn);
            }
        }
    }
    None
}

fn devt_of(meta: &fs::Metadata) -> (u32, u32) {
    let d = meta.rdev() as libc::dev_t;
    (libc::major(d), libc::minor(d))
}

fn st_dev_of(meta: &fs::Metadata) -> (u32, u32) {
    let d = meta.dev() as libc::dev_t;
    (libc::major(d), libc::minor(d))
}

/// Describe an image file (a regular file used as a source).
pub(crate) fn image_info_with(
    roots: Option<&Roots>,
    path: &Path,
    meta: &fs::Metadata,
) -> DeviceInfo {
    let mut info = crate::image_file_info(path, meta.len(), meta.permissions().readonly());
    if let Some(roots) = roots {
        let ctx = ScanCtx::new(roots);
        let mounts = loop_mounts(&ctx, path);
        if !mounts.is_empty() {
            let list: Vec<String> = mounts.iter().map(|p| p.display().to_string()).collect();
            info.warnings.push(format!(
                "the image is loop-mounted at {}: unmount it first (a mounted filesystem may change the image)",
                list.join(", ")
            ));
            info.mountpoints = mounts;
        }
    }
    info
}

fn loop_mounts(ctx: &ScanCtx<'_>, path: &Path) -> Vec<PathBuf> {
    let Ok(canon) = fs::canonicalize(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (dir, n) in loops_backed_by(ctx, &canon) {
        let set = ctx.device_set(&dir, &n);
        for m in ctx.mounts_for(&set) {
            if !out.contains(&m) {
                out.push(m);
            }
        }
    }
    out
}

/// Describe what `path` is: a device node (whole disk, or a partition
/// reported as a partition view of its disk) or a regular file
/// ([`DeviceKind::ImageFile`], size from its metadata).
pub fn describe_with(roots: &Roots, path: &Path) -> Result<DeviceInfo> {
    let meta = fs::metadata(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::NotFound(format!("{}: no such file or device", path.display()))
        } else {
            Error::Io(e)
        }
    })?;
    if meta.is_file() {
        return Ok(image_info_with(Some(roots), path, &meta));
    }
    if !meta.file_type().is_block_device() {
        return Err(Error::InvalidArgument(format!(
            "{} is neither a block device nor a regular file",
            path.display()
        )));
    }
    let devt = devt_of(&meta);
    let ctx = ScanCtx::new(roots);
    match name_for_devt(&ctx, devt) {
        Some(name) => describe_name_with(roots, &name),
        None => {
            log::warn!(
                "{}: block device {}:{} not found in {}",
                path.display(),
                devt.0,
                devt.1,
                roots.sysfs.display()
            );
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let mut info = crate::image_file_info(path, 0, true);
            info.name = name;
            info.kind = DeviceKind::Unknown;
            info.warnings = vec![format!(
                "no sysfs information for block device {}:{}; size and type are unknown until opened",
                devt.0, devt.1
            )];
            Ok(info)
        }
    }
}

/// Partition views (see [`crate::partition_view`]) of a disk, each with the
/// stable id of the partition's own by-id link.
pub fn partition_views_with(roots: &Roots, disk: &DeviceInfo) -> Vec<DeviceInfo> {
    let ctx = ScanCtx::new(roots);
    disk.partitions
        .iter()
        .map(|p| {
            let name = p
                .path
                .as_ref()
                .and_then(|x| x.file_name())
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default();
            crate::partition_view(disk, p, ctx.stable_id(&name, None))
        })
        .collect()
}

/// Re-find a device by fingerprint among whole disks and partitions.
/// Exact matches win; weak identities are confirmed by reading LBA 0 when
/// the fingerprint has a first-sector hash, otherwise accepted only when
/// unambiguous. A fingerprint with a unique serial number (a physical
/// device) never matches a virtual device: a loop device over an image of
/// the disk has the same first sector.
pub fn find_by_fingerprint_with(roots: &Roots, fp: &Fingerprint) -> Result<Option<DeviceInfo>> {
    let mut disks = enumerate_with(roots)?;
    if fp.has_strong_serial() {
        disks.retain(|d| d.kind != DeviceKind::Virtual);
    }
    let mut cands = disks.clone();
    for d in &disks {
        cands.extend(partition_views_with(roots, d));
    }
    let sel = select_candidate(fp, &Lost::default(), &cands, true, &|_| false);
    Ok(resolve_selection(sel, fp))
}

/// `diskseq` of a disk by kernel name, or of a partition's disk: a number
/// the kernel increments on every attach and media change (Linux >= 5.15).
pub(crate) fn diskseq_of(roots: &Roots, name: &str) -> Option<u64> {
    if !valid_name(name) {
        return None;
    }
    let disk = roots.sysfs.join("block").join(name);
    if disk.exists() {
        return read_u64(&disk.join("diskseq"));
    }
    let ctx = ScanCtx {
        roots,
        sys_canon: roots.sysfs.clone(),
        dev_norm: normalize_lexically(&roots.dev),
        mounts: Vec::new(),
        mount_names: Vec::new(),
        swap_names: Vec::new(),
        by_id: BTreeMap::new(),
    };
    let parent = parent_disk_of(&ctx, name)?;
    read_u64(&roots.sysfs.join("block").join(parent).join("diskseq"))
}

/// The disk directory and name a [`DeviceInfo`] refers to (the parent disk
/// for partition views).
fn disk_of(ctx: &ScanCtx<'_>, info: &DeviceInfo) -> Option<(PathBuf, String)> {
    let name = &info.name;
    if valid_name(name) {
        let p = ctx.roots.sysfs.join("block").join(name);
        if p.exists() {
            return Some((p, name.clone()));
        }
        if let Some(d) = parent_disk_of(ctx, name) {
            return Some((ctx.roots.sysfs.join("block").join(&d), d));
        }
    }
    None
}

fn source_members(ctx: &ScanCtx<'_>, info: &DeviceInfo) -> Vec<Member> {
    if let Some((dir, name)) = disk_of(ctx, info) {
        return ctx.device_set(&dir, &name);
    }
    // Not in sysfs: fall back to the node's own device number.
    match fs::metadata(&info.path) {
        Ok(m) if m.file_type().is_block_device() => vec![Member {
            name: info.name.clone(),
            devt: Some(devt_of(&m)),
        }],
        _ => Vec::new(),
    }
}

/// Mountpoints of a device node (with its partitions and anything stacked
/// on them) or, for an image file, of loop devices backed by it.
pub fn mountpoints_of_with(roots: &Roots, path: &Path) -> Result<Vec<PathBuf>> {
    let meta = fs::metadata(path).map_err(Error::Io)?;
    let ctx = ScanCtx::new(roots);
    if meta.is_file() {
        return Ok(loop_mounts(&ctx, path));
    }
    if !meta.file_type().is_block_device() {
        return Err(Error::InvalidArgument(format!(
            "{} is neither a block device nor a regular file",
            path.display()
        )));
    }
    let info = describe_with(roots, path)?;
    Ok(ctx.mounts_for(&mount_members(&ctx, &info)))
}

/// Whether the device (any partition, anything stacked on it, swap) or the
/// image file (through a loop device) is mounted right now.
pub fn is_mounted_with(roots: &Roots, info: &DeviceInfo) -> bool {
    if info.is_mounted() {
        return true;
    }
    let ctx = ScanCtx::new(roots);
    if info.kind == DeviceKind::ImageFile {
        return !loop_mounts(&ctx, &info.path).is_empty();
    }
    !ctx.mounts_for(&mount_members(&ctx, info)).is_empty()
}

/// What counts as "mounted" for `info`: for a partition view the partition
/// and everything stacked on it, for a whole disk the full device set.
fn mount_members(ctx: &ScanCtx<'_>, info: &DeviceInfo) -> Vec<Member> {
    if crate::is_partition_view(info) {
        if let Some(dir) = ctx.block_dir(&info.name) {
            let mut members = vec![Member {
                name: info.name.clone(),
                devt: read_str(&dir.join("dev")).and_then(|s| parse_majmin(&s)),
            }];
            let mut seen = BTreeSet::new();
            seen.insert(info.name.clone());
            ctx.holders(&dir, &mut members, &mut seen, 0);
            return members;
        }
    }
    source_members(ctx, info)
}

/// Symbolic links followed while resolving an output path (the kernel's
/// own limit for one path walk is 40).
const MAX_SYMLINKS: usize = 40;

/// Resolve `path` the way the kernel will when the output is created: every
/// symbolic link is followed, including a *dangling* one (creating a file
/// through it creates the link target), and `..` is applied after links
/// were resolved. Components that do not exist yet are kept as a lexical
/// tail. Returns `(nearest existing directory or file, full resolved path)`.
pub(crate) fn resolve_output_path(path: &Path) -> Result<(PathBuf, PathBuf)> {
    use std::ffi::OsString;
    use std::path::Component;

    let abs = std::path::absolute(path).map_err(Error::Io)?;
    // Components still to walk, last one first.
    let mut pending: Vec<OsString> = Vec::new();
    let push_components = |pending: &mut Vec<OsString>, p: &Path| {
        let comps: Vec<OsString> = p
            .components()
            .filter_map(|c| match c {
                Component::Normal(n) => Some(n.to_os_string()),
                Component::ParentDir => Some(OsString::from("..")),
                Component::CurDir | Component::RootDir | Component::Prefix(_) => None,
            })
            .collect();
        pending.extend(comps.into_iter().rev());
    };
    push_components(&mut pending, &abs);
    let mut cur = PathBuf::from("/");
    let mut missing: Vec<OsString> = Vec::new();
    let mut links = 0usize;
    while let Some(c) = pending.pop() {
        if c == ".." {
            if missing.pop().is_none() {
                cur.pop();
            }
            continue;
        }
        if !missing.is_empty() {
            missing.push(c);
            continue;
        }
        let cand = cur.join(&c);
        match fs::symlink_metadata(&cand) {
            Ok(m) if m.file_type().is_symlink() => {
                links += 1;
                if links > MAX_SYMLINKS {
                    return Err(Error::InvalidArgument(format!(
                        "{}: too many levels of symbolic links",
                        path.display()
                    )));
                }
                let target = fs::read_link(&cand).map_err(Error::Io)?;
                if target.is_absolute() {
                    cur = PathBuf::from("/");
                }
                push_components(&mut pending, &target);
            }
            Ok(_) => cur = cand,
            // Not there yet, below a file, or below a directory we may not
            // search (then nothing can be created there by us either): the
            // rest is judged by the deepest directory we could reach.
            Err(e)
                if e.kind() == std::io::ErrorKind::NotFound
                    || e.kind() == std::io::ErrorKind::PermissionDenied
                    || e.raw_os_error() == Some(libc::ENOTDIR) =>
            {
                missing.push(c)
            }
            Err(e) => return Err(Error::Io(e)),
        }
    }
    let mut full = cur.clone();
    for m in &missing {
        full.push(m);
    }
    Ok((cur, full))
}

/// Whether `name` (a disk, partition or stacked volume) is built on one of
/// `targets`: its parent disk (for a partition) or its `slaves/`,
/// recursively. `seen` keeps shared lower devices from being walked twice.
fn lower_reaches(
    ctx: &ScanCtx<'_>,
    name: &str,
    targets: &BTreeSet<String>,
    seen: &mut BTreeSet<String>,
    depth: usize,
) -> bool {
    if depth > MAX_DEPTH || seen.len() > MAX_MEMBERS || !seen.insert(name.to_string()) {
        return false;
    }
    let Some(dir) = ctx.block_dir(name) else {
        return false;
    };
    let mut lower: Vec<String> = list_dir(&dir.join("slaves"))
        .iter()
        .filter_map(entry_name)
        .collect();
    if dir.join("partition").is_file() {
        // A partition of a stacked device (md0p1, a dm partition): what
        // its disk is built on counts too.
        if let Some(parent) = fs::canonicalize(&dir)
            .ok()
            .and_then(|r| r.parent().and_then(|p| p.file_name()).map(|n| n.to_owned()))
        {
            lower.push(parent.to_string_lossy().into_owned());
        }
    }
    for s in lower {
        if targets.contains(&s) || lower_reaches(ctx, &s, targets, seen, depth + 1) {
            return true;
        }
    }
    false
}

/// The physical disks a stacked source (dm, md) is built on, following
/// `slaves/` down and mapping partitions to their disks.
fn lower_disks(ctx: &ScanCtx<'_>, name: &str, out: &mut BTreeSet<String>, depth: usize) {
    if depth > MAX_DEPTH || out.len() > MAX_MEMBERS {
        return;
    }
    let Some(dir) = ctx.block_dir(name) else {
        return;
    };
    for e in list_dir(&dir.join("slaves")) {
        let Some(s) = entry_name(&e) else { continue };
        let disk = if ctx.roots.sysfs.join("block").join(&s).exists() {
            Some(s.clone())
        } else {
            parent_disk_of(ctx, &s)
        };
        if let Some(d) = disk {
            if out.insert(d.clone()) {
                lower_disks(ctx, &d, out, depth + 1);
            }
        }
    }
}

/// Loop devices whose backing file is `file` (canonical path).
fn loops_backed_by(ctx: &ScanCtx<'_>, file: &Path) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    for e in list_dir(&ctx.roots.sysfs.join("block")) {
        let Some(n) = entry_name(&e) else { continue };
        if !n.starts_with("loop") {
            continue;
        }
        let Some(backing) = read_text(&e.path().join("loop/backing_file")) else {
            continue;
        };
        let backing = backing.trim_end_matches(" (deleted)");
        let same = Path::new(backing) == file
            || fs::canonicalize(backing).is_ok_and(|c| c.as_path() == file);
        if same {
            out.push((e.path(), n));
        }
    }
    out
}

/// What the output guard compares against.
struct GuardSet {
    devts: BTreeSet<(u32, u32)>,
    names: BTreeSet<String>,
}

impl GuardSet {
    fn add(&mut self, members: Vec<Member>) {
        for m in members {
            if let Some(d) = m.devt {
                self.devts.insert(d);
            }
            self.names.insert(m.name);
        }
    }
}

/// Everything writing to which would land on the source: the disk (the
/// parent disk for a partition), its partitions and everything stacked on
/// them; for a dm/md source also the disks it is built on (writing there
/// wears the same failing medium and, for thin or snapshot volumes, can
/// overwrite the source's data); for an image file the loop devices backed
/// by it.
fn guard_set(ctx: &ScanCtx<'_>, info: &DeviceInfo) -> GuardSet {
    let mut set = GuardSet {
        devts: BTreeSet::new(),
        names: BTreeSet::new(),
    };
    if info.kind == DeviceKind::ImageFile {
        if let Ok(canon) = fs::canonicalize(&info.path) {
            for (dir, name) in loops_backed_by(ctx, &canon) {
                set.add(ctx.device_set(&dir, &name));
            }
        }
        return set;
    }
    set.add(source_members(ctx, info));
    if let Some((_, disk)) = disk_of(ctx, info) {
        let mut lower = BTreeSet::new();
        lower_disks(ctx, &disk, &mut lower, 0);
        for d in lower {
            let dir = ctx.roots.sysfs.join("block").join(&d);
            set.add(ctx.device_set(&dir, &d));
        }
    }
    set
}

/// The mount that owns `canon` (longest matching mount point; for stacked
/// mounts on the same point the last, visible one).
fn owner_mount<'c>(
    ctx: &'c ScanCtx<'_>,
    canon: &Path,
) -> Option<(&'c MountEntry, Option<&'c str>)> {
    ctx.mounts
        .iter()
        .zip(&ctx.mount_names)
        .filter(|(m, _)| canon.starts_with(&m.mount_point))
        .max_by_key(|(m, _)| m.mount_point.components().count())
        .map(|(m, n)| (m, n.as_deref()))
}

/// Devices of the btrfs filesystem that `member` belongs to
/// (`/sys/fs/btrfs/<uuid>/devices/`).
fn btrfs_devices(ctx: &ScanCtx<'_>, member: &str) -> Vec<String> {
    for fsdir in list_dir(&ctx.roots.sysfs.join("fs/btrfs")) {
        let devs = fsdir.path().join("devices");
        if !devs.join(member).exists() {
            continue;
        }
        return list_dir(&devs).iter().filter_map(entry_name).collect();
    }
    Vec::new()
}

/// Whether a device by kernel name is (built on) a member of the set, or
/// is a loop device whose backing file is on the source.
fn name_on_source(ctx: &ScanCtx<'_>, name: &str, set: &GuardSet, depth: usize) -> bool {
    if set.names.contains(name) {
        return true;
    }
    let mut seen = BTreeSet::new();
    if lower_reaches(ctx, name, &set.names, &mut seen, 0) {
        return true;
    }
    // A loop device (or anything built on one) passes writes to its
    // backing file: follow it to the filesystem that holds the file.
    seen.insert(name.to_string());
    for n in seen {
        if !n.starts_with("loop") || depth >= 4 {
            continue;
        }
        let Some(dir) = ctx.block_dir(&n) else {
            continue;
        };
        let Some(backing) = read_text(&dir.join("loop/backing_file")) else {
            continue;
        };
        let backing = PathBuf::from(backing.trim_end_matches(" (deleted)"));
        if let Ok(on) = path_on_set(ctx, &backing, set, depth + 1) {
            if on {
                return true;
            }
        }
    }
    false
}

/// Whether `path` (resolved as for creation) is stored on the set.
fn path_on_set(ctx: &ScanCtx<'_>, path: &Path, set: &GuardSet, depth: usize) -> Result<bool> {
    let (existing, canon) = resolve_output_path(path)?;
    let meta = fs::metadata(&existing).map_err(Error::Io)?;
    let devt = st_dev_of(&meta);
    if set.devts.contains(&devt) {
        return Ok(true);
    }
    if let Some(out_dev) = name_for_devt(ctx, devt) {
        if name_on_source(ctx, &out_dev, set, depth) {
            return Ok(true);
        }
    }
    // Anonymous device numbers (btrfs, fuse/ntfs-3g, ...): look at the
    // mount that owns the path.
    if let Some((m, Some(src))) = owner_mount(ctx, &canon) {
        if name_on_source(ctx, src, set, depth) {
            return Ok(true);
        }
        if m.fs_type == "btrfs" {
            for d in btrfs_devices(ctx, src) {
                if name_on_source(ctx, &d, set, depth) {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

/// Whether `path` lives on the device described by `info` (the output
/// guard). The path is resolved as the kernel will resolve it when the
/// output is created: symbolic links anywhere in it are followed (a
/// dangling link at the end points at where a new file would really be
/// created) and, for the part that does not exist yet, the nearest existing
/// ancestor is examined. It is on the device when its filesystem's device
/// is:
///
/// * the disk (the parent disk when `info` is a partition), one of its
///   partitions or anything stacked on them (LVM, dm-crypt, md: found
///   through `holders/`, and through `slaves/` of the output's device);
/// * for a dm/md source, any disk the source is built on;
/// * a loop device whose backing file is itself on the device;
/// * for filesystems with anonymous device numbers (btrfs, fuse, ntfs-3g),
///   the mount's source device, or any device of a multi-device btrfs.
///
/// For an image file, it is "on the device" when it is the image itself or
/// inside a filesystem mounted from a loop device backed by the image;
/// writing next to an image is fine.
pub fn path_is_on_device_with(roots: &Roots, path: &Path, info: &DeviceInfo) -> Result<bool> {
    let ctx = ScanCtx::new(roots);
    let set = guard_set(&ctx, info);
    if info.kind == DeviceKind::ImageFile {
        let (_, canon) = resolve_output_path(path)?;
        if fs::canonicalize(&info.path).is_ok_and(|img| img == canon) {
            return Ok(true);
        }
        if set.names.is_empty() {
            return Ok(false);
        }
        return path_on_set(&ctx, path, &set, 0);
    }
    if set.devts.is_empty() && set.names.is_empty() {
        return Err(Error::NotFound(format!(
            "cannot determine the device numbers of {}",
            info.path.display()
        )));
    }
    path_on_set(&ctx, path, &set, 0)
}

/// Open a device or image file read-only, describing it through `roots`.
pub fn open_device_with(roots: &Roots, path: &Path, opts: &OpenOptions) -> Result<DeviceSource> {
    DeviceSource::open_with_ctx(
        path,
        opts,
        Ctx {
            roots: roots.clone(),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transports() {
        let t = |p: &str, n: &str| transport_from_sysfs_path(p, n);
        assert_eq!(
            t("/devices/pci0000:00/0000:00:14.0/usb1/1-3/1-3:1.0/host6/target6:0:0/6:0:0:0/block/sdb", "sdb"),
            Transport::Usb
        );
        assert_eq!(
            t(
                "/devices/pci0000:00/0000:00:17.0/ata1/host0/target0:0:0/0:0:0:0/block/sda",
                "sda"
            ),
            Transport::Ata
        );
        assert_eq!(
            t(
                "/devices/pci0000:00/0000:00:1d.0/0000:3d:00.0/nvme/nvme0/nvme0n1",
                "nvme0n1"
            ),
            Transport::Nvme
        );
        assert_eq!(
            t(
                "/devices/virtual/nvme-subsystem/nvme-subsys0/nvme0n1",
                "nvme0n1"
            ),
            Transport::Nvme
        );
        assert_eq!(
            t("/devices/virtual/block/loop0", "loop0"),
            Transport::Virtual
        );
        assert_eq!(t("/whatever", "dm-0"), Transport::Virtual);
        assert_eq!(
            t(
                "/devices/platform/soc/mmc_host/mmc0/mmc0:aaaa/block/mmcblk0",
                "mmcblk0"
            ),
            Transport::Mmc
        );
        assert_eq!(
            t("/devices/pci0000:00/virtio1/block/vda", "vda"),
            Transport::Virtio
        );
        assert_eq!(
            t("/devices/pci0000:00/0000:00:01.0/host3/port-3:0/end_device-3:0/target3:0:0/3:0:0:0/block/sdc", "sdc"),
            Transport::Scsi
        );
        assert_eq!(
            t("/devices/platform/foo/block/xyz", "xyz"),
            Transport::Unknown
        );
        // "usbmon" or "hostile" components are not numbered matches.
        assert_eq!(
            t("/devices/usbx/hostile/block/sdz", "sdz"),
            Transport::Unknown
        );
    }

    #[test]
    fn roots_layout() {
        let r = Roots::under(Path::new("/tmp/x"));
        assert_eq!(r.swaps(), PathBuf::from("/tmp/x/proc/swaps"));
        assert_eq!(Roots::system().swaps(), PathBuf::from("/proc/swaps"));
        assert_eq!(Roots::default(), Roots::system());
    }

    #[test]
    fn link_priorities_and_names() {
        let mut v = [
            "wwn-0x5000c500".to_string(),
            "nvme-eui.0001".to_string(),
            "nvme-KXG60_SER_1".to_string(),
            "nvme-KXG60_SER".to_string(),
            "lvm-pv-uuid-abc".to_string(),
        ];
        v.sort_by_key(|l| (link_priority(l), l.len(), l.clone()));
        assert_eq!(v[0], "nvme-KXG60_SER");
        assert_eq!(mmc_vendor(3), Some("SanDisk"));
        assert_eq!(mmc_vendor(0x9f), None);
        assert!(!valid_name(".."));
        assert!(!valid_name("a/b"));
        assert!(natural_key("sdb") < natural_key("sdaa"));
        assert!(natural_key("nvme2n1") < natural_key("nvme10n1"));
        assert!(natural_key("loop9") < natural_key("loop10"));
        assert!(natural_key("dm-0") < natural_key("sr0"));
        assert!(natural_key("sdz") < natural_key("sr0"));
    }
}
