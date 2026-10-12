#![cfg(unix)]

#[macro_use]
mod common;

use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use common::*;
use diskrescue_fixtures::disk::{
    build, guid_from_str, DiskSpec, PartSpec, PartType, Scheme, GUID_BASIC_DATA, GUID_LINUX_FS,
};
use diskrescue_fixtures::fs::{self, FileSpec, FsSpec, FsType};

fn volume(dir: &Path, name: &str, kind: FsType, size: u64) -> PathBuf {
    let p = dir.join(name);
    let mut s = FsSpec::new(kind, size);
    s.files
        .push(FileSpec::new("hello.txt", data(1000, name.len() as u64)));
    fs::build(&s, &p).unwrap();
    p
}

fn read(img: &Path, off: u64, len: usize) -> Vec<u8> {
    let mut f = std::fs::File::open(img).unwrap();
    f.seek(SeekFrom::Start(off)).unwrap();
    let mut v = vec![0u8; len];
    f.read_exact(&mut v).unwrap();
    v
}

fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn le64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// Independent GPT checker: both headers, both arrays, CRCs, ranges.
fn check_gpt(img: &Path, ss: u64, expect: &[(u64, u64)]) {
    let total = std::fs::metadata(img).unwrap().len() / ss;
    let entry_secs = (128 * 128u64).div_ceil(ss);
    let prot = read(img, 0, 512);
    assert_eq!(&prot[510..512], &[0x55, 0xAA]);
    assert_eq!(prot[0x1BE + 4], 0xEE, "protective MBR type");
    assert_eq!(le32(&prot, 0x1BE + 8), 1);
    for (hdr_lba, other_lba, arr_lba) in [
        (1u64, total - 1, 2u64),
        (total - 1, 1, total - entry_secs - 1),
    ] {
        let mut h = read(img, hdr_lba * ss, 92);
        assert_eq!(&h[..8], b"EFI PART");
        assert_eq!(le32(&h, 8), 0x0001_0000);
        assert_eq!(le32(&h, 12), 92);
        let crc = le32(&h, 16);
        h[16..20].copy_from_slice(&[0; 4]);
        assert_eq!(crc32fast::hash(&h), crc, "header CRC at LBA {hdr_lba}");
        assert_eq!(le64(&h, 24), hdr_lba);
        assert_eq!(le64(&h, 32), other_lba);
        assert_eq!(le64(&h, 40), 2 + entry_secs, "first usable");
        assert_eq!(le64(&h, 48), total - entry_secs - 2, "last usable");
        assert_eq!(le64(&h, 72), arr_lba);
        assert_eq!(le32(&h, 80), 128);
        assert_eq!(le32(&h, 84), 128);
        let arr = read(img, arr_lba * ss, 128 * 128);
        assert_eq!(crc32fast::hash(&arr), le32(&h, 88), "array CRC");
        for (i, &(start, size)) in expect.iter().enumerate() {
            let e = &arr[i * 128..(i + 1) * 128];
            assert_ne!(e[..16], [0u8; 16]);
            assert_ne!(e[16..32], [0u8; 16], "unique GUID set");
            assert_eq!(le64(e, 32), start / ss);
            assert_eq!(le64(e, 40), (start + size) / ss - 1);
        }
        assert!(arr[expect.len() * 128..].iter().all(|&b| b == 0));
    }
}

#[test]
fn mbr_two_partitions() {
    let dir = tempfile::tempdir().unwrap();
    let a = volume(dir.path(), "a.img", FsType::Fat32, 40 << 20);
    let b = volume(dir.path(), "b.img", FsType::Ext4, 20 << 20);
    let out = dir.path().join("mbr.img");
    let mut spec = DiskSpec::new(Scheme::Mbr, vec![PartSpec::new(&a), PartSpec::new(&b)]);
    spec.seed = 7;
    spec.total_size = Some(1 << 30);
    let rep = build(&spec, &out).unwrap();
    assert_eq!(rep.total_size, 1 << 30);
    assert_eq!(rep.partitions.len(), 2);
    assert_eq!(rep.partitions[0].0, 1 << 20);
    assert!(rep.partitions[1].0 >= rep.partitions[0].0 + rep.partitions[0].1);
    assert_eq!(rep.partitions[1].0 % (1 << 20), 0);
    let mbr = read(&out, 0, 512);
    assert_eq!(&mbr[510..512], &[0x55, 0xAA]);
    assert_eq!(le32(&mbr, 0x1B8), rep.mbr_signature);
    // FAT32 -> 0x0C, ext4 -> 0x83; LBA start/count match the report.
    assert_eq!(mbr[0x1BE + 4], 0x0C);
    assert_eq!(mbr[0x1CE + 4], 0x83);
    for (i, &(start, size)) in rep.partitions.iter().enumerate() {
        let o = 0x1BE + 16 * i;
        assert_eq!(le32(&mbr, o + 8) as u64, start / 512);
        assert_eq!(le32(&mbr, o + 12) as u64, size / 512);
    }
    assert!(
        mbr[0x1DE..0x1FE].iter().all(|&x| x == 0),
        "entries 3 and 4 empty"
    );
    // Partition contents equal the source images.
    for (p, src) in rep.partitions.iter().zip([&a, &b]) {
        let want = std::fs::read(src).unwrap();
        assert_bytes!(read(&out, p.0, want.len()), want);
    }
    // The file is sparse.
    let blocks = std::fs::metadata(&out).unwrap().blocks() * 512;
    assert!(blocks < 8 << 20, "allocated {blocks} of 1 GiB");
    if !skip_unless(&["sfdisk"]) {
        let (ok, text) = check("sfdisk", &["--list", out.to_str().unwrap()]);
        assert!(ok && text.contains("Disklabel type: dos"), "{text}");
        assert!(
            text.contains(&format!("0x{:08x}", rep.mbr_signature)),
            "{text}"
        );
    }
    // Same seed, same bytes.
    let out2 = dir.path().join("mbr2.img");
    let rep2 = build(&spec, &out2).unwrap();
    assert_eq!(rep2, rep);
    assert_bytes!(read(&out, 0, 1 << 20), read(&out2, 0, 1 << 20));
}

#[test]
fn gpt_headers_and_arrays_are_valid() {
    let dir = tempfile::tempdir().unwrap();
    let a = volume(dir.path(), "a.img", FsType::Fat32, 40 << 20);
    let b = volume(dir.path(), "b.img", FsType::Ext4, 20 << 20);
    let out = dir.path().join("gpt.img");
    let mut p1 = PartSpec::new(&a);
    p1.name = Some("Windows data".into());
    let spec = DiskSpec {
        seed: 3,
        ..DiskSpec::new(Scheme::Gpt, vec![p1, PartSpec::new(&b)])
    };
    let rep = build(&spec, &out).unwrap();
    check_gpt(&out, 512, &rep.partitions);
    assert_eq!(rep.partition_guids.len(), 2);
    assert_ne!(rep.disk_guid, [0u8; 16]);
    // Type GUIDs: basic data for FAT, Linux filesystem for ext4 (on-disk order).
    let arr = read(&out, 2 * 512, 256);
    let disk_order = |g: [u8; 16]| diskrescue_fixtures::disk::guid_to_disk(g);
    assert_eq!(arr[..16], disk_order(GUID_BASIC_DATA));
    assert_eq!(arr[128..144], disk_order(GUID_LINUX_FS));
    // Name is UTF-16LE.
    assert_eq!(&arr[56..58], &[b'W', 0]);
    // Contents.
    for (p, src) in rep.partitions.iter().zip([&a, &b]) {
        let want = std::fs::read(src).unwrap();
        assert_bytes!(read(&out, p.0, want.len()), want);
    }
    if !skip_unless(&["sgdisk"]) {
        let (ok, text) = check("sgdisk", &["-v", out.to_str().unwrap()]);
        assert!(ok && text.contains("No problems found"), "{text}");
        let (_, p) = check("sgdisk", &["-p", out.to_str().unwrap()]);
        assert!(p.contains("8300"), "Linux filesystem code:\n{p}");
        assert!(p.contains("0700"), "Microsoft basic data code:\n{p}");
    }
    // Different seed, different GUIDs.
    let out2 = dir.path().join("gpt2.img");
    let rep2 = build(
        &DiskSpec {
            seed: 4,
            ..spec.clone()
        },
        &out2,
    )
    .unwrap();
    assert_ne!(rep2.disk_guid, rep.disk_guid);
    assert_eq!(rep2.partitions, rep.partitions);
}

#[test]
fn gpt_4096_sectors_and_explicit_start() {
    let dir = tempfile::tempdir().unwrap();
    let a = volume(dir.path(), "a.img", FsType::Fat16, 20 << 20);
    let out = dir.path().join("gpt4k.img");
    let mut p = PartSpec::new(&a);
    p.start = Some(4 << 20);
    p.type_hint = Some(PartType::Gpt(
        guid_from_str("0FC63DAF-8483-4772-8E79-3D69D8477DE4").unwrap(),
    ));
    let spec = DiskSpec {
        sector: 4096,
        ..DiskSpec::new(Scheme::Gpt, vec![p])
    };
    let rep = build(&spec, &out).unwrap();
    assert_eq!(rep.partitions[0].0, 4 << 20);
    check_gpt(&out, 4096, &rep.partitions);
    let arr = read(&out, 2 * 4096, 16);
    assert_eq!(arr, diskrescue_fixtures::disk::guid_to_disk(GUID_LINUX_FS));
}

#[test]
fn superfloppy_is_the_image_itself() {
    let dir = tempfile::tempdir().unwrap();
    let a = volume(dir.path(), "a.img", FsType::Fat16, 20 << 20);
    let out = dir.path().join("flat.img");
    // Struct-literal construction with `..Default::default()` works too.
    let spec = DiskSpec {
        scheme: Scheme::None,
        parts: vec![PartSpec {
            fs_image: a.clone(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let rep = build(&spec, &out).unwrap();
    assert_eq!(rep.partitions, vec![(0, 20 << 20)]);
    assert_bytes!(std::fs::read(&out).unwrap(), std::fs::read(&a).unwrap());
}

#[test]
fn bad_layouts_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let a = volume(dir.path(), "a.img", FsType::Fat16, 20 << 20);
    let out = dir.path().join("bad.img");
    let mut p1 = PartSpec::new(&a);
    p1.start = Some(1 << 20);
    let mut p2 = PartSpec::new(&a);
    p2.start = Some(2 << 20); // inside p1
    assert!(build(&DiskSpec::new(Scheme::Mbr, vec![p1, p2]), &out).is_err());
    let mut low = PartSpec::new(&a);
    low.start = Some(512); // GPT arrays live there
    assert!(build(&DiskSpec::new(Scheme::Gpt, vec![low]), &out).is_err());
    assert!(build(
        &DiskSpec::new(
            Scheme::Mbr,
            vec![PartSpec::new(dir.path().join("missing.img"))]
        ),
        &out
    )
    .is_err());
}
