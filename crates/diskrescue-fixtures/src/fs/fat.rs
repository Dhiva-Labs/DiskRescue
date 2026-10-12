//! FAT12/16/32 builder.
//!
//! 1. The empty volume comes from `mkfs.vfat --invariant -F n -S 512 -s spc`
//!    or, when the tool is missing or `FsSpec::native_format` is set, from
//!    [`format_native`], a formatter that follows the Microsoft FAT
//!    specification (BPB, FAT[0]/FAT[1], FSInfo and backup boot sector for
//!    FAT32, FAT sizing by iteration, type by cluster count).
//! 2. The boot sector is parsed back into a [`Geo`], so population works the
//!    same for both formatters.
//! 3. Files and directories are allocated first-fit and contiguous (unless a
//!    [`Fragment`] is requested) in plan order; data is written in place;
//!    directories are rebuilt from scratch (volume label, `.`/`..`, short and
//!    long entries with checksums, DOS timestamps).
//! 4. Deletions mark the short entry and every LFN entry of the item with
//!    `0xE5` and zero its FAT chain, leaving data and directory slots where
//!    they were. With `windows_delete` the high word of the first cluster is
//!    zeroed too, as Windows does on FAT32.
//!
//! Limits: 512-byte sectors only, two FAT copies, no bad clusters, one
//! volume label, UTC DOS timestamps (years 1980..=2107).

use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::error::{FixtureError, Result};
use crate::fs::plan::{civil, Child, Plan};
use crate::fs::{create_sparse, BuildReport, Fragment, FsSpec, FsType, PlacedFile};
use crate::tools;

const SECTOR: u64 = 512;
const VOLUME_ID: u32 = 0x4452_4631; // "DRF1"

/// Parsed BIOS parameter block.
#[derive(Debug, Clone)]
pub(crate) struct Geo {
    pub kind: FsType,
    pub bps: u64,
    pub spc: u64,
    pub rsvd: u64,
    pub nfats: u64,
    pub fat_sectors: u64,
    pub root_entries: u64,
    pub root_sectors: u64,
    pub clusters: u32,
    pub root_cluster: u32,
    pub fsinfo: Option<u64>,
}

impl Geo {
    fn cluster_bytes(&self) -> u64 {
        self.bps * self.spc
    }
    fn fat_off(&self, copy: u64) -> u64 {
        (self.rsvd + copy * self.fat_sectors) * self.bps
    }
    fn root_off(&self) -> u64 {
        (self.rsvd + self.nfats * self.fat_sectors) * self.bps
    }
    fn data_off(&self) -> u64 {
        (self.rsvd + self.nfats * self.fat_sectors + self.root_sectors) * self.bps
    }
    fn cluster_off(&self, c: u32) -> u64 {
        self.data_off() + (c as u64 - 2) * self.cluster_bytes()
    }
    fn max_cluster(&self) -> u32 {
        self.clusters + 1
    }
    fn eoc(&self) -> u32 {
        match self.kind {
            FsType::Fat12 => 0xFFF,
            FsType::Fat16 => 0xFFFF,
            _ => 0x0FFF_FFFF,
        }
    }
}

fn le16(b: &[u8], o: usize) -> u64 {
    u16::from_le_bytes([b[o], b[o + 1]]) as u64
}
fn le32(b: &[u8], o: usize) -> u64 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) as u64
}

/// Parse and validate the boot sector.
pub(crate) fn parse_geo(b: &[u8]) -> Result<Geo> {
    if b.len() < 512 || b[510] != 0x55 || b[511] != 0xAA {
        return Err(FixtureError::corrupt("FAT boot sector", "missing 0x55AA"));
    }
    let bps = le16(b, 11);
    let spc = b[13] as u64;
    let rsvd = le16(b, 14);
    let nfats = b[16] as u64;
    let root_entries = le16(b, 17);
    let tot16 = le16(b, 19);
    let fatsz16 = le16(b, 22);
    let tot32 = le32(b, 32);
    let fatsz32 = le32(b, 36);
    if !(512..=4096).contains(&bps) || !bps.is_power_of_two() {
        return Err(FixtureError::corrupt(
            "FAT boot sector",
            format!("bytes/sector {bps}"),
        ));
    }
    if spc == 0 || !spc.is_power_of_two() || nfats == 0 || rsvd == 0 {
        return Err(FixtureError::corrupt(
            "FAT boot sector",
            "bad cluster/FAT geometry",
        ));
    }
    let fat_sectors = if fatsz16 != 0 { fatsz16 } else { fatsz32 };
    let total = if tot16 != 0 { tot16 } else { tot32 };
    let root_sectors = (root_entries * 32).div_ceil(bps);
    let meta = rsvd + nfats * fat_sectors + root_sectors;
    if fat_sectors == 0 || total <= meta {
        return Err(FixtureError::corrupt(
            "FAT boot sector",
            "sizes inconsistent",
        ));
    }
    let clusters = (total - meta) / spc;
    let kind = if clusters < 4085 {
        FsType::Fat12
    } else if clusters < 65525 {
        FsType::Fat16
    } else {
        FsType::Fat32
    };
    if clusters > 0x0FFF_FFF0 {
        return Err(FixtureError::corrupt(
            "FAT boot sector",
            "too many clusters",
        ));
    }
    let (root_cluster, fsinfo) = if kind == FsType::Fat32 {
        let fsi = le16(b, 48);
        (
            le32(b, 44) as u32,
            (fsi != 0 && fsi != 0xFFFF).then_some(fsi),
        )
    } else {
        (0, None)
    };
    Ok(Geo {
        kind,
        bps,
        spc,
        rsvd,
        nfats,
        fat_sectors,
        root_entries,
        root_sectors,
        clusters: clusters as u32,
        root_cluster,
        fsinfo,
    })
}

// ---------------------------------------------------------------- native formatter

/// Layout numbers for a given geometry choice.
#[derive(Debug, Clone, Copy)]
struct Layout {
    rsvd: u64,
    fat_sectors: u64,
    root_entries: u64,
    clusters: u64,
}

fn fat_type_for(clusters: u64) -> FsType {
    if clusters < 4085 {
        FsType::Fat12
    } else if clusters < 65525 {
        FsType::Fat16
    } else {
        FsType::Fat32
    }
}

/// Compute the layout for `kind` on `total` sectors with `spc` sectors per
/// cluster and two FATs; `None` when the volume cannot be that type.
fn layout(kind: FsType, total: u64, spc: u64) -> Option<Layout> {
    let nfats = 2;
    let (rsvd, root_entries) = match kind {
        FsType::Fat32 => (32, 0),
        _ => (1, 512),
    };
    let root_sectors = (root_entries * 32u64).div_ceil(SECTOR);
    if total <= rsvd + root_sectors + 2 * nfats {
        return None;
    }
    let mut fat_sectors = 1u64;
    for _ in 0..64 {
        let meta = rsvd + nfats * fat_sectors + root_sectors;
        if total <= meta {
            return None;
        }
        let clusters = (total - meta) / spc;
        if clusters > super::MAX_CLUSTERS {
            return None;
        }
        let bytes = match kind {
            FsType::Fat12 => ((clusters + 2) * 3).div_ceil(2),
            FsType::Fat16 => (clusters + 2) * 2,
            _ => (clusters + 2) * 4,
        };
        let need = bytes.div_ceil(SECTOR);
        if need <= fat_sectors {
            // Align the data area to the cluster size by padding the
            // reserved area (what mkfs.fat does too).
            let pad = (spc - meta % spc) % spc;
            let rsvd = rsvd + pad;
            if total <= meta + pad {
                return None;
            }
            let clusters = (total - meta - pad) / spc;
            if fat_type_for(clusters) != kind {
                return None;
            }
            return Some(Layout {
                rsvd,
                fat_sectors,
                root_entries,
                clusters,
            });
        }
        fat_sectors = need;
    }
    None
}

/// Sectors per cluster used when the spec does not say.
fn default_spc(kind: FsType, total: u64) -> Result<u64> {
    let candidates: Vec<u64> = (0..8).map(|i| 1u64 << i).collect();
    let fits = |spc: u64| -> bool {
        layout(kind, total, spc).is_some_and(|l| match kind {
            FsType::Fat12 => l.clusters + 16 < 4085,
            FsType::Fat16 => (4085 + 16..65525 - 16).contains(&l.clusters),
            _ => l.clusters >= 65525 + 16,
        })
    };
    let pick = if kind == FsType::Fat32 {
        // Largest cluster that still yields a legal FAT32 volume.
        candidates.iter().rev().copied().find(|&s| fits(s))
    } else {
        candidates.iter().copied().find(|&s| fits(s))
    };
    pick.ok_or_else(|| {
        FixtureError::invalid(format!(
            "{} bytes cannot hold a {:?} volume with any cluster size",
            total * SECTOR,
            kind
        ))
    })
}

const BOOT_CODE: [u8; 29] = [
    0x0e, 0x1f, 0xbe, 0x00, 0x7c, 0xac, 0x22, 0xc0, 0x74, 0x0b, 0x56, 0xb4, 0x0e, 0xbb, 0x07, 0x00,
    0xcd, 0x10, 0x5e, 0xeb, 0xf0, 0x32, 0xe4, 0xcd, 0x16, 0xcd, 0x19, 0xeb, 0xfe,
];
const BOOT_MSG: &[u8] = b"\r\nThis is not a bootable disk.\r\n";

fn put16(b: &mut [u8], o: usize, v: u16) {
    b[o..o + 2].copy_from_slice(&v.to_le_bytes());
}
fn put32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

fn label_field(label: Option<&str>) -> [u8; 11] {
    let mut out = [b' '; 11];
    match label {
        Some(l) => {
            for (i, c) in l.to_ascii_uppercase().bytes().take(11).enumerate() {
                out[i] = c;
            }
        }
        None => out.copy_from_slice(b"NO NAME    "),
    }
    out
}

/// Write an empty, specification-conformant FAT volume.
pub(crate) fn format_native(
    f: &mut File,
    kind: FsType,
    total: u64,
    spc: u64,
    label: Option<&str>,
) -> Result<()> {
    let l = layout(kind, total, spc).ok_or_else(|| {
        FixtureError::invalid(format!(
            "image of {total} sectors cannot be {kind:?} with {spc} sectors per cluster"
        ))
    })?;
    let mut bs = [0u8; 512];
    bs[0..3].copy_from_slice(&[0xEB, 0x58, 0x90]);
    if kind != FsType::Fat32 {
        bs[1] = 0x3C;
    }
    bs[3..11].copy_from_slice(b"MSWIN4.1");
    put16(&mut bs, 11, 512);
    bs[13] = spc as u8;
    put16(&mut bs, 14, l.rsvd as u16);
    bs[16] = 2;
    put16(&mut bs, 17, l.root_entries as u16);
    if total < 65536 && kind != FsType::Fat32 {
        put16(&mut bs, 19, total as u16);
    }
    bs[21] = 0xF8;
    if kind != FsType::Fat32 {
        put16(&mut bs, 22, l.fat_sectors as u16);
    }
    put16(&mut bs, 24, 63);
    put16(&mut bs, 26, 255);
    put32(&mut bs, 28, 0);
    if total >= 65536 || kind == FsType::Fat32 {
        put32(&mut bs, 32, total as u32);
    }
    let lab = label_field(label);
    let code_at;
    if kind == FsType::Fat32 {
        put32(&mut bs, 36, l.fat_sectors as u32);
        put16(&mut bs, 40, 0);
        put16(&mut bs, 42, 0);
        put32(&mut bs, 44, 2);
        put16(&mut bs, 48, 1);
        put16(&mut bs, 50, 6);
        bs[64] = 0x80;
        bs[66] = 0x29;
        put32(&mut bs, 67, VOLUME_ID);
        bs[71..82].copy_from_slice(&lab);
        bs[82..90].copy_from_slice(b"FAT32   ");
        code_at = 90;
    } else {
        bs[36] = 0x80;
        bs[38] = 0x29;
        put32(&mut bs, 39, VOLUME_ID);
        bs[43..54].copy_from_slice(&lab);
        let t: &[u8; 8] = if kind == FsType::Fat12 {
            b"FAT12   "
        } else {
            b"FAT16   "
        };
        bs[54..62].copy_from_slice(t);
        code_at = 62;
    }
    bs[code_at..code_at + BOOT_CODE.len()].copy_from_slice(&BOOT_CODE);
    let msg_at = code_at + BOOT_CODE.len();
    put16(&mut bs, code_at + 3, 0x7C00 + msg_at as u16);
    bs[msg_at..msg_at + BOOT_MSG.len()].copy_from_slice(BOOT_MSG);
    bs[510] = 0x55;
    bs[511] = 0xAA;

    write_at(f, 0, &bs)?;
    if kind == FsType::Fat32 {
        let mut fsi = [0u8; 512];
        put32(&mut fsi, 0, 0x4161_5252);
        put32(&mut fsi, 484, 0x6141_7272);
        put32(&mut fsi, 488, (l.clusters - 1) as u32);
        put32(&mut fsi, 492, 3);
        // The trailing signature of the FSInfo sector is 0xAA550000 at 508.
        fsi[508..512].copy_from_slice(&0xAA55_0000u32.to_le_bytes());
        write_at(f, SECTOR, &fsi)?;
        write_at(f, 6 * SECTOR, &bs)?;
        write_at(f, 7 * SECTOR, &fsi)?;
        // Sector 2 of each boot area carries only the signature.
        let mut sig = [0u8; 512];
        sig[510] = 0x55;
        sig[511] = 0xAA;
        write_at(f, 2 * SECTOR, &sig)?;
        write_at(f, 8 * SECTOR, &sig)?;
    }
    // FATs.
    let geo = parse_geo(&bs)?;
    let mut fat = vec![0u32; l.clusters as usize + 2];
    fat[0] = match kind {
        FsType::Fat12 => 0xFF8,
        FsType::Fat16 => 0xFFF8,
        _ => 0x0FFF_FFF8,
    };
    fat[1] = geo.eoc();
    if kind == FsType::Fat32 {
        fat[2] = geo.eoc(); // root directory cluster
    }
    write_fats(f, &geo, &fat)?;
    // Zero the root directory (sparse files are already zero, but a reused
    // file may not be).
    let root_len = if kind == FsType::Fat32 {
        geo.cluster_bytes()
    } else {
        geo.root_sectors * SECTOR
    };
    let root_at = if kind == FsType::Fat32 {
        geo.cluster_off(2)
    } else {
        geo.root_off()
    };
    write_at(f, root_at, &vec![0u8; root_len as usize])?;
    Ok(())
}

// ---------------------------------------------------------------- FAT codec

fn decode_fat(bytes: &[u8], kind: FsType, entries: usize) -> Vec<u32> {
    let mut out = Vec::with_capacity(entries);
    for n in 0..entries {
        let v = match kind {
            FsType::Fat12 => {
                let o = n * 3 / 2;
                let pair = bytes.get(o).copied().unwrap_or(0) as u32
                    | (bytes.get(o + 1).copied().unwrap_or(0) as u32) << 8;
                if n & 1 == 1 {
                    pair >> 4
                } else {
                    pair & 0xFFF
                }
            }
            FsType::Fat16 => {
                let o = n * 2;
                bytes.get(o).copied().unwrap_or(0) as u32
                    | (bytes.get(o + 1).copied().unwrap_or(0) as u32) << 8
            }
            _ => {
                let o = n * 4;
                let mut v = 0u32;
                for i in 0..4 {
                    v |= (bytes.get(o + i).copied().unwrap_or(0) as u32) << (8 * i);
                }
                v & 0x0FFF_FFFF
            }
        };
        out.push(v);
    }
    out
}

fn encode_fat(fat: &[u32], kind: FsType, len: usize) -> Vec<u8> {
    let mut b = vec![0u8; len];
    for (n, &v) in fat.iter().enumerate() {
        match kind {
            FsType::Fat12 => {
                let o = n * 3 / 2;
                if o + 1 >= b.len() {
                    continue;
                }
                if n & 1 == 0 {
                    b[o] = (v & 0xFF) as u8;
                    b[o + 1] = (b[o + 1] & 0xF0) | ((v >> 8) & 0x0F) as u8;
                } else {
                    b[o] = (b[o] & 0x0F) | ((v & 0x0F) << 4) as u8;
                    b[o + 1] = ((v >> 4) & 0xFF) as u8;
                }
            }
            FsType::Fat16 => {
                let o = n * 2;
                if o + 2 <= b.len() {
                    b[o..o + 2].copy_from_slice(&(v as u16).to_le_bytes());
                }
            }
            _ => {
                let o = n * 4;
                if o + 4 <= b.len() {
                    b[o..o + 4].copy_from_slice(&(v & 0x0FFF_FFFF).to_le_bytes());
                }
            }
        }
    }
    b
}

fn write_at(f: &mut File, off: u64, data: &[u8]) -> Result<()> {
    f.seek(SeekFrom::Start(off))?;
    f.write_all(data)?;
    Ok(())
}

fn read_at(f: &mut File, off: u64, len: usize) -> Result<Vec<u8>> {
    f.seek(SeekFrom::Start(off))?;
    let mut v = vec![0u8; len];
    f.read_exact(&mut v)?;
    Ok(v)
}

fn write_fats(f: &mut File, g: &Geo, fat: &[u32]) -> Result<()> {
    let bytes = encode_fat(fat, g.kind, (g.fat_sectors * g.bps) as usize);
    for copy in 0..g.nfats {
        write_at(f, g.fat_off(copy), &bytes)?;
    }
    Ok(())
}

// ---------------------------------------------------------------- names and entries

const SHORT_OK: &[u8] = b"$%'-_@~!(){}^#&";

fn short_char_ok(c: char) -> bool {
    c.is_ascii_alphanumeric() || (c.is_ascii() && SHORT_OK.contains(&(c as u8)))
}

/// A directory entry name pair.
#[derive(Debug, Clone)]
pub(crate) struct ShortName {
    pub raw: [u8; 11],
    /// `DIR_NTRes` case bits (0x08 lower-case base, 0x10 lower-case ext).
    pub nt_case: u8,
    /// True when long-name entries are required.
    pub lfn: bool,
}

/// Try to represent `name` as a plain 8.3 name.
fn direct_short(name: &str) -> Option<ShortName> {
    let (base, ext) = match name.rfind('.') {
        Some(i) => (&name[..i], &name[i + 1..]),
        None => (name, ""),
    };
    if base.is_empty() || base.len() > 8 || ext.len() > 3 || base.contains('.') {
        return None;
    }
    if !base.chars().chain(ext.chars()).all(short_char_ok) {
        return None;
    }
    let case_of = |s: &str| -> Option<bool> {
        let has_lower = s.chars().any(|c| c.is_ascii_lowercase());
        let has_upper = s.chars().any(|c| c.is_ascii_uppercase());
        if has_lower && has_upper {
            None
        } else {
            Some(has_lower)
        }
    };
    let bl = case_of(base)?;
    let el = case_of(ext)?;
    let mut raw = [b' '; 11];
    for (i, c) in base.to_ascii_uppercase().bytes().enumerate() {
        raw[i] = c;
    }
    for (i, c) in ext.to_ascii_uppercase().bytes().enumerate() {
        raw[8 + i] = c;
    }
    // 0xE5 as a first byte is stored as 0x05.
    if raw[0] == 0xE5 {
        raw[0] = 0x05;
    }
    Some(ShortName {
        raw,
        nt_case: (if bl { 0x08 } else { 0 }) | (if el { 0x10 } else { 0 }),
        lfn: false,
    })
}

/// Generate a short alias (`NAME~1.EXT`) unique in `used`.
fn alias_short(name: &str, used: &mut HashSet<[u8; 11]>) -> ShortName {
    let trimmed = name.trim_start_matches(['.', ' ']);
    let (base_s, ext_s) = match trimmed.rfind('.') {
        Some(i) => (&trimmed[..i], &trimmed[i + 1..]),
        None => (trimmed, ""),
    };
    let conv = |s: &str, max: usize| -> (Vec<u8>, bool) {
        let mut out = Vec::new();
        let mut lossy = false;
        for c in s.chars() {
            if c == ' ' || c == '.' {
                lossy = true;
                continue;
            }
            let u = c.to_ascii_uppercase();
            if short_char_ok(u) {
                out.push(u as u8);
            } else {
                out.push(b'_');
                lossy = true;
            }
        }
        if out.len() > max {
            out.truncate(max);
            lossy = true;
        }
        (out, lossy)
    };
    let (base, lb) = conv(base_s, 8);
    let (ext, le) = conv(ext_s, 3);
    let base = if base.is_empty() { b"_".to_vec() } else { base };
    let make = |b: &[u8]| -> [u8; 11] {
        let mut raw = [b' '; 11];
        raw[..b.len()].copy_from_slice(b);
        raw[8..8 + ext.len()].copy_from_slice(&ext);
        if raw[0] == 0xE5 {
            raw[0] = 0x05;
        }
        raw
    };
    if !lb && !le {
        let raw = make(&base);
        if used.insert(raw) {
            return ShortName {
                raw,
                nt_case: 0,
                lfn: true,
            };
        }
    }
    for n in 1u32.. {
        let tail = format!("~{n}");
        let keep = 8usize.saturating_sub(tail.len()).min(base.len());
        let mut b = base[..keep].to_vec();
        b.extend_from_slice(tail.as_bytes());
        let raw = make(&b);
        if used.insert(raw) {
            return ShortName {
                raw,
                nt_case: 0,
                lfn: true,
            };
        }
    }
    unreachable!("the numeric tail space is effectively unbounded")
}

fn lfn_checksum(short: &[u8; 11]) -> u8 {
    let mut s = 0u8;
    for &b in short {
        s = (((s & 1) << 7) | (s >> 1)).wrapping_add(b);
    }
    s
}

fn lfn_units(name: &str) -> Result<Vec<u16>> {
    let units: Vec<u16> = name.encode_utf16().collect();
    if units.is_empty() || units.len() > 255 {
        return Err(FixtureError::invalid(format!(
            "name length out of range: {name:?}"
        )));
    }
    if name
        .chars()
        .any(|c| (c as u32) < 0x20 || "\"*/:<>?\\|".contains(c))
    {
        return Err(FixtureError::invalid(format!(
            "character not allowed in FAT names: {name:?}"
        )));
    }
    if name.ends_with(' ') || name.ends_with('.') {
        return Err(FixtureError::invalid(format!(
            "FAT names may not end in space or dot: {name:?}"
        )));
    }
    Ok(units)
}

fn lfn_count(units: usize) -> usize {
    units.div_ceil(13)
}

/// Build the LFN entries (highest sequence first, as stored on disk).
fn lfn_entries(units: &[u16], checksum: u8) -> Vec<[u8; 32]> {
    let n = lfn_count(units.len());
    let mut padded: Vec<u16> = units.to_vec();
    if padded.len() % 13 != 0 {
        padded.push(0);
        while padded.len() % 13 != 0 {
            padded.push(0xFFFF);
        }
    }
    let mut out = Vec::with_capacity(n);
    for idx in (0..n).rev() {
        let chunk = &padded[idx * 13..idx * 13 + 13];
        let mut e = [0u8; 32];
        e[0] = (idx as u8 + 1) | if idx == n - 1 { 0x40 } else { 0 };
        e[11] = 0x0F;
        e[13] = checksum;
        let pos = [1usize, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];
        for (k, &p) in pos.iter().enumerate() {
            e[p..p + 2].copy_from_slice(&chunk[k].to_le_bytes());
        }
        out.push(e);
    }
    out
}

/// DOS date, time and tenths-of-a-second for a Unix timestamp (UTC).
fn dos_stamp(secs: i64) -> (u16, u16, u8) {
    let (y, m, d, hh, mm, ss) = civil(secs);
    let y = y.clamp(1980, 2107);
    let date = (((y - 1980) as u16) << 9) | ((m as u16) << 5) | d as u16;
    let time = ((hh as u16) << 11) | ((mm as u16) << 5) | (ss as u16 / 2);
    (date, time, ((ss % 2) * 100) as u8)
}

fn short_entry(
    short: &ShortName,
    attr: u8,
    mtime: i64,
    first: u32,
    size: u32,
    high_word: bool,
) -> [u8; 32] {
    let (date, time, tenths) = dos_stamp(mtime);
    let mut e = [0u8; 32];
    e[0..11].copy_from_slice(&short.raw);
    e[11] = attr;
    e[12] = short.nt_case;
    e[13] = tenths;
    put16(&mut e, 14, time);
    put16(&mut e, 16, date);
    put16(&mut e, 18, date);
    put16(&mut e, 20, if high_word { (first >> 16) as u16 } else { 0 });
    put16(&mut e, 22, time);
    put16(&mut e, 24, date);
    put16(&mut e, 26, first as u16);
    put32(&mut e, 28, size);
    e
}

// ---------------------------------------------------------------- the builder

struct Vol {
    f: File,
    geo: Geo,
    fat: Vec<u32>,
}

impl Vol {
    fn is_free(&self, c: u32) -> bool {
        self.fat[c as usize] == 0
    }

    /// First-fit run of `n` free clusters; returns the first one.
    fn find_run(&self, n: u32) -> Option<u32> {
        let mut run = 0;
        for c in 2..=self.geo.max_cluster() {
            if self.is_free(c) {
                run += 1;
                if run == n {
                    return Some(c + 1 - n);
                }
            } else {
                run = 0;
            }
        }
        None
    }

    /// Allocate `n` contiguous clusters and chain them.
    fn alloc_run(&mut self, n: u32, what: &str) -> Result<Vec<u32>> {
        let first = self
            .find_run(n)
            .ok_or_else(|| FixtureError::Full(format!("no run of {n} free clusters for {what}")))?;
        let list: Vec<u32> = (first..first + n).collect();
        self.chain(&list);
        Ok(list)
    }

    fn chain(&mut self, list: &[u32]) {
        let eoc = self.geo.eoc();
        for (i, &c) in list.iter().enumerate() {
            self.fat[c as usize] = list.get(i + 1).copied().unwrap_or(eoc);
        }
    }

    /// Allocate `n` clusters in two runs with `gap` free clusters between.
    fn alloc_fragmented(&mut self, n: u32, frag: Fragment, what: &str) -> Result<Vec<u32>> {
        let first_n = frag.first_clusters.min(n);
        if first_n == 0 || first_n == n {
            return self.alloc_run(n, what);
        }
        let mut list = self.alloc_run(first_n, what)?;
        // Reserve the gap by temporarily marking it used.
        let gap_start = list[list.len() - 1] + 1;
        let mut reserved = Vec::new();
        let mut c = gap_start;
        while reserved.len() < frag.gap_clusters as usize && c <= self.geo.max_cluster() {
            if self.is_free(c) {
                self.fat[c as usize] = 0x0FFF_FFFF;
                reserved.push(c);
            }
            c += 1;
        }
        let second = self.alloc_run(n - first_n, what);
        for r in reserved {
            self.fat[r as usize] = 0;
        }
        list.extend(second?);
        self.chain(&list);
        Ok(list)
    }

    fn free_count(&self) -> u32 {
        (2..=self.geo.max_cluster())
            .filter(|&c| self.is_free(c))
            .count() as u32
    }
}

/// Convert a cluster list into volume byte ranges, trimmed to `size`.
fn extents_of(geo: &Geo, clusters: &[u32], size: u64) -> Vec<(u64, u64)> {
    let cb = geo.cluster_bytes();
    let mut out: Vec<(u64, u64)> = Vec::new();
    let mut left = size;
    for &c in clusters {
        if left == 0 {
            break;
        }
        let take = left.min(cb);
        let off = geo.cluster_off(c);
        match out.last_mut() {
            Some((o, l)) if *o + *l == off => *l += take,
            _ => out.push((off, take)),
        }
        left -= take;
    }
    out
}

pub(crate) fn build(spec: &FsSpec, out: &Path) -> Result<BuildReport> {
    let plan = Plan::new(spec, true)?;
    let total_sectors = spec.size / SECTOR;
    let spc = match spec.cluster_size {
        Some(cs) => {
            if cs < 512 || !cs.is_power_of_two() || cs > 65536 {
                return Err(FixtureError::invalid(format!(
                    "FAT cluster size must be a power of two in 512..=65536, got {cs}"
                )));
            }
            (cs / 512) as u64
        }
        None => default_spc(spec.kind, total_sectors)?,
    };
    if let Some(l) = &spec.label {
        if l.len() > 11 || !l.is_ascii() || l.contains(|c: char| (c as u32) < 0x20) {
            return Err(FixtureError::invalid(
                "FAT label must be <= 11 ASCII characters",
            ));
        }
    }

    let mut f = create_sparse(out, spec.size)?;
    let mut report = BuildReport::default();
    let use_tool = !spec.native_format && tools::have("mkfs.vfat");
    if use_tool {
        let bits = match spec.kind {
            FsType::Fat12 => "12",
            FsType::Fat16 => "16",
            _ => "32",
        };
        let s = spc.to_string();
        let path = out.to_string_lossy().into_owned();
        let mut args = vec!["--invariant", "-F", bits, "-S", "512", "-s", &s, "-f", "2"];
        let lab;
        if let Some(l) = &spec.label {
            lab = l.to_ascii_uppercase();
            args.push("-n");
            args.push(&lab);
        }
        args.push(&path);
        tools::run("mkfs.vfat", &args)?;
        report.tool = "mkfs.vfat+builtin".into();
    } else {
        format_native(&mut f, spec.kind, total_sectors, spc, spec.label.as_deref())?;
        report.tool = "builtin".into();
        report
            .notes
            .push("volume formatted by the built-in FAT formatter".into());
    }

    let boot = read_at(&mut f, 0, 512)?;
    let geo = parse_geo(&boot)?;
    if geo.kind != spec.kind {
        return Err(FixtureError::invalid(format!(
            "requested {:?} but the formatted volume has {} clusters ({:?}); \
             adjust size or cluster_size",
            spec.kind, geo.clusters, geo.kind
        )));
    }
    if geo.bps != SECTOR {
        return Err(FixtureError::Unsupported("only 512-byte sectors".into()));
    }
    if geo.clusters as u64 > super::MAX_CLUSTERS {
        return Err(FixtureError::invalid(
            "too many clusters for the fixture builder; use a larger cluster size",
        ));
    }
    if geo.fat_sectors * geo.bps > (geo.clusters as u64 + 2) * 4 + (1 << 20) {
        return Err(FixtureError::corrupt(
            "FAT boot sector",
            "FAT size is implausible",
        ));
    }
    let fat_bytes = read_at(&mut f, geo.fat_off(0), (geo.fat_sectors * geo.bps) as usize)?;
    let fat = decode_fat(&fat_bytes, geo.kind, geo.clusters as usize + 2);
    let mut vol = Vol { f, geo, fat };
    populate(spec, &plan, &mut vol, &mut report)?;
    report.deleted_supported = true;
    report.cluster_size = vol.geo.cluster_bytes() as u32;
    report.data_start = vol.geo.data_off();
    Ok(report)
}

/// Directory-entry bookkeeping for one directory.
struct DirLayout {
    clusters: Vec<u32>,
    short: Vec<(Child, ShortName)>,
}

fn entry_slots(name_units: usize, lfn: bool) -> usize {
    1 + if lfn { lfn_count(name_units) } else { 0 }
}

fn populate(spec: &FsSpec, plan: &Plan, vol: &mut Vol, report: &mut BuildReport) -> Result<()> {
    let geo = vol.geo.clone();
    let cb = geo.cluster_bytes();
    let fat32 = geo.kind == FsType::Fat32;

    // Short names per directory (direct names first so aliases cannot steal them).
    let mut layouts: Vec<DirLayout> = Vec::new();
    for d in &plan.dirs {
        let mut used: HashSet<[u8; 11]> = HashSet::new();
        let mut shorts: Vec<(Child, Option<ShortName>, String)> = Vec::new();
        for &c in &d.children {
            let name = match c {
                Child::Dir(i) => plan.dirs[i].name.clone(),
                Child::File(i) => plan.files[i].name.clone(),
            };
            lfn_units(&name)?;
            let sn = direct_short(&name);
            if let Some(s) = &sn {
                used.insert(s.raw);
            }
            shorts.push((c, sn, name));
        }
        let mut resolved = Vec::new();
        for (c, sn, name) in shorts {
            let s = match sn {
                Some(s) => s,
                None => alias_short(&name, &mut used),
            };
            resolved.push((c, s));
        }
        layouts.push(DirLayout {
            clusters: Vec::new(),
            short: resolved,
        });
    }

    let slot_counts: Vec<usize> = (0..plan.dirs.len())
        .map(|di| {
            let mut n = 0;
            for (c, s) in &layouts[di].short {
                let name = match c {
                    Child::Dir(i) => &plan.dirs[*i].name,
                    Child::File(i) => &plan.files[*i].name,
                };
                n += entry_slots(name.encode_utf16().count(), s.lfn);
            }
            if di != 0 {
                n += 2;
            } else if spec.label.is_some() {
                n += 1;
            }
            n + 1 // terminator / spare
        })
        .collect();
    let slots_of = |di: usize| slot_counts[di];

    // Root directory.
    if fat32 {
        let mut chain = vec![geo.root_cluster];
        let mut c = vol.fat[geo.root_cluster as usize];
        while (2..=geo.max_cluster()).contains(&c) && chain.len() < 1 << 20 {
            chain.push(c);
            c = vol.fat[c as usize];
        }
        let need = ((slots_of(0) as u64 * 32).div_ceil(cb)) as usize;
        while chain.len() < need {
            let extra = vol.alloc_run(1, "root directory growth")?;
            let last = *chain.last().unwrap_or(&geo.root_cluster);
            vol.fat[last as usize] = extra[0];
            chain.push(extra[0]);
        }
        layouts[0].clusters = chain;
    } else if slots_of(0) as u64 > geo.root_entries {
        return Err(FixtureError::Full(format!(
            "root directory holds {} entries, {} needed",
            geo.root_entries,
            slots_of(0)
        )));
    }

    // Allocation pass.
    let mut file_clusters: Vec<Vec<u32>> = vec![Vec::new(); plan.files.len()];
    for &item in &plan.order {
        match item {
            Child::Dir(i) => {
                let n = ((slots_of(i) as u64 * 32).div_ceil(cb)).max(1) as u32;
                layouts[i].clusters = vol.alloc_run(n, &plan.dirs[i].path)?;
            }
            Child::File(i) => {
                let fs = &spec.files[plan.files[i].spec];
                let n = (fs.data.len() as u64).div_ceil(cb) as u32;
                if n == 0 {
                    continue;
                }
                let list = match fs.fragment {
                    Some(fr) => vol.alloc_fragmented(n, fr, &plan.files[i].path)?,
                    None => vol.alloc_run(n, &plan.files[i].path)?,
                };
                for (k, chunk) in fs.data.chunks(cb as usize).enumerate() {
                    write_at(&mut vol.f, geo.cluster_off(list[k]), chunk)?;
                }
                file_clusters[i] = list;
            }
        }
    }

    // Serialise directories.
    for (di, d) in plan.dirs.iter().enumerate() {
        let cap = if di == 0 && !fat32 {
            (geo.root_sectors * geo.bps) as usize
        } else {
            layouts[di].clusters.len() * cb as usize
        };
        let mut buf = vec![0u8; cap];
        let mut pos = 0usize;
        let mut push = |e: &[u8; 32], buf: &mut Vec<u8>| -> Result<()> {
            if pos + 32 > buf.len() {
                return Err(FixtureError::Full("directory overflow".into()));
            }
            buf[pos..pos + 32].copy_from_slice(e);
            pos += 32;
            Ok(())
        };
        if di == 0 {
            if let Some(l) = &spec.label {
                let sn = ShortName {
                    raw: label_field(Some(l)),
                    nt_case: 0,
                    lfn: false,
                };
                let mut e = short_entry(&sn, 0x08, super::DEFAULT_MTIME, 0, 0, false);
                e[12] = 0;
                push(&e, &mut buf)?;
            }
        } else {
            let own = layouts[di].clusters[0];
            let parent = d.parent.unwrap_or(0);
            let parent_cluster = if parent == 0 {
                0
            } else {
                layouts[parent].clusters[0]
            };
            let mut dot = ShortName {
                raw: [b' '; 11],
                nt_case: 0,
                lfn: false,
            };
            dot.raw[0] = b'.';
            let mut dotdot = dot.clone();
            dotdot.raw[1] = b'.';
            push(&short_entry(&dot, 0x10, d.mtime, own, 0, true), &mut buf)?;
            push(
                &short_entry(&dotdot, 0x10, d.mtime, parent_cluster, 0, true),
                &mut buf,
            )?;
        }
        for (c, sn) in &layouts[di].short {
            let (name, attr, mtime, first, size, deleted) = match *c {
                Child::Dir(i) => (
                    &plan.dirs[i].name,
                    0x10u8,
                    plan.dirs[i].mtime,
                    layouts[i].clusters[0],
                    0u32,
                    plan.dirs[i].deleted,
                ),
                Child::File(i) => {
                    let n = &plan.files[i];
                    let sz = spec.files[n.spec].data.len();
                    if sz as u64 > u32::MAX as u64 {
                        return Err(FixtureError::invalid("FAT files are limited to 4 GiB - 1"));
                    }
                    (
                        &n.name,
                        0x20u8,
                        n.mtime,
                        file_clusters[i].first().copied().unwrap_or(0),
                        sz as u32,
                        n.deleted,
                    )
                }
            };
            let win_zero = deleted && spec.windows_delete && fat32;
            if sn.lfn {
                let units: Vec<u16> = name.encode_utf16().collect();
                for mut e in lfn_entries(&units, lfn_checksum(&sn.raw)) {
                    if deleted {
                        e[0] = 0xE5;
                    }
                    push(&e, &mut buf)?;
                }
            }
            let mut e = short_entry(sn, attr, mtime, first, size, fat32 && !win_zero);
            if deleted {
                e[0] = 0xE5;
            }
            push(&e, &mut buf)?;
        }
        // Write out.
        if di == 0 && !fat32 {
            write_at(&mut vol.f, geo.root_off(), &buf)?;
        } else {
            for (k, &c) in layouts[di].clusters.iter().enumerate() {
                let s = k * cb as usize;
                write_at(&mut vol.f, geo.cluster_off(c), &buf[s..s + cb as usize])?;
            }
        }
    }

    // Report (before deletion frees the chains).
    for (i, n) in plan.files.iter().enumerate() {
        let fs = &spec.files[n.spec];
        report.placed.push(PlacedFile {
            path: n.path.clone(),
            size: fs.data.len() as u64,
            extents: extents_of(&geo, &file_clusters[i], fs.data.len() as u64),
            deleted: n.deleted,
            mtime: n.mtime,
        });
    }

    // Deletion: zero the FAT chains.
    for (i, n) in plan.files.iter().enumerate() {
        if n.deleted {
            for &c in &file_clusters[i] {
                vol.fat[c as usize] = 0;
            }
        }
    }
    for (i, d) in plan.dirs.iter().enumerate() {
        if d.deleted && i != 0 {
            for &c in &layouts[i].clusters {
                vol.fat[c as usize] = 0;
            }
        }
    }
    write_fats(&mut vol.f, &geo, &vol.fat)?;

    // FSInfo.
    if let Some(sec) = geo.fsinfo {
        let free = vol.free_count();
        let next = (2..=geo.max_cluster())
            .rev()
            .find(|&c| !vol.is_free(c))
            .map(|c| c + 1)
            .unwrap_or(2)
            .min(geo.max_cluster());
        let mut fsi = read_at(&mut vol.f, sec * geo.bps, 512)?;
        put32(&mut fsi, 488, free);
        put32(&mut fsi, 492, next);
        write_at(&mut vol.f, sec * geo.bps, &fsi)?;
        // Keep the backup copy (sector 6 + fsinfo) in sync when present.
        if sec == 1 {
            write_at(&mut vol.f, 7 * geo.bps, &fsi)?;
        }
    }
    vol.f.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fat_codec_roundtrip() {
        for kind in [FsType::Fat12, FsType::Fat16, FsType::Fat32] {
            let fat: Vec<u32> = (0..100u32)
                .map(|i| match kind {
                    FsType::Fat12 => (i * 37) & 0xFFF,
                    FsType::Fat16 => (i * 997) & 0xFFFF,
                    _ => (i * 99_991) & 0x0FFF_FFFF,
                })
                .collect();
            let bytes = encode_fat(&fat, kind, 1024);
            assert_eq!(decode_fat(&bytes, kind, 100), fat, "{kind:?}");
        }
    }

    #[test]
    fn short_names() {
        let d = direct_short("IMG_0001.JPG").unwrap();
        assert_eq!(&d.raw, b"IMG_0001JPG");
        assert_eq!(d.nt_case, 0);
        let lower = direct_short("photo.jpg").unwrap();
        assert_eq!(&lower.raw, b"PHOTO   JPG");
        assert_eq!(lower.nt_case, 0x18);
        let mixed = direct_short("Photo.JPG");
        assert!(mixed.is_none());
        assert!(direct_short("a b.txt").is_none());
        assert!(direct_short("toolongname.txt").is_none());
        assert!(direct_short(".hidden").is_none());
        let mut used = HashSet::new();
        let a = alias_short("Holiday Photo 2024.jpg", &mut used);
        assert_eq!(&a.raw, b"HOLIDA~1JPG");
        assert!(a.lfn);
        let b = alias_short("Holiday Photo 2025.jpg", &mut used);
        assert_eq!(&b.raw, b"HOLIDA~2JPG");
        let c = alias_short("MixedCase.txt", &mut used);
        assert_eq!(&c.raw, b"MIXEDC~1TXT");
    }

    #[test]
    fn lfn_checksum_matches_reference() {
        // Reference value computed from the algorithm in the FAT spec for
        // "FOO     BAR".
        let mut s = 0u8;
        for &b in b"FOO     BAR" {
            s = (if s & 1 == 1 { 0x80u8 } else { 0 })
                .wrapping_add(s >> 1)
                .wrapping_add(b);
        }
        let mut raw = [0u8; 11];
        raw.copy_from_slice(b"FOO     BAR");
        assert_eq!(lfn_checksum(&raw), s);
    }

    #[test]
    fn lfn_entry_layout() {
        let units: Vec<u16> = "Holiday Photo 2024.jpg".encode_utf16().collect();
        let e = lfn_entries(&units, 0x5A);
        assert_eq!(e.len(), 2);
        assert_eq!(e[0][0], 0x42);
        assert_eq!(e[1][0], 0x01);
        assert!(e.iter().all(|x| x[11] == 0x0F && x[13] == 0x5A));
        // First entry holds the characters 14..22 then NUL then 0xFFFF padding.
        assert_eq!(u16::from_le_bytes([e[0][1], e[0][2]]), units[13]);
    }

    #[test]
    fn layout_picks_legal_types() {
        // 40 MiB FAT16, 64 MiB FAT32, 8 MiB FAT12.
        let spc = default_spc(FsType::Fat16, 40 * 2048).unwrap();
        assert_eq!(
            layout(FsType::Fat16, 40 * 2048, spc).unwrap().root_entries,
            512
        );
        let spc = default_spc(FsType::Fat32, 96 * 2048).unwrap();
        assert!(layout(FsType::Fat32, 96 * 2048, spc).unwrap().clusters >= 65525);
        let spc = default_spc(FsType::Fat12, 8 * 2048).unwrap();
        assert!(layout(FsType::Fat12, 8 * 2048, spc).unwrap().clusters < 4085);
        // Too small for FAT32 whatever the cluster size.
        assert!(default_spc(FsType::Fat32, 2048).is_err());
    }

    #[test]
    fn geo_parse_rejects_garbage() {
        assert!(parse_geo(&[0u8; 512]).is_err());
        assert!(parse_geo(&[1, 2, 3]).is_err());
        let mut b = [0xFFu8; 512];
        b[510] = 0x55;
        b[511] = 0xAA;
        assert!(parse_geo(&b).is_err());
    }

    #[test]
    fn dos_stamp_epoch_clamp() {
        let (d, t, _) = dos_stamp(0);
        assert_eq!(d >> 9, 0); // clamped to 1980
        assert_eq!(t, 0);
        let (d, t, tenths) = dos_stamp(super::super::DEFAULT_MTIME);
        assert_eq!(d >> 9, 2024 - 1980);
        assert_eq!((d >> 5) & 0xF, 5);
        assert_eq!(d & 0x1F, 6);
        assert_eq!(t >> 11, 7);
        assert_eq!((t >> 5) & 0x3F, 8);
        assert_eq!(t & 0x1F, 4);
        assert_eq!(tenths, 100);
    }
}
