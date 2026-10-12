//! Hostile input: random bytes, truncation, mutation of valid tables and
//! absurd header fields must never panic, loop forever or return entries that
//! escape the device.

mod common;

use common::*;
use diskrescue_core::partition::{PartitionScheme, PartitionTable};
use diskrescue_core::SharedSource;
use diskrescue_partition::{
    probe_filesystem, read_partition_table, read_partition_table_with, scan_lost_partitions,
    volumes, ScanOptions,
};

/// Structural invariants every returned table must satisfy.
fn check_table(t: &PartitionTable, dev: u64) {
    let mut prev = 0u64;
    for (i, e) in t.entries.iter().enumerate() {
        assert_eq!(e.index as usize, i + 1, "indices are 1-based and dense");
        assert!(e.size > 0, "empty entry {e:?}");
        assert!(e.start >= prev, "entries sorted by start");
        prev = e.start;
        let end = e.start.checked_add(e.size).expect("entry end overflows");
        assert!(end <= dev, "entry {e:?} escapes the device ({dev})");
    }
    if t.scheme == PartitionScheme::Unknown {
        assert!(t.entries.is_empty());
    }
}

fn exercise(data: Vec<u8>) {
    let dev = data.len() as u64;
    let src = mem_source(data);
    let t = read_partition_table(&src).unwrap();
    check_table(&t, dev);
    let t = read_partition_table_with(&src, 4096).unwrap();
    check_table(&t, dev);
    let (t, vols) = volumes(&src, 512).unwrap();
    check_table(&t, dev);
    assert_eq!(t.entries.len(), vols.len());
    for off in [0u64, 512, 4096, dev / 2] {
        let _ = probe_filesystem(&src, off);
    }
}

#[test]
fn random_buffers() {
    let mut rng = Rng(0x1234_5678_9ABC_DEF0);
    let sizes = [
        0usize,
        1,
        7,
        100,
        511,
        512,
        513,
        1024,
        4096,
        4608,
        9000,
        70_000,
        300_000,
        1 << 20,
    ];
    for round in 0..120 {
        let len = sizes[round % sizes.len()];
        exercise(rng.bytes(len));
    }
}

#[test]
fn random_buffers_with_planted_signatures() {
    let mut rng = Rng(42);
    for round in 0..150 {
        let len = 64 * 1024 + (rng.below(200) as usize) * 4096;
        let mut d = rng.bytes(len);
        match round % 5 {
            0 => {
                d[510] = 0x55;
                d[511] = 0xAA;
            }
            1 => {
                d[512..520].copy_from_slice(b"EFI PART");
                let n = d.len();
                d[n - 512..n - 504].copy_from_slice(b"EFI PART");
            }
            2 => {
                d[4096..4104].copy_from_slice(b"EFI PART");
                d[510] = 0x55;
                d[511] = 0xAA;
                d[0x1BE + 4] = 0x05;
            }
            3 => {
                // Plausible MBR entries with random LBAs.
                d[510] = 0x55;
                d[511] = 0xAA;
                for i in 0..4 {
                    let o = 0x1BE + 16 * i;
                    d[o] = if i % 2 == 0 { 0x80 } else { 0 };
                    d[o + 4] = [0x05u8, 0x0F, 0x85, 0x83][rng.below(4) as usize];
                }
            }
            _ => {
                d[3..11].copy_from_slice(b"NTFS    ");
                d[510] = 0x55;
                d[511] = 0xAA;
            }
        }
        exercise(d);
    }
}

fn gpt_disk_bytes() -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let size = 4 * MIB;
    let disk = Disk::create(dir.path(), "g.img", size);
    let b = GptBuild::new(
        512,
        size / 512,
        vec![
            gpt_part(GUID_ESP, 1, 2048, 4095, "a"),
            gpt_part(GUID_BASIC, 2, 4096, 6000, "b"),
        ],
    );
    b.write(&disk);
    disk.mem()
}

#[test]
fn mutated_gpt_disks() {
    let base = gpt_disk_bytes();
    let n = base.len();
    let mut rng = Rng(7);
    for _ in 0..400 {
        let mut d = base.clone();
        let flips = 1 + rng.below(8) as usize;
        for _ in 0..flips {
            // Concentrate on the primary and backup metadata.
            let region = rng.below(3);
            let off = match region {
                0 => rng.below(34 * 512),
                1 => (n as u64 - 34 * 512) + rng.below(34 * 512),
                _ => rng.below(n as u64),
            } as usize;
            d[off] = rng.next() as u8;
        }
        exercise(d);
    }
}

#[test]
fn truncated_gpt_disks() {
    let base = gpt_disk_bytes();
    let mut lens: Vec<usize> = (0..70).map(|i| i * 512).collect();
    lens.extend([
        1,
        511,
        513,
        base.len() - 1,
        base.len() - 512,
        base.len() - 513,
        base.len() - 17 * 512,
        base.len() - 34 * 512,
        base.len() - 33 * 512,
    ]);
    for len in lens {
        exercise(base[..len.min(base.len())].to_vec());
    }
}

#[test]
fn absurd_gpt_header_fields_with_valid_crc() {
    let base = gpt_disk_bytes();
    let n = base.len();
    let b = GptBuild::new(
        512,
        (n / 512) as u64,
        vec![gpt_part(GUID_BASIC, 1, 2048, 4095, "a")],
    );
    let l = b.layout();
    let arr = b.array();
    let crc = crc32fast::hash(&arr);

    let u32s = [
        0u32,
        1,
        91,
        92,
        127,
        128,
        129,
        255,
        256,
        4096,
        4097,
        0xFFFF_FFFF,
    ];
    let u64s = [
        0u64,
        1,
        2,
        3,
        63,
        4095,
        8191,
        8192,
        u64::MAX,
        u64::MAX - 1,
        1 << 63,
    ];
    // (field offset, width) of header fields worth attacking.
    let fields: [(usize, usize); 9] = [
        (12, 4),
        (24, 8),
        (32, 8),
        (40, 8),
        (48, 8),
        (72, 8),
        (80, 4),
        (84, 4),
        (8, 4),
    ];
    for (off, width) in fields {
        let values: Vec<u64> = if width == 4 {
            u32s.iter().map(|&v| u64::from(v)).collect()
        } else {
            u64s.to_vec()
        };
        for v in values {
            for target in [1u64, l.backup_header_lba] {
                let mut d = base.clone();
                let mut h = b.header(
                    target,
                    if target == 1 { l.backup_header_lba } else { 1 },
                    l.primary_array_lba,
                    crc,
                );
                h[off..off + width].copy_from_slice(&v.to_le_bytes()[..width]);
                h[16..20].fill(0);
                let hdr_size = u32::from_le_bytes(h[12..16].try_into().unwrap()) as usize;
                let c = crc32fast::hash(&h[..hdr_size.min(h.len())]);
                h[16..20].copy_from_slice(&c.to_le_bytes());
                let at = target as usize * 512;
                d[at..at + 512].copy_from_slice(&h);
                exercise(d);
            }
        }
    }
}

#[test]
fn absurd_mbr_and_ebr_fields() {
    let mut rng = Rng(5);
    let values = [
        0u32,
        1,
        2,
        63,
        2048,
        0x7FFF_FFFF,
        0x8000_0000,
        0xFFFF_FFFE,
        0xFFFF_FFFF,
    ];
    for &lba in &values {
        for &n in &values {
            for ptype in [0x05u8, 0x0F, 0x85, 0x83, 0xEE, 0x07] {
                let mut d = vec![0u8; 256 * 1024];
                d[..512].copy_from_slice(&mbr_sector(&[
                    MbrPart {
                        status: 0x80,
                        ptype,
                        lba,
                        sectors: n,
                    },
                    MbrPart {
                        status: 0,
                        ptype: 0x0F,
                        lba: 1,
                        sectors: 100,
                    },
                ]));
                // Every sector of the first 100 is an EBR pointing at absurd places.
                for s in 1..100usize {
                    let e = mbr_sector(&[
                        MbrPart {
                            status: 0,
                            ptype: 0x83,
                            lba: values[rng.below(9) as usize],
                            sectors: values[rng.below(9) as usize],
                        },
                        MbrPart {
                            status: 0,
                            ptype: 0x05,
                            lba: values[rng.below(9) as usize],
                            sectors: values[rng.below(9) as usize],
                        },
                    ]);
                    d[s * 512..(s + 1) * 512].copy_from_slice(&e);
                }
                exercise(d);
            }
        }
    }
}

#[test]
fn scan_on_garbage_never_panics() {
    let mut rng = Rng(11);
    for round in 0..30 {
        let len = 256 * 1024 + (rng.below(64) as usize) * 4096;
        let mut d = rng.bytes(len);
        // Sprinkle valid-looking signatures everywhere.
        for _ in 0..40 {
            let at = (rng.below((len - 4096) as u64) as usize) & !511;
            match rng.below(5) {
                0 => {
                    d[at + 3..at + 11].copy_from_slice(b"NTFS    ");
                    d[at + 510] = 0x55;
                    d[at + 511] = 0xAA;
                }
                1 => {
                    d[at + 3..at + 11].copy_from_slice(b"EXFAT   ");
                    d[at + 11..at + 64].fill(0);
                    d[at + 510] = 0x55;
                    d[at + 511] = 0xAA;
                }
                2 => {
                    d[at + 1024 + 0x38..at + 1024 + 0x3A].copy_from_slice(&0xEF53u16.to_le_bytes())
                }
                3 => d[at..at + 4].copy_from_slice(b"XFSB"),
                _ => {
                    d[at] = 0xEB;
                    d[at + 510] = 0x55;
                    d[at + 511] = 0xAA;
                }
            }
        }
        let src: SharedSource = mem_source(d);
        let opts = ScanOptions {
            step: if round % 2 == 0 { 512 } else { 4096 },
            ..ScanOptions::default()
        };
        let found = scan_lost_partitions(&src, &opts, &mut |_, _| {}).unwrap();
        for e in &found {
            assert!(e.size > 0 && e.start + e.size <= src.size());
        }
    }
}

#[test]
fn public_helpers_accept_arbitrary_input() {
    use diskrescue_partition::{gpt, mbr, read_partition_table_detailed};
    let mut rng = Rng(2024);
    // Raw sector parsers on every length around the boundaries.
    for len in [0usize, 1, 15, 16, 100, 511, 512, 513, 1024, 4096] {
        let b = rng.bytes(len);
        let m = mbr::parse_mbr(&b);
        assert_eq!(m.is_some(), len >= 512);
        if let Some(m) = m {
            let _ = m.used_count();
            let _ = m.has_protective_entry();
        }
        let _ = mbr::MbrEntry::parse(&b, 0);
    }
    for t in 0..=255u8 {
        assert!(!mbr::type_description(t).is_empty());
        assert!(mbr::type_id(t).starts_with("0x"));
    }
    for s in [
        "",
        "x",
        "not a guid",
        "0FC63DAF-8483-4772-8E79-3D69D8477DE4",
        "ÿÿ",
    ] {
        let _ = gpt::type_description(s);
    }
    // GPT reader with every sector size, including nonsense ones.
    let base = gpt_disk_bytes();
    let src = mem_source(base.clone());
    for ss in [0u32, 1, 511, 512, 1024, 2048, 4096, 8192, u32::MAX] {
        let r = gpt::read_gpt(src.as_ref(), ss).unwrap();
        assert_eq!(r.is_some(), ss == 512, "sector size {ss}");
    }
    for hint in [
        None,
        Some(0u32),
        Some(7),
        Some(512),
        Some(4096),
        Some(u32::MAX),
    ] {
        let d = read_partition_table_detailed(&src, hint).unwrap();
        check_table(&d.table, base.len() as u64);
        assert!(matches!(d.sector_size, 512 | 1024 | 2048 | 4096));
    }
    // Random sources through the detailed entry point.
    for len in [0usize, 100, 512, 5000, 70_000] {
        let s = mem_source(rng.bytes(len));
        let d = read_partition_table_detailed(&s, Some(4096)).unwrap();
        check_table(&d.table, len as u64);
    }
}
