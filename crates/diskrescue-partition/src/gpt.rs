//! GUID Partition Table parsing with backup-header fallback.
//!
//! The primary header lives at LBA 1 and its mirror at the last LBA of the
//! device (the primary header's `AlternateLBA` says where). Each header has
//! its own CRC32 (computed over `HeaderSize` bytes with the CRC field zeroed)
//! and points at an entry array protected by a second CRC32. [`read_gpt`]
//! validates both copies and picks the best one:
//!
//! 1. the primary when header and array both verify,
//! 2. otherwise the backup when it fully verifies,
//! 3. otherwise the more plausible of the two (signature found, sane counts,
//!    entry array readable) with the failures reported as warnings.
//!
//! Limits applied to hostile input: at most 4096 entries of at most 4096 bytes
//! each, and an entry array of at most 4 MiB.

use diskrescue_core::util::bytes::{guid_mixed_endian, u32le, u64le, utf16le_string};
use diskrescue_core::{Result, Source};

use crate::io::{all_zero, read_soft};

/// The eight signature bytes of a GPT header.
pub const GPT_SIGNATURE: &[u8; 8] = b"EFI PART";

const MIN_HEADER_SIZE: usize = 92;
const MIN_ENTRY_SIZE: u32 = 128;
const MAX_ENTRY_SIZE: u32 = 4096;
const MAX_ENTRIES: u32 = 4096;
const MAX_ARRAY_BYTES: u64 = 4 << 20;

/// Decoded GPT header fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GptHeader {
    /// Revision (0x00010000 for version 1.0).
    pub revision: u32,
    /// Header size in bytes (92 for GPT 1.0).
    pub header_size: u32,
    /// CRC32 stored in the header.
    pub header_crc32: u32,
    /// LBA this header claims to live at.
    pub my_lba: u64,
    /// LBA of the other header.
    pub alternate_lba: u64,
    /// First LBA usable for partitions.
    pub first_usable_lba: u64,
    /// Last LBA usable for partitions (inclusive).
    pub last_usable_lba: u64,
    /// Disk GUID, text form.
    pub disk_guid: String,
    /// LBA of the partition entry array.
    pub entries_lba: u64,
    /// Number of entry slots.
    pub num_entries: u32,
    /// Size of each entry in bytes.
    pub entry_size: u32,
    /// CRC32 of the entry array stored in the header.
    pub entries_crc32: u32,
}

/// One used GPT partition entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GptEntry {
    /// Slot index in the entry array (0-based).
    pub slot: u32,
    /// Partition type GUID, text form.
    pub type_guid: String,
    /// Unique partition GUID, text form.
    pub unique_guid: String,
    /// First LBA.
    pub first_lba: u64,
    /// Last LBA (inclusive).
    pub last_lba: u64,
    /// Attribute flags (bit 0 platform required, bit 2 legacy BIOS bootable,
    /// bits 48..63 type specific).
    pub attributes: u64,
    /// Partition name (UTF-16LE in the entry).
    pub name: String,
}

/// A GPT as read from the device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GptInfo {
    /// Logical sector size the table was found with.
    pub sector_size: u32,
    /// Header that was used.
    pub header: GptHeader,
    /// True when the backup header and array were used.
    pub used_backup: bool,
    /// Used entries (non-zero type GUID).
    pub entries: Vec<GptEntry>,
    /// Validation failures and other findings.
    pub warnings: Vec<String>,
}

/// One header copy together with what we learned while validating it.
struct Candidate {
    lba: u64,
    header: GptHeader,
    header_size_ok: bool,
    header_crc_ok: bool,
    my_lba_ok: bool,
    geometry_ok: bool,
    array: Vec<u8>,
    array_complete: bool,
    array_crc_ok: bool,
}

impl Candidate {
    fn fully_valid(&self) -> bool {
        self.header_size_ok
            && self.header_crc_ok
            && self.my_lba_ok
            && self.geometry_ok
            && self.array_complete
            && self.array_crc_ok
    }

    fn usable(&self) -> bool {
        self.geometry_ok && self.array.len() >= self.header.entry_size as usize
    }

    fn score(&self) -> u32 {
        u32::from(self.header_crc_ok) * 4
            + u32::from(self.array_crc_ok) * 4
            + u32::from(self.my_lba_ok)
            + u32::from(self.header_size_ok)
    }

    fn problems(&self, which: &str) -> Vec<String> {
        let mut p = Vec::new();
        if !self.header_size_ok {
            p.push(format!(
                "{which} GPT header has an invalid header size {}",
                self.header.header_size
            ));
        }
        if !self.header_crc_ok {
            p.push(format!("{which} GPT header CRC32 mismatch"));
        }
        if !self.my_lba_ok {
            p.push(format!(
                "{which} GPT header claims LBA {} but was found at LBA {}",
                self.header.my_lba, self.lba
            ));
        }
        if !self.geometry_ok {
            p.push(format!(
                "{which} GPT header has an implausible entry array ({} entries of {} bytes)",
                self.header.num_entries, self.header.entry_size
            ));
        } else if !self.array_complete {
            p.push(format!(
                "{which} GPT entry array at LBA {} is unreadable or truncated",
                self.header.entries_lba
            ));
        } else if !self.array_crc_ok {
            p.push(format!("{which} GPT entry array CRC32 mismatch"));
        }
        p
    }
}

/// Decode a header sector. Returns `None` when the signature is absent.
fn parse_header(sec: &[u8]) -> Option<(GptHeader, bool, bool)> {
    if sec.len() < MIN_HEADER_SIZE || &sec[0..8] != GPT_SIGNATURE {
        return None;
    }
    let header_size = u32le(sec, 12)?;
    let size_ok = (MIN_HEADER_SIZE as u32..=sec.len() as u32).contains(&header_size);
    let stored = u32le(sec, 16)?;
    let crc_ok = if size_ok {
        let mut h = sec[..header_size as usize].to_vec();
        h[16..20].fill(0);
        crc32fast::hash(&h) == stored
    } else {
        false
    };
    let header = GptHeader {
        revision: u32le(sec, 8)?,
        header_size,
        header_crc32: stored,
        my_lba: u64le(sec, 24)?,
        alternate_lba: u64le(sec, 32)?,
        first_usable_lba: u64le(sec, 40)?,
        last_usable_lba: u64le(sec, 48)?,
        disk_guid: guid_mixed_endian(sec.get(56..72)?)?,
        entries_lba: u64le(sec, 72)?,
        num_entries: u32le(sec, 80)?,
        entry_size: u32le(sec, 84)?,
        entries_crc32: u32le(sec, 88)?,
    };
    Some((header, size_ok, crc_ok))
}

/// Read and validate the header at `lba` and its entry array.
fn load_candidate(src: &dyn Source, lba: u64, ss: u32) -> Result<Option<Candidate>> {
    let Some(off) = lba.checked_mul(u64::from(ss)) else {
        return Ok(None);
    };
    let Some(sec) = read_soft(src, off, ss as usize)? else {
        return Ok(None);
    };
    let Some((header, header_size_ok, header_crc_ok)) = parse_header(&sec) else {
        return Ok(None);
    };
    let geometry_ok = (1..=MAX_ENTRIES).contains(&header.num_entries)
        && (MIN_ENTRY_SIZE..=MAX_ENTRY_SIZE).contains(&header.entry_size)
        && header.entry_size % 8 == 0
        && u64::from(header.num_entries) * u64::from(header.entry_size) <= MAX_ARRAY_BYTES;
    let mut cand = Candidate {
        lba,
        my_lba_ok: header.my_lba == lba,
        header,
        header_size_ok,
        header_crc_ok,
        geometry_ok,
        array: Vec::new(),
        array_complete: false,
        array_crc_ok: false,
    };
    if geometry_ok {
        let want = cand.header.num_entries as usize * cand.header.entry_size as usize;
        if let Some(arr_off) = cand.header.entries_lba.checked_mul(u64::from(ss)) {
            if let Some(arr) = read_soft(src, arr_off, want)? {
                cand.array_complete = arr.len() == want;
                cand.array_crc_ok =
                    cand.array_complete && crc32fast::hash(&arr) == cand.header.entries_crc32;
                cand.array = arr;
            }
        }
    }
    Ok(Some(cand))
}

/// Decode the used entries of an entry array.
fn parse_entries(array: &[u8], entry_size: u32) -> Vec<GptEntry> {
    let es = entry_size as usize;
    if es < MIN_ENTRY_SIZE as usize {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (slot, e) in array.chunks_exact(es).enumerate() {
        if all_zero(&e[..16]) {
            continue;
        }
        let (Some(type_guid), Some(unique_guid), Some(first), Some(last), Some(attrs)) = (
            guid_mixed_endian(&e[0..16]),
            guid_mixed_endian(&e[16..32]),
            u64le(e, 32),
            u64le(e, 40),
            u64le(e, 48),
        ) else {
            continue;
        };
        out.push(GptEntry {
            slot: slot as u32,
            type_guid,
            unique_guid,
            first_lba: first,
            last_lba: last,
            attributes: attrs,
            name: utf16le_string(&e[56..128.min(es)]),
        });
    }
    out
}

/// Read the GUID partition table of `src` assuming `sector_size`-byte LBAs.
///
/// Returns `Ok(None)` when neither the header at LBA 1 nor one at the last LBA
/// carries the `EFI PART` signature. Media errors are tolerated; only fatal
/// source errors (device gone) are returned as `Err`.
pub fn read_gpt(src: &dyn Source, sector_size: u32) -> Result<Option<GptInfo>> {
    if !matches!(sector_size, 512 | 1024 | 2048 | 4096) {
        return Ok(None);
    }
    let ss = u64::from(sector_size);
    let total_lbas = src.size() / ss;
    if total_lbas < 2 {
        return Ok(None);
    }
    let last_lba = total_lbas - 1;
    let primary = load_candidate(src, 1, sector_size)?;

    let mut backup_lbas = Vec::new();
    if let Some(p) = &primary {
        let alt = p.header.alternate_lba;
        if alt > 1 {
            backup_lbas.push(alt);
        }
    }
    if last_lba > 1 && !backup_lbas.contains(&last_lba) {
        backup_lbas.push(last_lba);
    }
    let mut backup: Option<Candidate> = None;
    for lba in backup_lbas {
        if lba >= total_lbas {
            continue;
        }
        if let Some(c) = load_candidate(src, lba, sector_size)? {
            let better = match &backup {
                None => true,
                Some(b) => c.score() > b.score(),
            };
            if better {
                backup = Some(c);
            }
        }
    }

    let mut warnings = Vec::new();
    let (chosen, used_backup) = match (primary, backup) {
        (None, None) => return Ok(None),
        (Some(p), b) if p.fully_valid() => {
            match &b {
                None => warnings.push(format!(
                    "no backup GPT header found at the end of the device (LBA {last_lba}); \
                     the image may be incomplete"
                )),
                Some(b) if !b.fully_valid() => warnings.extend(b.problems("backup")),
                Some(b) => {
                    if b.header.entries_crc32 != p.header.entries_crc32 {
                        warnings.push("primary and backup GPT entry arrays differ".to_string());
                    }
                    if b.header.disk_guid != p.header.disk_guid {
                        warnings.push("primary and backup GPT disk GUIDs differ".to_string());
                    }
                }
            }
            if p.header.alternate_lba != last_lba {
                warnings.push(format!(
                    "primary GPT says the backup header is at LBA {} but the device ends at \
                     LBA {last_lba} (disk resized or image truncated)",
                    p.header.alternate_lba
                ));
            }
            (p, false)
        }
        (p, Some(b)) if b.fully_valid() => {
            match &p {
                Some(p) => warnings.extend(p.problems("primary")),
                None => warnings.push("primary GPT header (LBA 1) is missing".to_string()),
            }
            warnings.push("using the backup GPT header and entry array".to_string());
            (b, true)
        }
        (p, b) => {
            // Neither copy fully verifies: take the more plausible one.
            let p_ok = p.as_ref().is_some_and(Candidate::usable);
            let b_ok = b.as_ref().is_some_and(Candidate::usable);
            let use_backup = match (p_ok, b_ok) {
                (false, false) => {
                    let mut msgs = Vec::new();
                    if let Some(p) = &p {
                        msgs.extend(p.problems("primary"));
                    }
                    if let Some(b) = &b {
                        msgs.extend(b.problems("backup"));
                    }
                    log::debug!("GPT signature found but no usable header: {msgs:?}");
                    return Ok(None);
                }
                (true, false) => false,
                (false, true) => true,
                (true, true) => {
                    b.as_ref().map_or(0, Candidate::score) > p.as_ref().map_or(0, Candidate::score)
                }
            };
            let (chosen, other, name, other_name) = if use_backup {
                (b, p, "backup", "primary")
            } else {
                (p, b, "primary", "backup")
            };
            let chosen = chosen.expect("usable candidate present");
            warnings.extend(chosen.problems(name));
            if let Some(o) = &other {
                warnings.extend(o.problems(other_name));
            }
            warnings.push(format!(
                "no GPT copy fully verified; using the {name} copy with unverified checksums"
            ));
            (chosen, use_backup)
        }
    };

    if chosen.header.revision != 0x0001_0000 {
        warnings.push(format!(
            "unexpected GPT revision {:#010x} (expected 0x00010000)",
            chosen.header.revision
        ));
    }
    if chosen.header.first_usable_lba > chosen.header.last_usable_lba {
        warnings.push("GPT usable LBA range is inverted".to_string());
    }
    let entries = parse_entries(&chosen.array, chosen.header.entry_size);
    Ok(Some(GptInfo {
        sector_size,
        header: chosen.header,
        used_backup,
        entries,
        warnings,
    }))
}

/// Description of a GPT partition type GUID (uppercase text form), or `None`
/// for GUIDs this crate does not know.
pub fn type_description(guid: &str) -> Option<&'static str> {
    let g = guid.to_ascii_uppercase();
    Some(match g.as_str() {
        "00000000-0000-0000-0000-000000000000" => "Unused",
        "C12A7328-F81F-11D2-BA4B-00A0C93EC93B" => "EFI System",
        "024DEE41-33E7-11D3-9D69-0008C781F39F" => "MBR partition scheme",
        "21686148-6449-6E6F-744E-656564454649" => "BIOS boot",
        "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7" => "Microsoft basic data",
        "E3C9E316-0B5C-4DB8-817D-F92DF00215AE" => "Microsoft reserved",
        "DE94BBA4-06D1-4D40-A16A-BFD50179D6AC" => "Windows recovery environment",
        "5808C8AA-7E8F-42E0-85D2-E1E90434CFB3" => "Windows LDM metadata",
        "AF9B60A0-1431-4F62-BC68-3311714A69AD" => "Windows LDM data",
        "E75CAF8F-F680-4CEE-AFA3-B001E56EFC2D" => "Windows Storage Spaces",
        "37AFFC90-EF7D-4E96-91C3-2D7AE055B174" => "IBM GPFS",
        "0FC63DAF-8483-4772-8E79-3D69D8477DE4" => "Linux filesystem",
        "0657FD6D-A4AB-43C4-84E5-0933C84B4F4F" => "Linux swap",
        "A19D880F-05FC-4D3B-A006-743F0F84911E" => "Linux RAID",
        "E6D6D379-F507-44C2-A23C-238F2A3DF928" => "Linux LVM",
        "8DA63339-0007-60C0-C436-083AC8230908" => "Linux reserved",
        "933AC7E1-2EB4-4F13-B844-0E14E2AEF915" => "Linux /home",
        "3B8F8425-20E0-4F3B-907F-1A25A76F98E8" => "Linux /srv",
        "BC13C2FF-59E6-4262-A352-B275FD6F7172" => "Linux extended boot",
        "4F68BCE3-E8CD-4DB1-96E7-FBCAF984B709" => "Linux root (x86-64)",
        "44479540-F297-41B2-9AF7-D131D5F0458A" => "Linux root (x86)",
        "B921B045-1DF0-41C3-AF44-4C6F280D3FAE" => "Linux root (ARM64)",
        "69DAD710-2CE4-4E3C-B16C-21A1D49ABED3" => "Linux root (ARM)",
        "48465300-0000-11AA-AA11-00306543ECAC" => "Apple HFS/HFS+",
        "7C3457EF-0000-11AA-AA11-00306543ECAC" => "Apple APFS",
        "55465300-0000-11AA-AA11-00306543ECAC" => "Apple UFS",
        "52414944-0000-11AA-AA11-00306543ECAC" => "Apple RAID",
        "52414944-5F4F-11AA-AA11-00306543ECAC" => "Apple RAID (offline)",
        "426F6F74-0000-11AA-AA11-00306543ECAC" => "Apple boot (Recovery HD)",
        "4C616265-6C00-11AA-AA11-00306543ECAC" => "Apple label",
        "53746F72-6167-11AA-AA11-00306543ECAC" => "Apple Core Storage",
        "2568845D-2332-4675-BC39-8FA5A4748D15" => "Android bootloader",
        "114EAFFE-1552-4022-B26E-9B053604CF84" => "Android bootloader 2",
        "49A4D17F-93A3-45C1-A0DE-F50B2EBE2599" => "Android boot",
        "4177C722-9E92-4AAB-8644-43502BFD5506" => "Android recovery",
        "EF32A33B-A409-486C-9141-9FFB711F6266" => "Android misc",
        "20AC26BE-20B7-11E3-84C5-6CFDB94711E9" => "Android metadata",
        "38F428E6-D326-425D-9140-6E0EA133647C" => "Android system",
        "A893EF21-E428-470A-9E55-0668FD91A2D9" => "Android cache",
        "DC76DDA9-5AC1-491C-AF42-A82591580C0D" => "Android data",
        "EBC597D0-2053-4B15-8B64-E0AAC75F4DB1" => "Android persistent",
        "FE3A2A5D-4F32-41A7-B725-ACCC3285A309" => "ChromeOS kernel",
        "3CB8E202-3B7E-47DD-8A3C-7FF2A13CFCEC" => "ChromeOS root",
        "2E0A753D-9E48-43B0-8337-B15192CB1B5E" => "ChromeOS reserved",
        "83BD6B9D-7F41-11DC-BE0B-001560B84F0F" => "FreeBSD boot",
        "516E7CB4-6ECF-11D6-8FF8-00022D09712B" => "FreeBSD data",
        "516E7CB5-6ECF-11D6-8FF8-00022D09712B" => "FreeBSD swap",
        "516E7CB6-6ECF-11D6-8FF8-00022D09712B" => "FreeBSD UFS",
        "516E7CBA-6ECF-11D6-8FF8-00022D09712B" => "FreeBSD ZFS",
        "516E7CB8-6ECF-11D6-8FF8-00022D09712B" => "FreeBSD Vinum",
        "AA31E02A-400F-11DB-9590-000C2911D1B8" => "VMware VMFS",
        "9D275380-40AD-11DB-BF97-000C2911D1B8" => "VMware vmkcore",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_guids_have_descriptions() {
        assert_eq!(
            type_description("ebd0a0a2-b9e5-4433-87c0-68b6b72699c7"),
            Some("Microsoft basic data")
        );
        assert_eq!(
            type_description("C12A7328-F81F-11D2-BA4B-00A0C93EC93B"),
            Some("EFI System")
        );
        assert_eq!(
            type_description("0FC63DAF-8483-4772-8E79-3D69D8477DE4"),
            Some("Linux filesystem")
        );
        assert_eq!(
            type_description("12345678-0000-0000-0000-000000000000"),
            None
        );
    }

    #[test]
    fn header_parse_rejects_short_or_unsigned() {
        assert!(parse_header(&[0u8; 512]).is_none());
        assert!(parse_header(b"EFI PART").is_none());
    }

    #[test]
    fn entry_parse_skips_unused_and_bounds_names() {
        let mut arr = vec![0u8; 128 * 4];
        // Slot 1 is used.
        arr[128..144].copy_from_slice(&[1; 16]);
        arr[128 + 32..128 + 40].copy_from_slice(&100u64.to_le_bytes());
        arr[128 + 40..128 + 48].copy_from_slice(&199u64.to_le_bytes());
        for (i, c) in "boot".encode_utf16().enumerate() {
            arr[128 + 56 + 2 * i..128 + 58 + 2 * i].copy_from_slice(&c.to_le_bytes());
        }
        let e = parse_entries(&arr, 128);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].slot, 1);
        assert_eq!(e[0].name, "boot");
        assert_eq!((e[0].first_lba, e[0].last_lba), (100, 199));
        assert!(parse_entries(&arr, 64).is_empty());
    }
}
