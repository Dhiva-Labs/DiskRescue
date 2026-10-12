//! HEVC / H.265 header parsing (ITU-T H.265 §7.3): NAL header, VPS, SPS
//! (through the short-term reference picture sets and long-term reference
//! syntax; VUI is not parsed), PPS prefix, the slice segment header up to
//! `slice_pic_order_cnt_lsb`, and picture order count derivation (§8.3.1).
//!
//! NAL units are passed *with* their two-byte header. Every parser returns
//! `None` on malformed or out-of-range input; nothing panics.

use std::collections::BTreeMap;

use diskrescue_core::util::bits::{unescape_nal, BitReader};

/// NAL unit type from the first header byte (6 bits at bit 1).
pub fn nal_type(b0: u8) -> u8 {
    (b0 >> 1) & 0x3F
}

/// IRAP picture types (BLA, IDR, CRA, reserved IRAP 22..=23).
pub fn is_irap(t: u8) -> bool {
    (16..=23).contains(&t)
}

/// IDR types (19 IDR_W_RADL, 20 IDR_N_LP).
pub fn is_idr(t: u8) -> bool {
    t == 19 || t == 20
}

/// Keyframe types usable for `stss`: BLA (16..=18), IDR (19, 20), CRA (21).
pub fn is_keyframe_type(t: u8) -> bool {
    (16..=21).contains(&t)
}

/// VCL types that carry a slice segment header (0..=9 and 16..=21).
pub fn is_slice_type(t: u8) -> bool {
    t <= 9 || (16..=21).contains(&t)
}

/// RASL pictures (8, 9).
pub fn is_rasl(t: u8) -> bool {
    t == 8 || t == 9
}

/// RADL pictures (6, 7).
pub fn is_radl(t: u8) -> bool {
    t == 6 || t == 7
}

/// Sub-layer non-reference pictures (even types below 16).
pub fn is_sub_layer_non_ref(t: u8) -> bool {
    t < 16 && t % 2 == 0
}

/// Parsed two-byte NAL header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NalHeader {
    /// nal_unit_type.
    pub nal_type: u8,
    /// nuh_layer_id.
    pub layer_id: u8,
    /// nuh_temporal_id_plus1 (never 0 in a valid stream).
    pub temporal_id_plus1: u8,
}

/// Parse a NAL header: forbidden bit must be 0 and `nuh_temporal_id_plus1`
/// non-zero (H.265 §7.4.2.2 applies to every NAL type, EOB included).
pub fn parse_nal_header(b: &[u8]) -> Option<NalHeader> {
    let (b0, b1) = (*b.first()?, *b.get(1)?);
    if b0 & 0x80 != 0 {
        return None;
    }
    let h = NalHeader {
        nal_type: nal_type(b0),
        layer_id: ((b0 & 1) << 5) | (b1 >> 3),
        temporal_id_plus1: b1 & 7,
    };
    if h.temporal_id_plus1 == 0 {
        return None;
    }
    // VPS/SPS/EOS/EOB must have TemporalId 0.
    if matches!(h.nal_type, 32 | 33 | 36 | 37) && h.temporal_id_plus1 != 1 {
        return None;
    }
    // IRAP pictures have TemporalId 0.
    if is_irap(h.nal_type) && h.temporal_id_plus1 != 1 {
        return None;
    }
    Some(h)
}

/// Video parameter set (prefix).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Vps {
    /// vps_video_parameter_set_id.
    pub vps_id: u32,
    /// vps_max_sub_layers (minus1 + 1).
    pub max_sub_layers: u32,
}

/// Sequence parameter set (fields needed for slice parsing and sizing).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Sps {
    /// sps_video_parameter_set_id.
    pub vps_id: u32,
    /// sps_max_sub_layers (minus1 + 1).
    pub max_sub_layers: u32,
    /// general_profile_idc.
    pub profile_idc: u8,
    /// general_tier_flag.
    pub tier: bool,
    /// general_level_idc.
    pub level_idc: u8,
    /// sps_seq_parameter_set_id (0..=15).
    pub sps_id: u32,
    /// chroma_format_idc.
    pub chroma_format_idc: u32,
    /// separate_colour_plane_flag.
    pub separate_colour_plane: bool,
    /// pic_width_in_luma_samples.
    pub pic_width: u32,
    /// pic_height_in_luma_samples.
    pub pic_height: u32,
    /// Conformance window (left, right, top, bottom) in chroma units.
    pub conf_win: Option<(u32, u32, u32, u32)>,
    /// Display width after the conformance window.
    pub width: u32,
    /// Display height after the conformance window.
    pub height: u32,
    /// Luma bit depth.
    pub bit_depth_luma: u32,
    /// Chroma bit depth.
    pub bit_depth_chroma: u32,
    /// log2_max_pic_order_cnt_lsb (minus4 + 4).
    pub log2_max_poc_lsb: u32,
    /// sps_max_num_reorder_pics of the highest sub-layer.
    pub max_num_reorder: u32,
    /// MinCbLog2SizeY.
    pub log2_min_cb: u32,
    /// CtbLog2SizeY.
    pub log2_ctb: u32,
    /// PicSizeInCtbsY.
    pub pic_size_in_ctbs: u32,
    /// num_short_term_ref_pic_sets, when the parse reached it.
    pub num_short_term_ref_pic_sets: Option<u32>,
    /// long_term_ref_pics_present_flag, when the parse reached it.
    pub long_term_ref_pics_present: Option<bool>,
    /// vui_parameters_present_flag, when the parse reached it.
    pub vui_present: Option<bool>,
    /// The short-term reference picture sets of the SPS (empty when the
    /// parse did not reach them).
    pub st_rps: Vec<StRps>,
    /// num_long_term_ref_pics_sps.
    pub num_long_term_ref_pics_sps: u32,
}

/// Picture parameter set (prefix).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Pps {
    /// pps_pic_parameter_set_id (0..=63).
    pub pps_id: u32,
    /// pps_seq_parameter_set_id.
    pub sps_id: u32,
    /// dependent_slice_segments_enabled_flag.
    pub dependent_slice_segments_enabled: bool,
    /// output_flag_present_flag.
    pub output_flag_present: bool,
    /// num_extra_slice_header_bits.
    pub num_extra_slice_header_bits: u32,
    /// sign_data_hiding_enabled_flag.
    pub sign_data_hiding: bool,
    /// cabac_init_present_flag.
    pub cabac_init_present: bool,
}

/// VPS map keyed by id.
pub type VpsMap = BTreeMap<u32, Vps>;
/// SPS map keyed by id.
pub type SpsMap = BTreeMap<u32, Sps>;
/// PPS map keyed by id.
pub type PpsMap = BTreeMap<u32, Pps>;

/// Slice segment header fields up to the POC LSB.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SliceSegmentHeader {
    /// first_slice_segment_in_pic_flag.
    pub first_slice_segment_in_pic: bool,
    /// no_output_of_prior_pics_flag (IRAP only).
    pub no_output_of_prior_pics: bool,
    /// slice_pic_parameter_set_id.
    pub pps_id: u32,
    /// dependent_slice_segment_flag.
    pub dependent_slice_segment: bool,
    /// slice_segment_address.
    pub segment_address: u32,
    /// slice_type (0 B, 1 P, 2 I); `None` for dependent segments.
    pub slice_type: Option<u32>,
    /// pic_output_flag (1 when absent).
    pub pic_output_flag: bool,
    /// colour_plane_id.
    pub colour_plane_id: u32,
    /// slice_pic_order_cnt_lsb (0 for IDR); `None` for dependent segments.
    pub poc_lsb: Option<u32>,
    /// nal_unit_type of the NAL.
    pub nal_type: u8,
    /// nuh_layer_id.
    pub layer_id: u8,
    /// TemporalId.
    pub temporal_id: u8,
    /// POC deltas of every picture in the slice's reference picture set
    /// (short-term sets; `Some(empty)` for IDR pictures). `None` when the
    /// set could not be derived (long-term references, dependent slice
    /// segments, or a parameter set the parse did not reach).
    pub ref_deltas: Option<Vec<i32>>,
}

fn rbsp(nal: &[u8], max: usize) -> Vec<u8> {
    unescape_nal(&nal[..nal.len().min(max)])
}

fn skip_ptl(r: &mut BitReader<'_>, max_sub_minus1: u32) -> Option<(u8, bool, u8)> {
    r.read_bits(2)?; // profile_space
    let tier = r.read_flag()?;
    let profile = r.read_bits(5)? as u8;
    r.skip(32 + 4 + 43 + 1)?;
    let level = r.read_bits(8)? as u8;
    let mut prof_present = [false; 8];
    let mut lvl_present = [false; 8];
    for i in 0..max_sub_minus1 as usize {
        prof_present[i] = r.read_flag()?;
        lvl_present[i] = r.read_flag()?;
    }
    if max_sub_minus1 > 0 {
        for _ in max_sub_minus1..8 {
            r.skip(2)?;
        }
    }
    for i in 0..max_sub_minus1 as usize {
        if prof_present[i] {
            r.skip(88)?;
        }
        if lvl_present[i] {
            r.skip(8)?;
        }
    }
    Some((profile, tier, level))
}

/// Parse a VPS NAL (header included).
pub fn parse_vps(nal: &[u8]) -> Option<Vps> {
    let h = parse_nal_header(nal)?;
    if h.nal_type != 32 || nal.len() < 4 {
        return None;
    }
    let data = rbsp(&nal[2..], 64);
    let mut r = BitReader::new(&data);
    let vps_id = r.read_bits(4)?;
    r.skip(2)?; // base layer flags
    r.read_bits(6)?; // max_layers_minus1
    let sub = r.read_bits(3)?;
    if sub > 6 {
        return None;
    }
    Some(Vps {
        vps_id,
        max_sub_layers: sub + 1,
    })
}

fn skip_scaling_list_data(r: &mut BitReader<'_>) -> Option<()> {
    for size_id in 0..4u32 {
        let step = if size_id == 3 { 3 } else { 1 };
        let mut matrix = 0;
        while matrix < 6 {
            if !r.read_flag()? {
                r.read_ue()?; // pred_matrix_id_delta
            } else {
                let coef = 64.min(1usize << (4 + (size_id << 1)));
                if size_id > 1 {
                    r.read_se()?;
                }
                for _ in 0..coef {
                    r.read_se()?;
                }
            }
            matrix += step;
        }
    }
    Some(())
}

/// A short-term reference picture set (H.265 §7.4.8): POC deltas of the
/// pictures before (`neg`, decreasing) and after (`pos`, increasing) the
/// current one, each with its `used_by_curr_pic` flag.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StRps {
    /// DeltaPocS0 / UsedByCurrPicS0.
    pub neg: Vec<(i32, bool)>,
    /// DeltaPocS1 / UsedByCurrPicS1.
    pub pos: Vec<(i32, bool)>,
}

impl StRps {
    /// NumDeltaPocs.
    pub fn len(&self) -> usize {
        self.neg.len() + self.pos.len()
    }

    /// True when the set is empty.
    pub fn is_empty(&self) -> bool {
        self.neg.is_empty() && self.pos.is_empty()
    }

    /// Every POC delta of the set (used by the current picture or kept for
    /// later pictures).
    pub fn deltas(&self) -> Vec<i32> {
        self.neg
            .iter()
            .chain(self.pos.iter())
            .map(|x| x.0)
            .collect()
    }
}

/// Parse `st_ref_pic_set(idx)` (§7.3.7) and derive its delta POCs
/// (§7.4.8). `sets` holds the sets already parsed from the SPS; `num_sets`
/// is `num_short_term_ref_pic_sets` (a slice header passes `idx ==
/// num_sets`, which carries `delta_idx_minus1`).
fn parse_st_rps(
    r: &mut BitReader<'_>,
    idx: usize,
    num_sets: usize,
    sets: &[StRps],
) -> Option<StRps> {
    let inter = if idx != 0 { r.read_flag()? } else { false };
    if inter {
        let delta_idx = if idx == num_sets {
            r.read_ue()? as usize + 1
        } else {
            1
        };
        let reference = sets.get(idx.checked_sub(delta_idx)?)?;
        let sign = r.read_flag()?;
        let abs = r.read_ue()?;
        if abs >= 1 << 15 {
            return None;
        }
        let delta_rps = if sign {
            -(abs as i32 + 1)
        } else {
            abs as i32 + 1
        };
        let n = reference.len();
        let mut used = Vec::with_capacity(n + 1);
        let mut use_delta = Vec::with_capacity(n + 1);
        for _ in 0..=n {
            let u = r.read_flag()?;
            let d = if u { true } else { r.read_flag()? };
            used.push(u);
            use_delta.push(d);
        }
        let nn = reference.neg.len();
        let mut out = StRps::default();
        for j in (0..reference.pos.len()).rev() {
            let d = reference.pos[j].0 + delta_rps;
            if d < 0 && use_delta[nn + j] {
                out.neg.push((d, used[nn + j]));
            }
        }
        if delta_rps < 0 && use_delta[n] {
            out.neg.push((delta_rps, used[n]));
        }
        for j in 0..nn {
            let d = reference.neg[j].0 + delta_rps;
            if d < 0 && use_delta[j] {
                out.neg.push((d, used[j]));
            }
        }
        for j in (0..nn).rev() {
            let d = reference.neg[j].0 + delta_rps;
            if d > 0 && use_delta[j] {
                out.pos.push((d, used[j]));
            }
        }
        if delta_rps > 0 && use_delta[n] {
            out.pos.push((delta_rps, used[n]));
        }
        for j in 0..reference.pos.len() {
            let d = reference.pos[j].0 + delta_rps;
            if d > 0 && use_delta[nn + j] {
                out.pos.push((d, used[nn + j]));
            }
        }
        if out.neg.len() > 16 || out.pos.len() > 16 {
            return None;
        }
        Some(out)
    } else {
        let neg = r.read_ue()?;
        let pos = r.read_ue()?;
        if neg > 16 || pos > 16 {
            return None;
        }
        let mut out = StRps::default();
        let mut d = 0i32;
        for _ in 0..neg {
            let m = r.read_ue()?;
            if m >= 1 << 15 {
                return None;
            }
            d -= m as i32 + 1;
            out.neg.push((d, r.read_flag()?));
        }
        d = 0;
        for _ in 0..pos {
            let m = r.read_ue()?;
            if m >= 1 << 15 {
                return None;
            }
            d += m as i32 + 1;
            out.pos.push((d, r.read_flag()?));
        }
        Some(out)
    }
}

/// Parse an SPS NAL (header included).
pub fn parse_sps(nal: &[u8]) -> Option<Sps> {
    let h = parse_nal_header(nal)?;
    if h.nal_type != 33 || nal.len() < 4 {
        return None;
    }
    let data = rbsp(&nal[2..], 4096);
    let mut r = BitReader::new(&data);
    let vps_id = r.read_bits(4)?;
    let sub_minus1 = r.read_bits(3)?;
    if sub_minus1 > 6 {
        return None;
    }
    r.read_flag()?;
    let (profile_idc, tier, level_idc) = skip_ptl(&mut r, sub_minus1)?;
    let mut s = Sps {
        vps_id,
        max_sub_layers: sub_minus1 + 1,
        profile_idc,
        tier,
        level_idc,
        ..Default::default()
    };
    s.sps_id = r.read_ue()?;
    if s.sps_id > 15 {
        return None;
    }
    s.chroma_format_idc = r.read_ue()?;
    if s.chroma_format_idc > 3 {
        return None;
    }
    if s.chroma_format_idc == 3 {
        s.separate_colour_plane = r.read_flag()?;
    }
    s.pic_width = r.read_ue()?;
    s.pic_height = r.read_ue()?;
    if s.pic_width == 0 || s.pic_height == 0 || s.pic_width > 16888 || s.pic_height > 16888 {
        return None;
    }
    if r.read_flag()? {
        s.conf_win = Some((r.read_ue()?, r.read_ue()?, r.read_ue()?, r.read_ue()?));
    }
    let bl = r.read_ue()?;
    let bc = r.read_ue()?;
    if bl > 8 || bc > 8 {
        return None;
    }
    s.bit_depth_luma = bl + 8;
    s.bit_depth_chroma = bc + 8;
    let lp = r.read_ue()?;
    if lp > 12 {
        return None;
    }
    s.log2_max_poc_lsb = lp + 4;
    let ordering_all = r.read_flag()?;
    let start = if ordering_all { 0 } else { sub_minus1 };
    for _ in start..=sub_minus1 {
        r.read_ue()?;
        s.max_num_reorder = r.read_ue()?;
        r.read_ue()?;
    }
    let min_cb = r.read_ue()?;
    let diff = r.read_ue()?;
    s.log2_min_cb = min_cb + 3;
    s.log2_ctb = s.log2_min_cb + diff;
    if !(3..=6).contains(&s.log2_min_cb) || !(4..=6).contains(&s.log2_ctb) {
        return None;
    }
    let ctb = 1u32 << s.log2_ctb;
    s.pic_size_in_ctbs = s.pic_width.div_ceil(ctb) * s.pic_height.div_ceil(ctb);
    let (sub_w, sub_h) = match s.chroma_format_idc {
        1 => (2, 2),
        2 => (2, 1),
        _ => (1, 1),
    };
    let (cl, cr, ct, cb) = s.conf_win.unwrap_or((0, 0, 0, 0));
    s.width = s.pic_width.saturating_sub(sub_w * cl.saturating_add(cr));
    s.height = s.pic_height.saturating_sub(sub_h * ct.saturating_add(cb));
    // Best effort past the fields the slice parser needs.
    let rest = (|| -> Option<()> {
        r.read_ue()?; // log2_min_luma_transform_block_size_minus2
        r.read_ue()?; // log2_diff_max_min_luma_transform_block_size
        r.read_ue()?; // max_transform_hierarchy_depth_inter
        r.read_ue()?; // max_transform_hierarchy_depth_intra
        if r.read_flag()? && r.read_flag()? {
            skip_scaling_list_data(&mut r)?;
        }
        r.read_flag()?; // amp
        r.read_flag()?; // sao
        if r.read_flag()? {
            r.skip(8)?;
            r.read_ue()?;
            r.read_ue()?;
            r.read_flag()?;
        }
        let n = r.read_ue()?;
        if n > 64 {
            return None;
        }
        let mut sets: Vec<StRps> = Vec::with_capacity(n as usize);
        for i in 0..n as usize {
            let d = parse_st_rps(&mut r, i, n as usize, &sets)?;
            sets.push(d);
        }
        s.num_short_term_ref_pic_sets = Some(n);
        s.st_rps = sets;
        let lt = r.read_flag()?;
        if lt {
            let k = r.read_ue()?;
            if k > 32 {
                return None;
            }
            for _ in 0..k {
                r.skip(s.log2_max_poc_lsb as usize + 1)?;
            }
            s.num_long_term_ref_pics_sps = k;
        }
        s.long_term_ref_pics_present = Some(lt);
        r.read_flag()?; // temporal mvp
        r.read_flag()?; // strong intra smoothing
        s.vui_present = Some(r.read_flag()?);
        Some(())
    })();
    let _ = rest;
    Some(s)
}

/// Parse a PPS NAL (header included).
pub fn parse_pps(nal: &[u8]) -> Option<Pps> {
    let h = parse_nal_header(nal)?;
    if h.nal_type != 34 || nal.len() < 3 {
        return None;
    }
    let data = rbsp(&nal[2..], 256);
    let mut r = BitReader::new(&data);
    let p = Pps {
        pps_id: r.read_ue()?,
        sps_id: r.read_ue()?,
        dependent_slice_segments_enabled: r.read_flag()?,
        output_flag_present: r.read_flag()?,
        num_extra_slice_header_bits: r.read_bits(3)?,
        sign_data_hiding: r.read_flag()?,
        cabac_init_present: r.read_flag()?,
    };
    if p.pps_id > 63 || p.sps_id > 15 {
        return None;
    }
    Some(p)
}

/// Parse a slice segment header (NAL header included).
pub fn parse_slice_header(
    nal: &[u8],
    sps_map: &SpsMap,
    pps_map: &PpsMap,
) -> Option<SliceSegmentHeader> {
    let h = parse_nal_header(nal)?;
    if !is_slice_type(h.nal_type) || nal.len() < 3 {
        return None;
    }
    let data = rbsp(&nal[2..], 160);
    let mut r = BitReader::new(&data);
    let mut s = SliceSegmentHeader {
        first_slice_segment_in_pic: r.read_flag()?,
        nal_type: h.nal_type,
        layer_id: h.layer_id,
        temporal_id: h.temporal_id_plus1 - 1,
        pic_output_flag: true,
        ..Default::default()
    };
    if is_irap(h.nal_type) {
        s.no_output_of_prior_pics = r.read_flag()?;
    }
    s.pps_id = r.read_ue()?;
    let pps = pps_map.get(&s.pps_id)?;
    let sps = sps_map.get(&pps.sps_id)?;
    if !s.first_slice_segment_in_pic {
        if pps.dependent_slice_segments_enabled {
            s.dependent_slice_segment = r.read_flag()?;
        }
        let n = sps.pic_size_in_ctbs;
        let bits = if n <= 1 {
            0
        } else {
            32 - (n - 1).leading_zeros()
        };
        s.segment_address = r.read_bits(bits)?;
        if s.segment_address == 0 || s.segment_address >= n {
            return None;
        }
    }
    if !s.dependent_slice_segment {
        r.skip(pps.num_extra_slice_header_bits as usize)?;
        let st = r.read_ue()?;
        if st > 2 {
            return None;
        }
        if is_irap(h.nal_type) && h.layer_id == 0 && st != 2 {
            return None;
        }
        s.slice_type = Some(st);
        if pps.output_flag_present {
            s.pic_output_flag = r.read_flag()?;
        }
        if sps.separate_colour_plane {
            s.colour_plane_id = r.read_bits(2)?;
        }
        if is_idr(h.nal_type) {
            s.poc_lsb = Some(0);
            s.ref_deltas = Some(Vec::new());
        } else {
            s.poc_lsb = Some(r.read_bits(sps.log2_max_poc_lsb)?);
            s.ref_deltas = slice_rps(&mut r, sps);
        }
    }
    Some(s)
}

/// The short-term reference picture set of a non-IDR slice (the syntax
/// right after `slice_pic_order_cnt_lsb`). `None` when it cannot be
/// derived or long-term references are in use.
fn slice_rps(r: &mut BitReader<'_>, sps: &Sps) -> Option<Vec<i32>> {
    let num = sps.num_short_term_ref_pic_sets? as usize;
    if num != sps.st_rps.len() {
        return None;
    }
    let rps = if !r.read_flag()? {
        parse_st_rps(r, num, num, &sps.st_rps)?
    } else {
        let idx = if num > 1 {
            let bits = 32 - ((num - 1) as u32).leading_zeros();
            r.read_bits(bits)? as usize
        } else {
            0
        };
        sps.st_rps.get(idx)?.clone()
    };
    if sps.long_term_ref_pics_present? {
        let lt_sps = if sps.num_long_term_ref_pics_sps > 0 {
            r.read_ue()?
        } else {
            0
        };
        let lt_pics = r.read_ue()?;
        if lt_sps + lt_pics > 0 {
            return None;
        }
    }
    Some(rps.deltas())
}

/// Does `cur` start a new picture after `prev`?
pub fn is_new_picture(prev: &SliceSegmentHeader, cur: &SliceSegmentHeader) -> bool {
    cur.first_slice_segment_in_pic
        || cur.layer_id != prev.layer_id
        || cur.pps_id != prev.pps_id && !cur.dependent_slice_segment
        || cur.poc_lsb.is_some() && prev.poc_lsb.is_some() && cur.poc_lsb != prev.poc_lsb
}

/// State for POC derivation (§8.3.1).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PocState {
    prev_tid0_poc: i32,
    seen_first: bool,
    /// Set after an end-of-sequence NAL: the next IRAP resets the POC.
    pub after_eos: bool,
}

impl PocState {
    /// Fresh state (start of a bitstream).
    pub fn new() -> Self {
        Self::default()
    }
}

/// True when this IRAP picture has NoRaslOutputFlag = 1 given the state
/// (IDR, BLA, or the first CRA / a CRA after end of sequence).
pub fn no_rasl_output(nal_type: u8, state: &PocState) -> bool {
    is_idr(nal_type)
        || (16..=18).contains(&nal_type)
        || (nal_type == 21 && (!state.seen_first || state.after_eos))
}

/// Picture order count of the picture whose first slice is `hdr`; updates
/// `state`.
pub fn poc(sps: &Sps, hdr: &SliceSegmentHeader, state: &mut PocState) -> i32 {
    let max_lsb = 1i32 << sps.log2_max_poc_lsb;
    let lsb = hdr.poc_lsb.unwrap_or(0) as i32;
    let msb = if is_irap(hdr.nal_type) && no_rasl_output(hdr.nal_type, state) {
        0
    } else {
        let prev_lsb = state.prev_tid0_poc & (max_lsb - 1);
        let prev_msb = state.prev_tid0_poc - prev_lsb;
        if lsb < prev_lsb && prev_lsb - lsb >= max_lsb / 2 {
            prev_msb + max_lsb
        } else if lsb > prev_lsb && lsb - prev_lsb > max_lsb / 2 {
            prev_msb - max_lsb
        } else {
            prev_msb
        }
    };
    let poc = msb + lsb;
    let t = hdr.nal_type;
    if hdr.temporal_id == 0 && !is_rasl(t) && !is_radl(t) && !is_sub_layer_non_ref(t) {
        state.prev_tid0_poc = poc;
    }
    state.seen_first = true;
    if is_irap(t) {
        state.after_eos = false;
    }
    poc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nal_headers() {
        let h = parse_nal_header(&[0x40, 0x01]).unwrap();
        assert_eq!(h.nal_type, 32);
        assert!(parse_nal_header(&[0x40, 0x00]).is_none());
        assert!(parse_nal_header(&[0xC0, 0x01]).is_none());
        // EOB with temporal_id_plus1 == 1 is valid (untrunc inverts this).
        assert_eq!(parse_nal_header(&[37 << 1, 0x01]).unwrap().nal_type, 37);
        assert!(is_keyframe_type(20) && !is_keyframe_type(1));
    }

    #[test]
    fn poc_wrap_and_idr() {
        let sps = Sps {
            log2_max_poc_lsb: 4,
            ..Default::default()
        };
        let mut st = PocState::new();
        let mk = |t: u8, lsb: u32| SliceSegmentHeader {
            nal_type: t,
            poc_lsb: Some(lsb),
            first_slice_segment_in_pic: true,
            ..Default::default()
        };
        assert_eq!(poc(&sps, &mk(20, 0), &mut st), 0);
        assert_eq!(poc(&sps, &mk(1, 6), &mut st), 6);
        assert_eq!(poc(&sps, &mk(1, 12), &mut st), 12);
        assert_eq!(poc(&sps, &mk(1, 2), &mut st), 18);
        // Sub-layer non-reference pictures do not update prevTid0Pic.
        assert_eq!(poc(&sps, &mk(0, 8), &mut st), 24);
        assert_eq!(poc(&sps, &mk(1, 4), &mut st), 20);
        assert_eq!(poc(&sps, &mk(19, 0), &mut st), 0);
    }
}
