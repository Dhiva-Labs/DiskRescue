//! Open the right filesystem parser for a volume.

use diskrescue_core::fs::{FileSystem, FsKind};
use diskrescue_core::{Error, Result, SharedSource};

/// FAT/exFAT options for recovery. Bytes between an exFAT file's valid-data
/// length and its size are read as found: a camera that lost power while
/// recording leaves the valid-data length behind the real end of the clip,
/// and zero-filling would destroy the tail that is still on the card.
pub fn fat_options() -> diskrescue_fat::FatOptions {
    diskrescue_fat::FatOptions {
        read_past_valid_data_length: true,
        ..Default::default()
    }
}

/// Try to open `src` as a FAT/exFAT volume whose boot sector may be destroyed
/// (the FAT crate rebuilds it from the FAT copies and directories).
pub fn open_fat_rebuilding(src: SharedSource) -> Result<Box<dyn FileSystem>> {
    diskrescue_fat::open(src, &fat_options())
}

/// MBR types and GPT type GUIDs that hold FAT or exFAT volumes.
pub fn is_fat_partition_type(type_id: &str) -> bool {
    let t = type_id.trim().to_ascii_uppercase();
    matches!(
        t.as_str(),
        "0X01"
            | "0X04"
            | "0X06"
            | "0X07"
            | "0X0B"
            | "0X0C"
            | "0X0E"
            | "0X11"
            | "0X14"
            | "0X16"
            | "0X1B"
            | "0X1C"
            | "0X1E"
            | "0XEF"
    ) || t == "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7"
        || t == "C12A7328-F81F-11D2-BA4B-00A0C93EC93B"
}

/// Open `src` (one volume) as filesystem `kind`.
pub fn open_filesystem(kind: FsKind, src: SharedSource) -> Result<Box<dyn FileSystem>> {
    match kind {
        FsKind::Fat12 | FsKind::Fat16 | FsKind::Fat32 | FsKind::ExFat => {
            diskrescue_fat::open(src, &fat_options())
        }
        FsKind::Ntfs => {
            let opts = diskrescue_ntfs::NtfsOptions::default();
            Ok(Box::new(diskrescue_ntfs::open(src, &opts)?))
        }
        FsKind::Ext2 | FsKind::Ext3 | FsKind::Ext4 => {
            let opts = diskrescue_ext4::ExtOptions::default();
            Ok(Box::new(diskrescue_ext4::open(src, &opts)?))
        }
        FsKind::Other => Err(Error::unsupported(
            "this filesystem type is recognised but not parsed",
        )),
    }
}
