//! Minimal exFAT writer for test fixtures (independent of the crate's
//! reader): entry sets with SetChecksum and NameHash (using the volume's
//! up-case table), NoFatChain or FAT-chained data, directories, and
//! Linux-style deletion (bit 7 cleared on every entry of the set, bitmap
//! bits cleared, FAT left untouched). fsck.exfat -n is the oracle.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::Path;

/// A directory: first cluster and its clusters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExDir {
    pub clusters: Vec<u32>,
}

/// Data placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExMode {
    /// Contiguous clusters, NoFatChain = 1.
    NoFatChain,
    /// FAT chain over contiguous clusters.
    Chain,
    /// FAT chain over every other free cluster.
    FragmentedChain,
}

/// Result of a put.
#[derive(Debug, Clone)]
pub struct ExPut {
    pub clusters: Vec<u32>,
    /// Volume offset of the File (0x85) entry.
    pub entry_offset: u64,
}

pub struct ExImage {
    file: File,
    pub bps: u64,
    pub cs: u64,
    pub heap: u64,
    pub fat_off: u64,
    pub count: u32,
    pub root: u32,
    bitmap_clus: u32,
    pub bitmap: Vec<u8>,
    upcase: Vec<u16>,
    fat: Vec<u8>,
}

/// Packed timestamp 2024-05-01 12:30:10.
pub const STAMP: u32 = (((44u32 << 9) | (5 << 5) | 1) << 16) | ((12 << 11) | (30 << 5) | 5);

fn set_checksum(b: &[u8]) -> u16 {
    let mut s: u16 = 0;
    for (i, &x) in b.iter().enumerate() {
        if i == 2 || i == 3 {
            continue;
        }
        s = ((s & 1) << 15 | s >> 1).wrapping_add(x as u16);
    }
    s
}

fn name_hash(up: &[u16]) -> u16 {
    let mut s: u16 = 0;
    for u in up {
        for b in u.to_le_bytes() {
            s = ((s & 1) << 15 | s >> 1).wrapping_add(b as u16);
        }
    }
    s
}

impl ExImage {
    pub fn open(path: &Path) -> ExImage {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        let mut b = vec![0u8; 512];
        file.read_exact_at(&mut b, 0).unwrap();
        assert_eq!(&b[3..11], b"EXFAT   ");
        let r32 = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let bps = 1u64 << b[108];
        let cs = bps << b[109];
        let fat_off = r32(80) as u64 * bps;
        let fat_len = r32(84) as u64 * bps;
        let heap = r32(88) as u64 * bps;
        let count = r32(92);
        let root = r32(96);
        let mut fat = vec![0u8; fat_len as usize];
        file.read_exact_at(&mut fat, fat_off).unwrap();
        let mut img = ExImage {
            file,
            bps,
            cs,
            heap,
            fat_off,
            count,
            root,
            bitmap_clus: 0,
            bitmap: Vec::new(),
            upcase: (0..=0xFFFFu32).map(|c| c as u16).collect(),
            fat,
        };
        let root_dir = img.root_dir();
        let data = img.read_dir(&root_dir);
        for e in data.chunks(32) {
            let first = u32::from_le_bytes(e[20..24].try_into().unwrap());
            let len = u64::from_le_bytes(e[24..32].try_into().unwrap());
            if e[0] == 0x81 {
                img.bitmap_clus = first;
                img.bitmap = img.read_chain_bytes(first, len);
            }
            if e[0] == 0x82 {
                let raw = img.read_chain_bytes(first, len);
                let units: Vec<u16> = raw
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect();
                let mut ch = 0usize;
                let mut i = 0;
                while i < units.len() && ch < 0x10000 {
                    if units[i] == 0xFFFF && i + 1 < units.len() {
                        ch += units[i + 1] as usize;
                        i += 2;
                        continue;
                    }
                    img.upcase[ch] = units[i];
                    ch += 1;
                    i += 1;
                }
            }
        }
        assert!(img.bitmap_clus != 0);
        img
    }

    pub fn coff(&self, c: u32) -> u64 {
        self.heap + (c as u64 - 2) * self.cs
    }

    fn fat_get(&self, c: u32) -> u32 {
        let o = c as usize * 4;
        u32::from_le_bytes(self.fat[o..o + 4].try_into().unwrap())
    }

    fn fat_set(&mut self, c: u32, v: u32) {
        let o = c as usize * 4;
        self.fat[o..o + 4].copy_from_slice(&v.to_le_bytes());
    }

    pub fn chain(&self, first: u32) -> Vec<u32> {
        let mut v = Vec::new();
        let mut c = first;
        while c >= 2 && c <= self.count + 1 && v.len() <= self.count as usize {
            v.push(c);
            c = self.fat_get(c);
        }
        v
    }

    fn read_chain_bytes(&self, first: u32, len: u64) -> Vec<u8> {
        let mut out = Vec::new();
        for c in self.chain(first) {
            let mut b = vec![0u8; self.cs as usize];
            self.file.read_exact_at(&mut b, self.coff(c)).unwrap();
            out.extend_from_slice(&b);
        }
        out.truncate(len as usize);
        out
    }

    pub fn root_dir(&self) -> ExDir {
        ExDir {
            clusters: self.chain(self.root),
        }
    }

    fn bit(&self, c: u32) -> bool {
        let i = (c - 2) as usize;
        self.bitmap[i / 8] >> (i % 8) & 1 == 1
    }

    fn set_bit(&mut self, c: u32, v: bool) {
        let i = (c - 2) as usize;
        if v {
            self.bitmap[i / 8] |= 1 << (i % 8);
        } else {
            self.bitmap[i / 8] &= !(1 << (i % 8));
        }
    }

    pub fn free_count(&self) -> u64 {
        (2..self.count + 2).filter(|&c| !self.bit(c)).count() as u64
    }

    fn flush(&self) {
        let mut b = self.bitmap.clone();
        let n = b.len().div_ceil(self.cs as usize) * self.cs as usize;
        b.resize(n, 0);
        for (k, &c) in self.chain(self.bitmap_clus).iter().enumerate() {
            let s = k * self.cs as usize;
            if s >= b.len() {
                break;
            }
            self.file
                .write_all_at(&b[s..s + self.cs as usize], self.coff(c))
                .unwrap();
        }
        self.file.write_all_at(&self.fat, self.fat_off).unwrap();
    }

    fn alloc(&mut self, n: usize, mode: ExMode) -> Vec<u32> {
        if n == 0 {
            return Vec::new();
        }
        let max = self.count + 1;
        let v: Vec<u32> = if mode == ExMode::FragmentedChain {
            let mut v = Vec::new();
            let mut skip = false;
            for c in 2..=max {
                if self.bit(c) {
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
            v
        } else {
            let mut run = 0usize;
            let mut start = None;
            for c in 2..=max {
                run = if self.bit(c) { 0 } else { run + 1 };
                if run == n {
                    start = Some(c + 1 - n as u32);
                    break;
                }
            }
            let s = start.expect("no space");
            (s..s + n as u32).collect()
        };
        assert_eq!(v.len(), n);
        for &c in &v {
            self.set_bit(c, true);
        }
        if mode != ExMode::NoFatChain {
            for (i, &c) in v.iter().enumerate() {
                let next = if i + 1 < v.len() {
                    v[i + 1]
                } else {
                    0xFFFF_FFFF
                };
                self.fat_set(c, next);
            }
        }
        v
    }

    fn write_clusters(&self, clusters: &[u32], data: &[u8]) {
        let cs = self.cs as usize;
        for (i, &c) in clusters.iter().enumerate() {
            let mut buf = vec![0u8; cs];
            let s = i * cs;
            if s < data.len() {
                let e = (s + cs).min(data.len());
                buf[..e - s].copy_from_slice(&data[s..e]);
            }
            self.file.write_all_at(&buf, self.coff(c)).unwrap();
        }
    }

    fn read_dir(&self, d: &ExDir) -> Vec<u8> {
        let mut v = Vec::new();
        for &c in &d.clusters {
            let mut b = vec![0u8; self.cs as usize];
            self.file.read_exact_at(&mut b, self.coff(c)).unwrap();
            v.extend_from_slice(&b);
        }
        v
    }

    fn dir_off(&self, d: &ExDir, rel: usize) -> u64 {
        let k = rel / self.cs as usize;
        self.coff(d.clusters[k]) + (rel % self.cs as usize) as u64
    }

    fn add_set(&self, d: &ExDir, set: &[u8]) -> u64 {
        let data = self.read_dir(d);
        let need = set.len() / 32;
        let mut run = 0;
        for i in 0..data.len() / 32 {
            run = if data[i * 32] == 0 { run + 1 } else { 0 };
            if run == need {
                let start = (i + 1 - need) * 32;
                for (k, e) in set.chunks(32).enumerate() {
                    self.file
                        .write_all_at(e, self.dir_off(d, start + 32 * k))
                        .unwrap();
                }
                return self.dir_off(d, start);
            }
        }
        panic!("directory full");
    }

    /// Build an entry set.
    fn make_set(
        &self,
        name: &str,
        attrs: u16,
        flags: u8,
        first: u32,
        len: u64,
        vdl: u64,
    ) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        assert!(!units.is_empty() && units.len() <= 255);
        let ne = units.len().div_ceil(15);
        let mut set = vec![0u8; 32 * (2 + ne)];
        set[0] = 0x85;
        set[1] = (1 + ne) as u8;
        set[4..6].copy_from_slice(&attrs.to_le_bytes());
        for off in [8usize, 12, 16] {
            set[off..off + 4].copy_from_slice(&STAMP.to_le_bytes());
        }
        set[20] = 50; // create 10 ms increment: +0.5 s
        set[21] = 0;
        set[22] = 0x80 | 22; // UTC+05:30
        set[23] = 0x80 | 22;
        set[24] = 0x80 | 22;
        set[32] = 0xC0;
        set[33] = flags;
        set[35] = units.len() as u8;
        let up: Vec<u16> = units.iter().map(|&u| self.upcase[u as usize]).collect();
        set[36..38].copy_from_slice(&name_hash(&up).to_le_bytes());
        set[40..48].copy_from_slice(&vdl.to_le_bytes());
        set[52..56].copy_from_slice(&first.to_le_bytes());
        set[56..64].copy_from_slice(&len.to_le_bytes());
        for k in 0..ne {
            let e = &mut set[64 + 32 * k..96 + 32 * k];
            e[0] = 0xC1;
            for (j, &u) in units.iter().skip(15 * k).take(15).enumerate() {
                e[2 + 2 * j..4 + 2 * j].copy_from_slice(&u.to_le_bytes());
            }
        }
        let cs = set_checksum(&set);
        set[2..4].copy_from_slice(&cs.to_le_bytes());
        set
    }

    /// Put a file with ValidDataLength = `vdl` (None = full length).
    pub fn put_vdl(
        &mut self,
        d: &ExDir,
        name: &str,
        data: &[u8],
        mode: ExMode,
        vdl: Option<u64>,
    ) -> ExPut {
        let n = (data.len() as u64).div_ceil(self.cs) as usize;
        let clusters = self.alloc(n, mode);
        self.write_clusters(&clusters, data);
        self.flush();
        let first = clusters.first().copied().unwrap_or(0);
        let flags = if data.is_empty() {
            0x01
        } else if mode == ExMode::NoFatChain {
            0x03
        } else {
            0x01
        };
        let len = data.len() as u64;
        let set = self.make_set(name, 0x20, flags, first, len, vdl.unwrap_or(len));
        let entry_offset = self.add_set(d, &set);
        ExPut {
            clusters,
            entry_offset,
        }
    }

    pub fn put(&mut self, d: &ExDir, name: &str, data: &[u8], mode: ExMode) -> ExPut {
        self.put_vdl(d, name, data, mode, None)
    }

    /// Create a one-cluster subdirectory (NoFatChain).
    pub fn mkdir(&mut self, parent: &ExDir, name: &str) -> ExDir {
        let c = self.alloc(1, ExMode::NoFatChain);
        self.write_clusters(&c, &[]);
        self.flush();
        let set = self.make_set(name, 0x10, 0x03, c[0], self.cs, self.cs);
        self.add_set(parent, &set);
        ExDir { clusters: c }
    }

    /// Find a live set by name: (dir rel offset, set bytes).
    fn find(&self, d: &ExDir, name: &str) -> Option<(usize, Vec<u8>)> {
        let data = self.read_dir(d);
        let mut i = 0;
        while i + 32 <= data.len() {
            let t = data[i];
            if t == 0 {
                break;
            }
            if t == 0x85 {
                let sc = data[i + 1] as usize;
                let set = data[i..i + 32 * (sc + 1)].to_vec();
                let nl = set[35] as usize;
                let mut units = Vec::new();
                for k in 2..=sc {
                    let e = &set[32 * k..32 * k + 32];
                    if e[0] != 0xC1 {
                        break;
                    }
                    for j in 0..15 {
                        units.push(u16::from_le_bytes([e[2 + 2 * j], e[3 + 2 * j]]));
                    }
                }
                units.truncate(nl);
                if String::from_utf16_lossy(&units) == name {
                    return Some((i, set));
                }
                i += 32 * (sc + 1);
                continue;
            }
            i += 32;
        }
        None
    }

    /// Delete like the Linux driver: clear bit 7 of each entry, free the
    /// bitmap bits, leave the FAT alone.
    pub fn delete(&mut self, d: &ExDir, name: &str) {
        let (rel, set) = self
            .find(d, name)
            .unwrap_or_else(|| panic!("{name} not found"));
        for k in 0..set.len() / 32 {
            let off = self.dir_off(d, rel + 32 * k);
            self.file.write_all_at(&[set[32 * k] & 0x7F], off).unwrap();
        }
        let flags = set[33];
        let first = u32::from_le_bytes(set[52..56].try_into().unwrap());
        let len = u64::from_le_bytes(set[56..64].try_into().unwrap());
        let n = len.div_ceil(self.cs) as u32;
        if first >= 2 && n > 0 {
            let clusters: Vec<u32> = if flags & 2 != 0 {
                (first..first + n).collect()
            } else {
                self.chain(first).into_iter().take(n as usize).collect()
            };
            for c in clusters {
                self.set_bit(c, false);
            }
            self.flush();
        }
    }

    /// Zero a live set in place (simulates its sector reading as zeros).
    pub fn wipe(&mut self, d: &ExDir, name: &str) {
        let (rel, set) = self.find(d, name).unwrap();
        for k in 0..set.len() / 32 {
            let off = self.dir_off(d, rel + 32 * k);
            self.file.write_all_at(&[0u8; 32], off).unwrap();
        }
    }
}
