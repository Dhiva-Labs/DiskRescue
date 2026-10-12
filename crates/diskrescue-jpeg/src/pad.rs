//! Re-encoding of "empty" MCUs, so a cut or damaged scan can be completed
//! instead of ending early (image-formats spec 3.4 option B, 3.5).
//!
//! A decoder that meets the end of the data in the middle of a scan has to
//! guess: libjpeg fills the rest with grey and warns, zune-jpeg 0.4 fails
//! outright when the bits it reads past the cut do not form a valid code,
//! and some viewers refuse such files. Completing the scan with MCUs whose
//! blocks carry "DC back to zero, no AC" makes every decoder show the
//! missing part as flat grey and keeps the file standard-conforming.
//!
//! The encoder continues the original bit stream exactly where it was cut
//! (the bits of a partially used byte are re-emitted), uses the scan's own
//! Huffman tables, keeps the restart-marker sequence (RSTn after every `Ri`
//! MCUs), stuffs `FF` bytes and pads the last byte with 1-bits. It needs
//! the DC category 0 code (and the AC end-of-block code for sequential
//! scans); without them it returns `None` and the caller falls back to a
//! plain cut.
//!
//! Supported scans: sequential (baseline/extended) and progressive DC
//! first scans (`Ss = 0`, `Ah = 0`).

use crate::markers::{FrameInfo, Headers, HuffmanTable, ScanInfo};
use crate::tables::standard_table;

/// Canonical Huffman codes of one table: `codes[symbol] = (code, length)`.
struct Codes([Option<(u16, u8)>; 256]);

impl Codes {
    fn new(t: &HuffmanTable) -> Self {
        let mut codes = [None; 256];
        let mut code: u32 = 0;
        let mut k = 0usize;
        for (i, &count) in t.bits.iter().enumerate() {
            for _ in 0..count {
                if let Some(&v) = t.values.get(k) {
                    if code < (1 << (i + 1)) && codes[v as usize].is_none() {
                        codes[v as usize] = Some((code as u16, i as u8 + 1));
                    }
                }
                code += 1;
                k += 1;
            }
            code <<= 1;
        }
        Codes(codes)
    }

    fn get(&self, sym: u8) -> Option<(u16, u8)> {
        self.0[sym as usize]
    }
}

/// Bit writer with `FF 00` stuffing and 1-bit padding.
struct BitWriter {
    out: Vec<u8>,
    acc: u32,
    n: u32,
}

impl BitWriter {
    fn put(&mut self, value: u32, len: u32) {
        for i in (0..len).rev() {
            self.acc = (self.acc << 1) | ((value >> i) & 1);
            self.n += 1;
            if self.n == 8 {
                let b = self.acc as u8;
                self.out.push(b);
                if b == 0xFF {
                    self.out.push(0x00);
                }
                self.acc = 0;
                self.n = 0;
            }
        }
    }

    fn flush(&mut self) {
        if self.n > 0 {
            let pad = 8 - self.n;
            self.put((1 << pad) - 1, pad);
        }
    }
}

/// Where the padding starts.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PadStart {
    /// First MCU to emit (index within the scan).
    pub(crate) mcu: u64,
    /// Bits of a partially used byte to re-emit first (`value` holds them
    /// in its top `bits` bits).
    pub(crate) value: u8,
    /// Number of those bits (0..=7).
    pub(crate) bits: u8,
    /// DC predictors of the scan components at the cut (in units of the
    /// scan's point transform).
    pub(crate) preds: [i32; 4],
    /// Emit the restart marker that belongs before `mcu` when `mcu` is on
    /// an interval boundary (false when the original marker is kept).
    pub(crate) rst_first: bool,
}

/// Everything needed to encode empty MCUs for one scan.
pub(crate) struct ScanPadder {
    comps: Vec<(usize, usize, u32)>,
    dc: Vec<Codes>,
    ac: Vec<Option<Codes>>,
    sequential: bool,
    ri: u64,
    total: u64,
    max_cat: u32,
}

impl ScanPadder {
    /// Prepare for `scan` of `frame` with the tables of `h` (Annex K tables
    /// stand in for undefined ones, as in the analyzer). `None` for scans
    /// that cannot be padded.
    pub(crate) fn new(h: &Headers, frame: &FrameInfo, scan: &ScanInfo, ri: u64) -> Option<Self> {
        if !frame.is_huffman_dct() || frame.height == 0 || frame.check().is_err() {
            return None;
        }
        let sequential = !frame.progressive;
        if sequential {
            if scan.ss != 0 || scan.se != 63 || scan.ah != 0 || scan.al != 0 {
                return None;
            }
        } else if scan.ss != 0 || scan.se != 0 || scan.ah != 0 {
            return None;
        }
        let table = |class: u8, id: u8| -> Option<HuffmanTable> {
            h.huffman_table(class, id)
                .cloned()
                .or_else(|| standard_table(class, id))
        };
        let interleaved = scan.components.len() > 1;
        let mut comps = Vec::new();
        let mut dc = Vec::new();
        let mut ac = Vec::new();
        for sc in &scan.components {
            let fi = frame.component_index(sc.selector)?;
            let c = frame.components[fi];
            let blocks = if interleaved {
                c.h as u32 * c.v as u32
            } else {
                1
            };
            let d = Codes::new(&table(0, sc.dc_table)?);
            d.get(0)?;
            let a = if sequential {
                let a = Codes::new(&table(1, sc.ac_table)?);
                a.get(0)?;
                Some(a)
            } else {
                None
            };
            comps.push((fi, dc.len(), blocks));
            dc.push(d);
            ac.push(a);
        }
        let total = if interleaved {
            frame.mcus_x() as u64 * frame.mcus_y() as u64
        } else {
            let fc = frame.components[comps.first()?.0];
            let (hmax, vmax) = (frame.hmax() as u32, frame.vmax() as u32);
            let bx = ((frame.width as u32) * fc.h as u32)
                .div_ceil(hmax)
                .div_ceil(8);
            let by = ((frame.height as u32) * fc.v as u32)
                .div_ceil(vmax)
                .div_ceil(8);
            bx as u64 * by as u64
        };
        Some(ScanPadder {
            comps,
            dc,
            ac,
            sequential,
            ri,
            total,
            max_cat: if frame.precision > 8 { 15 } else { 11 },
        })
    }

    /// MCUs in the scan.
    pub(crate) fn total(&self) -> u64 {
        self.total
    }

    /// Encode MCUs `start.mcu..end` as empty blocks (the first block of each
    /// component brings its DC back to zero when the table allows it).
    /// Returns the entropy-coded bytes, 1-padded to a byte boundary; no EOI.
    pub(crate) fn encode(&self, start: PadStart, end: u64) -> Vec<u8> {
        let mut w = BitWriter {
            out: Vec::new(),
            acc: 0,
            n: 0,
        };
        if start.bits > 0 {
            let b = start.bits.min(7) as u32;
            w.put((start.value >> (8 - b)) as u32, b);
        }
        let mut preds = start.preds;
        let end = end.min(self.total);
        let mut m = start.mcu;
        while m < end {
            if self.ri > 0 && m > 0 && m % self.ri == 0 && (m != start.mcu || start.rst_first) {
                w.flush();
                let n = ((m / self.ri - 1) & 7) as u8;
                w.out.extend_from_slice(&[0xFF, 0xD0 + n]);
                preds = [0; 4];
            }
            for (ci, &(_, t, blocks)) in self.comps.iter().enumerate() {
                for _ in 0..blocks {
                    let pred = preds.get(ci).copied().unwrap_or(0);
                    let diff = pred.checked_neg().unwrap_or(0);
                    let cat = 32 - diff.unsigned_abs().leading_zeros();
                    let (diff, cat, code) = match self.dc[t].get(cat as u8) {
                        Some(code) if cat <= self.max_cat => (diff, cat, code),
                        // Category 0 exists (checked in `new`): keep the
                        // predictor, which continues the last colour.
                        _ => (0, 0, self.dc[t].get(0).unwrap_or((0, 1))),
                    };
                    w.put(code.0 as u32, code.1 as u32);
                    if cat > 0 {
                        let bits = if diff < 0 {
                            (diff + (1 << cat) - 1) as u32
                        } else {
                            diff as u32
                        };
                        w.put(bits & ((1 << cat) - 1), cat);
                    }
                    if let Some(p) = preds.get_mut(ci) {
                        *p += diff;
                    }
                    if self.sequential {
                        if let Some(eob) = self.ac[t].as_ref().and_then(|a| a.get(0)) {
                            w.put(eob.0 as u32, eob.1 as u32);
                        }
                    }
                }
            }
            m += 1;
        }
        w.flush();
        w.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cursor::SliceSource;
    use crate::markers::{tests::tiny_jpeg, walk_headers};
    use crate::scan::analyze;

    #[test]
    fn padding_completes_a_scan() {
        // tiny_jpeg is one 8x8 block; make it 4 MCUs wide and pad all of it.
        let mut d = tiny_jpeg();
        let sof = d.windows(2).position(|w| w == [0xFF, 0xC0]).unwrap();
        d[sof + 8] = 32; // width 32 -> 4 MCUs
        let h = walk_headers(&SliceSource(&d), 0, d.len() as u64).unwrap();
        let p = ScanPadder::new(&h, h.sof.as_ref().unwrap(), &h.scans[0], 0).unwrap();
        assert_eq!(p.total(), 4);
        let sdo = h.scan_data_offset.unwrap() as usize;
        let mut x = d[..sdo].to_vec();
        x.extend(p.encode(
            PadStart {
                mcu: 0,
                value: 0,
                bits: 0,
                preds: [0; 4],
                rst_first: false,
            },
            4,
        ));
        x.extend_from_slice(&[0xFF, 0xD9]);
        let a = analyze(&x).unwrap();
        assert!(a.is_clean(), "{a:?}");
        assert_eq!(a.mcus_decoded, 4);
    }
}
