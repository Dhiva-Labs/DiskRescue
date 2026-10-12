//! Live files on real NTFS images made by mkntfs, populated with ntfscp or
//! through an ntfs-3g FUSE mount.

mod common;

use std::collections::HashSet;

use common::*;
use diskrescue_core::fs::{EntryConfidence, FileSystem};
use diskrescue_core::RangeSet;
use diskrescue_ntfs::{probe, NtfsOptions};

#[test]
fn mounted_volume_features_roundtrip() {
    if !have_fuse() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("live.img");
    mkntfs(&img, 64, 4096, "LIVEVOL", &[]);
    let small = data(1, 500);
    let big = data(2, 8 << 20);
    let deep_data = data(3, 12_345);
    let uni = data(4, 70_000);
    let linked = data(5, 33_000);
    let ads = b"camera=TestCam; rating=5".to_vec();
    let mut deep_rel = String::new();
    {
        let m = Mount::new(&img, "streams_interface=windows");
        write(&m.path("small.txt"), &small);
        write(&m.path("small.txt:meta"), &ads);
        write(&m.path("big.bin"), &big);
        write(&m.path("empty.txt"), b"");
        for i in 0..30 {
            deep_rel.push_str(&format!("level{i:02}/"));
        }
        std::fs::create_dir_all(m.path(&deep_rel)).unwrap();
        deep_rel.push_str("deep.jpg");
        write(&m.path(&deep_rel), &deep_data);
        std::fs::create_dir(m.path("Fotos")).unwrap();
        write(&m.path("Fotos/Ünïcødé 日本語 😀.jpg"), &uni);
        std::fs::create_dir(m.path("links")).unwrap();
        write(&m.path("link_a.bin"), &linked);
        std::fs::hard_link(m.path("link_a.bin"), m.path("links/link_b.bin")).unwrap();
        // A sparse file: data, a 5 MiB hole, data.
        {
            use std::io::{Seek, SeekFrom, Write};
            let mut f = std::fs::File::create(m.path("sparse.bin")).unwrap();
            f.write_all(&data(6, 4096)).unwrap();
            f.seek(SeekFrom::Start(5 << 20)).unwrap();
            f.write_all(&data(7, 4096)).unwrap();
        }
        m.unmount();
    }
    let head = std::fs::read(&img).unwrap()[..512].to_vec();
    assert!(probe(&head));

    let fs = open_img(&img, &NtfsOptions::default());
    let info = fs.info();
    assert_eq!(info.label.as_deref(), Some("LIVEVOL"));
    assert_eq!(info.cluster_size, 4096);
    let serial = u64::from_le_bytes(head[0x48..0x50].try_into().unwrap());
    assert_eq!(
        info.serial.as_deref(),
        Some(format!("{serial:016X}").as_str())
    );
    assert!(info.warnings.is_empty(), "{:?}", info.warnings);

    let all = entries(&fs);
    let ids: HashSet<u64> = all.iter().map(|e| e.id).collect();
    assert_eq!(ids.len(), all.len(), "entry ids must be unique");
    assert!(
        all.iter().all(|e| !e.deleted),
        "fresh volume has no deleted files"
    );
    assert!(
        !all.iter()
            .any(|e| e.name.starts_with('$') || e.path.starts_with("$orphans")),
        "system files and orphans are not listed"
    );

    // Resident small file.
    let e = find(&all, "small.txt");
    assert!(e.resident);
    assert!(e.extents.is_empty());
    assert_eq!(e.size, 500);
    assert_eq!(e.confidence, EntryConfidence::Allocated);
    assert!(e.modified.is_some_and(|t| t.is_plausible()));
    assert_eq!(read(&fs, e).0, small);

    // Alternate data stream.
    let e = find(&all, "small.txt:meta");
    assert!(e.flags.iter().any(|f| f == "ads"));
    assert_eq!(read(&fs, e).0, ads);

    // Large non-resident file, checked against ntfscat too.
    let e = find(&all, "big.bin");
    assert!(!e.resident);
    assert_eq!(e.size, 8 << 20);
    assert_eq!(e.extent_bytes(), 8 << 20);
    let (got, rep) = read(&fs, e);
    assert!(rep.is_complete() && rep.holes.is_empty());
    assert_eq!(got, big);
    if have(&["ntfscat"]) {
        assert_eq!(ntfscat(&img, "/big.bin"), big);
    }

    // Empty file.
    let e = find(&all, "empty.txt");
    assert_eq!(e.size, 0);
    assert!(read(&fs, e).0.is_empty());

    // Deep path: every level is a directory reported before its child.
    let e = find(&all, &deep_rel);
    assert_eq!(read(&fs, e).0, deep_data);
    let pos_file = all.iter().position(|x| x.path == deep_rel).unwrap();
    let pos_dir = all.iter().position(|x| x.path == "level00").unwrap();
    assert!(pos_dir < pos_file);
    assert!(find(&all, "level00").is_dir);

    // Non-ASCII name.
    let e = find(&all, "Fotos/Ünïcødé 日本語 😀.jpg");
    assert_eq!(e.name, "Ünïcødé 日本語 😀.jpg");
    assert_eq!(read(&fs, e).0, uni);
    let dir = find(&all, "Fotos");
    assert_eq!(e.parent_id, Some(dir.id));

    // Hard links: two entries, same data, distinct ids.
    let a = find(&all, "link_a.bin");
    let b = find(&all, "links/link_b.bin");
    assert_ne!(a.id, b.id);
    assert_eq!(a.id & 0xFFFF_FFFF_FFFF, b.id & 0xFFFF_FFFF_FFFF);
    assert!(a.flags.iter().any(|f| f == "hardlink") ^ b.flags.iter().any(|f| f == "hardlink"));
    assert_eq!(read(&fs, a).0, linked);
    assert_eq!(read(&fs, b).0, linked);

    // Sparse file: holes reported, content exact.
    let e = find(&all, "sparse.bin");
    assert_eq!(e.size, (5 << 20) + 4096);
    let (got, rep) = read(&fs, e);
    let mut expect = vec![0u8; (5 << 20) + 4096];
    expect[..4096].copy_from_slice(&data(6, 4096));
    expect[5 << 20..].copy_from_slice(&data(7, 4096));
    assert_eq!(got, expect);
    assert!(rep.holes.total_len() >= 4 << 20, "{:?}", rep.holes);
    assert!(rep.unreadable.is_empty());
    assert!(e.flags.iter().any(|f| f == "sparse"));
    assert!(e.extent_bytes() < 1 << 20);

    // Free space never overlaps live data.
    let free = fs.unallocated().unwrap();
    assert!(free.total_len() > 0);
    let used: RangeSet = all.iter().flat_map(|e| e.extents.iter().copied()).collect();
    assert!(free.intersection(&used).is_empty());
    for r in fs.mft_extents() {
        assert!(!free.intersects(r), "MFT is allocated");
    }
    let alloc = fs.allocated().unwrap();
    assert_eq!(alloc.total_len() + free.total_len(), info.total_size);

    // The directory-tree walk sees the same live files.
    let tree = entries(&open_img(
        &img,
        &NtfsOptions {
            walk_whole_mft: false,
            ..Default::default()
        },
    ));
    let mut p1: Vec<&str> = all.iter().map(|e| e.path.as_str()).collect();
    let mut p2: Vec<&str> = tree.iter().map(|e| e.path.as_str()).collect();
    p1.sort();
    p2.sort();
    assert_eq!(p1, p2);
}

#[test]
fn walk_control_skip_and_stop() {
    if !have(&["mkntfs", "ntfscp"]) {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("ctl.img");
    mkntfs(&img, 16, 4096, "CTL", &[]);
    let f = tmp.path().join("f");
    write(&f, &data(9, 3000));
    ntfscp(&img, &f, "/a.bin");
    ntfscp(&img, &f, "/b.bin");
    let fs = open_img(&img, &NtfsOptions::default());
    let mut seen = 0;
    fs.walk(&mut |_| {
        seen += 1;
        diskrescue_core::fs::WalkControl::Stop
    })
    .unwrap();
    assert_eq!(seen, 1);
}

fn cluster_roundtrip(cluster: u32, sector: Option<u32>) {
    if !have(&["mkntfs", "ntfscp"]) {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("c.img");
    let sector_arg = sector.map(|s| s.to_string());
    let extra: Vec<&str> = match &sector_arg {
        Some(s) => vec!["-s", s.as_str()],
        None => vec![],
    };
    mkntfs(&img, 48, cluster, "CLUSTER", &extra);
    let files: Vec<(String, Vec<u8>)> = vec![
        ("tiny.txt".into(), data(10, 100)),
        ("photo.jpg".into(), data(11, 1_234_567)),
        ("video.mp4".into(), data(12, 5 << 20)),
    ];
    for (name, bytes) in &files {
        let p = tmp.path().join(name);
        write(&p, bytes);
        ntfscp(&img, &p, &format!("/{name}"));
    }
    let fs = open_img(&img, &NtfsOptions::default());
    assert_eq!(fs.info().cluster_size, cluster);
    if let Some(s) = sector {
        assert_eq!(fs.boot_sector().bytes_per_sector, s);
        assert_eq!(fs.record_size(), 4096);
    }
    assert!(fs.info().warnings.is_empty(), "{:?}", fs.info().warnings);
    let all = entries(&fs);
    for (name, bytes) in &files {
        let e = find(&all, name);
        assert_eq!(e.size, bytes.len() as u64);
        let (got, rep) = read(&fs, e);
        assert!(rep.is_complete());
        assert_eq!(&got, bytes, "{name} with {cluster}-byte clusters");
        if bytes.len() > 4096 {
            assert_eq!(e.extent_bytes(), bytes.len() as u64);
            for r in &e.extents {
                assert_eq!(r.start % cluster as u64, 0);
            }
        }
    }
}

#[test]
fn cluster_size_512() {
    cluster_roundtrip(512, None);
}

#[test]
fn cluster_size_4096() {
    cluster_roundtrip(4096, None);
}

#[test]
fn cluster_size_65536() {
    cluster_roundtrip(65536, None);
}

#[test]
fn four_k_sectors_and_records() {
    cluster_roundtrip(4096, Some(4096));
}

#[test]
fn negative_sectors_per_cluster_encoding() {
    if !have(&["mkntfs", "ntfscp"]) {
        return;
    }
    // A 64 KiB-cluster volume has 128 sectors per cluster, which can also
    // be written as the negative encoding 0xF9 (2^(256-249) = 128). Patch
    // both boot sectors and check the volume reads identically.
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("neg.img");
    mkntfs(&img, 48, 65536, "NEG", &[]);
    let content = data(20, 300_000);
    let p = tmp.path().join("x");
    write(&p, &content);
    ntfscp(&img, &p, "/x.bin");
    let mut bytes = std::fs::read(&img).unwrap();
    assert_eq!(bytes[0x0D], 0x80);
    bytes[0x0D] = 0xF9;
    let last = bytes.len() - 512;
    if bytes[last + 3..last + 11] == *b"NTFS    " {
        bytes[last + 0x0D] = 0xF9;
    }
    let fs = open_mem(bytes, &NtfsOptions::default()).unwrap();
    assert_eq!(fs.boot_sector().sectors_per_cluster_raw, 0xF9);
    assert_eq!(fs.boot_sector().sectors_per_cluster, 128);
    assert_eq!(fs.info().cluster_size, 65536);
    let all = entries(&fs);
    assert_eq!(read(&fs, find(&all, "x.bin")).0, content);
}

#[test]
fn compressed_files() {
    if !have_fuse() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("comp.img");
    // -C sets the compression flag on the root, so new files are compressed.
    mkntfs(&img, 48, 4096, "COMP", &["-C"]);
    let txt = text(1_500_000);
    let rnd = data(30, 300_000);
    let mut mixed = text(200_000);
    mixed.extend(vec![0u8; 300_000]);
    mixed.extend(data(31, 100_000));
    mixed.extend(text(77_777));
    {
        let m = Mount::new(&img, "");
        write(&m.path("log.txt"), &txt);
        write(&m.path("random.bin"), &rnd);
        write(&m.path("mixed.bin"), &mixed);
        write(&m.path("tiny.txt"), b"tiny but compressed flag");
        m.unmount();
    }
    let fs = open_img(&img, &NtfsOptions::default());
    let all = entries(&fs);
    for (name, bytes) in [
        ("log.txt", &txt),
        ("random.bin", &rnd),
        ("mixed.bin", &mixed),
    ] {
        let e = find(&all, name);
        assert!(
            e.flags.iter().any(|f| f == "compressed"),
            "{name}: {:?}",
            e.flags
        );
        let (got, rep) = read(&fs, e);
        assert!(rep.unreadable.is_empty(), "{name}: {:?}", rep.notes);
        assert_eq!(got.len(), bytes.len());
        assert!(got == *bytes, "{name} content differs");
        if have(&["ntfscat"]) {
            assert_eq!(ntfscat(&img, &format!("/{name}")), *bytes);
        }
    }
    // The text compresses: far fewer clusters than its size.
    assert!(find(&all, "log.txt").extent_bytes() < 1_000_000);
    assert_eq!(
        read(&fs, find(&all, "tiny.txt")).0,
        b"tiny but compressed flag"
    );
    // With decompression disabled, compressed files are refused.
    let fs2 = open_img(
        &img,
        &NtfsOptions {
            decompress: false,
            ..Default::default()
        },
    );
    let e = find(&all, "log.txt");
    assert!(fs2.read_file(e, &mut Vec::new()).is_err());
}

#[test]
fn fragmented_file() {
    if !have_fuse() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("frag.img");
    mkntfs(&img, 16, 4096, "FRAG", &[]);
    let big = data(40, 2_000_000);
    {
        let m = Mount::new(&img, "");
        let mut n = 0;
        loop {
            if std::fs::write(m.path(&format!("fill{n}.bin")), data(100 + n, 131_072)).is_err() {
                break;
            }
            n += 1;
            assert!(n < 1000);
        }
        for i in (0..n).step_by(2) {
            std::fs::remove_file(m.path(&format!("fill{i}.bin"))).unwrap();
        }
        write(&m.path("big.bin"), &big);
        m.unmount();
    }
    let fs = open_img(&img, &NtfsOptions::default());
    let all = entries(&fs);
    let e = find(&all, "big.bin");
    assert!(
        e.extents.len() >= 2,
        "expected fragmentation: {:?}",
        e.extents
    );
    assert_eq!(e.extent_bytes(), big.len() as u64);
    assert_eq!(read(&fs, e).0, big);
    let f1 = find(&all, "fill1.bin");
    assert_eq!(read(&fs, f1).0, data(101, 131_072));
}

#[test]
fn source_window_inside_a_disk_image() {
    if !have(&["mkntfs", "ntfscp"]) {
        return;
    }
    use diskrescue_core::source::{MemSource, SubSource};
    use std::sync::Arc;
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("p.img");
    mkntfs(&img, 16, 4096, "PART", &[]);
    let content = data(50, 200_000);
    let p = tmp.path().join("x");
    write(&p, &content);
    ntfscp(&img, &p, "/inside.bin");
    let vol = std::fs::read(&img).unwrap();
    let mut disk = vec![0u8; 1 << 20];
    disk.extend_from_slice(&vol);
    disk.extend(vec![0u8; 1 << 20]);
    let base: diskrescue_core::SharedSource = Arc::new(MemSource::new(disk));
    let sub = Arc::new(SubSource::new(base, 1 << 20, vol.len() as u64));
    let fs = diskrescue_ntfs::open(sub, &NtfsOptions::default()).unwrap();
    let all = entries(&fs);
    assert_eq!(read(&fs, find(&all, "inside.bin")).0, content);
}
