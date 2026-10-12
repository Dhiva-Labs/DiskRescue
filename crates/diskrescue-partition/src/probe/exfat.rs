//! exFAT boot region validation.
//!
//! The main boot region is sectors 0-11 of the volume and its byte-identical
//! backup is sectors 12-23. Sector 0 carries `EXFAT   ` at offset 3, a zeroed
//! 53-byte "MustBeZero" area and the volume geometry in sector/cluster shifts.
//! Sector 11 holds a checksum over sectors 0-10 (skipping VolumeFlags and
//! PercentInUse, which Windows updates in place). A probe accepts a boot
//! sector only after the geometry fields are mutually consistent; the checksum
//! decides between the main and the backup region when the main one is
//! damaged.

use diskrescue_core::fs::FsKind;
use diskrescue_core::util::bytes::{u32le, u64le, utf16le_exact};

use super::{Ctx, FsProbe};

/// Validated exFAT boot sector.
#[derive(Debug, Clone)]
pub(crate) struct ExfatBoot {
    pub bps: u64,
    pub spc_shift: u8,
    pub volume_length: u64,
    pub fat_offset: u64,
    pub heap_offset: u64,
    pub root_cluster: u32,
    pub serial: u32,
    pub cluster_bytes: u64,
}

impl ExfatBoot {
    /// Volume size in bytes.
    pub fn size_bytes(&self) -> u64 {
        self.volume_length.saturating_mul(self.bps)
    }
}

/// Validate the main boot sector. `None` when any hard check fails.
pub(crate) fn parse_boot(s: &[u8]) -> Option<ExfatBoot> {
    if s.len() < 512 || s[510] != 0x55 || s[511] != 0xAA {
        return None;
    }
    if &s[3..11] != b"EXFAT   " || s[11..64].iter().any(|&b| b != 0) {
        return None;
    }
    let bps_shift = s[0x6C];
    let spc_shift = s[0x6D];
    if !(9..=12).contains(&bps_shift) || u32::from(bps_shift) + u32::from(spc_shift) > 25 {
        return None;
    }
    let nfats = s[0x6E];
    if !(1..=2).contains(&nfats) {
        return None;
    }
    let bps = 1u64 << bps_shift;
    let volume_length = u64le(s, 0x48)?;
    let fat_offset = u64::from(u32le(s, 0x50)?);
    let fat_length = u64::from(u32le(s, 0x54)?);
    let heap_offset = u64::from(u32le(s, 0x58)?);
    let cluster_count = u64::from(u32le(s, 0x5C)?);
    let root_cluster = u32le(s, 0x60)?;
    if volume_length == 0 || cluster_count == 0 || cluster_count > 0xFFFF_FFF5 || fat_offset < 24 {
        return None;
    }
    let fats_end = fat_offset.checked_add(u64::from(nfats).checked_mul(fat_length)?)?;
    let heap_end = heap_offset.checked_add(cluster_count.checked_shl(u32::from(spc_shift))?)?;
    if fats_end > heap_offset || heap_end > volume_length {
        return None;
    }
    if fat_length.checked_mul(bps)? < (cluster_count + 2) * 4 {
        return None;
    }
    if u64::from(root_cluster) < 2 || u64::from(root_cluster) > cluster_count + 1 {
        return None;
    }
    Some(ExfatBoot {
        bps,
        spc_shift,
        volume_length,
        fat_offset,
        heap_offset,
        root_cluster,
        serial: u32le(s, 0x64)?,
        cluster_bytes: bps << spc_shift,
    })
}

/// exFAT boot checksum over `region`, which holds sectors 0-10 (11 sectors).
/// VolumeFlags (bytes 106, 107) and PercentInUse (byte 112) are skipped.
pub(crate) fn boot_checksum(region: &[u8]) -> u32 {
    let mut cs: u32 = 0;
    for (i, &b) in region.iter().enumerate() {
        if i == 106 || i == 107 || i == 112 {
            continue;
        }
        cs = (if cs & 1 != 0 { 0x8000_0000u32 } else { 0 })
            .wrapping_add(cs >> 1)
            .wrapping_add(u32::from(b));
    }
    cs
}

/// Outcome of verifying one 12-sector boot region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RegionCheck {
    /// The stored checksum matches.
    Ok,
    /// The stored checksum differs from the computed one.
    Mismatch,
    /// The region could not be read completely.
    Unreadable,
}

/// Verify the boot region starting at sector `first_sector` (0 or 12).
pub(crate) fn check_region(ctx: &Ctx, first_sector: u64, bps: u64) -> RegionCheck {
    let Some(off) = first_sector.checked_mul(bps) else {
        return RegionCheck::Unreadable;
    };
    let len = (12 * bps) as usize;
    match ctx.read(off, len) {
        Some(r) if r.len() == len => {
            let cs = boot_checksum(&r[..(11 * bps) as usize]);
            let stored = u32le(&r, (11 * bps) as usize).unwrap_or(!cs);
            if cs == stored {
                RegionCheck::Ok
            } else {
                RegionCheck::Mismatch
            }
        }
        _ => RegionCheck::Unreadable,
    }
}

/// Look for the volume label entry (type 0x83) in the first part of the root
/// directory.
fn read_label(ctx: &Ctx, b: &ExfatBoot) -> Option<String> {
    let cluster_index = u64::from(b.root_cluster).checked_sub(2)?;
    let sector = b
        .heap_offset
        .checked_add(cluster_index.checked_mul(1u64 << b.spc_shift)?)?;
    let dir = ctx.read(
        sector.checked_mul(b.bps)?,
        4096.min(b.cluster_bytes as usize),
    )?;
    for e in dir.chunks_exact(32) {
        match e[0] {
            0x00 => return None,
            0x83 => {
                let n = usize::from(e[1]).min(11);
                let s = utf16le_exact(&e[2..24], n);
                return if s.is_empty() { None } else { Some(s) };
            }
            _ => {}
        }
    }
    None
}

fn build(ctx: &Ctx, b: &ExfatBoot, via_backup: bool, notes: Vec<String>) -> FsProbe {
    let mut p = FsProbe::new(FsKind::ExFat);
    p.size = Some(b.size_bytes());
    p.cluster_size = u32::try_from(b.cluster_bytes).ok();
    p.via_backup = via_backup;
    p.notes = notes;
    p.notes.push(format!(
        "serial {:04X}-{:04X}",
        b.serial >> 16,
        b.serial & 0xFFFF
    ));
    p.label = read_label(ctx, b);
    if b.size_bytes() > ctx.avail {
        p.notes.push(format!(
            "filesystem claims {} bytes but only {} are available",
            b.size_bytes(),
            ctx.avail
        ));
    }
    p
}

/// Probe the main boot region; fall back to the backup region when the main
/// region's checksum fails and the backup verifies.
pub(crate) fn probe_primary(ctx: &Ctx) -> Option<FsProbe> {
    let boot = parse_boot(ctx.head)?;
    let mut notes = Vec::new();
    match check_region(ctx, 0, boot.bps) {
        RegionCheck::Ok => return Some(build(ctx, &boot, false, notes)),
        RegionCheck::Mismatch => notes.push("boot region checksum mismatch".to_string()),
        RegionCheck::Unreadable => notes.push("boot region is unreadable or truncated".to_string()),
    }
    if let Some(sec) = ctx.read(12 * boot.bps, 512) {
        if let Some(bk) = parse_boot(&sec) {
            if bk.bps == boot.bps && check_region(ctx, 12, bk.bps) == RegionCheck::Ok {
                notes.push("main boot region is damaged; using the backup boot region".to_string());
                return Some(build(ctx, &bk, true, notes));
            }
        }
    }
    Some(build(ctx, &boot, false, notes))
}

/// Probe the backup boot region at sector 12 when sector 0 is unusable.
pub(crate) fn probe_backup(ctx: &Ctx) -> Option<FsProbe> {
    for shift in 9u32..=12 {
        let bps = 1u64 << shift;
        let Some(sec) = ctx.read(12 * bps, 512) else {
            continue;
        };
        let Some(boot) = parse_boot(&sec) else {
            continue;
        };
        if boot.bps != bps {
            continue;
        }
        let mut notes =
            vec!["main boot region is damaged; using the backup boot region".to_string()];
        match check_region(ctx, 12, bps) {
            RegionCheck::Ok => {}
            RegionCheck::Mismatch => notes.push("backup boot region checksum mismatch".to_string()),
            RegionCheck::Unreadable => {
                notes.push("backup boot region is truncated or unreadable".to_string())
            }
        }
        return Some(build(ctx, &boot, true, notes));
    }
    None
}

/// First two FAT entries of a healthy exFAT: `F8 FF FF FF  FF FF FF FF`.
pub(crate) fn fat_header_ok(b: &[u8]) -> bool {
    b.len() >= 8 && b[0] >= 0xF8 && b[1..8].iter().all(|&x| x == 0xFF)
}

/// Return the FAT offset in bytes relative to the volume start.
pub(crate) fn fat_byte_offset(b: &ExfatBoot) -> u64 {
    b.fat_offset.saturating_mul(b.bps)
}

/// Compare two boot sectors ignoring the in-place-updated fields.
pub(crate) fn boot_sectors_equal(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .enumerate()
            .all(|(i, (x, y))| x == y || i == 106 || i == 107 || i == 112)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A minimal valid exFAT main boot sector (512-byte sectors, 8 sectors per cluster).
    pub(crate) fn boot_sector() -> Vec<u8> {
        let mut s = vec![0u8; 512];
        s[0] = 0xEB;
        s[1] = 0x76;
        s[2] = 0x90;
        s[3..11].copy_from_slice(b"EXFAT   ");
        s[0x48..0x50].copy_from_slice(&131072u64.to_le_bytes());
        s[0x50..0x54].copy_from_slice(&2048u32.to_le_bytes());
        s[0x54..0x58].copy_from_slice(&126u32.to_le_bytes());
        s[0x58..0x5C].copy_from_slice(&4096u32.to_le_bytes());
        s[0x5C..0x60].copy_from_slice(&15872u32.to_le_bytes());
        s[0x60..0x64].copy_from_slice(&5u32.to_le_bytes());
        s[0x64..0x68].copy_from_slice(&0x1234_5678u32.to_le_bytes());
        s[0x68..0x6A].copy_from_slice(&0x0100u16.to_le_bytes());
        s[0x6C] = 9;
        s[0x6D] = 3;
        s[0x6E] = 1;
        s[510] = 0x55;
        s[511] = 0xAA;
        s
    }

    #[test]
    fn accepts_valid_and_rejects_inconsistent_geometry() {
        let good = boot_sector();
        let b = parse_boot(&good).unwrap();
        assert_eq!(b.bps, 512);
        assert_eq!(b.cluster_bytes, 4096);
        assert_eq!(b.size_bytes(), 64 << 20);
        assert_eq!(fat_byte_offset(&b), 2048 * 512);

        let mut s = good.clone();
        s[20] = 1; // MustBeZero violated
        assert!(parse_boot(&s).is_none());
        let mut s = good.clone();
        s[0x6C] = 8;
        assert!(parse_boot(&s).is_none());
        let mut s = good.clone();
        s[0x6D] = 20; // 9 + 20 > 25
        assert!(parse_boot(&s).is_none());
        let mut s = good.clone();
        s[0x5C..0x60].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(parse_boot(&s).is_none());
        let mut s = good.clone();
        s[0x60..0x64].copy_from_slice(&1u32.to_le_bytes());
        assert!(parse_boot(&s).is_none());
        let mut s = good.clone();
        s[0x50..0x54].copy_from_slice(&3u32.to_le_bytes());
        assert!(parse_boot(&s).is_none());
        let mut s = good;
        s[3] = b'N';
        assert!(parse_boot(&s).is_none());
    }

    #[test]
    fn checksum_skips_volatile_fields() {
        let mut region = vec![0x11u8; 512 * 11];
        let a = boot_checksum(&region);
        region[106] = 0xFF;
        region[107] = 0x01;
        region[112] = 0x55;
        assert_eq!(boot_checksum(&region), a);
        region[200] ^= 1;
        assert_ne!(boot_checksum(&region), a);
    }

    #[test]
    fn boot_sector_comparison_ignores_percent_in_use() {
        let a = boot_sector();
        let mut b = a.clone();
        b[112] = 42;
        b[106] = 2;
        assert!(boot_sectors_equal(&a, &b));
        b[300] = 1;
        assert!(!boot_sectors_equal(&a, &b));
    }

    #[test]
    fn fat_header_pattern() {
        assert!(fat_header_ok(&[
            0xF8, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF
        ]));
        assert!(!fat_header_ok(&[0x00; 8]));
    }
}
