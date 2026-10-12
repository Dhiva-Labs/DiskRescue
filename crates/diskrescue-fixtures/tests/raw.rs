#[macro_use]
mod common;

use common::*;
use diskrescue_core::util::hash::blake3_hex;
use diskrescue_fixtures::raw::{self, RawImage};

fn bytes_of(img: &RawImage, o: &raw::RawObject) -> Vec<u8> {
    read_extents(&img.path, &o.extents)
}

fn check_truth_json(img: &RawImage) {
    let name = img.path.file_stem().unwrap().to_str().unwrap();
    let json =
        std::fs::read_to_string(img.path.with_file_name(format!("{name}.truth.json"))).unwrap();
    let back: RawImage = serde_json::from_str(&json).unwrap();
    assert_eq!(&back, img);
}

#[test]
fn contig_and_unaligned() {
    if skip_unless(&["ffmpeg"]) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let c = raw::contig(dir.path(), 1).unwrap();
    check_truth_json(&c);
    assert!(c.objects.len() >= 15, "{} objects", c.objects.len());
    assert_eq!(c.size, std::fs::metadata(&c.path).unwrap().len());
    let mut prev_end = 0;
    for o in &c.objects {
        assert_eq!(o.status, "valid");
        assert!(o.aligned, "{} at {}", o.name, o.extents[0].0);
        assert!(o.extents[0].0 >= prev_end, "objects do not overlap");
        prev_end = o.extents[0].0 + o.extents[0].1;
        assert_eq!(blake3_hex(&bytes_of(&c, o)), o.blake3, "{}", o.name);
    }
    // The video samples probe clean straight out of the image.
    let mp4 = c
        .objects
        .iter()
        .find(|o| o.name == "video_moovlast.mp4")
        .unwrap();
    assert!(ffprobe_streams(&bytes_of(&c, mp4), "v.mp4").is_some());
    // Same seed, same image.
    let c2 = raw::contig(&dir.path().join("again"), 1).unwrap();
    assert_bytes!(
        std::fs::read(&c.path).unwrap(),
        std::fs::read(&c2.path).unwrap()
    );

    let u = raw::unaligned(dir.path(), 1).unwrap();
    check_truth_json(&u);
    let unaligned = u.objects.iter().filter(|o| !o.aligned).count();
    assert!(
        unaligned >= u.objects.len() - 1,
        "{unaligned} of {}",
        u.objects.len()
    );
    for o in &u.objects {
        assert_eq!(blake3_hex(&bytes_of(&u, o)), o.blake3, "{}", o.name);
    }
}

#[test]
fn nested_lists_inner_pictures() {
    if skip_unless(&["ffmpeg"]) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let n = raw::nested(dir.path(), 2).unwrap();
    check_truth_json(&n);
    let valid: Vec<&str> = n
        .objects
        .iter()
        .filter(|o| o.status == "valid")
        .map(|o| o.name.as_str())
        .collect();
    assert!(
        valid.contains(&"raw.cr2") && valid.contains(&"mjpeg.avi"),
        "{valid:?}"
    );
    let nested: Vec<_> = n.objects.iter().filter(|o| o.status == "nested").collect();
    assert!(nested.len() >= 4, "{} nested", nested.len());
    for o in &nested {
        let b = bytes_of(&n, o);
        assert_eq!(&b[..2], &[0xFF, 0xD8]);
        assert_eq!(&b[b.len() - 2..], &[0xFF, 0xD9]);
        // Fully inside some valid container.
        let (s, l) = o.extents[0];
        assert!(n.objects.iter().any(|p| p.status == "valid"
            && p.extents[0].0 <= s
            && s + l <= p.extents[0].0 + p.extents[0].1));
    }
    for cname in ["cover.mp3", "covr.mp4"] {
        if n.skipped.iter().any(|s| s.starts_with(cname)) {
            eprintln!("skipped: {cname} not writable here");
            continue;
        }
        let c = n
            .objects
            .iter()
            .find(|o| o.name == cname)
            .unwrap_or_else(|| panic!("{cname}"));
        assert!(n
            .objects
            .iter()
            .any(|o| o.name.starts_with(&format!("{cname}#"))));
        assert_eq!(c.status, "valid");
    }
}

#[test]
fn bifrag_fragments_reassemble() {
    let dir = tempfile::tempdir().unwrap();
    let b = raw::bifrag(dir.path(), 3).unwrap();
    check_truth_json(&b);
    let frags: Vec<_> = b
        .objects
        .iter()
        .filter(|o| o.status == "fragment")
        .collect();
    assert_eq!(frags.len(), 28);
    for f in &frags {
        assert_eq!(f.extents.len(), 2);
        assert_eq!(f.extents[0].0 % 4096, 0);
        assert_eq!(f.extents[0].1 % 4096, 0, "split on a cluster boundary");
        let gap = f.extents[1].0 - (f.extents[0].0 + f.extents[0].1);
        assert_eq!(gap % 4096, 0);
        assert!(
            [1u64, 2, 4, 8, 16, 32, 64].contains(&(gap / 4096)),
            "{}: gap {}",
            f.name,
            gap / 4096
        );
        assert_eq!(blake3_hex(&bytes_of(&b, f)), f.blake3, "{}", f.name);
        // The first fragment alone is a truncated JPEG.
        let first = read_extents(&b.path, &f.extents[..1]);
        assert_eq!(&first[..2], &[0xFF, 0xD8]);
    }
    // The "newer file" is a complete JPEG inside the gap.
    let newer: Vec<_> = b
        .objects
        .iter()
        .filter(|o| o.name.starts_with("newer_gap"))
        .collect();
    assert_eq!(newer.len(), 7);
    for n in newer {
        let d = bytes_of(&b, n);
        assert_eq!(&d[..2], &[0xFF, 0xD8]);
        assert_eq!(&d[d.len() - 2..], &[0xFF, 0xD9]);
    }
    let off_grid = b
        .objects
        .iter()
        .find(|o| o.name == "split_off_grid.jpg")
        .unwrap();
    assert_eq!(off_grid.status, "truncated");
    assert_ne!(off_grid.extents[0].1 % 4096, 0);
}

#[test]
fn truncated_holes_weak_fake_motion() {
    if skip_unless(&["ffmpeg", "ffprobe"]) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();

    let t = raw::truncated(dir.path(), 4).unwrap();
    check_truth_json(&t);
    assert!(t.objects.iter().all(|o| o.status == "truncated"));
    for o in &t.objects {
        assert_eq!(blake3_hex(&bytes_of(&t, o)), o.blake3);
    }

    let h = raw::holes(dir.path(), 5).unwrap();
    check_truth_json(&h);
    let map = std::fs::read_to_string(h.mapfile.as_ref().unwrap()).unwrap();
    let bad: Vec<(u64, u64)> = map
        .lines()
        .filter(|l| !l.starts_with('#') && l.ends_with(" -"))
        .map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            (
                u64::from_str_radix(&f[0][2..], 16).unwrap(),
                u64::from_str_radix(&f[1][2..], 16).unwrap(),
            )
        })
        .collect();
    assert!(!bad.is_empty());
    let img = std::fs::read(&h.path).unwrap();
    for &(s, l) in &bad {
        assert!(
            img[s as usize..(s + l) as usize].iter().all(|&b| b == 0),
            "hole zeroed"
        );
        let owner = h
            .objects
            .iter()
            .find(|o| o.extents[0].0 <= s && s < o.extents[0].0 + o.extents[0].1)
            .unwrap();
        assert_eq!(owner.unreadable_bytes, 4096);
    }
    // The mapfile covers the image exactly once, in order.
    let mut pos = 0u64;
    for l in map.lines().filter(|l| {
        !l.starts_with('#')
            && l.split_whitespace()
                .nth(1)
                .is_some_and(|f| f.starts_with("0x"))
    }) {
        let f: Vec<&str> = l.split_whitespace().collect();
        let (s, n) = (
            u64::from_str_radix(&f[0][2..], 16).unwrap(),
            u64::from_str_radix(&f[1][2..], 16).unwrap(),
        );
        assert_eq!(s, pos);
        pos += n;
    }
    assert_eq!(pos, h.size);

    let w = raw::weak_sigs(dir.path(), 6, 1 << 20).unwrap();
    assert!(w.objects.is_empty());
    assert_eq!(w.size, 1 << 20);

    let f = raw::fake_capacity(dir.path(), 7).unwrap();
    check_truth_json(&f);
    let half = f.objects.len() / 2;
    for i in 0..half {
        let d = &f.objects[half + i];
        assert_eq!(d.status, "duplicate");
        assert_eq!(d.duplicate_of, Some(i));
        assert_bytes!(bytes_of(&f, d), bytes_of(&f, &f.objects[i]));
    }

    let m = raw::motion_photo(dir.path(), 8).unwrap();
    check_truth_json(&m);
    assert_eq!(m.objects.len(), 1);
    let o = &m.objects[0];
    let data = bytes_of(&m, o);
    assert!(data.windows(19).any(|w| w == b"GCamera:MicroVideo="));
    // The MP4 starts right after the JPEG's EOI.
    let eoi = data.windows(2).position(|w| w == [0xFF, 0xD9]).unwrap() + 2;
    assert_eq!(&data[eoi + 4..eoi + 8], b"ftyp");
    assert!(ffprobe_streams(&data[eoi..], "m.mp4").is_some());
}
