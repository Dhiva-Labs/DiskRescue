//! Byte-exact cross-checks against the ntfs-3g tools: every live file (and
//! every named stream) must read exactly as `ntfscat` prints it, and every
//! deleted file `ntfsundelete` can fully recover must read exactly as
//! `ntfsundelete -u` writes it. The images are populated through a real
//! ntfs-3g mount so they carry resident, non-resident, fragmented, sparse,
//! compressed and attribute-list layouts as the driver writes them.

mod common;

use std::collections::BTreeMap;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use common::*;
use diskrescue_core::fs::{EntryConfidence, FileEntry, FileSystem};
use diskrescue_core::Range;
use diskrescue_ntfs::attr::types;
use diskrescue_ntfs::walk::{split_id, SUB_ADS};
use diskrescue_ntfs::{NtfsFs, NtfsOptions};

/// `ntfscat` of the unnamed stream, or of a named stream with `-n`.
fn ntfscat_stream(img: &Path, path: &str, stream: Option<&str>) -> Option<Vec<u8>> {
    let mut cmd = Command::new(tool("ntfscat").unwrap());
    if let Some(s) = stream {
        cmd.args(["-a", "0x80", "-n", s]);
    }
    let out = cmd
        .arg(img)
        .arg(path)
        .stderr(Stdio::null())
        .output()
        .unwrap();
    out.status.success().then_some(out.stdout)
}

/// Compare every live non-directory entry with ntfscat. Returns how many
/// entries were compared.
fn compare_live_with_ntfscat(fs: &NtfsFs, img: &Path, all: &[FileEntry]) -> usize {
    let mut compared = 0;
    for e in all {
        if e.is_dir || e.deleted || e.flags.iter().any(|f| f == "virtual") {
            continue;
        }
        let (_, sub) = split_id(e.id);
        let (path, stream) = if sub >= SUB_ADS {
            let (base, s) = e.path.rsplit_once(':').unwrap();
            (format!("/{base}"), Some(s.to_string()))
        } else {
            (format!("/{}", e.path), None)
        };
        let Some(theirs) = ntfscat_stream(img, &path, stream.as_deref()) else {
            panic!("ntfscat cannot read {path} {stream:?}");
        };
        let mut ours = Vec::new();
        let rep = fs
            .read_file(e, &mut ours)
            .unwrap_or_else(|err| panic!("read {}: {err}", e.path));
        assert_eq!(ours.len() as u64, e.size, "{}: size", e.path);
        assert_eq!(rep.bytes_written, e.size, "{}", e.path);
        assert!(
            ours == theirs,
            "{}: content differs from ntfscat ({} vs {} bytes, first diff at {:?}); notes {:?}",
            e.path,
            ours.len(),
            theirs.len(),
            ours.iter().zip(&theirs).position(|(a, b)| a != b),
            rep.notes
        );
        assert!(
            rep.unreadable.is_empty() && rep.reused.is_empty(),
            "{}: {:?}",
            e.path,
            rep
        );
        compared += 1;
    }
    compared
}

fn sparse_file(path: &Path, pieces: &[(u64, Vec<u8>)], len: Option<u64>) {
    let mut f = std::fs::File::create(path).unwrap();
    for (off, bytes) in pieces {
        f.seek(SeekFrom::Start(*off)).unwrap();
        f.write_all(bytes).unwrap();
    }
    if let Some(l) = len {
        f.set_len(l).unwrap();
    }
}

fn run_ok(cmd: &mut Command) {
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{cmd:?}: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn populate_plain(m: &Mount) -> BTreeMap<String, Vec<u8>> {
    let mut expect = BTreeMap::new();
    let mut put = |rel: &str, bytes: Vec<u8>| {
        write(&m.path(rel), &bytes);
        expect.insert(rel.to_string(), bytes);
    };
    // Around the resident / non-resident boundary of a 1 KiB record.
    for (i, n) in [0usize, 1, 2, 100, 500, 650, 700, 720, 740, 760, 800, 900]
        .iter()
        .enumerate()
    {
        put(&format!("res_{n}.bin"), data(100 + i as u64, *n));
    }
    for (i, n) in [1000usize, 4095, 4096, 4097, 65535, 65536, 65537, 1_048_579]
        .iter()
        .enumerate()
    {
        put(&format!("nonres_{n}.bin"), data(200 + i as u64, *n));
    }
    std::fs::create_dir_all(m.path("deep/er/still deeper/日本")).unwrap();
    put("deep/er/still deeper/日本/写真.jpg", data(300, 54_321));
    // Named streams: small, large (non-resident) and many (attribute list).
    put("streams.txt", data(301, 3000));
    put("streams.txt:small", b"a small stream".to_vec());
    put("streams.txt:big", data(302, 200_000));
    put("many.txt", data(303, 10));
    for k in 0..48 {
        put(
            &format!("many.txt:a_fairly_long_stream_name_number_{k:03}"),
            data(400 + k, 40 + k as usize),
        );
    }
    expect
}

#[test]
fn every_live_file_matches_ntfscat() {
    if !have_fuse() || !have(&["ntfscat", "ntfsfallocate"]) {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    for cluster in [4096u32, 512, 65536] {
        let img = tmp.path().join(format!("oracle{cluster}.img"));
        mkntfs(&img, 48, cluster, "ORACLE", &[]);
        let mut expect;
        {
            let m = Mount::new(&img, "streams_interface=windows");
            expect = populate_plain(&m);
            // Sparse layouts: a hole first, holes between data, a file
            // extended by ftruncate.
            sparse_file(
                &m.path("hole_first.bin"),
                &[(3 << 20, data(500, 777))],
                None,
            );
            sparse_file(
                &m.path("holes.bin"),
                &[
                    (0, data(501, 70_000)),
                    (1 << 20, data(502, 9_000)),
                    (5 << 20, data(503, 1)),
                ],
                None,
            );
            sparse_file(
                &m.path("extended.bin"),
                &[(0, data(504, 5000))],
                Some(9 << 20),
            );
            // Fragmentation: checkerboard the free space, then write.
            std::fs::create_dir(m.path("fill")).unwrap();
            let mut n = 0u64;
            while n < 400
                && std::fs::write(m.path(&format!("fill/{n}")), data(600 + n, 24_576)).is_ok()
            {
                n += 1;
            }
            for i in (0..n).step_by(2) {
                std::fs::remove_file(m.path(&format!("fill/{i}"))).unwrap();
            }
            write(&m.path("frag.bin"), &data(505, 1_500_000));
            m.unmount();
        }
        expect.insert("frag.bin".into(), data(505, 1_500_000));
        // Preallocation: initialized size < data size, and allocated
        // size > data size.
        let src = tmp.path().join("prealloc.src");
        write(&src, &data(506, 10_000));
        ntfscp(&img, &src, "/prealloc.bin");
        run_ok(
            Command::new(tool("ntfsfallocate").unwrap())
                .args(["-l", "200000"])
                .arg(&img)
                .arg("/prealloc.bin"),
        );
        ntfscp(&img, &src, "/keepsize.bin");
        run_ok(
            Command::new(tool("ntfsfallocate").unwrap())
                .args(["-n", "-l", "400000"])
                .arg(&img)
                .arg("/keepsize.bin"),
        );

        let fs = open_img(&img, &NtfsOptions::default());
        assert!(fs.info().warnings.is_empty(), "{:?}", fs.info().warnings);
        let all = entries(&fs);
        for (rel, bytes) in &expect {
            let e = find(&all, rel);
            assert_eq!(read(&fs, e).0, *bytes, "{rel} ({cluster})");
        }
        let n = compare_live_with_ntfscat(&fs, &img, &all);
        assert!(n >= expect.len() + 4, "{n} compared");

        // The preallocated tail reads as zeros and is reported as a hole.
        let e = find(&all, "prealloc.bin");
        assert_eq!(e.size, 200_000);
        let (got, rep) = read(&fs, e);
        assert_eq!(got[..10_000], data(506, 10_000)[..]);
        assert!(got[10_000..].iter().all(|&b| b == 0));
        assert!(
            rep.holes.covers(Range::new(10_000, 200_000)),
            "{:?}",
            rep.holes
        );
        let e = find(&all, "keepsize.bin");
        assert_eq!(e.size, 10_000);
        assert_eq!(read(&fs, e).0, data(506, 10_000));
        // Holes of sparse files are holes, never unreadable.
        for name in ["hole_first.bin", "holes.bin", "extended.bin"] {
            let e = find(&all, name);
            let (_, rep) = read(&fs, e);
            assert!(rep.holes.total_len() > 0, "{name}");
            assert!(rep.unreadable.is_empty(), "{name}");
        }
        // The stream-heavy file really uses an attribute list.
        let many = find(&all, "many.txt");
        let rec = fs.record(split_id(many.id).0).unwrap();
        assert!(fs
            .attributes(&rec)
            .iter()
            .any(|a| a.type_code == types::ATTRIBUTE_LIST));
    }
}

#[test]
fn compressed_files_of_every_shape_match_ntfscat() {
    if !have_fuse() || !have(&["ntfscat"]) {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    for cluster in [4096u32, 512, 2048] {
        let img = tmp.path().join(format!("comp{cluster}.img"));
        mkntfs(&img, 48, cluster, "COMP", &["-C"]);
        let cu = 16 * cluster as usize;
        let mut expect = BTreeMap::new();
        {
            let m = Mount::new(&img, "");
            let mut put = |rel: &str, bytes: Vec<u8>| {
                write(&m.path(rel), &bytes);
                expect.insert(rel.to_string(), bytes);
            };
            for (i, n) in [
                0usize,
                1,
                100,
                1000,
                4096,
                4097,
                cu - 1,
                cu,
                cu + 1,
                cu - cluster as usize / 2,
                cu - cluster as usize - 1,
                2 * cu + 3 * cluster as usize / 2,
                5 * cu - 7,
            ]
            .iter()
            .enumerate()
            {
                put(&format!("rnd_{n}.bin"), data(700 + i as u64, *n));
                put(&format!("txt_{n}.txt"), text(*n));
            }
            // Units of zeros become sparse units.
            let mut mixed = text(cu * 2 + 100);
            mixed.extend(vec![0u8; cu * 3]);
            mixed.extend(data(720, cu + 5));
            mixed.extend(vec![0u8; 10]);
            put("mixed.bin", mixed);
            // Partly compressible units.
            let mut half = Vec::new();
            for k in 0..9 {
                half.extend(text(cu / 2));
                half.extend(data(730 + k, cu / 2));
            }
            put("half.bin", half);
            m.unmount();
        }
        {
            // Holes in a compressed file written with seeks.
            let m = Mount::new(&img, "");
            sparse_file(
                &m.path("sparse_comp.bin"),
                &[(0, text(5000)), (cu as u64 * 7 + 123, data(740, 3000))],
                Some(cu as u64 * 12),
            );
            m.unmount();
        }
        let mut sc = vec![0u8; cu * 12];
        sc[..5000].copy_from_slice(&text(5000));
        sc[cu * 7 + 123..cu * 7 + 3123].copy_from_slice(&data(740, 3000));
        expect.insert("sparse_comp.bin".into(), sc);

        let fs = open_img(&img, &NtfsOptions::default());
        let all = entries(&fs);
        for (rel, bytes) in &expect {
            let e = find(&all, rel);
            let (got, rep) = read(&fs, e);
            assert!(
                got == *bytes,
                "{rel} ({cluster}-byte clusters): first diff at {:?}, notes {:?}",
                got.iter().zip(bytes).position(|(a, b)| a != b),
                rep.notes
            );
            assert!(rep.unreadable.is_empty(), "{rel}: {:?}", rep.notes);
        }
        let n = compare_live_with_ntfscat(&fs, &img, &all);
        assert!(n >= expect.len());
    }
}

/// `ntfsundelete -s` listing: record -> recoverable percentage.
fn undeletable(img: &Path) -> BTreeMap<u64, u32> {
    let out = Command::new(tool("ntfsundelete").unwrap())
        .arg("-s")
        .arg(img)
        .stderr(Stdio::null())
        .output()
        .unwrap();
    let mut map = BTreeMap::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 7 {
            continue;
        }
        if let Ok(inode) = f[0].parse::<u64>() {
            let pct = f[2].trim_end_matches('%').parse::<u32>().unwrap_or(0);
            map.insert(inode, pct);
        }
    }
    map
}

/// Recover record `rec` with `ntfsundelete -u -T` (exact size).
fn undelete(img: &Path, rec: u64, dir: &Path) -> Option<Vec<u8>> {
    let name = format!("rec{rec}");
    let status = Command::new(tool("ntfsundelete").unwrap())
        .args([
            "-u",
            "-T",
            "-f",
            "-q",
            "-i",
            &rec.to_string(),
            "-o",
            &name,
            "-d",
        ])
        .arg(dir)
        .arg(img)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }
    std::fs::read(dir.join(&name)).ok()
}

#[test]
fn every_fully_recoverable_deleted_file_matches_ntfsundelete() {
    if !have_fuse() || !have(&["ntfsundelete"]) {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let img = tmp.path().join("undel.img");
    mkntfs(&img, 32, 4096, "UNDEL", &[]);
    let mut expect: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    {
        let m = Mount::new(&img, "");
        std::fs::create_dir_all(m.path("DCIM/100CANON")).unwrap();
        for (i, n) in [0usize, 10, 600, 3000, 4096, 70_000, 1_000_001]
            .iter()
            .enumerate()
        {
            let rel = format!("DCIM/100CANON/IMG_{i:04}.JPG");
            let bytes = data(800 + i as u64, *n);
            write(&m.path(&rel), &bytes);
            expect.insert(rel, bytes);
        }
        sparse_file(
            &m.path("DCIM/sparse.mp4"),
            &[(0, data(810, 9000)), (2 << 20, data(811, 4000))],
            None,
        );
        let mut s = data(810, 9000);
        s.resize(2 << 20, 0);
        s.extend(data(811, 4000));
        expect.insert("DCIM/sparse.mp4".into(), s);
        std::fs::create_dir(m.path("fill")).unwrap();
        let mut n = 0u64;
        while n < 200 && std::fs::write(m.path(&format!("fill/{n}")), data(900 + n, 16_384)).is_ok()
        {
            n += 1;
        }
        for i in (0..n).step_by(2) {
            std::fs::remove_file(m.path(&format!("fill/{i}"))).unwrap();
        }
        write(&m.path("DCIM/frag.mov"), &data(812, 700_000));
        expect.insert("DCIM/frag.mov".into(), data(812, 700_000));
        m.unmount();
    }
    {
        let m = Mount::new(&img, "");
        std::fs::remove_dir_all(m.path("DCIM")).unwrap();
        m.unmount();
    }
    let fs = open_img(&img, &NtfsOptions::default());
    let all = entries(&fs);
    let theirs = undeletable(&img);
    let out = tmp.path().join("out");
    std::fs::create_dir(&out).unwrap();
    let mut compared = 0;
    for (rel, bytes) in &expect {
        let e = find(&all, rel);
        assert!(e.deleted, "{rel}");
        let (got, rep) = read(&fs, e);
        assert!(got == *bytes, "{rel}: recovered content differs");
        assert!(rep.unreadable.is_empty() && rep.reused.is_empty(), "{rel}");
        assert_eq!(e.confidence, EntryConfidence::DeletedIntact, "{rel}");
        let rec = split_id(e.id).0;
        if theirs.get(&rec) == Some(&100) && e.size > 0 {
            let u = undelete(&img, rec, &out)
                .unwrap_or_else(|| panic!("ntfsundelete could not recover {rel}"));
            // ntfsundelete -T does not trim sparse files: it writes whole
            // clusters. The extra tail is cluster slack.
            assert!(
                u.len() >= got.len() && u.len() - got.len() < 4096,
                "{rel}: {} vs {} bytes",
                u.len(),
                got.len()
            );
            assert!(
                u[..got.len()] == got[..],
                "{rel}: differs from ntfsundelete"
            );
            compared += 1;
        }
    }
    assert!(compared >= 5, "{compared} compared with ntfsundelete");
}
