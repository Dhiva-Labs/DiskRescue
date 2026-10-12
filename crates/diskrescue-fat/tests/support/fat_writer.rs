//! Minimal FAT12/16/32 writer for test fixtures (independent of the crate's
//! reader). Files are written with 8.3 + LFN entries, directories with `.`
//! and `..`, FAT copies and the FAT32 FSInfo free count kept consistent so
//! fsck.vfat -n accepts the result. Deletion follows the Linux driver
//! (0xE5 on every slot, FAT chain freed, high cluster word kept) or, with
//! `win32`, also zeroes the high cluster word like Windows does on FAT32.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::Path;

/// Fixed timestamp used for every entry: 2024-05-01 12:30:10.
pub const DATE: u16 = (44 << 9) | (5 << 5) | 1;
/// Time part of the fixed timestamp.
pub const TIME: u16 = (12 << 11) | (30 << 5) | 5;

/// A directory: the FAT12/16 root region or a cluster chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Root,
    Cluster(u32),
}

/// How to place a file's clusters.
#[derive(Debug, Clone, Copy)]
pub enum Alloc {
    /// First fit, contiguous.
    Contiguous,
    /// Every other free cluster (non-contiguous chain).
    Fragmented,
    /// Contiguous starting exactly at this cluster.
    At(u32),
}

/// Result of a put.
#[derive(Debug, Clone)]
pub struct Put {
    pub clusters: Vec<u32>,
    pub short: [u8; 11],
    /// Volume offset of the short entry.
    pub entry_offset: u64,
}

pub struct FatImage {
    file: File,
    pub bps: u64,
    pub spc: u64,
    pub rsvd: u64,
    pub nfats: u64,
    pub fat_sz: u64,
    pub root_ent: u64,
    pub bits: u32,
    pub first_data: u64,
    pub count: u32,
    pub root_clus: u32,
    fsinfo: u64,
    bk_boot: u64,
    pub fat: Vec<u8>,
}

fn lfn_checksum(short: &[u8; 11]) -> u8 {
    let mut s: u8 = 0;
    for &b in short {
        s = ((s & 1) << 7).wrapping_add(s >> 1).wrapping_add(b);
    }
    s
}

fn u16at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn u32at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

impl FatImage {
    pub fn open(path: &Path) -> FatImage {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        let mut b = vec![0u8; 512];
        file.read_exact_at(&mut b, 0).unwrap();
        let bps = u16at(&b, 11) as u64;
        let spc = b[13] as u64;
        let rsvd = u16at(&b, 14) as u64;
        let nfats = b[16] as u64;
        let root_ent = u16at(&b, 17) as u64;
        let tot16 = u16at(&b, 19) as u64;
        let fatsz16 = u16at(&b, 22) as u64;
        let tot32 = u32at(&b, 32) as u64;
        let fatsz32 = u32at(&b, 36) as u64;
        let fat_sz = if fatsz16 != 0 { fatsz16 } else { fatsz32 };
        let tot = if tot16 != 0 { tot16 } else { tot32 };
        let root_secs = (root_ent * 32).div_ceil(bps);
        let first_data = rsvd + nfats * fat_sz + root_secs;
        let count = ((tot - first_data) / spc) as u32;
        let bits = if count < 4085 {
            12
        } else if count < 65525 {
            16
        } else {
            32
        };
        let root_clus = if bits == 32 { u32at(&b, 44) } else { 0 };
        let fsinfo = if bits == 32 { u16at(&b, 48) as u64 } else { 0 };
        let bk_boot = if bits == 32 { u16at(&b, 50) as u64 } else { 0 };
        let mut fat = vec![0u8; (fat_sz * bps) as usize];
        file.read_exact_at(&mut fat, rsvd * bps).unwrap();
        FatImage {
            file,
            bps,
            spc,
            rsvd,
            nfats,
            fat_sz,
            root_ent,
            bits,
            first_data,
            count,
            root_clus,
            fsinfo,
            bk_boot,
            fat,
        }
    }

    pub fn cs(&self) -> u64 {
        self.bps * self.spc
    }

    pub fn clus_off(&self, c: u32) -> u64 {
        (self.first_data + (c as u64 - 2) * self.spc) * self.bps
    }

    pub fn eoc(&self) -> u32 {
        match self.bits {
            12 => 0xFFF,
            16 => 0xFFFF,
            _ => 0x0FFF_FFFF,
        }
    }

    pub fn get(&self, c: u32) -> u32 {
        let c = c as usize;
        match self.bits {
            12 => {
                let o = c + c / 2;
                let v = u16at(&self.fat, o);
                if c & 1 == 1 {
                    (v >> 4) as u32
                } else {
                    (v & 0xFFF) as u32
                }
            }
            16 => u16at(&self.fat, c * 2) as u32,
            _ => u32at(&self.fat, c * 4) & 0x0FFF_FFFF,
        }
    }

    pub fn set(&mut self, c: u32, v: u32) {
        let c = c as usize;
        match self.bits {
            12 => {
                let o = c + c / 2;
                let cur = u16at(&self.fat, o);
                let new = if c & 1 == 1 {
                    (cur & 0x000F) | ((v as u16) << 4)
                } else {
                    (cur & 0xF000) | (v as u16 & 0x0FFF)
                };
                self.fat[o..o + 2].copy_from_slice(&new.to_le_bytes());
            }
            16 => self.fat[c * 2..c * 2 + 2].copy_from_slice(&(v as u16).to_le_bytes()),
            _ => {
                let old = u32at(&self.fat, c * 4) & 0xF000_0000;
                self.fat[c * 4..c * 4 + 4]
                    .copy_from_slice(&(old | (v & 0x0FFF_FFFF)).to_le_bytes());
            }
        }
    }

    pub fn free_count(&self) -> u64 {
        (2..self.count + 2).filter(|&c| self.get(c) == 0).count() as u64
    }

    /// Write both FATs and the FSInfo free count.
    pub fn flush(&mut self) {
        for i in 0..self.nfats {
            self.file
                .write_all_at(&self.fat, (self.rsvd + i * self.fat_sz) * self.bps)
                .unwrap();
        }
        if self.bits == 32 {
            let free = self.free_count() as u32;
            let mut sectors = vec![self.fsinfo];
            if self.bk_boot != 0 {
                sectors.push(self.bk_boot + 1);
            }
            for s in sectors {
                let off = s * self.bps;
                self.file
                    .write_all_at(&free.to_le_bytes(), off + 0x1E8)
                    .unwrap();
                self.file
                    .write_all_at(&0xFFFF_FFFFu32.to_le_bytes(), off + 0x1EC)
                    .unwrap();
            }
        }
    }

    fn alloc(&mut self, n: usize, how: Alloc) -> Vec<u32> {
        if n == 0 {
            return Vec::new();
        }
        let max = self.count + 1;
        let out: Vec<u32> = match how {
            Alloc::At(start) => {
                let v: Vec<u32> = (start..start + n as u32).collect();
                assert!(
                    v.iter().all(|&c| c <= max && self.get(c) == 0),
                    "clusters not free"
                );
                v
            }
            Alloc::Contiguous => {
                let mut run = 0usize;
                let mut found = None;
                for c in 2..=max {
                    run = if self.get(c) == 0 { run + 1 } else { 0 };
                    if run == n {
                        found = Some(c + 1 - n as u32);
                        break;
                    }
                }
                let s = found.expect("no contiguous space");
                (s..s + n as u32).collect()
            }
            Alloc::Fragmented => {
                let mut v = Vec::new();
                let mut skip = false;
                for c in 2..=max {
                    if self.get(c) != 0 {
                        continue;
                    }
                    if !skip {
                        v.push(c);
                        if v.len() == n {
                            break;
                        }
                    }
                    skip = !skip;
                }
                assert_eq!(v.len(), n, "no space");
                v
            }
        };
        for (i, &c) in out.iter().enumerate() {
            let next = if i + 1 < out.len() {
                out[i + 1]
            } else {
                self.eoc()
            };
            self.set(c, next);
        }
        out
    }

    fn write_clusters(&self, clusters: &[u32], data: &[u8]) {
        let cs = self.cs() as usize;
        for (i, &c) in clusters.iter().enumerate() {
            let mut buf = vec![0u8; cs];
            let s = i * cs;
            if s < data.len() {
                let e = (s + cs).min(data.len());
                buf[..e - s].copy_from_slice(&data[s..e]);
            }
            self.file.write_all_at(&buf, self.clus_off(c)).unwrap();
        }
    }

    /// Chain of a directory or file.
    pub fn chain(&self, first: u32) -> Vec<u32> {
        let mut v = Vec::new();
        let mut c = first;
        while c >= 2 && c <= self.count + 1 && v.len() <= self.count as usize {
            v.push(c);
            c = self.get(c);
        }
        v
    }

    fn regions(&self, d: Dir) -> Vec<(u64, u64)> {
        match d {
            Dir::Root if self.bits != 32 => {
                let off = (self.rsvd + self.nfats * self.fat_sz) * self.bps;
                vec![(off, self.root_ent * 32)]
            }
            Dir::Root => self
                .chain(self.root_clus)
                .iter()
                .map(|&c| (self.clus_off(c), self.cs()))
                .collect(),
            Dir::Cluster(f) => self
                .chain(f)
                .iter()
                .map(|&c| (self.clus_off(c), self.cs()))
                .collect(),
        }
    }

    fn read_dir(&self, d: Dir) -> (Vec<u8>, Vec<(u64, u64)>) {
        let regs = self.regions(d);
        let mut v = Vec::new();
        for &(o, l) in &regs {
            let mut b = vec![0u8; l as usize];
            self.file.read_exact_at(&mut b, o).unwrap();
            v.extend_from_slice(&b);
        }
        (v, regs)
    }

    fn vol_off(regs: &[(u64, u64)], rel: u64) -> u64 {
        let mut base = 0;
        for &(o, l) in regs {
            if rel < base + l {
                return o + rel - base;
            }
            base += l;
        }
        panic!("offset outside directory");
    }

    fn write_entry_bytes(&self, regs: &[(u64, u64)], rel: u64, bytes: &[u8]) {
        for (k, chunk) in bytes.chunks(32).enumerate() {
            let off = Self::vol_off(regs, rel + 32 * k as u64);
            self.file.write_all_at(chunk, off).unwrap();
        }
    }

    /// Append entries in a free (0x00) run of slots, growing a cluster
    /// directory when needed. Returns the volume offset of each entry.
    fn add_entries(&mut self, d: Dir, entries: &[[u8; 32]]) -> Vec<u64> {
        loop {
            let (data, regs) = self.read_dir(d);
            let mut run = 0usize;
            for i in 0..data.len() / 32 {
                run = if data[i * 32] == 0 { run + 1 } else { 0 };
                if run == entries.len() {
                    let start = (i + 1 - run) * 32;
                    let mut flat = Vec::new();
                    for e in entries {
                        flat.extend_from_slice(e);
                    }
                    self.write_entry_bytes(&regs, start as u64, &flat);
                    return (0..entries.len())
                        .map(|k| Self::vol_off(&regs, (start + 32 * k) as u64))
                        .collect();
                }
            }
            let first = match d {
                Dir::Root if self.bits != 32 => panic!("root directory full"),
                Dir::Root => self.root_clus,
                Dir::Cluster(f) => f,
            };
            let last = *self.chain(first).last().unwrap();
            let new = self.alloc(1, Alloc::Contiguous)[0];
            self.set(last, new);
            self.write_clusters(&[new], &[]);
            self.flush();
        }
    }

    fn existing_shorts(&self, d: Dir) -> Vec<[u8; 11]> {
        let (data, _) = self.read_dir(d);
        data.chunks(32)
            .filter(|e| e[0] != 0 && e[11] != 0x0F)
            .map(|e| {
                let mut s = [0u8; 11];
                s.copy_from_slice(&e[..11]);
                s
            })
            .collect()
    }

    /// Short name and NT case flags for `name`, and whether an LFN is needed.
    fn short_name(&self, d: Dir, name: &str) -> ([u8; 11], u8, bool) {
        let (base, ext) = match name.rfind('.') {
            Some(i) if i > 0 => (&name[..i], &name[i + 1..]),
            _ => (name, ""),
        };
        let valid = |s: &str, max: usize| {
            !s.is_empty()
                && s.len() <= max
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "_-$~!#%&".contains(c))
        };
        let fits = valid(base, 8) && (ext.is_empty() || valid(ext, 3));
        let case_of = |s: &str| -> Option<bool> {
            // Some(false) = all upper, Some(true) = all lower, None = mixed
            let has_lower = s.chars().any(|c| c.is_ascii_lowercase());
            let has_upper = s.chars().any(|c| c.is_ascii_uppercase());
            match (has_lower, has_upper) {
                (true, true) => None,
                (true, false) => Some(true),
                _ => Some(false),
            }
        };
        if fits {
            if let (Some(bl), Some(el)) = (case_of(base), case_of(ext)) {
                let mut s = [b' '; 11];
                for (i, c) in base.to_ascii_uppercase().bytes().enumerate() {
                    s[i] = c;
                }
                for (i, c) in ext.to_ascii_uppercase().bytes().enumerate() {
                    s[8 + i] = c;
                }
                let nt = if bl { 0x08 } else { 0 } | if el && !ext.is_empty() { 0x10 } else { 0 };
                if !self.existing_shorts(d).contains(&s) {
                    return (s, nt, false);
                }
            }
        }
        let clean = |s: &str, max: usize| -> Vec<u8> {
            s.chars()
                .filter(|&c| c != ' ' && c != '.')
                .map(|c| {
                    if c.is_ascii_alphanumeric() {
                        c.to_ascii_uppercase() as u8
                    } else if "_-$~!#%&".contains(c) {
                        c as u8
                    } else {
                        b'_'
                    }
                })
                .take(max)
                .collect()
        };
        let mut b = clean(base, 6);
        if b.is_empty() {
            b = b"FILE".to_vec();
        }
        let e = clean(ext, 3);
        let existing = self.existing_shorts(d);
        for n in 1..1000 {
            let tail = format!("~{n}");
            let keep = (8 - tail.len()).min(b.len());
            let mut s = [b' '; 11];
            s[..keep].copy_from_slice(&b[..keep]);
            s[keep..keep + tail.len()].copy_from_slice(tail.as_bytes());
            s[8..8 + e.len()].copy_from_slice(&e);
            if !existing.contains(&s) {
                return (s, 0, true);
            }
        }
        panic!("no short name");
    }

    fn lfn_entries(name: &str, short: &[u8; 11]) -> Vec<[u8; 32]> {
        let chk = lfn_checksum(short);
        let mut units: Vec<u16> = name.encode_utf16().collect();
        assert!(units.len() <= 255);
        if units.len() % 13 != 0 {
            units.push(0);
            while units.len() % 13 != 0 {
                units.push(0xFFFF);
            }
        }
        let count = units.len() / 13;
        let offs = [1usize, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];
        let mut out = Vec::new();
        for seq in (1..=count).rev() {
            let mut e = [0u8; 32];
            e[0] = seq as u8 | if seq == count { 0x40 } else { 0 };
            e[11] = 0x0F;
            e[13] = chk;
            for (k, &o) in offs.iter().enumerate() {
                e[o..o + 2].copy_from_slice(&units[(seq - 1) * 13 + k].to_le_bytes());
            }
            out.push(e);
        }
        out
    }

    fn short_entry(short: &[u8; 11], attr: u8, nt: u8, first: u32, size: u32) -> [u8; 32] {
        let mut e = [0u8; 32];
        e[..11].copy_from_slice(short);
        e[11] = attr;
        e[12] = nt;
        e[13] = 50;
        e[14..16].copy_from_slice(&TIME.to_le_bytes());
        e[16..18].copy_from_slice(&DATE.to_le_bytes());
        e[18..20].copy_from_slice(&DATE.to_le_bytes());
        e[20..22].copy_from_slice(&((first >> 16) as u16).to_le_bytes());
        e[22..24].copy_from_slice(&TIME.to_le_bytes());
        e[24..26].copy_from_slice(&DATE.to_le_bytes());
        e[26..28].copy_from_slice(&(first as u16).to_le_bytes());
        e[28..32].copy_from_slice(&size.to_le_bytes());
        e
    }

    fn add_named(
        &mut self,
        d: Dir,
        name: &str,
        attr: u8,
        first: u32,
        size: u32,
    ) -> ([u8; 11], u64) {
        let (short, nt, lfn) = self.short_name(d, name);
        let mut ents = if lfn {
            Self::lfn_entries(name, &short)
        } else {
            Vec::new()
        };
        ents.push(Self::short_entry(&short, attr, nt, first, size));
        let offs = self.add_entries(d, &ents);
        (short, *offs.last().unwrap())
    }

    /// Put a file into directory `d`.
    pub fn put(&mut self, d: Dir, name: &str, data: &[u8], how: Alloc) -> Put {
        let n = (data.len() as u64).div_ceil(self.cs()) as usize;
        let clusters = self.alloc(n, how);
        self.write_clusters(&clusters, data);
        self.flush();
        let first = clusters.first().copied().unwrap_or(0);
        let (short, entry_offset) = self.add_named(d, name, 0x20, first, data.len() as u32);
        self.flush();
        Put {
            clusters,
            short,
            entry_offset,
        }
    }

    /// Create a subdirectory.
    pub fn mkdir(&mut self, parent: Dir, name: &str) -> Dir {
        let c = self.alloc(1, Alloc::Contiguous)[0];
        let parent_clus = match parent {
            Dir::Root => 0,
            Dir::Cluster(p) => p,
        };
        let dot = Self::short_entry(b".          ", 0x10, 0, c, 0);
        let dotdot = Self::short_entry(b"..         ", 0x10, 0, parent_clus, 0);
        let mut data = Vec::new();
        data.extend_from_slice(&dot);
        data.extend_from_slice(&dotdot);
        self.write_clusters(&[c], &data);
        self.flush();
        self.add_named(parent, name, 0x10, c, 0);
        self.flush();
        Dir::Cluster(c)
    }

    /// Find an entry by long or short display name: (rel offset of the
    /// first slot, rel offset of the short entry, short entry bytes).
    fn find(&self, d: Dir, name: &str) -> Option<(usize, usize, [u8; 32])> {
        let (data, _) = self.read_dir(d);
        let mut lfn_start: Option<usize> = None;
        let mut parts: Vec<Vec<u16>> = Vec::new();
        for i in 0..data.len() / 32 {
            let e = &data[i * 32..i * 32 + 32];
            if e[0] == 0 {
                break;
            }
            if e[0] == 0xE5 {
                lfn_start = None;
                parts.clear();
                continue;
            }
            if e[11] == 0x0F {
                if e[0] & 0x40 != 0 {
                    lfn_start = Some(i * 32);
                    parts.clear();
                }
                let offs = [1usize, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];
                parts.push(offs.iter().map(|&o| u16at(e, o)).collect());
                continue;
            }
            let long: Vec<u16> = parts
                .iter()
                .rev()
                .flatten()
                .copied()
                .take_while(|&u| u != 0)
                .collect();
            let long = String::from_utf16_lossy(&long);
            let base = String::from_utf8_lossy(&e[..8]).trim_end().to_string();
            let ext = String::from_utf8_lossy(&e[8..11]).trim_end().to_string();
            let short = if ext.is_empty() {
                base
            } else {
                format!("{base}.{ext}")
            };
            if long == name || short.eq_ignore_ascii_case(name) {
                let mut se = [0u8; 32];
                se.copy_from_slice(e);
                return Some((lfn_start.unwrap_or(i * 32), i * 32, se));
            }
            lfn_start = None;
            parts.clear();
        }
        None
    }

    /// Delete `name` from `d` (directories must already be emptied).
    pub fn delete(&mut self, d: Dir, name: &str, win32: bool) {
        let (start, short_rel, e) = self
            .find(d, name)
            .unwrap_or_else(|| panic!("{name} not found"));
        let (_, regs) = self.read_dir(d);
        let first = if self.bits == 32 {
            ((u16at(&e, 20) as u32) << 16) | u16at(&e, 26) as u32
        } else {
            u16at(&e, 26) as u32
        };
        for c in self.chain(first) {
            self.set(c, 0);
        }
        self.flush();
        let mut rel = start;
        while rel <= short_rel {
            self.write_entry_bytes(&regs, rel as u64, &[0xE5]);
            rel += 32;
        }
        if win32 {
            let off = Self::vol_off(&regs, short_rel as u64) + 20;
            self.file.write_all_at(&[0, 0], off).unwrap();
        }
    }

    /// Overwrite a directory entry (and its LFN slots) with zeros, as a
    /// bad sector read as zeros would; the FAT is left alone.
    pub fn wipe_entry(&mut self, d: Dir, name: &str) {
        let (start, short_rel, _) = self.find(d, name).unwrap();
        let (_, regs) = self.read_dir(d);
        let zeros = vec![0u8; short_rel + 32 - start];
        self.write_entry_bytes(&regs, start as u64, &zeros);
    }

    /// Volume offset of the first sector of FAT copy `i`.
    pub fn fat_offset(&self, i: u64) -> u64 {
        (self.rsvd + i * self.fat_sz) * self.bps
    }
}
