//! LZNT1 decompression (NTFS file compression).
//!
//! A compressed NTFS attribute is split into compression units of
//! `2^compression_unit` clusters (16 clusters = 64 KiB with 4 KiB clusters).
//! A unit that is stored compressed holds a sequence of chunks, each
//! producing up to 4096 bytes of output:
//!
//! * a 2-byte little-endian header: bits 0..11 = total chunk length - 3
//!   (i.e. data bytes - 1), bit 15 = the chunk is compressed; a header of 0
//!   ends the unit early;
//! * an uncompressed chunk carries exactly 4096 literal bytes;
//! * a compressed chunk is a series of groups: one flag byte followed by up
//!   to eight tokens; a clear flag bit is a literal byte, a set bit is a
//!   2-byte back-reference whose split between displacement and length
//!   depends on how much of the chunk has been produced so far (the more
//!   output, the more bits go to the displacement).
//!
//! A compressed chunk that produces fewer than 4096 bytes is zero-padded to
//! 4096 bytes, as ntfs-3g and Windows do. Malformed input never panics; the
//! output produced so far is kept and the error describes the problem.

use diskrescue_core::util::bytes::u16le;

/// Uncompressed size of one chunk.
pub const CHUNK_SIZE: usize = 4096;

/// Why decompression stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lznt1Error {
    /// A chunk's declared length runs past the input.
    TruncatedChunk {
        /// Input offset where the problem was found.
        at: usize,
    },
    /// A back-reference token was cut off by the end of the chunk.
    TruncatedToken {
        /// Input offset where the problem was found.
        at: usize,
    },
    /// A back-reference appeared before any output in its chunk.
    BackrefAtStart {
        /// Input offset where the problem was found.
        at: usize,
    },
    /// A back-reference points before the start of its chunk.
    BadDisplacement {
        /// Input offset where the problem was found.
        at: usize,
    },
    /// The chunk would produce more than 4096 bytes.
    ChunkOverflow {
        /// Input offset where the problem was found.
        at: usize,
    },
}

impl std::fmt::Display for Lznt1Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Lznt1Error::TruncatedChunk { at } => write!(f, "LZNT1 chunk at {at} is truncated"),
            Lznt1Error::TruncatedToken { at } => write!(f, "LZNT1 token at {at} is truncated"),
            Lznt1Error::BackrefAtStart { at } => {
                write!(f, "LZNT1 back-reference at {at} precedes any output")
            }
            Lznt1Error::BadDisplacement { at } => {
                write!(f, "LZNT1 back-reference at {at} points before the chunk")
            }
            Lznt1Error::ChunkOverflow { at } => {
                write!(
                    f,
                    "LZNT1 chunk at {at} decompresses to more than 4096 bytes"
                )
            }
        }
    }
}

/// Decompress `input`, appending at most `max_out` bytes to `out`. On error
/// the bytes produced before the failure stay in `out`.
pub fn decompress(input: &[u8], max_out: usize, out: &mut Vec<u8>) -> Result<(), Lznt1Error> {
    let start = out.len();
    let mut pos = 0usize;
    loop {
        let produced = out.len() - start;
        if produced >= max_out {
            return Ok(());
        }
        let Some(header) = u16le(input, pos) else {
            return Ok(());
        };
        if header == 0 {
            return Ok(());
        }
        let data_len = (header & 0x0FFF) as usize + 1;
        let data_start = pos + 2;
        let data_end = data_start + data_len;
        if data_end > input.len() {
            return Err(Lznt1Error::TruncatedChunk { at: pos });
        }
        let limit = CHUNK_SIZE.min(max_out - produced);
        if header & 0x8000 == 0 {
            // Stored chunk: copy as-is (normally exactly 4096 bytes). A
            // shorter one still occupies a whole 4096-byte slot, so the
            // chunks after it stay at their offsets.
            let n = data_len.min(limit);
            let chunk_out = out.len();
            out.extend_from_slice(&input[data_start..data_start + n]);
            out.resize(chunk_out + limit, 0);
        } else {
            decompress_chunk(input, data_start, data_end, limit, out)?;
        }
        pos = data_end;
    }
}

fn decompress_chunk(
    input: &[u8],
    data_start: usize,
    data_end: usize,
    limit: usize,
    out: &mut Vec<u8>,
) -> Result<(), Lznt1Error> {
    let chunk_out = out.len();
    let mut i = data_start;
    while i < data_end {
        let tag = input[i];
        i += 1;
        for bit in 0..8 {
            if i >= data_end {
                break;
            }
            let produced = out.len() - chunk_out;
            if tag & (1 << bit) == 0 {
                if produced >= limit {
                    return Err(Lznt1Error::ChunkOverflow { at: i });
                }
                out.push(input[i]);
                i += 1;
            } else {
                let Some(token) = u16le(input, i).filter(|_| i + 2 <= data_end) else {
                    return Err(Lznt1Error::TruncatedToken { at: i });
                };
                if produced == 0 {
                    return Err(Lznt1Error::BackrefAtStart { at: i });
                }
                let mut lg = 0u32;
                let mut p = produced - 1;
                while p >= 0x10 {
                    lg += 1;
                    p >>= 1;
                }
                let back = (token >> (12 - lg)) as usize + 1;
                let length = (token & (0x0FFF >> lg)) as usize + 3;
                if back > produced {
                    return Err(Lznt1Error::BadDisplacement { at: i });
                }
                if produced + length > limit {
                    return Err(Lznt1Error::ChunkOverflow { at: i });
                }
                for _ in 0..length {
                    let b = out[out.len() - back];
                    out.push(b);
                }
                i += 2;
            }
        }
    }
    // A short compressed chunk still occupies a full 4096-byte slot.
    let produced = out.len() - chunk_out;
    if produced < limit {
        out.resize(chunk_out + limit, 0);
    }
    Ok(())
}

/// Decompress one compression unit of `unit_size` bytes. The result is
/// always exactly `unit_size` bytes (zero-padded); the error, if any,
/// describes why the tail may be missing.
pub fn decompress_unit(input: &[u8], unit_size: usize) -> (Vec<u8>, Option<Lznt1Error>) {
    let mut out = Vec::with_capacity(unit_size);
    let err = decompress(input, unit_size, &mut out).err();
    out.resize(unit_size, 0);
    (out, err)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compressed_chunk(data: &[u8]) -> Vec<u8> {
        let header = 0xB000u16 | (data.len() as u16 - 1);
        let mut v = header.to_le_bytes().to_vec();
        v.extend_from_slice(data);
        v
    }

    #[test]
    fn literal_only_chunk() {
        let mut data = vec![0x00];
        data.extend_from_slice(b"abcdefgh");
        data.push(0x00);
        data.extend_from_slice(b"ij");
        let input = compressed_chunk(&data);
        let mut out = Vec::new();
        decompress(&input, 8192, &mut out).unwrap();
        assert_eq!(out.len(), 4096);
        assert_eq!(&out[..10], b"abcdefghij");
        assert!(out[10..].iter().all(|&b| b == 0));
    }

    #[test]
    fn back_reference_chunk() {
        // "abc" then a back-reference of length 9, displacement 3.
        let data = [0x08, b'a', b'b', b'c', 0x06, 0x20];
        let input = compressed_chunk(&data);
        let (out, err) = decompress_unit(&input, 4096);
        assert!(err.is_none());
        assert_eq!(&out[..12], b"abcabcabcabc");
        assert!(out[12..].iter().all(|&b| b == 0));
    }

    #[test]
    fn displacement_split_grows_with_position() {
        // 17 literals, then a back-reference at position 17: the position
        // minus one (16) is >= 0x10, so one bit moves from the length to the
        // displacement (shift 11, length mask 0x7FF).
        let mut data = vec![0x00];
        data.extend_from_slice(b"01234567");
        data.push(0x00);
        data.extend_from_slice(b"89ABCDEF");
        // Literal 'G', then copy 5 bytes from 17 back.
        let token: u16 = (16 << 11) | (5 - 3);
        data.push(0x02);
        data.push(b'G');
        data.extend_from_slice(&token.to_le_bytes());
        let (out, err) = decompress_unit(&compressed_chunk(&data), 4096);
        assert!(err.is_none(), "{err:?}");
        assert_eq!(&out[..22], b"0123456789ABCDEFG01234");
        // At position 16 the split is still 12/4.
        let mut data = vec![0x00];
        data.extend_from_slice(b"01234567");
        data.push(0x00);
        data.extend_from_slice(b"89ABCDEF");
        let token: u16 = (15 << 12) | (4 - 3);
        data.push(0x01);
        data.extend_from_slice(&token.to_le_bytes());
        let (out, err) = decompress_unit(&compressed_chunk(&data), 4096);
        assert!(err.is_none(), "{err:?}");
        assert_eq!(&out[..20], b"0123456789ABCDEF0123");
    }

    #[test]
    fn uncompressed_chunk_and_sequence() {
        let raw: Vec<u8> = (0..4096u32).map(|i| (i * 31 % 256) as u8).collect();
        let mut input = 0x3FFFu16.to_le_bytes().to_vec();
        input.extend_from_slice(&raw);
        let data = [0x08, b'x', b'y', b'z', 0x00, 0x20];
        input.extend_from_slice(&compressed_chunk(&data));
        input.extend_from_slice(&[0, 0]);
        let (out, err) = decompress_unit(&input, 65536);
        assert!(err.is_none());
        assert_eq!(&out[..4096], &raw[..]);
        assert_eq!(&out[4096..4096 + 6], b"xyzxyz");
        assert_eq!(out.len(), 65536);
    }

    #[test]
    fn short_stored_chunk_keeps_the_next_chunk_in_its_slot() {
        let mut input = (0x3000u16 | 9).to_le_bytes().to_vec();
        input.extend_from_slice(b"0123456789");
        let data = [0x08, b'x', b'y', b'z', 0x00, 0x20];
        input.extend_from_slice(&compressed_chunk(&data));
        let (out, err) = decompress_unit(&input, 65536);
        assert!(err.is_none());
        assert_eq!(&out[..10], b"0123456789");
        assert!(out[10..4096].iter().all(|&b| b == 0));
        assert_eq!(&out[4096..4102], b"xyzxyz");
    }

    #[test]
    fn malformed_inputs_do_not_panic() {
        // Back-reference first.
        let (_, e) = decompress_unit(&compressed_chunk(&[0x01, 0x00, 0x10]), 4096);
        assert!(matches!(e, Some(Lznt1Error::BackrefAtStart { .. })));
        // Displacement too far back.
        let (_, e) = decompress_unit(&compressed_chunk(&[0x02, b'a', 0x00, 0xF0]), 4096);
        assert!(matches!(e, Some(Lznt1Error::BadDisplacement { .. })));
        // Truncated chunk.
        let (_, e) = decompress_unit(&[0x10, 0xB0, 0x00], 4096);
        assert!(matches!(e, Some(Lznt1Error::TruncatedChunk { .. })));
        // Truncated token.
        let (_, e) = decompress_unit(&compressed_chunk(&[0x02, b'a', 0x00]), 4096);
        assert!(matches!(e, Some(Lznt1Error::TruncatedToken { .. })));
        // Overflow: a back-reference that would exceed 4096 bytes.
        let mut data = vec![0x02, b'a'];
        data.extend_from_slice(&0x0FFFu16.to_le_bytes());
        let (out, e) = decompress_unit(&compressed_chunk(&data), 4096);
        assert!(matches!(e, Some(Lznt1Error::ChunkOverflow { .. })));
        assert_eq!(out.len(), 4096);
        // Random garbage.
        let mut seed = 0x1234_5678u32;
        for _ in 0..200 {
            let len = (seed % 300) as usize;
            let buf: Vec<u8> = (0..len)
                .map(|_| {
                    seed ^= seed << 13;
                    seed ^= seed >> 17;
                    seed ^= seed << 5;
                    seed as u8
                })
                .collect();
            let (out, _) = decompress_unit(&buf, 65536);
            assert_eq!(out.len(), 65536);
        }
    }
}
