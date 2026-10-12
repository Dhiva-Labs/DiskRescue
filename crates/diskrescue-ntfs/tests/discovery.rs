//! Rebuilding NTFS volumes whose boot sectors are both destroyed, from the
//! MFT alone (`find_volumes` + `open_found`).

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use common::*;
use diskrescue_core::fs::FileSystem;
use diskrescue_core::source::MemSource;
use diskrescue_core::{Range, SharedSource};
use diskrescue_ntfs::{find_volumes, open_found, NtfsOptions};

struct Vol {
    bytes: Vec<u8>,
    files: BTreeMap<String, Vec<u8>>,
    cluster: u64,
    record_size: u64,
    total_clusters: u64,
    mft_offset: u64,
    mirror_offset: u64,
    bps: u64,
}

fn volume(dir: &Path, tag: &str, cluster: u32, extra: &[&str]) -> Vol {
    let img = dir.join(format!("{tag}.img"));
    mkntfs(&img, 12, cluster, "LOST", extra);
    let mut files = BTreeMap::new();
    for (i, size) in [100usize, 3000, 70_000, 400_001].iter().enumerate() {
        let name = format!("{tag}_{i}.jpg");
        let bytes = data(9000 + i as u64 + cluster as u64, *size);
        let p = dir.join(&name);
        write(&p, &bytes);
        ntfscp(&img, &p, &format!("/{name}"));
        files.insert(name, bytes);
    }
    let bytes = std::fs::read(&img).unwrap();
    let fs = open_mem(bytes.clone(), &NtfsOptions::default()).unwrap();
    let bs = fs.boot_sector().clone();
    Vol {
        files,
        cluster: bs.cluster_size as u64,
        record_size: fs.record_size(),
        total_clusters: bs.total_clusters(),
        mft_offset: bs.mft_lcn * bs.cluster_size as u64,
        mirror_offset: bs.mftmirr_lcn * bs.cluster_size as u64,
        bps: bs.bytes_per_sector as u64,
        bytes,
    }
}

/// Destroy both boot sector copies.
fn wipe_boot(v: &mut Vol) {
    let bps = v.bps as usize;
    v.bytes[..bps].fill(0);
    let n = v.bytes.len();
    v.bytes[n - bps..].fill(0);
}

fn junk(len: usize, seed: u64) -> Vec<u8> {
    data(seed, len)
}

fn check_found(disk: &[u8], start: u64, v: &Vol, mirror: bool) {
    let src: SharedSource = Arc::new(MemSource::new(disk.to_vec()));
    let found = find_volumes(&src, Range::new(0, disk.len() as u64)).unwrap();
    let f = found
        .iter()
        .find(|f| f.start == start)
        .unwrap_or_else(|| panic!("no volume at {start}: {found:?}"));
    assert_eq!(f.boot.cluster_size as u64, v.cluster, "{:?}", f.evidence);
    assert_eq!(f.boot.mft_record_size as u64, v.record_size);
    assert_eq!(
        f.boot.total_clusters(),
        v.total_clusters,
        "{:?}",
        f.evidence
    );
    assert_eq!(f.size, v.total_clusters * v.cluster);
    assert_eq!(f.from_mirror, mirror, "{:?}", f.evidence);
    let expect_pos = start
        + if mirror {
            v.mirror_offset
        } else {
            v.mft_offset
        };
    assert_eq!(f.record0_offset, expect_pos);
    let fs = open_found(src.clone(), f, &NtfsOptions::default()).unwrap();
    assert_eq!(fs.info().cluster_size as u64, v.cluster);
    let all = entries(&fs);
    for (name, bytes) in &v.files {
        let e = find(&all, name);
        let (got, rep) = read(&fs, e);
        assert!(got == *bytes, "{name}");
        assert!(rep.is_complete(), "{name}: {:?}", rep.notes);
    }
    // The free-space map is available for the carver.
    assert!(fs.unallocated().unwrap().total_len() > 0);
}

#[test]
fn volume_without_boot_sectors_is_rebuilt_from_the_mft() {
    if !have(&["mkntfs", "ntfscp"]) {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    for (tag, cluster, extra) in [
        ("c4k", 4096u32, vec![]),
        ("c512", 512, vec![]),
        ("c2k", 2048, vec![]),
        ("c64k", 65536, vec![]),
        ("s4k", 4096, vec!["-s", "4096"]),
    ] {
        let mut v = volume(tmp.path(), tag, cluster, &extra);
        wipe_boot(&mut v);
        // The damaged volume does not open by itself.
        assert!(open_mem(v.bytes.clone(), &NtfsOptions::default()).is_err());
        // Inside a disk, at an odd sector offset, between noise.
        let lead = (1 << 20) + 3 * 512;
        let mut disk = junk(lead, 1);
        disk.extend_from_slice(&v.bytes);
        disk.extend(junk(1 << 20, 2));
        check_found(&disk, lead as u64, &v, false);

        // Record 0 in the MFT destroyed too: the mirror copy is enough.
        let m = v.mft_offset as usize;
        let rs = v.record_size as usize;
        let mut only_mirror = disk.clone();
        only_mirror[lead + m..lead + m + rs].fill(0);
        check_found(&only_mirror, lead as u64, &v, true);
    }
}

#[test]
fn two_lost_volumes_on_one_disk() {
    if !have(&["mkntfs", "ntfscp"]) {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let mut a = volume(tmp.path(), "a", 4096, &[]);
    let mut b = volume(tmp.path(), "b", 1024, &[]);
    wipe_boot(&mut a);
    wipe_boot(&mut b);
    let mut disk = vec![0u8; 1 << 20];
    disk.extend_from_slice(&a.bytes);
    let b_start = disk.len() + (1 << 20);
    disk.extend(vec![0u8; 1 << 20]);
    disk.extend_from_slice(&b.bytes);
    let src: SharedSource = Arc::new(MemSource::new(disk.clone()));
    let found = find_volumes(&src, Range::new(0, disk.len() as u64)).unwrap();
    let starts: Vec<u64> = found.iter().map(|f| f.start).collect();
    assert_eq!(starts, vec![1 << 20, b_start as u64], "{found:?}");
    check_found(&disk, 1 << 20, &a, false);
    check_found(&disk, b_start as u64, &b, false);
    // A scan window that excludes the first volume's MFT finds only the
    // second.
    let found = find_volumes(&src, Range::new(b_start as u64, disk.len() as u64)).unwrap();
    assert_eq!(found.len(), 1);
}

#[test]
fn noise_and_lookalikes_find_nothing() {
    let mut seed = 0xFEED_F00D_u64;
    let mut rnd = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mut disk: Vec<u8> = (0..(4 << 20)).map(|_| rnd() as u8).collect();
    // Plant record-0 lookalikes: FILE, self number 0, "$MFT" in UTF-16,
    // random everything else.
    for k in 0..64 {
        let at = (k * 61 * 512) % (disk.len() - 4096);
        let at = at - at % 512;
        disk[at..at + 4].copy_from_slice(b"FILE");
        disk[at + 4..at + 6].copy_from_slice(&0x30u16.to_le_bytes());
        disk[at + 6..at + 8].copy_from_slice(&3u16.to_le_bytes());
        disk[at + 0x1C..at + 0x20].copy_from_slice(&1024u32.to_le_bytes());
        disk[at + 0x2C..at + 0x30].copy_from_slice(&0u32.to_le_bytes());
        let name: Vec<u8> = "$MFT"
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        disk[at + 0x100..at + 0x108].copy_from_slice(&name);
    }
    let src: SharedSource = Arc::new(MemSource::new(disk.clone()));
    let found = find_volumes(&src, Range::new(0, disk.len() as u64)).unwrap();
    assert!(found.is_empty(), "{found:?}");
    // Absurd ranges.
    assert!(find_volumes(&src, Range::new(u64::MAX - 10, u64::MAX))
        .unwrap()
        .is_empty());
    assert!(find_volumes(&src, Range::new(5, 5)).unwrap().is_empty());
    // An empty source.
    let empty: SharedSource = Arc::new(MemSource::new(Vec::new()));
    assert!(find_volumes(&empty, Range::new(0, u64::MAX))
        .unwrap()
        .is_empty());
}
