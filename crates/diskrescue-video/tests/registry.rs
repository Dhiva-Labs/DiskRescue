//! The set of formats as a whole: registry behaviour and cross-format
//! misattribution (the carver offers every candidate to every format whose
//! signature matches, so a format must say "no" to the others' files).

mod common;

use common::*;
use diskrescue_core::format::FormatRegistry;
use diskrescue_video::{formats, EXTENSION_KEY};

fn registry() -> FormatRegistry {
    let mut r = FormatRegistry::new();
    for f in formats() {
        r.register(f);
    }
    r
}

/// Every available fixture with the id of the format that owns it.
fn corpus() -> Vec<(&'static str, Vec<u8>, &'static str)> {
    let mut v: Vec<(&'static str, Vec<u8>, &'static str)> = Vec::new();
    let mut add = |name: &'static str, f: Option<std::sync::Arc<Vec<u8>>>, id: &'static str| {
        if let Some(f) = f {
            v.push((name, f.to_vec(), id));
        }
    };
    add(
        "avi mpeg4+pcm",
        need_encoders(&["mpeg4", "pcm_s16le"]).then(|| {
            fixture("avi_mpeg4_pcm", || {
                av(
                    &["-t", "3", "-c:v", "mpeg4", "-q:v", "5", "-c:a", "pcm_s16le"],
                    "o.avi",
                )
            })
        }),
        "avi",
    );
    add(
        "avi mpeg4+mp3",
        need_encoders(&["mpeg4", "libmp3lame"]).then(|| {
            fixture("avi_mpeg4_mp3", || {
                av(
                    &[
                        "-t",
                        "2",
                        "-c:v",
                        "mpeg4",
                        "-q:v",
                        "5",
                        "-c:a",
                        "libmp3lame",
                    ],
                    "o.avi",
                )
            })
        }),
        "avi",
    );
    add("mkv", mkv_h264_aac(), "mkv");
    add("webm", webm_vp9_opus(), "mkv");
    add("mkv live", mkv_live(), "mkv");
    add("ts", ts_h264_aac(), "mts");
    add("ts mpeg2", ts_mpeg2_mp2(), "mts");
    add("m2ts", m2ts_native(), "mts");
    add("vob", vob_mpeg2_ac3(), "mpg");
    add("mpg", mpg_mpeg2_mp2(), "mpg");
    add("mp3", mp3_plain(), "mp3");
    add("wav", wav_pcm(), "wav");
    add("flac", flac_stereo(), "flac");
    v
}

#[test]
fn ids_extensions_and_basic_properties() {
    let all = formats();
    let ids: Vec<&str> = all.iter().map(|f| f.id()).collect();
    assert_eq!(ids, ["avi", "mkv", "mts", "mpg", "mp3", "wav", "flac"]);
    let r = registry();
    assert_eq!(r.len(), 7);
    for f in &all {
        assert!(!f.name().is_empty());
        assert!(!f.extensions().is_empty());
        assert!(!f.signatures().is_empty());
        assert!(f.min_size() > 0 && f.min_size() < f.max_size());
        assert!(f.priority() > 0);
    }
    assert_eq!(r.by_extension("AVI").unwrap().id(), "avi");
    assert_eq!(r.by_extension(".webm").unwrap().id(), "mkv");
    assert_eq!(r.by_extension("m2ts").unwrap().id(), "mts");
    assert_eq!(r.by_extension("ts").unwrap().id(), "mts");
    assert_eq!(r.by_extension("vob").unwrap().id(), "mpg");
    assert_eq!(r.by_extension("flac").unwrap().id(), "flac");
    let by_id = |id: &str| r.by_id(id).unwrap();
    assert!(by_id("avi").can_repair());
    for id in ["mkv", "mts", "mpg", "mp3", "wav", "flac"] {
        assert!(!by_id(id).can_repair(), "{id}");
    }
    assert!(r.max_probe_len() >= 197);
    assert_eq!(by_id("mts").extensions()[0], "mts");
    assert_eq!(by_id("mkv").extensions()[0], "mkv");
}

#[test]
fn each_fixture_is_claimed_by_its_own_format_only() {
    let r = registry();
    let corpus = corpus();
    if corpus.is_empty() {
        return;
    }
    for (name, data, owner) in &corpus {
        let src = mem_slice(data);
        let head = &data[..data.len().min(64 * 1024)];
        for f in formats() {
            let m = f.measure(&src, 0, u64::MAX).unwrap();
            if f.id() == *owner {
                let m = m.unwrap_or_else(|| panic!("{name}: {} did not measure it", f.id()));
                assert_eq!(m.len, data.len() as u64, "{name}");
                assert!(
                    m.meta.extra.contains_key(EXTENSION_KEY),
                    "{name}: no extension key"
                );
                assert!(f.probe(head), "{name}: probe rejects its own file");
            } else {
                assert!(m.is_none(), "{name}: {} also claims it: {:?}", f.id(), m);
                // A format whose signature matches must at least be able
                // to say no cheaply; probe may reject or accept.
            }
        }
        // The registry offers the file to its owner.
        let matching: Vec<String> = r
            .matching(head)
            .iter()
            .map(|f| f.id().to_string())
            .collect();
        assert!(
            matching.iter().any(|m| m == owner),
            "{name}: signatures of {owner} do not match (matching: {matching:?})"
        );
    }
}

#[test]
fn files_at_a_sector_offset_inside_a_disk_image() {
    // Concatenate the corpus with sector padding, as a carver would see it,
    // and measure each file at its own offset.
    let corpus = corpus();
    if corpus.is_empty() {
        return;
    }
    let mut image = Vec::new();
    let mut at = Vec::new();
    let mut rng = Rng(77);
    for (name, data, owner) in &corpus {
        while image.len() % 512 != 0 {
            image.push(0);
        }
        image.extend(rng.bytes(4096));
        at.push((image.len() as u64, name, data.len() as u64, *owner));
        image.extend_from_slice(data);
    }
    image.extend(rng.bytes(8192));
    let src = mem(image);
    let all = formats();
    for (off, name, len, owner) in at {
        let f = all.iter().find(|f| f.id() == owner).unwrap();
        let m = f.measure(&src, off, u64::MAX).unwrap().unwrap();
        // The file is followed by another file or noise; every format finds
        // its end, except runs that continue into compatible data.
        assert!(
            m.len == len || (m.len > len && matches!(owner, "mts" | "mp3")),
            "{name}: measured {} expected {len} ({:?})",
            m.len,
            m.status
        );
    }
}

#[test]
fn nothing_panics_on_noise_zeroes_and_ones() {
    let all = formats();
    let mut rng = Rng(0xDEAD);
    let mut inputs: Vec<Vec<u8>> = vec![
        Vec::new(),
        vec![0u8; 100_000],
        vec![0xFFu8; 100_000],
        vec![0x47u8; 100_000],
        b"RIFF".repeat(5000),
    ];
    for n in [1usize, 2, 3, 4, 7, 12, 16, 100, 4096, 70_000] {
        inputs.push(rng.bytes(n));
    }
    for input in &inputs {
        let src = mem_slice(input);
        for f in &all {
            let _ = f.probe(input);
            if let Ok(Some(m)) = f.measure(&src, 0, u64::MAX) {
                assert!(m.len <= input.len() as u64, "{}", f.id());
            }
            let _ = f.validate(&src);
            // Measuring from a non-zero start and with a small limit.
            let _ = f.measure(&src, 3, u64::MAX);
            let _ = f.measure(&src, 0, 100);
            let _ = f.measure(&src, input.len() as u64 + 10, 10);
        }
    }
}

#[test]
fn measure_never_reads_beyond_start_plus_limit() {
    use std::sync::atomic::Ordering;
    let corpus = corpus();
    if corpus.is_empty() {
        return;
    }
    let all = formats();
    for (name, data, owner) in &corpus {
        let f = all.iter().find(|f| f.id() == *owner).unwrap();
        // Embed the file at an offset inside a larger buffer.
        let mut big = vec![0x5Au8; 10_000];
        big.extend_from_slice(data);
        big.extend(vec![0x33u8; 50_000]);
        for limit in [data.len() as u64 / 2, data.len() as u64 / 5, 5000] {
            let counting = std::sync::Arc::new(Counting::new(mem_slice(&big)));
            let shared: diskrescue_core::SharedSource = counting.clone();
            let m = f.measure(&shared, 10_000, limit).unwrap();
            if let Some(m) = m {
                assert!(m.len <= limit, "{name}: len {} limit {limit}", m.len);
            }
            let end = counting.max_end.load(Ordering::Relaxed);
            assert!(
                end <= 10_000 + limit,
                "{name} ({owner}): read up to {end}, limit ends at {}",
                10_000 + limit
            );
        }
    }
}
