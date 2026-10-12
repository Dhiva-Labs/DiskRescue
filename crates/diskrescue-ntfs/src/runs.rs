//! Data runs ("mapping pairs"): the compressed extent list of a
//! non-resident attribute.
//!
//! Each run starts with a header byte whose low nibble is the size of the
//! length field and whose high nibble is the size of the offset field. The
//! length is an unsigned cluster count; the offset is a signed delta from
//! the previous run's LCN. An offset size of 0 marks a sparse run (a hole
//! that reads as zeros, also used for the tail of compressed units). A
//! header byte of 0 terminates the list.
//!
//! Decoding is bounded by the input length, rejects zero-length runs,
//! negative LCNs and VCN overflow, and (when the volume size is known)
//! LCNs outside the volume. On the first invalid run decoding stops and the
//! runs decoded so far are returned together with the error, so a torn
//! record still yields its intact prefix.

use diskrescue_core::util::bytes::u8_at;

/// One run of a non-resident attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Run {
    /// First virtual cluster number (file-relative) covered by the run.
    pub vcn: u64,
    /// First logical cluster number (volume-relative), or `None` for a
    /// sparse run (hole).
    pub lcn: Option<u64>,
    /// Length in clusters (never 0).
    pub len: u64,
}

impl Run {
    /// One past the last VCN covered.
    pub fn end_vcn(&self) -> u64 {
        self.vcn.saturating_add(self.len)
    }

    /// True for sparse runs.
    pub fn is_sparse(&self) -> bool {
        self.lcn.is_none()
    }
}

/// Result of decoding a mapping-pairs array.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DecodedRuns {
    /// Runs in VCN order.
    pub runs: Vec<Run>,
    /// VCN following the last decoded run.
    pub next_vcn: u64,
    /// Why decoding stopped early, if it did.
    pub error: Option<String>,
}

/// Decode a mapping-pairs array starting at `lowest_vcn`. When
/// `total_clusters` is given every real run must lie inside the volume.
pub fn decode_runs(bytes: &[u8], lowest_vcn: u64, total_clusters: Option<u64>) -> DecodedRuns {
    let mut out = DecodedRuns {
        runs: Vec::new(),
        next_vcn: lowest_vcn,
        error: None,
    };
    let mut pos = 0usize;
    let mut lcn: i128 = 0;
    loop {
        let Some(h) = u8_at(bytes, pos) else {
            // Ran off the end without a terminator: keep what we have.
            if !out.runs.is_empty() || pos > 0 {
                out.error = Some("mapping pairs not terminated".into());
            }
            break;
        };
        if h == 0 {
            break;
        }
        let len_size = (h & 0x0F) as usize;
        let off_size = (h >> 4) as usize;
        if len_size == 0 || len_size > 8 || off_size > 8 {
            out.error = Some(format!("invalid run header {h:#04x} at byte {pos}"));
            break;
        }
        let body = pos + 1;
        if body + len_size + off_size > bytes.len() {
            out.error = Some(format!("run at byte {pos} is truncated"));
            break;
        }
        let length = le_unsigned(&bytes[body..body + len_size]);
        if length == 0 {
            out.error = Some(format!("zero-length run at byte {pos}"));
            break;
        }
        let Some(end_vcn) = out.next_vcn.checked_add(length) else {
            out.error = Some("run VCN overflow".into());
            break;
        };
        let run_lcn = if off_size == 0 {
            None
        } else {
            let delta = le_signed(&bytes[body + len_size..body + len_size + off_size]);
            lcn += delta as i128;
            if lcn < 0 || lcn > u64::MAX as i128 {
                out.error = Some(format!("run LCN {lcn} is out of range"));
                break;
            }
            let l = lcn as u64;
            if let Some(total) = total_clusters {
                if l >= total || length > total - l {
                    out.error = Some(format!(
                        "run of {length} clusters at LCN {l} lies outside the volume ({total} clusters)"
                    ));
                    break;
                }
            }
            Some(l)
        };
        out.runs.push(Run {
            vcn: out.next_vcn,
            lcn: run_lcn,
            len: length,
        });
        out.next_vcn = end_vcn;
        pos = body + len_size + off_size;
    }
    out
}

/// Encode runs back into mapping pairs (used to build test images).
#[cfg(test)]
pub(crate) fn encode_runs(runs: &[(Option<u64>, u64)]) -> Vec<u8> {
    fn min_signed_bytes(v: i64) -> usize {
        for n in 1..=8 {
            let shift = 64 - 8 * n;
            if ((v << shift) >> shift) == v {
                return n;
            }
        }
        8
    }
    let mut out = Vec::new();
    let mut prev: i64 = 0;
    for &(lcn, len) in runs {
        let lb = len.to_le_bytes();
        let ln = (8 - len.leading_zeros() as usize / 8).max(1);
        match lcn {
            None => {
                out.push(ln as u8);
                out.extend_from_slice(&lb[..ln]);
            }
            Some(l) => {
                let delta = l as i64 - prev;
                prev = l as i64;
                let on = min_signed_bytes(delta);
                out.push(((on as u8) << 4) | ln as u8);
                out.extend_from_slice(&lb[..ln]);
                out.extend_from_slice(&delta.to_le_bytes()[..on]);
            }
        }
    }
    out.push(0);
    out
}

fn le_unsigned(b: &[u8]) -> u64 {
    b.iter().rev().fold(0u64, |acc, &x| (acc << 8) | x as u64)
}

fn le_signed(b: &[u8]) -> i64 {
    let v = le_unsigned(b);
    let bits = 8 * b.len() as u32;
    if bits >= 64 {
        v as i64
    } else {
        let shift = 64 - bits;
        ((v << shift) as i64) >> shift
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn simple(d: &DecodedRuns) -> Vec<(u64, Option<u64>, u64)> {
        d.runs.iter().map(|r| (r.vcn, r.lcn, r.len)).collect()
    }

    #[test]
    fn flatcap_examples() {
        let d = decode_runs(&[0x21, 0x18, 0x34, 0x56, 0x00], 0, None);
        assert_eq!(simple(&d), vec![(0, Some(0x5634), 0x18)]);
        assert!(d.error.is_none());
        let d = decode_runs(
            &[
                0x11, 0x30, 0x60, 0x21, 0x10, 0x00, 0x01, 0x11, 0x20, 0xE0, 0x00,
            ],
            0,
            None,
        );
        assert_eq!(
            simple(&d),
            vec![
                (0, Some(0x60), 0x30),
                (0x30, Some(0x160), 0x10),
                (0x40, Some(0x140), 0x20)
            ]
        );
        let d = decode_runs(
            &[0x11, 0x30, 0x20, 0x01, 0x60, 0x11, 0x10, 0x30, 0x00],
            0,
            None,
        );
        assert_eq!(
            simple(&d),
            vec![
                (0, Some(0x20), 0x30),
                (0x30, None, 0x60),
                (0x90, Some(0x50), 0x10)
            ]
        );
        assert_eq!(d.next_vcn, 0xA0);
        // mkntfs $MFT: 19 clusters at LCN 4.
        let d = decode_runs(&[0x11, 0x13, 0x04, 0x00], 0, Some(100));
        assert_eq!(simple(&d), vec![(0, Some(4), 0x13)]);
    }

    #[test]
    fn lowest_vcn_offsets_runs() {
        let d = decode_runs(&[0x11, 0x05, 0x10, 0x00], 100, None);
        assert_eq!(simple(&d), vec![(100, Some(0x10), 5)]);
        assert_eq!(d.next_vcn, 105);
    }

    #[test]
    fn hostile_inputs_stop_cleanly() {
        // Negative LCN.
        let d = decode_runs(&[0x11, 0x05, 0xF0, 0x00], 0, None);
        assert!(d.error.is_some() && d.runs.is_empty());
        // Outside the volume.
        let d = decode_runs(&[0x11, 0x05, 0x10, 0x11, 0x05, 0x70, 0x00], 0, Some(0x50));
        assert_eq!(d.runs.len(), 1);
        assert!(d.error.is_some());
        // Zero length.
        let d = decode_runs(&[0x11, 0x00, 0x10, 0x00], 0, None);
        assert!(d.error.is_some());
        // Truncated body.
        let d = decode_runs(&[0x44, 0x01, 0x02], 0, None);
        assert!(d.error.is_some());
        // Bad nibble sizes.
        let d = decode_runs(&[0x9F, 0, 0], 0, None);
        assert!(d.error.is_some());
        // Missing terminator.
        let d = decode_runs(&[0x11, 0x01, 0x01], 0, None);
        assert_eq!(d.runs.len(), 1);
        assert!(d.error.is_some());
        // VCN overflow.
        let d = decode_runs(
            &[0x08, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00],
            2,
            None,
        );
        assert!(d.error.is_some());
        // Empty input is simply "no runs".
        let d = decode_runs(&[], 0, None);
        assert!(d.runs.is_empty() && d.error.is_none());
    }

    #[test]
    fn encode_decode_roundtrip() {
        let runs = [
            (Some(1000u64), 16u64),
            (None, 300),
            (Some(10), 1),
            (Some(70000), 70000),
            (Some(3), 2),
        ];
        let bytes = encode_runs(&runs);
        let d = decode_runs(&bytes, 0, None);
        assert!(d.error.is_none());
        let got: Vec<(Option<u64>, u64)> = d.runs.iter().map(|r| (r.lcn, r.len)).collect();
        assert_eq!(got, runs.to_vec());
    }

    #[test]
    fn sign_extension() {
        assert_eq!(le_signed(&[0xE0]), -0x20);
        assert_eq!(le_signed(&[0x00, 0x80]), -0x8000);
        assert_eq!(le_signed(&[0xFF, 0x7F]), 0x7FFF);
        assert_eq!(le_signed(&[0xFF; 8]), -1);
    }
}
