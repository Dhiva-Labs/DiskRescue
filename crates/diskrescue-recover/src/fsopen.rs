//! Open the right filesystem parser for a volume.

use diskrescue_core::fs::{FileSystem, FsKind};
use diskrescue_core::{Error, Result, SharedSource};

/// Open `src` (one volume) as filesystem `kind`.
pub fn open_filesystem(kind: FsKind, src: SharedSource) -> Result<Box<dyn FileSystem>> {
    match kind {
        FsKind::Fat12 | FsKind::Fat16 | FsKind::Fat32 | FsKind::ExFat => {
            let opts = diskrescue_fat::FatOptions::default();
            diskrescue_fat::open(src, &opts)
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
