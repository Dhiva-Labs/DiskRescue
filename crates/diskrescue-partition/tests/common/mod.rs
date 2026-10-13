//! Shared test helpers: sparse disk images, filesystem builders (real mkfs
//! tools), MBR/GPT writers and a tiny deterministic RNG.
#![allow(dead_code)]

use diskrescue_fixtures::FileExt;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use diskrescue_core::source::{FileSource, MemSource};
use diskrescue_core::SharedSource;

pub const MIB: u64 = 1 << 20;

/// Locate an external tool in PATH or the usual sbin directories.
pub fn tool_path(name: &str) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    for extra in ["/sbin", "/usr/sbin", "/usr/local/sbin"] {
        dirs.push(PathBuf::from(extra));
    }
    dirs.into_iter().map(|d| d.join(name)).find(|p| p.is_file())
}

/// Evaluate to the tool path or return from the test with an explicit skip note.
#[macro_export]
macro_rules! require_tool {
    ($name:expr) => {
        match $crate::common::tool_path($name) {
            Some(p) => p,
            None => {
                eprintln!("skipped: {} missing", $name);
                return;
            }
        }
    };
}

/// Run a tool, panicking with its output when it fails.
pub fn run(tool: &Path, args: &[&str]) {
    let out = Command::new(tool)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("cannot run {}: {e}", tool.display()));
    assert!(
        out.status.success(),
        "{} {:?} failed: {}{}",
        tool.display(),
        args,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// xorshift64* generator: deterministic, dependency free.
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }

    pub fn fill(&mut self, buf: &mut [u8]) {
        for b in buf {
            *b = self.next() as u8;
        }
    }

    pub fn bytes(&mut self, n: usize) -> Vec<u8> {
        let mut v = vec![0u8; n];
        self.fill(&mut v);
        v
    }
}

/// A sparse, file-backed disk image.
pub struct Disk {
    pub path: PathBuf,
    file: File,
    pub size: u64,
}

impl Disk {
    pub fn create(dir: &Path, name: &str, size: u64) -> Disk {
        let path = dir.join(name);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        file.set_len(size).unwrap();
        Disk { path, file, size }
    }

    pub fn write_at(&self, off: u64, data: &[u8]) {
        self.file.write_all_at(data, off).unwrap();
    }

    pub fn read_at(&self, off: u64, len: usize) -> Vec<u8> {
        let mut v = vec![0u8; len];
        self.file.read_exact_at(&mut v, off).unwrap();
        v
    }

    pub fn zero(&self, off: u64, len: u64) {
        let chunk = vec![0u8; (1 << 20).min(len as usize)];
        let mut done = 0u64;
        while done < len {
            let n = (len - done).min(chunk.len() as u64) as usize;
            self.write_at(off + done, &chunk[..n]);
            done += n as u64;
        }
    }

    /// Copy the file `src` to `off`, skipping all-zero 64 KiB blocks so
    /// sparse images stay sparse.
    pub fn embed(&self, off: u64, src: &Path) {
        let f = File::open(src).unwrap();
        let len = f.metadata().unwrap().len();
        assert!(off + len <= self.size, "image does not fit in the disk");
        let mut buf = vec![0u8; 64 << 10];
        let mut done = 0u64;
        while done < len {
            let n = (len - done).min(buf.len() as u64) as usize;
            f.read_exact_at(&mut buf[..n], done).unwrap();
            if buf[..n].iter().any(|&b| b != 0) {
                self.write_at(off + done, &buf[..n]);
            }
            done += n as u64;
        }
    }

    pub fn source(&self) -> SharedSource {
        Arc::new(FileSource::open(&self.path).unwrap())
    }

    /// Load the whole image into memory (small images only).
    pub fn mem(&self) -> Vec<u8> {
        std::fs::read(&self.path).unwrap()
    }
}

pub fn mem_source(data: Vec<u8>) -> SharedSource {
    Arc::new(MemSource::new(data))
}

/// Which filesystem to format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mk {
    Fat12,
    Fat16,
    Fat32,
    Fat32Sector4096,
    ExFat,
    ExFatSector4096,
    Ntfs,
    Ext2,
    Ext3,
    Ext4Block1k,
    Ext4Block4k,
}

impl Mk {
    pub fn tool(self) -> &'static str {
        match self {
            Mk::Fat12 | Mk::Fat16 | Mk::Fat32 | Mk::Fat32Sector4096 => "mkfs.vfat",
            Mk::ExFat | Mk::ExFatSector4096 => "mkfs.exfat",
            Mk::Ntfs => "mkntfs",
            Mk::Ext2 => "mkfs.ext2",
            Mk::Ext3 => "mkfs.ext3",
            Mk::Ext4Block1k | Mk::Ext4Block4k => "mkfs.ext4",
        }
    }

    /// Image size in bytes.
    pub fn size(self) -> u64 {
        match self {
            Mk::Fat12 => 4 * MIB,
            Mk::Fat16 => 20 * MIB,
            Mk::Fat32 => 40 * MIB,
            Mk::Fat32Sector4096 => 64 * MIB,
            Mk::ExFat => 16 * MIB,
            Mk::ExFatSector4096 => 32 * MIB,
            Mk::Ntfs => 16 * MIB,
            Mk::Ext2 | Mk::Ext3 => 16 * MIB,
            Mk::Ext4Block1k => 32 * MIB,
            Mk::Ext4Block4k => 160 * MIB,
        }
    }

    pub fn label(self) -> Option<&'static str> {
        match self {
            Mk::Fat12 => Some("FAT12LAB"),
            Mk::Fat16 => Some("FAT16LAB"),
            Mk::Fat32 | Mk::Fat32Sector4096 => Some("FAT32LAB"),
            Mk::ExFat | Mk::ExFatSector4096 => Some("EXLAB"),
            Mk::Ntfs => None,
            Mk::Ext2 => Some("EXT2LAB"),
            Mk::Ext3 => Some("EXT3LAB"),
            Mk::Ext4Block1k | Mk::Ext4Block4k => Some("EXT4LAB"),
        }
    }
}

/// Create a filesystem image with the real mkfs tool. `None` (after printing
/// a skip note) when the tool is not installed.
pub fn mkfs(dir: &Path, kind: Mk) -> Option<PathBuf> {
    let tool = match tool_path(kind.tool()) {
        Some(t) => t,
        None => {
            eprintln!("skipped: {} missing", kind.tool());
            return None;
        }
    };
    let path = dir.join(format!("{kind:?}.img"));
    let f = File::create(&path).unwrap();
    f.set_len(kind.size()).unwrap();
    drop(f);
    let p = path.to_str().unwrap();
    let label = kind.label().unwrap_or("");
    match kind {
        Mk::Fat12 => run(&tool, &["-F", "12", "-n", label, p]),
        Mk::Fat16 => run(&tool, &["-F", "16", "-n", label, p]),
        Mk::Fat32 => run(&tool, &["-F", "32", "-n", label, p]),
        Mk::Fat32Sector4096 => run(&tool, &["-F", "32", "-S", "4096", "-n", label, p]),
        Mk::ExFat => run(&tool, &["-L", label, p]),
        Mk::ExFatSector4096 => run(&tool, &["-s", "4096", "-L", label, p]),
        Mk::Ntfs => run(&tool, &["-F", "-Q", "-L", "NTLAB", p]),
        Mk::Ext2 | Mk::Ext3 => run(&tool, &["-q", "-F", "-b", "1024", "-L", label, p]),
        Mk::Ext4Block1k => run(&tool, &["-q", "-F", "-b", "1024", "-L", label, p]),
        Mk::Ext4Block4k => run(
            &tool,
            &[
                "-q",
                "-F",
                "-b",
                "4096",
                "-E",
                "lazy_itable_init=1,lazy_journal_init=1,nodiscard",
                "-L",
                label,
                p,
            ],
        ),
    }
    Some(path)
}

// ---------------------------------------------------------------- MBR

pub struct MbrPart {
    pub status: u8,
    pub ptype: u8,
    pub lba: u32,
    pub sectors: u32,
}

pub fn mbr_sector(parts: &[MbrPart]) -> Vec<u8> {
    let mut s = vec![0u8; 512];
    s[0x1B8..0x1BC].copy_from_slice(&0x1234_5678u32.to_le_bytes());
    for (i, p) in parts.iter().enumerate().take(4) {
        let o = 0x1BE + 16 * i;
        s[o] = p.status;
        s[o + 1..o + 4].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
        s[o + 4] = p.ptype;
        s[o + 5..o + 8].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
        s[o + 8..o + 12].copy_from_slice(&p.lba.to_le_bytes());
        s[o + 12..o + 16].copy_from_slice(&p.sectors.to_le_bytes());
    }
    s[510] = 0x55;
    s[511] = 0xAA;
    s
}

// ---------------------------------------------------------------- GPT

pub const GUID_BASIC: &str = "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7";
pub const GUID_ESP: &str = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B";
pub const GUID_LINUX: &str = "0FC63DAF-8483-4772-8E79-3D69D8477DE4";

/// GUID text to the mixed-endian on-disk layout.
pub fn guid_bytes(s: &str) -> [u8; 16] {
    let hex: Vec<u8> = s
        .bytes()
        .filter(|b| *b != b'-')
        .collect::<Vec<_>>()
        .chunks(2)
        .map(|c| u8::from_str_radix(std::str::from_utf8(c).unwrap(), 16).unwrap())
        .collect();
    assert_eq!(hex.len(), 16);
    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&[hex[3], hex[2], hex[1], hex[0]]);
    out[4..6].copy_from_slice(&[hex[5], hex[4]]);
    out[6..8].copy_from_slice(&[hex[7], hex[6]]);
    out[8..16].copy_from_slice(&hex[8..16]);
    out
}

#[derive(Clone)]
pub struct GptPart {
    pub type_guid: &'static str,
    pub unique: [u8; 16],
    pub first: u64,
    pub last: u64,
    pub attrs: u64,
    pub name: &'static str,
}

#[derive(Clone)]
pub struct GptBuild {
    pub ss: u64,
    pub total_lbas: u64,
    pub parts: Vec<GptPart>,
    pub disk_guid: &'static str,
    pub num_entries: u32,
    pub entry_size: u32,
}

/// Where the pieces of a written GPT live.
#[derive(Clone, Copy, Debug)]
pub struct GptLayout {
    pub primary_header_lba: u64,
    pub primary_array_lba: u64,
    pub backup_array_lba: u64,
    pub backup_header_lba: u64,
    pub array_sectors: u64,
    pub first_usable: u64,
    pub last_usable: u64,
}

impl GptBuild {
    pub fn new(ss: u64, total_lbas: u64, parts: Vec<GptPart>) -> GptBuild {
        GptBuild {
            ss,
            total_lbas,
            parts,
            disk_guid: "11111111-2222-3333-4444-555555555555",
            num_entries: 128,
            entry_size: 128,
        }
    }

    pub fn array(&self) -> Vec<u8> {
        let mut arr = vec![0u8; self.num_entries as usize * self.entry_size as usize];
        for (i, p) in self.parts.iter().enumerate() {
            let o = i * self.entry_size as usize;
            arr[o..o + 16].copy_from_slice(&guid_bytes(p.type_guid));
            arr[o + 16..o + 32].copy_from_slice(&p.unique);
            arr[o + 32..o + 40].copy_from_slice(&p.first.to_le_bytes());
            arr[o + 40..o + 48].copy_from_slice(&p.last.to_le_bytes());
            arr[o + 48..o + 56].copy_from_slice(&p.attrs.to_le_bytes());
            for (j, c) in p.name.encode_utf16().enumerate().take(36) {
                arr[o + 56 + 2 * j..o + 58 + 2 * j].copy_from_slice(&c.to_le_bytes());
            }
        }
        arr
    }

    pub fn layout(&self) -> GptLayout {
        let array_bytes = u64::from(self.num_entries) * u64::from(self.entry_size);
        let array_sectors = array_bytes.div_ceil(self.ss);
        GptLayout {
            primary_header_lba: 1,
            primary_array_lba: 2,
            backup_array_lba: self.total_lbas - 1 - array_sectors,
            backup_header_lba: self.total_lbas - 1,
            array_sectors,
            first_usable: 2 + array_sectors,
            last_usable: self.total_lbas - 2 - array_sectors,
        }
    }

    pub fn header(&self, my: u64, alt: u64, entries_lba: u64, array_crc: u32) -> Vec<u8> {
        let l = self.layout();
        let mut h = vec![0u8; self.ss as usize];
        h[0..8].copy_from_slice(b"EFI PART");
        h[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
        h[12..16].copy_from_slice(&92u32.to_le_bytes());
        h[24..32].copy_from_slice(&my.to_le_bytes());
        h[32..40].copy_from_slice(&alt.to_le_bytes());
        h[40..48].copy_from_slice(&l.first_usable.to_le_bytes());
        h[48..56].copy_from_slice(&l.last_usable.to_le_bytes());
        h[56..72].copy_from_slice(&guid_bytes(self.disk_guid));
        h[72..80].copy_from_slice(&entries_lba.to_le_bytes());
        h[80..84].copy_from_slice(&self.num_entries.to_le_bytes());
        h[84..88].copy_from_slice(&self.entry_size.to_le_bytes());
        h[88..92].copy_from_slice(&array_crc.to_le_bytes());
        let crc = crc32fast::hash(&h[..92]);
        h[16..20].copy_from_slice(&crc.to_le_bytes());
        h
    }

    pub fn protective_mbr(&self) -> Vec<u8> {
        let sectors = (self.total_lbas - 1).min(0xFFFF_FFFF) as u32;
        mbr_sector(&[MbrPart {
            status: 0,
            ptype: 0xEE,
            lba: 1,
            sectors,
        }])
    }

    /// Write protective MBR, both headers and both arrays.
    pub fn write(&self, disk: &Disk) -> GptLayout {
        let l = self.layout();
        let arr = self.array();
        let crc = crc32fast::hash(&arr);
        disk.write_at(0, &self.protective_mbr());
        disk.write_at(
            l.primary_header_lba * self.ss,
            &self.header(1, l.backup_header_lba, l.primary_array_lba, crc),
        );
        disk.write_at(l.primary_array_lba * self.ss, &arr);
        disk.write_at(l.backup_array_lba * self.ss, &arr);
        disk.write_at(
            l.backup_header_lba * self.ss,
            &self.header(l.backup_header_lba, 1, l.backup_array_lba, crc),
        );
        l
    }
}

pub fn unique(n: u8) -> [u8; 16] {
    [n; 16]
}

/// Convenience: a `GptPart` covering `[first_lba, last_lba]`.
pub fn gpt_part(
    type_guid: &'static str,
    n: u8,
    first: u64,
    last: u64,
    name: &'static str,
) -> GptPart {
    GptPart {
        type_guid,
        unique: unique(n),
        first,
        last,
        attrs: 0,
        name,
    }
}
