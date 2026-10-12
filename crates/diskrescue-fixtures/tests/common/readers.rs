//! Independent, deliberately simple readers for the filesystems the builders
//! write. They share no code with the builders, so a mistake on one side
//! shows up as a disagreement.
#![allow(dead_code)]

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

fn u16le(b: &[u8], o: usize) -> u32 {
    u16::from_le_bytes([b[o], b[o + 1]]) as u32
}
fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn u64le(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

fn read_at(f: &mut File, off: u64, len: usize) -> Vec<u8> {
    f.seek(SeekFrom::Start(off)).unwrap();
    let mut v = vec![0u8; len];
    f.read_exact(&mut v).unwrap();
    v
}

// ------------------------------------------------------------------ FAT

/// A directory entry found by [`FatImage::walk`].
#[derive(Debug, Clone)]
pub struct FatEnt {
    pub path: String,
    pub deleted: bool,
    pub is_dir: bool,
    pub attr: u8,
    pub short: [u8; 11],
    pub first: u32,
    pub first_hi: u32,
    pub size: u32,
    /// `Some(true)` when LFN entries preceded the entry and their checksums
    /// match the short name.
    pub lfn_ok: Option<bool>,
}

pub struct FatImage {
    pub f: File,
    pub bps: u64,
    pub spc: u64,
    pub fat: Vec<u32>,
    pub bits: u32,
    pub root_off: u64,
    pub root_len: u64,
    pub root_cluster: u32,
    pub data_off: u64,
    pub label: Option<String>,
}

impl FatImage {
    pub fn open(path: &Path) -> FatImage {
        let mut f = File::open(path).unwrap();
        let b = read_at(&mut f, 0, 512);
        let bps = u16le(&b, 11) as u64;
        let spc = b[13] as u64;
        let rsvd = u16le(&b, 14) as u64;
        let nfats = b[16] as u64;
        let root_ent = u16le(&b, 17) as u64;
        let tot = if u16le(&b, 19) != 0 {
            u16le(&b, 19) as u64
        } else {
            u32le(&b, 32) as u64
        };
        let fatsz = if u16le(&b, 22) != 0 {
            u16le(&b, 22) as u64
        } else {
            u32le(&b, 36) as u64
        };
        let root_secs = (root_ent * 32).div_ceil(bps);
        let clusters = (tot - rsvd - nfats * fatsz - root_secs) / spc;
        let bits = if clusters < 4085 {
            12
        } else if clusters < 65525 {
            16
        } else {
            32
        };
        let raw = read_at(&mut f, rsvd * bps, (fatsz * bps) as usize);
        let fat = (0..clusters as usize + 2)
            .map(|n| match bits {
                12 => {
                    let o = n * 3 / 2;
                    let v = raw[o] as u32 | (raw[o + 1] as u32) << 8;
                    if n & 1 == 1 {
                        v >> 4
                    } else {
                        v & 0xFFF
                    }
                }
                16 => u16le(&raw, n * 2),
                _ => u32le(&raw, n * 4) & 0x0FFF_FFFF,
            })
            .collect();
        FatImage {
            f,
            bps,
            spc,
            fat,
            bits,
            root_off: (rsvd + nfats * fatsz) * bps,
            root_len: root_secs * bps,
            root_cluster: if bits == 32 { u32le(&b, 44) } else { 0 },
            data_off: (rsvd + nfats * fatsz + root_secs) * bps,
            label: None,
        }
    }

    pub fn cluster_bytes(&self) -> u64 {
        self.bps * self.spc
    }

    pub fn cluster_off(&self, c: u32) -> u64 {
        self.data_off + (c as u64 - 2) * self.cluster_bytes()
    }

    fn is_eoc(&self, v: u32) -> bool {
        match self.bits {
            12 => v >= 0xFF8,
            16 => v >= 0xFFF8,
            _ => v >= 0x0FFF_FFF8,
        }
    }

    pub fn chain(&self, first: u32) -> Vec<u32> {
        let mut out = Vec::new();
        let mut c = first;
        while c >= 2 && (c as usize) < self.fat.len() && out.len() < 1 << 22 {
            out.push(c);
            let n = self.fat[c as usize];
            if self.is_eoc(n) || n == 0 {
                break;
            }
            c = n;
        }
        out
    }

    /// File bytes by following the FAT chain, truncated to `size`.
    pub fn read_file(&mut self, first: u32, size: u32) -> Vec<u8> {
        let mut out = Vec::new();
        let cb = self.cluster_bytes() as usize;
        for c in self.chain(first) {
            let off = self.cluster_off(c);
            out.extend_from_slice(&read_at(&mut self.f, off, cb));
            if out.len() >= size as usize {
                break;
            }
        }
        out.truncate(size as usize);
        out
    }

    fn dir_bytes_root(&mut self) -> Vec<u8> {
        if self.bits == 32 {
            self.dir_bytes(self.root_cluster)
        } else {
            read_at(&mut self.f, self.root_off, self.root_len as usize)
        }
    }

    fn dir_bytes(&mut self, first: u32) -> Vec<u8> {
        let cb = self.cluster_bytes() as usize;
        let mut out = Vec::new();
        for c in self.chain(first) {
            let off = self.cluster_off(c);
            out.extend_from_slice(&read_at(&mut self.f, off, cb));
        }
        out
    }

    /// Walk every directory reachable through live entries.
    pub fn walk(&mut self) -> Vec<FatEnt> {
        let mut out = Vec::new();
        let root = self.dir_bytes_root();
        self.parse_dir(&root, "", &mut out, 0);
        out
    }

    fn parse_dir(&mut self, buf: &[u8], prefix: &str, out: &mut Vec<FatEnt>, depth: usize) {
        assert!(depth < 16);
        let mut lfn: Vec<[u8; 32]> = Vec::new();
        for e in buf.chunks_exact(32) {
            if e[0] == 0 {
                break;
            }
            if e[11] == 0x0F {
                lfn.push(e.try_into().unwrap());
                continue;
            }
            let group = std::mem::take(&mut lfn);
            if e[11] & 0x08 != 0 {
                self.label = Some(String::from_utf8_lossy(&e[..11]).trim_end().to_string());
                continue;
            }
            let deleted = e[0] == 0xE5;
            let mut short = [0u8; 11];
            short.copy_from_slice(&e[..11]);
            // Assemble the long name (physical order is highest sequence first).
            let mut name = None;
            let mut lfn_ok = None;
            if !group.is_empty() {
                let sum_with = |first: u8| {
                    let mut sum = 0u8;
                    for (i, &b) in short.iter().enumerate() {
                        let b = if i == 0 { first } else { b };
                        sum = (((sum & 1) << 7) | (sum >> 1)).wrapping_add(b);
                    }
                    sum
                };
                // A deleted entry lost its first byte; accept any value that
                // makes the stored checksums consistent.
                let mut ok = if deleted {
                    (0..=255u8).any(|c| group.iter().all(|g| g[13] == sum_with(c)))
                } else {
                    group.iter().all(|g| g[13] == sum_with(short[0]))
                };
                let mut units: Vec<u16> = Vec::new();
                for g in group.iter().rev() {
                    for &p in &[1usize, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30] {
                        units.push(u16::from_le_bytes([g[p], g[p + 1]]));
                    }
                }
                if let Some(z) = units.iter().position(|&u| u == 0) {
                    units.truncate(z);
                } else {
                    ok = ok && !units.is_empty();
                }
                lfn_ok = Some(ok);
                name = Some(String::from_utf16_lossy(&units));
            }
            let name = name.unwrap_or_else(|| {
                let mut s = short;
                if deleted {
                    s[0] = b'?';
                }
                let base = String::from_utf8_lossy(&s[..8]).trim_end().to_string();
                let ext = String::from_utf8_lossy(&s[8..]).trim_end().to_string();
                let (base, ext) = if e[12] & 0x08 != 0 {
                    (base.to_lowercase(), ext)
                } else {
                    (base, ext)
                };
                let ext = if e[12] & 0x10 != 0 {
                    ext.to_lowercase()
                } else {
                    ext
                };
                if ext.is_empty() {
                    base
                } else {
                    format!("{base}.{ext}")
                }
            });
            if name == "." || name == ".." || short[0] == b'.' {
                continue;
            }
            let path = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            let first_hi = u16le(e, 20);
            let first = u16le(e, 26) | (first_hi << 16);
            let ent = FatEnt {
                path: path.clone(),
                deleted,
                is_dir: e[11] & 0x10 != 0,
                attr: e[11],
                short,
                first,
                first_hi,
                size: u32le(e, 28),
                lfn_ok,
            };
            let recurse = ent.is_dir && !deleted && first >= 2;
            out.push(ent);
            if recurse {
                let sub = self.dir_bytes(first);
                self.parse_dir(&sub, &path, out, depth + 1);
            }
        }
    }
}

// ------------------------------------------------------------------ exFAT

#[derive(Debug, Clone)]
pub struct ExEnt {
    pub path: String,
    pub deleted: bool,
    pub is_dir: bool,
    pub first: u32,
    pub size: u64,
    pub valid_len: u64,
    pub no_fat_chain: bool,
    pub checksum_ok: bool,
    pub hash_ok: bool,
    pub name_len_ok: bool,
}

pub struct ExImage {
    pub f: File,
    pub bps: u64,
    pub spc: u64,
    pub heap_off: u64,
    pub fat: Vec<u32>,
    pub root: u32,
    pub clusters: u32,
    pub bitmap: Vec<u8>,
    pub label: Option<String>,
    pub percent_in_use: u8,
}

fn ex_cs(set: &[u8]) -> u16 {
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

fn ex_hash(units: &[u16]) -> u16 {
    let mut s = 0u16;
    for u in units {
        for b in u.to_le_bytes() {
            s = (if s & 1 == 1 { 0x8000u16 } else { 0 })
                .wrapping_add(s >> 1)
                .wrapping_add(b as u16);
        }
    }
    s
}

impl ExImage {
    pub fn open(path: &Path) -> ExImage {
        let mut f = File::open(path).unwrap();
        let b = read_at(&mut f, 0, 512);
        assert_eq!(&b[3..11], b"EXFAT   ");
        let bps = 1u64 << b[108];
        let spc = 1u64 << b[109];
        let fat_off = u32le(&b, 80) as u64;
        let clusters = u32le(&b, 92);
        let raw = read_at(&mut f, fat_off * bps, (clusters as usize + 2) * 4);
        let fat = raw.chunks_exact(4).map(|c| u32le(c, 0)).collect();
        let mut img = ExImage {
            f,
            bps,
            spc,
            heap_off: u32le(&b, 88) as u64,
            fat,
            root: u32le(&b, 96),
            clusters,
            bitmap: Vec::new(),
            label: None,
            percent_in_use: b[112],
        };
        let root = img.chain_bytes(img.root);
        for e in root.chunks_exact(32) {
            match e[0] {
                0x81 => {
                    let first = u32le(e, 20);
                    let len = u64le(e, 24) as usize;
                    let mut bm = img.contiguous_or_chain(first, len);
                    bm.truncate(len);
                    img.bitmap = bm;
                }
                0x83 => {
                    let n = e[1] as usize;
                    let units: Vec<u16> = (0..n).map(|i| u16le(e, 2 + 2 * i) as u16).collect();
                    img.label = Some(String::from_utf16_lossy(&units));
                }
                _ => {}
            }
        }
        img
    }

    pub fn cb(&self) -> u64 {
        self.bps * self.spc
    }

    pub fn cluster_off(&self, c: u32) -> u64 {
        (self.heap_off + (c as u64 - 2) * self.spc) * self.bps
    }

    pub fn bit(&self, c: u32) -> bool {
        let i = (c - 2) as usize;
        self.bitmap[i / 8] & (1 << (i % 8)) != 0
    }

    pub fn used_clusters(&self) -> u64 {
        (2..self.clusters + 2).filter(|&c| self.bit(c)).count() as u64
    }

    pub fn chain_bytes(&mut self, first: u32) -> Vec<u8> {
        let mut out = Vec::new();
        let mut c = first;
        let cb = self.cb() as usize;
        let mut guard = 0;
        while c >= 2 && c < self.clusters + 2 && guard < 1 << 20 {
            let off = self.cluster_off(c);
            out.extend_from_slice(&read_at(&mut self.f, off, cb));
            c = self.fat[c as usize];
            guard += 1;
        }
        out
    }

    fn contiguous_or_chain(&mut self, first: u32, len: usize) -> Vec<u8> {
        if self.fat[first as usize] != 0 {
            return self.chain_bytes(first);
        }
        let n = len.div_ceil(self.cb() as usize);
        let off = self.cluster_off(first);
        let len = n * self.cb() as usize;
        read_at(&mut self.f, off, len)
    }

    /// File bytes: contiguous when NoFatChain is set, FAT chain otherwise.
    pub fn read_file(&mut self, e: &ExEnt) -> Vec<u8> {
        if e.size == 0 {
            return Vec::new();
        }
        let mut v = if e.no_fat_chain {
            let n = (e.size as usize).div_ceil(self.cb() as usize);
            let off = self.cluster_off(e.first);
            let len = n * self.cb() as usize;
            read_at(&mut self.f, off, len)
        } else {
            self.chain_bytes(e.first)
        };
        v.truncate(e.size as usize);
        v
    }

    pub fn walk(&mut self) -> Vec<ExEnt> {
        let mut out = Vec::new();
        let root = self.chain_bytes(self.root);
        self.parse_dir(&root, "", &mut out, 0);
        out
    }

    fn parse_dir(&mut self, buf: &[u8], prefix: &str, out: &mut Vec<ExEnt>, depth: usize) {
        assert!(depth < 16);
        let mut pos = 0;
        while pos + 32 <= buf.len() {
            let t = buf[pos];
            if t == 0 {
                break;
            }
            if t != 0x85 && t != 0x05 {
                pos += 32;
                continue;
            }
            let count = buf[pos + 1] as usize;
            let set = &buf[pos..(pos + 32 * (count + 1)).min(buf.len())];
            assert_eq!(set.len(), 32 * (count + 1), "entry set cut off");
            let deleted = t == 0x05;
            // Verify that every secondary of a deleted set has its InUse bit clear.
            for k in 0..=count {
                assert_eq!(set[32 * k] & 0x80 == 0, deleted, "InUse bits uniform");
            }
            let stream = &set[32..64];
            let name_len = stream[3] as usize;
            let mut units = Vec::new();
            for k in 2..=count {
                for j in 0..15 {
                    units.push(u16le(&set[32 * k..], 2 + 2 * j) as u16);
                }
            }
            let units_len = units.len();
            units.truncate(name_len);
            let name = String::from_utf16_lossy(&units);
            let up: Vec<u16> = name
                .chars()
                .flat_map(|c| c.to_uppercase().next().into_iter())
                .map(|c| c as u16)
                .collect();
            // Re-check the checksum against the set with the InUse bits restored.
            let mut restored = set.to_vec();
            if deleted {
                for k in 0..=count {
                    restored[32 * k] |= 0x80;
                }
            }
            let flags = stream[1];
            let ent = ExEnt {
                path: if prefix.is_empty() {
                    name.clone()
                } else {
                    format!("{prefix}/{name}")
                },
                deleted,
                is_dir: u16le(set, 4) & 0x10 != 0,
                first: u32le(stream, 20),
                size: u64le(stream, 24),
                valid_len: u64le(stream, 8),
                no_fat_chain: flags & 2 != 0,
                checksum_ok: ex_cs(&restored) as u32 == u16le(set, 2),
                hash_ok: (up.len() != units.len()) || ex_hash(&up) as u32 == u16le(stream, 4),
                name_len_ok: name_len <= units_len && name_len > 0,
            };
            let recurse = ent.is_dir && !deleted && ent.first >= 2;
            let path = ent.path.clone();
            let first = ent.first;
            out.push(ent);
            if recurse {
                let sub = self.chain_bytes(first);
                self.parse_dir(&sub, &path, out, depth + 1);
            }
            pos += 32 * (count + 1);
        }
    }
}
