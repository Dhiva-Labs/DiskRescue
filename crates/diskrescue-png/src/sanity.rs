//! Plausibility checks for the contents of well-known chunks.
//!
//! A chunk whose CRC matches can still carry nonsense (a flipped bit that was
//! made consistent by a "fix", a buggy writer, damage on a medium that
//! recomputes checksums). Strict decoders reject such files for reasons that
//! have nothing to do with the pixels, so validation treats a malformed
//! ancillary chunk as damage and repair drops it.
//!
//! Only chunks with a rigid layout are judged; unknown ancillary chunks are
//! always accepted.

use std::io::Read;

use crate::ihdr::Ihdr;

/// Chunks larger than this are not inspected.
pub(crate) const MAX_CHECK_LEN: u32 = 256 * 1024;

/// Critical chunk types this crate understands.
pub(crate) fn is_known_critical(kind: &[u8; 4]) -> bool {
    matches!(kind, b"IHDR" | b"PLTE" | b"IDAT" | b"IEND" | b"CgBI")
}

/// Ancillary chunk types whose content cannot change how the picture looks,
/// so a failed CRC is repaired by rewriting it, provided the content is
/// plausible. Colour-affecting chunks (gAMA, cHRM, sRGB, sBIT, bKGD, ...) are
/// dropped instead.
pub(crate) fn is_vouchable(kind: &[u8; 4]) -> bool {
    matches!(
        kind,
        b"tEXt"
            | b"iTXt"
            | b"zTXt"
            | b"iCCP"
            | b"tIME"
            | b"pHYs"
            | b"eXIf"
            | b"oFFs"
            | b"sCAL"
            | b"pCAL"
            | b"sTER"
            | b"acTL"
            | b"fcTL"
            | b"fdAT"
            | b"tRNS"
    )
}

fn keyword_len(data: &[u8]) -> Result<usize, String> {
    let n = data
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| "keyword is not terminated".to_string())?;
    if n == 0 || n > 79 {
        return Err(format!("keyword length {n}"));
    }
    let kw = &data[..n];
    if !kw.iter().all(|&b| (32..=126).contains(&b) || b >= 161) {
        return Err("keyword contains control characters".to_string());
    }
    if kw[0] == b' ' || kw[n - 1] == b' ' {
        return Err("keyword has leading or trailing spaces".to_string());
    }
    Ok(n)
}

fn inflates(z: &[u8]) -> bool {
    let mut sink = Vec::new();
    flate2::read::ZlibDecoder::new(z)
        .take(8 << 20)
        .read_to_end(&mut sink)
        .is_ok()
}

fn want_len(name: &str, data: &[u8], n: usize) -> Result<(), String> {
    if data.len() == n {
        Ok(())
    } else {
        Err(format!("{name} has {} bytes, expected {n}", data.len()))
    }
}

/// Check the contents of a chunk. `Ok` also for chunk types that are not
/// judged. `ihdr` enables the checks that depend on the colour type.
pub(crate) fn check(kind: &[u8; 4], data: &[u8], ihdr: Option<&Ihdr>) -> Result<(), String> {
    match kind {
        b"gAMA" => want_len("gAMA", data, 4),
        b"cICP" => want_len("cICP", data, 4),
        b"cHRM" => want_len("cHRM", data, 32),
        b"mDCV" => want_len("mDCV", data, 24),
        b"cLLI" => want_len("cLLI", data, 8),
        b"acTL" => want_len("acTL", data, 8),
        b"fcTL" => want_len("fcTL", data, 26),
        b"sRGB" => {
            want_len("sRGB", data, 1)?;
            if data[0] > 3 {
                return Err(format!("rendering intent {}", data[0]));
            }
            Ok(())
        }
        b"pHYs" => {
            want_len("pHYs", data, 9)?;
            if data[8] > 1 {
                return Err(format!("unit specifier {}", data[8]));
            }
            Ok(())
        }
        b"oFFs" => {
            want_len("oFFs", data, 9)?;
            if data[8] > 1 {
                return Err(format!("unit specifier {}", data[8]));
            }
            Ok(())
        }
        b"tIME" => {
            want_len("tIME", data, 7)?;
            let (mo, d, h, mi, s) = (data[2], data[3], data[4], data[5], data[6]);
            if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || s > 60 {
                return Err("impossible date".to_string());
            }
            Ok(())
        }
        b"fdAT" => {
            if data.len() < 4 {
                Err("fdAT shorter than its sequence number".to_string())
            } else {
                Ok(())
            }
        }
        b"hIST" => {
            if data.len() < 2 || data.len() % 2 != 0 || data.len() > 512 {
                Err(format!("hIST has {} bytes", data.len()))
            } else {
                Ok(())
            }
        }
        b"tEXt" => {
            let n = keyword_len(data)?;
            if data[n + 1..].contains(&0) {
                return Err("text contains NUL".to_string());
            }
            Ok(())
        }
        b"zTXt" => {
            let n = keyword_len(data)?;
            match data.get(n + 1) {
                Some(0) => {}
                _ => return Err("unknown compression method".to_string()),
            }
            if inflates(&data[n + 2..]) {
                Ok(())
            } else {
                Err("compressed text does not inflate".to_string())
            }
        }
        b"iCCP" => {
            let n = keyword_len(data)?;
            match data.get(n + 1) {
                Some(0) => {}
                _ => return Err("unknown compression method".to_string()),
            }
            if inflates(&data[n + 2..]) {
                Ok(())
            } else {
                Err("profile does not inflate".to_string())
            }
        }
        b"iTXt" => {
            let n = keyword_len(data)?;
            let flag = *data.get(n + 1).ok_or("truncated iTXt")?;
            let method = *data.get(n + 2).ok_or("truncated iTXt")?;
            if flag > 1 || method != 0 {
                return Err("bad iTXt compression fields".to_string());
            }
            let rest = &data[n + 3..];
            let l1 = rest
                .iter()
                .position(|&b| b == 0)
                .ok_or("language tag is not terminated")?;
            let rest = &rest[l1 + 1..];
            let l2 = rest
                .iter()
                .position(|&b| b == 0)
                .ok_or("translated keyword is not terminated")?;
            let text = &rest[l2 + 1..];
            if flag == 1 && !inflates(text) {
                return Err("compressed text does not inflate".to_string());
            }
            Ok(())
        }
        b"eXIf" => {
            let t = data.strip_prefix(b"Exif\0\0").unwrap_or(data);
            if t.len() >= 8 && (t.starts_with(b"II*\0") || t.starts_with(b"MM\0*")) {
                Ok(())
            } else {
                Err("not a TIFF header".to_string())
            }
        }
        b"sBIT" => match ihdr {
            Some(ih) => {
                let want = match ih.color_type {
                    0 => 1,
                    2 | 3 => 3,
                    4 => 2,
                    _ => 4,
                };
                want_len("sBIT", data, want)
            }
            None => Ok(()),
        },
        b"bKGD" => match ihdr {
            Some(ih) => {
                let want = match ih.color_type {
                    0 | 4 => 2,
                    3 => 1,
                    _ => 6,
                };
                want_len("bKGD", data, want)
            }
            None => Ok(()),
        },
        b"tRNS" => match ihdr {
            Some(ih) => match ih.color_type {
                0 => want_len("tRNS", data, 2),
                2 => want_len("tRNS", data, 6),
                3 => {
                    if (1..=256).contains(&data.len()) {
                        Ok(())
                    } else {
                        Err(format!("tRNS has {} entries", data.len()))
                    }
                }
                _ => Err("tRNS is not allowed with an alpha channel".to_string()),
            },
            None => Ok(()),
        },
        _ => Ok(()),
    }
}

/// True when `len` bytes can be a palette for `ihdr`.
pub(crate) fn plte_len_ok(len: u32, ihdr: Option<&Ihdr>) -> bool {
    if len == 0 || len % 3 != 0 || len > 768 {
        return false;
    }
    match ihdr {
        Some(ih) if ih.color_type == 3 => len / 3 <= 1u32 << ih.bit_depth.min(8),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rigid_layouts() {
        assert!(check(b"gAMA", &[0; 4], None).is_ok());
        assert!(check(b"gAMA", &[0; 5], None).is_err());
        assert!(check(b"pHYs", &[0, 0, 0, 1, 0, 0, 0, 1, 1], None).is_ok());
        assert!(check(b"pHYs", &[0, 0, 0, 1, 0, 0, 0, 1, 5], None).is_err());
        assert!(check(b"tIME", &[7, 231, 11, 8, 9, 10, 11], None).is_ok());
        assert!(check(b"tIME", &[7, 231, 13, 8, 9, 10, 11], None).is_err());
        assert!(check(b"sRGB", &[4], None).is_err());
        assert!(check(b"unKn", &[1, 2, 3], None).is_ok());
    }

    #[test]
    fn text_keywords() {
        assert!(check(b"tEXt", b"Title\0Hello", None).is_ok());
        assert!(check(b"tEXt", b"\0Hello", None).is_err());
        assert!(check(b"tEXt", b"NoNul", None).is_err());
        assert!(check(b"tEXt", b" lead\0x", None).is_err());
        assert!(check(b"tEXt", b"k\x01y\0x", None).is_err());
        assert!(check(b"tEXt", b"key\0a\0b", None).is_err());
        let long = vec![b'a'; 80];
        let mut d = long.clone();
        d.push(0);
        assert!(check(b"tEXt", &d, None).is_err());
        assert!(check(b"zTXt", b"k\0\0garbage", None).is_err());
        assert!(check(b"iTXt", b"k\0\0\0\0\0hello", None).is_ok());
        assert!(check(b"iTXt", b"k\0\x07\0\0\0hello", None).is_err());
    }

    #[test]
    fn colour_dependent_chunks() {
        let rgb = Ihdr {
            width: 1,
            height: 1,
            bit_depth: 8,
            color_type: 2,
            compression: 0,
            filter: 0,
            interlace: 0,
        };
        assert!(check(b"bKGD", &[0; 6], Some(&rgb)).is_ok());
        assert!(check(b"bKGD", &[0; 2], Some(&rgb)).is_err());
        assert!(check(b"tRNS", &[0; 6], Some(&rgb)).is_ok());
        assert!(check(b"sBIT", &[8; 3], Some(&rgb)).is_ok());
        assert!(plte_len_ok(48, None));
        assert!(!plte_len_ok(47, None));
        assert!(!plte_len_ok(0, None));
        assert!(!plte_len_ok(771, None));
        let pal = Ihdr {
            color_type: 3,
            bit_depth: 2,
            ..rgb
        };
        assert!(plte_len_ok(12, Some(&pal)));
        assert!(!plte_len_ok(15, Some(&pal)));
    }
}
