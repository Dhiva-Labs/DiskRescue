//! Content hashing for de-duplication.

use crate::source::{Source, SourceExt};

/// BLAKE3 hash of a byte slice as lowercase hex.
pub fn blake3_hex(data: &[u8]) -> String {
    blake3::hash(data).to_hex().to_string()
}

/// BLAKE3 hash of a whole source, streamed in 4 MiB chunks. Unreadable
/// ranges are skipped (treated as absent) so the hash still identifies the
/// readable content.
pub fn blake3_source(src: &dyn Source) -> Result<String, crate::source::ReadError> {
    let mut hasher = blake3::Hasher::new();
    let size = src.size();
    let mut pos = 0u64;
    const CHUNK: usize = 4 << 20;
    while pos < size {
        let want = ((size - pos).min(CHUNK as u64)) as usize;
        match src.read_vec(pos, want) {
            Ok(v) if v.is_empty() => break,
            Ok(v) => {
                hasher.update(&v);
                pos += v.len() as u64;
            }
            Err(e) if e.kind.is_media_error() => {
                // Skip the failing sector-sized unit and continue.
                let step = src.sector_size().max(512) as u64;
                pos = e.offset.max(pos) + step;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// Hash of the first `n` bytes, for quick duplicate pre-filtering.
pub fn blake3_prefix(src: &dyn Source, n: usize) -> Result<String, crate::source::ReadError> {
    let v = src.read_vec(0, n)?;
    Ok(blake3_hex(&v))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::MemSource;

    #[test]
    fn source_hash_matches_slice_hash() {
        let data: Vec<u8> = (0..100_000u32).map(|i| (i * 7 % 256) as u8).collect();
        let s = MemSource::new(data.clone());
        assert_eq!(blake3_source(&s).unwrap(), blake3_hex(&data));
        assert_eq!(blake3_prefix(&s, 16).unwrap(), blake3_hex(&data[..16]));
    }
}
