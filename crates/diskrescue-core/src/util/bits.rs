//! MSB-first bit reader with Exp-Golomb helpers (H.264/HEVC headers, JPEG
//! Huffman streams after unstuffing).

/// Reads bits most-significant-bit first from a byte slice.
#[derive(Debug, Clone)]
pub struct BitReader<'a> {
    data: &'a [u8],
    pos: usize, // in bits
}

impl<'a> BitReader<'a> {
    /// Start at the first bit of `data`.
    pub fn new(data: &'a [u8]) -> Self {
        BitReader { data, pos: 0 }
    }

    /// Current position in bits.
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Bits remaining.
    pub fn remaining(&self) -> usize {
        self.data.len().saturating_mul(8).saturating_sub(self.pos)
    }

    /// True when all bits are consumed.
    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    /// Move to an absolute bit position.
    pub fn seek_bits(&mut self, pos: usize) {
        self.pos = pos.min(self.data.len() * 8);
    }

    /// Read one bit.
    #[inline]
    pub fn read_bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.pos / 8)?;
        let bit = (byte >> (7 - (self.pos % 8))) & 1;
        self.pos += 1;
        Some(bit as u32)
    }

    /// Read `n` bits (0..=32) as an unsigned integer.
    pub fn read_bits(&mut self, n: u32) -> Option<u32> {
        if n == 0 {
            return Some(0);
        }
        if n > 32 || self.remaining() < n as usize {
            return None;
        }
        let mut v: u64 = 0;
        let mut left = n;
        while left > 0 {
            let byte = self.data[self.pos / 8] as u64;
            let bit_off = (self.pos % 8) as u32;
            let avail = 8 - bit_off;
            let take = avail.min(left);
            let chunk = (byte >> (avail - take)) & ((1u64 << take) - 1);
            v = (v << take) | chunk;
            self.pos += take as usize;
            left -= take;
        }
        Some(v as u32)
    }

    /// Read `n` bits (0..=64).
    pub fn read_bits64(&mut self, n: u32) -> Option<u64> {
        if n <= 32 {
            return self.read_bits(n).map(u64::from);
        }
        let hi = self.read_bits(n - 32)? as u64;
        let lo = self.read_bits(32)? as u64;
        Some((hi << 32) | lo)
    }

    /// Look at the next `n` bits without consuming them.
    pub fn peek_bits(&self, n: u32) -> Option<u32> {
        let mut c = self.clone();
        c.read_bits(n)
    }

    /// Skip `n` bits.
    pub fn skip(&mut self, n: usize) -> Option<()> {
        if self.remaining() < n {
            return None;
        }
        self.pos += n;
        Some(())
    }

    /// Read a boolean flag (one bit).
    pub fn read_flag(&mut self) -> Option<bool> {
        self.read_bit().map(|b| b == 1)
    }

    /// Unsigned Exp-Golomb `ue(v)`.
    pub fn read_ue(&mut self) -> Option<u32> {
        let mut zeros = 0u32;
        while self.read_bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        if zeros == 0 {
            return Some(0);
        }
        let rest = self.read_bits(zeros)?;
        Some(((1u64 << zeros) - 1 + rest as u64) as u32)
    }

    /// Signed Exp-Golomb `se(v)`.
    pub fn read_se(&mut self) -> Option<i32> {
        let k = self.read_ue()? as i64;
        Some(if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) } as i32)
    }

    /// Align to the next byte boundary (no-op if already aligned).
    pub fn align_byte(&mut self) {
        let rem = self.pos % 8;
        if rem != 0 {
            self.pos += 8 - rem;
        }
    }

    /// Byte index of the current position.
    pub fn byte_position(&self) -> usize {
        self.pos / 8
    }
}

/// Remove H.264/HEVC emulation-prevention bytes (`00 00 03` -> `00 00`).
pub fn unescape_nal(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0usize;
    for &b in data {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        out.push(b);
        if b == 0 {
            zeros += 1;
        } else {
            zeros = 0;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bits_and_golomb() {
        // 1 010 011 00100 -> ue: 0, 1, 2, 3
        let data = [0b1010_0110, 0b0100_0000];
        let mut r = BitReader::new(&data);
        assert_eq!(r.read_ue(), Some(0));
        assert_eq!(r.read_ue(), Some(1));
        assert_eq!(r.read_ue(), Some(2));
        assert_eq!(r.read_ue(), Some(3));
        let data = [0b0001_0000, 0b1111_0000];
        let mut r = BitReader::new(&data);
        assert_eq!(r.read_bits(3), Some(0));
        assert_eq!(r.read_bits(9), Some(0b1_0000_1111));
        assert_eq!(r.remaining(), 4);
        assert_eq!(r.read_bits(5), None);
        let data = [0b0110_0000];
        let mut r = BitReader::new(&data);
        assert_eq!(r.read_se(), Some(-1)); // 011 -> ue 2 -> se -1
        let data = [0b0100_0000];
        let mut r = BitReader::new(&data);
        assert_eq!(r.read_se(), Some(1)); // 010 -> ue 1 -> se 1
    }

    #[test]
    fn nal_unescape() {
        assert_eq!(
            unescape_nal(&[0, 0, 3, 1, 0, 0, 3, 0]),
            vec![0, 0, 1, 0, 0, 0]
        );
        assert_eq!(unescape_nal(&[0, 0, 0, 3]), vec![0, 0, 0]);
        assert_eq!(unescape_nal(&[1, 2, 3]), vec![1, 2, 3]);
    }
}
