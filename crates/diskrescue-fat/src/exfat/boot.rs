//! exFAT boot region: Main Boot Sector fields, validation, the boot
//! checksum sector and the derived geometry.
//!
//! The boot region is sectors 0-11 (backup: 12-23). Sector 11 repeats a
//! 32-bit checksum over sectors 0-10, skipping VolumeFlags and
//! PercentInUse. A region whose boot sector is structurally valid and whose
//! checksum matches is preferred; the main region wins ties.

use diskrescue_core::util::bytes::{u16le, u32le, u64le, u8_at};

use crate::util::exfat_boot_checksum;

/// Main Boot Sector of an exFAT volume, fields named as in the Microsoft
/// exFAT specification (section 3.1).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BootSector {
    /// JumpBoot: `EB 76 90`.
    pub jump_boot: [u8; 3],
    /// FileSystemName: `EXFAT   `.
    pub file_system_name: [u8; 8],
    /// MustBeZero (bytes 0x0B..0x40) was entirely zero.
    pub must_be_zero_is_zero: bool,
    /// PartitionOffset: sector offset of the volume on the medium.
    pub partition_offset: u64,
    /// VolumeLength: volume size in sectors.
    pub volume_length: u64,
    /// FatOffset: sector of the first FAT.
    pub fat_offset: u32,
    /// FatLength: sectors per FAT.
    pub fat_length: u32,
    /// ClusterHeapOffset: sector of cluster 2.
    pub cluster_heap_offset: u32,
    /// ClusterCount: clusters in the heap.
    pub cluster_count: u32,
    /// FirstClusterOfRootDirectory.
    pub first_cluster_of_root_directory: u32,
    /// VolumeSerialNumber.
    pub volume_serial_number: u32,
    /// FileSystemRevision (high byte major, low byte minor).
    pub file_system_revision: u16,
    /// VolumeFlags: bit 0 ActiveFat, 1 VolumeDirty, 2 MediaFailure, 3 ClearToZero.
    pub volume_flags: u16,
    /// BytesPerSectorShift: 9..=12.
    pub bytes_per_sector_shift: u8,
    /// SectorsPerClusterShift.
    pub sectors_per_cluster_shift: u8,
    /// NumberOfFats: 1 (2 on TexFAT).
    pub number_of_fats: u8,
    /// DriveSelect.
    pub drive_select: u8,
    /// PercentInUse (0xFF = unknown).
    pub percent_in_use: u8,
    /// BootSignature: 0xAA55.
    pub boot_signature: u16,
}

/// Derived geometry in bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Geometry {
    pub bytes_per_sector: u64,
    pub cluster_size: u64,
    pub fats: Vec<u64>,
    pub fat_bytes: u64,
    pub heap: u64,
    pub cluster_count: u32,
    pub max_cluster: u32,
    pub root: u32,
    pub total_size: u64,
    pub warnings: Vec<String>,
}

/// True when `b` starts like an exFAT boot sector (name plus zeroed BPB).
pub(crate) fn probe_sector(b: &[u8]) -> bool {
    b.len() >= 512 && &b[3..11] == b"EXFAT   " && b[11..64].iter().all(|&x| x == 0)
}

impl BootSector {
    /// Parse the fields (needs 512 bytes). No validation.
    pub fn parse(b: &[u8]) -> Option<BootSector> {
        if b.len() < 512 {
            return None;
        }
        let mut jump = [0u8; 3];
        jump.copy_from_slice(&b[0..3]);
        let mut name = [0u8; 8];
        name.copy_from_slice(&b[3..11]);
        Some(BootSector {
            jump_boot: jump,
            file_system_name: name,
            must_be_zero_is_zero: b[11..64].iter().all(|&x| x == 0),
            partition_offset: u64le(b, 0x40)?,
            volume_length: u64le(b, 0x48)?,
            fat_offset: u32le(b, 0x50)?,
            fat_length: u32le(b, 0x54)?,
            cluster_heap_offset: u32le(b, 0x58)?,
            cluster_count: u32le(b, 0x5C)?,
            first_cluster_of_root_directory: u32le(b, 0x60)?,
            volume_serial_number: u32le(b, 0x64)?,
            file_system_revision: u16le(b, 0x68)?,
            volume_flags: u16le(b, 0x6A)?,
            bytes_per_sector_shift: u8_at(b, 0x6C)?,
            sectors_per_cluster_shift: u8_at(b, 0x6D)?,
            number_of_fats: u8_at(b, 0x6E)?,
            drive_select: u8_at(b, 0x6F)?,
            percent_in_use: u8_at(b, 0x70)?,
            boot_signature: u16le(b, 0x1FE)?,
        })
    }

    /// Validate the Main Boot Sector fields (not the checksum sector).
    pub fn validate(&self) -> Result<(), String> {
        self.geometry().map(|_| ())
    }

    /// Serial number as `XXXX-XXXX`.
    pub fn serial(&self) -> String {
        format!(
            "{:04X}-{:04X}",
            self.volume_serial_number >> 16,
            self.volume_serial_number & 0xFFFF
        )
    }

    /// Validate and derive the geometry (spec 2.3.1 of the filesystems
    /// research notes). Inconsistent cluster counts are clipped with a
    /// warning; anything that makes the layout meaningless is an error.
    pub(crate) fn geometry(&self) -> Result<Geometry, String> {
        let mut warnings = Vec::new();
        if &self.file_system_name != b"EXFAT   " {
            return Err("file system name is not EXFAT".into());
        }
        if !self.must_be_zero_is_zero {
            return Err("MustBeZero region is not zero".into());
        }
        if self.boot_signature != 0xAA55 {
            return Err(format!(
                "boot signature is {:#06x}, not 0xaa55",
                self.boot_signature
            ));
        }
        let bps_shift = self.bytes_per_sector_shift as u32;
        if !(9..=12).contains(&bps_shift) {
            return Err(format!("BytesPerSectorShift {bps_shift} is not 9..12"));
        }
        let spc_shift = self.sectors_per_cluster_shift as u32;
        if bps_shift + spc_shift > 25 {
            return Err(format!(
                "cluster size 2^{} exceeds 32 MiB",
                bps_shift + spc_shift
            ));
        }
        if !matches!(self.number_of_fats, 1 | 2) {
            return Err(format!(
                "NumberOfFats {} is not 1 or 2",
                self.number_of_fats
            ));
        }
        if self.fat_offset < 24 {
            return Err(format!(
                "FatOffset {} is inside the boot regions",
                self.fat_offset
            ));
        }
        if self.fat_length == 0 {
            return Err("FatLength is 0".into());
        }
        let fat_end = self.fat_offset as u64 + self.number_of_fats as u64 * self.fat_length as u64;
        if fat_end > self.cluster_heap_offset as u64 {
            return Err("FAT overlaps the cluster heap".into());
        }
        if self.cluster_heap_offset as u64 >= self.volume_length {
            return Err("cluster heap starts beyond the volume".into());
        }
        if self.cluster_count == 0 {
            return Err("ClusterCount is 0".into());
        }
        let bps = 1u64 << bps_shift;
        let cs = bps << spc_shift;
        let mut count = self.cluster_count as u64;
        let fits = (self.volume_length - self.cluster_heap_offset as u64) >> spc_shift;
        if count > fits {
            warnings.push(format!(
                "ClusterCount {count} exceeds what VolumeLength holds ({fits}); clipped"
            ));
            count = fits;
        }
        let fat_entries = self.fat_length as u64 * bps / 4;
        if count + 2 > fat_entries {
            let c = fat_entries.saturating_sub(2);
            warnings.push(format!(
                "the FAT holds only {fat_entries} entries but {count} clusters are claimed; clipped to {c}"
            ));
            count = c;
        }
        if count > 0xFFFF_FFF5 {
            count = 0xFFFF_FFF5;
        }
        if count == 0 {
            return Err("no usable clusters".into());
        }
        let max_cluster = (count + 1) as u32;
        let root = self.first_cluster_of_root_directory;
        if root < 2 || root > max_cluster {
            warnings.push(format!(
                "root directory cluster {root} is out of range; the root cannot be listed (orphan scan may find its contents)"
            ));
        }
        let fat_bytes = self.fat_length as u64 * bps;
        let first_fat = self.fat_offset as u64 * bps;
        let mut fats = vec![first_fat];
        if self.number_of_fats == 2 {
            warnings.push("TexFAT volume with two FATs; using the active FAT".to_string());
            let second = first_fat + fat_bytes;
            if self.volume_flags & 1 == 1 {
                fats.insert(0, second);
            } else {
                fats.push(second);
            }
        }
        if self.volume_flags & 0x2 != 0 {
            warnings.push(
                "volume was not cleanly unmounted (VolumeDirty set); expect torn metadata"
                    .to_string(),
            );
        }
        if self.volume_flags & 0x4 != 0 {
            warnings.push(
                "the driver recorded media failures on this volume (MediaFailure set)".to_string(),
            );
        }
        if self.file_system_revision >> 8 != 1 {
            warnings.push(format!(
                "unknown exFAT revision {}.{:02}",
                self.file_system_revision >> 8,
                self.file_system_revision & 0xFF
            ));
        }
        Ok(Geometry {
            bytes_per_sector: bps,
            cluster_size: cs,
            fats,
            fat_bytes,
            heap: self.cluster_heap_offset as u64 * bps,
            cluster_count: count as u32,
            max_cluster,
            root,
            total_size: self.volume_length.saturating_mul(bps),
            warnings,
        })
    }
}

/// Verify the checksum sector of a boot region (`region` = 12 sectors).
pub(crate) fn region_checksum_ok(region: &[u8], bps: usize) -> bool {
    if region.len() < 12 * bps {
        return false;
    }
    let cs = exfat_boot_checksum(&region[..11 * bps]);
    region[11 * bps..12 * bps]
        .chunks_exact(4)
        .all(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]) == cs)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A 12-sector main boot region like mkfs.exfat makes for 64 MiB.
    pub(crate) fn boot_region() -> Vec<u8> {
        let bps = 512usize;
        let mut r = vec![0u8; 12 * bps];
        let b = &mut r[..512];
        b[0..3].copy_from_slice(&[0xEB, 0x76, 0x90]);
        b[3..11].copy_from_slice(b"EXFAT   ");
        b[0x48..0x50].copy_from_slice(&131_072u64.to_le_bytes());
        b[0x50..0x54].copy_from_slice(&2048u32.to_le_bytes());
        b[0x54..0x58].copy_from_slice(&126u32.to_le_bytes());
        b[0x58..0x5C].copy_from_slice(&4096u32.to_le_bytes());
        b[0x5C..0x60].copy_from_slice(&15_872u32.to_le_bytes());
        b[0x60..0x64].copy_from_slice(&5u32.to_le_bytes());
        b[0x64..0x68].copy_from_slice(&0xEACE_9663u32.to_le_bytes());
        b[0x68..0x6A].copy_from_slice(&0x0100u16.to_le_bytes());
        b[0x6C] = 9;
        b[0x6D] = 3;
        b[0x6E] = 1;
        b[0x6F] = 0x80;
        b[0x1FE] = 0x55;
        b[0x1FF] = 0xAA;
        for s in 1..9 {
            r[s * bps + 510] = 0x55;
            r[s * bps + 511] = 0xAA;
        }
        let cs = exfat_boot_checksum(&r[..11 * bps]);
        for k in 0..bps / 4 {
            r[11 * bps + 4 * k..11 * bps + 4 * k + 4].copy_from_slice(&cs.to_le_bytes());
        }
        r
    }

    #[test]
    fn parses_and_validates() {
        let r = boot_region();
        assert!(probe_sector(&r));
        assert!(region_checksum_ok(&r, 512));
        let bs = BootSector::parse(&r).unwrap();
        let g = bs.geometry().unwrap();
        assert_eq!(g.cluster_size, 4096);
        assert_eq!(g.heap, 4096 * 512);
        assert_eq!(g.max_cluster, 15_873);
        assert_eq!(bs.serial(), "EACE-9663");
        assert!(g.warnings.is_empty());
        // Flags and PercentInUse are excluded from the checksum.
        let mut r2 = r.clone();
        r2[106] = 2;
        r2[112] = 50;
        assert!(region_checksum_ok(&r2, 512));
        r2[0x64] ^= 1;
        assert!(!region_checksum_ok(&r2, 512));
    }

    #[test]
    fn clips_and_rejects() {
        let mut r = boot_region();
        r[0x5C..0x60].copy_from_slice(&1_000_000u32.to_le_bytes());
        let g = BootSector::parse(&r).unwrap().geometry().unwrap();
        assert!(g.cluster_count < 1_000_000);
        assert!(!g.warnings.is_empty());
        let mut r = boot_region();
        r[0x6C] = 13;
        assert!(BootSector::parse(&r).unwrap().geometry().is_err());
        let mut r = boot_region();
        r[20] = 1;
        assert!(BootSector::parse(&r).unwrap().geometry().is_err());
        assert!(!probe_sector(&r));
    }
}
