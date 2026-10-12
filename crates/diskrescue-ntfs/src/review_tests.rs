//! Adversarial tests on synthetic volumes: absurd sizes and run counts,
//! negative run deltas, run lists that overrun their piece, torn deleted
//! records, directory cycles in index mode and corrupt parent references.

use std::io::Write;
use std::time::{Duration, Instant};

use diskrescue_core::fs::{collect_entries, EntryConfidence, FileSystem};

use crate::attr::{namespace, types, ATTR_SPARSE};
use crate::record::testutil::{build_record, build_record_sized, non_resident, resident};
use crate::record::{FLAG_DIRECTORY, FLAG_IN_USE};
use crate::runs::encode_runs;
use crate::synth_tests::{
    fname, get, open_bytes, pattern, read_all, si, Img, CLUSTER, MFT_LCN, RS,
};
use crate::walk::flags;
use crate::{open, NtfsOptions};

/// A sink that fails once more than `cap` bytes were written, so a read
/// that would produce absurd amounts of data ends quickly.
struct Capped {
    n: u64,
    cap: u64,
}

impl Write for Capped {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.n + buf.len() as u64 > self.cap {
            return Err(std::io::Error::other("cap reached"));
        }
        self.n += buf.len() as u64;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Store `data` at a fixed cluster.
fn store_at(img: &mut Img, lcn: u64, data: &[u8]) {
    let off = (lcn * CLUSTER) as usize;
    img.bytes[off..off + data.len()].copy_from_slice(data);
    img.mark(lcn, (data.len() as u64).div_ceil(CLUSTER));
}

#[test]
fn absurd_sizes_on_plain_attributes_are_clamped() {
    let mut img = Img::new(256, 64);
    let body = pattern(1, 8192);
    let l = img.store(&body, true);
    // data size > allocated size.
    let a = non_resident(
        types::DATA,
        "",
        0,
        2,
        0,
        1,
        (8192, 1 << 60, 1 << 60),
        0,
        &encode_runs(&[(Some(l), 2)]),
    );
    img.file(16, 1, FLAG_IN_USE, 5, "big_data_size.bin", vec![a]);
    // data size == allocated size, both far beyond the volume, and the
    // header's highest VCN agrees with them: only the runs are sane.
    let huge = 1u64 << 60;
    let a = non_resident(
        types::DATA,
        "",
        0,
        2,
        0,
        huge / CLUSTER - 1,
        (huge, huge, huge),
        0,
        &encode_runs(&[(Some(l), 2)]),
    );
    img.file(17, 1, FLAG_IN_USE, 5, "big_everything.bin", vec![a.clone()]);
    img.file(18, 2, 0, 5, "deleted_big.bin", vec![a]);
    let fs = open_bytes(img.finish());
    let all = collect_entries(&fs).unwrap();
    for name in ["big_data_size.bin", "big_everything.bin", "deleted_big.bin"] {
        let e = get(&all, name);
        assert!(
            e.size <= 256 * CLUSTER,
            "{name}: size {} is larger than the volume",
            e.size
        );
        let mut sink = Capped {
            n: 0,
            cap: 64 << 20,
        };
        let start = Instant::now();
        let rep = fs
            .read_file(e, &mut sink)
            .unwrap_or_else(|err| panic!("{name}: {err}"));
        assert!(start.elapsed() < Duration::from_secs(10));
        assert_eq!(rep.bytes_written, e.size, "{name}");
        assert!(sink.n <= 8192, "{name}: wrote {}", sink.n);
        assert!(
            rep.notes.iter().any(|n| n.contains("size")),
            "{name}: {:?}",
            rep.notes
        );
        let mut v = Vec::new();
        fs.read_file(e, &mut v).unwrap();
        assert_eq!(v, body[..v.len()].to_vec());
    }
}

#[test]
fn huge_sparse_run_counts_stay_cheap() {
    let mut img = Img::new(256, 64);
    let body = pattern(2, 4096);
    let l = img.store(&body, true);
    let holes = 1u64 << 40;
    let size = (holes + 1) * CLUSTER;
    let a = non_resident(
        types::DATA,
        "",
        ATTR_SPARSE,
        2,
        0,
        holes,
        (size, size, size),
        0,
        &encode_runs(&[(None, holes), (Some(l), 1)]),
    );
    img.file(16, 1, FLAG_IN_USE, 5, "huge_sparse.bin", vec![a]);
    // The same run list with a sparse run whose count overflows the VCN
    // space.
    let mut mp = vec![0x08u8];
    mp.extend_from_slice(&u64::MAX.to_le_bytes());
    mp.extend_from_slice(&[0x11, 0x01, l as u8, 0x00]);
    let a = non_resident(
        types::DATA,
        "",
        ATTR_SPARSE,
        2,
        0,
        5,
        (6 * CLUSTER, 6 * CLUSTER, 6 * CLUSTER),
        0,
        &mp,
    );
    img.file(17, 1, FLAG_IN_USE, 5, "overflow.bin", vec![a]);
    let start = Instant::now();
    let fs = open_bytes(img.finish());
    let all = collect_entries(&fs).unwrap();
    assert!(start.elapsed() < Duration::from_secs(10));
    let e = get(&all, "huge_sparse.bin");
    assert_eq!(e.size, size);
    assert_eq!(e.extent_bytes(), CLUSTER);
    assert_eq!(e.extents[0].start, l * CLUSTER);
    // Reading streams zeros; a sink that gives up ends the read promptly.
    let mut sink = Capped {
        n: 0,
        cap: 16 << 20,
    };
    let start = Instant::now();
    assert!(fs.read_file(e, &mut sink).is_err());
    assert!(start.elapsed() < Duration::from_secs(10));
    let e = get(&all, "overflow.bin");
    let (got, rep) = read_all(&fs, e);
    assert_eq!(got.len() as u64, 6 * CLUSTER);
    assert!(got.iter().all(|&b| b == 0));
    assert!(!rep.unreadable.is_empty(), "lost mapping is not a hole");
    assert!(e.flags.iter().any(|f| f == flags::INCOMPLETE));
}

#[test]
fn negative_run_deltas_read_the_right_clusters() {
    let mut img = Img::new(512, 64);
    let parts: Vec<(u64, Vec<u8>)> = vec![
        (300, pattern(10, 4096)),
        (120, pattern(11, 8192)),
        (250, pattern(12, 4096)),
        (119, pattern(13, 4096)),
        (400, pattern(14, 4096)),
    ];
    let mut runs = Vec::new();
    let mut expect = Vec::new();
    for (lcn, d) in &parts {
        store_at(&mut img, *lcn, d);
        runs.push((Some(*lcn), d.len() as u64 / CLUSTER));
        expect.extend_from_slice(d);
    }
    let clusters = expect.len() as u64 / CLUSTER;
    let size = expect.len() as u64 - 100;
    expect.truncate(size as usize);
    let mp = encode_runs(&runs);
    // Check the encoding really has negative deltas.
    let d = crate::runs::decode_runs(&mp, 0, Some(512));
    assert!(d.error.is_none());
    assert_eq!(d.runs.len(), 5);
    let a = non_resident(
        types::DATA,
        "",
        0,
        2,
        0,
        clusters - 1,
        (clusters * CLUSTER, size, size),
        0,
        &mp,
    );
    img.file(16, 1, FLAG_IN_USE, 5, "zigzag.bin", vec![a]);
    // A run list that steps below LCN 0 after its first run.
    let mut bad = vec![0x11, 0x01, 0x64]; // 1 cluster at LCN 100
    bad.extend_from_slice(&[0x21, 0x01, 0x00, 0xFF]); // delta -256
    bad.push(0);
    store_at(&mut img, 100, &pattern(15, 4096));
    let a = non_resident(
        types::DATA,
        "",
        0,
        2,
        0,
        1,
        (2 * CLUSTER, 2 * CLUSTER, 2 * CLUSTER),
        0,
        &bad,
    );
    img.file(17, 1, FLAG_IN_USE, 5, "below_zero.bin", vec![a]);
    let fs = open_bytes(img.finish());
    let all = collect_entries(&fs).unwrap();
    let e = get(&all, "zigzag.bin");
    let (got, rep) = read_all(&fs, e);
    assert!(got == expect);
    assert!(rep.is_complete() && rep.holes.is_empty());
    let e = get(&all, "below_zero.bin");
    let (got, rep) = read_all(&fs, e);
    assert_eq!(got[..4096], pattern(15, 4096)[..]);
    assert!(got[4096..].iter().all(|&b| b == 0));
    assert_eq!(rep.unreadable.total_len(), 4096);
    assert!(e.flags.iter().any(|f| f == flags::INCOMPLETE));
}

#[test]
fn runs_past_the_piece_end_do_not_shadow_the_next_piece() {
    let mut img = Img::new(256, 64);
    let p0 = pattern(20, 8192);
    let p1 = pattern(21, 8192);
    let junk = pattern(22, 8192);
    let l0 = img.store(&p0, true);
    let lj = img.store(&junk, true);
    let l1 = img.store(&p1, true);
    let size = 16384u64;
    // Piece 0 says it covers VCN 0..=1 but its (torn) run list goes on.
    let piece0 = non_resident(
        types::DATA,
        "",
        0,
        2,
        0,
        1,
        (4 * CLUSTER, size, size),
        0,
        &encode_runs(&[(Some(l0), 2), (Some(lj), 2)]),
    );
    let piece1 = non_resident(
        types::DATA,
        "",
        0,
        0,
        2,
        3,
        (0, 0, 0),
        0,
        &encode_runs(&[(Some(l1), 2)]),
    );
    let mut list = Vec::new();
    for (rec, id, low) in [(16u64, 2u16, 0u64), (17, 0, 2)] {
        let mut e = vec![0u8; 0x20];
        e[0..4].copy_from_slice(&types::DATA.to_le_bytes());
        e[4..6].copy_from_slice(&0x20u16.to_le_bytes());
        e[7] = 0x1A;
        e[8..16].copy_from_slice(&low.to_le_bytes());
        e[16..24].copy_from_slice(&(rec | (1 << 48)).to_le_bytes());
        e[24..26].copy_from_slice(&id.to_le_bytes());
        list.extend(e);
    }
    img.put(
        16,
        build_record(
            16,
            1,
            FLAG_IN_USE,
            &[
                resident(types::STANDARD_INFORMATION, "", &si(), 0),
                resident(types::ATTRIBUTE_LIST, "", &list, 5),
                resident(
                    types::FILE_NAME,
                    "",
                    &fname(5, 5, "pieces.bin", namespace::POSIX, false),
                    1,
                ),
                piece0,
            ],
        ),
    );
    img.put(
        17,
        build_record_sized(1024, 17, 1, FLAG_IN_USE, 16 | (1 << 48), &[piece1]),
    );
    let fs = open_bytes(img.finish());
    let all = collect_entries(&fs).unwrap();
    let e = get(&all, "pieces.bin");
    let (got, _) = read_all(&fs, e);
    let mut expect = p0.clone();
    expect.extend_from_slice(&p1);
    assert!(got == expect, "the runs beyond piece 0's end were used");
}

#[test]
fn torn_deleted_record_is_not_reported_intact() {
    let mut img = Img::new(256, 64);
    let body = pattern(30, 9000);
    let l = img.store(&body, false);
    let attrs = vec![
        resident(types::STANDARD_INFORMATION, "", &si(), 0),
        resident(
            types::FILE_NAME,
            "",
            &fname(5, 5, "torn.jpg", namespace::POSIX, false),
            1,
        ),
        crate::synth_tests::nr_data("", l, 3, 9000),
    ];
    let mut raw = build_record(16, 2, 0, &attrs);
    raw[1022] ^= 0x55;
    img.put(16, raw);
    let fs = open_bytes(img.finish());
    let all = collect_entries(&fs).unwrap();
    let e = get(&all, "torn.jpg");
    assert!(e.deleted);
    assert!(e.flags.iter().any(|f| f == flags::TORN));
    assert_eq!(e.confidence, EntryConfidence::DeletedGuessed);
    let (got, rep) = read_all(&fs, e);
    assert_eq!(got, body);
    assert!(rep.notes.iter().any(|n| n.contains("torn")));
}

/// An $INDEX_ROOT listing `children` (reference, $FILE_NAME key).
fn index_root_with(children: &[(u64, Vec<u8>)]) -> Vec<u8> {
    let mut entries = Vec::new();
    for (r, key) in children {
        let len = (0x10 + key.len() + 7) & !7;
        let mut e = vec![0u8; len];
        e[0..8].copy_from_slice(&r.to_le_bytes());
        e[8..10].copy_from_slice(&(len as u16).to_le_bytes());
        e[10..12].copy_from_slice(&(key.len() as u16).to_le_bytes());
        e[0x10..0x10 + key.len()].copy_from_slice(key);
        entries.extend(e);
    }
    let mut end = vec![0u8; 0x10];
    end[8..10].copy_from_slice(&0x10u16.to_le_bytes());
    end[12..14].copy_from_slice(&2u16.to_le_bytes());
    entries.extend(end);
    let mut v = vec![0u8; 0x20];
    v[0..4].copy_from_slice(&0x30u32.to_le_bytes());
    v[4..8].copy_from_slice(&1u32.to_le_bytes());
    v[8..12].copy_from_slice(&4096u32.to_le_bytes());
    v[12] = 1;
    v[0x10..0x14].copy_from_slice(&0x10u32.to_le_bytes());
    let total = 0x10 + entries.len() as u32;
    v[0x14..0x18].copy_from_slice(&total.to_le_bytes());
    v[0x18..0x1C].copy_from_slice(&total.to_le_bytes());
    v.extend(entries);
    v
}

#[test]
fn index_cycles_and_corrupt_parents_terminate() {
    let mut img = Img::new(256, 64);
    let key = |parent: u64, name: &str| fname(parent, 1, name, namespace::POSIX, true);
    // Root lists A; A lists B; B lists A and itself and the root.
    let a_key = fname(5, 5, "A", namespace::POSIX, true);
    let root_attrs = vec![
        resident(types::STANDARD_INFORMATION, "", &si(), 0),
        resident(
            types::FILE_NAME,
            "",
            &fname(5, 5, ".", namespace::WIN32_AND_DOS, true),
            1,
        ),
        resident(
            types::INDEX_ROOT,
            "$I30",
            &index_root_with(&[(20 | (1 << 48), a_key.clone())]),
            2,
        ),
    ];
    img.put(
        5,
        build_record(5, 5, FLAG_IN_USE | FLAG_DIRECTORY, &root_attrs),
    );
    let dir = |img: &mut Img, n: u64, parent: u64, name: &str, kids: Vec<(u64, Vec<u8>)>| {
        let attrs = vec![
            resident(types::STANDARD_INFORMATION, "", &si(), 0),
            resident(
                types::FILE_NAME,
                "",
                &fname(
                    parent,
                    if parent == 5 { 5 } else { 1 },
                    name,
                    namespace::POSIX,
                    true,
                ),
                1,
            ),
            resident(types::INDEX_ROOT, "$I30", &index_root_with(&kids), 2),
        ];
        img.put(
            n,
            build_record(n as u32, 1, FLAG_IN_USE | FLAG_DIRECTORY, &attrs),
        );
    };
    dir(&mut img, 20, 5, "A", vec![(21 | (1 << 48), key(20, "B"))]);
    dir(
        &mut img,
        21,
        20,
        "B",
        vec![
            (20 | (1 << 48), key(21, "A_again")),
            (21 | (1 << 48), key(21, "B_self")),
            (5 | (5 << 48), key(21, "root_again")),
            (0x7FFF_FFFF_FFFF | (1 << 48), key(21, "far_away")),
        ],
    );
    // Files whose parent references are corrupt: an extension record, a
    // record far beyond the MFT, the MFT itself.
    img.put(
        30,
        build_record_sized(
            1024,
            30,
            1,
            FLAG_IN_USE,
            31 | (1 << 48),
            &[resident(types::DATA, "", b"ext", 0)],
        ),
    );
    img.file(
        31,
        1,
        FLAG_IN_USE,
        30,
        "parent_is_extension.txt",
        vec![resident(types::DATA, "", b"x", 2)],
    );
    img.file(
        32,
        1,
        FLAG_IN_USE,
        0xFFFF_FFFF_FFFF,
        "parent_far.txt",
        vec![resident(types::DATA, "", b"y", 2)],
    );
    img.file(
        33,
        1,
        FLAG_IN_USE,
        0,
        "parent_mft.txt",
        vec![resident(types::DATA, "", b"z", 2)],
    );
    let bytes = img.finish();
    for whole in [true, false] {
        let fs = open(
            std::sync::Arc::new(diskrescue_core::source::MemSource::new(bytes.clone())),
            &NtfsOptions {
                walk_whole_mft: whole,
                ..Default::default()
            },
        )
        .unwrap();
        let start = Instant::now();
        let all = collect_entries(&fs).unwrap();
        assert!(start.elapsed() < Duration::from_secs(10));
        assert!(all.len() < 50, "{} entries", all.len());
        let a_count = all
            .iter()
            .filter(|e| crate::walk::split_id(e.id).0 == 20)
            .count();
        assert!(a_count <= 2, "A emitted {a_count} times");
        assert!(get(&all, "A/B").is_dir);
        if whole {
            for name in [
                "parent_is_extension.txt",
                "parent_far.txt",
                "parent_mft.txt",
            ] {
                let e = all.iter().find(|e| e.name == name).unwrap();
                assert!(e.path.starts_with("$orphans/"), "{}", e.path);
            }
        }
    }
}

#[test]
fn mft_record_size_4096_on_512_byte_sectors() {
    // A volume whose boot sector asks for 4 KiB records with 512-byte
    // sectors: nine fixup slots per record, all of them checked.
    let mut bytes = vec![0u8; 256 * CLUSTER as usize + 512];
    let mut boot = crate::boot::sample_boot_sector();
    boot[0x28..0x30].copy_from_slice(&(256u64 * 8).to_le_bytes());
    boot[0x30..0x38].copy_from_slice(&MFT_LCN.to_le_bytes());
    boot[0x38..0x40].copy_from_slice(&2u64.to_le_bytes());
    boot[0x40] = 0xF4; // -12 => 4096-byte records
    bytes[..512].copy_from_slice(&boot);
    let n = bytes.len();
    bytes[n - 512..].copy_from_slice(&boot);
    let rs = 4096usize;
    let records = 32u64;
    let mft_clusters = records * rs as u64 / CLUSTER;
    let put = |bytes: &mut Vec<u8>, n: u64, raw: Vec<u8>| {
        let off = (MFT_LCN * CLUSTER) as usize + n as usize * rs;
        bytes[off..off + rs].copy_from_slice(&raw);
    };
    let mp = encode_runs(&[(Some(MFT_LCN), mft_clusters)]);
    let size = records * rs as u64;
    put(
        &mut bytes,
        0,
        build_record_sized(
            rs,
            0,
            1,
            FLAG_IN_USE,
            0,
            &[
                resident(
                    types::FILE_NAME,
                    "",
                    &fname(5, 5, "$MFT", namespace::WIN32_AND_DOS, false),
                    1,
                ),
                non_resident(
                    types::DATA,
                    "",
                    0,
                    2,
                    0,
                    mft_clusters - 1,
                    (size, size, size),
                    0,
                    &mp,
                ),
            ],
        ),
    );
    put(
        &mut bytes,
        5,
        build_record_sized(
            rs,
            5,
            5,
            FLAG_IN_USE | FLAG_DIRECTORY,
            0,
            &[
                resident(
                    types::FILE_NAME,
                    "",
                    &fname(5, 5, ".", namespace::WIN32_AND_DOS, true),
                    1,
                ),
                resident(
                    types::INDEX_ROOT,
                    "$I30",
                    &crate::synth_tests::index_root_empty(),
                    2,
                ),
            ],
        ),
    );
    // A file whose resident data spans several 512-byte strides, so every
    // fixup slot holds file content.
    let content = pattern(40, 3000);
    put(
        &mut bytes,
        16,
        build_record_sized(
            rs,
            16,
            1,
            FLAG_IN_USE,
            0,
            &[
                resident(types::STANDARD_INFORMATION, "", &si(), 0),
                resident(
                    types::FILE_NAME,
                    "",
                    &fname(5, 5, "wide.txt", namespace::POSIX, false),
                    1,
                ),
                resident(types::DATA, "", &content, 2),
            ],
        ),
    );
    // The same file, torn in its sixth stride.
    let mut torn = build_record_sized(
        rs,
        17,
        1,
        FLAG_IN_USE,
        0,
        &[
            resident(types::STANDARD_INFORMATION, "", &si(), 0),
            resident(
                types::FILE_NAME,
                "",
                &fname(5, 5, "torn.txt", namespace::POSIX, false),
                1,
            ),
            resident(types::DATA, "", &content, 2),
        ],
    );
    torn[6 * 512 - 2] ^= 0xFF;
    put(&mut bytes, 17, torn);
    // A record whose update sequence count is wrong for its size.
    let mut bad = build_record_sized(
        rs,
        18,
        1,
        FLAG_IN_USE,
        0,
        &[resident(
            types::FILE_NAME,
            "",
            &fname(5, 5, "bad_usa.txt", namespace::POSIX, false),
            1,
        )],
    );
    bad[6] = 3;
    put(&mut bytes, 18, bad);
    let fs = open_bytes(bytes);
    assert_eq!(fs.record_size(), 4096);
    let all = collect_entries(&fs).unwrap();
    let e = get(&all, "wide.txt");
    assert_eq!(read_all(&fs, e).0, content);
    let e = get(&all, "torn.txt");
    assert!(e.flags.iter().any(|f| f == flags::TORN));
    assert_eq!(read_all(&fs, e).0, content);
    let t = fs.record(17).unwrap();
    assert_eq!(t.torn_strides, vec![5]);
    assert!(!all.iter().any(|e| e.name == "bad_usa.txt"));
    assert!(fs.record(18).is_err());
    let _ = RS;
}
