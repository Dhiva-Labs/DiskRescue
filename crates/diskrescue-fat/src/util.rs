//! Small helpers shared by the FAT and exFAT parsers: OEM code page
//! decoding, name sanitising, the three checksums defined by the
//! specifications, media signatures and a tiny pseudo-random generator for
//! tests.

/// CP437 (the default OEM code page of FAT short names) for bytes 0x80..=0xFF.
const CP437_HIGH: [char; 128] = [
    'Ç', 'ü', 'é', 'â', 'ä', 'à', 'å', 'ç', 'ê', 'ë', 'è', 'ï', 'î', 'ì', 'Ä', 'Å', //
    'É', 'æ', 'Æ', 'ô', 'ö', 'ò', 'û', 'ù', 'ÿ', 'Ö', 'Ü', '¢', '£', '¥', '₧', 'ƒ', //
    'á', 'í', 'ó', 'ú', 'ñ', 'Ñ', 'ª', 'º', '¿', '⌐', '¬', '½', '¼', '¡', '«', '»', //
    '░', '▒', '▓', '│', '┤', '╡', '╢', '╖', '╕', '╣', '║', '╗', '╝', '╜', '╛', '┐', //
    '└', '┴', '┬', '├', '─', '┼', '╞', '╟', '╚', '╔', '╩', '╦', '╠', '═', '╬', '╧', //
    '╨', '╤', '╥', '╙', '╘', '╒', '╓', '╫', '╪', '┘', '┌', '█', '▄', '▌', '▐', '▀', //
    'α', 'ß', 'Γ', 'π', 'Σ', 'σ', 'µ', 'τ', 'Φ', 'Θ', 'Ω', 'δ', '∞', 'φ', 'ε', '∩', //
    '≡', '±', '≥', '≤', '⌠', '⌡', '÷', '≈', '°', '∙', '·', '√', 'ⁿ', '²', '■', '\u{a0}',
];

/// Decode bytes of a FAT short name or label using CP437.
pub(crate) fn decode_oem(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&b| {
            if b < 0x80 {
                b as char
            } else {
                CP437_HIGH[(b - 0x80) as usize]
            }
        })
        .collect()
}

/// Make a name safe to use as a path component: no separators, no NUL or
/// control characters, never empty, never `.` or `..`.
pub(crate) fn sanitize_name(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if c == '/' || c == '\\' || (c as u32) < 0x20 {
                '_'
            } else {
                c
            }
        })
        .collect();
    if out.is_empty() {
        out.push('_');
    }
    if out == "." || out == ".." {
        out.insert(0, '_');
    }
    out
}

/// Join a parent path and a component (`""` parent means the volume root).
pub(crate) fn join_path(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        let mut s = String::with_capacity(parent.len() + 1 + name.len());
        s.push_str(parent);
        s.push('/');
        s.push_str(name);
        s
    }
}

/// Checksum of an 11-byte short name, stored in every LFN entry
/// (Microsoft FAT specification, section 7).
pub(crate) fn lfn_checksum(short: &[u8]) -> u8 {
    let mut sum: u8 = 0;
    for &b in short.iter().take(11) {
        sum = ((sum & 1) << 7).wrapping_add(sum >> 1).wrapping_add(b);
    }
    sum
}

/// exFAT entry set checksum over `bytes` (the whole set), skipping bytes 2
/// and 3 (the checksum field itself).
pub(crate) fn exfat_set_checksum(bytes: &[u8]) -> u16 {
    let mut cs: u16 = 0;
    for (i, &b) in bytes.iter().enumerate() {
        if i == 2 || i == 3 {
            continue;
        }
        cs = (if cs & 1 != 0 { 0x8000u16 } else { 0 })
            .wrapping_add(cs >> 1)
            .wrapping_add(b as u16);
    }
    cs
}

/// exFAT boot region checksum over the first 11 sectors, skipping the
/// VolumeFlags (106, 107) and PercentInUse (112) bytes.
pub(crate) fn exfat_boot_checksum(bytes: &[u8]) -> u32 {
    let mut cs: u32 = 0;
    for (i, &b) in bytes.iter().enumerate() {
        if i == 106 || i == 107 || i == 112 {
            continue;
        }
        cs = (if cs & 1 != 0 { 0x8000_0000u32 } else { 0 })
            .wrapping_add(cs >> 1)
            .wrapping_add(b as u32);
    }
    cs
}

/// exFAT up-case table checksum (same rotation as the boot checksum, no
/// skipped bytes).
pub(crate) fn exfat_table_checksum(bytes: &[u8]) -> u32 {
    let mut cs: u32 = 0;
    for &b in bytes {
        cs = (if cs & 1 != 0 { 0x8000_0000u32 } else { 0 })
            .wrapping_add(cs >> 1)
            .wrapping_add(b as u32);
    }
    cs
}

/// exFAT NameHash over already up-cased UTF-16 code units.
pub(crate) fn exfat_name_hash(upcased: &[u16]) -> u16 {
    let mut h: u16 = 0;
    for &u in upcased {
        for b in u.to_le_bytes() {
            h = (if h & 1 != 0 { 0x8000u16 } else { 0 })
                .wrapping_add(h >> 1)
                .wrapping_add(b as u16);
        }
    }
    h
}

/// True when `head` starts with the signature of a common photo or video
/// container. Used to pick between candidate first clusters of deleted
/// files whose cluster number is incomplete.
pub(crate) fn looks_like_media(head: &[u8]) -> bool {
    let starts = |p: &[u8]| head.len() >= p.len() && &head[..p.len()] == p;
    let at = |off: usize, p: &[u8]| head.len() >= off + p.len() && &head[off..off + p.len()] == p;
    starts(&[0xFF, 0xD8, 0xFF])
        || starts(&[0x89, b'P', b'N', b'G'])
        || starts(b"RIFF")
        || starts(b"II*\0")
        || starts(b"MM\0*")
        || starts(b"IIRO")
        || starts(b"IIRS")
        || starts(b"IIU\0")
        || starts(b"II\x1a\0\0\0HEAPCCDR")
        || starts(b"FOVb")
        || starts(b"FUJIFILM")
        || starts(b"GIF8")
        || starts(&[0x1A, 0x45, 0xDF, 0xA3])
        || starts(&[0x00, 0x00, 0x01, 0xBA])
        // ISO BMFF / QuickTime: a top-level box type at offset 4.
        || [&b"ftyp"[..], b"moov", b"mdat", b"wide", b"free", b"skip", b"pnot"]
            .iter()
            .any(|t| at(4, t))
        // MPEG-TS (188-byte packets) and AVCHD/M2TS (192-byte packets).
        || (at(0, &[0x47]) && at(188, &[0x47]))
        || (at(4, &[0x47]) && at(196, &[0x47]))
}

/// True when a file name has a photo/video extension (used to decide
/// whether a content-signature guess is meaningful for it).
pub(crate) fn is_media_name(name: &str) -> bool {
    const EXT: &[&str] = &[
        "jpg", "jpeg", "jpe", "png", "gif", "tif", "tiff", "heic", "heif", "avif", "webp", "cr2",
        "cr3", "nef", "arw", "dng", "orf", "rw2", "raf", "pef", "srw", "mp4", "m4v", "mov", "3gp",
        "avi", "mkv", "webm", "mts", "m2ts", "mpg", "mpeg", "vob", "lrv", "thm",
    ];
    match name.rfind('.') {
        Some(i) => {
            let ext = name[i + 1..].to_ascii_lowercase();
            EXT.contains(&ext.as_str())
        }
        None => false,
    }
}

/// Ceiling division for `u64`, treating a zero divisor as 1.
pub(crate) fn div_ceil(a: u64, b: u64) -> u64 {
    a.div_ceil(b.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oem_decoding_and_sanitising() {
        assert_eq!(decode_oem(b"ABC"), "ABC");
        assert_eq!(decode_oem(&[0xE5, 0x81]), "σü");
        assert_eq!(sanitize_name("a/b\\c\u{1}"), "a_b_c_");
        assert_eq!(sanitize_name(""), "_");
        assert_eq!(sanitize_name(".."), "_..");
        assert_eq!(join_path("", "x"), "x");
        assert_eq!(join_path("a/b", "x"), "a/b/x");
    }

    #[test]
    fn lfn_checksum_known_value() {
        // Checksum of "FOO     BAR" computed with the reference algorithm.
        let mut sum: u8 = 0;
        for &b in b"FOO     BAR" {
            sum = (if sum & 1 != 0 { 0x80u8 } else { 0 })
                .wrapping_add(sum >> 1)
                .wrapping_add(b);
        }
        assert_eq!(lfn_checksum(b"FOO     BAR"), sum);
    }

    #[test]
    fn exfat_hash_of_empty_is_zero() {
        assert_eq!(exfat_name_hash(&[]), 0);
        assert_eq!(exfat_set_checksum(&[0, 0, 0xFF, 0xFF]), 0);
        assert!(looks_like_media(&[0xFF, 0xD8, 0xFF, 0xE0]));
        assert!(looks_like_media(b"\0\0\0\x18ftypmp42"));
        assert!(!looks_like_media(b"hello"));
    }
}
