//! MFT FILE records.
//!
//! A record is read as raw bytes, its update sequence array is applied
//! ([`crate::fixup`]), and the header is decoded. Records whose signature
//! is not `FILE` (all zeros, `BAAD` written by chkdsk, garbage) or whose
//! fixup array is unusable are rejected; a record whose fixup check bytes
//! mismatch (torn write) is accepted and carries the torn stride indices so
//! the caller can warn and lower confidence.

use diskrescue_core::util::bytes::{u16le, u32le, u64le};

use crate::attr::{mref, mseq, parse_attributes, Attribute};
use crate::fixup::apply_fixups;

/// Record header flag: the record is in use.
pub const FLAG_IN_USE: u16 = 0x0001;
/// Record header flag: the record is a directory.
pub const FLAG_DIRECTORY: u16 = 0x0002;

/// Why a record could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordError {
    /// The record has never been used (all zeros) or holds something else.
    NotARecord,
    /// chkdsk marked the record bad ("BAAD").
    Baad,
    /// The header is inconsistent (fixup array, offsets, size).
    Corrupt(String),
}

impl std::fmt::Display for RecordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecordError::NotARecord => f.write_str("not a FILE record"),
            RecordError::Baad => f.write_str("record marked BAAD by chkdsk"),
            RecordError::Corrupt(s) => write!(f, "corrupt record header: {s}"),
        }
    }
}

/// A decoded, fixed-up MFT record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MftRecord {
    /// Record number (position in the MFT).
    pub number: u64,
    /// Sequence number (incremented when the record is freed or reused).
    pub sequence: u16,
    /// Hard link count (a hint only).
    pub link_count: u16,
    /// Header flags ([`FLAG_IN_USE`], [`FLAG_DIRECTORY`], ...).
    pub flags: u16,
    /// Offset of the first attribute.
    pub first_attribute: u16,
    /// Bytes in use.
    pub used_size: u32,
    /// Bytes allocated for the record (the record size).
    pub allocated_size: u32,
    /// File reference of the base record (0 for base records).
    pub base_reference: u64,
    /// The record's own number as stored in the header (NTFS 3.1+).
    pub self_number: Option<u32>,
    /// $LogFile sequence number.
    pub lsn: u64,
    /// 512-byte strides whose fixup check bytes did not match (torn write).
    pub torn_strides: Vec<usize>,
    /// Non-fatal inconsistencies noticed in the header.
    pub warnings: Vec<String>,
    data: Vec<u8>,
}

impl MftRecord {
    /// Decode record `number` from its raw on-disk bytes.
    pub fn parse(number: u64, mut raw: Vec<u8>) -> Result<MftRecord, RecordError> {
        if raw.len() < 0x30 {
            return Err(RecordError::Corrupt(format!("only {} bytes", raw.len())));
        }
        match &raw[0..4] {
            b"FILE" => {}
            b"BAAD" => return Err(RecordError::Baad),
            _ => return Err(RecordError::NotARecord),
        }
        let torn = apply_fixups(&mut raw).map_err(|e| RecordError::Corrupt(e.to_string()))?;
        let len = raw.len();
        let usa_ofs = u16le(&raw, 4).unwrap_or(0);
        let first = u16le(&raw, 0x14).unwrap_or(0);
        if (first as usize) < 0x18 || first as usize + 8 > len || first % 8 != 0 {
            return Err(RecordError::Corrupt(format!(
                "first attribute offset {first} invalid"
            )));
        }
        let mut warnings = Vec::new();
        let mut used = u32le(&raw, 0x18).unwrap_or(0);
        if used as usize > len || used < first as u32 + 8 {
            warnings.push(format!(
                "record {number}: bytes in use {used} invalid; scanning the whole record"
            ));
            used = len as u32;
        }
        let allocated = u32le(&raw, 0x1C).unwrap_or(0);
        if allocated as usize != len {
            warnings.push(format!(
                "record {number}: allocated size {allocated} differs from record size {len}"
            ));
        }
        let self_number = if usa_ofs >= 0x30 {
            u32le(&raw, 0x2C)
        } else {
            None
        };
        if let Some(s) = self_number {
            if s as u64 != number & 0xFFFF_FFFF {
                warnings.push(format!(
                    "record {number} says it is record {s} (MFT run list damaged?)"
                ));
            }
        }
        if !torn.is_empty() {
            warnings.push(format!(
                "record {number}: torn write, fixup mismatch in 512-byte block(s) {torn:?}"
            ));
        }
        Ok(MftRecord {
            number,
            sequence: u16le(&raw, 0x10).unwrap_or(0),
            link_count: u16le(&raw, 0x12).unwrap_or(0),
            flags: u16le(&raw, 0x16).unwrap_or(0),
            first_attribute: first,
            used_size: used,
            allocated_size: allocated,
            base_reference: u64le(&raw, 0x20).unwrap_or(0),
            self_number,
            lsn: u64le(&raw, 0x08).unwrap_or(0),
            torn_strides: torn,
            warnings,
            data: raw,
        })
    }

    /// True when the in-use flag is set.
    pub fn in_use(&self) -> bool {
        self.flags & FLAG_IN_USE != 0
    }

    /// True when the directory flag is set.
    pub fn is_directory(&self) -> bool {
        self.flags & FLAG_DIRECTORY != 0
    }

    /// True for extension records (attributes spilled from a base record).
    pub fn is_extension(&self) -> bool {
        mref(self.base_reference) != 0
    }

    /// Base record number for extension records.
    pub fn base_record(&self) -> Option<u64> {
        self.is_extension().then(|| mref(self.base_reference))
    }

    /// Sequence number stored in the base reference.
    pub fn base_sequence(&self) -> u16 {
        mseq(self.base_reference)
    }

    /// This record's file reference (number + sequence).
    pub fn reference(&self) -> u64 {
        (self.number & 0x0000_FFFF_FFFF_FFFF) | ((self.sequence as u64) << 48)
    }

    /// True when a fixup mismatch was detected.
    pub fn is_torn(&self) -> bool {
        !self.torn_strides.is_empty()
    }

    /// The fixed-up record bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.data
    }

    /// Parse the attributes stored in this record only (no attribute-list
    /// folding). `total_clusters` bounds data-run LCNs when known.
    pub fn own_attributes(&self, total_clusters: Option<u64>) -> (Vec<Attribute>, Vec<String>) {
        parse_attributes(
            &self.data,
            self.first_attribute as usize,
            self.used_size as usize,
            self.number,
            total_clusters,
        )
    }

    /// Whether a reference with sequence `ref_seq` can denote this record:
    /// equal sequence numbers, or (for a freed record) the sequence one
    /// higher than the reference because ntfs-3g and Windows bump it on
    /// deletion. A reference sequence of 0 carries no information.
    pub fn matches_sequence(&self, ref_seq: u16) -> bool {
        sequence_matches(self.sequence, self.in_use(), ref_seq)
    }
}

/// See [`MftRecord::matches_sequence`].
pub fn sequence_matches(record_seq: u16, in_use: bool, ref_seq: u16) -> bool {
    if ref_seq == 0 || record_seq == ref_seq {
        return true;
    }
    if in_use {
        return false;
    }
    let bumped = if ref_seq == 0xFFFF { 1 } else { ref_seq + 1 };
    record_seq == bumped
}

#[cfg(test)]
pub(crate) mod testutil {
    //! Builders for synthetic records used by unit tests.
    use crate::fixup::protect;

    /// Build a 1 KiB FILE record with the given attributes (already encoded,
    /// 8-byte aligned) and apply the update sequence protection.
    pub fn build_record(number: u32, seq: u16, flags: u16, attrs: &[Vec<u8>]) -> Vec<u8> {
        build_record_sized(1024, number, seq, flags, 0, attrs)
    }

    /// Like [`build_record`] with an explicit size and base reference.
    pub fn build_record_sized(
        size: usize,
        number: u32,
        seq: u16,
        flags: u16,
        base: u64,
        attrs: &[Vec<u8>],
    ) -> Vec<u8> {
        let mut r = vec![0u8; size];
        r[0..4].copy_from_slice(b"FILE");
        r[4..6].copy_from_slice(&0x30u16.to_le_bytes());
        r[6..8].copy_from_slice(&((size / 512 + 1) as u16).to_le_bytes());
        r[0x10..0x12].copy_from_slice(&seq.to_le_bytes());
        r[0x12..0x14].copy_from_slice(&1u16.to_le_bytes());
        let first = (0x30 + 2 * (size / 512 + 1) + 7) & !7;
        r[0x14..0x16].copy_from_slice(&(first as u16).to_le_bytes());
        r[0x16..0x18].copy_from_slice(&flags.to_le_bytes());
        r[0x1C..0x20].copy_from_slice(&(size as u32).to_le_bytes());
        r[0x20..0x28].copy_from_slice(&base.to_le_bytes());
        r[0x2C..0x30].copy_from_slice(&number.to_le_bytes());
        let mut off = first;
        for a in attrs {
            r[off..off + a.len()].copy_from_slice(a);
            off += a.len();
        }
        r[off..off + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        r[0x18..0x1C].copy_from_slice(&((off + 8) as u32).to_le_bytes());
        protect(&mut r, 0x0042);
        r
    }

    /// Encode a resident attribute.
    pub fn resident(t: u32, name: &str, value: &[u8], instance: u16) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let name_off = 0x18usize;
        let value_off = (name_off + units.len() * 2 + 7) & !7;
        let len = (value_off + value.len() + 7) & !7;
        let mut a = vec![0u8; len];
        a[0..4].copy_from_slice(&t.to_le_bytes());
        a[4..8].copy_from_slice(&(len as u32).to_le_bytes());
        a[9] = units.len() as u8;
        a[0x0A..0x0C].copy_from_slice(&(name_off as u16).to_le_bytes());
        a[0x0E..0x10].copy_from_slice(&instance.to_le_bytes());
        a[0x10..0x14].copy_from_slice(&(value.len() as u32).to_le_bytes());
        a[0x14..0x16].copy_from_slice(&(value_off as u16).to_le_bytes());
        for (i, u) in units.iter().enumerate() {
            a[name_off + i * 2..name_off + i * 2 + 2].copy_from_slice(&u.to_le_bytes());
        }
        a[value_off..value_off + value.len()].copy_from_slice(value);
        a
    }

    /// Encode a non-resident attribute.
    #[allow(clippy::too_many_arguments)]
    pub fn non_resident(
        t: u32,
        name: &str,
        flags: u16,
        instance: u16,
        lowest_vcn: u64,
        highest_vcn: u64,
        sizes: (u64, u64, u64),
        compression_unit: u8,
        mapping_pairs: &[u8],
    ) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let header = if flags & 0x80FF != 0 { 0x48 } else { 0x40 };
        let name_off = header;
        let mp_off = (name_off + units.len() * 2 + 7) & !7;
        let len = (mp_off + mapping_pairs.len() + 7) & !7;
        let mut a = vec![0u8; len];
        a[0..4].copy_from_slice(&t.to_le_bytes());
        a[4..8].copy_from_slice(&(len as u32).to_le_bytes());
        a[8] = 1;
        a[9] = units.len() as u8;
        a[0x0A..0x0C].copy_from_slice(&(name_off as u16).to_le_bytes());
        a[0x0C..0x0E].copy_from_slice(&flags.to_le_bytes());
        a[0x0E..0x10].copy_from_slice(&instance.to_le_bytes());
        a[0x10..0x18].copy_from_slice(&lowest_vcn.to_le_bytes());
        a[0x18..0x20].copy_from_slice(&highest_vcn.to_le_bytes());
        a[0x20..0x22].copy_from_slice(&(mp_off as u16).to_le_bytes());
        a[0x22] = compression_unit;
        a[0x28..0x30].copy_from_slice(&sizes.0.to_le_bytes());
        a[0x30..0x38].copy_from_slice(&sizes.1.to_le_bytes());
        a[0x38..0x40].copy_from_slice(&sizes.2.to_le_bytes());
        for (i, u) in units.iter().enumerate() {
            a[name_off + i * 2..name_off + i * 2 + 2].copy_from_slice(&u.to_le_bytes());
        }
        a[mp_off..mp_off + mapping_pairs.len()].copy_from_slice(mapping_pairs);
        a
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;
    use crate::attr::types;

    #[test]
    fn parses_header_and_attributes() {
        let raw = build_record(
            70,
            3,
            FLAG_IN_USE,
            &[resident(types::DATA, "", b"hello world", 1)],
        );
        let rec = MftRecord::parse(70, raw).unwrap();
        assert!(rec.in_use());
        assert!(!rec.is_directory());
        assert!(!rec.is_extension());
        assert_eq!(rec.sequence, 3);
        assert_eq!(rec.reference(), 70 | (3 << 48));
        assert!(rec.warnings.is_empty(), "{:?}", rec.warnings);
        let (attrs, w) = rec.own_attributes(None);
        assert!(w.is_empty());
        assert_eq!(attrs[0].value().unwrap(), b"hello world");
    }

    #[test]
    fn torn_record_still_parses() {
        let mut raw = build_record(9, 1, FLAG_IN_USE, &[]);
        raw[1022] ^= 0xFF;
        let rec = MftRecord::parse(9, raw).unwrap();
        assert!(rec.is_torn());
        assert_eq!(rec.torn_strides, vec![1]);
    }

    #[test]
    fn rejects_non_records() {
        assert_eq!(
            MftRecord::parse(1, vec![0; 1024]).unwrap_err(),
            RecordError::NotARecord
        );
        let mut raw = build_record(1, 1, 0, &[]);
        raw[0..4].copy_from_slice(b"BAAD");
        assert_eq!(MftRecord::parse(1, raw).unwrap_err(), RecordError::Baad);
        let mut raw = build_record(1, 1, 0, &[]);
        raw[0x14] = 0xFF;
        raw[0x15] = 0xFF;
        assert!(matches!(
            MftRecord::parse(1, raw),
            Err(RecordError::Corrupt(_))
        ));
        assert!(MftRecord::parse(1, b"FILE".to_vec()).is_err());
    }

    #[test]
    fn sequence_matching_rules() {
        assert!(sequence_matches(5, true, 5));
        assert!(!sequence_matches(6, true, 5));
        assert!(sequence_matches(6, false, 5));
        assert!(!sequence_matches(7, false, 5));
        assert!(sequence_matches(1, false, 0xFFFF));
        assert!(sequence_matches(9, true, 0));
    }

    #[test]
    fn self_number_mismatch_is_a_warning() {
        let raw = build_record(12, 1, FLAG_IN_USE, &[]);
        let rec = MftRecord::parse(13, raw).unwrap();
        assert!(rec
            .warnings
            .iter()
            .any(|w| w.contains("says it is record 12")));
    }
}
