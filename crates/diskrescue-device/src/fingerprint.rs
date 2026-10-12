//! Device fingerprints: re-finding the same physical medium after it
//! dropped off the bus and came back, possibly under another name.
//!
//! A USB bridge or a flash controller that resets makes the kernel remove
//! the disk and add it again (`sdb` becomes `sdc`, a new `diskseq`). The
//! imaging engine must continue on the *same* medium, never on whatever
//! now answers to the old name. A [`Fingerprint`] captures what identifies
//! the medium: serial number, model, vendor, size, logical sector size, a
//! stable `/dev/disk/by-id` name and a BLAKE3 hash of LBA 0.
//!
//! [`Fingerprint::matches`] compares a fingerprint with a candidate:
//!
//! * [`Match::Exact`]: a unique (non-generic) serial number, size and
//!   logical sector size are equal (and the model, when both have one).
//! * [`Match::Probable`]: nothing contradicts, but the identity is weak:
//!   no serial, a generic serial shared by a whole product line
//!   (`0123456789ABCDEF`), or the serial vanished. The caller should
//!   confirm with the first-sector hash before resuming.
//! * [`Match::Mismatch`]: size, sector size, serial or model differ. A
//!   device whose serial matches but whose size changed is a controller in
//!   a failure mode (or a different card in the same reader); resuming on
//!   it would mix data, so the device layer refuses.

use diskrescue_core::device::DeviceInfo;
use diskrescue_core::util::hash::blake3_hex;

/// What identifies a medium across re-enumeration. Serialisable so the
/// imaging engine can store it next to the mapfile.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Fingerprint {
    /// Serial number as reported by the device (trimmed).
    pub serial: Option<String>,
    /// Model string.
    pub model: Option<String>,
    /// Vendor string.
    pub vendor: Option<String>,
    /// Size in bytes.
    pub size: u64,
    /// Logical sector size.
    pub logical_sector: u32,
    /// Stable OS identifier (`/dev/disk/by-id` name on Linux).
    pub stable_id: Option<String>,
    /// BLAKE3 (hex) of the first logical sector, if it was readable.
    pub first_sector_hash: Option<String>,
}

/// Result of [`Fingerprint::matches`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Match {
    /// Same unique serial, size and sector size.
    Exact,
    /// Nothing contradicts but the identity is weak; the reason says why.
    Probable(String),
    /// A different device (or the same one in a state we must not trust).
    Mismatch(String),
}

impl Match {
    /// True for [`Match::Exact`].
    pub fn is_exact(&self) -> bool {
        matches!(self, Match::Exact)
    }

    /// True for [`Match::Mismatch`].
    pub fn is_mismatch(&self) -> bool {
        matches!(self, Match::Mismatch(_))
    }
}

/// Serial numbers that many devices share: placeholder strings shipped by
/// cheap controllers and bridges. A generic serial cannot identify a
/// medium on its own.
pub fn is_generic_serial(serial: &str) -> bool {
    let s = serial.trim();
    if s.len() < 4 {
        return true;
    }
    let lower = s.to_ascii_lowercase();
    const KNOWN: &[&str] = &[
        "0123456789abcdef",
        "123456789abcdef",
        "0123456789",
        "1234567890",
        "123456789012",
        "abcdef0123456789",
        "general_udisk",
        "general",
        "udisk",
        "usb disk",
        "flash disk",
        "mass storage",
        "sn0001",
        "00000000000000000001",
        "default string",
        "to be filled by o.e.m.",
        "none",
        "n/a",
        "unknown",
    ];
    if KNOWN.contains(&lower.as_str()) {
        return true;
    }
    // A single repeated character (000000000, FFFFFFFF, AAAAAAAAAAAA) or
    // an MMC serial of zero.
    let first = lower.as_bytes()[0];
    if lower.bytes().all(|c| c == first) {
        return true;
    }
    if let Some(hex) = lower.strip_prefix("0x") {
        if hex.bytes().all(|c| c == b'0') {
            return true;
        }
    }
    // Strictly ascending digit runs ("123456789") are placeholders.
    let b = lower.as_bytes();
    if b.len() >= 8
        && b.iter().all(|c| c.is_ascii_hexdigit())
        && b.windows(2)
            .all(|w| w[1] == w[0] + 1 || (w[0] == b'9' && w[1] == b'a'))
    {
        return true;
    }
    false
}

fn norm(s: &Option<String>) -> Option<String> {
    s.as_ref()
        .map(|v| v.trim().to_ascii_lowercase())
        .filter(|v| !v.is_empty())
}

fn strong_serial(s: &Option<String>) -> Option<String> {
    norm(s).filter(|v| !is_generic_serial(v))
}

impl Fingerprint {
    /// Build a fingerprint from a device description and, optionally, the
    /// bytes of its first sector (only the first `logical_sector` bytes are
    /// hashed, so callers may pass a larger block).
    pub fn of(info: &DeviceInfo, first_sector: Option<&[u8]>) -> Self {
        let sector = info.logical_sector.max(1) as usize;
        Fingerprint {
            serial: info.serial.clone().filter(|s| !s.trim().is_empty()),
            model: info.model.clone().filter(|s| !s.trim().is_empty()),
            vendor: info.vendor.clone().filter(|s| !s.trim().is_empty()),
            size: info.size,
            logical_sector: info.logical_sector,
            stable_id: info.stable_id.clone(),
            first_sector_hash: first_sector
                .filter(|b| !b.is_empty())
                .map(|b| hash_first_sector(b, sector)),
        }
    }

    /// True when the serial number alone can identify the medium.
    pub fn has_strong_serial(&self) -> bool {
        strong_serial(&self.serial).is_some()
    }

    /// Compare with a candidate device. See the module docs.
    pub fn matches(&self, info: &DeviceInfo) -> Match {
        let my_serial = strong_serial(&self.serial);
        let their_serial = norm(&info.serial);
        let same_serial = my_serial.is_some() && my_serial == their_serial;

        if info.size != self.size {
            return Match::Mismatch(if same_serial {
                format!(
                    "same serial number but the size changed from {} to {} bytes",
                    self.size, info.size
                )
            } else {
                format!("size differs ({} vs {} bytes)", info.size, self.size)
            });
        }
        if info.logical_sector != self.logical_sector {
            return Match::Mismatch(format!(
                "logical sector size differs ({} vs {})",
                info.logical_sector, self.logical_sector
            ));
        }
        let my_model = norm(&self.model);
        let their_model = norm(&info.model);
        let model_conflict = my_model.is_some() && their_model.is_some() && my_model != their_model;

        if let Some(mine) = &my_serial {
            match &their_serial {
                Some(theirs) if theirs == mine => {
                    return if model_conflict {
                        Match::Probable(
                            "serial number and size match but the model string differs".into(),
                        )
                    } else {
                        Match::Exact
                    };
                }
                Some(theirs) if !is_generic_serial(theirs) => {
                    return Match::Mismatch(format!(
                        "serial number differs ({} vs {})",
                        info.serial.as_deref().unwrap_or("").trim(),
                        self.serial.as_deref().unwrap_or("").trim()
                    ));
                }
                _ => {
                    if model_conflict {
                        return Match::Mismatch(
                            "model differs and the candidate has no usable serial number".into(),
                        );
                    }
                    return Match::Probable(
                        "size matches but the candidate reports no usable serial number".into(),
                    );
                }
            }
        }
        // Weak identity on our side.
        // We had a generic serial and the candidate reports a unique one. (If
        // we never had a serial, a candidate that has one may still be the
        // same medium: some bridges only report it sometimes.)
        if self.serial.is_some()
            && their_serial
                .as_deref()
                .is_some_and(|s| !is_generic_serial(s))
        {
            return Match::Mismatch("candidate reports a different serial number".into());
        }
        if model_conflict {
            return Match::Mismatch(format!(
                "model differs ({} vs {})",
                info.model.as_deref().unwrap_or(""),
                self.model.as_deref().unwrap_or("")
            ));
        }
        let my_vendor = norm(&self.vendor);
        let their_vendor = norm(&info.vendor);
        if my_vendor.is_some() && their_vendor.is_some() && my_vendor != their_vendor {
            return Match::Mismatch("vendor differs".into());
        }
        if self.stable_id.is_some() && self.stable_id == info.stable_id {
            return Match::Probable(
                "model, size and stable id match, but the device has no unique serial number"
                    .into(),
            );
        }
        if my_model.is_some() && my_model == their_model {
            return Match::Probable(
                "model and size match, but the device has no unique serial number".into(),
            );
        }
        Match::Probable("size matches; no serial number or model to compare".into())
    }
}

/// BLAKE3 (hex) of the first `sector` bytes of `data`.
pub fn hash_first_sector(data: &[u8], sector: usize) -> String {
    blake3_hex(&data[..data.len().min(sector.max(1))])
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskrescue_core::device::{DeviceKind, Transport};

    fn info(serial: Option<&str>, model: Option<&str>, size: u64) -> DeviceInfo {
        DeviceInfo {
            path: "/dev/sdb".into(),
            name: "sdb".into(),
            kind: DeviceKind::UsbFlash,
            transport: Transport::Usb,
            size,
            logical_sector: 512,
            physical_sector: 512,
            model: model.map(String::from),
            vendor: Some("SanDisk".into()),
            serial: serial.map(String::from),
            firmware: None,
            stable_id: None,
            removable: true,
            rotational: true,
            read_only: false,
            partitions: vec![],
            mountpoints: vec![],
            health: None,
            warnings: vec![],
        }
    }

    #[test]
    fn generic_serials() {
        for s in [
            "0123456789ABCDEF",
            "000000000",
            "SN0001",
            "General_UDisk",
            "AAAAAAAAAAAA",
            "0x00000000",
            "abc",
            "123456789",
        ] {
            assert!(is_generic_serial(s), "{s}");
        }
        for s in [
            "4C530001231012345678",
            "WD-WCC6Y1234567",
            "0x1b2c3d4e",
            "S3Z9NB0K123456X",
        ] {
            assert!(!is_generic_serial(s), "{s}");
        }
    }

    #[test]
    fn exact_probable_mismatch() {
        let base = info(Some("4C530001231012345678"), Some("Cruzer Blade"), 1 << 30);
        let fp = Fingerprint::of(&base, Some(&[7u8; 4096]));
        assert!(fp.has_strong_serial());
        assert_eq!(fp.first_sector_hash, Some(blake3_hex(&[7u8; 512])));
        let mut renamed = base.clone();
        renamed.name = "sdc".into();
        renamed.path = "/dev/sdc".into();
        assert_eq!(fp.matches(&renamed), Match::Exact);

        let shrunk = info(Some("4C530001231012345678"), Some("Cruzer Blade"), 0);
        match fp.matches(&shrunk) {
            Match::Mismatch(r) => assert!(r.contains("size changed"), "{r}"),
            m => panic!("{m:?}"),
        }
        let other = info(Some("FFFF0001231012345678"), Some("Cruzer Blade"), 1 << 30);
        assert!(fp.matches(&other).is_mismatch());
        let no_serial = info(None, Some("Cruzer Blade"), 1 << 30);
        assert!(matches!(fp.matches(&no_serial), Match::Probable(_)));
        let mut sector = base.clone();
        sector.logical_sector = 4096;
        assert!(fp.matches(&sector).is_mismatch());
    }

    #[test]
    fn weak_identities() {
        let base = info(Some("0123456789ABCDEF"), Some("USB DISK"), 8 << 30);
        let fp = Fingerprint::of(&base, None);
        assert!(!fp.has_strong_serial());
        assert!(matches!(fp.matches(&base), Match::Probable(_)));
        let other_model = info(Some("0123456789ABCDEF"), Some("Other"), 8 << 30);
        assert!(fp.matches(&other_model).is_mismatch());
        let unique = info(Some("REALSERIAL12345"), Some("USB DISK"), 8 << 30);
        assert!(fp.matches(&unique).is_mismatch());

        let none = info(None, None, 4 << 30);
        let fp = Fingerprint::of(&none, None);
        assert!(matches!(fp.matches(&none), Match::Probable(_)));
        let mut stable = none.clone();
        stable.stable_id = Some("usb-Generic_Flash_Disk-0:0".into());
        let fp = Fingerprint::of(&stable, None);
        match fp.matches(&stable) {
            Match::Probable(r) => assert!(r.contains("stable id")),
            m => panic!("{m:?}"),
        }
    }

    #[test]
    fn serde_roundtrip() {
        let fp = Fingerprint::of(&info(Some("X1234567"), None, 42), Some(b"abc"));
        let j = serde_json::to_string(&fp).unwrap();
        let back: Fingerprint = serde_json::from_str(&j).unwrap();
        assert_eq!(fp, back);
    }
}
