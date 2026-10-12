//! What legitimately follows the main image's EOI.
//!
//! After the scan walk finds the real EOI, the file may continue with:
//!
//! * **MPF images** (CIPA DC-007): the APP2 `MPF\0` index gives each extra
//!   image's offset (relative to the MPF TIFF header) and size; images that
//!   start after the main EOI with an SOI extend the file.
//! * **Samsung SEF trailer**: data blocks (`00 00 <type LE> <name_len LE>
//!   name payload`, e.g. `MotionPhoto_Data` holding an MP4), then a `SEFH`
//!   directory, then `<u32 dir_len> "SEFT"` as the last 8 bytes.
//! * **Google motion photo**: XMP says `MotionPhoto`/`MicroVideo` and an
//!   MP4 (`ftyp`...) follows the EOI directly; its end is found by walking
//!   the top-level boxes.
//!
//! Anything else after EOI is not part of the file (cluster slack, the next
//! file). Every search is bounded by the cursor's limit.

use diskrescue_core::format::{Nested, NestedRole};
use diskrescue_core::util::bytes::{u16le, u32be, u32le, u64be};
use diskrescue_core::Range;

use crate::cursor::Cursor;
use crate::markers::Headers;

/// Top-level ISO BMFF boxes accepted while walking a trailer video.
const MP4_BOXES: [&[u8; 4]; 16] = [
    b"ftyp", b"moov", b"mdat", b"free", b"skip", b"wide", b"uuid", b"meta", b"udta", b"mfra",
    b"moof", b"sidx", b"styp", b"emsg", b"prft", b"pdin",
];

/// Known Samsung SEF block names (a block header must carry one of these
/// prefixes to start the SEF search).
const SEF_NAMES: [&str; 10] = [
    "MotionPhoto_Data",
    "Image_UTC_Data",
    "MCC_Data",
    "BackupRestore_Data",
    "EmbeddedAudioFile",
    "SurroundShotVideo",
    "DualShot_",
    "SingleShot",
    "Camera_",
    "Photo_",
];

/// Extent of the file after the main image.
#[derive(Debug, Clone, Default)]
pub(crate) struct AfterEoi {
    /// File-relative end of the whole object.
    pub(crate) end: u64,
    /// Embedded objects found (MPF images, trailer video).
    pub(crate) nested: Vec<Nested>,
    /// Observations.
    pub(crate) notes: Vec<String>,
    /// Kinds of trailer found (`mpf`, `samsung_sef`, `motion_photo`).
    pub(crate) kinds: Vec<&'static str>,
    /// An MPF or trailer runs past the readable data.
    pub(crate) truncated: bool,
}

/// Extend the extent past the EOI that ends at `eoi_end`.
pub(crate) fn extend_after_eoi(cur: &mut Cursor<'_>, h: &Headers, eoi_end: u64) -> AfterEoi {
    let mut out = AfterEoi {
        end: eoi_end,
        ..Default::default()
    };
    let limit = cur.limit();
    if let Some(mpf) = &h.mpf {
        let mut n = 0;
        for (i, e) in mpf.entries.iter().enumerate().take(64) {
            if e.offset == 0 || e.size < 4 {
                continue;
            }
            let start = mpf.tiff_offset + e.offset as u64;
            let end = start + e.size as u64;
            if start < eoi_end {
                continue;
            }
            if start + 3 > limit {
                out.notes
                    .push(format!("MPF image {} lies beyond the readable data", i + 1));
                out.truncated = true;
                continue;
            }
            if cur.byte(start) != Some(0xFF)
                || cur.byte(start + 1) != Some(0xD8)
                || cur.byte(start + 2) != Some(0xFF)
            {
                out.notes.push(format!(
                    "MPF image {} offset {start} does not point at a JPEG",
                    i + 1
                ));
                continue;
            }
            if end > limit {
                out.notes
                    .push(format!("MPF image {} is cut off by the end of data", i + 1));
                out.truncated = true;
            }
            let end = end.min(limit);
            out.end = out.end.max(end);
            let role = match e.type_code() {
                0x020001..=0x020003 => NestedRole::Frame,
                _ => NestedRole::Preview,
            };
            out.nested.push(Nested {
                range: Range::new(start, end),
                format: Some("jpeg".into()),
                role,
            });
            n += 1;
        }
        if n > 0 {
            out.kinds.push("mpf");
            out.notes
                .push(format!("MPF: {} additional image(s) after the main EOI", n));
        }
    }

    let motion_hint = h.motion_photo.as_ref().is_some_and(|m| m.motion_photo);
    for _ in 0..4 {
        let pos = out.end;
        if pos + 8 > limit {
            break;
        }
        // Samsung SEF data blocks.
        if let Some(name) = sef_block_name(cur, pos) {
            if let Some((end, mp4)) = find_sef_end(cur, pos, eoi_end) {
                if let Some(r) = mp4 {
                    out.nested.push(Nested {
                        range: r,
                        format: Some("mp4".into()),
                        role: NestedRole::Trailer,
                    });
                }
                out.nested.push(Nested {
                    range: Range::new(pos, end),
                    format: None,
                    role: NestedRole::Trailer,
                });
                out.kinds.push("samsung_sef");
                out.notes.push(format!(
                    "Samsung SEF trailer ({name}...) of {} bytes",
                    end - pos
                ));
                out.end = end;
                continue;
            }
            out.notes.push(format!(
                "SEF block {name} after EOI but no SEFT directory found"
            ));
            break;
        }
        // A directory directly (blocks absent or already consumed).
        if cur.read_exact(pos, 4).as_deref() == Some(b"SEFH") {
            if let Some((end, _)) = parse_sefh(cur, pos, eoi_end) {
                out.nested.push(Nested {
                    range: Range::new(pos, end),
                    format: None,
                    role: NestedRole::Trailer,
                });
                out.kinds.push("samsung_sef");
                out.end = end;
                continue;
            }
        }
        // Google motion photo: MP4 right after the image.
        if motion_hint && cur.read_exact(pos + 4, 4).as_deref() == Some(b"ftyp") {
            if let Some((end, truncated)) = walk_mp4(cur, pos) {
                if let Some(off) = h.motion_photo.as_ref().and_then(|m| m.micro_video_offset) {
                    if pos + off != end {
                        out.notes.push(format!(
                            "MicroVideoOffset says the video ends at {}, box walk says {end}",
                            pos + off
                        ));
                    }
                }
                out.nested.push(Nested {
                    range: Range::new(pos, end),
                    format: Some("mp4".into()),
                    role: NestedRole::Trailer,
                });
                out.kinds.push("motion_photo");
                out.notes
                    .push(format!("motion photo video of {} bytes", end - pos));
                if truncated {
                    out.truncated = true;
                    out.notes.push("motion photo video is cut off".into());
                }
                out.end = end;
                continue;
            }
        }
        break;
    }
    out
}

/// Name of a Samsung SEF data block starting at `pos`, if one does.
fn sef_block_name(cur: &mut Cursor<'_>, pos: u64) -> Option<String> {
    let head = cur.read_exact(pos, 8)?;
    if head[0] != 0 || head[1] != 0 {
        return None;
    }
    let name_len = u32le(&head, 4)? as usize;
    if !(4..=64).contains(&name_len) {
        return None;
    }
    let name = cur.read_exact(pos + 8, name_len)?;
    if !name.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_') {
        return None;
    }
    let name = String::from_utf8(name).ok()?;
    SEF_NAMES
        .iter()
        .any(|k| name.starts_with(k))
        .then_some(name)
}

/// Search forward from `from` for a valid `SEFH` directory whose blocks lie
/// after `floor`; returns the end of `SEFT` and the MP4 block, if any.
fn find_sef_end(cur: &mut Cursor<'_>, from: u64, floor: u64) -> Option<(u64, Option<Range>)> {
    let limit = cur.limit();
    let mut p = from;
    for _ in 0..256 {
        let h = cur.find_bytes(p, limit, b"SEFH")?;
        if let Some(r) = parse_sefh(cur, h, floor) {
            return Some(r);
        }
        p = h + 1;
    }
    None
}

/// Validate a SEF directory at `h`; returns `(end of "SEFT", MP4 range)`.
fn parse_sefh(cur: &mut Cursor<'_>, h: u64, floor: u64) -> Option<(u64, Option<Range>)> {
    let head = cur.read_exact(h, 12)?;
    let count = u32le(&head, 8)? as u64;
    if count == 0 || count > 64 {
        return None;
    }
    let dir_len = 12 + 12 * count;
    let entries = cur.read_exact(h + 12, (12 * count) as usize)?;
    // The directory may carry padding; accept SEFT within 64 bytes.
    let mut seft = None;
    for extra in 0..=64u64 {
        let at = h + dir_len + extra;
        let tail = cur.read_exact(at, 8)?;
        if &tail[4..8] == b"SEFT" && u32le(&tail, 0)? as u64 == dir_len + extra {
            seft = Some(at + 8);
            break;
        }
    }
    let end = seft?;
    let mut mp4 = None;
    for i in 0..count as usize {
        let e = &entries[i * 12..i * 12 + 12];
        let typ = u16le(e, 2)?;
        let noff = u32le(e, 4)? as u64;
        let size = u32le(e, 8)? as u64;
        if noff > h || size > noff {
            return None;
        }
        let start = h - noff;
        if start < floor {
            return None;
        }
        if typ == 0x0A30 {
            // MotionPhoto_Data: header (8 + name) then the MP4.
            let name_len = cur.read_exact(start + 4, 4).and_then(|b| u32le(&b, 0))? as u64;
            let data = start + 8 + name_len;
            if data < start + size && cur.read_exact(data + 4, 4).as_deref() == Some(b"ftyp") {
                mp4 = Some(Range::new(data, start + size));
            }
        }
    }
    Some((end, mp4))
}

/// Walk top-level ISO BMFF boxes from `start`; returns the end and whether
/// a box ran past the readable data.
pub(crate) fn walk_mp4(cur: &mut Cursor<'_>, start: u64) -> Option<(u64, bool)> {
    let limit = cur.limit();
    let mut pos = start;
    let mut boxes = 0usize;
    let mut seen_media = false;
    while boxes < 100_000 {
        let Some(head) = cur.read_exact(pos, 16).or_else(|| cur.read_exact(pos, 8)) else {
            break;
        };
        let typ: &[u8] = &head[4..8];
        if boxes == 0 && typ != b"ftyp" {
            return None;
        }
        if !MP4_BOXES.iter().any(|b| b.as_slice() == typ) {
            break;
        }
        let size32 = u32be(&head, 0)? as u64;
        let size = match size32 {
            0 => break,
            1 => {
                let s = u64be(&head, 8)?;
                if s < 16 {
                    break;
                }
                s
            }
            s if s < 8 => break,
            s => s,
        };
        if typ == b"moov" || typ == b"mdat" || typ == b"moof" {
            seen_media = true;
        }
        let Some(end) = pos.checked_add(size) else {
            break;
        };
        if end > limit {
            return seen_media.then_some((limit, true)).or(Some((limit, true)));
        }
        pos = end;
        boxes += 1;
    }
    (boxes > 0 && seen_media).then_some((pos, false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cursor::SliceSource;

    pub(crate) fn fake_mp4(payload: usize) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&24u32.to_be_bytes());
        v.extend_from_slice(b"ftypisom\0\0\x02\0isommp42");
        v.extend_from_slice(&((payload + 8) as u32).to_be_bytes());
        v.extend_from_slice(b"mdat");
        v.extend((0..payload).map(|i| (i * 7) as u8));
        v.extend_from_slice(&16u32.to_be_bytes());
        v.extend_from_slice(b"moov");
        v.extend_from_slice(&[0u8; 8]);
        v
    }

    #[test]
    fn walks_mp4_boxes() {
        let mut d = fake_mp4(1000);
        let n = d.len() as u64;
        d.extend_from_slice(&[0xA5; 100]);
        let src = SliceSource(&d);
        let mut c = Cursor::new(&src, 0, d.len() as u64, 4096);
        assert_eq!(walk_mp4(&mut c, 0), Some((n, false)));
        let mut c = Cursor::new(&src, 0, 500, 4096);
        assert_eq!(walk_mp4(&mut c, 0), Some((500, true)));
        let mut c = Cursor::new(&src, 0, d.len() as u64, 4096);
        assert_eq!(walk_mp4(&mut c, 10), None);
    }

    #[test]
    fn parses_sef_trailer() {
        let start = 100u64;
        let mut d = vec![0u8; start as usize];
        let mp4 = fake_mp4(300);
        let name = b"MotionPhoto_Data";
        let mut block = vec![0, 0, 0x30, 0x0A];
        block.extend_from_slice(&(name.len() as u32).to_le_bytes());
        block.extend_from_slice(name);
        block.extend_from_slice(&mp4);
        let block_start = d.len() as u64;
        d.extend_from_slice(&block);
        let h = d.len() as u64;
        let mut dir = b"SEFH".to_vec();
        dir.extend_from_slice(&101u32.to_le_bytes());
        dir.extend_from_slice(&1u32.to_le_bytes());
        dir.extend_from_slice(&[0, 0, 0x30, 0x0A]);
        dir.extend_from_slice(&((h - block_start) as u32).to_le_bytes());
        dir.extend_from_slice(&(block.len() as u32).to_le_bytes());
        let dir_len = dir.len() as u32;
        d.extend_from_slice(&dir);
        d.extend_from_slice(&dir_len.to_le_bytes());
        d.extend_from_slice(b"SEFT");
        let total = d.len() as u64;
        d.extend_from_slice(&[0x5A; 64]);
        let src = SliceSource(&d);
        let mut c = Cursor::new(&src, 0, d.len() as u64, 4096);
        assert_eq!(
            sef_block_name(&mut c, start).as_deref(),
            Some("MotionPhoto_Data")
        );
        let (end, mp4r) = find_sef_end(&mut c, start, start).unwrap();
        assert_eq!(end, total);
        let r = mp4r.unwrap();
        assert_eq!(r.len(), mp4.len() as u64);
        // A floor after the block start rejects the directory.
        assert!(find_sef_end(&mut c, start, start + 1).is_none());
    }
}
