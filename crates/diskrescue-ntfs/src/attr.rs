//! MFT attributes: the common header, resident and non-resident forms, and
//! the typed values the reader needs ($STANDARD_INFORMATION, $FILE_NAME,
//! $ATTRIBUTE_LIST entries, reparse tags, volume name/information).
//!
//! Attributes follow each other from the record's first-attribute offset,
//! 8-byte aligned, until the type `0xFFFFFFFF`. Parsing stops at the
//! terminator, at the record's used size, at the first header whose length
//! is not a multiple of 8 / smaller than 24 bytes / runs past the record,
//! or after [`MAX_ATTRIBUTES`] attributes. An attribute whose header is
//! sound but whose body is inconsistent (name or value out of bounds) is
//! skipped with a warning and parsing continues with the next one.

use diskrescue_core::util::bytes::{slice, u16le, u32le, u64le, u8_at, utf16le_exact};

use crate::runs::{decode_runs, Run};

/// Attribute type codes.
pub mod types {
    /// $STANDARD_INFORMATION.
    pub const STANDARD_INFORMATION: u32 = 0x10;
    /// $ATTRIBUTE_LIST.
    pub const ATTRIBUTE_LIST: u32 = 0x20;
    /// $FILE_NAME.
    pub const FILE_NAME: u32 = 0x30;
    /// $OBJECT_ID.
    pub const OBJECT_ID: u32 = 0x40;
    /// $SECURITY_DESCRIPTOR.
    pub const SECURITY_DESCRIPTOR: u32 = 0x50;
    /// $VOLUME_NAME.
    pub const VOLUME_NAME: u32 = 0x60;
    /// $VOLUME_INFORMATION.
    pub const VOLUME_INFORMATION: u32 = 0x70;
    /// $DATA.
    pub const DATA: u32 = 0x80;
    /// $INDEX_ROOT.
    pub const INDEX_ROOT: u32 = 0x90;
    /// $INDEX_ALLOCATION.
    pub const INDEX_ALLOCATION: u32 = 0xA0;
    /// $BITMAP.
    pub const BITMAP: u32 = 0xB0;
    /// $REPARSE_POINT.
    pub const REPARSE_POINT: u32 = 0xC0;
    /// $EA_INFORMATION.
    pub const EA_INFORMATION: u32 = 0xD0;
    /// $EA.
    pub const EA: u32 = 0xE0;
    /// $PROPERTY_SET (NT 4 only).
    pub const PROPERTY_SET: u32 = 0xF0;
    /// $LOGGED_UTILITY_STREAM ($EFS for encrypted files).
    pub const LOGGED_UTILITY_STREAM: u32 = 0x100;
    /// End-of-attributes marker.
    pub const END: u32 = 0xFFFF_FFFF;
}

/// Attribute header flag: compressed (low byte is the compression method).
pub const ATTR_COMPRESSED: u16 = 0x0001;
/// Mask of the compression method bits.
pub const ATTR_COMPRESSION_MASK: u16 = 0x00FF;
/// Attribute header flag: encrypted (EFS).
pub const ATTR_ENCRYPTED: u16 = 0x4000;
/// Attribute header flag: sparse.
pub const ATTR_SPARSE: u16 = 0x8000;

/// File attribute flag (STANDARD_INFORMATION / FILE_NAME): encrypted.
pub const FILE_ATTR_ENCRYPTED: u32 = 0x4000;
/// File attribute flag: compressed.
pub const FILE_ATTR_COMPRESSED: u32 = 0x0800;
/// File attribute flag: sparse.
pub const FILE_ATTR_SPARSE: u32 = 0x0200;
/// File attribute flag: reparse point.
pub const FILE_ATTR_REPARSE: u32 = 0x0400;
/// FILE_NAME flag: the entry is a directory (has an $I30 index).
pub const FILE_NAME_DIRECTORY: u32 = 0x1000_0000;

/// Reparse tag of a symbolic link.
pub const REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;
/// Reparse tag of a junction / mount point.
pub const REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;

/// Upper bound on attributes parsed from one record.
pub const MAX_ATTRIBUTES: usize = 1024;

/// Record number part of a file reference.
pub fn mref(reference: u64) -> u64 {
    reference & 0x0000_FFFF_FFFF_FFFF
}

/// Sequence number part of a file reference.
pub fn mseq(reference: u64) -> u16 {
    (reference >> 48) as u16
}

/// Human-readable name of an attribute type.
pub fn type_name(t: u32) -> &'static str {
    match t {
        types::STANDARD_INFORMATION => "$STANDARD_INFORMATION",
        types::ATTRIBUTE_LIST => "$ATTRIBUTE_LIST",
        types::FILE_NAME => "$FILE_NAME",
        types::OBJECT_ID => "$OBJECT_ID",
        types::SECURITY_DESCRIPTOR => "$SECURITY_DESCRIPTOR",
        types::VOLUME_NAME => "$VOLUME_NAME",
        types::VOLUME_INFORMATION => "$VOLUME_INFORMATION",
        types::DATA => "$DATA",
        types::INDEX_ROOT => "$INDEX_ROOT",
        types::INDEX_ALLOCATION => "$INDEX_ALLOCATION",
        types::BITMAP => "$BITMAP",
        types::REPARSE_POINT => "$REPARSE_POINT",
        types::EA_INFORMATION => "$EA_INFORMATION",
        types::EA => "$EA",
        types::PROPERTY_SET => "$PROPERTY_SET",
        types::LOGGED_UTILITY_STREAM => "$LOGGED_UTILITY_STREAM",
        _ => "unknown",
    }
}

/// One attribute of an MFT record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attribute {
    /// Type code (see [`types`]).
    pub type_code: u32,
    /// Attribute name (empty for the unnamed attribute, e.g. file content).
    pub name: String,
    /// Header flags ([`ATTR_COMPRESSED`], [`ATTR_ENCRYPTED`], [`ATTR_SPARSE`]).
    pub flags: u16,
    /// Attribute instance id (unique within its record).
    pub instance: u16,
    /// MFT record number that holds this attribute (differs from the base
    /// record for attributes spilled into extension records).
    pub record: u64,
    /// Byte offset of the attribute inside that record.
    pub offset: usize,
    /// Resident value or non-resident description.
    pub body: AttributeBody,
}

/// Where an attribute's value lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttributeBody {
    /// The value is stored inside the MFT record.
    Resident(ResidentValue),
    /// The value is stored in clusters described by data runs.
    NonResident(NonResidentInfo),
}

/// A resident attribute value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResidentValue {
    /// The value bytes.
    pub value: Vec<u8>,
    /// Resident flag 0x01: the attribute is indexed (FILE_NAME).
    pub indexed: bool,
}

/// Header fields of a non-resident attribute (one piece of it when it is
/// split across extension records).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NonResidentInfo {
    /// First VCN described by this piece.
    pub lowest_vcn: u64,
    /// Last VCN described by this piece (`u64::MAX` = empty attribute).
    pub highest_vcn: u64,
    /// log2 of the compression unit in clusters (4 = 16 clusters; 0 when
    /// not compressed).
    pub compression_unit: u8,
    /// Allocated bytes (valid only when `lowest_vcn == 0`).
    pub allocated_size: u64,
    /// Logical size in bytes (valid only when `lowest_vcn == 0`).
    pub data_size: u64,
    /// Bytes actually written; beyond this the content reads as zeros
    /// (valid only when `lowest_vcn == 0`).
    pub initialized_size: u64,
    /// Compressed size (only for compressed/sparse attributes).
    pub compressed_size: Option<u64>,
    /// Decoded data runs.
    pub runs: Vec<Run>,
    /// Why run decoding stopped early, if it did.
    pub runs_error: Option<String>,
}

impl Attribute {
    /// True for resident attributes.
    pub fn is_resident(&self) -> bool {
        matches!(self.body, AttributeBody::Resident(_))
    }

    /// True when the header says the attribute is compressed.
    pub fn is_compressed(&self) -> bool {
        self.flags & ATTR_COMPRESSION_MASK != 0
    }

    /// True when the header says the attribute is encrypted.
    pub fn is_encrypted(&self) -> bool {
        self.flags & ATTR_ENCRYPTED != 0
    }

    /// True when the header says the attribute is sparse.
    pub fn is_sparse(&self) -> bool {
        self.flags & ATTR_SPARSE != 0
    }

    /// Resident value, if resident.
    pub fn value(&self) -> Option<&[u8]> {
        match &self.body {
            AttributeBody::Resident(r) => Some(&r.value),
            AttributeBody::NonResident(_) => None,
        }
    }

    /// Non-resident header, if non-resident.
    pub fn non_resident(&self) -> Option<&NonResidentInfo> {
        match &self.body {
            AttributeBody::Resident(_) => None,
            AttributeBody::NonResident(n) => Some(n),
        }
    }

    /// Name of the attribute type.
    pub fn type_name(&self) -> &'static str {
        type_name(self.type_code)
    }

    /// Logical size of the value (resident length or non-resident data size).
    pub fn data_size(&self) -> u64 {
        match &self.body {
            AttributeBody::Resident(r) => r.value.len() as u64,
            AttributeBody::NonResident(n) => n.data_size,
        }
    }
}

/// Parse the attributes of a fixed-up record. `record_no` is stored in each
/// attribute; `total_clusters` bounds run LCNs. Returns the attributes and
/// warnings about anything skipped.
pub fn parse_attributes(
    rec: &[u8],
    first: usize,
    used: usize,
    record_no: u64,
    total_clusters: Option<u64>,
) -> (Vec<Attribute>, Vec<String>) {
    let mut attrs = Vec::new();
    let mut warnings = Vec::new();
    let end = used.min(rec.len());
    let mut off = first;
    while attrs.len() < MAX_ATTRIBUTES {
        let Some(t) = u32le(rec, off).filter(|_| off + 4 <= end) else {
            warnings.push(format!(
                "attribute list of record {record_no} not terminated (stopped at {off})"
            ));
            break;
        };
        if t == types::END {
            break;
        }
        let len = u32le(rec, off + 4).unwrap_or(0) as usize;
        if len < 0x18 || len % 8 != 0 || off.checked_add(len).is_none_or(|e| e > end) {
            warnings.push(format!(
                "record {record_no}: attribute at {off} has invalid length {len}"
            ));
            break;
        }
        let a = &rec[off..off + len];
        match parse_one(a, t, record_no, off, total_clusters) {
            Ok(attr) => attrs.push(attr),
            Err(e) => warnings.push(format!(
                "record {record_no}: skipped {} attribute at {off}: {e}",
                type_name(t)
            )),
        }
        off += len;
    }
    (attrs, warnings)
}

fn parse_one(
    a: &[u8],
    t: u32,
    record_no: u64,
    offset: usize,
    total_clusters: Option<u64>,
) -> Result<Attribute, String> {
    let non_res = u8_at(a, 8).ok_or("short header")?;
    let name_len = u8_at(a, 9).ok_or("short header")? as usize;
    let name_off = u16le(a, 0x0A).ok_or("short header")? as usize;
    let flags = u16le(a, 0x0C).ok_or("short header")?;
    let instance = u16le(a, 0x0E).ok_or("short header")?;
    let name = if name_len == 0 {
        String::new()
    } else {
        let raw = slice(a, name_off, name_len * 2).ok_or("name out of bounds")?;
        utf16le_exact(raw, name_len)
    };
    let body = match non_res {
        0 => {
            let vlen = u32le(a, 0x10).ok_or("short resident header")? as usize;
            let voff = u16le(a, 0x14).ok_or("short resident header")? as usize;
            let value = slice(a, voff, vlen).ok_or("resident value out of bounds")?;
            AttributeBody::Resident(ResidentValue {
                value: value.to_vec(),
                indexed: u8_at(a, 0x16).unwrap_or(0) & 1 != 0,
            })
        }
        1 => {
            if a.len() < 0x40 {
                return Err("short non-resident header".into());
            }
            let lowest_vcn = u64le(a, 0x10).unwrap_or(0);
            let highest_vcn = u64le(a, 0x18).unwrap_or(0);
            let mp_off = u16le(a, 0x20).unwrap_or(0) as usize;
            let compression_unit = u8_at(a, 0x22).unwrap_or(0);
            let allocated_size = u64le(a, 0x28).unwrap_or(0);
            let data_size = u64le(a, 0x30).unwrap_or(0);
            let initialized_size = u64le(a, 0x38).unwrap_or(0);
            let compressed_size = if flags & (ATTR_COMPRESSION_MASK | ATTR_SPARSE) != 0 {
                u64le(a, 0x40).filter(|_| a.len() >= 0x48)
            } else {
                None
            };
            if mp_off < 0x40 || mp_off > a.len() {
                return Err(format!("mapping pairs offset {mp_off} out of bounds"));
            }
            let decoded = decode_runs(&a[mp_off..], lowest_vcn, total_clusters);
            let mut runs_error = decoded.error;
            let expected_end = highest_vcn.wrapping_add(1);
            if runs_error.is_none() && decoded.next_vcn != expected_end && highest_vcn != u64::MAX {
                runs_error = Some(format!(
                    "runs end at VCN {} but the header says {}",
                    decoded.next_vcn, expected_end
                ));
            }
            AttributeBody::NonResident(NonResidentInfo {
                lowest_vcn,
                highest_vcn,
                compression_unit,
                allocated_size,
                data_size,
                initialized_size,
                compressed_size,
                runs: decoded.runs,
                runs_error,
            })
        }
        other => return Err(format!("non-resident flag {other}")),
    };
    Ok(Attribute {
        type_code: t,
        name,
        flags,
        instance,
        record: record_no,
        offset,
        body,
    })
}

/// Decoded $STANDARD_INFORMATION value. Times are raw FILETIMEs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StandardInformation {
    /// Creation time.
    pub created: u64,
    /// Last data change.
    pub modified: u64,
    /// Last MFT record change.
    pub mft_modified: u64,
    /// Last access.
    pub accessed: u64,
    /// DOS-style file attribute flags.
    pub file_attributes: u32,
}

impl StandardInformation {
    /// Parse a resident value (at least 0x24 bytes).
    pub fn parse(v: &[u8]) -> Option<Self> {
        Some(StandardInformation {
            created: u64le(v, 0)?,
            modified: u64le(v, 8)?,
            mft_modified: u64le(v, 0x10)?,
            accessed: u64le(v, 0x18)?,
            file_attributes: u32le(v, 0x20)?,
        })
    }
}

/// $FILE_NAME namespaces.
pub mod namespace {
    /// POSIX (case-sensitive, any characters but NUL and '/').
    pub const POSIX: u8 = 0;
    /// Win32 long name.
    pub const WIN32: u8 = 1;
    /// DOS 8.3 short name.
    pub const DOS: u8 = 2;
    /// Name valid in both the Win32 and DOS namespaces.
    pub const WIN32_AND_DOS: u8 = 3;
}

/// Decoded $FILE_NAME value (also the key of every directory index entry).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileName {
    /// File reference of the parent directory (record number + sequence).
    pub parent: u64,
    /// Creation time (FILETIME).
    pub created: u64,
    /// Modification time (FILETIME).
    pub modified: u64,
    /// MFT change time (FILETIME).
    pub mft_modified: u64,
    /// Access time (FILETIME).
    pub accessed: u64,
    /// Allocated size of the unnamed $DATA (often stale).
    pub allocated_size: u64,
    /// Real size of the unnamed $DATA (often stale; 0 when written by ntfscp).
    pub real_size: u64,
    /// File attribute flags plus [`FILE_NAME_DIRECTORY`].
    pub flags: u32,
    /// Reparse tag or EA size.
    pub reparse: u32,
    /// Namespace (see [`namespace`]).
    pub namespace: u8,
    /// The name.
    pub name: String,
}

impl FileName {
    /// Parse a $FILE_NAME body. Rejects empty names, unknown namespaces and
    /// names that run past the value.
    pub fn parse(v: &[u8]) -> Option<Self> {
        let len = u8_at(v, 0x40)? as usize;
        let ns = u8_at(v, 0x41)?;
        if len == 0 || ns > 3 {
            return None;
        }
        let raw = slice(v, 0x42, len * 2)?;
        Some(FileName {
            parent: u64le(v, 0)?,
            created: u64le(v, 8)?,
            modified: u64le(v, 0x10)?,
            mft_modified: u64le(v, 0x18)?,
            accessed: u64le(v, 0x20)?,
            allocated_size: u64le(v, 0x28)?,
            real_size: u64le(v, 0x30)?,
            flags: u32le(v, 0x38)?,
            reparse: u32le(v, 0x3C)?,
            namespace: ns,
            name: utf16le_exact(raw, len),
        })
    }

    /// Length in bytes of the encoded body.
    pub fn encoded_len(&self) -> usize {
        0x42 + 2 * self.name.encode_utf16().count()
    }

    /// Parent record number.
    pub fn parent_record(&self) -> u64 {
        mref(self.parent)
    }

    /// Parent sequence number.
    pub fn parent_seq(&self) -> u16 {
        mseq(self.parent)
    }

    /// True when the FILE_NAME says the entry is a directory.
    pub fn is_directory(&self) -> bool {
        self.flags & FILE_NAME_DIRECTORY != 0
    }

    /// True for the DOS 8.3-only namespace.
    pub fn is_dos_only(&self) -> bool {
        self.namespace == namespace::DOS
    }
}

/// One $ATTRIBUTE_LIST entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttributeListEntry {
    /// Attribute type.
    pub type_code: u32,
    /// Attribute name.
    pub name: String,
    /// Lowest VCN of the piece described.
    pub lowest_vcn: u64,
    /// File reference of the record holding the attribute.
    pub reference: u64,
    /// Attribute instance id inside that record.
    pub instance: u16,
}

/// Upper bound on attribute list entries considered.
pub const MAX_LIST_ENTRIES: usize = 65536;

/// Parse an $ATTRIBUTE_LIST value. Stops at the first malformed entry.
pub fn parse_attribute_list(v: &[u8]) -> Vec<AttributeListEntry> {
    let mut out = Vec::new();
    let mut off = 0usize;
    while off + 0x1A <= v.len() && out.len() < MAX_LIST_ENTRIES {
        let t = u32le(v, off).unwrap_or(0);
        let len = u16le(v, off + 4).unwrap_or(0) as usize;
        if t == 0 || t == types::END || len < 0x1A || off + len > v.len() {
            break;
        }
        let name_len = v[off + 6] as usize;
        let name_off = v[off + 7] as usize;
        let name = if name_len == 0 {
            String::new()
        } else {
            match slice(v, off + name_off, name_len * 2) {
                Some(raw) if name_off + name_len * 2 <= len => utf16le_exact(raw, name_len),
                _ => break,
            }
        };
        out.push(AttributeListEntry {
            type_code: t,
            name,
            lowest_vcn: u64le(v, off + 8).unwrap_or(0),
            reference: u64le(v, off + 0x10).unwrap_or(0),
            instance: u16le(v, off + 0x18).unwrap_or(0),
        });
        off += len;
    }
    out
}

/// Decoded $VOLUME_INFORMATION.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VolumeInformation {
    /// NTFS major version (3 for XP and later).
    pub major: u8,
    /// NTFS minor version.
    pub minor: u8,
    /// Volume flags (0x0001 = dirty).
    pub flags: u16,
}

impl VolumeInformation {
    /// Parse a resident value.
    pub fn parse(v: &[u8]) -> Option<Self> {
        Some(VolumeInformation {
            major: u8_at(v, 8)?,
            minor: u8_at(v, 9)?,
            flags: u16le(v, 0x0A)?,
        })
    }

    /// True when the dirty bit is set (unclean shutdown).
    pub fn is_dirty(&self) -> bool {
        self.flags & 0x0001 != 0
    }
}

#[cfg(test)]
pub(crate) fn build_file_name(parent: u64, name: &str, ns: u8, flags: u32, size: u64) -> Vec<u8> {
    let units: Vec<u16> = name.encode_utf16().collect();
    let mut v = vec![0u8; 0x42 + units.len() * 2];
    v[0..8].copy_from_slice(&parent.to_le_bytes());
    let t = 133_000_000_000_000_000u64;
    for i in 0..4 {
        v[8 + i * 8..16 + i * 8].copy_from_slice(&t.to_le_bytes());
    }
    v[0x28..0x30].copy_from_slice(&size.to_le_bytes());
    v[0x30..0x38].copy_from_slice(&size.to_le_bytes());
    v[0x38..0x3C].copy_from_slice(&flags.to_le_bytes());
    v[0x40] = units.len() as u8;
    v[0x41] = ns;
    for (i, u) in units.iter().enumerate() {
        v[0x42 + i * 2..0x44 + i * 2].copy_from_slice(&u.to_le_bytes());
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runs::encode_runs;

    fn resident(t: u32, name: &str, value: &[u8]) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let name_off = 0x18usize;
        let value_off = (name_off + units.len() * 2 + 7) & !7;
        let len = (value_off + value.len() + 7) & !7;
        let mut a = vec![0u8; len];
        a[0..4].copy_from_slice(&t.to_le_bytes());
        a[4..8].copy_from_slice(&(len as u32).to_le_bytes());
        a[9] = units.len() as u8;
        a[0x0A..0x0C].copy_from_slice(&(name_off as u16).to_le_bytes());
        a[0x10..0x14].copy_from_slice(&(value.len() as u32).to_le_bytes());
        a[0x14..0x16].copy_from_slice(&(value_off as u16).to_le_bytes());
        for (i, u) in units.iter().enumerate() {
            a[name_off + i * 2..name_off + i * 2 + 2].copy_from_slice(&u.to_le_bytes());
        }
        a[value_off..value_off + value.len()].copy_from_slice(value);
        a
    }

    #[test]
    fn parses_resident_and_non_resident() {
        let mut rec = vec![0u8; 1024];
        let mut off = 0x38;
        let si = resident(types::STANDARD_INFORMATION, "", &[1u8; 48]);
        rec[off..off + si.len()].copy_from_slice(&si);
        off += si.len();
        let fname = build_file_name(5 | (5 << 48), "héllo.jpg", namespace::WIN32, 0x20, 10);
        let fna = resident(types::FILE_NAME, "", &fname);
        rec[off..off + fna.len()].copy_from_slice(&fna);
        off += fna.len();
        // Non-resident $DATA with two runs.
        let mp = encode_runs(&[(Some(100), 4), (None, 2)]);
        let len = (0x40 + mp.len() + 7) & !7;
        let mut nr = vec![0u8; len];
        nr[0..4].copy_from_slice(&types::DATA.to_le_bytes());
        nr[4..8].copy_from_slice(&(len as u32).to_le_bytes());
        nr[8] = 1;
        nr[0x18..0x20].copy_from_slice(&5u64.to_le_bytes());
        nr[0x20..0x22].copy_from_slice(&0x40u16.to_le_bytes());
        nr[0x28..0x30].copy_from_slice(&(6u64 * 4096).to_le_bytes());
        nr[0x30..0x38].copy_from_slice(&20000u64.to_le_bytes());
        nr[0x38..0x40].copy_from_slice(&20000u64.to_le_bytes());
        nr[0x40..0x40 + mp.len()].copy_from_slice(&mp);
        rec[off..off + len].copy_from_slice(&nr);
        off += len;
        rec[off..off + 4].copy_from_slice(&types::END.to_le_bytes());
        let (attrs, warnings) = parse_attributes(&rec, 0x38, off + 8, 70, Some(1000));
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(attrs.len(), 3);
        let fnv = FileName::parse(attrs[1].value().unwrap()).unwrap();
        assert_eq!(fnv.name, "héllo.jpg");
        assert_eq!(fnv.parent_record(), 5);
        assert_eq!(fnv.parent_seq(), 5);
        let nr = attrs[2].non_resident().unwrap();
        assert_eq!(nr.runs.len(), 2);
        assert_eq!(nr.data_size, 20000);
        assert!(nr.runs_error.is_none());
        assert_eq!(attrs[2].record, 70);
    }

    #[test]
    fn hostile_attribute_headers() {
        let mut rec = vec![0u8; 1024];
        // Length not a multiple of 8.
        rec[0x38..0x3C].copy_from_slice(&0x10u32.to_le_bytes());
        rec[0x3C..0x40].copy_from_slice(&0x19u32.to_le_bytes());
        let (attrs, w) = parse_attributes(&rec, 0x38, 1024, 1, None);
        assert!(attrs.is_empty() && !w.is_empty());
        // Length runs past the record.
        rec[0x3C..0x40].copy_from_slice(&0x1000u32.to_le_bytes());
        let (attrs, _) = parse_attributes(&rec, 0x38, 1024, 1, None);
        assert!(attrs.is_empty());
        // Resident value out of bounds: skipped, then terminator.
        let mut a = resident(types::DATA, "", b"hello");
        a[0x10..0x14].copy_from_slice(&5000u32.to_le_bytes());
        rec[0x38..0x38 + a.len()].copy_from_slice(&a);
        let end = 0x38 + a.len();
        rec[end..end + 4].copy_from_slice(&types::END.to_le_bytes());
        let (attrs, w) = parse_attributes(&rec, 0x38, 1024, 1, None);
        assert!(attrs.is_empty());
        assert_eq!(w.len(), 1);
        // First attribute offset beyond the record.
        let (attrs, _) = parse_attributes(&rec, 5000, 1024, 1, None);
        assert!(attrs.is_empty());
    }

    #[test]
    fn attribute_list_entries() {
        let mut v = vec![0u8; 0x20 * 2];
        for (i, (t, r)) in [(0x10u32, 70u64), (0x80, 71)].iter().enumerate() {
            let o = i * 0x20;
            v[o..o + 4].copy_from_slice(&t.to_le_bytes());
            v[o + 4..o + 6].copy_from_slice(&0x20u16.to_le_bytes());
            v[o + 7] = 0x1A;
            v[o + 0x10..o + 0x18].copy_from_slice(&(r | (1 << 48)).to_le_bytes());
            v[o + 0x18..o + 0x1A].copy_from_slice(&(i as u16).to_le_bytes());
        }
        let e = parse_attribute_list(&v);
        assert_eq!(e.len(), 2);
        assert_eq!(mref(e[1].reference), 71);
        assert_eq!(mseq(e[1].reference), 1);
        assert_eq!(e[1].instance, 1);
        // Truncated / zero-length entries stop the walk.
        let mut bad = v.clone();
        bad[0x24..0x26].copy_from_slice(&0u16.to_le_bytes());
        assert_eq!(parse_attribute_list(&bad).len(), 1);
        assert!(parse_attribute_list(&v[..10]).is_empty());
    }

    #[test]
    fn file_name_validation() {
        let v = build_file_name(5, "a", namespace::DOS, 0, 0);
        assert!(FileName::parse(&v).unwrap().is_dos_only());
        let mut bad = v.clone();
        bad[0x41] = 7;
        assert!(FileName::parse(&bad).is_none());
        let mut bad = v.clone();
        bad[0x40] = 50;
        assert!(FileName::parse(&bad).is_none());
        assert!(FileName::parse(&v[..0x40]).is_none());
        assert_eq!(FileName::parse(&v).unwrap().encoded_len(), v.len());
    }
}
