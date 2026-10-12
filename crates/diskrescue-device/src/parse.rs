//! Pure parsers for the text and binary records the device layer reads.
//!
//! Everything here works on byte slices or strings that came from the
//! kernel or from udev: sysfs `MAJ:MIN` pairs, the udev database
//! (`/run/udev/data/b<maj>:<min>`), `/proc/self/mountinfo`, `/proc/swaps`,
//! SCSI VPD pages 0x80 (unit serial number) and 0xB1 (block device
//! characteristics) and the MMC CSD register. None of it does I/O, so it is
//! compiled on every platform and fuzzed by the tests.
//!
//! All parsers are total: hostile or truncated input yields `None` or an
//! empty result, never a panic. Loops are bounded by the input length and by
//! explicit line limits.

use std::collections::BTreeMap;
use std::path::PathBuf;

use diskrescue_core::util::bytes;

/// Maximum number of lines looked at in any line-oriented record. Real
/// udev records have a few dozen lines and mountinfo a few hundred.
pub const MAX_LINES: usize = 65_536;

/// Parse a sysfs `dev` attribute (`"8:16"`, `"259:0\n"`) into
/// `(major, minor)`.
pub fn parse_majmin(s: &str) -> Option<(u32, u32)> {
    let s = s.trim();
    let (a, b) = s.split_once(':')?;
    let maj = a.trim().parse::<u32>().ok()?;
    let min = b.trim().parse::<u32>().ok()?;
    Some((maj, min))
}

/// One record of the udev database (`/run/udev/data/b<maj>:<min>`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UdevRecord {
    /// `S:` lines: symlinks relative to `/dev` (`disk/by-id/usb-...`).
    pub links: Vec<String>,
    /// `E:` lines: properties (`ID_SERIAL_SHORT` -> `4C53...`).
    pub props: BTreeMap<String, String>,
}

impl UdevRecord {
    /// Property value, if present and non-empty.
    pub fn prop(&self, key: &str) -> Option<&str> {
        self.props
            .get(key)
            .map(|s| s.as_str())
            .filter(|s| !s.trim().is_empty())
    }

    /// `ID_ATA_ROTATION_RATE_RPM` as a number (0 = non-rotating).
    pub fn ata_rotation_rpm(&self) -> Option<u32> {
        self.prop("ID_ATA_ROTATION_RATE_RPM")?.trim().parse().ok()
    }

    /// The `disk/by-id/` link names (without the directory part).
    pub fn by_id_links(&self) -> Vec<String> {
        self.links
            .iter()
            .filter_map(|l| l.strip_prefix("disk/by-id/"))
            .filter(|l| !l.is_empty() && !l.contains('/'))
            .map(|l| l.to_string())
            .collect()
    }
}

/// Parse the udev database text format. Unknown prefixes (`L:`, `W:`, `G:`,
/// `Q:`, `I:`, `V:` and anything else) are ignored. A property value may
/// itself contain `=`; only the first `=` separates key and value.
pub fn parse_udev_db(text: &str) -> UdevRecord {
    let mut rec = UdevRecord::default();
    for line in text.lines().take(MAX_LINES) {
        let line = line.trim_end_matches('\r');
        if let Some(link) = line.strip_prefix("S:") {
            if !link.is_empty() {
                rec.links.push(link.to_string());
            }
        } else if let Some(prop) = line.strip_prefix("E:") {
            if let Some((k, v)) = prop.split_once('=') {
                if !k.is_empty() {
                    rec.props.insert(k.to_string(), v.to_string());
                }
            }
        }
    }
    rec
}

/// Decode udev's `\xNN` escapes (`ID_MODEL_ENC=WDC\x20WD10EZEX`). Invalid
/// escapes are kept literally. The result is trimmed.
pub fn decode_udev_enc(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0usize;
    while i < b.len() {
        if b[i] == b'\\' && b.get(i + 1) == Some(&b'x') {
            let hex = b
                .get(i + 2..i + 4)
                .and_then(|h| std::str::from_utf8(h).ok());
            if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 4;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).trim().to_string()
}

/// Decode the octal escapes used in mountinfo and `/proc/swaps`
/// (`\040` space, `\011` tab, `\012` newline, `\134` backslash).
pub fn unescape_octal(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0usize;
    while i < b.len() {
        if b[i] == b'\\' {
            if let Some(d) = b.get(i + 1..i + 4) {
                if d.iter().all(|c| (b'0'..=b'7').contains(c)) {
                    let v =
                        (d[0] - b'0') as u32 * 64 + (d[1] - b'0') as u32 * 8 + (d[2] - b'0') as u32;
                    if v <= 255 {
                        out.push(v as u8);
                        i += 4;
                        continue;
                    }
                }
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// One line of `/proc/self/mountinfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountEntry {
    /// Field 1: mount id.
    pub mount_id: u64,
    /// Field 2: parent mount id.
    pub parent_id: u64,
    /// Field 3: `st_dev` of files on this mount. Anonymous (`0:NN`) for
    /// btrfs, overlayfs, fuse and other non-block filesystems.
    pub devt: (u32, u32),
    /// Field 4: root of the mount within the filesystem (unescaped).
    pub root: String,
    /// Field 5: mount point (unescaped).
    pub mount_point: PathBuf,
    /// Field 6: per-mount options.
    pub options: String,
    /// Field 9: filesystem type.
    pub fs_type: String,
    /// Field 10: mount source (`/dev/sdb1`, `tmpfs`, ...), unescaped.
    pub source: String,
}

/// Parse `/proc/self/mountinfo`. Malformed lines are skipped.
pub fn parse_mountinfo(text: &str) -> Vec<MountEntry> {
    let mut out = Vec::new();
    for line in text.lines().take(MAX_LINES) {
        if let Some(e) = parse_mountinfo_line(line) {
            out.push(e);
        }
    }
    out
}

fn parse_mountinfo_line(line: &str) -> Option<MountEntry> {
    let fields: Vec<&str> = line.split(' ').filter(|f| !f.is_empty()).collect();
    if fields.len() < 10 {
        return None;
    }
    let mount_id = fields[0].parse().ok()?;
    let parent_id = fields[1].parse().ok()?;
    let devt = parse_majmin(fields[2])?;
    let root = unescape_octal(fields[3]);
    let mount_point = PathBuf::from(unescape_octal(fields[4]));
    let options = fields[5].to_string();
    // Optional fields follow field 6 until a lone "-".
    let sep = fields.iter().skip(6).position(|f| *f == "-")? + 6;
    let fs_type = fields.get(sep + 1)?.to_string();
    let source = unescape_octal(fields.get(sep + 2)?);
    Some(MountEntry {
        mount_id,
        parent_id,
        devt,
        root,
        mount_point,
        options,
        fs_type,
        source,
    })
}

/// Parse `/proc/swaps`: returns the first column (device or file path) of
/// every entry after the header line.
pub fn parse_swaps(text: &str) -> Vec<String> {
    text.lines()
        .take(MAX_LINES)
        .skip(1)
        .filter_map(|l| l.split_whitespace().next())
        .map(unescape_octal)
        .collect()
}

/// Decode SCSI VPD page 0x80 (Unit Serial Number). Byte 1 must be 0x80;
/// bytes 2-3 hold the big-endian length of the serial that starts at byte
/// 4. The length is clamped to the bytes present. Non-printable bytes are
/// dropped; an empty serial yields `None`.
pub fn vpd80_serial(page: &[u8]) -> Option<String> {
    if bytes::u8_at(page, 1)? != 0x80 {
        return None;
    }
    let len = bytes::u16be(page, 2)? as usize;
    let avail = page.len().saturating_sub(4);
    let body = bytes::slice(page, 4, len.min(avail))?;
    let s: String = body
        .iter()
        .filter(|&&c| (0x20..0x7f).contains(&c))
        .map(|&c| c as char)
        .collect();
    let s = s.trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// What VPD page 0xB1 says about the rotation rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rotation {
    /// `0x0001`: non-rotating medium (SSD, flash).
    NonRotating,
    /// `0x0401..=0xFFFE`: nominal rotation rate in RPM.
    Rpm(u16),
}

/// Decoded SCSI VPD page 0xB1 (Block Device Characteristics).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VpdB1 {
    /// Medium rotation rate; `None` when not reported or reserved.
    pub rotation: Option<Rotation>,
    /// Product type (byte 6): 0 not indicated, 1 CFast, 2 CompactFlash,
    /// 3 Memory Stick, 4 MultiMediaCard, 5 SD card, 6 XQD, 7 UFS card.
    pub product_type: u8,
    /// Nominal form factor (byte 7 bits 3:0).
    pub form_factor: u8,
    /// Zoned capabilities (byte 8 bits 5:4): 1 host-aware, 2 host-managed.
    pub zoned: u8,
}

impl VpdB1 {
    /// True when the product type names a memory card (CF, MS, MMC, SD,
    /// UFS card), i.e. the device is a card reader with a card inserted.
    pub fn is_memory_card(&self) -> bool {
        matches!(self.product_type, 2..=5 | 7)
    }
}

/// Decode SCSI VPD page 0xB1. Mirrors the kernel's own check (sd.c
/// `sd_read_block_characteristics`: the page must be longer than 8 bytes).
pub fn vpd_b1(page: &[u8]) -> Option<VpdB1> {
    if page.len() <= 8 || page[1] != 0xB1 {
        return None;
    }
    let rate = bytes::u16be(page, 4)?;
    let rotation = match rate {
        0x0001 => Some(Rotation::NonRotating),
        0x0401..=0xFFFE => Some(Rotation::Rpm(rate)),
        _ => None,
    };
    Some(VpdB1 {
        rotation,
        product_type: page[6],
        form_factor: page[7] & 0x0F,
        zoned: (page[8] >> 4) & 0x03,
    })
}

/// `CSD_STRUCTURE` (bits 127:126) of an MMC/SD CSD register given as the
/// 32 hex digits sysfs prints: 0 = SDSC, 1 = SDHC/SDXC, 2 = SDUC.
pub fn csd_structure(csd_hex: &str) -> Option<u8> {
    let s = csd_hex.trim();
    if s.len() != 32 || !s.bytes().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let first = u8::from_str_radix(&s[..2], 16).ok()?;
    Some(first >> 6)
}

/// Parse a hexadecimal sysfs attribute such as `0x000003` or `05e3`.
pub fn parse_hex_u32(s: &str) -> Option<u32> {
    let s = s.trim();
    let s = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);
    if s.is_empty() || s.len() > 8 {
        return None;
    }
    u32::from_str_radix(s, 16).ok()
}

/// Lexically normalise a path: drop `.` components and resolve `..`
/// against the preceding component without touching the filesystem.
pub fn normalize_lexically(p: &std::path::Path) -> PathBuf {
    use std::path::Component;
    let mut out: Vec<Component> = Vec::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => match out.last() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir) | Some(Component::Prefix(_)) => {}
                _ => out.push(c),
            },
            other => out.push(other),
        }
    }
    out.iter().map(|c| c.as_os_str()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn majmin() {
        assert_eq!(parse_majmin("8:16\n"), Some((8, 16)));
        assert_eq!(parse_majmin(" 259:0 "), Some((259, 0)));
        assert_eq!(parse_majmin("8"), None);
        assert_eq!(parse_majmin("a:b"), None);
        assert_eq!(parse_majmin("99999999999:1"), None);
    }

    #[test]
    fn udev_db_parsing() {
        let text = "S:disk/by-id/usb-SanDisk_Cruzer_Blade_4C53-0:0\nS:disk/by-path/pci-0000:00:14.0-usb-0:3:1.0-scsi-0:0:0:0\nL:-100\nW:12\nI:123\nE:ID_SERIAL_SHORT=4C53\nE:ID_FOO=a=b=c\nE:=novalue\nX:unknown\nE:ID_ATA_ROTATION_RATE_RPM=7200\nV:1\n";
        let r = parse_udev_db(text);
        assert_eq!(r.links.len(), 2);
        assert_eq!(r.prop("ID_SERIAL_SHORT"), Some("4C53"));
        assert_eq!(r.prop("ID_FOO"), Some("a=b=c"));
        assert_eq!(r.ata_rotation_rpm(), Some(7200));
        assert_eq!(
            r.by_id_links(),
            vec!["usb-SanDisk_Cruzer_Blade_4C53-0:0".to_string()]
        );
        assert!(!r.props.contains_key(""));
    }

    #[test]
    fn udev_enc() {
        assert_eq!(
            decode_udev_enc("WDC\\x20WD10EZEX\\x20\\x20"),
            "WDC WD10EZEX"
        );
        assert_eq!(decode_udev_enc("bad\\xZZ"), "bad\\xZZ");
        assert_eq!(decode_udev_enc("tail\\x"), "tail\\x");
        assert_eq!(decode_udev_enc("tail\\x4"), "tail\\x4");
    }

    #[test]
    fn mountinfo_parsing_and_escapes() {
        let text = "33 2 259:8 / / rw,relatime shared:1 - ext4 /dev/nvme1n1p3 rw,stripe=32\n\
                    40 33 8:17 / /mnt/my\\040disk rw,nosuid shared:5 master:2 - vfat /dev/sdb1 rw\n\
                    41 33 0:45 /@home /home rw - btrfs /dev/sdc2 rw\n\
                    garbage line\n\
                    42 33 0:50 / /run rw - tmpfs\n";
        let m = parse_mountinfo(text);
        assert_eq!(m.len(), 3);
        assert_eq!(m[0].devt, (259, 8));
        assert_eq!(m[0].source, "/dev/nvme1n1p3");
        assert_eq!(m[1].mount_point, PathBuf::from("/mnt/my disk"));
        assert_eq!(m[1].fs_type, "vfat");
        assert_eq!(m[2].root, "/@home");
        assert_eq!(unescape_octal("a\\134b\\011c\\999"), "a\\b\tc\\999");
    }

    #[test]
    fn swaps_parsing() {
        let t = "Filename\tType\tSize\tUsed\tPriority\n/dev/sdb2   partition\t1\t0\t-2\n/swap\\040file file 1 0 -3\n";
        assert_eq!(parse_swaps(t), vec!["/dev/sdb2", "/swap file"]);
        assert!(parse_swaps("").is_empty());
    }

    #[test]
    fn vpd_pages() {
        let mut p80 = vec![0u8, 0x80, 0, 20];
        p80.extend_from_slice(b"4C530001231012345678");
        assert_eq!(vpd80_serial(&p80).as_deref(), Some("4C530001231012345678"));
        // Length larger than the page: clamped.
        let short = [0u8, 0x80, 0x10, 0, b'A', b'B'];
        assert_eq!(vpd80_serial(&short).as_deref(), Some("AB"));
        assert_eq!(vpd80_serial(&[0, 0x83, 0, 1, b'x']), None);
        assert_eq!(vpd80_serial(&[0, 0x80, 0, 3, b' ', 0, b' ']), None);
        assert_eq!(vpd80_serial(&[]), None);

        let mut b1 = vec![0u8; 64];
        b1[1] = 0xB1;
        b1[3] = 0x3C;
        b1[5] = 1;
        let v = vpd_b1(&b1).unwrap();
        assert_eq!(v.rotation, Some(Rotation::NonRotating));
        b1[4] = 0x1C;
        b1[5] = 0x20;
        assert_eq!(vpd_b1(&b1).unwrap().rotation, Some(Rotation::Rpm(7200)));
        b1[4] = 0;
        b1[5] = 0;
        b1[6] = 5;
        let v = vpd_b1(&b1).unwrap();
        assert_eq!(v.rotation, None);
        assert!(v.is_memory_card());
        assert_eq!(vpd_b1(&b1[..8]), None);
    }

    #[test]
    fn csd_and_hex() {
        assert_eq!(csd_structure("400e00325b590000ee7f7f800a404000"), Some(1));
        assert_eq!(csd_structure("002e00325b5aa3a9ffffff800a800000"), Some(0));
        assert_eq!(csd_structure("40"), None);
        assert_eq!(csd_structure("zz0e00325b590000ee7f7f800a404000"), None);
        assert_eq!(parse_hex_u32("0x000003"), Some(3));
        assert_eq!(parse_hex_u32("05e3\n"), Some(0x05e3));
        assert_eq!(parse_hex_u32("0x"), None);
        assert_eq!(parse_hex_u32("123456789"), None);
    }

    #[test]
    fn lexical_normalisation() {
        use std::path::Path;
        assert_eq!(
            normalize_lexically(Path::new("/dev/disk/by-id/../../sdb")),
            PathBuf::from("/dev/sdb")
        );
        assert_eq!(
            normalize_lexically(Path::new("/../a/./b")),
            PathBuf::from("/a/b")
        );
    }

    #[test]
    fn hostile_inputs_never_panic() {
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        for round in 0..2000 {
            let len = (round % 97) as usize;
            let mut buf = Vec::with_capacity(len);
            for _ in 0..len {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                buf.push((seed & 0xff) as u8);
            }
            let text = String::from_utf8_lossy(&buf);
            let _ = parse_majmin(&text);
            let _ = parse_udev_db(&text);
            let _ = decode_udev_enc(&text);
            let _ = unescape_octal(&text);
            let _ = parse_mountinfo(&text);
            let _ = parse_swaps(&text);
            let _ = vpd80_serial(&buf);
            let _ = vpd_b1(&buf);
            let _ = csd_structure(&text);
            let _ = parse_hex_u32(&text);
        }
    }
}
