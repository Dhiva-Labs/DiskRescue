//! Tests on small NTFS volumes built byte by byte in memory, so tricky
//! layouts (compression units, parent cycles, hostile attribute lists,
//! depth limits) are covered without external tools.

use std::sync::Arc;

use diskrescue_core::fs::{collect_entries, EntryConfidence, FileEntry, FileSystem};
use diskrescue_core::source::MemSource;

use crate::attr::{build_file_name, namespace, types};
use crate::boot::sample_boot_sector;
use crate::record::testutil::{build_record, build_record_sized, non_resident, resident};
use crate::record::{FLAG_DIRECTORY, FLAG_IN_USE};
use crate::runs::encode_runs;
use crate::walk::{flags, split_id, MAX_DEPTH};
use crate::{open, NtfsFs, NtfsOptions};

pub(crate) const CLUSTER: u64 = 4096;
pub(crate) const RS: u64 = 1024;
pub(crate) const MFT_LCN: u64 = 4;

pub(crate) struct Img {
    pub(crate) bytes: Vec<u8>,
    pub(crate) used: Vec<bool>,
    mft_records: u64,
    next_lcn: u64,
}

pub(crate) fn si() -> Vec<u8> {
    let mut v = vec![0u8; 48];
    let t = 133_000_000_000_000_000u64;
    for i in 0..4 {
        v[i * 8..i * 8 + 8].copy_from_slice(&t.to_le_bytes());
    }
    v[0x20..0x24].copy_from_slice(&0x20u32.to_le_bytes());
    v
}

pub(crate) fn index_root_empty() -> Vec<u8> {
    let mut v = vec![0u8; 0x20];
    v[0..4].copy_from_slice(&0x30u32.to_le_bytes());
    v[4..8].copy_from_slice(&1u32.to_le_bytes());
    v[8..12].copy_from_slice(&4096u32.to_le_bytes());
    v[12] = 1;
    v[0x10..0x14].copy_from_slice(&0x10u32.to_le_bytes());
    v[0x14..0x18].copy_from_slice(&0x20u32.to_le_bytes());
    v[0x18..0x1C].copy_from_slice(&0x20u32.to_le_bytes());
    let mut end = vec![0u8; 0x10];
    end[8..10].copy_from_slice(&0x10u16.to_le_bytes());
    end[12..14].copy_from_slice(&2u16.to_le_bytes());
    v.extend(end);
    v
}

pub(crate) fn fname(parent: u64, pseq: u16, name: &str, ns: u8, dir: bool) -> Vec<u8> {
    build_file_name(
        parent | ((pseq as u64) << 48),
        name,
        ns,
        if dir { 0x1000_0000 } else { 0x20 },
        0,
    )
}

impl Img {
    pub(crate) fn new(clusters: u64, mft_records: u64) -> Img {
        let mut bytes = vec![0u8; (clusters * CLUSTER) as usize + 512];
        let mut boot = sample_boot_sector();
        let sectors = clusters * 8;
        boot[0x28..0x30].copy_from_slice(&sectors.to_le_bytes());
        boot[0x30..0x38].copy_from_slice(&MFT_LCN.to_le_bytes());
        boot[0x38..0x40].copy_from_slice(&2u64.to_le_bytes());
        bytes[..512].copy_from_slice(&boot);
        let n = bytes.len();
        bytes[n - 512..].copy_from_slice(&boot);
        let mft_clusters = (mft_records * RS).div_ceil(CLUSTER);
        let mut img = Img {
            bytes,
            used: vec![false; clusters as usize],
            mft_records,
            next_lcn: MFT_LCN + mft_clusters,
        };
        img.mark(0, 4);
        img.mark(MFT_LCN, mft_clusters);
        let mp = encode_runs(&[(Some(MFT_LCN), mft_clusters)]);
        let size = mft_records * RS;
        img.put(
            0,
            build_record(
                0,
                1,
                FLAG_IN_USE,
                &[
                    resident(types::STANDARD_INFORMATION, "", &si(), 0),
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
                        (mft_clusters * CLUSTER, size, size),
                        0,
                        &mp,
                    ),
                ],
            ),
        );
        let label: Vec<u8> = "SYNTH"
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        let mut vi = vec![0u8; 12];
        vi[8] = 3;
        vi[9] = 1;
        img.put(
            3,
            build_record(
                3,
                3,
                FLAG_IN_USE,
                &[
                    resident(types::VOLUME_NAME, "", &label, 0),
                    resident(types::VOLUME_INFORMATION, "", &vi, 1),
                ],
            ),
        );
        img.put(
            5,
            build_record(
                5,
                5,
                FLAG_IN_USE | FLAG_DIRECTORY,
                &[
                    resident(types::STANDARD_INFORMATION, "", &si(), 0),
                    resident(
                        types::FILE_NAME,
                        "",
                        &fname(5, 5, ".", namespace::WIN32_AND_DOS, true),
                        1,
                    ),
                    resident(types::INDEX_ROOT, "$I30", &index_root_empty(), 2),
                ],
            ),
        );
        img
    }

    pub(crate) fn mark(&mut self, lcn: u64, count: u64) {
        for c in lcn..lcn + count {
            self.used[c as usize] = true;
        }
    }

    pub(crate) fn put(&mut self, n: u64, raw: Vec<u8>) {
        assert!(n < self.mft_records);
        let off = (MFT_LCN * CLUSTER + n * RS) as usize;
        self.bytes[off..off + raw.len()].copy_from_slice(&raw);
    }

    /// Store `data` in fresh clusters; returns the first LCN.
    pub(crate) fn store(&mut self, data: &[u8], allocated: bool) -> u64 {
        let lcn = self.next_lcn;
        let n = (data.len() as u64).div_ceil(CLUSTER).max(1);
        let off = (lcn * CLUSTER) as usize;
        self.bytes[off..off + data.len()].copy_from_slice(data);
        if allocated {
            self.mark(lcn, n);
        }
        self.next_lcn += n;
        lcn
    }

    pub(crate) fn file(
        &mut self,
        n: u64,
        seq: u16,
        flags: u16,
        parent: u64,
        name: &str,
        extra: Vec<Vec<u8>>,
    ) {
        let mut attrs = vec![
            resident(types::STANDARD_INFORMATION, "", &si(), 0),
            resident(
                types::FILE_NAME,
                "",
                &fname(
                    parent,
                    if parent == 5 { 5 } else { 1 },
                    name,
                    namespace::POSIX,
                    flags & FLAG_DIRECTORY != 0,
                ),
                1,
            ),
        ];
        attrs.extend(extra);
        self.put(n, build_record(n as u32, seq, flags, &attrs));
    }

    pub(crate) fn finish(mut self) -> Vec<u8> {
        let mut bm = vec![0u8; self.used.len().div_ceil(8)];
        for (i, u) in self.used.iter().enumerate() {
            if *u {
                bm[i / 8] |= 1 << (i % 8);
            }
        }
        self.put(
            6,
            build_record(6, 6, FLAG_IN_USE, &[resident(types::DATA, "", &bm, 0)]),
        );
        self.bytes
    }
}

pub(crate) fn nr_data(name: &str, lcn: u64, len: u64, size: u64) -> Vec<u8> {
    let clusters = len;
    non_resident(
        types::DATA,
        name,
        0,
        3,
        0,
        clusters - 1,
        (clusters * CLUSTER, size, size),
        0,
        &encode_runs(&[(Some(lcn), clusters)]),
    )
}

pub(crate) fn open_bytes(bytes: Vec<u8>) -> NtfsFs {
    open(Arc::new(MemSource::new(bytes)), &NtfsOptions::default()).unwrap()
}

pub(crate) fn read_all(
    fs: &NtfsFs,
    e: &FileEntry,
) -> (Vec<u8>, diskrescue_core::fs::FileReadReport) {
    let mut v = Vec::new();
    let r = fs.read_file(e, &mut v).unwrap();
    (v, r)
}

pub(crate) fn get<'a>(all: &'a [FileEntry], path: &str) -> &'a FileEntry {
    all.iter().find(|e| e.path == path).unwrap_or_else(|| {
        panic!(
            "{path} missing: {:?}",
            all.iter().map(|e| &e.path).collect::<Vec<_>>()
        )
    })
}

pub(crate) fn pattern(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

#[test]
fn basic_volume_live_deleted_and_reused() {
    let mut img = Img::new(256, 64);
    img.file(
        16,
        1,
        FLAG_IN_USE,
        5,
        "small.txt",
        vec![resident(types::DATA, "", b"hello", 2)],
    );
    let big = pattern(1, 10_000);
    let l = img.store(&big, true);
    img.file(
        17,
        1,
        FLAG_IN_USE,
        5,
        "big.bin",
        vec![nr_data("", l, 3, 10_000)],
    );
    let gone = pattern(2, 9000);
    let l = img.store(&gone, false);
    img.file(18, 2, 0, 5, "gone.jpg", vec![nr_data("", l, 3, 9000)]);
    let reused = pattern(3, 5000);
    let l = img.store(&reused, true);
    img.file(19, 2, 0, 5, "reused.jpg", vec![nr_data("", l, 2, 5000)]);
    // A directory with a Win32 and a DOS name and a child.
    let attrs = vec![
        resident(types::STANDARD_INFORMATION, "", &si(), 0),
        resident(
            types::FILE_NAME,
            "",
            &fname(5, 5, "Holiday Photos", namespace::WIN32, true),
            1,
        ),
        resident(
            types::FILE_NAME,
            "",
            &fname(5, 5, "HOLIDA~1", namespace::DOS, true),
            2,
        ),
        resident(types::INDEX_ROOT, "$I30", &index_root_empty(), 3),
    ];
    img.put(
        20,
        build_record(20, 1, FLAG_IN_USE | FLAG_DIRECTORY, &attrs),
    );
    img.file(
        21,
        1,
        FLAG_IN_USE,
        20,
        "a.jpg",
        vec![resident(types::DATA, "", b"jpeg", 2)],
    );
    let fs = open_bytes(img.finish());
    assert_eq!(fs.info().label.as_deref(), Some("SYNTH"));
    assert!(fs.info().warnings.is_empty(), "{:?}", fs.info().warnings);
    let all = collect_entries(&fs).unwrap();
    assert_eq!(read_all(&fs, get(&all, "small.txt")).0, b"hello");
    assert_eq!(read_all(&fs, get(&all, "big.bin")).0, big);
    let g = get(&all, "gone.jpg");
    assert!(g.deleted);
    assert_eq!(g.confidence, EntryConfidence::DeletedIntact);
    assert_eq!(read_all(&fs, g).0, gone);
    let r = get(&all, "reused.jpg");
    assert_eq!(r.confidence, EntryConfidence::DeletedGuessed);
    let (data, rep) = read_all(&fs, r);
    assert_eq!(data, reused);
    assert_eq!(rep.reused.total_len(), 5000);
    assert!(get(&all, "Holiday Photos").is_dir);
    assert!(!all.iter().any(|e| e.name == "HOLIDA~1"));
    assert_eq!(read_all(&fs, get(&all, "Holiday Photos/a.jpg")).0, b"jpeg");
    // Free space: the deleted file's clusters, not the reused ones.
    let free = fs.unallocated().unwrap();
    for x in &g.extents {
        assert!(free.covers(*x));
    }
    for x in &r.extents {
        assert!(!free.intersects(*x));
    }
}

#[test]
fn compressed_units_raw_compressed_and_sparse() {
    let mut img = Img::new(256, 64);
    // Unit 0: LZNT1 data in 1 real cluster + 15 sparse. Two literal-only
    // chunks, then a stored (uncompressed) chunk.
    let mut c = Vec::new();
    let mut chunk = vec![0x00u8];
    chunk.extend_from_slice(b"ABCDEFGH");
    c.extend_from_slice(&(0xB000u16 | (chunk.len() as u16 - 1)).to_le_bytes());
    c.extend_from_slice(&chunk);
    let chunk2 = [0x08u8, b'x', b'y', b'z', 0x06, 0x20];
    c.extend_from_slice(&(0xB000u16 | (chunk2.len() as u16 - 1)).to_le_bytes());
    c.extend_from_slice(&chunk2);
    let stored = pattern(9, 4096);
    c.extend_from_slice(&0x3FFFu16.to_le_bytes());
    c.extend_from_slice(&stored);
    c.extend_from_slice(&[0, 0]);
    let mut c_cluster = c.clone();
    c_cluster.resize(2 * 4096, 0);
    let l0 = img.store(&c_cluster, true);
    // Unit 1: stored raw (16 real clusters).
    let raw = pattern(5, 65536);
    let l1 = img.store(&raw, true);
    // Unit 2: sparse. Unit 3: partial, compressed again (copy of unit 0).
    let l3 = img.store(&c_cluster, true);
    let size = 3 * 65536 + 20_000;
    let runs = encode_runs(&[
        (Some(l0), 2),
        (None, 14),
        (Some(l1), 16),
        (None, 16),
        (Some(l3), 2),
        (None, 14),
    ]);
    let attr = non_resident(
        types::DATA,
        "",
        crate::attr::ATTR_COMPRESSED,
        2,
        0,
        63,
        (64 * 4096, size, size),
        4,
        &runs,
    );
    img.file(16, 1, FLAG_IN_USE, 5, "packed.bin", vec![attr]);
    let fs = open_bytes(img.finish());
    let all = collect_entries(&fs).unwrap();
    let e = get(&all, "packed.bin");
    assert!(e.flags.iter().any(|f| f == flags::COMPRESSED));
    let (got, rep) = read_all(&fs, e);
    assert_eq!(got.len() as u64, size);
    let mut unit0 = vec![0u8; 65536];
    unit0[..8].copy_from_slice(b"ABCDEFGH");
    unit0[4096..4096 + 12].copy_from_slice(b"xyzxyzxyzxyz");
    unit0[8192..12288].copy_from_slice(&stored);
    assert!(got[..65536] == unit0[..]);
    assert!(got[65536..131072] == raw[..]);
    assert!(got[131072..196608].iter().all(|&b| b == 0));
    assert!(got[196608..] == unit0[..20_000]);
    assert!(rep.unreadable.is_empty(), "{:?}", rep.notes);
    assert!(rep.holes.contains(131072));
    // Decompression disabled: refused.
    let fs2 = open(
        fs.source().clone(),
        &NtfsOptions {
            decompress: false,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(fs2.read_file(e, &mut Vec::new()).is_err());
}

#[test]
fn parent_cycles_self_parents_and_missing_parents() {
    let mut img = Img::new(256, 64);
    let dir = |img: &mut Img, n: u64, parent: u64, name: &str| {
        let attrs = vec![
            resident(types::STANDARD_INFORMATION, "", &si(), 0),
            resident(
                types::FILE_NAME,
                "",
                &fname(parent, 1, name, namespace::POSIX, true),
                1,
            ),
            resident(types::INDEX_ROOT, "$I30", &index_root_empty(), 2),
        ];
        img.put(
            n,
            build_record(n as u32, 1, FLAG_IN_USE | FLAG_DIRECTORY, &attrs),
        );
    };
    dir(&mut img, 30, 31, "A");
    dir(&mut img, 31, 30, "B");
    img.file(
        32,
        1,
        FLAG_IN_USE,
        30,
        "in_cycle.txt",
        vec![resident(types::DATA, "", b"c", 2)],
    );
    dir(&mut img, 33, 33, "Self");
    img.file(
        34,
        1,
        FLAG_IN_USE,
        50,
        "lost_parent.txt",
        vec![resident(types::DATA, "", b"l", 2)],
    );
    img.file(
        35,
        1,
        FLAG_IN_USE,
        34,
        "under_a_file.txt",
        vec![resident(types::DATA, "", b"f", 2)],
    );
    // Parent reference with a stale sequence number.
    let attrs = vec![
        resident(types::STANDARD_INFORMATION, "", &si(), 0),
        resident(
            types::FILE_NAME,
            "",
            &fname(36, 9, "old.txt", namespace::POSIX, false),
            1,
        ),
        resident(types::DATA, "", b"o", 2),
    ];
    img.put(37, build_record(37, 1, FLAG_IN_USE, &attrs));
    dir(&mut img, 36, 5, "Reused");
    // A nameless record with data.
    img.put(
        40,
        build_record(40, 1, 0, &[resident(types::DATA, "", b"nameless data", 0)]),
    );
    let fs = open_bytes(img.finish());
    let all = collect_entries(&fs).unwrap();
    let has = |name: &str, flag: &str| {
        let e = all
            .iter()
            .find(|e| e.name == name)
            .unwrap_or_else(|| panic!("{name} missing"));
        assert!(e.path.starts_with("$orphans/"), "{}", e.path);
        assert!(e.flags.iter().any(|f| f == flag), "{name}: {:?}", e.flags);
        e
    };
    has("A", flags::CYCLE);
    assert!(all
        .iter()
        .any(|e| e.name == "B" && e.path.starts_with("$orphans/")));
    assert!(all
        .iter()
        .any(|e| e.name == "in_cycle.txt" && e.path.starts_with("$orphans/")));
    has("Self", flags::CYCLE);
    has("lost_parent.txt", flags::PARENT_MISSING);
    has("under_a_file.txt", flags::PARENT_NOT_DIR);
    has("old.txt", flags::PARENT_REUSED);
    let nl = has("record_40", flags::NAMELESS);
    assert_eq!(read_all(&fs, nl).0, b"nameless data");
    assert!(get(&all, "Reused").is_dir);
    // Every record is listed once.
    let mut recs: Vec<u64> = all
        .iter()
        .filter(|e| !e.flags.iter().any(|f| f == flags::VIRTUAL))
        .map(|e| split_id(e.id).0)
        .collect();
    let n = recs.len();
    recs.sort_unstable();
    recs.dedup();
    assert_eq!(recs.len(), n);
    // Directories precede their children.
    for (i, e) in all.iter().enumerate() {
        if let Some(pos) = e.path.rfind('/') {
            let parent = &e.path[..pos];
            let p = all.iter().position(|x| x.path == parent);
            assert!(p.is_some_and(|p| p < i), "{} before its parent", e.path);
        }
    }
}

#[test]
fn depth_limit_reroots_deep_chains() {
    let depth = MAX_DEPTH as u64 + 20;
    let mut img = Img::new(2048, depth + 40);
    for i in 0..depth {
        let n = 24 + i;
        let parent = if i == 0 { 5 } else { n - 1 };
        let attrs = vec![
            resident(
                types::FILE_NAME,
                "",
                &fname(
                    parent,
                    if i == 0 { 5 } else { 1 },
                    "d",
                    namespace::POSIX,
                    true,
                ),
                1,
            ),
            resident(types::INDEX_ROOT, "$I30", &index_root_empty(), 2),
        ];
        img.put(
            n,
            build_record(n as u32, 1, FLAG_IN_USE | FLAG_DIRECTORY, &attrs),
        );
    }
    let leaf = 24 + depth;
    img.file(
        leaf,
        1,
        FLAG_IN_USE,
        leaf - 1,
        "leaf.txt",
        vec![resident(types::DATA, "", b"deep", 2)],
    );
    let fs = open_bytes(img.finish());
    let all = collect_entries(&fs).unwrap();
    let e = all
        .iter()
        .find(|e| e.name == "leaf.txt")
        .expect("leaf listed");
    assert_eq!(read_all(&fs, e).0, b"deep");
    assert!(e.path.starts_with("$orphans/"));
    let dirs = all.iter().filter(|e| e.name == "d").count() as u64;
    assert_eq!(dirs, depth);
}

#[test]
fn hostile_attribute_lists() {
    let mut img = Img::new(256, 64);
    let part1 = pattern(1, 8192);
    let part2 = pattern(2, 4096);
    let l1 = img.store(&part1, true);
    let l2 = img.store(&part2, true);
    let size = 12_000u64;
    // Base record 16: first $DATA piece (VCN 0..1) + attribute list.
    let piece0 = non_resident(
        types::DATA,
        "",
        0,
        2,
        0,
        1,
        (3 * 4096, size, size),
        0,
        &encode_runs(&[(Some(l1), 2)]),
    );
    let piece1 = non_resident(
        types::DATA,
        "",
        0,
        0,
        2,
        2,
        (0, 0, 0),
        0,
        &encode_runs(&[(Some(l2), 1)]),
    );
    let entry = |t: u32, rec: u64, seq: u16, id: u16, low: u64| {
        let mut e = vec![0u8; 0x20];
        e[0..4].copy_from_slice(&t.to_le_bytes());
        e[4..6].copy_from_slice(&0x20u16.to_le_bytes());
        e[7] = 0x1A;
        e[8..16].copy_from_slice(&low.to_le_bytes());
        e[16..24].copy_from_slice(&(rec | ((seq as u64) << 48)).to_le_bytes());
        e[24..26].copy_from_slice(&id.to_le_bytes());
        e
    };
    let mut list = Vec::new();
    list.extend(entry(types::DATA, 16, 1, 2, 0));
    list.extend(entry(types::DATA, 17, 1, 0, 2)); // genuine extension
    list.extend(entry(types::DATA, 18, 1, 0, 2)); // belongs to someone else
    list.extend(entry(types::DATA, 9999, 1, 0, 2)); // beyond the MFT
    list.extend(entry(types::DATA, 16, 1, 7, 0)); // points back at the base
    list.extend(entry(types::DATA, 17, 1, 0, 2)); // duplicate
    let base = build_record(
        16,
        1,
        FLAG_IN_USE,
        &[
            resident(types::STANDARD_INFORMATION, "", &si(), 0),
            resident(types::ATTRIBUTE_LIST, "", &list, 5),
            resident(
                types::FILE_NAME,
                "",
                &fname(5, 5, "split.bin", namespace::POSIX, false),
                1,
            ),
            piece0,
        ],
    );
    img.put(16, base);
    img.put(
        17,
        build_record_sized(
            1024,
            17,
            1,
            FLAG_IN_USE,
            16 | (1 << 48),
            std::slice::from_ref(&piece1),
        ),
    );
    img.put(
        18,
        build_record_sized(1024, 18, 1, FLAG_IN_USE, 50 | (1 << 48), &[piece1]),
    );
    let fs = open_bytes(img.finish());
    let all = collect_entries(&fs).unwrap();
    let e = get(&all, "split.bin");
    let (got, rep) = read_all(&fs, e);
    let mut expect = part1.clone();
    expect.extend_from_slice(&part2);
    expect.truncate(size as usize);
    assert_eq!(got, expect);
    assert!(rep.unreadable.is_empty(), "{:?}", rep.notes);
    // The pieces are physically adjacent, so their byte extents merge.
    assert_eq!(e.extent_bytes(), size);
    assert!(!e.flags.iter().any(|f| f == flags::INCOMPLETE));
}

#[test]
fn encrypted_reparse_and_streams() {
    let mut img = Img::new(256, 64);
    img.file(
        16,
        1,
        FLAG_IN_USE,
        5,
        "secret.doc",
        vec![
            resident(types::DATA, "", b"ciphertext", 2),
            resident(types::LOGGED_UTILITY_STREAM, "$EFS", b"efs", 3),
        ],
    );
    let mut rp = vec![0u8; 16];
    rp[0..4].copy_from_slice(&crate::attr::REPARSE_TAG_SYMLINK.to_le_bytes());
    img.file(
        17,
        1,
        FLAG_IN_USE,
        5,
        "link",
        vec![resident(types::REPARSE_POINT, "", &rp, 2)],
    );
    let attrs = vec![
        resident(types::STANDARD_INFORMATION, "", &si(), 0),
        resident(
            types::FILE_NAME,
            "",
            &fname(5, 5, "Dir", namespace::WIN32, true),
            1,
        ),
        resident(types::INDEX_ROOT, "$I30", &index_root_empty(), 2),
        resident(types::DATA, "notes", b"dir stream", 3),
    ];
    img.put(
        18,
        build_record(18, 1, FLAG_IN_USE | FLAG_DIRECTORY, &attrs),
    );
    let fs = open_bytes(img.finish());
    let all = collect_entries(&fs).unwrap();
    let s = get(&all, "secret.doc");
    assert!(s.flags.iter().any(|f| f == flags::ENCRYPTED));
    assert!(matches!(
        fs.read_file(s, &mut Vec::new()),
        Err(diskrescue_core::Error::Unsupported(_))
    ));
    let l = get(&all, "link");
    assert!(l.flags.iter().any(|f| f == flags::SYMLINK));
    assert_eq!(l.size, 0);
    let d = get(&all, "Dir:notes");
    assert!(!d.is_dir);
    assert_eq!(read_all(&fs, d).0, b"dir stream");
    assert!(fs.read_file(get(&all, "Dir"), &mut Vec::new()).is_err());
}
