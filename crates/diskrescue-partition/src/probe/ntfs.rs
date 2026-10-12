//! NTFS boot sector validation.
//!
//! Checks: OEM id `NTFS    `, bytes per sector, the sectors-per-cluster byte
//! (plain powers of two up to 128, or the negative encoding `1 << (256 - v)`
//! used since Windows 10 1709 for clusters above 64 KiB; the decoded cluster
//! must be a power of two of at most 2 MiB), zero reserved/FAT fields, the MFT
//! and MFT mirror cluster numbers inside the volume, the MFT record size
//! (1 KiB to 4 KiB, positive = clusters, negative = `1 << -v` bytes) and the
//! boot signature.
//!
//! The boot sector is duplicated in the last sector of the volume, which is
//! one sector *after* the `total sectors` count, so the partition spans
//! `total_sectors + 1` sectors. [`probe_backup`] looks there when sector 0 is
//! damaged.

use diskrescue_core::fs::FsKind;
use diskrescue_core::util::bytes::{u16le, u32le, u64le};

use super::{Ctx, FsProbe};

/// Validated NTFS boot sector.
#[derive(Debug, Clone)]
pub(crate) struct NtfsBoot {
    pub bps: u64,
    pub spc: u64,
    pub total_sectors: u64,
    pub mft_lcn: u64,
    pub mft_record_size: u64,
    pub serial: u64,
    pub notes: Vec<String>,
}

impl NtfsBoot {
    /// Cluster size in bytes.
    pub fn cluster_bytes(&self) -> u64 {
        self.bps * self.spc
    }

    /// Partition size implied by the boot sector (volume plus backup sector).
    pub fn size_bytes(&self) -> u64 {
        self.total_sectors
            .saturating_add(1)
            .saturating_mul(self.bps)
    }

    /// Byte offset (from the volume start) of the backup boot sector.
    pub fn backup_offset(&self) -> u64 {
        self.total_sectors.saturating_mul(self.bps)
    }

    /// Byte offset of MFT record 0.
    pub fn mft_offset(&self) -> Option<u64> {
        self.mft_lcn.checked_mul(self.cluster_bytes())
    }
}

/// Decode the sectors-per-cluster byte.
pub(crate) fn decode_sectors_per_cluster(v: u8) -> Option<u64> {
    match v {
        1 | 2 | 4 | 8 | 16 | 32 | 64 | 128 => Some(u64::from(v)),
        // Negative encoding: 0xF0..=0xFD (-16..=-3). Anything else is invalid.
        0xF0..=0xFD => Some(1u64 << (256 - u32::from(v))),
        _ => None,
    }
}

/// Decode a "clusters per record" byte (signed): positive counts clusters,
/// negative is the base-2 logarithm of the size in bytes.
fn decode_record_size(v: u8, cluster_bytes: u64) -> Option<u64> {
    let s = v as i8;
    if s > 0 {
        u64::try_from(s).ok()?.checked_mul(cluster_bytes)
    } else if s < 0 {
        let shift = u32::from(s.unsigned_abs());
        if shift > 30 {
            None
        } else {
            Some(1u64 << shift)
        }
    } else {
        None
    }
}

/// Validate a boot sector as NTFS. `None` when any hard check fails.
pub(crate) fn parse_boot(s: &[u8]) -> Option<NtfsBoot> {
    if s.len() < 512 || s[510] != 0x55 || s[511] != 0xAA || &s[3..11] != b"NTFS    " {
        return None;
    }
    let mut notes = Vec::new();
    let bps = u64::from(u16le(s, 11)?);
    if !matches!(bps, 512 | 1024 | 2048 | 4096) {
        return None;
    }
    let spc = decode_sectors_per_cluster(s[13])?;
    let cluster_bytes = bps * spc;
    if cluster_bytes > 2 << 20 {
        return None;
    }
    // Reserved sectors, FAT count, root entries must be zero on NTFS.
    if u16le(s, 14)? != 0 || s[16..19] != [0, 0, 0] || u16le(s, 19)? != 0 {
        return None;
    }
    if u16le(s, 22)? != 0 || u32le(s, 32)? != 0 {
        notes.push("unused BPB fields are not zero".to_string());
    }
    let total_sectors = u64le(s, 0x28)?;
    if total_sectors == 0 {
        return None;
    }
    let total_clusters = total_sectors / spc;
    let mft_lcn = u64le(s, 0x30)?;
    let mftmirr_lcn = u64le(s, 0x38)?;
    if mft_lcn >= total_clusters || mftmirr_lcn >= total_clusters {
        return None;
    }
    let mft_record_size = decode_record_size(s[0x40], cluster_bytes)?;
    if !mft_record_size.is_power_of_two() || !(1024..=4096).contains(&mft_record_size) {
        return None;
    }
    match decode_record_size(s[0x44], cluster_bytes) {
        Some(i) if i.is_power_of_two() && (512..=2 << 20).contains(&i) => {}
        _ => notes.push("unusual index record size".to_string()),
    }
    Some(NtfsBoot {
        bps,
        spc,
        total_sectors,
        mft_lcn,
        mft_record_size,
        serial: u64le(s, 0x48)?,
        notes,
    })
}

fn build(ctx: &Ctx, b: &NtfsBoot, via_backup: bool) -> FsProbe {
    let mut p = FsProbe::new(FsKind::Ntfs);
    p.size = Some(b.size_bytes());
    p.cluster_size = u32::try_from(b.cluster_bytes()).ok();
    p.via_backup = via_backup;
    p.notes.clone_from(&b.notes);
    p.notes.push(format!("serial {:016X}", b.serial));
    p.notes
        .push(format!("MFT record size {} bytes", b.mft_record_size));
    if via_backup {
        p.notes.push(
            "boot sector 0 is damaged; using the backup boot sector at the end of the volume"
                .to_string(),
        );
    }
    if b.size_bytes() > ctx.avail {
        p.notes.push(format!(
            "filesystem claims {} bytes but only {} are available",
            b.size_bytes(),
            ctx.avail
        ));
    }
    p
}

/// Probe the boot sector at the start of the volume.
pub(crate) fn probe_primary(ctx: &Ctx) -> Option<FsProbe> {
    let b = parse_boot(ctx.head)?;
    Some(build(ctx, &b, false))
}

/// Probe the backup boot sector in the last sector of the volume.
pub(crate) fn probe_backup(ctx: &Ctx) -> Option<FsProbe> {
    for bps in [512u64, 1024, 2048, 4096] {
        let sectors = ctx.avail / bps;
        if sectors < 2 {
            continue;
        }
        let pos = (sectors - 1) * bps;
        let Some(sec) = ctx.read(pos, 512) else {
            continue;
        };
        let Some(b) = parse_boot(&sec) else { continue };
        // The backup describes the volume it belongs to: it must sit exactly
        // `total_sectors` sectors after the volume start.
        if b.bps == bps && b.backup_offset() == pos {
            return Some(build(ctx, &b, true));
        }
    }
    None
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A valid NTFS boot sector: 512-byte sectors, 8 sectors per cluster.
    pub(crate) fn boot_sector(total_sectors: u64) -> Vec<u8> {
        let mut s = vec![0u8; 512];
        s[0] = 0xEB;
        s[1] = 0x52;
        s[2] = 0x90;
        s[3..11].copy_from_slice(b"NTFS    ");
        s[11..13].copy_from_slice(&512u16.to_le_bytes());
        s[13] = 8;
        s[21] = 0xF8;
        s[0x24..0x28].copy_from_slice(&0x0080_0080u32.to_le_bytes());
        s[0x28..0x30].copy_from_slice(&total_sectors.to_le_bytes());
        s[0x30..0x38].copy_from_slice(&4u64.to_le_bytes());
        s[0x38..0x40].copy_from_slice(&(total_sectors / 16).to_le_bytes());
        s[0x40] = 0xF6;
        s[0x44] = 1;
        s[0x48..0x50].copy_from_slice(&0xDEAD_BEEF_CAFE_F00Du64.to_le_bytes());
        s[510] = 0x55;
        s[511] = 0xAA;
        s
    }

    #[test]
    fn sectors_per_cluster_decoding() {
        assert_eq!(decode_sectors_per_cluster(1), Some(1));
        assert_eq!(decode_sectors_per_cluster(128), Some(128));
        assert_eq!(decode_sectors_per_cluster(0xF4), Some(4096));
        assert_eq!(decode_sectors_per_cluster(0xF8), Some(256));
        assert_eq!(decode_sectors_per_cluster(0xFD), Some(8));
        assert_eq!(decode_sectors_per_cluster(0), None);
        assert_eq!(decode_sectors_per_cluster(3), None);
        assert_eq!(decode_sectors_per_cluster(0x81), None);
        assert_eq!(decode_sectors_per_cluster(0xFF), None);
        assert_eq!(decode_sectors_per_cluster(0xFE), None);
    }

    #[test]
    fn accepts_valid_boot_sector() {
        let s = boot_sector(32767);
        let b = parse_boot(&s).unwrap();
        assert_eq!(b.cluster_bytes(), 4096);
        assert_eq!(b.size_bytes(), 32768 * 512);
        assert_eq!(b.backup_offset(), 32767 * 512);
        assert_eq!(b.mft_record_size, 1024);
        assert_eq!(b.mft_offset(), Some(4 * 4096));
    }

    #[test]
    fn large_clusters_use_negative_encoding() {
        // 0xF4 => 4096 sectors => 2 MiB clusters at 512 B/sector.
        let mut s = boot_sector(1 << 24);
        s[0x38..0x40].copy_from_slice(&100u64.to_le_bytes());
        s[13] = 0xF4;
        let b = parse_boot(&s).unwrap();
        assert_eq!(b.cluster_bytes(), 2 << 20);
        // 0xF3 would be 4 MiB: too large.
        s[13] = 0xF3;
        assert!(parse_boot(&s).is_none());
    }

    #[test]
    fn rejects_inconsistent_fields() {
        let good = boot_sector(32767);
        let mut s = good.clone();
        s[0x30..0x38].copy_from_slice(&(1u64 << 40).to_le_bytes());
        assert!(parse_boot(&s).is_none(), "MFT outside the volume");
        let mut s = good.clone();
        s[14] = 1; // reserved sectors
        assert!(parse_boot(&s).is_none());
        let mut s = good.clone();
        s[0x40] = 0; // zero record size
        assert!(parse_boot(&s).is_none());
        let mut s = good.clone();
        s[0x40] = 0xE0; // 2^32 byte records
        assert!(parse_boot(&s).is_none());
        let mut s = good.clone();
        s[0x28..0x30].copy_from_slice(&0u64.to_le_bytes());
        assert!(parse_boot(&s).is_none());
        let mut s = good;
        s[3] = b'X';
        assert!(parse_boot(&s).is_none());
    }
}
