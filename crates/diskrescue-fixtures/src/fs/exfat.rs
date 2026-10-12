//! exFAT builder.
//!
//! 1. `mkfs.exfat -s 512 [-c size] [-L label]` creates the empty volume
//!    (required: without it the builder returns `Missing`).
//! 2. The boot sector, FAT, allocation bitmap, up-case table and the system
//!    entries of the root directory are parsed back.
//! 3. Files are allocated first-fit and contiguous in the bitmap and get
//!    `NoFatChain` entry sets (File 0x85, Stream Extension 0xC0, File Name
//!    0xC1 entries) with the exFAT entry-set checksum and the name hash
//!    computed through the volume's own up-case table. Directories and
//!    fragmented files use FAT chains. The root directory grows by whole
//!    clusters when needed.
//! 4. Deletion clears the InUse bit (bit 7) of every entry of the set
//!    (`0x85 -> 0x05`, `0xC0 -> 0x40`, `0xC1 -> 0x41`), clears the bitmap
//!    bits and zeroes the FAT entries of chained files; the data stays.
//! 5. Afterwards the volume serial number is pinned to a fixed value and
//!    `PercentInUse` is recomputed; the boot checksum sector (11) is
//!    rewritten for the main and the backup boot region.
//!
//! Limits: one FAT, 512-byte sectors, no TexFAT, no volume GUID changes.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::error::{FixtureError, Result};
use crate::fs::plan::{civil, Child, Plan};
use crate::fs::{create_sparse, BuildReport, Fragment, FsSpec, PlacedFile};
use crate::tools;

const EOF: u32 = 0xFFFF_FFFF;
const SERIAL: u32 = 0x4452_4558; // "DREX"

struct Geo {
    bps: u64,
    spc: u64,
    fat_off: u64,
    fat_len: u64,
    heap_off: u64,
    clusters: u32,
    root_cluster: u32,
    volume_len: u64,
}

impl Geo {
    fn cb(&self) -> u64 {
        self.bps * self.spc
    }
    fn cluster_off(&self, c: u32) -> u64 {
        (self.heap_off + (c as u64 - 2) * self.spc) * self.bps
    }
}

fn rd32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn rd64(b: &[u8], o: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(a)
}

fn parse_geo(b: &[u8]) -> Result<(Geo, u8)> {
    if b.len() < 512 || &b[3..11] != b"EXFAT   " || b[510] != 0x55 || b[511] != 0xAA {
        return Err(FixtureError::corrupt("exFAT boot sector", "bad signature"));
    }
    let bps_shift = b[108];
    let spc_shift = b[109];
    if !(9..=12).contains(&bps_shift) || spc_shift > 25 - bps_shift {
        return Err(FixtureError::corrupt("exFAT boot sector", "bad shifts"));
    }
    if b[110] != 1 {
        return Err(FixtureError::Unsupported(
            "exFAT with two FATs (TexFAT)".into(),
        ));
    }
    let g = Geo {
        bps: 1 << bps_shift,
        spc: 1 << spc_shift,
        fat_off: rd32(b, 80) as u64,
        fat_len: rd32(b, 84) as u64,
        heap_off: rd32(b, 88) as u64,
        clusters: rd32(b, 92),
        root_cluster: rd32(b, 96),
        volume_len: rd64(b, 72),
    };
    if g.clusters == 0
        || g.root_cluster < 2
        || g.root_cluster > g.clusters + 1
        || g.fat_len == 0
        || g.heap_off == 0
    {
        return Err(FixtureError::corrupt(
            "exFAT boot sector",
            "inconsistent geometry",
        ));
    }
    if (g.fat_len * g.bps) < (g.clusters as u64 + 2) * 4 {
        return Err(FixtureError::corrupt("exFAT boot sector", "FAT too small"));
    }
    Ok((g, b[112]))
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

// ---------------------------------------------------------------- checksums

/// exFAT entry-set checksum over `set`, skipping bytes 2 and 3.
pub(crate) fn set_checksum(set: &[u8]) -> u16 {
    let mut s = 0u16;
    for (i, &b) in set.iter().enumerate() {
        if i == 2 || i == 3 {
            continue;
        }
        s = (if s & 1 == 1 { 0x8000u16 } else { 0 })
            .wrapping_add(s >> 1)
            .wrapping_add(b as u16);
    }
    s
}

/// exFAT name hash over the up-cased UTF-16LE bytes.
pub(crate) fn name_hash(upcased: &[u16]) -> u16 {
    let mut s = 0u16;
    for u in upcased {
        for b in u.to_le_bytes() {
            s = (if s & 1 == 1 { 0x8000u16 } else { 0 })
                .wrapping_add(s >> 1)
                .wrapping_add(b as u16);
        }
    }
    s
}

/// Boot region checksum over sectors 0..=10, skipping VolumeFlags and
/// PercentInUse.
pub(crate) fn boot_checksum(region: &[u8]) -> u32 {
    let mut s = 0u32;
    for (i, &b) in region.iter().enumerate() {
        if i == 106 || i == 107 || i == 112 {
            continue;
        }
        s = (if s & 1 == 1 { 0x8000_0000u32 } else { 0 })
            .wrapping_add(s >> 1)
            .wrapping_add(b as u32);
    }
    s
}

// ---------------------------------------------------------------- volume model

struct Vol {
    f: File,
    geo: Geo,
    fat: Vec<u32>,
    bitmap: Vec<u8>,
    bitmap_clusters: Vec<u32>,
    upcase: Vec<u16>,
}

impl Vol {
    fn bit(&self, c: u32) -> bool {
        let i = (c - 2) as usize;
        self.bitmap[i >> 3] & (1 << (i & 7)) != 0
    }
    fn set_bit(&mut self, c: u32, v: bool) {
        let i = (c - 2) as usize;
        if v {
            self.bitmap[i >> 3] |= 1 << (i & 7);
        } else {
            self.bitmap[i >> 3] &= !(1 << (i & 7));
        }
    }
    fn max_cluster(&self) -> u32 {
        self.geo.clusters + 1
    }
    fn find_run(&self, n: u32) -> Option<u32> {
        let mut run = 0;
        for c in 2..=self.max_cluster() {
            if !self.bit(c) {
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
    fn mark(&mut self, list: &[u32], v: bool) {
        for &c in list {
            self.set_bit(c, v);
        }
    }
    /// Contiguous allocation, bitmap only (caller decides about the FAT).
    fn alloc_run(&mut self, n: u32, what: &str) -> Result<Vec<u32>> {
        let first = self
            .find_run(n)
            .ok_or_else(|| FixtureError::Full(format!("no run of {n} free clusters for {what}")))?;
        let list: Vec<u32> = (first..first + n).collect();
        self.mark(&list, true);
        Ok(list)
    }
    fn chain(&mut self, list: &[u32]) {
        for (i, &c) in list.iter().enumerate() {
            self.fat[c as usize] = list.get(i + 1).copied().unwrap_or(EOF);
        }
    }
    fn alloc_fragmented(&mut self, n: u32, frag: Fragment, what: &str) -> Result<Vec<u32>> {
        let first_n = frag.first_clusters.min(n);
        if first_n == 0 || first_n == n {
            return self.alloc_run(n, what);
        }
        let mut list = self.alloc_run(first_n, what)?;
        let mut reserved = Vec::new();
        let mut c = list[list.len() - 1] + 1;
        while reserved.len() < frag.gap_clusters as usize && c <= self.max_cluster() {
            if !self.bit(c) {
                self.set_bit(c, true);
                reserved.push(c);
            }
            c += 1;
        }
        let second = self.alloc_run(n - first_n, what);
        for r in reserved {
            self.set_bit(r, false);
        }
        list.extend(second?);
        Ok(list)
    }
    fn up(&self, u: u16) -> u16 {
        self.upcase.get(u as usize).copied().unwrap_or(u)
    }
    fn used_clusters(&self) -> u64 {
        (2..=self.max_cluster()).filter(|&c| self.bit(c)).count() as u64
    }
}

/// Clusters of a system file (bitmap, up-case): follow the FAT when the
/// first entry is chained, otherwise assume a contiguous run.
fn system_clusters(fat: &[u32], first: u32, len: u64, cb: u64, max: u32) -> Result<Vec<u32>> {
    let n = len.div_ceil(cb).max(1) as usize;
    if first < 2 || first > max {
        return Err(FixtureError::corrupt(
            "exFAT system file",
            "bad first cluster",
        ));
    }
    let mut out = vec![first];
    if fat[first as usize] != 0 {
        let mut c = first;
        while out.len() < n {
            c = fat[c as usize];
            if c < 2 || c > max {
                return Err(FixtureError::corrupt(
                    "exFAT system file",
                    "chain ends early",
                ));
            }
            out.push(c);
        }
    } else {
        for k in 1..n as u32 {
            out.push(first + k);
        }
    }
    Ok(out)
}

fn read_clusters(f: &mut File, g: &Geo, list: &[u32]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(list.len() * g.cb() as usize);
    for &c in list {
        out.extend_from_slice(&read_at(f, g.cluster_off(c), g.cb() as usize)?);
    }
    Ok(out)
}

fn decode_upcase(raw: &[u8]) -> Vec<u16> {
    let mut table: Vec<u16> = (0..=0xFFFFu32).map(|i| i as u16).collect();
    let vals: Vec<u16> = raw
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let mut i = 0;
    let mut ch = 0usize;
    while i < vals.len() && ch <= 0xFFFF {
        if vals[i] == 0xFFFF && i + 1 < vals.len() {
            ch += vals[i + 1] as usize;
            i += 2;
        } else {
            table[ch] = vals[i];
            ch += 1;
            i += 1;
        }
    }
    table
}

// ---------------------------------------------------------------- entries

fn exfat_stamp(secs: i64) -> (u32, u8) {
    let (y, m, d, hh, mm, ss) = civil(secs);
    let y = y.clamp(1980, 2107);
    let v = (((y - 1980) as u32) << 25) | (m << 21) | (d << 16) | (hh << 11) | (mm << 5) | (ss / 2);
    (v, ((ss % 2) * 100) as u8)
}

fn check_name(name: &str) -> Result<Vec<u16>> {
    let units: Vec<u16> = name.encode_utf16().collect();
    if units.is_empty() || units.len() > 255 {
        return Err(FixtureError::invalid(format!(
            "name length out of range: {name:?}"
        )));
    }
    if name
        .chars()
        .any(|c| (c as u32) < 0x20 || "\"*/:<>?\\|".contains(c))
        || name.ends_with(' ')
        || name.ends_with('.')
    {
        return Err(FixtureError::invalid(format!(
            "name not allowed on exFAT: {name:?}"
        )));
    }
    Ok(units)
}

struct SetArgs<'a> {
    name: &'a str,
    attr: u16,
    mtime: i64,
    first: u32,
    size: u64,
    /// GeneralSecondaryFlags: bit 0 AllocationPossible, bit 1 NoFatChain.
    flags: u8,
}

fn entry_set(vol: &Vol, a: &SetArgs) -> Result<Vec<u8>> {
    let units = check_name(a.name)?;
    let nname = units.len().div_ceil(15);
    let mut set = vec![0u8; 32 * (2 + nname)];
    let (ts, tenths) = exfat_stamp(a.mtime);
    set[0] = 0x85;
    set[1] = (1 + nname) as u8;
    set[4..6].copy_from_slice(&a.attr.to_le_bytes());
    for o in [8, 12, 16] {
        set[o..o + 4].copy_from_slice(&ts.to_le_bytes());
    }
    set[20] = tenths;
    set[21] = tenths;
    set[22] = 0x80;
    set[23] = 0x80;
    set[24] = 0x80;
    set[32] = 0xC0;
    set[33] = a.flags;
    set[35] = units.len() as u8;
    let up: Vec<u16> = units.iter().map(|&u| vol.up(u)).collect();
    set[36..38].copy_from_slice(&name_hash(&up).to_le_bytes());
    set[40..48].copy_from_slice(&a.size.to_le_bytes());
    set[52..56].copy_from_slice(&a.first.to_le_bytes());
    set[56..64].copy_from_slice(&a.size.to_le_bytes());
    for k in 0..nname {
        let o = 64 + 32 * k;
        set[o] = 0xC1;
        for j in 0..15 {
            let u = units.get(k * 15 + j).copied().unwrap_or(0);
            set[o + 2 + 2 * j..o + 4 + 2 * j].copy_from_slice(&u.to_le_bytes());
        }
    }
    let cs = set_checksum(&set);
    set[2..4].copy_from_slice(&cs.to_le_bytes());
    Ok(set)
}

fn extents_of(g: &Geo, clusters: &[u32], size: u64) -> Vec<(u64, u64)> {
    let cb = g.cb();
    let mut out: Vec<(u64, u64)> = Vec::new();
    let mut left = size;
    for &c in clusters {
        if left == 0 {
            break;
        }
        let take = left.min(cb);
        let off = g.cluster_off(c);
        match out.last_mut() {
            Some((o, l)) if *o + *l == off => *l += take,
            _ => out.push((off, take)),
        }
        left -= take;
    }
    out
}

// ---------------------------------------------------------------- builder

pub(crate) fn build(spec: &FsSpec, out: &Path) -> Result<BuildReport> {
    tools::require("mkfs.exfat")?;
    let plan = Plan::new(spec, true)?;
    let mut f = create_sparse(out, spec.size)?;
    drop(f);
    let path = out.to_string_lossy().into_owned();
    let cs;
    let mut args = vec!["-q", "-s", "512"];
    if let Some(c) = spec.cluster_size {
        if c < 512 || !c.is_power_of_two() || c > 32 << 20 {
            return Err(FixtureError::invalid(format!("bad exFAT cluster size {c}")));
        }
        cs = c.to_string();
        args.push("-c");
        args.push(&cs);
    }
    let lab;
    if let Some(l) = &spec.label {
        if l.encode_utf16().count() > 11 {
            return Err(FixtureError::invalid(
                "exFAT label is limited to 11 characters",
            ));
        }
        lab = l.clone();
        args.push("-L");
        args.push(&lab);
    }
    args.push(&path);
    tools::run("mkfs.exfat", &args)?;

    f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(out)?;
    let boot = read_at(&mut f, 0, 512)?;
    let (geo, _) = parse_geo(&boot)?;
    if geo.bps != 512 {
        return Err(FixtureError::Unsupported("only 512-byte sectors".into()));
    }
    if geo.clusters as u64 > super::MAX_CLUSTERS {
        return Err(FixtureError::invalid(
            "too many clusters for the fixture builder; use a larger cluster size",
        ));
    }
    let max = geo.clusters + 1;
    let fat_raw = read_at(
        &mut f,
        geo.fat_off * geo.bps,
        (geo.clusters as usize + 2) * 4,
    )?;
    let fat: Vec<u32> = fat_raw.chunks_exact(4).map(|c| rd32(c, 0)).collect();

    // Root directory: follow its FAT chain, keep the system entries.
    let mut root_chain = vec![geo.root_cluster];
    let mut c = fat[geo.root_cluster as usize];
    while (2..=max).contains(&c) && root_chain.len() < 1 << 20 {
        root_chain.push(c);
        c = fat[c as usize];
    }
    let root_raw = read_clusters(&mut f, &geo, &root_chain)?;
    let mut sys_entries: Vec<[u8; 32]> = Vec::new();
    let mut bitmap_entry = None;
    let mut upcase_entry = None;
    for e in root_raw.chunks_exact(32) {
        match e[0] {
            0x00 => break,
            0x81 => {
                bitmap_entry = Some((rd32(e, 20), rd64(e, 24)));
                sys_entries.push(e.try_into().unwrap_or([0; 32]));
            }
            0x82 => {
                upcase_entry = Some((rd32(e, 20), rd64(e, 24)));
                sys_entries.push(e.try_into().unwrap_or([0; 32]));
            }
            0x83 | 0xA0 => sys_entries.push(e.try_into().unwrap_or([0; 32])),
            t if t & 0x80 != 0 => {
                return Err(FixtureError::corrupt(
                    "exFAT root directory",
                    format!("unexpected entry type {t:#x} on a fresh volume"),
                ))
            }
            _ => {}
        }
    }
    let (bm_first, bm_len) =
        bitmap_entry.ok_or_else(|| FixtureError::corrupt("exFAT root", "no allocation bitmap"))?;
    let (uc_first, uc_len) =
        upcase_entry.ok_or_else(|| FixtureError::corrupt("exFAT root", "no up-case table"))?;
    if bm_len > (geo.clusters as u64).div_ceil(8) + 2 * geo.cb() || uc_len > 1 << 20 {
        return Err(FixtureError::corrupt(
            "exFAT root",
            "bitmap or up-case table size is implausible",
        ));
    }
    let bitmap_clusters = system_clusters(&fat, bm_first, bm_len, geo.cb(), max)?;
    let mut bitmap = read_clusters(&mut f, &geo, &bitmap_clusters)?;
    bitmap.truncate(bm_len as usize);
    if (bitmap.len() as u64) < (geo.clusters as u64).div_ceil(8) {
        return Err(FixtureError::corrupt(
            "exFAT bitmap",
            "shorter than the cluster count",
        ));
    }
    let uc_clusters = system_clusters(&fat, uc_first, uc_len, geo.cb(), max)?;
    let mut uc_raw = read_clusters(&mut f, &geo, &uc_clusters)?;
    uc_raw.truncate(uc_len as usize);
    let upcase = decode_upcase(&uc_raw);

    let mut vol = Vol {
        f,
        geo,
        fat,
        bitmap,
        bitmap_clusters,
        upcase,
    };
    let mut report = BuildReport {
        tool: "mkfs.exfat+builtin".into(),
        ..Default::default()
    };
    populate(spec, &plan, &mut vol, sys_entries, root_chain, &mut report)?;
    report.deleted_supported = true;
    report.cluster_size = vol.geo.cb() as u32;
    report.data_start = vol.geo.heap_off * vol.geo.bps;
    Ok(report)
}

fn populate(
    spec: &FsSpec,
    plan: &Plan,
    vol: &mut Vol,
    sys_entries: Vec<[u8; 32]>,
    mut root_chain: Vec<u32>,
    report: &mut BuildReport,
) -> Result<()> {
    let cb = vol.geo.cb();
    // Directory cluster lists; the root keeps its existing chain.
    let mut dir_clusters: Vec<Vec<u32>> = vec![Vec::new(); plan.dirs.len()];
    let mut file_clusters: Vec<Vec<u32>> = vec![Vec::new(); plan.files.len()];
    let mut file_flags: Vec<u8> = vec![0x01; plan.files.len()];

    // Pre-compute entry-set sizes per directory so clusters can be sized.
    let set_len = |name: &str| -> Result<usize> {
        let u = check_name(name)?;
        Ok(32 * (2 + u.len().div_ceil(15)))
    };
    let mut dir_bytes = vec![0usize; plan.dirs.len()];
    for (di, d) in plan.dirs.iter().enumerate() {
        let mut n = if di == 0 { sys_entries.len() * 32 } else { 0 };
        for &c in &d.children {
            let name = match c {
                Child::Dir(i) => &plan.dirs[i].name,
                Child::File(i) => &plan.files[i].name,
            };
            n += set_len(name)?;
        }
        dir_bytes[di] = n + 32; // terminator
    }

    // Grow the root first.
    let need_root = (dir_bytes[0] as u64).div_ceil(cb).max(1) as usize;
    while root_chain.len() < need_root {
        let extra = vol.alloc_run(1, "root directory growth")?;
        let last = *root_chain.last().unwrap_or(&vol.geo.root_cluster);
        vol.fat[last as usize] = extra[0];
        vol.fat[extra[0] as usize] = EOF;
        root_chain.push(extra[0]);
    }
    dir_clusters[0] = root_chain;

    for &item in &plan.order {
        match item {
            Child::Dir(i) => {
                let n = (dir_bytes[i] as u64).div_ceil(cb).max(1) as u32;
                let list = vol.alloc_run(n, &plan.dirs[i].path)?;
                vol.chain(&list);
                dir_clusters[i] = list;
            }
            Child::File(i) => {
                let fs = &spec.files[plan.files[i].spec];
                let n = (fs.data.len() as u64).div_ceil(cb) as u32;
                if n == 0 {
                    continue;
                }
                let (list, chained) = match fs.fragment {
                    Some(fr) if fr.first_clusters > 0 && fr.first_clusters < n => {
                        (vol.alloc_fragmented(n, fr, &plan.files[i].path)?, true)
                    }
                    _ => (vol.alloc_run(n, &plan.files[i].path)?, false),
                };
                if chained {
                    vol.chain(&list);
                    file_flags[i] = 0x01;
                } else {
                    file_flags[i] = 0x03;
                }
                for (k, chunk) in fs.data.chunks(cb as usize).enumerate() {
                    write_at(&mut vol.f, vol.geo.cluster_off(list[k]), chunk)?;
                }
                file_clusters[i] = list;
            }
        }
    }

    // Serialise directories.
    for (di, d) in plan.dirs.iter().enumerate() {
        let cap = dir_clusters[di].len() * cb as usize;
        let mut buf = vec![0u8; cap];
        let mut pos = 0usize;
        if di == 0 {
            for e in &sys_entries {
                buf[pos..pos + 32].copy_from_slice(e);
                pos += 32;
            }
        }
        for &c in &d.children {
            let (name, attr, mtime, first, size, flags, deleted) = match c {
                Child::Dir(i) => (
                    plan.dirs[i].name.as_str(),
                    0x10u16,
                    plan.dirs[i].mtime,
                    dir_clusters[i][0],
                    dir_clusters[i].len() as u64 * cb,
                    0x01u8,
                    plan.dirs[i].deleted,
                ),
                Child::File(i) => {
                    let n = &plan.files[i];
                    let sz = spec.files[n.spec].data.len() as u64;
                    (
                        n.name.as_str(),
                        0x20u16,
                        n.mtime,
                        file_clusters[i].first().copied().unwrap_or(0),
                        sz,
                        file_flags[i],
                        n.deleted,
                    )
                }
            };
            let mut set = entry_set(
                vol,
                &SetArgs {
                    name,
                    attr,
                    mtime,
                    first,
                    size,
                    flags,
                },
            )?;
            if deleted {
                for k in (0..set.len()).step_by(32) {
                    set[k] &= 0x7F;
                }
            }
            if pos + set.len() > cap {
                return Err(FixtureError::Full("directory overflow".into()));
            }
            buf[pos..pos + set.len()].copy_from_slice(&set);
            pos += set.len();
        }
        for (k, &c) in dir_clusters[di].iter().enumerate() {
            let s = k * cb as usize;
            write_at(&mut vol.f, vol.geo.cluster_off(c), &buf[s..s + cb as usize])?;
        }
    }

    for (i, n) in plan.files.iter().enumerate() {
        let sz = spec.files[n.spec].data.len() as u64;
        report.placed.push(PlacedFile {
            path: n.path.clone(),
            size: sz,
            extents: extents_of(&vol.geo, &file_clusters[i], sz),
            deleted: n.deleted,
            mtime: n.mtime,
        });
    }

    // Deletion: bitmap bits and FAT chains.
    for (i, n) in plan.files.iter().enumerate() {
        if n.deleted {
            let list = file_clusters[i].clone();
            vol.mark(&list, false);
            if file_flags[i] & 0x02 == 0 {
                for c in list {
                    vol.fat[c as usize] = 0;
                }
            }
        }
    }
    for (i, d) in plan.dirs.iter().enumerate() {
        if d.deleted && i != 0 {
            let list = dir_clusters[i].clone();
            vol.mark(&list, false);
            for c in list {
                vol.fat[c as usize] = 0;
            }
        }
    }

    // Flush FAT and bitmap.
    let mut fat_bytes = Vec::with_capacity(vol.fat.len() * 4);
    for v in &vol.fat {
        fat_bytes.extend_from_slice(&v.to_le_bytes());
    }
    write_at(&mut vol.f, vol.geo.fat_off * vol.geo.bps, &fat_bytes)?;
    let mut pos = 0usize;
    for &c in &vol.bitmap_clusters.clone() {
        let end = (pos + cb as usize).min(vol.bitmap.len());
        if pos >= end {
            break;
        }
        write_at(&mut vol.f, vol.geo.cluster_off(c), &vol.bitmap[pos..end])?;
        pos = end;
    }
    update_boot_region(vol)?;
    vol.f.flush()?;
    Ok(())
}

/// Fix the serial number and PercentInUse, then rewrite the checksum
/// sectors of the main and backup boot regions.
fn update_boot_region(vol: &mut Vol) -> Result<()> {
    let bps = vol.geo.bps as usize;
    let mut region = read_at(&mut vol.f, 0, bps * 12)?;
    let used = vol.used_clusters();
    let pct = ((used * 100) / vol.geo.clusters as u64).min(100) as u8;
    region[100..104].copy_from_slice(&SERIAL.to_le_bytes());
    region[112] = pct;
    let sum = boot_checksum(&region[..bps * 11]);
    for chunk in region[bps * 11..bps * 12].chunks_exact_mut(4) {
        chunk.copy_from_slice(&sum.to_le_bytes());
    }
    write_at(&mut vol.f, 0, &region)?;
    if vol.geo.volume_len >= 24 {
        write_at(&mut vol.f, 12 * vol.geo.bps, &region)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksums_match_spec_examples() {
        // The entry-set checksum skips bytes 2 and 3.
        let mut a = vec![0u8; 64];
        a[0] = 0x85;
        a[1] = 1;
        let c1 = set_checksum(&a);
        a[2] = 0xAA;
        a[3] = 0xBB;
        assert_eq!(set_checksum(&a), c1);
        a[4] = 1;
        assert_ne!(set_checksum(&a), c1);
        // Name hash of the empty name is 0; it changes with every unit.
        assert_eq!(name_hash(&[]), 0);
        assert_ne!(name_hash(&[0x41]), name_hash(&[0x42]));
        // Boot checksum ignores VolumeFlags and PercentInUse.
        let mut r = vec![7u8; 512 * 11];
        let base = boot_checksum(&r);
        r[106] = 1;
        r[107] = 2;
        r[112] = 3;
        assert_eq!(boot_checksum(&r), base);
        r[113] = 3;
        assert_ne!(boot_checksum(&r), base);
    }

    #[test]
    fn upcase_decoding() {
        // identity run of 'a' (0x61) characters, then 'a'..'c' map to upper-case.
        let mut raw = Vec::new();
        for v in [0xFFFFu16, 0x61, 0x41, 0x42, 0x43] {
            raw.extend_from_slice(&v.to_le_bytes());
        }
        let t = decode_upcase(&raw);
        assert_eq!(t[0x60], 0x60);
        assert_eq!(t[0x61], 0x41);
        assert_eq!(t[0x63], 0x43);
        assert_eq!(t[0x64], 0x64);
        assert_eq!(t.len(), 65536);
    }

    #[test]
    fn stamp_encoding() {
        let (v, tenths) = exfat_stamp(crate::fs::DEFAULT_MTIME);
        assert_eq!(v >> 25, 2024 - 1980);
        assert_eq!((v >> 21) & 0xF, 5);
        assert_eq!((v >> 16) & 0x1F, 6);
        assert_eq!((v >> 11) & 0x1F, 7);
        assert_eq!((v >> 5) & 0x3F, 8);
        assert_eq!(v & 0x1F, 4);
        assert_eq!(tenths, 100);
    }

    #[test]
    fn name_validation() {
        assert!(check_name("ok name.jpg").is_ok());
        assert!(check_name("bad:name").is_err());
        assert!(check_name("trailing.").is_err());
        assert!(check_name(&"x".repeat(256)).is_err());
        assert!(check_name("tab\tname").is_err());
    }

    #[test]
    fn geo_rejects_garbage() {
        assert!(parse_geo(&[0u8; 512]).is_err());
        assert!(parse_geo(&[1u8; 10]).is_err());
    }
}
