//! Interop with files written by Pillow (skipped when python3 + Pillow are
//! not installed).

mod common;

use std::path::Path;
use std::process::Command;

use common::*;
use diskrescue_core::format::{
    ExtentStatus, FileFormat, RepairContext, RepairResult, ValidationStatus,
};
use diskrescue_core::source::{FileSource, SharedSource};
use diskrescue_png::{lenient, PngFormat};

const SCRIPT: &str = r#"
import random, sys
from PIL import Image, PngImagePlugin
out = sys.argv[1]
random.seed(7)

def noise(mode, w, h, bands):
    px = bytearray()
    for y in range(h):
        for x in range(w):
            for b in range(bands):
                px.append((x * 3 + y * 5 + b * 40 + random.randrange(40)) & 255)
    return Image.frombytes(mode, (w, h), bytes(px))

rgb = noise("RGB", 200, 150, 3)
rgb.save(f"{out}/rgb.png")
rgb.save(f"{out}/rgb_opt.png", optimize=True, compress_level=9)
noise("RGBA", 120, 90, 4).save(f"{out}/rgba.png")
noise("L", 133, 71, 1).save(f"{out}/gray.png")
noise("LA", 64, 48, 2).save(f"{out}/gray_alpha.png")
rgb.convert("P", palette=Image.ADAPTIVE, colors=64).save(f"{out}/palette.png")
rgb.convert("1").save(f"{out}/bilevel.png")
big = Image.new("I;16", (90, 60))
big.putdata([(i * 257 + random.randrange(256)) & 0xFFFF for i in range(90 * 60)])
big.save(f"{out}/gray16.png")
info = PngImagePlugin.PngInfo()
info.add_text("Creation Time", "2024-05-06T07:08:09Z")
info.add_text("Software", "Pillow test")
info.add_itxt("Description", "café", lang="fr", tkey="Description")
info.add_text("Comment", "compressed text " * 20, zip=True)
ex = Image.Exif()
ex[0x010F] = "DiskRescue"
ex[0x0110] = "FixtureCam"
ex[0x0112] = 6
ex.get_ifd(0x8769)[0x9003] = "2023:07:14 09:05:59"
rgb.save(f"{out}/meta.png", pnginfo=info, exif=ex.tobytes(), dpi=(300, 300))
print("ok")
"#;

fn have_pillow() -> bool {
    Command::new("python3")
        .args(["-c", "import PIL"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn generate(dir: &Path) -> bool {
    if !have_pillow() {
        eprintln!("skipped: python3 with Pillow missing");
        return false;
    }
    let out = Command::new("python3")
        .arg("-c")
        .arg(SCRIPT)
        .arg(dir)
        .output()
        .expect("run python3");
    assert!(
        out.status.success(),
        "pillow script failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    true
}

#[test]
fn pillow_files_measure_validate_and_decode_identically() {
    let dir = tempfile::tempdir().unwrap();
    if !generate(dir.path()) {
        return;
    }
    let mut seen = 0;
    for name in [
        "rgb",
        "rgb_opt",
        "rgba",
        "gray",
        "gray_alpha",
        "palette",
        "bilevel",
        "gray16",
        "meta",
    ] {
        let path = dir.path().join(format!("{name}.png"));
        let bytes = std::fs::read(&path).unwrap();
        let src: SharedSource = std::sync::Arc::new(FileSource::open(&path).unwrap());
        let m = PngFormat.measure(&src, 0, u64::MAX).unwrap().unwrap();
        assert_eq!(m.len, bytes.len() as u64, "{name}");
        assert_eq!(m.status, ExtentStatus::Complete, "{name}");
        let v = PngFormat.validate(&src).unwrap();
        assert_eq!(
            v.status,
            ValidationStatus::Valid,
            "{name}: {:?}",
            v.problems
        );
        // Lenient decode == png crate decode.
        let reference = png_decode(&bytes).unwrap();
        let img = lenient::decode(&bytes).unwrap();
        assert_eq!(img.pixels, reference.data, "{name}");
        assert_eq!((img.width, img.height), (reference.width, reference.height));
        // Cut in half and repair: the png crate must accept the result and
        // the recovered rows must match.
        let half = bytes[..bytes.len() / 2].to_vec();
        let mut out = Vec::new();
        let o = PngFormat
            .repair(&mem(half.clone()), &mut out, &RepairContext::default())
            .unwrap();
        assert_eq!(o.result, RepairResult::PartiallyRepaired, "{name}");
        let d = png_decode(&out).unwrap();
        assert_eq!(
            (d.width, d.height),
            (reference.width, reference.height),
            "{name}"
        );
        let rec = lenient::decode(&half).unwrap().rows_recovered as usize;
        let rb = d.data.len() / d.height as usize;
        assert!(rec > 0, "{name}");
        assert_eq!(&d.data[..rec * rb], &reference.data[..rec * rb], "{name}");
        seen += 1;
    }
    assert_eq!(seen, 9);
}

#[test]
fn pillow_metadata_is_extracted() {
    let dir = tempfile::tempdir().unwrap();
    if !generate(dir.path()) {
        return;
    }
    let path = dir.path().join("meta.png");
    let src: SharedSource = std::sync::Arc::new(FileSource::open(&path).unwrap());
    let m = PngFormat.measure(&src, 0, u64::MAX).unwrap().unwrap();
    // EXIF DateTimeOriginal wins over the text chunk.
    assert_eq!(m.meta.created.unwrap().to_rfc3339(), "2023-07-14T09:05:59Z");
    assert_eq!(m.meta.make.as_deref(), Some("DiskRescue"));
    assert_eq!(m.meta.model.as_deref(), Some("FixtureCam"));
    assert_eq!(m.meta.orientation, Some(6));
    assert_eq!(m.meta.software.as_deref(), Some("Pillow test"));
    assert_eq!((m.meta.width, m.meta.height), (Some(200), Some(150)));
}

#[test]
fn pillow_files_with_bad_crcs_are_repaired_identically() {
    let dir = tempfile::tempdir().unwrap();
    if !generate(dir.path()) {
        return;
    }
    for name in ["rgb", "palette", "meta", "gray16"] {
        let bytes = std::fs::read(dir.path().join(format!("{name}.png"))).unwrap();
        let reference = png_decode(&bytes).unwrap();
        let mut bad = bytes.clone();
        for kind in [b"IHDR", b"IDAT"] {
            let c = find(&bad, kind, 0);
            bad[c.crc_at() + 2] ^= 0x08;
        }
        let mut out = Vec::new();
        let o = PngFormat
            .repair(&mem(bad), &mut out, &RepairContext::default())
            .unwrap();
        assert_eq!(o.result, RepairResult::Repaired, "{name}: {:?}", o.notes);
        assert_eq!(out, bytes, "{name}");
        assert_eq!(png_decode(&out).unwrap().data, reference.data);
    }
}
