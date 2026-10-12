//! FAT12/16/32 boot sector (BPB) parsing, validation and the derived
//! geometry (Microsoft FAT specification, fatgen103).
//!
//! The FAT width follows the Microsoft cluster-count rule (fewer than 4085
//! clusters is FAT12, fewer than 65525 FAT16, else FAT32), with one
//! override: a BPB in the FAT32 layout (`BPB_FATSz16 == 0`, no fixed root
//! directory) is always FAT32, because reading it any other way leaves no
//! root directory at all (this matches what the Linux driver does).
//! Conversely a FAT12/16-layout BPB that claims 65525 or more clusters is
//! treated as FAT16 with the cluster count clipped.

use diskrescue_core::fs::FsKind;
use diskrescue_core::util::bytes::{u16le, u32le, u8_at};

use crate::util::decode_oem;

/// Every field of a FAT boot sector, named as in the Microsoft
/// specification (`BS_` = boot sector, `BPB_` = BIOS parameter block).
/// FAT32-only fields are zero on FAT12/16 volumes; the extended fields
/// (`bs_drv_num` .. `bs_fil_sys_type`) are read from offset 0x24 or 0x40
/// depending on the layout.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BootSector {
    /// BS_jmpBoot: x86 jump (`EB xx 90` or `E9 xx xx`).
    pub bs_jmp_boot: [u8; 3],
    /// BS_OEMName: formatting tool name.
    pub bs_oem_name: [u8; 8],
    /// BPB_BytsPerSec: 512, 1024, 2048 or 4096.
    pub bpb_byts_per_sec: u16,
    /// BPB_SecPerClus: power of two, 1..=128.
    pub bpb_sec_per_clus: u8,
    /// BPB_RsvdSecCnt: sectors before the first FAT.
    pub bpb_rsvd_sec_cnt: u16,
    /// BPB_NumFATs: number of FAT copies.
    pub bpb_num_fats: u8,
    /// BPB_RootEntCnt: entries of the fixed root directory (0 on FAT32).
    pub bpb_root_ent_cnt: u16,
    /// BPB_TotSec16: total sectors when < 65536 (else 0).
    pub bpb_tot_sec16: u16,
    /// BPB_Media: media descriptor.
    pub bpb_media: u8,
    /// BPB_FATSz16: sectors per FAT on FAT12/16 (0 on FAT32).
    pub bpb_fat_sz16: u16,
    /// BPB_SecPerTrk: CHS geometry (ignored).
    pub bpb_sec_per_trk: u16,
    /// BPB_NumHeads: CHS geometry (ignored).
    pub bpb_num_heads: u16,
    /// BPB_HiddSec: sectors preceding the volume.
    pub bpb_hidd_sec: u32,
    /// BPB_TotSec32: total sectors when BPB_TotSec16 is 0.
    pub bpb_tot_sec32: u32,
    /// BPB_FATSz32: sectors per FAT (FAT32).
    pub bpb_fat_sz32: u32,
    /// BPB_ExtFlags: bit 7 = mirroring disabled, bits 0-3 = active FAT (FAT32).
    pub bpb_ext_flags: u16,
    /// BPB_FSVer: must be 0 (FAT32).
    pub bpb_fs_ver: u16,
    /// BPB_RootClus: first cluster of the root directory (FAT32).
    pub bpb_root_clus: u32,
    /// BPB_FSInfo: sector of the FSInfo structure (FAT32).
    pub bpb_fs_info: u16,
    /// BPB_BkBootSec: sector of the backup boot sector (FAT32, usually 6).
    pub bpb_bk_boot_sec: u16,
    /// BS_DrvNum: BIOS drive number.
    pub bs_drv_num: u8,
    /// BS_Reserved1.
    pub bs_reserved1: u8,
    /// BS_BootSig: 0x29 when the next three fields are valid.
    pub bs_boot_sig: u8,
    /// BS_VolID: volume serial number.
    pub bs_vol_id: u32,
    /// BS_VolLab: volume label, space padded.
    pub bs_vol_lab: [u8; 11],
    /// BS_FilSysType: informational type string ("FAT12   ", ...).
    pub bs_fil_sys_type: [u8; 8],
    /// Signature_word at 0x1FE: 0xAA55.
    pub signature_word: u16,
}

/// FAT32 FSInfo sector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FsInfo {
    /// FSI_LeadSig: 0x41615252.
    pub fsi_lead_sig: u32,
    /// FSI_StrucSig: 0x61417272.
    pub fsi_struc_sig: u32,
    /// FSI_Free_Count: last known free cluster count (0xFFFFFFFF = unknown).
    pub fsi_free_count: u32,
    /// FSI_Nxt_Free: hint where the driver allocated last.
    pub fsi_nxt_free: u32,
    /// FSI_TrailSig: 0xAA550000.
    pub fsi_trail_sig: u32,
}

impl FsInfo {
    /// Parse a 512-byte FSInfo sector; `None` when the signatures are wrong.
    pub fn parse(b: &[u8]) -> Option<FsInfo> {
        let fi = FsInfo {
            fsi_lead_sig: u32le(b, 0)?,
            fsi_struc_sig: u32le(b, 0x1E4)?,
            fsi_free_count: u32le(b, 0x1E8)?,
            fsi_nxt_free: u32le(b, 0x1EC)?,
            fsi_trail_sig: u32le(b, 0x1FC)?,
        };
        if fi.fsi_lead_sig == 0x4161_5252
            && fi.fsi_struc_sig == 0x6141_7272
            && fi.fsi_trail_sig == 0xAA55_0000
        {
            Some(fi)
        } else {
            None
        }
    }

    /// Free cluster count when the driver recorded one.
    pub fn free_count(&self) -> Option<u32> {
        (self.fsi_free_count != 0xFFFF_FFFF).then_some(self.fsi_free_count)
    }
}

impl BootSector {
    /// Parse the fields of a boot sector (at least 512 bytes). No
    /// validation happens here; see [`BootSector::validate`].
    pub fn parse(b: &[u8]) -> Option<BootSector> {
        if b.len() < 512 {
            return None;
        }
        let fat_sz16 = u16le(b, 0x16)?;
        let root_ent = u16le(b, 0x11)?;
        let fat32 = fat_sz16 == 0 && root_ent == 0;
        let ext = if fat32 { 0x40 } else { 0x24 };
        let mut jmp = [0u8; 3];
        jmp.copy_from_slice(&b[0..3]);
        let mut oem = [0u8; 8];
        oem.copy_from_slice(&b[3..11]);
        let mut lab = [0u8; 11];
        lab.copy_from_slice(&b[ext + 7..ext + 18]);
        let mut typ = [0u8; 8];
        typ.copy_from_slice(&b[ext + 18..ext + 26]);
        let f32v = |off: usize| if fat32 { u32le(b, off) } else { Some(0) };
        let f16v = |off: usize| if fat32 { u16le(b, off) } else { Some(0) };
        Some(BootSector {
            bs_jmp_boot: jmp,
            bs_oem_name: oem,
            bpb_byts_per_sec: u16le(b, 0x0B)?,
            bpb_sec_per_clus: u8_at(b, 0x0D)?,
            bpb_rsvd_sec_cnt: u16le(b, 0x0E)?,
            bpb_num_fats: u8_at(b, 0x10)?,
            bpb_root_ent_cnt: root_ent,
            bpb_tot_sec16: u16le(b, 0x13)?,
            bpb_media: u8_at(b, 0x15)?,
            bpb_fat_sz16: fat_sz16,
            bpb_sec_per_trk: u16le(b, 0x18)?,
            bpb_num_heads: u16le(b, 0x1A)?,
            bpb_hidd_sec: u32le(b, 0x1C)?,
            bpb_tot_sec32: u32le(b, 0x20)?,
            bpb_fat_sz32: f32v(0x24)?,
            bpb_ext_flags: f16v(0x28)?,
            bpb_fs_ver: f16v(0x2A)?,
            bpb_root_clus: f32v(0x2C)?,
            bpb_fs_info: f16v(0x30)?,
            bpb_bk_boot_sec: f16v(0x32)?,
            bs_drv_num: u8_at(b, ext)?,
            bs_reserved1: u8_at(b, ext + 1)?,
            bs_boot_sig: u8_at(b, ext + 2)?,
            bs_vol_id: u32le(b, ext + 3)?,
            bs_vol_lab: lab,
            bs_fil_sys_type: typ,
            signature_word: u16le(b, 0x1FE)?,
        })
    }

    /// Validate the boot sector against the Microsoft rules and return the
    /// FAT width it describes (cluster-count rule, FAT32 layout override).
    pub fn validate(&self) -> Result<FsKind, String> {
        self.geometry().map(|g| g.kind)
    }

    /// True when the BPB uses the FAT32 layout (no fixed root directory,
    /// 32-bit FAT size field).
    pub fn is_fat32_layout(&self) -> bool {
        self.bpb_fat_sz16 == 0 && self.bpb_root_ent_cnt == 0
    }

    /// OEM name as text.
    pub fn oem_name(&self) -> String {
        decode_oem(&self.bs_oem_name).trim_end().to_string()
    }

    /// Label from the boot sector (`None` for blank or "NO NAME").
    pub fn label(&self) -> Option<String> {
        if self.bs_boot_sig != 0x29 {
            return None;
        }
        let l = decode_oem(&self.bs_vol_lab).trim_end().to_string();
        if l.is_empty() || l == "NO NAME" {
            None
        } else {
            Some(l)
        }
    }

    /// Serial number as `XXXX-XXXX` when the extended boot signature is present.
    pub fn serial(&self) -> Option<String> {
        if self.bs_boot_sig == 0x29 || self.bs_boot_sig == 0x28 {
            Some(format!(
                "{:04X}-{:04X}",
                self.bs_vol_id >> 16,
                self.bs_vol_id & 0xFFFF
            ))
        } else {
            None
        }
    }

    /// Validate the boot sector and derive the volume geometry. Returns the
    /// reason when the sector cannot describe a FAT volume.
    pub(crate) fn geometry(&self) -> Result<Geometry, String> {
        let mut warnings = Vec::new();
        if self.signature_word != 0xAA55 {
            return Err(format!(
                "boot signature is {:#06x}, not 0xaa55",
                self.signature_word
            ));
        }
        if self.bs_jmp_boot[0] != 0xEB && self.bs_jmp_boot[0] != 0xE9 {
            return Err(format!(
                "jump byte {:#04x} is not EB/E9",
                self.bs_jmp_boot[0]
            ));
        }
        let bps = self.bpb_byts_per_sec as u64;
        if !matches!(bps, 512 | 1024 | 2048 | 4096) {
            return Err(format!("bytes per sector {bps} is not 512..4096"));
        }
        let spc = self.bpb_sec_per_clus as u64;
        if spc == 0 || !spc.is_power_of_two() || spc > 128 {
            return Err(format!(
                "sectors per cluster {spc} is not a power of two <= 128"
            ));
        }
        let cluster = bps * spc;
        if cluster > 32 * 1024 {
            warnings.push(format!(
                "cluster size {cluster} exceeds the 32 KiB the specification allows"
            ));
        }
        if self.bpb_rsvd_sec_cnt == 0 {
            return Err("reserved sector count is 0".into());
        }
        if !matches!(self.bpb_num_fats, 1 | 2) {
            return Err(format!(
                "number of FATs {} is not 1 or 2",
                self.bpb_num_fats
            ));
        }
        if !(self.bpb_media == 0xF0 || self.bpb_media >= 0xF8) {
            warnings.push(format!("unusual media descriptor {:#04x}", self.bpb_media));
        }
        let fat32_layout = self.is_fat32_layout();
        let fat_sz = if self.bpb_fat_sz16 != 0 {
            self.bpb_fat_sz16 as u64
        } else {
            self.bpb_fat_sz32 as u64
        };
        let tot_sec = if self.bpb_tot_sec16 != 0 {
            self.bpb_tot_sec16 as u64
        } else {
            self.bpb_tot_sec32 as u64
        };
        if tot_sec == 0 {
            return Err("total sector count is 0".into());
        }
        if fat_sz == 0 {
            return Err("FAT size is 0".into());
        }
        if fat32_layout {
            if self.bpb_tot_sec16 != 0 {
                return Err("FAT32 boot sector with a 16-bit sector count".into());
            }
            if self.bpb_fs_ver != 0 {
                warnings.push(format!("unknown FAT32 version {:#06x}", self.bpb_fs_ver));
            }
        } else if self.bpb_root_ent_cnt == 0 {
            return Err("FAT12/16 boot sector without root directory entries".into());
        }
        let root_dir_sectors = (self.bpb_root_ent_cnt as u64 * 32).div_ceil(bps);
        let first_data_sector =
            self.bpb_rsvd_sec_cnt as u64 + self.bpb_num_fats as u64 * fat_sz + root_dir_sectors;
        if first_data_sector >= tot_sec {
            return Err(format!(
                "data area starts at sector {first_data_sector}, beyond the {tot_sec} sectors of the volume"
            ));
        }
        let mut count = (tot_sec - first_data_sector) / spc;
        if count == 0 {
            return Err("volume has no clusters".into());
        }
        let kind = if fat32_layout {
            if count < 65_525 {
                warnings.push(format!(
                    "FAT32 layout with only {count} clusters (the cluster-count rule says FAT{}); read as FAT32",
                    if count < 4085 { 12 } else { 16 }
                ));
            }
            FsKind::Fat32
        } else if count < 4085 {
            FsKind::Fat12
        } else {
            if count >= 65_525 {
                warnings.push(format!(
                    "FAT16 layout claims {count} clusters; clipped to 65524"
                ));
                count = 65_524;
            }
            FsKind::Fat16
        };
        if kind == FsKind::Fat32 && count > 0x0FFF_FFF5 {
            warnings.push(format!(
                "FAT32 claims {count} clusters; clipped to 268435445"
            ));
            count = 0x0FFF_FFF5;
        }
        // The FAT must be able to describe every cluster.
        let fat_bytes = fat_sz * bps;
        let capacity = match kind {
            FsKind::Fat12 => fat_bytes * 2 / 3,
            FsKind::Fat16 => fat_bytes / 2,
            _ => fat_bytes / 4,
        };
        if count + 2 > capacity {
            let clipped = capacity.saturating_sub(2);
            warnings.push(format!(
                "the FAT holds only {capacity} entries but the volume claims {count} clusters; clipped to {clipped}"
            ));
            count = clipped;
            if count == 0 {
                return Err("FAT too small for any cluster".into());
            }
        }
        let max_cluster = (count + 1) as u32;
        let mut root_clus = self.bpb_root_clus;
        if kind == FsKind::Fat32 && (root_clus < 2 || root_clus > max_cluster) {
            warnings.push(format!(
                "root directory cluster {root_clus} is out of range; trying cluster 2"
            ));
            root_clus = 2;
        }
        // FAT copies, preferred first.
        let fat_start = self.bpb_rsvd_sec_cnt as u64 * bps;
        let mut order: Vec<u64> = (0..self.bpb_num_fats as u64).collect();
        if kind == FsKind::Fat32 && self.bpb_ext_flags & 0x80 != 0 {
            let active = (self.bpb_ext_flags & 0x0F) as u64;
            if active < self.bpb_num_fats as u64 {
                order.retain(|&i| i != active);
                order.insert(0, active);
            }
        }
        let fats = order.iter().map(|i| fat_start + i * fat_bytes).collect();
        let root_region = if kind == FsKind::Fat32 {
            None
        } else {
            let start = (self.bpb_rsvd_sec_cnt as u64 + self.bpb_num_fats as u64 * fat_sz) * bps;
            Some((start, root_dir_sectors * bps))
        };
        Ok(Geometry {
            kind,
            bytes_per_sector: bps,
            cluster_size: cluster,
            fat_bytes,
            fats,
            count_of_clusters: count as u32,
            max_cluster,
            root_clus,
            root_region,
            data_start: first_data_sector * bps,
            total_size: tot_sec * bps,
            warnings,
        })
    }
}

/// Derived volume geometry (all offsets in bytes from the volume start).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Geometry {
    pub kind: FsKind,
    pub bytes_per_sector: u64,
    pub cluster_size: u64,
    pub fat_bytes: u64,
    /// FAT copies, preferred first.
    pub fats: Vec<u64>,
    pub count_of_clusters: u32,
    pub max_cluster: u32,
    pub root_clus: u32,
    /// FAT12/16 fixed root directory `(offset, length)`.
    pub root_region: Option<(u64, u64)>,
    pub data_start: u64,
    pub total_size: u64,
    pub warnings: Vec<String>,
}

/// Classify a candidate boot sector without touching the rest of the
/// volume: `Some(kind)` when it is a structurally valid FAT boot sector.
pub(crate) fn probe_sector(b: &[u8]) -> Option<FsKind> {
    let bs = BootSector::parse(b)?;
    bs.geometry().ok().map(|g| g.kind)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A FAT16 boot sector like mkfs.fat makes for 32 MiB.
    pub(crate) fn fat16_sector() -> Vec<u8> {
        let mut b = vec![0u8; 512];
        b[0..3].copy_from_slice(&[0xEB, 0x3C, 0x90]);
        b[3..11].copy_from_slice(b"mkfs.fat");
        b[0x0B..0x0D].copy_from_slice(&512u16.to_le_bytes());
        b[0x0D] = 4;
        b[0x0E..0x10].copy_from_slice(&4u16.to_le_bytes());
        b[0x10] = 2;
        b[0x11..0x13].copy_from_slice(&512u16.to_le_bytes());
        b[0x13..0x15].copy_from_slice(&0u16.to_le_bytes());
        b[0x15] = 0xF8;
        b[0x16..0x18].copy_from_slice(&64u16.to_le_bytes());
        b[0x20..0x24].copy_from_slice(&65536u32.to_le_bytes());
        b[0x26] = 0x29;
        b[0x27..0x2B].copy_from_slice(&0x1234_ABCDu32.to_le_bytes());
        b[0x2B..0x36].copy_from_slice(b"MYLABEL    ");
        b[0x36..0x3E].copy_from_slice(b"FAT16   ");
        b[0x1FE] = 0x55;
        b[0x1FF] = 0xAA;
        b
    }

    #[test]
    fn fat16_geometry() {
        let bs = BootSector::parse(&fat16_sector()).unwrap();
        let g = bs.geometry().unwrap();
        assert_eq!(g.kind, FsKind::Fat16);
        assert_eq!(g.cluster_size, 2048);
        assert_eq!(g.root_region, Some((132 * 512, 32 * 512)));
        assert_eq!(g.data_start, 164 * 512);
        assert_eq!(g.count_of_clusters as u64, (65536 - 164) / 4);
        assert_eq!(bs.label().as_deref(), Some("MYLABEL"));
        assert_eq!(bs.serial().as_deref(), Some("1234-ABCD"));
        assert!(g.warnings.is_empty());
    }

    #[test]
    fn rejects_bad_fields() {
        let base = fat16_sector();
        let mut b = base.clone();
        b[0x1FE] = 0;
        assert!(BootSector::parse(&b).unwrap().geometry().is_err());
        let mut b = base.clone();
        b[0x0D] = 3;
        assert!(BootSector::parse(&b).unwrap().geometry().is_err());
        let mut b = base.clone();
        b[0x0B..0x0D].copy_from_slice(&100u16.to_le_bytes());
        assert!(BootSector::parse(&b).unwrap().geometry().is_err());
        let mut b = base.clone();
        b[0x10] = 0;
        assert!(BootSector::parse(&b).unwrap().geometry().is_err());
        let mut b = base;
        b[0x20..0x24].copy_from_slice(&100u32.to_le_bytes());
        assert!(BootSector::parse(&b).unwrap().geometry().is_err());
        assert!(BootSector::parse(&[0u8; 100]).is_none());
    }

    #[test]
    fn fat_capacity_clips_cluster_count() {
        let mut b = fat16_sector();
        b[0x16..0x18].copy_from_slice(&8u16.to_le_bytes()); // FAT holds 2048 entries
        let g = BootSector::parse(&b).unwrap().geometry().unwrap();
        assert_eq!(g.count_of_clusters, 2046);
        assert!(g.warnings.iter().any(|w| w.contains("clipped")));
    }
}
