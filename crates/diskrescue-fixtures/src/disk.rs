//! Partitioned disk images.
//!
//! [`build`] wraps one or more filesystem images (see [`crate::fs`]) in an
//! MBR or GPT partition table, or places a single image at offset 0 (a
//! "superfloppy", the layout of most SD cards and USB sticks).
//!
//! * MBR: disk signature from the seed, up to four primary entries with
//!   computed CHS (or `FE FF FF` beyond 1023 cylinders), LBA start/size,
//!   `55 AA`.
//! * GPT: protective MBR, primary header at LBA 1 and entry array at LBA 2,
//!   backup array and header at the end of the disk, 128 entries of 128
//!   bytes, CRC32 of the header and of the array, disk and partition GUIDs
//!   derived from the seed.
//!
//! The output is a sparse file: only the non-zero parts of the filesystem
//! images and the table structures are written, so a multi-gigabyte disk
//! costs a few hundred kilobytes. Partitions start 1 MiB-aligned unless
//! `start` is given. Sector sizes 512 and 4096 are supported for the
//! table; the filesystem images keep their own internal sector size.
//!
//! No external tool is needed.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::error::{FixtureError, Result};
use crate::rng::Rng;

/// Partitioning scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Scheme {
    /// Master boot record.
    #[default]
    Mbr,
    /// GUID partition table (with protective MBR).
    Gpt,
    /// No table: the single image sits at offset 0.
    None,
}

/// Partition type: an MBR type byte or a GPT type GUID (canonical text
/// order, i.e. as printed by tools).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartType {
    /// MBR system ID.
    Mbr(u8),
    /// GPT partition type GUID in canonical byte order.
    Gpt([u8; 16]),
}

/// GPT type GUID "Microsoft basic data".
pub const GUID_BASIC_DATA: [u8; 16] = [
    0xEB, 0xD0, 0xA0, 0xA2, 0xB9, 0xE5, 0x44, 0x33, 0x87, 0xC0, 0x68, 0xB6, 0xB7, 0x26, 0x99, 0xC7,
];
/// GPT type GUID "Linux filesystem data".
pub const GUID_LINUX_FS: [u8; 16] = [
    0x0F, 0xC6, 0x3D, 0xAF, 0x84, 0x83, 0x47, 0x72, 0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D, 0xE4,
];

/// One partition to place. Construct with [`PartSpec::new`] or
/// `PartSpec { fs_image, ..Default::default() }`.
#[derive(Debug, Clone, Default)]
pub struct PartSpec {
    /// Filesystem image to copy into the partition.
    pub fs_image: PathBuf,
    /// Start offset in bytes (multiple of the sector size); `None` = the next
    /// 1 MiB boundary after the previous partition.
    pub start: Option<u64>,
    /// Type; `None` = detected from the image (FAT/NTFS/exFAT/ext).
    pub type_hint: Option<PartType>,
    /// GPT partition name; `None` = `partN`.
    pub name: Option<String>,
}

impl PartSpec {
    /// A partition holding `fs_image` at the default position with a
    /// detected type.
    pub fn new(fs_image: impl Into<PathBuf>) -> Self {
        PartSpec {
            fs_image: fs_image.into(),
            start: None,
            type_hint: None,
            name: None,
        }
    }
}

/// Disk to build. Construct with [`DiskSpec::new`] or
/// `DiskSpec { scheme, parts, ..Default::default() }`.
#[derive(Debug, Clone)]
pub struct DiskSpec {
    /// Partitioning scheme.
    pub scheme: Scheme,
    /// Logical sector size of the table: 512 or 4096.
    pub sector: u32,
    /// Partitions in table order.
    pub parts: Vec<PartSpec>,
    /// Seed for the MBR signature and the GPT GUIDs.
    pub seed: u64,
    /// Minimum disk size in bytes (the file is extended sparsely); `None` =
    /// just enough.
    pub total_size: Option<u64>,
}

impl Default for DiskSpec {
    /// An empty MBR disk with 512-byte sectors and seed 1.
    fn default() -> Self {
        DiskSpec::new(Scheme::Mbr, Vec::new())
    }
}

impl DiskSpec {
    /// Spec with 512-byte sectors, seed 1 and no explicit size.
    pub fn new(scheme: Scheme, parts: Vec<PartSpec>) -> Self {
        DiskSpec {
            scheme,
            sector: 512,
            parts,
            seed: 1,
            total_size: None,
        }
    }
}

/// What [`build`] produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskReport {
    /// `(start, size)` in bytes for every partition, in table order.
    pub partitions: Vec<(u64, u64)>,
    /// Disk size in bytes.
    pub total_size: u64,
    /// Sector size used.
    pub sector: u32,
    /// MBR disk signature (also set for GPT's protective MBR: zero).
    pub mbr_signature: u32,
    /// GPT disk GUID in on-disk byte order (all zero for MBR/None).
    pub disk_guid: [u8; 16],
    /// GPT partition GUIDs in on-disk byte order (empty for MBR/None).
    pub partition_guids: Vec<[u8; 16]>,
}

const ALIGN: u64 = 1 << 20;

fn align_up(v: u64, a: u64) -> u64 {
    v.div_ceil(a) * a
}

/// Parse a GUID in text form (`EBD0A0A2-B9E5-4433-87C0-68B6B72699C7`) to the
/// canonical byte order used by [`PartType::Gpt`].
pub fn guid_from_str(s: &str) -> Option<[u8; 16]> {
    let hex: String = s.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

/// Convert canonical GUID bytes to the mixed-endian on-disk layout.
pub fn guid_to_disk(g: [u8; 16]) -> [u8; 16] {
    let mut o = g;
    o[0..4].copy_from_slice(&[g[3], g[2], g[1], g[0]]);
    o[4..6].copy_from_slice(&[g[5], g[4]]);
    o[6..8].copy_from_slice(&[g[7], g[6]]);
    o
}

fn random_guid(rng: &mut Rng) -> [u8; 16] {
    let mut g = [0u8; 16];
    rng.fill(&mut g);
    g[6] = (g[6] & 0x0F) | 0x40;
    g[8] = (g[8] & 0x3F) | 0x80;
    g
}

/// Guess the filesystem family of an image from its first sectors.
fn detect(path: &Path) -> Result<Detected> {
    let mut f = File::open(path)?;
    let mut head = vec![0u8; 2048];
    let n = f.read(&mut head)?;
    head.truncate(n);
    if head.len() < 1090 {
        return Ok(Detected::Other);
    }
    if &head[3..11] == b"NTFS    " || &head[3..11] == b"EXFAT   " {
        return Ok(Detected::NtfsLike);
    }
    if &head[82..87] == b"FAT32" {
        return Ok(Detected::Fat32);
    }
    if &head[54..59] == b"FAT12" {
        return Ok(Detected::Fat12);
    }
    if &head[54..59] == b"FAT16" {
        return Ok(Detected::Fat16);
    }
    if head[1024 + 56] == 0x53 && head[1024 + 57] == 0xEF {
        return Ok(Detected::Ext);
    }
    Ok(Detected::Other)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Detected {
    Fat12,
    Fat16,
    Fat32,
    NtfsLike,
    Ext,
    Other,
}

fn default_type(d: Detected, scheme: Scheme) -> PartType {
    match scheme {
        Scheme::Gpt => PartType::Gpt(if d == Detected::Ext {
            GUID_LINUX_FS
        } else {
            GUID_BASIC_DATA
        }),
        _ => PartType::Mbr(match d {
            Detected::Fat12 => 0x01,
            Detected::Fat16 => 0x0E,
            Detected::Fat32 => 0x0C,
            Detected::NtfsLike => 0x07,
            Detected::Ext | Detected::Other => 0x83,
        }),
    }
}

fn chs(lba: u64) -> [u8; 3] {
    // 255 heads, 63 sectors per track.
    let cyl = lba / (255 * 63);
    if cyl > 1023 {
        return [0xFE, 0xFF, 0xFF];
    }
    let rem = lba % (255 * 63);
    let head = rem / 63;
    let sec = rem % 63 + 1;
    [
        head as u8,
        (sec as u8 & 0x3F) | (((cyl >> 8) as u8 & 3) << 6),
        (cyl & 0xFF) as u8,
    ]
}

fn mbr_sector(sig: u32, entries: &[(u8, u64, u64)]) -> [u8; 512] {
    let mut m = [0u8; 512];
    m[0x1B8..0x1BC].copy_from_slice(&sig.to_le_bytes());
    for (i, &(t, start, count)) in entries.iter().take(4).enumerate() {
        let o = 0x1BE + 16 * i;
        m[o + 1..o + 4].copy_from_slice(&chs(start));
        m[o + 4] = t;
        m[o + 5..o + 8].copy_from_slice(&chs(start + count.saturating_sub(1)));
        m[o + 8..o + 12].copy_from_slice(&(start.min(u32::MAX as u64) as u32).to_le_bytes());
        m[o + 12..o + 16].copy_from_slice(&(count.min(u32::MAX as u64) as u32).to_le_bytes());
    }
    m[510] = 0x55;
    m[511] = 0xAA;
    m
}

fn crc32(data: &[u8]) -> u32 {
    crc32fast::hash(data)
}

/// Copy `src` into `dst` at `at`, skipping all-zero blocks so the sparse
/// destination stays sparse.
fn copy_sparse(src: &Path, dst: &mut File, at: u64) -> Result<u64> {
    let mut s = File::open(src)?;
    let mut buf = vec![0u8; 1 << 20];
    let mut pos = 0u64;
    loop {
        let mut filled = 0;
        while filled < buf.len() {
            let n = s.read(&mut buf[filled..])?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        if filled == 0 {
            break;
        }
        if buf[..filled].iter().any(|&b| b != 0) {
            dst.seek(SeekFrom::Start(at + pos))?;
            dst.write_all(&buf[..filled])?;
        }
        pos += filled as u64;
        if filled < buf.len() {
            break;
        }
    }
    Ok(pos)
}

/// Build the disk image at `out`.
pub fn build(spec: &DiskSpec, out: &Path) -> Result<DiskReport> {
    let ss = spec.sector as u64;
    if spec.sector != 512 && spec.sector != 4096 {
        return Err(FixtureError::invalid("sector size must be 512 or 4096"));
    }
    if spec.parts.is_empty() {
        return Err(FixtureError::invalid("a disk needs at least one partition"));
    }
    match spec.scheme {
        Scheme::None if spec.parts.len() != 1 => {
            return Err(FixtureError::invalid(
                "Scheme::None holds exactly one image",
            ))
        }
        Scheme::Mbr if spec.parts.len() > 4 => {
            return Err(FixtureError::invalid(
                "MBR holds at most four primary partitions",
            ))
        }
        Scheme::Gpt if spec.parts.len() > 128 => {
            return Err(FixtureError::invalid(
                "GPT holds at most 128 partitions here",
            ))
        }
        _ => {}
    }

    let entry_sectors = (128 * 128u64).div_ceil(ss);
    let first_usable_lba = match spec.scheme {
        Scheme::Gpt => 2 + entry_sectors,
        Scheme::Mbr => 1,
        Scheme::None => 0,
    };
    let mut rng = Rng::new(spec.seed ^ 0xD15C_0000);

    // Lay out partitions.
    struct Placed {
        start: u64,
        size: u64,
        ty: PartType,
        name: String,
    }
    let mut placed: Vec<Placed> = Vec::new();
    let mut cursor = match spec.scheme {
        Scheme::None => 0,
        _ => ALIGN.max(first_usable_lba * ss),
    };
    for (i, p) in spec.parts.iter().enumerate() {
        let len = std::fs::metadata(&p.fs_image)?.len();
        let size = align_up(len.max(1), ss);
        let start = match (spec.scheme, p.start) {
            (Scheme::None, Some(0) | None) => 0,
            (Scheme::None, Some(_)) => {
                return Err(FixtureError::invalid("Scheme::None starts at offset 0"))
            }
            (_, Some(s)) => s,
            (_, None) => align_up(cursor, ALIGN),
        };
        if start % ss != 0 {
            return Err(FixtureError::invalid(
                "partition start must be sector-aligned",
            ));
        }
        if start / ss < first_usable_lba {
            return Err(FixtureError::invalid(
                "partition overlaps the partition table",
            ));
        }
        if let Some(prev) = placed.last() {
            if start < prev.start + prev.size {
                return Err(FixtureError::invalid(
                    "partitions overlap or are out of order",
                ));
            }
        }
        cursor = start + size;
        let ty = match p.type_hint {
            Some(t) => t,
            None => default_type(detect(&p.fs_image)?, spec.scheme),
        };
        match (spec.scheme, ty) {
            (Scheme::Gpt, PartType::Mbr(_)) | (Scheme::Mbr, PartType::Gpt(_)) => {
                return Err(FixtureError::invalid(
                    "partition type does not match the scheme",
                ))
            }
            _ => {}
        }
        placed.push(Placed {
            start,
            size,
            ty,
            name: p.name.clone().unwrap_or_else(|| format!("part{}", i + 1)),
        });
    }

    let tail = match spec.scheme {
        Scheme::Gpt => (entry_sectors + 1) * ss,
        _ => 0,
    };
    let mut total = align_up(cursor + tail, ss);
    if spec.scheme != Scheme::None {
        total = align_up(total, ALIGN.min(ss * 2048));
    }
    if let Some(t) = spec.total_size {
        total = total.max(align_up(t, ss));
    }
    let total_sectors = total / ss;

    let mut f = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(out)?;
    f.set_len(total)?;
    for (p, pl) in spec.parts.iter().zip(&placed) {
        copy_sparse(&p.fs_image, &mut f, pl.start)?;
    }

    let mut report = DiskReport {
        partitions: placed.iter().map(|p| (p.start, p.size)).collect(),
        total_size: total,
        sector: spec.sector,
        mbr_signature: 0,
        disk_guid: [0; 16],
        partition_guids: Vec::new(),
    };

    match spec.scheme {
        Scheme::None => {}
        Scheme::Mbr => {
            let sig = rng.next_u32() | 1;
            report.mbr_signature = sig;
            let entries: Vec<(u8, u64, u64)> = placed
                .iter()
                .map(|p| {
                    let t = match p.ty {
                        PartType::Mbr(t) => t,
                        PartType::Gpt(_) => 0x83,
                    };
                    (t, p.start / ss, p.size / ss)
                })
                .collect();
            f.seek(SeekFrom::Start(0))?;
            f.write_all(&mbr_sector(sig, &entries))?;
        }
        Scheme::Gpt => {
            let disk_guid = random_guid(&mut rng);
            let mut array = vec![0u8; 128 * 128];
            for (i, p) in placed.iter().enumerate() {
                let o = i * 128;
                let tg = match p.ty {
                    PartType::Gpt(g) => g,
                    PartType::Mbr(_) => GUID_BASIC_DATA,
                };
                let ug = random_guid(&mut rng);
                array[o..o + 16].copy_from_slice(&guid_to_disk(tg));
                array[o + 16..o + 32].copy_from_slice(&guid_to_disk(ug));
                array[o + 32..o + 40].copy_from_slice(&(p.start / ss).to_le_bytes());
                array[o + 40..o + 48].copy_from_slice(&((p.start + p.size) / ss - 1).to_le_bytes());
                let name: Vec<u8> = p
                    .name
                    .encode_utf16()
                    .take(36)
                    .flat_map(|u| u.to_le_bytes())
                    .collect();
                array[o + 56..o + 56 + name.len()].copy_from_slice(&name);
                report.partition_guids.push(guid_to_disk(ug));
            }
            report.disk_guid = guid_to_disk(disk_guid);
            let array_crc = crc32(&array);
            let first_usable = 2 + entry_sectors;
            let last_usable = total_sectors - entry_sectors - 2;
            let header = |my: u64, alt: u64, entries_lba: u64| -> Vec<u8> {
                let mut h = vec![0u8; 92];
                h[0..8].copy_from_slice(b"EFI PART");
                h[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
                h[12..16].copy_from_slice(&92u32.to_le_bytes());
                h[24..32].copy_from_slice(&my.to_le_bytes());
                h[32..40].copy_from_slice(&alt.to_le_bytes());
                h[40..48].copy_from_slice(&first_usable.to_le_bytes());
                h[48..56].copy_from_slice(&last_usable.to_le_bytes());
                h[56..72].copy_from_slice(&report.disk_guid);
                h[72..80].copy_from_slice(&entries_lba.to_le_bytes());
                h[80..84].copy_from_slice(&128u32.to_le_bytes());
                h[84..88].copy_from_slice(&128u32.to_le_bytes());
                h[88..92].copy_from_slice(&array_crc.to_le_bytes());
                let crc = crc32(&h);
                h[16..20].copy_from_slice(&crc.to_le_bytes());
                h.resize(ss as usize, 0);
                h
            };
            let protective = mbr_sector(0, &[(0xEE, 1, (total_sectors - 1).min(u32::MAX as u64))]);
            let mut prot = vec![0u8; ss as usize];
            prot[..512].copy_from_slice(&protective);
            // The protective entry's start CHS is conventionally 00 02 00.
            prot[0x1BE + 1..0x1BE + 4].copy_from_slice(&[0x00, 0x02, 0x00]);
            f.seek(SeekFrom::Start(0))?;
            f.write_all(&prot)?;
            f.seek(SeekFrom::Start(ss))?;
            f.write_all(&header(1, total_sectors - 1, 2))?;
            f.seek(SeekFrom::Start(2 * ss))?;
            f.write_all(&array)?;
            let backup_entries = total_sectors - entry_sectors - 1;
            f.seek(SeekFrom::Start(backup_entries * ss))?;
            f.write_all(&array)?;
            f.seek(SeekFrom::Start((total_sectors - 1) * ss))?;
            f.write_all(&header(total_sectors - 1, 1, backup_entries))?;
        }
    }
    f.flush()?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guid_helpers() {
        let g = guid_from_str("EBD0A0A2-B9E5-4433-87C0-68B6B72699C7").unwrap();
        assert_eq!(g, GUID_BASIC_DATA);
        let d = guid_to_disk(g);
        assert_eq!(&d[..4], &[0xA2, 0xA0, 0xD0, 0xEB]);
        assert_eq!(&d[4..8], &[0xE5, 0xB9, 0x33, 0x44]);
        assert_eq!(&d[8..], &g[8..]);
        assert_eq!(guid_from_str("nope"), None);
        assert_eq!(
            guid_from_str("0FC63DAF-8483-4772-8E79-3D69D8477DE4").unwrap(),
            GUID_LINUX_FS
        );
    }

    #[test]
    fn chs_encoding() {
        assert_eq!(chs(0), [0, 1, 0]);
        assert_eq!(chs(2048), [32, 33, 0]);
        assert_eq!(chs(255 * 63 * 2000), [0xFE, 0xFF, 0xFF]);
        // Cylinder 300: low byte 44, high bits in the sector byte.
        let c = chs(300 * 255 * 63 + 5);
        assert_eq!(c[2], (300 & 0xFF) as u8);
        assert_eq!(c[1] >> 6, 1);
    }

    #[test]
    fn rejects_bad_specs() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("x.img");
        std::fs::write(&img, vec![1u8; 4096]).unwrap();
        let out = dir.path().join("d.img");
        assert!(build(&DiskSpec::new(Scheme::Mbr, vec![]), &out).is_err());
        let five = vec![PartSpec::new(&img); 5];
        assert!(build(&DiskSpec::new(Scheme::Mbr, five), &out).is_err());
        let two = vec![PartSpec::new(&img); 2];
        assert!(build(&DiskSpec::new(Scheme::None, two), &out).is_err());
        let mut s = DiskSpec::new(Scheme::Mbr, vec![PartSpec::new(&img)]);
        s.sector = 1000;
        assert!(build(&s, &out).is_err());
        let mut bad = PartSpec::new(&img);
        bad.start = Some(100);
        assert!(build(&DiskSpec::new(Scheme::Mbr, vec![bad]), &out).is_err());
        let mut mism = PartSpec::new(&img);
        mism.type_hint = Some(PartType::Gpt(GUID_LINUX_FS));
        assert!(build(&DiskSpec::new(Scheme::Mbr, vec![mism]), &out).is_err());
    }
}
