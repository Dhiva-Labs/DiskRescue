//! MBR and EBR (extended partition) parsing.
//!
//! A classic MBR sector holds four 16-byte entries at offset 0x1BE and the
//! boot signature `55 AA` at 0x1FE. Entries are interpreted with the LBA
//! fields only (CHS is ignored), in units of the device's logical sector
//! size. An entry whose type is an extended-partition type (0x05, 0x0F,
//! 0x85) is a container: the logical partitions inside it are found by
//! walking the chain of Extended Boot Records.
//!
//! EBR rules (see the filesystems spec, 2.1.2): entry 0 describes the logical
//! partition and its start LBA is relative to the EBR's own sector; entry 1
//! links to the next EBR and its start LBA is relative to the start of the
//! outer extended partition. The walk is bounded in three ways so hostile or
//! damaged tables cannot loop: a visited-set of EBR sectors, a hard limit of
//! [`MAX_LOGICAL_PARTITIONS`] logical partitions, and a requirement that every
//! EBR lies inside the extended partition that contains it.

use std::collections::HashSet;

use diskrescue_core::partition::PartitionEntry;
use diskrescue_core::util::bytes::u32le;
use diskrescue_core::{Result, Source};

use crate::io::read_soft;

/// Maximum number of logical partitions followed in one extended chain.
pub const MAX_LOGICAL_PARTITIONS: usize = 128;

/// Offset of the first partition entry inside an MBR/EBR sector.
const TABLE_OFFSET: usize = 0x1BE;

/// One raw 16-byte MBR/EBR partition entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MbrEntry {
    /// Slot (0..=3) the entry was read from.
    pub slot: u8,
    /// Status byte: 0x80 bootable, 0x00 inactive, anything else is suspicious.
    pub status: u8,
    /// Packed CHS address of the first sector (ignored).
    pub chs_start: [u8; 3],
    /// Partition type byte.
    pub ptype: u8,
    /// Packed CHS address of the last sector (ignored).
    pub chs_end: [u8; 3],
    /// First sector (LBA, relative to the table that holds the entry).
    pub first_lba: u32,
    /// Number of sectors.
    pub sectors: u32,
}

impl MbrEntry {
    /// Parse the 16 bytes at the start of `b`.
    pub fn parse(b: &[u8], slot: u8) -> Option<MbrEntry> {
        let raw = b.get(..16)?;
        Some(MbrEntry {
            slot,
            status: raw[0],
            chs_start: [raw[1], raw[2], raw[3]],
            ptype: raw[4],
            chs_end: [raw[5], raw[6], raw[7]],
            first_lba: u32le(raw, 8)?,
            sectors: u32le(raw, 12)?,
        })
    }

    /// An entry is unused when its type or its sector count is zero.
    pub fn is_empty(&self) -> bool {
        self.ptype == 0 || self.sectors == 0
    }

    /// Extended-partition container (0x05, 0x0F, 0x85).
    pub fn is_extended(&self) -> bool {
        is_extended_type(self.ptype)
    }

    /// GPT protective partition (0xEE).
    pub fn is_protective(&self) -> bool {
        self.ptype == 0xEE
    }

    /// Bit 7 of the status byte.
    pub fn bootable(&self) -> bool {
        self.status & 0x80 != 0
    }

    /// Status byte is one of the two values real tools write.
    pub fn status_valid(&self) -> bool {
        self.status == 0x00 || self.status == 0x80
    }
}

/// A parsed MBR (or EBR) sector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mbr {
    /// Whether bytes 0x1FE..0x200 are `55 AA`.
    pub boot_signature_ok: bool,
    /// The 32-bit disk signature at 0x1B8 (may be 0).
    pub disk_signature: u32,
    /// The four table entries.
    pub entries: [MbrEntry; 4],
}

impl Mbr {
    /// Non-empty entries.
    pub fn used(&self) -> impl Iterator<Item = &MbrEntry> {
        self.entries.iter().filter(|e| !e.is_empty())
    }

    /// Number of non-empty entries.
    pub fn used_count(&self) -> usize {
        self.used().count()
    }

    /// True when the first used entry is a GPT protective partition.
    pub fn has_protective_entry(&self) -> bool {
        self.used().any(MbrEntry::is_protective)
    }
}

/// Parse a 512-byte (or longer) sector as an MBR/EBR. Returns `None` only when
/// the buffer is too short; check [`Mbr::boot_signature_ok`] yourself.
pub fn parse_mbr(sector: &[u8]) -> Option<Mbr> {
    if sector.len() < 512 {
        return None;
    }
    let e = |i: usize| MbrEntry::parse(sector.get(TABLE_OFFSET + 16 * i..)?, i as u8);
    Some(Mbr {
        boot_signature_ok: sector[510] == 0x55 && sector[511] == 0xAA,
        disk_signature: u32le(sector, 0x1B8)?,
        entries: [e(0)?, e(1)?, e(2)?, e(3)?],
    })
}

/// True for the DOS extended-partition types 0x05, 0x0F and 0x85.
pub fn is_extended_type(t: u8) -> bool {
    matches!(t, 0x05 | 0x0F | 0x85)
}

/// Text form of an MBR type byte used as `PartitionEntry::type_id`.
pub fn type_id(t: u8) -> String {
    format!("0x{t:02X}")
}

/// Human description of an MBR partition type byte.
pub fn type_description(t: u8) -> &'static str {
    match t {
        0x00 => "Empty",
        0x01 => "FAT12",
        0x02 => "XENIX root",
        0x03 => "XENIX usr",
        0x04 => "FAT16 <32M",
        0x05 => "Extended (CHS)",
        0x06 => "FAT16",
        0x07 => "HPFS/NTFS/exFAT",
        0x08 => "AIX / QNX / Commodore",
        0x09 => "AIX bootable",
        0x0A => "OS/2 Boot Manager",
        0x0B => "W95 FAT32",
        0x0C => "W95 FAT32 (LBA)",
        0x0E => "W95 FAT16 (LBA)",
        0x0F => "W95 Extended (LBA)",
        0x10 => "OPUS",
        0x11 => "Hidden FAT12",
        0x12 => "Compaq diagnostics",
        0x14 => "Hidden FAT16 <32M",
        0x16 => "Hidden FAT16",
        0x17 => "Hidden HPFS/NTFS",
        0x1B => "Hidden W95 FAT32",
        0x1C => "Hidden W95 FAT32 (LBA)",
        0x1E => "Hidden W95 FAT16 (LBA)",
        0x24 => "NEC DOS",
        0x27 => "Windows RE hidden NTFS",
        0x39 => "Plan 9",
        0x3C => "PartitionMagic recovery",
        0x40 => "Venix 80286",
        0x41 => "PPC PReP Boot",
        0x42 => "SFS / Windows dynamic disk",
        0x4D => "QNX4.x",
        0x4E => "QNX4.x 2nd part",
        0x4F => "QNX4.x 3rd part",
        0x50 => "OnTrack DM",
        0x51 => "OnTrack DM6 Aux1",
        0x52 => "CP/M",
        0x63 => "GNU HURD / System V",
        0x64 => "Novell Netware 286",
        0x65 => "Novell Netware 386",
        0x70 => "DiskSecure Multi-Boot",
        0x75 => "PC/IX",
        0x80 => "Old Minix",
        0x81 => "Minix / old Linux",
        0x82 => "Linux swap / Solaris",
        0x83 => "Linux",
        0x84 => "OS/2 hidden or hibernation",
        0x85 => "Linux extended",
        0x86 => "NTFS volume set",
        0x87 => "NTFS volume set (HPFS mirror)",
        0x88 => "Linux plaintext",
        0x8E => "Linux LVM",
        0x93 => "Amoeba",
        0x94 => "Amoeba BBT",
        0x9F => "BSD/OS",
        0xA0 => "IBM Thinkpad hibernation",
        0xA5 => "FreeBSD",
        0xA6 => "OpenBSD",
        0xA7 => "NeXTSTEP",
        0xA8 => "Darwin UFS",
        0xA9 => "NetBSD",
        0xAB => "Darwin boot",
        0xAF => "HFS / HFS+",
        0xB7 => "BSDI fs",
        0xB8 => "BSDI swap",
        0xBB => "Boot Wizard hidden",
        0xBE => "Solaris boot",
        0xBF => "Solaris",
        0xC1 => "DRDOS/sec (FAT-12)",
        0xC4 => "DRDOS/sec (FAT-16 <32M)",
        0xC6 => "DRDOS/sec (FAT-16)",
        0xC7 => "Syrinx",
        0xDA => "Non-FS data",
        0xDB => "CP/M / CTOS",
        0xDE => "Dell Utility",
        0xDF => "BootIt",
        0xE1 => "DOS access",
        0xE3 => "DOS R/O",
        0xE4 => "SpeedStor",
        0xEB => "BeOS fs",
        0xEE => "GPT protective",
        0xEF => "EFI System (FAT)",
        0xF0 => "Linux/PA-RISC boot",
        0xF1 => "SpeedStor",
        0xF2 => "DOS secondary",
        0xF4 => "SpeedStor",
        0xFB => "VMware VMFS",
        0xFC => "VMware VMKCORE",
        0xFD => "Linux RAID autodetect",
        0xFE => "LANstep / Windows IBM PS/2 diagnostics",
        0xFF => "Xenix bad block table",
        _ => "Unknown",
    }
}

/// Build a [`PartitionEntry`] from an MBR/EBR entry. `start_lba` is the
/// absolute LBA already resolved by the caller; `None` is returned when the
/// byte arithmetic overflows.
pub(crate) fn entry_from(
    e: &MbrEntry,
    start_lba: u64,
    sector_size: u64,
    origin: &str,
) -> Option<PartitionEntry> {
    let start = start_lba.checked_mul(sector_size)?;
    let size = u64::from(e.sectors).checked_mul(sector_size)?;
    Some(PartitionEntry {
        index: 0,
        start,
        size,
        type_id: type_id(e.ptype),
        type_desc: type_description(e.ptype).to_string(),
        name: None,
        guid: None,
        bootable: e.bootable(),
        origin: origin.to_string(),
        fs: None,
    })
}

/// Result of resolving the four primary entries of an MBR.
#[derive(Debug, Default)]
pub(crate) struct MbrWalk {
    /// Primary and logical partitions in the order found (unsorted, unclipped).
    pub entries: Vec<PartitionEntry>,
    /// Problems noticed while walking.
    pub warnings: Vec<String>,
}

/// Resolve the primary entries of `mbr` and walk every extended chain.
///
/// Entries of type 0xEE (protective) are not returned. Extended containers are
/// walked but not returned as partitions themselves.
pub(crate) fn walk(src: &dyn Source, mbr: &Mbr, sector_size: u32) -> Result<MbrWalk> {
    let ss = u64::from(sector_size.max(512));
    let mut out = MbrWalk::default();
    let mut visited: HashSet<u64> = HashSet::new();
    let mut logical_total = 0usize;
    for e in mbr.used() {
        if e.is_protective() {
            continue;
        }
        if !e.status_valid() {
            out.warnings.push(format!(
                "MBR entry {} has an unusual status byte 0x{:02X}",
                e.slot + 1,
                e.status
            ));
        }
        if e.is_extended() {
            walk_extended(
                src,
                ss,
                u64::from(e.first_lba),
                u64::from(e.sectors),
                &mut visited,
                &mut logical_total,
                &mut out,
            )?;
            continue;
        }
        match entry_from(e, u64::from(e.first_lba), ss, "mbr") {
            Some(p) => out.entries.push(p),
            None => out.warnings.push(format!(
                "MBR entry {} overflows the address space",
                e.slot + 1
            )),
        }
    }
    Ok(out)
}

/// Follow one extended partition's EBR chain.
fn walk_extended(
    src: &dyn Source,
    ss: u64,
    ext_start: u64,
    ext_sectors: u64,
    visited: &mut HashSet<u64>,
    logical_total: &mut usize,
    out: &mut MbrWalk,
) -> Result<()> {
    let ext_end = ext_start.saturating_add(ext_sectors);
    let mut ebr_lba = ext_start;
    loop {
        if *logical_total >= MAX_LOGICAL_PARTITIONS {
            out.warnings.push(format!(
                "more than {MAX_LOGICAL_PARTITIONS} logical partitions; chain truncated"
            ));
            break;
        }
        if !visited.insert(ebr_lba) {
            out.warnings.push(format!(
                "EBR chain loops back to LBA {ebr_lba}; chain stopped"
            ));
            break;
        }
        if ebr_lba >= ext_end || ebr_lba < ext_start {
            out.warnings.push(format!(
                "EBR at LBA {ebr_lba} lies outside its extended partition; chain stopped"
            ));
            break;
        }
        let Some(off) = ebr_lba.checked_mul(ss) else {
            break;
        };
        let sec = match read_soft(src, off, 512)? {
            Some(s) if s.len() >= 512 => s,
            Some(_) => {
                out.warnings.push(format!(
                    "EBR at LBA {ebr_lba} is beyond the end of the device; chain stopped"
                ));
                break;
            }
            None => {
                out.warnings
                    .push(format!("EBR at LBA {ebr_lba} is unreadable; chain stopped"));
                break;
            }
        };
        let Some(ebr) = parse_mbr(&sec) else { break };
        if !ebr.boot_signature_ok {
            out.warnings.push(format!(
                "EBR at LBA {ebr_lba} has no boot signature; chain stopped"
            ));
            break;
        }
        let e0 = ebr.entries[0];
        let e1 = ebr.entries[1];
        if !e0.is_empty() {
            if e0.is_extended() {
                out.warnings.push(format!(
                    "EBR at LBA {ebr_lba} describes an extended partition as its logical partition"
                ));
            } else if let Some(p) = entry_from(
                &e0,
                ebr_lba.saturating_add(u64::from(e0.first_lba)),
                ss,
                "ebr",
            ) {
                let p_end_lba = (p.start + p.size) / ss;
                if p_end_lba > ext_end {
                    out.warnings.push(format!(
                        "logical partition at LBA {} extends past its extended partition",
                        p.start / ss
                    ));
                }
                out.entries.push(p);
                *logical_total += 1;
            }
        }
        if e1.is_empty() {
            break;
        }
        if !e1.is_extended() {
            out.warnings.push(format!(
                "EBR at LBA {ebr_lba} links to a non-extended type 0x{:02X}; chain stopped",
                e1.ptype
            ));
            break;
        }
        ebr_lba = ext_start.saturating_add(u64::from(e1.first_lba));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskrescue_core::source::MemSource;

    fn put_entry(sec: &mut [u8], slot: usize, status: u8, ptype: u8, lba: u32, n: u32) {
        let o = TABLE_OFFSET + 16 * slot;
        sec[o] = status;
        sec[o + 4] = ptype;
        sec[o + 8..o + 12].copy_from_slice(&lba.to_le_bytes());
        sec[o + 12..o + 16].copy_from_slice(&n.to_le_bytes());
    }

    fn blank() -> Vec<u8> {
        let mut s = vec![0u8; 512];
        s[510] = 0x55;
        s[511] = 0xAA;
        s
    }

    #[test]
    fn parses_entries() {
        let mut s = blank();
        put_entry(&mut s, 0, 0x80, 0x0C, 2048, 4096);
        put_entry(&mut s, 2, 0x00, 0x83, 8192, 100);
        let m = parse_mbr(&s).unwrap();
        assert!(m.boot_signature_ok);
        assert_eq!(m.used_count(), 2);
        assert!(m.entries[0].bootable());
        assert_eq!(m.entries[0].first_lba, 2048);
        assert!(m.entries[1].is_empty());
        assert!(!m.has_protective_entry());
        assert!(parse_mbr(&s[..100]).is_none());
    }

    #[test]
    fn type_table_covers_common_types() {
        for (t, d) in [
            (0x01, "FAT12"),
            (0x07, "HPFS/NTFS/exFAT"),
            (0x0C, "W95 FAT32 (LBA)"),
            (0x82, "Linux swap / Solaris"),
            (0x83, "Linux"),
            (0x8E, "Linux LVM"),
            (0xAF, "HFS / HFS+"),
            (0xEE, "GPT protective"),
            (0xEF, "EFI System (FAT)"),
            (0xFD, "Linux RAID autodetect"),
        ] {
            assert_eq!(type_description(t), d);
        }
        assert_eq!(type_description(0x99), "Unknown");
        assert_eq!(type_id(0x0c), "0x0C");
        for t in [0x05u8, 0x0F, 0x85] {
            assert!(is_extended_type(t));
        }
        assert!(!is_extended_type(0x83));
    }

    #[test]
    fn ebr_chain_is_followed_with_relative_addressing() {
        // Extended partition at LBA 100, 1000 sectors long.
        let mut disk = vec![0u8; 2000 * 512];
        let mut mbr = blank();
        put_entry(&mut mbr, 0, 0x80, 0x0C, 10, 50);
        put_entry(&mut mbr, 1, 0x00, 0x0F, 100, 1000);
        disk[..512].copy_from_slice(&mbr);
        // EBR 1 at LBA 100: logical at +63 (rel. to EBR), next EBR at +300 (rel. to ext).
        let mut e1 = blank();
        put_entry(&mut e1, 0, 0, 0x83, 63, 200);
        put_entry(&mut e1, 1, 0, 0x05, 300, 200);
        disk[100 * 512..101 * 512].copy_from_slice(&e1);
        let mut e2 = blank();
        put_entry(&mut e2, 0, 0, 0x07, 63, 100);
        disk[400 * 512..401 * 512].copy_from_slice(&e2);
        let src = MemSource::new(disk);
        let m = parse_mbr(src.bytes()).unwrap();
        let w = walk(&src, &m, 512).unwrap();
        assert_eq!(w.entries.len(), 3, "{:?}", w.warnings);
        assert_eq!(w.entries[0].origin, "mbr");
        assert_eq!(w.entries[1].start, 163 * 512);
        assert_eq!(w.entries[1].origin, "ebr");
        assert_eq!(w.entries[2].start, 463 * 512);
        assert_eq!(w.entries[2].type_id, "0x07");
        assert!(w.warnings.is_empty(), "{:?}", w.warnings);
    }

    #[test]
    fn cyclic_ebr_chain_terminates() {
        let mut disk = vec![0u8; 400 * 512];
        let mut mbr = blank();
        put_entry(&mut mbr, 0, 0, 0x05, 100, 200);
        disk[..512].copy_from_slice(&mbr);
        // EBR at 100 links to itself.
        let mut e = blank();
        put_entry(&mut e, 0, 0, 0x83, 1, 10);
        put_entry(&mut e, 1, 0, 0x05, 0, 200);
        disk[100 * 512..101 * 512].copy_from_slice(&e);
        let src = MemSource::new(disk);
        let m = parse_mbr(src.bytes()).unwrap();
        let w = walk(&src, &m, 512).unwrap();
        assert_eq!(w.entries.len(), 1);
        assert!(w.warnings.iter().any(|w| w.contains("loops")));
    }

    #[test]
    fn ebr_chain_is_capped() {
        // 300 EBRs, each linking to the next one.
        let n = 300usize;
        let mut disk = vec![0u8; (n + 10) * 512];
        let mut mbr = blank();
        put_entry(&mut mbr, 0, 0, 0x05, 1, (n + 8) as u32);
        disk[..512].copy_from_slice(&mbr);
        for i in 0..n {
            let mut e = blank();
            put_entry(&mut e, 0, 0, 0x83, 1, 1);
            put_entry(&mut e, 1, 0, 0x05, (i + 1) as u32, 1);
            disk[(1 + i) * 512..(2 + i) * 512].copy_from_slice(&e);
        }
        let src = MemSource::new(disk);
        let m = parse_mbr(src.bytes()).unwrap();
        let w = walk(&src, &m, 512).unwrap();
        assert_eq!(w.entries.len(), MAX_LOGICAL_PARTITIONS);
        assert!(w.warnings.iter().any(|w| w.contains("truncated")));
    }
}
