//! Hand-built TIFF-family, RAF and PSD files. Every builder returns a file
//! whose declared structure ends exactly at its last byte, so the expected
//! extent of each sample is its length.

use super::*;

/// A generated file and what the tests should expect of it.
pub struct Sample {
    pub name: &'static str,
    /// Id of the dedicated format that must accept it.
    pub id: &'static str,
    pub bytes: Vec<u8>,
    /// Suggested extension (`meta.extra["extension"]`) when the format sets one.
    pub ext: &'static str,
}

fn u16x(be: bool, v: u16) -> [u8; 2] {
    if be {
        v.to_be_bytes()
    } else {
        v.to_le_bytes()
    }
}

fn u32x(be: bool, v: u32) -> [u8; 4] {
    if be {
        v.to_be_bytes()
    } else {
        v.to_le_bytes()
    }
}

/// One raw 12-byte IFD entry with an inline LONG (or SHORT/BYTE) value.
fn raw_entry(be: bool, tag: u16, typ: u16, count: u32, value: [u8; 4]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&u16x(be, tag));
    v.extend_from_slice(&u16x(be, typ));
    v.extend_from_slice(&u32x(be, count));
    v.extend_from_slice(&value);
    v
}

fn long_entry(be: bool, tag: u16, v: u32) -> Vec<u8> {
    raw_entry(be, tag, 4, 1, u32x(be, v))
}

/// Nikon maker note with a `PreviewIFD` (tag 0x0011) holding a JPEG pointer.
fn nikon_mn(be: bool, mn_abs: usize, prev_abs: usize, prev_len: usize) -> Vec<u8> {
    let base = mn_abs + 10;
    let mut v = b"Nikon\0".to_vec();
    v.extend_from_slice(&[2, 0x10, 0, 0]);
    v.extend_from_slice(if be { b"MM\0*" } else { b"II*\0" });
    v.extend_from_slice(&u32x(be, 8));
    v.extend_from_slice(&u16x(be, 1));
    v.extend(long_entry(be, 0x0011, 26));
    v.extend_from_slice(&u32x(be, 0));
    v.extend_from_slice(&u16x(be, 2));
    v.extend(long_entry(be, 0x0201, prev_abs.saturating_sub(base) as u32));
    v.extend(long_entry(be, 0x0202, prev_len as u32));
    v.extend_from_slice(&u32x(be, 0));
    assert_eq!(v.len(), 66);
    v
}

/// Pentax `AOC\0` maker note; the preview offset is absolute.
fn pentax_mn(be: bool, prev_abs: usize, prev_len: usize) -> Vec<u8> {
    let mut v = b"AOC\0".to_vec();
    v.extend_from_slice(if be { b"MM" } else { b"II" });
    v.extend_from_slice(&u16x(be, 2));
    v.extend(long_entry(be, 0x0003, prev_len as u32));
    v.extend(long_entry(be, 0x0004, prev_abs as u32));
    v.extend_from_slice(&u32x(be, 0));
    assert_eq!(v.len(), 6 + 2 + 24 + 4);
    v
}

/// Olympus `OLYMPUS\0` maker note; offsets are relative to the maker note.
fn olympus_mn(be: bool, mn_abs: usize, prev_abs: usize, prev_len: usize) -> Vec<u8> {
    let mut v = b"OLYMPUS\0".to_vec();
    v.extend_from_slice(if be { b"MM" } else { b"II" });
    v.extend_from_slice(&u16x(be, 3));
    v.extend_from_slice(&u16x(be, 1));
    v.extend(long_entry(be, 0x2020, 30));
    v.extend_from_slice(&u32x(be, 0));
    v.extend_from_slice(&u16x(be, 3));
    v.extend(raw_entry(be, 0x0100, 3, 1, {
        let mut f = [0u8; 4];
        f[..2].copy_from_slice(&u16x(be, 1));
        f
    }));
    v.extend(long_entry(
        be,
        0x0101,
        prev_abs.saturating_sub(mn_abs) as u32,
    ));
    v.extend(long_entry(be, 0x0102, prev_len as u32));
    v.extend_from_slice(&u32x(be, 0));
    assert_eq!(v.len(), 72);
    v
}

/// Sony maker note carrying only `FileFormat` (0xB000).
fn sony_mn(file_format: [u8; 4]) -> Vec<u8> {
    let mut v = b"SONY DSC \0\0\0".to_vec();
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend(raw_entry(false, 0xB000, 1, 4, file_format));
    v.extend_from_slice(&0u32.to_le_bytes());
    v
}

const DT: &str = "2023:07:14 09:05:59";

fn rgb_bits() -> E {
    e(0x0102, V::Short(vec![8, 8, 8]))
}

pub fn cr2() -> Sample {
    let mut rng = Rng(11);
    let preview = fake_jpeg(1920, 1280, 40_000, &mut rng);
    let thumb = fake_jpeg(160, 120, 3_000, &mut rng);
    let rgb = rng.bytes(128 * 96 * 3);
    let raw = fake_lossless_jpeg(120_000, &mut rng);
    let (bytes, _) = build(false, |o| {
        let mut h = tiff_header(false, 16);
        h.extend_from_slice(b"CR\x02\x00");
        h.extend_from_slice(&(o[4] as u32).to_le_bytes());
        let ifd0 = Item::ifd(
            vec![
                short(0x0100, 1920),
                short(0x0101, 1280),
                rgb_bits(),
                short(0x0103, 6),
                ascii(0x010F, "Canon"),
                ascii(0x0110, "Canon EOS 5D Mark III"),
                long(0x0111, o[5] as u32),
                short(0x0112, 1),
                short(0x0115, 3),
                long(0x0117, preview.len() as u32),
                long(0x8769, o[1] as u32),
            ],
            o[2] as u32,
        );
        let exif = Item::ifd(
            vec![ascii(0x9003, DT), long(0xA002, 5616), long(0xA003, 3744)],
            0,
        );
        let ifd1 = Item::ifd(
            vec![
                short(0x0103, 6),
                long(0x0201, o[6] as u32),
                long(0x0202, thumb.len() as u32),
            ],
            o[3] as u32,
        );
        let ifd2 = Item::ifd(
            vec![
                short(0x0100, 128),
                short(0x0101, 96),
                rgb_bits(),
                short(0x0103, 1),
                short(0x0106, 2),
                long(0x0111, o[7] as u32),
                short(0x0115, 3),
                long(0x0117, rgb.len() as u32),
            ],
            o[4] as u32,
        );
        let ifd3 = Item::ifd(
            vec![
                short(0x0100, 5920),
                short(0x0101, 3950),
                short(0x0102, 14),
                short(0x0103, 6),
                long(0x0111, o[8] as u32),
                long(0x0117, raw.len() as u32),
                e(0xC640, V::Short(vec![1, 2960, 2960])),
            ],
            0,
        );
        (
            h,
            vec![
                ifd0,
                exif,
                ifd1,
                ifd2,
                ifd3,
                Item::Blob(preview.clone()),
                Item::Blob(thumb.clone()),
                Item::Blob(rgb.clone()),
                Item::Blob(raw.clone()),
            ],
        )
    });
    Sample {
        name: "cr2",
        id: "cr2",
        bytes,
        ext: "cr2",
    }
}

pub fn nef(coolpix: bool) -> Sample {
    let mut rng = Rng(12);
    let thumb = rng.bytes(160 * 120 * 3);
    let preview = fake_jpeg(1600, 1066, 30_000, &mut rng);
    let raw = rng.bytes(150_000);
    let mn_prev = fake_jpeg(1600, 1066, 25_000, &mut rng);
    let model = if coolpix {
        "COOLPIX P1000"
    } else {
        "NIKON D850"
    };
    let (bytes, _) = build(true, |o| {
        let exif_entries = |mn: Vec<u8>| {
            vec![
                ascii(0x9003, DT),
                e(0x927C, V::Undef(mn)),
                long(0xA002, 8256),
                long(0xA003, 5504),
            ]
        };
        let mn_abs = value_at(o[1], &exif_entries(vec![0; 66]), 0x927C);
        let mn = nikon_mn(true, mn_abs, o[7], mn_prev.len());
        let ifd0 = Item::ifd(
            vec![
                short(0x0100, 160),
                short(0x0101, 120),
                rgb_bits(),
                short(0x0103, 1),
                short(0x0106, 2),
                ascii(0x010F, "NIKON CORPORATION"),
                ascii(0x0110, model),
                long(0x0111, o[4] as u32),
                short(0x0112, 1),
                short(0x0115, 3),
                long(0x0117, thumb.len() as u32),
                e(0x014A, V::Long(vec![o[2] as u32, o[3] as u32])),
                long(0x8769, o[1] as u32),
            ],
            0,
        );
        let sub_prev = Item::ifd(
            vec![
                long(0x00FE, 1),
                short(0x0100, 1600),
                short(0x0101, 1066),
                short(0x0103, 6),
                long(0x0201, o[5] as u32),
                long(0x0202, preview.len() as u32),
            ],
            0,
        );
        let sub_raw = Item::ifd(
            vec![
                long(0x00FE, 0),
                short(0x0100, 8288),
                short(0x0101, 5520),
                short(0x0102, 14),
                short(0x0103, 34713),
                long(0x0111, o[6] as u32),
                long(0x0117, raw.len() as u32),
            ],
            0,
        );
        (
            tiff_header(true, 8),
            vec![
                ifd0,
                Item::ifd(exif_entries(mn), 0),
                sub_prev,
                sub_raw,
                Item::Blob(thumb.clone()),
                Item::Blob(preview.clone()),
                Item::Blob(raw.clone()),
                Item::Blob(mn_prev.clone()),
            ],
        )
    });
    Sample {
        name: if coolpix { "nrw" } else { "nef" },
        id: "nef",
        bytes,
        ext: if coolpix { "nrw" } else { "nef" },
    }
}

pub fn arw(sr2: bool) -> Sample {
    arw_with(sr2, false)
}

/// ARW whose `DNGPrivateData` pointer is stored as four BYTEs when `byte_ptr`.
pub fn arw_with(sr2: bool, byte_ptr: bool) -> Sample {
    let mut rng = Rng(13);
    let preview = fake_jpeg(1616, 1080, 35_000, &mut rng);
    let raw = rng.bytes(180_000);
    let sr2_data = rng.bytes(4_096);
    let ff = if sr2 { [1, 0, 0, 0] } else { [2, 0, 0, 0] };
    let (bytes, _) = build(false, |o| {
        let ifd0 = Item::ifd(
            vec![
                short(0x0100, 1616),
                short(0x0101, 1080),
                short(0x0103, 6),
                ascii(0x010F, "SONY"),
                ascii(0x0110, "ILCE-7M3"),
                short(0x0112, 1),
                e(0x014A, V::Long(vec![o[2] as u32])),
                long(0x0201, o[3] as u32),
                long(0x0202, preview.len() as u32),
                long(0x8769, o[1] as u32),
                if byte_ptr {
                    e(0xC634, V::Byte((o[5] as u32).to_le_bytes().to_vec()))
                } else {
                    long(0xC634, o[5] as u32)
                },
            ],
            0,
        );
        let exif = Item::ifd(
            vec![
                ascii(0x9003, DT),
                e(0x927C, V::Undef(sony_mn(ff))),
                long(0xA002, 6000),
                long(0xA003, 4000),
            ],
            0,
        );
        let sub_raw = Item::ifd(
            vec![
                long(0x00FE, 0),
                short(0x0100, 6048),
                short(0x0101, 4024),
                short(0x0102, 14),
                short(0x0103, 32767),
                long(0x0111, o[4] as u32),
                long(0x0117, raw.len() as u32),
            ],
            0,
        );
        let sr2_ifd = Item::ifd(
            vec![
                long(0x7200, o[6] as u32),
                long(0x7201, sr2_data.len() as u32),
                long(0x7221, 0x1234),
            ],
            0,
        );
        (
            tiff_header(false, 8),
            vec![
                ifd0,
                exif,
                sub_raw,
                Item::Blob(preview.clone()),
                Item::Blob(raw.clone()),
                sr2_ifd,
                Item::Blob(sr2_data.clone()),
            ],
        )
    });
    Sample {
        name: if sr2 { "sr2" } else { "arw" },
        id: "arw",
        bytes,
        ext: if sr2 { "sr2" } else { "arw" },
    }
}

pub fn dng() -> Sample {
    let mut rng = Rng(14);
    let preview = fake_jpeg(256, 171, 9_000, &mut rng);
    let tiles: Vec<Vec<u8>> = (0..4).map(|_| rng.bytes(20_000)).collect();
    let original = rng.bytes(60_000);
    let (bytes, _) = build(false, |o| {
        let ifd0 = Item::ifd(
            vec![
                long(0x00FE, 1),
                short(0x0100, 256),
                short(0x0101, 171),
                rgb_bits(),
                short(0x0103, 7),
                ascii(0x010F, "PENTAX"),
                ascii(0x0110, "PENTAX K-1"),
                long(0x0111, o[2] as u32),
                short(0x0112, 1),
                short(0x0115, 3),
                long(0x0117, preview.len() as u32),
                e(0x014A, V::Long(vec![o[1] as u32])),
                e(0xC612, V::Byte(vec![1, 4, 0, 0])),
                ptr(0xC68C, 7, original.len(), o[7]),
            ],
            0,
        );
        let sub = Item::ifd(
            vec![
                long(0x00FE, 0),
                short(0x0100, 8000),
                short(0x0101, 6000),
                short(0x0102, 16),
                short(0x0103, 7),
                short(0x0106, 32803),
                short(0x0142, 4000),
                short(0x0143, 3000),
                e(0x0144, V::Long((3..7).map(|i| o[i] as u32).collect())),
                e(
                    0x0145,
                    V::Long(tiles.iter().map(|t| t.len() as u32).collect()),
                ),
            ],
            0,
        );
        let mut items = vec![ifd0, sub, Item::Blob(preview.clone())];
        items.extend(tiles.iter().map(|t| Item::Blob(t.clone())));
        items.push(Item::Blob(original.clone()));
        (tiff_header(false, 8), items)
    });
    Sample {
        name: "dng",
        id: "dng",
        bytes,
        ext: "dng",
    }
}

pub fn pef(ricoh: bool) -> Sample {
    let mut rng = Rng(15);
    let thumb = fake_jpeg(160, 120, 4_000, &mut rng);
    let raw = rng.bytes(140_000);
    let mn_prev = fake_jpeg(1024, 768, 20_000, &mut rng);
    let make = if ricoh {
        "RICOH IMAGING COMPANY, LTD."
    } else {
        "PENTAX Corporation"
    };
    let (bytes, _) = build(true, |o| {
        let ifd0 = Item::ifd(
            vec![
                short(0x0100, 4928),
                short(0x0101, 3264),
                short(0x0102, 12),
                short(0x0103, 65535),
                ascii(0x010F, make),
                ascii(0x0110, "PENTAX K-5"),
                long(0x0111, o[4] as u32),
                short(0x0112, 1),
                long(0x0117, raw.len() as u32),
                long(0x8769, o[1] as u32),
            ],
            o[2] as u32,
        );
        let exif = Item::ifd(
            vec![
                ascii(0x9003, DT),
                e(0x927C, V::Undef(pentax_mn(true, o[5], mn_prev.len()))),
            ],
            0,
        );
        let ifd1 = Item::ifd(
            vec![
                short(0x0103, 6),
                long(0x0201, o[3] as u32),
                long(0x0202, thumb.len() as u32),
            ],
            0,
        );
        (
            tiff_header(true, 8),
            vec![
                ifd0,
                exif,
                ifd1,
                Item::Blob(thumb.clone()),
                Item::Blob(raw.clone()),
                Item::Blob(mn_prev.clone()),
            ],
        )
    });
    Sample {
        name: if ricoh { "pef-ricoh" } else { "pef" },
        id: "pef",
        bytes,
        ext: "pef",
    }
}

pub fn orf(be: bool) -> Sample {
    let mut rng = Rng(16);
    let thumb = fake_jpeg(160, 120, 4_000, &mut rng);
    let raw = rng.bytes(130_000);
    let mn_prev = fake_jpeg(1280, 960, 18_000, &mut rng);
    let (bytes, _) = build(be, |o| {
        let exif_entries = |mn: Vec<u8>| vec![ascii(0x9003, DT), e(0x927C, V::Undef(mn))];
        let mn_abs = value_at(o[1], &exif_entries(vec![0; 72]), 0x927C);
        let mn = olympus_mn(be, mn_abs, o[4], mn_prev.len());
        let ifd0 = Item::ifd(
            vec![
                short(0x0100, 4608),
                short(0x0101, 3456),
                short(0x0102, 12),
                short(0x0103, 1),
                ascii(0x010F, "OLYMPUS IMAGING CORP."),
                ascii(0x0110, "E-M1"),
                long(0x0111, o[3] as u32),
                short(0x0112, 1),
                long(0x0117, raw.len() as u32),
                long(0x8769, o[1] as u32),
            ],
            o[2] as u32,
        );
        let ifd1 = Item::ifd(
            vec![
                short(0x0103, 6),
                long(0x0201, o[5] as u32),
                long(0x0202, thumb.len() as u32),
            ],
            0,
        );
        let h = {
            let mut h = if be {
                b"MMOR".to_vec()
            } else {
                b"IIRO".to_vec()
            };
            h.extend_from_slice(&u32x(be, 8));
            h
        };
        (
            h,
            vec![
                ifd0,
                Item::ifd(exif_entries(mn), 0),
                ifd1,
                Item::Blob(raw.clone()),
                Item::Blob(mn_prev.clone()),
                Item::Blob(thumb.clone()),
            ],
        )
    });
    Sample {
        name: if be { "orf-mm" } else { "orf-ii" },
        id: "orf",
        bytes,
        ext: "orf",
    }
}

/// Panasonic RW2. `raw_format` 4 has a size estimate; others do not.
pub fn rw2(raw_format: u16) -> Sample {
    let mut rng = Rng(17);
    let jpeg = fake_jpeg(1920, 1080, 25_000, &mut rng);
    let (sw, sh) = (1000u32, 700u32);
    let exact = (sw as usize * sh as usize * 8).div_ceil(7);
    let raw = rng.bytes(if raw_format == 4 {
        exact.div_ceil(0x4000) * 0x4000
    } else {
        50_000
    });
    let (bytes, _) = build(false, |o| {
        let mut h = b"IIU\0".to_vec();
        h.extend_from_slice(&0x18u32.to_le_bytes());
        h.resize(0x18, 0);
        let ifd0 = Item::ifd(
            vec![
                e(0x0001, V::Undef(vec![2, 0, 0, 0])),
                short(0x0002, sw as u16),
                short(0x0003, sh as u16),
                short(0x002D, raw_format),
                ptr(0x002E, 7, jpeg.len(), o[1]),
                ascii(0x010F, "Panasonic"),
                ascii(0x0110, "DC-GH5"),
                short(0x0112, 1),
                long(0x0118, o[2] as u32),
            ],
            0,
        );
        (
            h,
            vec![ifd0, Item::Blob(jpeg.clone()), Item::Blob(raw.clone())],
        )
    });
    Sample {
        name: if raw_format == 4 { "rw2" } else { "rw2-other" },
        id: "rw2",
        bytes,
        ext: "rw2",
    }
}

/// Make-only RAW (SRW, ERF): served by the generic TIFF format.
pub fn generic_raw(make: &'static str, compression: u16, ext: &'static str) -> Sample {
    let mut rng = Rng(18);
    let raw = rng.bytes(90_000);
    let (bytes, _) = build(false, |o| {
        (
            tiff_header(false, 8),
            vec![
                Item::ifd(
                    vec![
                        short(0x0100, 5472),
                        short(0x0101, 3648),
                        short(0x0102, 12),
                        short(0x0103, compression),
                        ascii(0x010F, make),
                        ascii(0x0110, "Model X"),
                        long(0x0111, o[1] as u32),
                        long(0x0117, raw.len() as u32),
                    ],
                    0,
                ),
                Item::Blob(raw.clone()),
            ],
        )
    });
    Sample {
        name: ext,
        id: "tiff",
        bytes,
        ext,
    }
}

pub fn tiled_tiff() -> Sample {
    let mut rng = Rng(19);
    let tiles: Vec<Vec<u8>> = (0..4).map(|_| rng.bytes(256)).collect();
    let (bytes, _) = build(true, |o| {
        let ifd = Item::ifd(
            vec![
                short(0x0100, 32),
                short(0x0101, 32),
                short(0x0102, 8),
                short(0x0103, 1),
                short(0x0106, 1),
                short(0x0115, 1),
                short(0x0142, 16),
                short(0x0143, 16),
                e(0x0144, V::Long((1..5).map(|i| o[i] as u32).collect())),
                e(0x0145, V::Long(vec![256; 4])),
            ],
            0,
        );
        let mut items = vec![ifd];
        items.extend(tiles.iter().map(|t| Item::Blob(t.clone())));
        (tiff_header(true, 8), items)
    });
    Sample {
        name: "tiled",
        id: "tiff",
        bytes,
        ext: "tif",
    }
}

/// Multi-IFD TIFF with a SubIFD holding a JPEG preview, an IFD1 thumbnail and
/// a trailing blob only the SubIFD points at.
pub fn nested_tiff() -> Sample {
    let mut rng = Rng(20);
    let main = rng.bytes(2_000);
    let preview = fake_jpeg(640, 480, 12_000, &mut rng);
    let thumb = fake_jpeg(120, 80, 2_500, &mut rng);
    let tail = rng.bytes(5_000);
    let (bytes, _) = build(false, |o| {
        let ifd0 = Item::ifd(
            vec![
                short(0x0100, 40),
                short(0x0101, 50),
                short(0x0102, 8),
                short(0x0103, 1),
                short(0x0106, 1),
                ascii(0x010F, "TestCam"),
                ascii(0x0110, "Model 1"),
                long(0x0111, o[3] as u32),
                short(0x0112, 8),
                long(0x0117, main.len() as u32),
                e(0x014A, V::Long(vec![o[2] as u32])),
            ],
            o[1] as u32,
        );
        let ifd1 = Item::ifd(
            vec![
                short(0x0103, 6),
                long(0x0201, o[5] as u32),
                long(0x0202, thumb.len() as u32),
            ],
            0,
        );
        let sub = Item::ifd(
            vec![
                long(0x00FE, 1),
                short(0x0103, 6),
                long(0x0111, o[6] as u32),
                long(0x0117, tail.len() as u32),
                long(0x0201, o[4] as u32),
                long(0x0202, preview.len() as u32),
            ],
            0,
        );
        (
            tiff_header(false, 8),
            vec![
                ifd0,
                ifd1,
                sub,
                Item::Blob(main.clone()),
                Item::Blob(preview.clone()),
                Item::Blob(thumb.clone()),
                Item::Blob(tail.clone()),
            ],
        )
    });
    Sample {
        name: "nested",
        id: "tiff",
        bytes,
        ext: "tif",
    }
}

/// A BigTIFF with one strip: `II+\0`, 8-byte offsets, 20-byte entries.
pub fn bigtiff() -> Sample {
    let strip = 640usize;
    let n = 8u64;
    let ifd_at = 16u64;
    let ifd_len = 8 + 20 * n + 8;
    let strip_at = ifd_at + ifd_len + 8; // 8 bytes of value area for Make
    let mut b = b"II+\0".to_vec();
    b.extend_from_slice(&8u16.to_le_bytes());
    b.extend_from_slice(&0u16.to_le_bytes());
    b.extend_from_slice(&ifd_at.to_le_bytes());
    b.extend_from_slice(&n.to_le_bytes());
    let mut entry = |tag: u16, typ: u16, count: u64, val: [u8; 8]| {
        b.extend_from_slice(&tag.to_le_bytes());
        b.extend_from_slice(&typ.to_le_bytes());
        b.extend_from_slice(&count.to_le_bytes());
        b.extend_from_slice(&val);
    };
    entry(0x0100, 4, 1, 32u64.to_le_bytes());
    entry(0x0101, 4, 1, 20u64.to_le_bytes());
    entry(0x0102, 3, 1, 8u64.to_le_bytes());
    entry(0x0103, 3, 1, 1u64.to_le_bytes());
    entry(0x010F, 2, 7, *b"Cam01\0\0\0");
    entry(0x0111, 16, 1, strip_at.to_le_bytes());
    entry(0x0116, 4, 1, 20u64.to_le_bytes());
    entry(0x0117, 16, 1, (strip as u64).to_le_bytes());
    b.extend_from_slice(&0u64.to_le_bytes());
    b.extend_from_slice(&[0u8; 8]);
    assert_eq!(b.len() as u64, strip_at);
    b.extend(Rng(21).bytes(strip));
    Sample {
        name: "bigtiff",
        id: "tiff",
        bytes: b,
        ext: "tif",
    }
}

// ---------------------------------------------------------------------------
// RAF
// ---------------------------------------------------------------------------

pub fn raf(jpeg_first: bool) -> Sample {
    let mut rng = Rng(22);
    let exif = exif_tiff(false, "FUJIFILM", "X-T3", 1, DT);
    let jpeg = fake_jpeg_with_exif(96, 64, 30_000, &exif, &mut rng);
    raf_with_jpeg(&jpeg, jpeg_first)
}

/// RAF around a given embedded JPEG.
pub fn raf_with_jpeg(jpeg: &[u8], jpeg_first: bool) -> Sample {
    let mut rng = Rng(24);
    // CFA header: count 2; 0x0100 RawImageFullSize (h, w), 0x0130 FujiLayout.
    let mut cfah = be32(2).to_vec();
    cfah.extend_from_slice(&be16(0x0100));
    cfah.extend_from_slice(&be16(4));
    cfah.extend_from_slice(&be16(4000));
    cfah.extend_from_slice(&be16(6000));
    cfah.extend_from_slice(&be16(0x0130));
    cfah.extend_from_slice(&be16(1));
    cfah.push(7);
    cfah.resize(32, 0);
    let mut cfa = b"II*\0\x08\0\0\0".to_vec();
    cfa.extend(rng.bytes(90_000));
    let (j_off, h_off, c_off) = if jpeg_first {
        let j = 160;
        let h = j + jpeg.len();
        (j, h, h + cfah.len())
    } else {
        let h = 160;
        let c = h + cfah.len();
        (c + cfa.len(), h, c)
    };
    let mut f = b"FUJIFILMCCD-RAW 0201FF129007".to_vec();
    let mut model = b"X-T3".to_vec();
    model.resize(32, 0);
    f.extend_from_slice(&model);
    f.extend_from_slice(b"0100");
    f.resize(84, 0);
    for v in [j_off, jpeg.len(), h_off, cfah.len(), c_off, cfa.len()] {
        f.extend_from_slice(&be32(v as u32));
    }
    f.extend_from_slice(&be32(2)); // RAFCompression: lossless
    f.resize(160, 0);
    let mut place = |off: usize, data: &[u8]| {
        if f.len() < off + data.len() {
            f.resize(off + data.len(), 0);
        }
        f[off..off + data.len()].copy_from_slice(data);
    };
    place(j_off, jpeg);
    place(h_off, &cfah);
    place(c_off, &cfa);
    Sample {
        name: if jpeg_first { "raf" } else { "raf-jpeg-last" },
        id: "raf",
        bytes: f,
        ext: "raf",
    }
}

// ---------------------------------------------------------------------------
// PSD
// ---------------------------------------------------------------------------

pub const PSD_W: u32 = 16;
pub const PSD_H: u32 = 12;
pub const PSD_CH: u16 = 3;

pub fn psd(compression: u16, resources: bool) -> Sample {
    let mut rng = Rng(23);
    let mut f = b"8BPS".to_vec();
    f.extend_from_slice(&be16(1));
    f.extend_from_slice(&[0; 6]);
    f.extend_from_slice(&be16(PSD_CH));
    f.extend_from_slice(&be32(PSD_H));
    f.extend_from_slice(&be32(PSD_W));
    f.extend_from_slice(&be16(8));
    f.extend_from_slice(&be16(3));
    f.extend_from_slice(&be32(0)); // colour mode data
    let mut res = Vec::new();
    if resources {
        let jpeg = fake_jpeg(80, 60, 1_200, &mut rng);
        let mut t = Vec::new();
        for v in [1u32, 80, 60, 240, 14_400, jpeg.len() as u32] {
            t.extend_from_slice(&be32(v));
        }
        t.extend_from_slice(&be16(24));
        t.extend_from_slice(&be16(1));
        t.extend_from_slice(&jpeg);
        let mut block = |id: u16, data: &[u8]| {
            res.extend_from_slice(b"8BIM");
            res.extend_from_slice(&be16(id));
            res.extend_from_slice(&[0, 0]); // empty Pascal name, padded
            res.extend_from_slice(&be32(data.len() as u32));
            res.extend_from_slice(data);
            if data.len() % 2 == 1 {
                res.push(0);
            }
        };
        block(0x0404, &[1, 2, 3]);
        block(0x040C, &t);
        block(0x0422, &exif_tiff(true, "Adobe", "Photoshop Test", 3, DT));
    }
    f.extend_from_slice(&be32(res.len() as u32));
    f.extend_from_slice(&res);
    let layers = rng.bytes(300);
    f.extend_from_slice(&be32(layers.len() as u32));
    f.extend_from_slice(&layers);
    f.extend_from_slice(&be16(compression));
    let rows = PSD_CH as usize * PSD_H as usize;
    let row_bytes = PSD_W as usize;
    match compression {
        0 => f.extend(rng.bytes(rows * row_bytes)),
        1 => {
            let sizes: Vec<u16> = (0..rows).map(|i| 4 + (i % 14) as u16).collect();
            for s in &sizes {
                f.extend_from_slice(&be16(*s));
            }
            for s in &sizes {
                f.extend(rng.bytes(*s as usize));
            }
        }
        _ => {
            use flate2::write::ZlibEncoder;
            use std::io::Write;
            let mut z = ZlibEncoder::new(Vec::new(), flate2::Compression::default());
            z.write_all(&rng.bytes(rows * row_bytes)).unwrap();
            f.extend(z.finish().unwrap());
        }
    }
    Sample {
        name: match compression {
            0 => "psd-raw",
            1 => "psd-rle",
            _ => "psd-zip",
        },
        id: "psd",
        bytes: f,
        ext: "psd",
    }
}

/// Every hand-built sample, for table-driven tests.
pub fn all() -> Vec<Sample> {
    vec![
        cr2(),
        nef(false),
        nef(true),
        arw(false),
        arw(true),
        dng(),
        pef(false),
        pef(true),
        orf(false),
        orf(true),
        rw2(4),
        rw2(5),
        generic_raw("SAMSUNG", 32770, "srw"),
        generic_raw("SEIKO EPSON CORP.", 1, "erf"),
        tiled_tiff(),
        nested_tiff(),
        bigtiff(),
        raf(true),
        raf(false),
        psd(0, false),
        psd(1, true),
        psd(2, true),
    ]
}
