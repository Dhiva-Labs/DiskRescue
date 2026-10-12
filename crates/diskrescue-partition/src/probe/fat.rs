//! FAT12/16/32 boot sector (BIOS Parameter Block) validation.
//!
//! The checks follow the Microsoft FAT specification: jump byte, bytes per
//! sector, power-of-two sectors per cluster, reserved sectors, FAT count,
//! media byte, then the derived geometry. The FAT width is decided by the
//! *cluster count* (not by the `FAT32   ` string or the partition type):
//! below 4085 clusters FAT12, below 65525 FAT16, else FAT32. A BPB with the
//! FAT32 layout (zero 16-bit FAT size, zero root entry count) is always FAT32,
//! even when a non-conforming formatter produced fewer than 65525 clusters;
//! that case is reported in the notes.
//!
//! FAT32 keeps a copy of sectors 0-2 at sectors 6-8; [`probe_backup`] uses it
//! when sector 0 is damaged. FAT12/16 have no backup boot sector.

use diskrescue_core::fs::FsKind;
use diskrescue_core::util::bytes::{u16le, u32le};

use super::{label_from, Ctx, FsProbe};

/// Validated BPB geometry.
#[derive(Debug, Clone)]
pub(crate) struct FatBpb {
    pub kind: FsKind,
    /// True when the FAT32 extended BPB layout is in use.
    pub fat32_layout: bool,
    pub bps: u32,
    pub spc: u32,
    pub rsvd: u32,
    pub tot_sec: u64,
    pub media: u8,
    pub bk_boot_sec: u32,
    pub fsinfo: u32,
    pub label: Option<String>,
    pub serial: Option<u32>,
    pub notes: Vec<String>,
}

impl FatBpb {
    /// Volume size in bytes.
    pub fn size_bytes(&self) -> u64 {
        self.tot_sec.saturating_mul(u64::from(self.bps))
    }

    /// Cluster size in bytes.
    pub fn cluster_bytes(&self) -> u32 {
        self.bps * self.spc
    }
}

/// Validate a boot sector as FAT. `None` when any hard check fails.
pub(crate) fn parse_bpb(s: &[u8]) -> Option<FatBpb> {
    if s.len() < 512 || s[510] != 0x55 || s[511] != 0xAA {
        return None;
    }
    let mut notes = Vec::new();
    if s[0] != 0xEB && s[0] != 0xE9 {
        return None;
    }
    if s[0] == 0xEB && s[2] != 0x90 {
        notes.push(format!("unusual jump instruction (EB xx {:02X})", s[2]));
    }
    let bps = u32::from(u16le(s, 11)?);
    if !matches!(bps, 512 | 1024 | 2048 | 4096) {
        return None;
    }
    let spc = u32::from(s[13]);
    if spc == 0 || !spc.is_power_of_two() || spc > 128 {
        return None;
    }
    let rsvd = u32::from(u16le(s, 14)?);
    if rsvd == 0 {
        return None;
    }
    let nfats = u32::from(s[16]);
    if !(1..=2).contains(&nfats) {
        return None;
    }
    let root_ent = u32::from(u16le(s, 17)?);
    let tot16 = u64::from(u16le(s, 19)?);
    let media = s[21];
    if media < 0xF0 {
        return None;
    }
    let fatsz16 = u64::from(u16le(s, 22)?);
    let tot32 = u64::from(u32le(s, 32)?);
    let tot_sec = if tot16 != 0 { tot16 } else { tot32 };
    if tot_sec == 0 {
        return None;
    }
    let fat32_layout = fatsz16 == 0;
    let (fat_sz, root_clus, bk_boot_sec, fsinfo) = if fat32_layout {
        let fatsz32 = u64::from(u32le(s, 36)?);
        let fsver = u16le(s, 42)?;
        let root_clus = u32le(s, 44)?;
        if fatsz32 == 0 || root_ent != 0 || fsver != 0 || root_clus < 2 {
            return None;
        }
        (
            fatsz32,
            root_clus,
            u32::from(u16le(s, 50)?),
            u32::from(u16le(s, 48)?),
        )
    } else {
        if root_ent == 0 {
            return None;
        }
        (fatsz16, 0, 0, 0)
    };
    let root_dir_sectors = (u64::from(root_ent) * 32).div_ceil(u64::from(bps));
    let first_data = u64::from(rsvd) + u64::from(nfats) * fat_sz + root_dir_sectors;
    if first_data >= tot_sec {
        return None;
    }
    let clusters = (tot_sec - first_data) / u64::from(spc);
    if clusters == 0 {
        return None;
    }
    if bps * spc > 64 * 1024 {
        return None;
    }
    if bps * spc > 32 * 1024 {
        notes.push(format!(
            "cluster size {} KiB exceeds 32 KiB",
            bps * spc / 1024
        ));
    }
    let kind = if fat32_layout {
        if root_clus as u64 > clusters + 1 {
            return None;
        }
        if clusters < 65525 {
            notes.push(format!(
                "FAT32 layout with only {clusters} clusters (below the 65525 FAT32 minimum)"
            ));
        }
        FsKind::Fat32
    } else if clusters < 4085 {
        FsKind::Fat12
    } else {
        if clusters >= 65525 {
            notes.push(format!(
                "FAT12/16 layout with {clusters} clusters (above the FAT16 maximum)"
            ));
        }
        FsKind::Fat16
    };
    if !fat32_layout && (u64::from(root_ent) * 32) % u64::from(bps) != 0 {
        notes.push("root directory is not a whole number of sectors".to_string());
    }
    let entry_bits = match kind {
        FsKind::Fat12 => 12,
        FsKind::Fat16 => 16,
        _ => 32,
    };
    let fat_bytes_needed = (clusters + 2) * entry_bits / 8;
    if fat_sz * u64::from(bps) < fat_bytes_needed {
        notes.push("FAT is too small for the cluster count".to_string());
    }
    // The type string is informational only; flag disagreements.
    let type_str_off = if fat32_layout { 0x52 } else { 0x36 };
    if let Some(t) = s.get(type_str_off..type_str_off + 8) {
        let claimed = match t {
            b"FAT12   " => Some(FsKind::Fat12),
            b"FAT16   " => Some(FsKind::Fat16),
            b"FAT32   " => Some(FsKind::Fat32),
            _ => None,
        };
        if let Some(c) = claimed {
            if c != kind {
                notes.push(format!(
                    "BS_FilSysType says {} but the geometry says {}",
                    c.name(),
                    kind.name()
                ));
            }
        }
    }
    let (sig_off, serial_off, label_off) = if fat32_layout {
        (66, 67, 71)
    } else {
        (38, 39, 43)
    };
    let (label, serial) = if s[sig_off] == 0x29 {
        (
            label_from(s.get(label_off..label_off + 11)?),
            u32le(s, serial_off),
        )
    } else {
        (None, None)
    };
    if fat32_layout && bk_boot_sec != 0 && bk_boot_sec >= rsvd {
        notes.push(format!(
            "backup boot sector {bk_boot_sec} lies outside the reserved area"
        ));
    }
    Some(FatBpb {
        kind,
        fat32_layout,
        bps,
        spc,
        rsvd,
        tot_sec,
        media,
        bk_boot_sec,
        fsinfo,
        label,
        serial,
        notes,
    })
}

/// True when the first FAT sector at `fat_sector0` starts with the expected
/// "media byte, then ones" pattern (FAT[0] and FAT[1] of a healthy volume).
pub(crate) fn fat_header_ok(first_fat_bytes: &[u8], media: u8) -> bool {
    first_fat_bytes.len() >= 3
        && first_fat_bytes[0] == media
        && first_fat_bytes[1] == 0xFF
        && first_fat_bytes[2] == 0xFF
}

fn build(ctx: &Ctx, bpb: &FatBpb, via_backup: bool) -> FsProbe {
    let mut p = FsProbe::new(bpb.kind);
    p.size = Some(bpb.size_bytes());
    p.cluster_size = Some(bpb.cluster_bytes());
    p.label = bpb.label.clone();
    p.via_backup = via_backup;
    p.notes.clone_from(&bpb.notes);
    if let Some(serial) = bpb.serial {
        p.notes.push(format!(
            "serial {:04X}-{:04X}",
            serial >> 16,
            serial & 0xFFFF
        ));
    }
    if via_backup {
        p.notes.push(format!(
            "boot sector 0 is damaged; using the backup boot sector at sector {}",
            bpb.bk_boot_sec
        ));
    }
    if bpb.size_bytes() > ctx.avail {
        p.notes.push(format!(
            "filesystem claims {} bytes but only {} are available",
            bpb.size_bytes(),
            ctx.avail
        ));
    }
    let fat_off = u64::from(bpb.rsvd) * u64::from(bpb.bps);
    match ctx.read(fat_off, 4) {
        Some(f) if fat_header_ok(&f, bpb.media) => {}
        Some(_) => p
            .notes
            .push("FAT does not start with the media byte pattern".to_string()),
        None => p.notes.push("first FAT sector is unreadable".to_string()),
    }
    p
}

/// Probe the boot sector at the start of the volume.
pub(crate) fn probe_primary(ctx: &Ctx) -> Option<FsProbe> {
    let bpb = parse_bpb(ctx.head)?;
    Some(build(ctx, &bpb, false))
}

/// Probe the FAT32 backup boot sector (sector 6).
pub(crate) fn probe_backup(ctx: &Ctx) -> Option<FsProbe> {
    const BACKUP_SECTOR: u64 = 6;
    for bps in [512u64, 1024, 2048, 4096] {
        let Some(sec) = ctx.read(BACKUP_SECTOR * bps, 512) else {
            continue;
        };
        let Some(bpb) = parse_bpb(&sec) else { continue };
        if bpb.fat32_layout
            && u64::from(bpb.bps) == bps
            && u64::from(bpb.bk_boot_sec) == BACKUP_SECTOR
        {
            return Some(build(ctx, &bpb, true));
        }
    }
    None
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Build a plausible FAT BPB sector for tests.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn fat_sector(
        bps: u16,
        spc: u8,
        rsvd: u16,
        nfats: u8,
        root_ent: u16,
        tot16: u16,
        tot32: u32,
        fatsz16: u16,
        fatsz32: u32,
    ) -> Vec<u8> {
        let mut s = vec![0u8; 512];
        s[0] = 0xEB;
        s[1] = 0x58;
        s[2] = 0x90;
        s[3..11].copy_from_slice(b"mkfs.fat");
        s[11..13].copy_from_slice(&bps.to_le_bytes());
        s[13] = spc;
        s[14..16].copy_from_slice(&rsvd.to_le_bytes());
        s[16] = nfats;
        s[17..19].copy_from_slice(&root_ent.to_le_bytes());
        s[19..21].copy_from_slice(&tot16.to_le_bytes());
        s[21] = 0xF8;
        s[22..24].copy_from_slice(&fatsz16.to_le_bytes());
        s[32..36].copy_from_slice(&tot32.to_le_bytes());
        s[36..40].copy_from_slice(&fatsz32.to_le_bytes());
        s[44..48].copy_from_slice(&2u32.to_le_bytes());
        s[48..50].copy_from_slice(&1u16.to_le_bytes());
        s[50..52].copy_from_slice(&6u16.to_le_bytes());
        s[510] = 0x55;
        s[511] = 0xAA;
        s
    }

    #[test]
    fn classifies_by_cluster_count() {
        // 64 MiB FAT32, one sector per cluster.
        let s = fat_sector(512, 1, 32, 2, 0, 0, 131072, 0, 1009);
        let b = parse_bpb(&s).unwrap();
        assert_eq!(b.kind, FsKind::Fat32);
        assert_eq!(b.size_bytes(), 64 << 20);
        // 16 MiB FAT12: 4 sectors/cluster -> 8k clusters is FAT16 actually.
        let s = fat_sector(512, 4, 1, 2, 512, 32768, 0, 32, 0);
        assert_eq!(parse_bpb(&s).unwrap().kind, FsKind::Fat16);
        // Tiny floppy-like FAT12.
        let s = fat_sector(512, 1, 1, 2, 224, 2880, 0, 9, 0);
        assert_eq!(parse_bpb(&s).unwrap().kind, FsKind::Fat12);
    }

    #[test]
    fn rejects_bad_fields() {
        let good = fat_sector(512, 1, 32, 2, 0, 0, 131072, 0, 1009);
        assert!(parse_bpb(&good).is_some());
        for (off, val) in [
            (0usize, 0x00u8),
            (13, 3),
            (13, 0),
            (16, 0),
            (16, 3),
            (21, 0x10),
            (510, 0),
        ] {
            let mut s = good.clone();
            s[off] = val;
            assert!(parse_bpb(&s).is_none(), "byte {off}={val:#x} accepted");
        }
        let mut s = good.clone();
        s[11..13].copy_from_slice(&513u16.to_le_bytes());
        assert!(parse_bpb(&s).is_none());
        let mut s = good.clone();
        s[14..16].copy_from_slice(&0u16.to_le_bytes());
        assert!(parse_bpb(&s).is_none());
        // FAT32 layout with non-zero root entries.
        let mut s = good.clone();
        s[17..19].copy_from_slice(&512u16.to_le_bytes());
        assert!(parse_bpb(&s).is_none());
        // Total sectors smaller than the metadata.
        let mut s = good.clone();
        s[32..36].copy_from_slice(&10u32.to_le_bytes());
        assert!(parse_bpb(&s).is_none());
        assert!(parse_bpb(&good[..100]).is_none());
    }

    #[test]
    fn mbr_like_sector_is_not_fat() {
        let mut s = vec![0u8; 512];
        s[0] = 0x33;
        s[1] = 0xC0;
        s[0x1BE + 4] = 0x0C;
        s[0x1BE + 8..0x1BE + 12].copy_from_slice(&2048u32.to_le_bytes());
        s[0x1BE + 12..0x1BE + 16].copy_from_slice(&40960u32.to_le_bytes());
        s[510] = 0x55;
        s[511] = 0xAA;
        assert!(parse_bpb(&s).is_none());
    }

    #[test]
    fn type_string_is_only_a_hint() {
        // A FAT32-layout BPB with an (incorrect) "FAT16   " type string is
        // still FAT32 and gets a note.
        let mut s = fat_sector(512, 1, 32, 2, 0, 0, 131072, 0, 1009);
        s[0x52..0x5A].copy_from_slice(b"FAT16   ");
        let b = parse_bpb(&s).unwrap();
        assert_eq!(b.kind, FsKind::Fat32);
        assert!(b.notes.iter().any(|n| n.contains("BS_FilSysType")));
        // FAT32 with a 16-bit total sector count (small volumes) is accepted.
        let s = fat_sector(4096, 1, 32, 2, 0, 16384, 0, 0, 16);
        assert_eq!(parse_bpb(&s).unwrap().kind, FsKind::Fat32);
    }

    #[test]
    fn fat_header_pattern() {
        assert!(fat_header_ok(&[0xF8, 0xFF, 0xFF, 0x0F], 0xF8));
        assert!(!fat_header_ok(&[0xF0, 0xFF, 0xFF, 0x0F], 0xF8));
        assert!(!fat_header_ok(&[0xF8], 0xF8));
    }
}
