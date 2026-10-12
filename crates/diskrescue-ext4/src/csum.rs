//! Checksums used by ext4 metadata.
//!
//! ext4 uses two checksum algorithms, both in the Linux kernel's "raw" form
//! (the caller supplies the initial value and no final XOR is applied):
//!
//! * CRC-32C (Castagnoli, reflected polynomial `0x82F63B78`) for
//!   `metadata_csum`: superblock, group descriptors, inodes, directory blocks.
//! * CRC-16 (polynomial `0x8005`, reflected table built from `0xA001`, i.e.
//!   CRC-16/MODBUS when started from `0xFFFF`) for the older `gdt_csum`
//!   group-descriptor checksum.
//!
//! The parser only uses checksums to *score* structures (pick between a
//! primary and a backup, warn about damage); it never refuses to read data
//! because a checksum does not match.

const fn make_table(poly: u32) -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ poly } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

static CRC32C_TABLE: [u32; 256] = make_table(0x82F6_3B78);
static CRC16_TABLE: [u32; 256] = make_table(0xA001);

/// Kernel-style CRC-32C: starts from `init`, no final inversion.
///
/// The standard (finalised) CRC-32C of `data` is `!crc32c_raw(!0, data)`.
pub fn crc32c_raw(init: u32, data: &[u8]) -> u32 {
    let mut c = init;
    for &b in data {
        c = CRC32C_TABLE[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    c
}

/// Kernel-style CRC-16 (poly 0x8005, reflected): starts from `init`, no
/// final inversion. `crc16(0xFFFF, data)` is CRC-16/MODBUS.
pub fn crc16(init: u16, data: &[u8]) -> u16 {
    let mut c = init as u32;
    for &b in data {
        c = (c >> 8) ^ CRC16_TABLE[((c ^ b as u32) & 0xFF) as usize];
    }
    c as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32c_check_value() {
        // Standard CRC-32C check value for "123456789".
        assert_eq!(!crc32c_raw(!0, b"123456789"), 0xE306_9283);
        assert_eq!(crc32c_raw(0x1234_5678, b""), 0x1234_5678);
        // Chaining equals one-shot.
        let a = crc32c_raw(!0, b"12345");
        assert_eq!(crc32c_raw(a, b"6789"), crc32c_raw(!0, b"123456789"));
    }

    #[test]
    fn crc16_check_value() {
        // CRC-16/MODBUS check value for "123456789".
        assert_eq!(crc16(0xFFFF, b"123456789"), 0x4B37);
        // CRC-16/ARC (init 0) check value.
        assert_eq!(crc16(0, b"123456789"), 0xBB3D);
    }
}
