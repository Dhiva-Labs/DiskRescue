//! HEIC (HEIF with an HEVC primary item) generator.
//!
//! The local ffmpeg has no `heif` muxer, so this module encodes one intra
//! frame with libx265 into a raw Annex-B stream (`-f hevc`) and wraps the
//! NAL units in a hand-written HEIF container:
//!
//! ```text
//! ftyp(heic, mif1)
//! meta { hdlr(pict) pitm iinf{infe hvc1} iloc iprp{ ipco{hvcC ispe} ipma} }
//! mdat { length-prefixed slice NAL units }
//! ```
//!
//! A single `hvc1` item is a valid HEIF still image (no grid, no thumbnail).
//! If a future ffmpeg offers a `heif` muxer it is not used; the output of
//! this module is always the layout above, so tests stay stable.
//!
//! Needs: `ffmpeg` with libx265. Without it the call returns
//! [`FixtureError::Missing`].

use crate::error::{FixtureError, Result};
use crate::tools;

/// Options for [`heic`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeicOpts {
    /// Width in pixels (rounded down to even).
    pub width: u32,
    /// Height in pixels (rounded down to even).
    pub height: u32,
    /// Content seed (picture hue).
    pub seed: u64,
}

impl Default for HeicOpts {
    fn default() -> Self {
        HeicOpts {
            width: 320,
            height: 240,
            seed: 1,
        }
    }
}

/// Split an Annex-B byte stream into NAL units (without start codes).
pub fn split_annex_b(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::new();
    for (k, &s) in starts.iter().enumerate() {
        let mut e = starts.get(k + 1).map(|&n| n - 3).unwrap_or(data.len());
        // Trailing zero bytes belong to the next start code (4-byte form).
        while e > s && data[e - 1] == 0 {
            e -= 1;
        }
        if e > s {
            out.push(&data[s..e]);
        }
    }
    out
}

fn unescape(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len());
    let mut zeros = 0;
    for &b in rbsp {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}

fn bx(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + payload.len());
    v.extend_from_slice(&((8 + payload.len()) as u32).to_be_bytes());
    v.extend_from_slice(kind);
    v.extend_from_slice(payload);
    v
}

fn full(kind: &[u8; 4], version: u8, flags: u32, payload: &[u8]) -> Vec<u8> {
    let mut p = vec![version];
    p.extend_from_slice(&flags.to_be_bytes()[1..]);
    p.extend_from_slice(payload);
    bx(kind, &p)
}

/// `hvcC` box from the parameter sets (4:2:0, 8-bit, 4-byte NAL lengths).
fn hvcc(vps: &[u8], sps: &[u8], pps: &[u8]) -> Result<Vec<u8>> {
    let s = unescape(sps);
    // NAL header (2) + vps_id/sub_layers/nesting (1) + profile_tier_level.
    if s.len() < 2 + 1 + 12 {
        return Err(FixtureError::corrupt("HEVC SPS", "too short"));
    }
    let ptl = &s[3..15];
    let mut p = vec![1u8];
    p.extend_from_slice(ptl); // profile/tier byte, 4 compat, 6 constraint, level
    p.extend_from_slice(&0xF000u16.to_be_bytes()); // min_spatial_segmentation
    p.push(0xFC); // parallelismType
    p.push(0xFC | 1); // chroma_format_idc = 4:2:0
    p.push(0xF8); // luma depth 8
    p.push(0xF8); // chroma depth 8
    p.extend_from_slice(&0u16.to_be_bytes()); // avgFrameRate
    p.push(0x0F); // constFR 0, layers 1, nested 1, lenSize-1 = 3
    p.push(3);
    for (nal_type, nal) in [(32u8, vps), (33, sps), (34, pps)] {
        p.push(0x80 | nal_type); // array_completeness = 1
        p.extend_from_slice(&1u16.to_be_bytes());
        p.extend_from_slice(&(nal.len() as u16).to_be_bytes());
        p.extend_from_slice(nal);
    }
    Ok(bx(b"hvcC", &p))
}

/// Wrap an HEVC intra frame (Annex-B) into a HEIC file.
pub fn wrap_hevc_still(annex_b: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    let nals = split_annex_b(annex_b);
    let find = |t: u8| {
        nals.iter()
            .find(|n| !n.is_empty() && (n[0] >> 1) & 0x3F == t)
    };
    let vps = find(32).ok_or_else(|| FixtureError::corrupt("HEVC stream", "no VPS"))?;
    let sps = find(33).ok_or_else(|| FixtureError::corrupt("HEVC stream", "no SPS"))?;
    let pps = find(34).ok_or_else(|| FixtureError::corrupt("HEVC stream", "no PPS"))?;
    let mut mdat = Vec::new();
    for n in &nals {
        let t = (n[0] >> 1) & 0x3F;
        if t < 32 {
            mdat.extend_from_slice(&(n.len() as u32).to_be_bytes());
            mdat.extend_from_slice(n);
        }
    }
    if mdat.is_empty() {
        return Err(FixtureError::corrupt("HEVC stream", "no slice NAL units"));
    }

    let ftyp = bx(b"ftyp", &{
        let mut p = b"heic".to_vec();
        p.extend_from_slice(&0u32.to_be_bytes());
        p.extend_from_slice(b"mif1heic");
        p
    });
    let mut hdlr_p = vec![0u8; 4];
    hdlr_p.extend_from_slice(b"pict");
    hdlr_p.extend_from_slice(&[0u8; 12]);
    hdlr_p.push(0);
    let hdlr = full(b"hdlr", 0, 0, &hdlr_p);
    let pitm = full(b"pitm", 0, 0, &1u16.to_be_bytes());
    let mut infe_p = Vec::new();
    infe_p.extend_from_slice(&1u16.to_be_bytes());
    infe_p.extend_from_slice(&0u16.to_be_bytes());
    infe_p.extend_from_slice(b"hvc1");
    infe_p.push(0);
    let infe = full(b"infe", 2, 0, &infe_p);
    let mut iinf_p = 1u16.to_be_bytes().to_vec();
    iinf_p.extend_from_slice(&infe);
    let iinf = full(b"iinf", 0, 0, &iinf_p);

    let ispe = full(b"ispe", 0, 0, &{
        let mut p = width.to_be_bytes().to_vec();
        p.extend_from_slice(&height.to_be_bytes());
        p
    });
    let hvcc_box = hvcc(vps, sps, pps)?;
    let ipco = bx(b"ipco", &[hvcc_box, ispe].concat());
    let mut ipma_p = 1u32.to_be_bytes().to_vec();
    ipma_p.extend_from_slice(&1u16.to_be_bytes()); // item_ID
    ipma_p.push(2); // association_count
    ipma_p.push(0x80 | 1); // essential, hvcC
    ipma_p.push(2); // ispe
    let ipma = full(b"ipma", 0, 0, &ipma_p);
    let iprp = bx(b"iprp", &[ipco, ipma].concat());

    // iloc needs the absolute offset of the mdat payload, which depends on
    // the size of meta; the iloc box has a fixed size so build it twice.
    let build_iloc = |offset: u32| {
        let mut p = vec![0x44, 0x00]; // offset_size 4, length_size 4, base 0, reserved
        p.extend_from_slice(&1u16.to_be_bytes()); // item_count
        p.extend_from_slice(&1u16.to_be_bytes()); // item_ID
        p.extend_from_slice(&0u16.to_be_bytes()); // data_reference_index
        p.extend_from_slice(&1u16.to_be_bytes()); // extent_count
        p.extend_from_slice(&offset.to_be_bytes());
        p.extend_from_slice(&(mdat.len() as u32).to_be_bytes());
        full(b"iloc", 0, 0, &p)
    };
    let meta_with = |iloc: &[u8]| {
        full(
            b"meta",
            0,
            0,
            &[
                hdlr.clone(),
                pitm.clone(),
                iinf.clone(),
                iloc.to_vec(),
                iprp.clone(),
            ]
            .concat(),
        )
    };
    let meta_len = meta_with(&build_iloc(0)).len();
    let mdat_payload_off = (ftyp.len() + meta_len + 8) as u32;
    let meta = meta_with(&build_iloc(mdat_payload_off));
    debug_assert_eq!(meta.len(), meta_len);

    let mut out = Vec::with_capacity(ftyp.len() + meta.len() + 8 + mdat.len());
    out.extend_from_slice(&ftyp);
    out.extend_from_slice(&meta);
    out.extend_from_slice(&bx(b"mdat", &mdat));
    Ok(out)
}

/// Generate a HEIC still image. See the module docs.
pub fn heic(opts: &HeicOpts) -> Result<Vec<u8>> {
    tools::require("ffmpeg")?;
    if !tools::ffmpeg_has_encoder("libx265") {
        return Err(tools::Missing("ffmpeg encoder libx265".into()).into());
    }
    let w = (opts.width.clamp(16, 4096) / 2) * 2;
    let h = (opts.height.clamp(16, 4096) / 2) * 2;
    let hue = (opts.seed.wrapping_mul(47) % 360).to_string();
    let src = format!("testsrc2=size={w}x{h}:rate=1:duration=1,hue=h={hue}");
    let dir = tempfile::tempdir()?;
    let out = dir.path().join("frame.hevc");
    let out_s = out.to_string_lossy().into_owned();
    tools::run(
        "ffmpeg",
        &[
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            &src,
            "-frames:v",
            "1",
            "-c:v",
            "libx265",
            "-preset",
            "ultrafast",
            "-crf",
            "28",
            "-pix_fmt",
            "yuv420p",
            "-x265-params",
            "log-level=none:keyint=1",
            "-f",
            "hevc",
            &out_s,
        ],
    )?;
    wrap_hevc_still(&std::fs::read(&out)?, w, h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annex_b_split() {
        let s = [
            0, 0, 0, 1, 0x40, 1, 9, 9, 0, 0, 1, 0x42, 1, 7, 0, 0, 0, 1, 0x44, 1, 5,
        ];
        let n = split_annex_b(&s);
        assert_eq!(n.len(), 3);
        assert_eq!(n[0], &[0x40, 1, 9, 9]);
        assert_eq!(n[1], &[0x42, 1, 7]);
        assert_eq!(n[2], &[0x44, 1, 5]);
        assert!(split_annex_b(&[1, 2, 3]).is_empty());
    }

    #[test]
    fn unescape_removes_emulation_prevention() {
        assert_eq!(unescape(&[0, 0, 3, 1, 0, 0, 3]), vec![0, 0, 1, 0, 0]);
    }

    #[test]
    fn wrap_rejects_garbage() {
        assert!(wrap_hevc_still(&[1, 2, 3], 16, 16).is_err());
    }
}
