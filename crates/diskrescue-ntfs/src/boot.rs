//! The NTFS boot sector (`$Boot`, sector 0 of the volume; a byte-identical
//! backup lives in the last sector of the partition).
//!
//! Only the BIOS parameter block fields NTFS actually uses are decoded. Two
//! fields use compact encodings that are easy to get wrong:
//!
//! * **Sectors per cluster** (0x0D, `u8`): `0x01..=0x80` is the literal
//!   count (a power of two). Values `0xF0..=0xFD` are negative-encoded and
//!   mean `2^(256 - value)` sectors; Windows 10 1709+ writes them for clusters
//!   larger than 64 KiB (`0xF4` = 4096 sectors = 2 MiB with 512-byte
//!   sectors). This is the union of what ntfs-3g and the Linux ntfs3 driver
//!   accept. Every other value is invalid.
//! * **Clusters per MFT record / index record** (0x40 / 0x44, `i8`): a
//!   positive value is a cluster count, a negative value `-n` means `2^n`
//!   bytes (`0xF6` = -10 = 1024 bytes).
//!
//! Validation is deliberately lenient about fields NTFS ignores (CHS
//! geometry, hidden sectors) and strict about the ones that steer reads
//! (geometry, MFT location, record sizes), so a damaged sector 0 is rejected
//! and the backup can be tried instead.

use diskrescue_core::util::bytes::{u16le, u64le, u8_at};

/// The OEM identifier every NTFS boot sector carries at offset 3.
pub const OEM_ID: &[u8; 8] = b"NTFS    ";

/// Largest cluster size NTFS supports (Windows 10 1709+).
pub const MAX_CLUSTER_SIZE: u64 = 2 * 1024 * 1024;

/// Smallest and largest accepted MFT record / index record size.
pub const MIN_RECORD_SIZE: u32 = 512;
/// See [`MIN_RECORD_SIZE`].
pub const MAX_RECORD_SIZE: u32 = 65536;

/// Decoded and validated NTFS boot sector.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BootSector {
    /// Bytes per logical sector (512, 1024, 2048 or 4096).
    pub bytes_per_sector: u32,
    /// Raw byte at 0x0D.
    pub sectors_per_cluster_raw: u8,
    /// Decoded sectors per cluster (see the module docs for the encoding).
    pub sectors_per_cluster: u32,
    /// Cluster size in bytes.
    pub cluster_size: u32,
    /// Number of sectors in the volume (the backup boot sector sits in the
    /// sector after the last one counted here).
    pub total_sectors: u64,
    /// Cluster holding MFT record 0.
    pub mft_lcn: u64,
    /// Cluster holding `$MFTMirr` (copy of the first MFT records).
    pub mftmirr_lcn: u64,
    /// Raw signed byte at 0x40.
    pub clusters_per_mft_record_raw: i8,
    /// Raw signed byte at 0x44.
    pub clusters_per_index_record_raw: i8,
    /// MFT record size in bytes (1024 typically, 4096 on 4Kn media).
    pub mft_record_size: u32,
    /// Index (INDX) record size in bytes, 4096 typically. Falls back to 4096
    /// when the field is damaged; directories also carry their own value.
    pub index_record_size: u32,
    /// Volume serial number.
    pub serial: u64,
    /// Whether the `55 AA` end-of-sector marker was present.
    pub has_signature: bool,
    /// Byte offset (volume-relative) this copy was read from.
    pub offset: u64,
    /// True when this is the backup copy from the end of the volume.
    pub is_backup: bool,
    /// Non-fatal oddities noticed while parsing.
    pub warnings: Vec<String>,
}

impl BootSector {
    /// Parse and validate a boot sector image (at least 512 bytes).
    ///
    /// `offset` and `is_backup` are recorded for reporting only.
    pub fn parse(b: &[u8], offset: u64, is_backup: bool) -> Result<BootSector, String> {
        if b.len() < 512 {
            return Err(format!("boot sector too short ({} bytes)", b.len()));
        }
        if &b[3..11] != OEM_ID {
            return Err("OEM id is not \"NTFS    \"".into());
        }
        let bps = u16le(b, 0x0B).unwrap_or(0) as u32;
        if !matches!(bps, 512 | 1024 | 2048 | 4096) {
            return Err(format!("invalid bytes per sector {bps}"));
        }
        let spc_raw = u8_at(b, 0x0D).unwrap_or(0);
        let spc = decode_sectors_per_cluster(spc_raw)
            .ok_or_else(|| format!("invalid sectors per cluster byte {spc_raw:#04x}"))?;
        let cluster = bps as u64 * spc as u64;
        if cluster > MAX_CLUSTER_SIZE {
            return Err(format!("cluster size {cluster} exceeds 2 MiB"));
        }
        let cluster = cluster as u32;
        let total_sectors = u64le(b, 0x28).unwrap_or(0);
        if total_sectors == 0 || total_sectors > (u64::MAX >> 13) {
            return Err(format!("invalid total sector count {total_sectors}"));
        }
        let total_clusters = total_sectors / spc as u64;
        if total_clusters == 0 {
            return Err("volume smaller than one cluster".into());
        }
        let mft_lcn = u64le(b, 0x30).unwrap_or(u64::MAX);
        let mftmirr_lcn = u64le(b, 0x38).unwrap_or(u64::MAX);
        if mft_lcn >= total_clusters {
            return Err(format!(
                "$MFT cluster {mft_lcn} outside the volume ({total_clusters} clusters)"
            ));
        }
        if mftmirr_lcn >= total_clusters {
            return Err(format!(
                "$MFTMirr cluster {mftmirr_lcn} outside the volume ({total_clusters} clusters)"
            ));
        }
        let cpmr = u8_at(b, 0x40).unwrap_or(0) as i8;
        let mft_record_size = decode_record_size(cpmr, cluster)
            .ok_or_else(|| format!("invalid clusters per MFT record byte {cpmr}"))?;
        let cpir = u8_at(b, 0x44).unwrap_or(0) as i8;
        let mut warnings = Vec::new();
        let index_record_size = match decode_record_size(cpir, cluster) {
            Some(v) => v,
            None => {
                warnings.push(format!(
                    "invalid clusters per index record byte {cpir}; assuming 4096"
                ));
                4096
            }
        };
        let has_signature = b[510] == 0x55 && b[511] == 0xAA;
        if !has_signature {
            warnings.push("boot sector lacks the 55 AA signature".into());
        }
        Ok(BootSector {
            bytes_per_sector: bps,
            sectors_per_cluster_raw: spc_raw,
            sectors_per_cluster: spc,
            cluster_size: cluster,
            total_sectors,
            mft_lcn,
            mftmirr_lcn,
            clusters_per_mft_record_raw: cpmr,
            clusters_per_index_record_raw: cpir,
            mft_record_size,
            index_record_size,
            serial: u64le(b, 0x48).unwrap_or(0),
            has_signature,
            offset,
            is_backup,
            warnings,
        })
    }

    /// Number of clusters in the volume.
    pub fn total_clusters(&self) -> u64 {
        self.total_sectors / self.sectors_per_cluster.max(1) as u64
    }

    /// Volume size in bytes as the boot sector states it (not counting the
    /// backup boot sector that follows).
    pub fn volume_size(&self) -> u64 {
        self.total_sectors
            .saturating_mul(self.bytes_per_sector as u64)
    }

    /// Volume serial number as 16 upper-case hex digits.
    pub fn serial_string(&self) -> String {
        format!("{:016X}", self.serial)
    }
}

/// Decode the sectors-per-cluster byte (see the module docs).
pub fn decode_sectors_per_cluster(raw: u8) -> Option<u32> {
    match raw {
        1..=0x80 if raw.is_power_of_two() => Some(raw as u32),
        0xF0..=0xFD => Some(1u32 << (256 - raw as u32)),
        _ => None,
    }
}

/// Decode a clusters-per-record byte into a size in bytes. Positive values
/// count clusters, negative values `-n` mean `2^n` bytes. The result must be
/// a power of two in `512..=65536`.
pub fn decode_record_size(raw: i8, cluster_size: u32) -> Option<u32> {
    let size: u64 = if raw > 0 {
        raw as u64 * cluster_size as u64
    } else if raw < 0 {
        let shift = -(raw as i32);
        if shift > 31 {
            return None;
        }
        1u64 << shift
    } else {
        return None;
    };
    if size.is_power_of_two() && (MIN_RECORD_SIZE as u64..=MAX_RECORD_SIZE as u64).contains(&size) {
        Some(size as u32)
    } else {
        None
    }
}

/// Cheap check whether `head` (the first bytes of a volume) looks like an
/// NTFS boot sector: OEM id, valid geometry and the `55 AA` marker.
pub fn probe(head: &[u8]) -> bool {
    match BootSector::parse(head, 0, false) {
        Ok(b) => b.has_signature,
        Err(_) => false,
    }
}

#[cfg(test)]
pub(crate) fn sample_boot_sector() -> Vec<u8> {
    let mut b = vec![0u8; 512];
    b[0..3].copy_from_slice(&[0xEB, 0x52, 0x90]);
    b[3..11].copy_from_slice(OEM_ID);
    b[0x0B..0x0D].copy_from_slice(&512u16.to_le_bytes());
    b[0x0D] = 8;
    b[0x15] = 0xF8;
    b[0x28..0x30].copy_from_slice(&131071u64.to_le_bytes());
    b[0x30..0x38].copy_from_slice(&4u64.to_le_bytes());
    b[0x38..0x40].copy_from_slice(&8191u64.to_le_bytes());
    b[0x40] = 0xF6;
    b[0x44] = 1;
    b[0x48..0x50].copy_from_slice(&0x1234_5678_9ABC_DEF0u64.to_le_bytes());
    b[510] = 0x55;
    b[511] = 0xAA;
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_mkntfs_style_sector() {
        let b = sample_boot_sector();
        let bs = BootSector::parse(&b, 0, false).unwrap();
        assert_eq!(bs.cluster_size, 4096);
        assert_eq!(bs.mft_record_size, 1024);
        assert_eq!(bs.index_record_size, 4096);
        assert_eq!(bs.total_clusters(), 131071 / 8);
        assert_eq!(bs.serial_string(), "123456789ABCDEF0");
        assert!(probe(&b));
    }

    #[test]
    fn sectors_per_cluster_encoding() {
        assert_eq!(decode_sectors_per_cluster(1), Some(1));
        assert_eq!(decode_sectors_per_cluster(0x80), Some(128));
        assert_eq!(decode_sectors_per_cluster(3), None);
        assert_eq!(decode_sectors_per_cluster(0), None);
        assert_eq!(decode_sectors_per_cluster(0x81), None);
        assert_eq!(decode_sectors_per_cluster(0xF4), Some(4096));
        assert_eq!(decode_sectors_per_cluster(0xF8), Some(256));
        assert_eq!(decode_sectors_per_cluster(0xF9), Some(128));
        assert_eq!(decode_sectors_per_cluster(0xF0), Some(65536));
        assert_eq!(decode_sectors_per_cluster(0xFE), None);
        assert_eq!(decode_sectors_per_cluster(0xFF), None);
    }

    #[test]
    fn negative_sectors_per_cluster_gives_two_mib_clusters() {
        let mut b = sample_boot_sector();
        b[0x0D] = 0xF4;
        // 2 MiB clusters: shrink the volume numbers so the LCNs stay inside.
        b[0x28..0x30].copy_from_slice(&(4096u64 * 64).to_le_bytes());
        b[0x30..0x38].copy_from_slice(&2u64.to_le_bytes());
        b[0x38..0x40].copy_from_slice(&32u64.to_le_bytes());
        b[0x44] = 0xF4; // -12 => 4096 bytes
        let bs = BootSector::parse(&b, 0, false).unwrap();
        assert_eq!(bs.sectors_per_cluster, 4096);
        assert_eq!(bs.cluster_size, 2 * 1024 * 1024);
        assert_eq!(bs.mft_record_size, 1024);
        assert_eq!(bs.index_record_size, 4096);
        assert_eq!(bs.total_clusters(), 64);
        // 4 KiB sectors with 0xF4 would be 16 MiB clusters: rejected.
        b[0x0B..0x0D].copy_from_slice(&4096u16.to_le_bytes());
        assert!(BootSector::parse(&b, 0, false).is_err());
    }

    #[test]
    fn record_size_encoding() {
        assert_eq!(decode_record_size(-10, 4096), Some(1024));
        assert_eq!(decode_record_size(-12, 512), Some(4096));
        assert_eq!(decode_record_size(2, 512), Some(1024));
        assert_eq!(decode_record_size(1, 4096), Some(4096));
        assert_eq!(decode_record_size(1, 2 * 1024 * 1024), None);
        assert_eq!(decode_record_size(0, 4096), None);
        assert_eq!(decode_record_size(-128, 4096), None);
        assert_eq!(decode_record_size(-8, 4096), None);
        assert_eq!(decode_record_size(3, 512), None);
    }

    #[test]
    fn rejects_garbage() {
        let mut b = sample_boot_sector();
        b[3] = b'X';
        assert!(BootSector::parse(&b, 0, false).is_err());
        let mut b = sample_boot_sector();
        b[0x30..0x38].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(BootSector::parse(&b, 0, false).is_err());
        let mut b = sample_boot_sector();
        b[0x40] = 0;
        assert!(BootSector::parse(&b, 0, false).is_err());
        let mut b = sample_boot_sector();
        b[510] = 0;
        assert!(!probe(&b));
        assert!(BootSector::parse(&b, 0, false).is_ok());
        assert!(!probe(&b[..100]));
        assert!(!probe(&[]));
    }
}
