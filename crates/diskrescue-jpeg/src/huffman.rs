//! Huffman decoding tables (T.81 Annex C and F.2.2.3).
//!
//! A [`HuffDecoder`] is built from the BITS/HUFFVAL pair of a DHT entry. It
//! has a 9-bit direct lookup table that resolves the vast majority of codes
//! in one step and the classic MAXCODE/VALPTR/MINCODE slow path for longer
//! codes. Construction rejects tables that are not prefix codes (Kraft
//! violation, all-ones code, more than 256 symbols).

/// Bits resolved by the direct lookup table.
pub(crate) const LOOKUP_BITS: u32 = 9;

/// Decoding form of one Huffman table.
#[derive(Clone)]
pub(crate) struct HuffDecoder {
    /// `(len << 8) | value` for every 9-bit prefix whose code is at most 9
    /// bits long; 0 when the slow path is needed.
    lookup: Box<[u16; 1 << LOOKUP_BITS]>,
    maxcode: [i32; 18],
    valptr: [i32; 17],
    mincode: [i32; 17],
    values: Vec<u8>,
}

impl HuffDecoder {
    /// Build from `bits` (counts of codes of length 1..=16) and `values`.
    pub(crate) fn new(bits: &[u8; 16], values: &[u8]) -> Result<Self, String> {
        let n: usize = bits.iter().map(|&b| b as usize).sum();
        if n > 256 {
            return Err(format!("{n} symbols (more than 256)"));
        }
        if values.len() < n {
            return Err(format!("{} values for {n} codes", values.len()));
        }
        // HUFFSIZE / HUFFCODE generation (C.1, C.2).
        let mut sizes = Vec::with_capacity(n);
        for (i, &count) in bits.iter().enumerate() {
            for _ in 0..count {
                sizes.push(i as u32 + 1);
            }
        }
        let mut codes = Vec::with_capacity(n);
        let mut code: u32 = 0;
        let mut si: u32 = sizes.first().copied().unwrap_or(1);
        let mut k = 0usize;
        while k < n {
            while k < n && sizes[k] == si {
                codes.push(code);
                code += 1;
                k += 1;
            }
            // The codes of length `si` must fit in `si` bits. (libjpeg also
            // refuses a complete code that uses the all-ones pattern; we
            // accept it, as other decoders do, to stay tolerant.)
            if code > (1u32 << si) {
                return Err(format!("code space overflow at length {si}"));
            }
            if k >= n {
                break;
            }
            while sizes[k] != si {
                code <<= 1;
                si += 1;
                if si > 16 {
                    return Err("code length above 16".into());
                }
            }
        }
        let mut maxcode = [-1i32; 18];
        let mut valptr = [0i32; 17];
        let mut mincode = [0i32; 17];
        let mut j = 0usize;
        for l in 1..=16usize {
            let count = bits[l - 1] as usize;
            if count == 0 {
                maxcode[l] = -1;
            } else {
                valptr[l] = j as i32;
                mincode[l] = codes[j] as i32;
                j += count - 1;
                maxcode[l] = codes[j] as i32;
                j += 1;
            }
        }
        maxcode[17] = i32::MAX;
        let mut lookup = Box::new([0u16; 1 << LOOKUP_BITS]);
        for (idx, (&size, &c)) in sizes.iter().zip(codes.iter()).enumerate() {
            if size <= LOOKUP_BITS {
                let shift = LOOKUP_BITS - size;
                let first = (c << shift) as usize;
                let entry = ((size as u16) << 8) | values[idx] as u16;
                for slot in lookup.iter_mut().skip(first).take(1 << shift) {
                    *slot = entry;
                }
            }
        }
        Ok(HuffDecoder {
            lookup,
            maxcode,
            valptr,
            mincode,
            values: values[..n].to_vec(),
        })
    }

    /// Decode one symbol from the 16 most significant bits of `peek`.
    /// Returns `(code length, symbol)`, or `None` for an invalid code.
    #[inline]
    pub(crate) fn decode(&self, peek: u32) -> Option<(u32, u8)> {
        let e = self.lookup[(peek >> (16 - LOOKUP_BITS)) as usize];
        if e != 0 {
            return Some(((e >> 8) as u32, e as u8));
        }
        self.decode_slow(peek)
    }

    #[cold]
    fn decode_slow(&self, peek: u32) -> Option<(u32, u8)> {
        for l in (LOOKUP_BITS as usize + 1)..=16 {
            let code = (peek >> (16 - l)) as i32;
            if code <= self.maxcode[l] {
                let idx = self.valptr[l] + code - self.mincode[l];
                let v = *self.values.get(usize::try_from(idx).ok()?)?;
                return Some((l as u32, v));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tables::*;

    fn encode_table(bits: &[u8; 16], values: &[u8]) -> Vec<(u32, u32, u8)> {
        // (length, code, value) per Annex C, independent of the decoder.
        let mut out = Vec::new();
        let mut code = 0u32;
        let mut k = 0usize;
        for (i, &count) in bits.iter().enumerate() {
            for _ in 0..count {
                out.push((i as u32 + 1, code, values[k]));
                code += 1;
                k += 1;
            }
            code <<= 1;
        }
        out
    }

    #[test]
    fn decodes_every_standard_code() {
        for (bits, vals) in [
            (&DC_LUMINANCE_BITS, &DC_LUMINANCE_VALUES[..]),
            (&AC_LUMINANCE_BITS, &AC_LUMINANCE_VALUES[..]),
            (&DC_CHROMINANCE_BITS, &DC_CHROMINANCE_VALUES[..]),
            (&AC_CHROMINANCE_BITS, &AC_CHROMINANCE_VALUES[..]),
        ] {
            let d = HuffDecoder::new(bits, vals).unwrap();
            for (len, code, val) in encode_table(bits, vals) {
                // Pad with ones after the code; must still decode the same.
                let peek = (code << (16 - len)) | ((1 << (16 - len)) - 1);
                assert_eq!(d.decode(peek), Some((len, val)), "len {len} code {code:b}");
                let peek0 = code << (16 - len);
                assert_eq!(d.decode(peek0), Some((len, val)));
            }
            // The all-ones 16-bit pattern is never a valid code.
            assert_eq!(d.decode(0xFFFF), None);
        }
    }

    #[test]
    fn rejects_bad_tables() {
        // Three codes of length 1: impossible.
        let mut bits = [0u8; 16];
        bits[0] = 3;
        assert!(HuffDecoder::new(&bits, &[1, 2, 3]).is_err());
        // Two codes of length 1 form a complete code: tolerated.
        bits[0] = 2;
        assert!(HuffDecoder::new(&bits, &[1, 2]).is_ok());
        // Too few values.
        bits[0] = 1;
        assert!(HuffDecoder::new(&bits, &[]).is_err());
        // 300 symbols.
        let bits = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 255];
        let mut b2 = bits;
        b2[14] = 45;
        assert!(HuffDecoder::new(&b2, &[0u8; 300]).is_err());
        // Empty table is accepted but decodes nothing.
        let d = HuffDecoder::new(&[0u8; 16], &[]).unwrap();
        assert_eq!(d.decode(0), None);
    }
}
