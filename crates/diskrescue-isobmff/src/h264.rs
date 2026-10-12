//! H.264 / AVC header parsing (ITU-T H.264 §7.3): SPS, PPS, the slice
//! header up to the picture order count fields, the
//! first-VCL-NAL-of-a-new-picture test (§7.4.1.2.4) and picture order count
//! derivation (§8.2.1, types 0, 1 and 2).
//!
//! The parsers work on NAL units *with* their one-byte header and remove
//! emulation-prevention bytes themselves. They never panic and return
//! `None` for anything malformed or out of range; the moov rebuild uses that
//! as its main plausibility test, so the range checks are deliberately
//! strict (e.g. an IDR slice must be an I or SI slice with frame_num 0).
//!
//! Limits: memory-management control operation 5 (which resets POC like an
//! IDR) is not parsed; MVC/SVC extension NALs are treated as opaque.

use std::collections::BTreeMap;

use diskrescue_core::util::bits::{unescape_nal, BitReader};

/// NAL unit type of an H.264 NAL header byte.
pub fn nal_type(byte: u8) -> u8 {
    byte & 0x1F
}

/// nal_ref_idc of an H.264 NAL header byte.
pub fn nal_ref_idc(byte: u8) -> u8 {
    (byte >> 5) & 3
}

/// True for VCL NAL types that carry a slice header (1, 2 and 5).
pub fn has_slice_header(t: u8) -> bool {
    matches!(t, 1 | 2 | 5)
}

/// Sequence parameter set (fields needed for slice parsing and sizing).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Sps {
    /// profile_idc.
    pub profile_idc: u8,
    /// constraint_set flags byte.
    pub constraint_flags: u8,
    /// level_idc.
    pub level: u8,
    /// seq_parameter_set_id (0..=31).
    pub sps_id: u32,
    /// chroma_format_idc (1 = 4:2:0 when absent).
    pub chroma_format_idc: u32,
    /// separate_colour_plane_flag.
    pub separate_colour_plane: bool,
    /// Luma bit depth.
    pub bit_depth_luma: u32,
    /// Chroma bit depth.
    pub bit_depth_chroma: u32,
    /// log2_max_frame_num (minus4 + 4).
    pub log2_max_frame_num: u32,
    /// pic_order_cnt_type (0, 1, 2).
    pub pic_order_cnt_type: u32,
    /// log2_max_pic_order_cnt_lsb (minus4 + 4), type 0 only.
    pub log2_max_poc_lsb: u32,
    /// delta_pic_order_always_zero_flag (type 1).
    pub delta_pic_order_always_zero: bool,
    /// offset_for_non_ref_pic (type 1).
    pub offset_for_non_ref_pic: i32,
    /// offset_for_top_to_bottom_field (type 1).
    pub offset_for_top_to_bottom: i32,
    /// offset_for_ref_frame[] (type 1).
    pub offset_for_ref_frame: Vec<i32>,
    /// max_num_ref_frames.
    pub max_num_ref_frames: u32,
    /// gaps_in_frame_num_value_allowed_flag.
    pub gaps_in_frame_num_allowed: bool,
    /// Width in macroblocks.
    pub width_mbs: u32,
    /// Height in map units.
    pub height_map_units: u32,
    /// frame_mbs_only_flag.
    pub frame_mbs_only: bool,
    /// mb_adaptive_frame_field_flag.
    pub mb_adaptive_frame_field: bool,
    /// direct_8x8_inference_flag.
    pub direct_8x8_inference: bool,
    /// Frame cropping offsets (left, right, top, bottom) in crop units.
    pub crop: Option<(u32, u32, u32, u32)>,
    /// Display width after cropping.
    pub width: u32,
    /// Display height after cropping.
    pub height: u32,
    /// vui_parameters_present_flag (VUI itself is not parsed).
    pub vui_present: bool,
}

impl Sps {
    /// Macroblocks in one frame.
    pub fn frame_size_in_mbs(&self) -> u32 {
        self.width_mbs
            .saturating_mul(self.height_map_units)
            .saturating_mul(if self.frame_mbs_only { 1 } else { 2 })
    }
    /// MaxFrameNum.
    pub fn max_frame_num(&self) -> u32 {
        1u32 << self.log2_max_frame_num
    }
}

/// Picture parameter set.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Pps {
    /// pic_parameter_set_id (0..=255).
    pub pps_id: u32,
    /// seq_parameter_set_id it refers to.
    pub sps_id: u32,
    /// entropy_coding_mode_flag (CABAC).
    pub entropy_coding_mode: bool,
    /// bottom_field_pic_order_in_frame_present_flag.
    pub bottom_field_pic_order_in_frame_present: bool,
    /// num_slice_groups (minus1 + 1).
    pub num_slice_groups: u32,
    /// slice_group_map_type (when more than one group).
    pub slice_group_map_type: u32,
    /// num_ref_idx_l0_default_active.
    pub num_ref_idx_l0_default: u32,
    /// num_ref_idx_l1_default_active.
    pub num_ref_idx_l1_default: u32,
    /// weighted_pred_flag.
    pub weighted_pred: bool,
    /// weighted_bipred_idc.
    pub weighted_bipred_idc: u32,
    /// pic_init_qp (26 + minus26).
    pub pic_init_qp: i32,
    /// pic_init_qs.
    pub pic_init_qs: i32,
    /// chroma_qp_index_offset.
    pub chroma_qp_index_offset: i32,
    /// deblocking_filter_control_present_flag.
    pub deblocking_filter_control_present: bool,
    /// constrained_intra_pred_flag.
    pub constrained_intra_pred: bool,
    /// redundant_pic_cnt_present_flag.
    pub redundant_pic_cnt_present: bool,
}

/// SPS map keyed by id.
pub type SpsMap = BTreeMap<u32, Sps>;
/// PPS map keyed by id.
pub type PpsMap = BTreeMap<u32, Pps>;

/// Slice header fields up to the POC syntax.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SliceHeader {
    /// first_mb_in_slice.
    pub first_mb_in_slice: u32,
    /// slice_type (0..=9).
    pub slice_type: u32,
    /// pic_parameter_set_id.
    pub pps_id: u32,
    /// SPS id of that PPS.
    pub sps_id: u32,
    /// colour_plane_id (4:4:4 separate planes).
    pub colour_plane_id: u32,
    /// frame_num.
    pub frame_num: u32,
    /// field_pic_flag.
    pub field_pic: bool,
    /// bottom_field_flag.
    pub bottom_field: bool,
    /// idr_pic_id (IDR slices).
    pub idr_pic_id: Option<u32>,
    /// pic_order_cnt_lsb (POC type 0).
    pub poc_lsb: Option<u32>,
    /// delta_pic_order_cnt_bottom (POC type 0).
    pub delta_poc_bottom: i32,
    /// delta_pic_order_cnt[0..2] (POC type 1).
    pub delta_poc: [i32; 2],
    /// redundant_pic_cnt.
    pub redundant_pic_cnt: u32,
    /// nal_ref_idc of the NAL.
    pub nal_ref_idc: u8,
    /// nal_unit_type of the NAL.
    pub nal_unit_type: u8,
}

impl SliceHeader {
    /// True for IDR slices.
    pub fn is_idr(&self) -> bool {
        self.nal_unit_type == 5
    }
    /// Slice type modulo 5 (0 P, 1 B, 2 I, 3 SP, 4 SI).
    pub fn kind(&self) -> u32 {
        self.slice_type % 5
    }
}

fn rbsp(nal: &[u8], max: usize) -> Vec<u8> {
    unescape_nal(&nal[..nal.len().min(max)])
}

fn skip_scaling_list(r: &mut BitReader<'_>, size: usize) -> Option<()> {
    let mut last = 8i32;
    let mut next = 8i32;
    for _ in 0..size {
        if next != 0 {
            let delta = r.read_se()?;
            if !(-128..=127).contains(&delta) {
                return None;
            }
            next = (last + delta + 256) % 256;
        }
        last = if next == 0 { last } else { next };
    }
    Some(())
}

/// Parse a sequence parameter set NAL (header byte included).
pub fn parse_sps(nal: &[u8]) -> Option<Sps> {
    if nal.len() < 4 || nal[0] & 0x80 != 0 || nal_type(nal[0]) != 7 {
        return None;
    }
    let data = rbsp(&nal[1..], 4096);
    let mut r = BitReader::new(&data);
    let mut s = Sps {
        profile_idc: r.read_bits(8)? as u8,
        constraint_flags: r.read_bits(8)? as u8,
        level: r.read_bits(8)? as u8,
        sps_id: r.read_ue()?,
        chroma_format_idc: 1,
        bit_depth_luma: 8,
        bit_depth_chroma: 8,
        ..Default::default()
    };
    if s.sps_id > 31 {
        return None;
    }
    if matches!(
        s.profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        s.chroma_format_idc = r.read_ue()?;
        if s.chroma_format_idc > 3 {
            return None;
        }
        if s.chroma_format_idc == 3 {
            s.separate_colour_plane = r.read_flag()?;
        }
        let bl = r.read_ue()?;
        let bc = r.read_ue()?;
        if bl > 6 || bc > 6 {
            return None;
        }
        s.bit_depth_luma = bl + 8;
        s.bit_depth_chroma = bc + 8;
        r.read_flag()?; // qpprime_y_zero_transform_bypass_flag
        if r.read_flag()? {
            let n = if s.chroma_format_idc != 3 { 8 } else { 12 };
            for i in 0..n {
                if r.read_flag()? {
                    skip_scaling_list(&mut r, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    let l2fn = r.read_ue()?;
    if l2fn > 12 {
        return None;
    }
    s.log2_max_frame_num = l2fn + 4;
    s.pic_order_cnt_type = r.read_ue()?;
    match s.pic_order_cnt_type {
        0 => {
            let l = r.read_ue()?;
            if l > 12 {
                return None;
            }
            s.log2_max_poc_lsb = l + 4;
        }
        1 => {
            s.delta_pic_order_always_zero = r.read_flag()?;
            s.offset_for_non_ref_pic = r.read_se()?;
            s.offset_for_top_to_bottom = r.read_se()?;
            let n = r.read_ue()?;
            if n > 255 {
                return None;
            }
            for _ in 0..n {
                s.offset_for_ref_frame.push(r.read_se()?);
            }
        }
        2 => {}
        _ => return None,
    }
    s.max_num_ref_frames = r.read_ue()?;
    if s.max_num_ref_frames > 32 {
        return None;
    }
    s.gaps_in_frame_num_allowed = r.read_flag()?;
    let w = r.read_ue()?;
    let h = r.read_ue()?;
    if w >= 1024 || h >= 1024 {
        return None;
    }
    s.width_mbs = w + 1;
    s.height_map_units = h + 1;
    s.frame_mbs_only = r.read_flag()?;
    if !s.frame_mbs_only {
        s.mb_adaptive_frame_field = r.read_flag()?;
    }
    s.direct_8x8_inference = r.read_flag()?;
    if r.read_flag()? {
        s.crop = Some((r.read_ue()?, r.read_ue()?, r.read_ue()?, r.read_ue()?));
    }
    s.vui_present = r.read_flag().unwrap_or(false);
    let frame_h_mbs = s.height_map_units * if s.frame_mbs_only { 1 } else { 2 };
    let (cux, cuy) = if s.chroma_format_idc == 0 || s.separate_colour_plane {
        (1, if s.frame_mbs_only { 1 } else { 2 })
    } else {
        let sub_w = if s.chroma_format_idc == 3 { 1 } else { 2 };
        let sub_h = if s.chroma_format_idc == 1 { 2 } else { 1 };
        (sub_w, sub_h * if s.frame_mbs_only { 1 } else { 2 })
    };
    let (cl, cr, ct, cb) = s.crop.unwrap_or((0, 0, 0, 0));
    let full_w = s.width_mbs * 16;
    let full_h = frame_h_mbs * 16;
    s.width = full_w.saturating_sub(cux * (cl.saturating_add(cr)));
    s.height = full_h.saturating_sub(cuy * (ct.saturating_add(cb)));
    if s.width == 0 || s.height == 0 {
        return None;
    }
    Some(s)
}

/// Parse a picture parameter set NAL (header byte included).
pub fn parse_pps(nal: &[u8]) -> Option<Pps> {
    if nal.len() < 2 || nal[0] & 0x80 != 0 || nal_type(nal[0]) != 8 {
        return None;
    }
    let data = rbsp(&nal[1..], 4096);
    let mut r = BitReader::new(&data);
    let mut p = Pps {
        pps_id: r.read_ue()?,
        sps_id: r.read_ue()?,
        ..Default::default()
    };
    if p.pps_id > 255 || p.sps_id > 31 {
        return None;
    }
    p.entropy_coding_mode = r.read_flag()?;
    p.bottom_field_pic_order_in_frame_present = r.read_flag()?;
    let groups = r.read_ue()?;
    if groups > 7 {
        return None;
    }
    p.num_slice_groups = groups + 1;
    if groups > 0 {
        p.slice_group_map_type = r.read_ue()?;
        match p.slice_group_map_type {
            0 => {
                for _ in 0..=groups {
                    r.read_ue()?;
                }
            }
            2 => {
                for _ in 0..groups {
                    r.read_ue()?;
                    r.read_ue()?;
                }
            }
            3..=5 => {
                r.read_flag()?;
                r.read_ue()?;
            }
            6 => {
                let units = r.read_ue()?;
                if units > 1 << 20 {
                    return None;
                }
                let bits = 32 - groups.leading_zeros();
                r.skip((units as usize + 1) * bits as usize)?;
            }
            1 => {}
            _ => return None,
        }
    }
    let l0 = r.read_ue()?;
    let l1 = r.read_ue()?;
    if l0 > 31 || l1 > 31 {
        return None;
    }
    p.num_ref_idx_l0_default = l0 + 1;
    p.num_ref_idx_l1_default = l1 + 1;
    p.weighted_pred = r.read_flag()?;
    p.weighted_bipred_idc = r.read_bits(2)?;
    if p.weighted_bipred_idc > 2 {
        return None;
    }
    let qp = r.read_se()?;
    let qs = r.read_se()?;
    let cqp = r.read_se()?;
    if !(-26..=25).contains(&qs) || !(-12..=12).contains(&cqp) || !(-62..=25).contains(&qp) {
        return None;
    }
    p.pic_init_qp = 26 + qp;
    p.pic_init_qs = 26 + qs;
    p.chroma_qp_index_offset = cqp;
    p.deblocking_filter_control_present = r.read_flag()?;
    p.constrained_intra_pred = r.read_flag()?;
    p.redundant_pic_cnt_present = r.read_flag()?;
    Some(p)
}

/// Parse the slice header of a VCL NAL (types 1, 2, 5; header byte
/// included) using the given parameter sets.
pub fn parse_slice_header(nal: &[u8], sps_map: &SpsMap, pps_map: &PpsMap) -> Option<SliceHeader> {
    if nal.len() < 2 || nal[0] & 0x80 != 0 {
        return None;
    }
    let t = nal_type(nal[0]);
    if !has_slice_header(t) {
        return None;
    }
    let ref_idc = nal_ref_idc(nal[0]);
    if t == 5 && ref_idc == 0 {
        return None;
    }
    let data = rbsp(&nal[1..], 96);
    let mut r = BitReader::new(&data);
    let first_mb = r.read_ue()?;
    let slice_type = r.read_ue()?;
    if slice_type > 9 {
        return None;
    }
    let pps_id = r.read_ue()?;
    let pps = pps_map.get(&pps_id)?;
    let sps = sps_map.get(&pps.sps_id)?;
    if first_mb >= sps.frame_size_in_mbs() {
        return None;
    }
    if t == 5 && !matches!(slice_type % 5, 2 | 4) {
        return None;
    }
    let mut h = SliceHeader {
        first_mb_in_slice: first_mb,
        slice_type,
        pps_id,
        sps_id: sps.sps_id,
        nal_ref_idc: ref_idc,
        nal_unit_type: t,
        ..Default::default()
    };
    if sps.separate_colour_plane {
        h.colour_plane_id = r.read_bits(2)?;
        if h.colour_plane_id > 2 {
            return None;
        }
    }
    h.frame_num = r.read_bits(sps.log2_max_frame_num)?;
    if t == 5 && h.frame_num != 0 {
        return None;
    }
    if !sps.frame_mbs_only {
        h.field_pic = r.read_flag()?;
        if h.field_pic {
            h.bottom_field = r.read_flag()?;
        }
    }
    if t == 5 {
        let id = r.read_ue()?;
        if id > 65535 {
            return None;
        }
        h.idr_pic_id = Some(id);
    }
    if sps.pic_order_cnt_type == 0 {
        h.poc_lsb = Some(r.read_bits(sps.log2_max_poc_lsb)?);
        if pps.bottom_field_pic_order_in_frame_present && !h.field_pic {
            h.delta_poc_bottom = r.read_se()?;
        }
    }
    if sps.pic_order_cnt_type == 1 && !sps.delta_pic_order_always_zero {
        h.delta_poc[0] = r.read_se()?;
        if pps.bottom_field_pic_order_in_frame_present && !h.field_pic {
            h.delta_poc[1] = r.read_se()?;
        }
    }
    if pps.redundant_pic_cnt_present {
        h.redundant_pic_cnt = r.read_ue()?;
        if h.redundant_pic_cnt > 127 {
            return None;
        }
    }
    Some(h)
}

/// H.264 §7.4.1.2.4: does `cur` start a new primary coded picture after
/// `prev`? `first_mb_in_slice == 0` is treated as the primary signal.
pub fn is_new_picture(prev: &SliceHeader, cur: &SliceHeader, sps: &Sps) -> bool {
    if cur.first_mb_in_slice == 0 {
        return true;
    }
    if cur.frame_num != prev.frame_num
        || cur.pps_id != prev.pps_id
        || cur.field_pic != prev.field_pic
        || (cur.field_pic && cur.bottom_field != prev.bottom_field)
        || ((cur.nal_ref_idc == 0) != (prev.nal_ref_idc == 0))
        || cur.is_idr() != prev.is_idr()
        || (cur.is_idr() && prev.is_idr() && cur.idr_pic_id != prev.idr_pic_id)
    {
        return true;
    }
    match sps.pic_order_cnt_type {
        0 => cur.poc_lsb != prev.poc_lsb || cur.delta_poc_bottom != prev.delta_poc_bottom,
        1 => cur.delta_poc != prev.delta_poc,
        _ => false,
    }
}

/// State carried between pictures for POC derivation (§8.2.1).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PocState {
    prev_poc_msb: i32,
    prev_poc_lsb: i32,
    prev_frame_num_offset: i32,
    prev_frame_num: u32,
}

impl PocState {
    /// Fresh state (as after an IDR).
    pub fn new() -> Self {
        Self::default()
    }
}

/// Picture order count of the picture whose first slice is `hdr`
/// (§8.2.1; for frames the minimum of the top and bottom field counts).
/// Updates `state` for the next picture in decoding order.
pub fn poc(sps: &Sps, hdr: &SliceHeader, state: &mut PocState) -> i32 {
    let idr = hdr.is_idr();
    let max_frame_num = sps.max_frame_num() as i32;
    match sps.pic_order_cnt_type {
        0 => {
            if idr {
                state.prev_poc_msb = 0;
                state.prev_poc_lsb = 0;
            }
            let max_lsb = 1i32 << sps.log2_max_poc_lsb;
            let lsb = hdr.poc_lsb.unwrap_or(0) as i32;
            let (pm, pl) = (state.prev_poc_msb, state.prev_poc_lsb);
            let msb = if lsb < pl && pl - lsb >= max_lsb / 2 {
                pm + max_lsb
            } else if lsb > pl && lsb - pl > max_lsb / 2 {
                pm - max_lsb
            } else {
                pm
            };
            let top = msb + lsb;
            let poc = if hdr.field_pic {
                top
            } else {
                top.min(top + hdr.delta_poc_bottom)
            };
            if hdr.nal_ref_idc != 0 {
                state.prev_poc_msb = msb;
                state.prev_poc_lsb = lsb;
            }
            poc
        }
        1 => {
            let fno = frame_num_offset(hdr, state, max_frame_num);
            let n = sps.offset_for_ref_frame.len() as i32;
            let mut abs = if n != 0 {
                fno + hdr.frame_num as i32
            } else {
                0
            };
            if hdr.nal_ref_idc == 0 && abs > 0 {
                abs -= 1;
            }
            let mut expected = 0i32;
            if abs > 0 {
                let cycle_cnt = (abs - 1) / n;
                let in_cycle = (abs - 1) % n;
                let delta_cycle: i32 = sps
                    .offset_for_ref_frame
                    .iter()
                    .fold(0i32, |a, &b| a.wrapping_add(b));
                expected = cycle_cnt.wrapping_mul(delta_cycle);
                for i in 0..=in_cycle as usize {
                    expected = expected.wrapping_add(sps.offset_for_ref_frame[i]);
                }
            }
            if hdr.nal_ref_idc == 0 {
                expected = expected.wrapping_add(sps.offset_for_non_ref_pic);
            }
            let poc = if !hdr.field_pic {
                let top = expected.wrapping_add(hdr.delta_poc[0]);
                let bottom = top
                    .wrapping_add(sps.offset_for_top_to_bottom)
                    .wrapping_add(hdr.delta_poc[1]);
                top.min(bottom)
            } else if !hdr.bottom_field {
                expected.wrapping_add(hdr.delta_poc[0])
            } else {
                expected
                    .wrapping_add(sps.offset_for_top_to_bottom)
                    .wrapping_add(hdr.delta_poc[0])
            };
            state.prev_frame_num_offset = fno;
            state.prev_frame_num = hdr.frame_num;
            poc
        }
        _ => {
            let fno = frame_num_offset(hdr, state, max_frame_num);
            let poc = if idr {
                0
            } else if hdr.nal_ref_idc == 0 {
                2 * (fno + hdr.frame_num as i32) - 1
            } else {
                2 * (fno + hdr.frame_num as i32)
            };
            state.prev_frame_num_offset = fno;
            state.prev_frame_num = hdr.frame_num;
            poc
        }
    }
}

fn frame_num_offset(hdr: &SliceHeader, state: &PocState, max_frame_num: i32) -> i32 {
    if hdr.is_idr() {
        0
    } else if state.prev_frame_num > hdr.frame_num {
        state.prev_frame_num_offset.saturating_add(max_frame_num)
    } else {
        state.prev_frame_num_offset
    }
}

/// Validate an SEI NAL's message structure (H.264 §7.3.2.3): every
/// payloadType/payloadSize pair must fit and the messages must end exactly
/// at the RBSP trailing bits.
pub fn sei_well_formed(nal: &[u8]) -> bool {
    if nal.len() < 3 || nal[0] & 0x80 != 0 {
        return false;
    }
    sei_messages_fit(&unescape_nal(&nal[1..]))
}

/// Shared SEI structure check on an RBSP (without NAL header).
pub(crate) fn sei_messages_fit(rbsp: &[u8]) -> bool {
    let mut pos = 0usize;
    let mut messages = 0;
    loop {
        // rbsp_trailing_bits: a 0x80 byte (optionally followed by zero
        // padding) ends the SEI.
        if pos < rbsp.len() && rbsp[pos] == 0x80 && rbsp[pos + 1..].iter().all(|&b| b == 0) {
            return messages > 0;
        }
        if pos >= rbsp.len() || messages > 64 {
            return false;
        }
        let mut ptype = 0usize;
        while rbsp.get(pos) == Some(&0xFF) {
            ptype += 255;
            pos += 1;
        }
        let Some(&b) = rbsp.get(pos) else {
            return false;
        };
        ptype += b as usize;
        pos += 1;
        let mut psize = 0usize;
        while rbsp.get(pos) == Some(&0xFF) {
            psize += 255;
            pos += 1;
            if psize > 1 << 24 {
                return false;
            }
        }
        let Some(&b) = rbsp.get(pos) else {
            return false;
        };
        psize += b as usize;
        pos += 1;
        let _ = ptype;
        pos = match pos.checked_add(psize) {
            Some(p) if p <= rbsp.len() => p,
            _ => return false,
        };
        messages += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // From an ffmpeg/x264 (High, CABAC) file: avcC SPS and PPS.
    pub(crate) const SPS: [u8; 25] = [
        0x67, 0x64, 0x00, 0x0d, 0xac, 0xd9, 0x41, 0x41, 0xfb, 0x01, 0x10, 0x00, 0x00, 0x03, 0x00,
        0x10, 0x00, 0x00, 0x03, 0x03, 0xc0, 0xf1, 0x42, 0x99, 0x60,
    ];
    pub(crate) const PPS: [u8; 6] = [0x68, 0xeb, 0xe3, 0xcb, 0x22, 0xc0];

    #[test]
    fn sps_pps_from_x264() {
        let s = parse_sps(&SPS).unwrap();
        assert_eq!(s.profile_idc, 100);
        assert_eq!((s.width, s.height), (320, 240));
        assert_eq!(s.pic_order_cnt_type, 0);
        assert!(s.frame_mbs_only);
        let p = parse_pps(&PPS).unwrap();
        assert!(p.entropy_coding_mode);
        assert_eq!(p.sps_id, 0);
        assert!(parse_sps(&SPS[..3]).is_none());
        assert!(parse_pps(&[0x68]).is_none());
    }

    #[test]
    fn poc_type0_wraps() {
        let sps = Sps {
            pic_order_cnt_type: 0,
            log2_max_poc_lsb: 4,
            log2_max_frame_num: 4,
            ..Default::default()
        };
        let mut st = PocState::new();
        let mk = |lsb: u32, idr: bool| SliceHeader {
            poc_lsb: Some(lsb),
            nal_ref_idc: 1,
            nal_unit_type: if idr { 5 } else { 1 },
            ..Default::default()
        };
        assert_eq!(poc(&sps, &mk(0, true), &mut st), 0);
        assert_eq!(poc(&sps, &mk(6, false), &mut st), 6);
        assert_eq!(poc(&sps, &mk(12, false), &mut st), 12);
        // 2 after 12 wraps forward (MaxPicOrderCntLsb 16).
        assert_eq!(poc(&sps, &mk(2, false), &mut st), 18);
        // A jump of more than half the range is read as backwards.
        assert_eq!(poc(&sps, &mk(4, true), &mut st), 4);
        assert_eq!(poc(&sps, &mk(14, false), &mut st), -2);
    }

    #[test]
    fn sei_structure() {
        // payload type 5 (user data unregistered), size 2, then trailing bits.
        assert!(sei_well_formed(&[0x06, 0x05, 0x02, 0xAA, 0xBB, 0x80]));
        assert!(!sei_well_formed(&[0x06, 0x05, 0x09, 0xAA, 0xBB, 0x80]));
        assert!(!sei_well_formed(&[0x06, 0x05, 0x02, 0xAA, 0xBB]));
    }
}
