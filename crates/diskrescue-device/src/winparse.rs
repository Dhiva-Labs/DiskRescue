//! Parsers for the Windows storage IOCTL replies, as plain byte slices.
//!
//! Kept free of any Windows API so they compile and are tested on every
//! platform (device-layer spec §5.3). The Windows backend
//! (`crate::windows`, compiled only on Windows) issues the
//! `DeviceIoControl` calls and hands the raw replies to these functions.
//!
//! Layouts (all little-endian, offsets from the start of the reply):
//!
//! * `STORAGE_DEVICE_DESCRIPTOR`: `Version` u32 @0, `Size` u32 @4,
//!   `DeviceType` u8 @8, `DeviceTypeModifier` u8 @9, `RemovableMedia` u8
//!   @10, `CommandQueueing` u8 @11, `VendorIdOffset` u32 @12,
//!   `ProductIdOffset` u32 @16, `ProductRevisionOffset` u32 @20,
//!   `SerialNumberOffset` u32 @24, `BusType` u32 @28. String offsets point
//!   at NUL-terminated ASCII inside the reply; 0 means absent.
//! * `DEVICE_SEEK_PENALTY_DESCRIPTOR`: `IncursSeekPenalty` u8 @8.
//! * `STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR`: `BytesPerLogicalSector` u32
//!   @16, `BytesPerPhysicalSector` u32 @20.
//! * `DISK_GEOMETRY_EX`: `BytesPerSector` u32 @20, `MediaType` u32 @8,
//!   `DiskSize` i64 @24.
//! * `GET_LENGTH_INFORMATION`: `Length` i64 @0.
//! * `VOLUME_DISK_EXTENTS`: `NumberOfDiskExtents` u32 @0, then 24-byte
//!   `DISK_EXTENT { DiskNumber u32, pad, StartingOffset i64, ExtentLength i64 }`
//!   entries from @8.
//!
//! The bus-type mapping follows spec §3.7. It is UNVERIFIED on real
//! Windows hardware.

use diskrescue_core::device::{DeviceKind, Transport};
use diskrescue_core::util::bytes;

use crate::classify::Confidence;

/// Decoded `STORAGE_DEVICE_DESCRIPTOR`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StorageDeviceDescriptor {
    /// SCSI peripheral type (0 disk, 5 CD-ROM).
    pub device_type: u8,
    /// `RemovableMedia`.
    pub removable: bool,
    /// Vendor id string.
    pub vendor: Option<String>,
    /// Product id string.
    pub product: Option<String>,
    /// Product revision.
    pub revision: Option<String>,
    /// Serial number string.
    pub serial: Option<String>,
    /// `STORAGE_BUS_TYPE`.
    pub bus_type: u32,
}

fn string_at(buf: &[u8], off: u32) -> Option<String> {
    let off = off as usize;
    if off == 0 || off >= buf.len() {
        return None;
    }
    let tail = &buf[off..];
    let end = tail
        .iter()
        .take(1024)
        .position(|&c| c == 0)
        .unwrap_or(tail.len().min(1024));
    let s: String = tail[..end]
        .iter()
        .filter(|&&c| (0x20..0x7f).contains(&c))
        .map(|&c| c as char)
        .collect();
    let s = s.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Parse a `STORAGE_DEVICE_DESCRIPTOR` reply.
pub fn parse_storage_device_descriptor(buf: &[u8]) -> Option<StorageDeviceDescriptor> {
    if buf.len() < 32 {
        return None;
    }
    let size = bytes::u32le(buf, 4)? as usize;
    let buf = &buf[..size.clamp(32, buf.len())];
    Some(StorageDeviceDescriptor {
        device_type: buf[8],
        removable: buf[10] != 0,
        vendor: string_at(buf, bytes::u32le(buf, 12)?),
        product: string_at(buf, bytes::u32le(buf, 16)?),
        revision: string_at(buf, bytes::u32le(buf, 20)?),
        serial: string_at(buf, bytes::u32le(buf, 24)?),
        bus_type: bytes::u32le(buf, 28)?,
    })
}

/// `IncursSeekPenalty` from a `DEVICE_SEEK_PENALTY_DESCRIPTOR` reply.
pub fn parse_seek_penalty(buf: &[u8]) -> Option<bool> {
    bytes::u8_at(buf, 8).map(|b| b != 0)
}

/// `(BytesPerLogicalSector, BytesPerPhysicalSector)` from a
/// `STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR` reply.
pub fn parse_access_alignment(buf: &[u8]) -> Option<(u32, u32)> {
    Some((bytes::u32le(buf, 16)?, bytes::u32le(buf, 20)?))
}

/// `(DiskSize, BytesPerSector, MediaType)` from a `DISK_GEOMETRY_EX` reply.
pub fn parse_disk_geometry_ex(buf: &[u8]) -> Option<(u64, u32, u32)> {
    let size = bytes::i64le(buf, 24)?;
    Some((
        u64::try_from(size).ok()?,
        bytes::u32le(buf, 20)?,
        bytes::u32le(buf, 8)?,
    ))
}

/// `Length` from a `GET_LENGTH_INFORMATION` reply.
pub fn parse_length_info(buf: &[u8]) -> Option<u64> {
    u64::try_from(bytes::i64le(buf, 0)?).ok()
}

/// Disk numbers from a `VOLUME_DISK_EXTENTS` reply (at most 64 extents).
pub fn parse_volume_disk_extents(buf: &[u8]) -> Vec<u32> {
    let Some(n) = bytes::u32le(buf, 0) else {
        return Vec::new();
    };
    (0..(n as usize).min(64))
        .filter_map(|i| bytes::u32le(buf, 8 + i * 24))
        .collect()
}

/// Bus type, seek penalty and removable flag -> (kind, transport,
/// confidence), per device-layer spec §3.7.
pub fn classify_bus(
    bus: u32,
    seek_penalty: Option<bool>,
    removable: bool,
) -> (DeviceKind, Transport, Confidence) {
    use Confidence::*;
    match bus {
        17 => (DeviceKind::NvmeSsd, Transport::Nvme, High),
        3 | 11 => match seek_penalty {
            Some(false) => (DeviceKind::SataSsd, Transport::Ata, High),
            Some(true) => (DeviceKind::Hdd, Transport::Ata, High),
            None => (DeviceKind::Hdd, Transport::Ata, Low),
        },
        12 => (DeviceKind::SdCard, Transport::Mmc, High),
        13 => (DeviceKind::Mmc, Transport::Mmc, High),
        7 => match (seek_penalty, removable) {
            (_, true) => (DeviceKind::UsbFlash, Transport::Usb, Medium),
            (Some(false), false) => (DeviceKind::UsbSsd, Transport::Usb, Medium),
            (Some(true), false) => (DeviceKind::UsbHdd, Transport::Usb, Low),
            (None, false) => (DeviceKind::UsbHdd, Transport::Usb, Low),
        },
        14..=16 => (DeviceKind::Virtual, Transport::Virtual, High),
        1 | 8 | 10 => {
            let t = if bus == 10 {
                Transport::Sas
            } else {
                Transport::Scsi
            };
            match seek_penalty {
                Some(true) => (DeviceKind::Hdd, t, Medium),
                Some(false) => (DeviceKind::SataSsd, t, Medium),
                None => (DeviceKind::Unknown, t, Low),
            }
        }
        2 => (DeviceKind::Optical, Transport::Ata, Medium),
        4 => (DeviceKind::Unknown, Transport::Firewire, Low),
        _ => (DeviceKind::Unknown, Transport::Unknown, Low),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor(vendor: &str, product: &str, serial: &str, bus: u32, removable: bool) -> Vec<u8> {
        let mut b = vec![0u8; 40];
        b[8] = 0;
        b[10] = removable as u8;
        let push = |b: &mut Vec<u8>, s: &str, field: usize| {
            if s.is_empty() {
                return;
            }
            let off = b.len() as u32;
            b[field..field + 4].copy_from_slice(&off.to_le_bytes());
            b.extend_from_slice(s.as_bytes());
            b.push(0);
        };
        push(&mut b, vendor, 12);
        push(&mut b, product, 16);
        push(&mut b, serial, 24);
        b[28..32].copy_from_slice(&bus.to_le_bytes());
        let size = b.len() as u32;
        b[4..8].copy_from_slice(&size.to_le_bytes());
        b
    }

    #[test]
    fn device_descriptor() {
        let b = descriptor(
            "SanDisk ",
            "Cruzer Blade    ",
            "4C530001231012345678",
            7,
            true,
        );
        let d = parse_storage_device_descriptor(&b).unwrap();
        assert_eq!(d.vendor.as_deref(), Some("SanDisk"));
        assert_eq!(d.product.as_deref(), Some("Cruzer Blade"));
        assert_eq!(d.serial.as_deref(), Some("4C530001231012345678"));
        assert_eq!(d.revision, None);
        assert_eq!(d.bus_type, 7);
        assert!(d.removable);
        // Offsets past the end and a lying Size field are tolerated.
        let mut bad = b.clone();
        bad[12..16].copy_from_slice(&10_000u32.to_le_bytes());
        bad[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(parse_storage_device_descriptor(&bad).unwrap().vendor, None);
        assert!(parse_storage_device_descriptor(&b[..20]).is_none());
    }

    #[test]
    fn small_structs() {
        let mut sp = vec![0u8; 12];
        sp[8] = 1;
        assert_eq!(parse_seek_penalty(&sp), Some(true));
        assert_eq!(parse_seek_penalty(&sp[..8]), None);
        let mut al = vec![0u8; 28];
        al[16..20].copy_from_slice(&512u32.to_le_bytes());
        al[20..24].copy_from_slice(&4096u32.to_le_bytes());
        assert_eq!(parse_access_alignment(&al), Some((512, 4096)));
        let mut g = vec![0u8; 40];
        g[8..12].copy_from_slice(&12u32.to_le_bytes());
        g[20..24].copy_from_slice(&512u32.to_le_bytes());
        g[24..32].copy_from_slice(&(1i64 << 40).to_le_bytes());
        assert_eq!(parse_disk_geometry_ex(&g), Some((1 << 40, 512, 12)));
        g[24..32].copy_from_slice(&(-1i64).to_le_bytes());
        assert_eq!(parse_disk_geometry_ex(&g), None);
        assert_eq!(
            parse_length_info(&(5i64 << 30).to_le_bytes()),
            Some(5 << 30)
        );
        let mut ext = vec![0u8; 8 + 48];
        ext[0..4].copy_from_slice(&2u32.to_le_bytes());
        ext[8..12].copy_from_slice(&3u32.to_le_bytes());
        ext[32..36].copy_from_slice(&5u32.to_le_bytes());
        assert_eq!(parse_volume_disk_extents(&ext), vec![3, 5]);
        ext[0..4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(parse_volume_disk_extents(&ext), vec![3, 5]);
    }

    #[test]
    fn bus_mapping() {
        assert_eq!(classify_bus(17, None, false).0, DeviceKind::NvmeSsd);
        assert_eq!(classify_bus(11, Some(false), false).0, DeviceKind::SataSsd);
        assert_eq!(classify_bus(11, Some(true), false).0, DeviceKind::Hdd);
        assert_eq!(classify_bus(7, Some(true), true).0, DeviceKind::UsbFlash);
        assert_eq!(classify_bus(7, Some(false), false).0, DeviceKind::UsbSsd);
        assert_eq!(classify_bus(7, None, false).0, DeviceKind::UsbHdd);
        assert_eq!(classify_bus(12, None, true).0, DeviceKind::SdCard);
        assert_eq!(classify_bus(15, None, false).0, DeviceKind::Virtual);
        assert_eq!(classify_bus(99, None, false).0, DeviceKind::Unknown);
    }

    #[test]
    fn hostile_blobs() {
        let mut seed = 0x1234_5678_9abc_def0u64;
        for len in 0..300usize {
            let mut b = Vec::with_capacity(len);
            for _ in 0..len {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                b.push(seed as u8);
            }
            let _ = parse_storage_device_descriptor(&b);
            let _ = parse_seek_penalty(&b);
            let _ = parse_access_alignment(&b);
            let _ = parse_disk_geometry_ex(&b);
            let _ = parse_length_info(&b);
            let _ = parse_volume_disk_extents(&b);
        }
    }
}
