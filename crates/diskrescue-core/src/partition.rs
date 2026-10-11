//! Partition-table vocabulary. Parsing lives in `diskrescue-partition`.

/// Which partitioning scheme was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PartitionScheme {
    /// Classic MBR (possibly with extended partitions).
    Mbr,
    /// GUID partition table (with or without a protective MBR).
    Gpt,
    /// No table: a filesystem starts at offset 0 ("superfloppy", typical
    /// for SD cards and some USB sticks).
    None,
    /// Neither a table nor a recognisable filesystem at offset 0.
    Unknown,
}

/// One partition.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PartitionEntry {
    /// 1-based index in the order found.
    pub index: u32,
    /// Byte offset from the start of the device/image.
    pub start: u64,
    /// Size in bytes.
    pub size: u64,
    /// MBR type byte (`0x07`) or GPT type GUID as text.
    pub type_id: String,
    /// Human description (`Microsoft basic data`, `Linux filesystem`, `W95 FAT32 (LBA)`).
    pub type_desc: String,
    /// GPT partition name, when present.
    pub name: Option<String>,
    /// GPT unique partition GUID, when present.
    pub guid: Option<String>,
    /// MBR bootable flag.
    pub bootable: bool,
    /// Where this entry came from: `"mbr"`, `"ebr"`, `"gpt"`, `"gpt-backup"`, `"scan"`.
    pub origin: String,
    /// Filesystem detected at `start`, if any (filled by the probe step).
    pub fs: Option<crate::fs::FsKind>,
}

impl PartitionEntry {
    /// End offset (exclusive).
    pub fn end(&self) -> u64 {
        self.start.saturating_add(self.size)
    }

    /// Byte range on the device.
    pub fn range(&self) -> crate::ranges::Range {
        crate::ranges::Range::with_len(self.start, self.size)
    }
}

/// The result of reading the partition table(s).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PartitionTable {
    /// Scheme detected.
    pub scheme: PartitionScheme,
    /// Partitions in ascending start order.
    pub entries: Vec<PartitionEntry>,
    /// GPT disk GUID when present.
    pub disk_guid: Option<String>,
    /// Problems noticed (bad CRC, backup header used, overlapping entries).
    pub warnings: Vec<String>,
}

impl PartitionTable {
    /// Empty table with the given scheme.
    pub fn empty(scheme: PartitionScheme) -> Self {
        PartitionTable {
            scheme,
            entries: Vec::new(),
            disk_guid: None,
            warnings: Vec::new(),
        }
    }
}
