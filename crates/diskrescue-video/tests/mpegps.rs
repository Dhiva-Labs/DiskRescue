//! MPEG program streams (MPG and DVD VOB).

mod common;

use common::*;
use diskrescue_core::format::{ExtentStatus, FileFormat, ValidationStatus};
use diskrescue_video::MpegPsFormat;

fn measure(data: &[u8]) -> Option<diskrescue_core::format::Measured> {
    MpegPsFormat.measure(&mem_slice(data), 0, u64::MAX).unwrap()
}

#[test]
fn vob_with_ac3_is_labelled_vob_and_exact() {
    let Some(f) = vob_mpeg2_ac3() else { return };
    assert_eq!(f.len() % 2048, 0);
    let data = with_trailer(&f, 20_000, 6);
    let m = measure(&data).expect("recognised");
    assert_eq!(m.len, f.len() as u64);
    // ffmpeg's vob muxer writes no program end code.
    assert_eq!(m.status, ExtentStatus::UnknownLength);
    assert_eq!(m.err_offset, None);
    assert_eq!(m.meta.extra["extension"], "vob");
    assert_eq!(m.meta.extra["system"], "mpeg2-ps");
    assert_eq!(m.meta.video_codec.as_deref(), Some("mpeg2video"));
    assert_eq!(m.meta.audio_codec.as_deref(), Some("ac3"));
    assert_eq!(m.meta.width, Some(320));
    assert_eq!(m.meta.height, Some(240));
    assert_eq!(m.meta.extra["fps"], "30.000");
    let d = m.meta.duration_ms.unwrap();
    assert!((2700..=3300).contains(&d), "duration {d}");
    assert_eq!(m.meta.extra["packs"], (f.len() / 2048).to_string());
}

#[test]
fn program_stream_without_dvd_markers_is_mpg() {
    for (f, vc) in [
        (vob_mpeg2_mp2(), "mpeg2video"),
        (mpg_mpeg2_mp2(), "mpeg2video"),
        (mpg_mpeg1(), "mpeg1video"),
    ] {
        let Some(f) = f else { continue };
        let m = measure(&with_trailer(&f, 5000, 2)).unwrap();
        assert_eq!(m.len, f.len() as u64, "{vc}");
        assert_eq!(m.meta.extra["extension"], "mpg");
        assert_eq!(m.meta.video_codec.as_deref(), Some(vc));
        assert_eq!(m.meta.audio_codec.as_deref(), Some("mp2"));
        assert_eq!(m.meta.width, Some(320));
        assert_eq!(m.meta.height, Some(240));
    }
    if let Some(f) = mpg_mpeg1() {
        let m = measure(&f).unwrap();
        assert_eq!(m.meta.extra["system"], "mpeg1-ps");
    }
}

#[test]
fn cut_vob_ends_at_the_last_complete_pack() {
    let Some(f) = vob_mpeg2_ac3() else { return };
    let cut = &f[..300_001];
    let m = measure(cut).unwrap();
    assert_eq!(m.status, ExtentStatus::Truncated);
    assert_eq!(m.len, 146 * 2048);
    assert!(m.err_offset.unwrap() >= m.len && m.err_offset.unwrap() <= 300_001);
    assert_eq!(m.meta.extra["extension"], "vob");
    // The kept part is a clean stream.
    let again = measure(&cut[..m.len as usize]).unwrap();
    assert_eq!(again.len, m.len);
    assert_eq!(again.status, ExtentStatus::UnknownLength);
    let v = MpegPsFormat.validate(&mem_slice(cut)).unwrap();
    assert_eq!(v.status, ValidationStatus::Damaged);
    let v = MpegPsFormat
        .validate(&mem_slice(&cut[..m.len as usize]))
        .unwrap();
    assert_eq!(v.status, ValidationStatus::Valid, "{:?}", v.problems);
}

#[test]
fn program_end_code_makes_the_stream_complete() {
    let Some(f) = mpg_mpeg2_mp2() else { return };
    let mut d = f.to_vec();
    d.extend_from_slice(&[0, 0, 1, 0xB9]);
    let n = d.len() as u64;
    d.extend(Rng(3).bytes(3000));
    let m = measure(&d).unwrap();
    assert_eq!(m.len, n);
    assert_eq!(m.status, ExtentStatus::Complete);
    assert!(m.confidence.value() >= 90);
    let v = MpegPsFormat.validate(&mem_slice(&d[..n as usize])).unwrap();
    assert_eq!(v.status, ValidationStatus::Valid, "{:?}", v.problems);
}

#[test]
fn damaged_packet_header_is_truncated_at_its_pack() {
    let Some(f) = vob_mpeg2_ac3() else { return };
    let mut d = f.to_vec();
    // Break the PES marker bits of the first packet in pack 50.
    d[50 * 2048 + 14 + 6] = 0x00;
    let m = measure(&d).unwrap();
    assert_eq!(m.status, ExtentStatus::Truncated);
    assert_eq!(m.len, 50 * 2048);
    // Break a pack header's marker bits.
    let mut d = f.to_vec();
    d[70 * 2048 + 4] = 0x00;
    let m = measure(&d).unwrap();
    assert_eq!(m.status, ExtentStatus::Truncated);
    assert_eq!(m.len, 70 * 2048);
}

#[test]
fn healthy_files_validate() {
    for f in [
        vob_mpeg2_ac3(),
        vob_mpeg2_mp2(),
        mpg_mpeg2_mp2(),
        mpg_mpeg1(),
    ]
    .into_iter()
    .flatten()
    {
        let v = MpegPsFormat.validate(&mem_slice(&f)).unwrap();
        assert_eq!(v.status, ValidationStatus::Valid, "{:?}", v.problems);
    }
    let v = MpegPsFormat.validate(&mem(Rng(1).bytes(4096))).unwrap();
    assert_eq!(v.status, ValidationStatus::Invalid);
}

#[test]
fn probe_and_limits() {
    let Some(f) = vob_mpeg2_ac3() else { return };
    let fmt = MpegPsFormat;
    assert!(fmt.probe(&f[..65536]));
    assert!(fmt.signatures().iter().any(|s| s.matches(&f[..64])));
    assert!(!fmt.probe(&[
        0, 0, 1, 0xBA, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0, 0, 1, 0xE0
    ]));
    assert!(!fmt.probe(&Rng(2).bytes(4096)));
    let m = fmt.measure(&mem_slice(&f), 0, 100_000).unwrap().unwrap();
    assert_eq!(m.status, ExtentStatus::Capped);
    assert!(m.len <= 100_000);
    // A pack header followed by garbage is not a stream.
    let mut g = f[..14].to_vec();
    g.extend(Rng(9).bytes(500));
    assert!(measure(&g).is_none());
}

#[test]
fn hostile_input_never_panics() {
    let fmt = MpegPsFormat;
    let mut rng = Rng(0x55);
    let pack: [u8; 14] = [0, 0, 1, 0xBA, 0x44, 0, 4, 0, 4, 1, 0x43, 0x37, 0x8B, 0xF8];
    for _ in 0..300 {
        let n = (14 + rng.next() % 5000) as usize;
        let mut d = rng.bytes(n);
        d[..14].copy_from_slice(&pack);
        if rng.next() % 2 == 0 && n > 24 {
            d[14..18].copy_from_slice(&[0, 0, 1, 0xE0]);
            d[18] = (rng.next() % 4) as u8;
        }
        let s = mem(d);
        let _ = fmt.measure(&s, 0, u64::MAX);
        let _ = fmt.validate(&s);
    }
    let Some(f) = vob_mpeg2_ac3() else { return };
    let mut len = 0usize;
    while len < f.len() {
        let s = mem_slice(&f[..len]);
        if let Some(m) = fmt.measure(&s, 0, u64::MAX).unwrap() {
            assert!(m.len <= len as u64);
        }
        let _ = fmt.validate(&s);
        len += 1 + (rng.next() % 9000) as usize;
    }
    for off in (0..4200).step_by(7) {
        let mut d = f[..8192].to_vec();
        d[off] ^= 0xA5;
        let s = mem(d);
        let _ = fmt.measure(&s, 0, u64::MAX);
    }
}
