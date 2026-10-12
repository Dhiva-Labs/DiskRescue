//! End-to-end tests of the `diskrescue` binary.
//!
//! Sources are always regular files (images); no test touches a real block
//! device. Tests that need ffmpeg (video fixtures) skip with a message when
//! it is missing.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use diskrescue_core::util::hash::blake3_hex;
use diskrescue_fixtures::sets::{self, Recoverability, Scenario};
use rand::rngs::StdRng;
use rand::{RngCore, SeedableRng};
use serde_json::Value;

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Out {
    fn all(&self) -> String {
        format!("{}\n{}", self.stdout, self.stderr)
    }
}

fn command() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_diskrescue"));
    c.env("NO_COLOR", "1")
        .env_remove("RUST_LOG")
        .env("COLUMNS", "120")
        .stdin(Stdio::null());
    c
}

fn run<S: AsRef<OsStr>>(args: &[S]) -> Out {
    let o = command()
        .args(args.iter().map(|a| a.as_ref()))
        .output()
        .expect("cannot start diskrescue");
    Out {
        code: o.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
    }
}

fn random_file(path: &Path, len: usize, seed: u64) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    StdRng::seed_from_u64(seed).fill_bytes(&mut buf);
    std::fs::write(path, &buf).unwrap();
    buf
}

/// The camera-card scenario, built once per test run and cached in the
/// target directory.
fn camera_card() -> Option<&'static Scenario> {
    static CELL: OnceLock<Option<Scenario>> = OnceLock::new();
    CELL.get_or_init(|| {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("fixtures")
            .join("camera-sd-card");
        if let Ok(sc) = Scenario::load(&dir) {
            if sc.disk_image.as_ref().is_some_and(|p| p.exists()) {
                return Some(sc);
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        match sets::camera_sd_card(&dir) {
            Ok(sc) => Some(sc),
            Err(e) if e.is_missing() => {
                eprintln!("skipped: {e}");
                None
            }
            Err(e) => panic!("cannot build the camera-card fixture: {e}"),
        }
    })
    .as_ref()
}

/// Loose damaged files, built once per test run.
fn damaged_media() -> Option<&'static Scenario> {
    static CELL: OnceLock<Option<Scenario>> = OnceLock::new();
    CELL.get_or_init(|| {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("fixtures")
            .join("damaged-media");
        if let Ok(sc) = Scenario::load(&dir) {
            if dir.join("no_moov.mp4").exists() {
                return Some(sc);
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        match sets::damaged_media(&dir) {
            Ok(sc) => Some(sc),
            Err(e) if e.is_missing() => {
                eprintln!("skipped: {e}");
                None
            }
            Err(e) => panic!("cannot build the damaged-media fixture: {e}"),
        }
    })
    .as_ref()
}

fn files_under(dir: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else {
                    out.push(p);
                }
            }
        }
    }
    let mut v = Vec::new();
    walk(dir, &mut v);
    v.sort();
    v
}

fn json(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("not JSON ({e}):\n{text}"))
}

// ------------------------------------------------------------------ help

#[test]
fn help_for_every_subcommand() {
    let top = run(&["--help"]);
    assert_eq!(top.code, 0, "{}", top.all());
    for cmd in [
        "list", "info", "image", "scan", "recover", "carve", "repair", "verify", "map",
    ] {
        assert!(top.stdout.contains(cmd), "top-level help lacks {cmd}");
        let h = run(&[cmd, "--help"]);
        assert_eq!(h.code, 0, "{cmd} --help: {}", h.all());
        assert!(h.stdout.contains("Usage:"), "{cmd}: {}", h.stdout);
        assert!(h.stdout.contains(cmd), "{cmd}: {}", h.stdout);
    }
    let v = run(&["--version"]);
    assert_eq!(v.code, 0);
    assert!(v.stdout.starts_with("diskrescue "), "{}", v.stdout);
}

#[test]
fn help_mentions_the_documented_options() {
    let image = run(&["image", "--help"]).stdout;
    for opt in [
        "--profile",
        "--timeout",
        "--block-size",
        "--retries",
        "--no-trim",
        "--no-scrape",
        "--reverse",
        "--start",
        "--size",
        "--no-tui",
        "--json-progress",
    ] {
        assert!(image.contains(opt), "image --help lacks {opt}");
    }
    let rec = run(&["recover", "--help"]).stdout;
    for opt in [
        "--photos",
        "--videos",
        "--raw",
        "--audio",
        "--all-files",
        "--existing-only",
        "--deleted-only",
        "--min-size",
        "--carve",
        "--donor",
        "--no-repair",
        "--aggressive",
        "--image-to",
        "--jobs",
    ] {
        assert!(rec.contains(opt), "recover --help lacks {opt}");
    }
}

#[test]
fn usage_errors_exit_with_2() {
    for args in [
        vec!["image", "only-one-argument"],
        vec!["recover", "x.img"],
        vec!["image", "a", "b", "--profile", "warp-drive"],
        vec!["image", "a", "b", "--block-size", "enormous"],
        vec![
            "recover",
            "a",
            "-o",
            "b",
            "--existing-only",
            "--deleted-only",
        ],
        vec!["verify"],
        vec!["no-such-command"],
    ] {
        let o = run(&args);
        assert_eq!(o.code, 2, "{args:?}: {}", o.all());
    }
}

// ------------------------------------------------------------------ list / info

#[test]
fn list_json_is_an_array() {
    let o = run(&["list", "--json"]);
    assert_eq!(o.code, 0, "{}", o.all());
    let v = json(&o.stdout);
    assert!(v.is_array(), "{v}");
    for d in v.as_array().unwrap() {
        assert!(d["path"].is_string() && d["kind"].is_string(), "{d}");
    }
    let o = run(&["list", "--all", "--json"]);
    assert_eq!(o.code, 0);
    assert!(json(&o.stdout).is_array());
}

#[test]
fn list_table_runs() {
    let o = run(&["list"]);
    assert_eq!(o.code, 0, "{}", o.all());
}

#[test]
fn info_describes_an_image() {
    let Some(sc) = camera_card() else { return };
    let img = sc.disk_image.as_ref().unwrap();
    let o = run(&[OsStr::new("info"), img.as_os_str()]);
    assert_eq!(o.code, 0, "{}", o.all());
    for needle in [
        "image file",
        "Partition table",
        "fat32",
        "EOS_DIGITAL",
        "Imaging profile",
    ] {
        assert!(o.stdout.contains(needle), "missing {needle}:\n{}", o.stdout);
    }
    let o = run(&[OsStr::new("info"), img.as_os_str(), OsStr::new("--json")]);
    assert_eq!(o.code, 0, "{}", o.all());
    let v = json(&o.stdout);
    assert_eq!(v["device"]["kind"], "image_file");
    assert_eq!(v["partition_table"]["scheme"], "none");
    assert_eq!(v["volumes"][0]["label"], "EOS_DIGITAL");
    assert_eq!(v["profile"]["name"], "generic");
}

#[test]
fn missing_source_is_a_calm_error() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nope.img");
    for cmd in ["info", "scan"] {
        let o = run(&[OsStr::new(cmd), missing.as_os_str()]);
        assert_eq!(o.code, 1, "{cmd}: {}", o.all());
        assert!(o.stderr.contains("does not exist"), "{cmd}: {}", o.stderr);
        assert!(o.stderr.contains("diskrescue list"), "{cmd}: {}", o.stderr);
    }
    let out = dir.path().join("out");
    let o = run(&[
        OsStr::new("recover"),
        missing.as_os_str(),
        OsStr::new("-o"),
        out.as_os_str(),
    ]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(!out.exists(), "nothing may be created for a missing source");
}

#[cfg(unix)]
#[test]
fn unreadable_file_explains_permissions() {
    use std::os::unix::fs::PermissionsExt;
    // Root can read everything; the hint only matters for normal users.
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipped: running as root");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("locked.img");
    std::fs::write(&img, vec![0u8; 4096]).unwrap();
    std::fs::set_permissions(&img, std::fs::Permissions::from_mode(0o000)).unwrap();
    let o = run(&[OsStr::new("scan"), img.as_os_str()]);
    assert_eq!(o.code, 1, "{}", o.all());
    let t = o.stderr.to_lowercase();
    assert!(t.contains("permission"), "{}", o.stderr);
    assert!(t.contains("sudo") || t.contains("ls -l"), "{}", o.stderr);
}

// ------------------------------------------------------------------ image

#[test]
fn image_copies_exactly_and_resumes_instantly() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("source.bin");
    let data = random_file(&src, 16 << 20, 1);
    let img = dir.path().join("copy.img");

    let o = run(&[
        OsStr::new("image"),
        src.as_os_str(),
        img.as_os_str(),
        OsStr::new("--no-tui"),
        OsStr::new("--profile"),
        OsStr::new("generic"),
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert_eq!(
        std::fs::read(&img).unwrap(),
        data,
        "image differs from source"
    );
    assert!(o.stdout.contains("Finished copying"), "{}", o.stdout);
    assert!(
        o.stdout.contains("diskrescue recover"),
        "next step missing:\n{}",
        o.stdout
    );

    // The mapfile is ddrescue's format: one finished block covering everything.
    let map = std::fs::read_to_string(dir.path().join("copy.img.map")).unwrap();
    let blocks: Vec<&str> = map
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .collect();
    // First non-comment line is the status line, the rest are blocks.
    assert_eq!(blocks.len(), 2, "{map}");
    let fields: Vec<&str> = blocks[1].split_whitespace().collect();
    assert_eq!(fields, ["0x00000000", "0x01000000", "+"], "{map}");

    // Run again: nothing to do, and no need to read the source at all.
    let t = Instant::now();
    let o = run(&[
        OsStr::new("image"),
        src.as_os_str(),
        img.as_os_str(),
        OsStr::new("--no-tui"),
        OsStr::new("--profile"),
        OsStr::new("generic"),
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(
        t.elapsed() < Duration::from_secs(5),
        "resume was not instant"
    );
    assert!(
        o.stdout.contains("already completely copied"),
        "{}",
        o.stdout
    );
    assert_eq!(std::fs::read(&img).unwrap(), data);
}

#[test]
fn image_of_a_part_then_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("source.bin");
    let data = random_file(&src, 8 << 20, 2);
    let img = dir.path().join("part.img");
    let o = run(&[
        OsStr::new("image"),
        src.as_os_str(),
        img.as_os_str(),
        OsStr::new("--no-tui"),
        OsStr::new("--profile"),
        OsStr::new("generic"),
        OsStr::new("--start"),
        OsStr::new("1M"),
        OsStr::new("--size"),
        OsStr::new("2M"),
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    let image = std::fs::read(&img).unwrap();
    assert_eq!(&image[1 << 20..3 << 20], &data[1 << 20..3 << 20]);
    assert!(
        image[..1 << 20].iter().all(|b| *b == 0),
        "outside the part must stay empty"
    );

    let m = run(&[
        OsStr::new("map"),
        dir.path().join("part.img.map").as_os_str(),
        OsStr::new("--json"),
    ]);
    assert_eq!(m.code, 0, "{}", m.all());
    let v = json(&m.stdout);
    assert_eq!(v["rescued"], 2 << 20);
    assert_eq!(v["total"], 8 << 20);
    assert_eq!(v["percent_rescued"], 25.0);

    // `map` also accepts the image itself.
    let m = run(&[OsStr::new("map"), img.as_os_str()]);
    assert_eq!(m.code, 0, "{}", m.all());
    assert!(m.stdout.contains("25.0%"), "{}", m.stdout);

    // Running without the limits continues and completes.
    let o = run(&[
        OsStr::new("image"),
        src.as_os_str(),
        img.as_os_str(),
        OsStr::new("--no-tui"),
        OsStr::new("--profile"),
        OsStr::new("generic"),
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert_eq!(std::fs::read(&img).unwrap(), data);
}

#[test]
fn image_never_overwrites_without_a_mapfile() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("source.bin");
    let data = random_file(&src, 1 << 20, 3);
    let img = dir.path().join("existing.img");
    std::fs::write(&img, b"precious data").unwrap();
    let o = run(&[
        OsStr::new("image"),
        src.as_os_str(),
        img.as_os_str(),
        OsStr::new("--no-tui"),
    ]);
    assert_eq!(o.code, 3, "{}", o.all());
    assert!(o.stderr.contains("will not overwrite"), "{}", o.stderr);
    assert!(o.stderr.contains("--force"), "{}", o.stderr);
    assert_eq!(std::fs::read(&img).unwrap(), b"precious data");

    // --force replaces it; leftovers of the old file must not survive.
    let o = run(&[
        OsStr::new("image"),
        src.as_os_str(),
        img.as_os_str(),
        OsStr::new("--no-tui"),
        OsStr::new("--force"),
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert_eq!(std::fs::read(&img).unwrap(), data);
}

#[test]
fn image_refuses_the_source_itself_and_non_files() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("source.bin");
    random_file(&src, 1 << 20, 4);
    let o = run(&[
        OsStr::new("image"),
        src.as_os_str(),
        src.as_os_str(),
        OsStr::new("--no-tui"),
    ]);
    assert_eq!(o.code, 3, "{}", o.all());
    assert!(o.stderr.contains("the source itself"), "{}", o.stderr);

    let o = run(&[
        OsStr::new("image"),
        src.as_os_str(),
        dir.path().as_os_str(),
        OsStr::new("--no-tui"),
    ]);
    assert_eq!(o.code, 2, "{}", o.all());
    assert!(o.stderr.contains("is a folder"), "{}", o.stderr);

    let map = dir.path().join("same");
    let o = run(&[
        OsStr::new("image"),
        src.as_os_str(),
        map.as_os_str(),
        OsStr::new("--map"),
        map.as_os_str(),
        OsStr::new("--no-tui"),
    ]);
    assert_eq!(o.code, 2, "{}", o.all());
    assert_eq!(
        std::fs::read(&src).unwrap().len(),
        1 << 20,
        "the source is untouched"
    );
}

#[test]
fn image_json_progress_is_one_object_per_line() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("source.bin");
    random_file(&src, 4 << 20, 5);
    let img = dir.path().join("j.img");
    let o = run(&[
        OsStr::new("image"),
        src.as_os_str(),
        img.as_os_str(),
        OsStr::new("--json-progress"),
        OsStr::new("--profile"),
        OsStr::new("generic"),
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    let events: Vec<Value> = o.stdout.lines().map(json).collect();
    assert_eq!(events.first().unwrap()["event"], "start");
    let last = events.last().unwrap();
    assert_eq!(last["event"], "finished");
    assert_eq!(last["finished"], true);
    assert_eq!(last["stats"]["rescued"], 4 << 20);
    assert!(events.iter().any(|e| e["event"] == "phase"));
}

#[test]
fn image_option_overrides_are_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("source.bin");
    let data = random_file(&src, 2 << 20, 6);
    let img = dir.path().join("o.img");
    let o = run(&[
        OsStr::new("image"),
        src.as_os_str(),
        img.as_os_str(),
        OsStr::new("--no-tui"),
        OsStr::new("--profile"),
        OsStr::new("hdd"),
        OsStr::new("--timeout"),
        OsStr::new("3"),
        OsStr::new("--block-size"),
        OsStr::new("128k"),
        OsStr::new("--retries"),
        OsStr::new("0"),
        OsStr::new("--no-trim"),
        OsStr::new("--no-scrape"),
        OsStr::new("--reverse"),
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert_eq!(std::fs::read(&img).unwrap(), data);
    assert!(o.stdout.contains("hdd"), "{}", o.stdout);
}

#[cfg(unix)]
#[test]
fn ctrl_c_saves_progress_and_exits_130() {
    // Disk-backed scratch space: the image is preallocated at full size.
    let dir = tempfile::Builder::new()
        .prefix("sigint-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap();
    let src = dir.path().join("big.bin");
    // A sparse 512 MiB source copied in 512-byte reads takes seconds.
    let f = std::fs::File::create(&src).unwrap();
    f.set_len(512 << 20).unwrap();
    drop(f);
    let img = dir.path().join("big.img");
    let map = dir.path().join("big.img.map");
    let child = command()
        .args([
            OsStr::new("image"),
            src.as_os_str(),
            img.as_os_str(),
            OsStr::new("--no-tui"),
            OsStr::new("--profile"),
            OsStr::new("generic"),
            OsStr::new("--block-size"),
            OsStr::new("512"),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Wait until the copy is really running (the mapfile is written first).
    let t = Instant::now();
    while !map.exists() {
        assert!(
            t.elapsed() < Duration::from_secs(20),
            "the copy never started"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(400));
    // SAFETY: plain kill(2) on the child we just started.
    unsafe {
        libc::kill(child.id() as i32, libc::SIGINT);
    }
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(130), "{stdout}\n{stderr}");
    assert!(stdout.contains("Stopped. Progress is saved"), "{stdout}");
    assert!(stdout.contains("run the same command again"), "{stdout}");
    // The map says it is partial; resuming finishes the job.
    let m = run(&[OsStr::new("map"), map.as_os_str(), OsStr::new("--json")]);
    let v = json(&m.stdout);
    assert!(v["rescued"].as_u64().unwrap() < 512 << 20, "{v}");
    assert!(v["rescued"].as_u64().unwrap() > 0, "{v}");
}

// ------------------------------------------------------------------ map

#[test]
fn map_prints_the_right_percentages() {
    let dir = tempfile::tempdir().unwrap();
    let mapfile = dir.path().join("card.map");
    std::fs::write(
        &mapfile,
        "# Mapfile. Created by GNU ddrescue version 1.27\n\
         # Command line: ddrescue -d /dev/sdb card.img card.map\n\
         # Start time:   2024-05-01 10:00:00\n\
         # Current time: 2024-05-01 10:05:00\n\
         # Copying non-tried blocks... Pass 1 (forwards)\n\
         # current_pos  current_status  current_pass\n\
         0x00000200     ?               1\n\
         #      pos        size  status\n\
         0x00000000  0x00000190  +\n\
         0x00000190  0x00000064  -\n\
         0x000001F4  0x000001F4  ?\n",
    )
    .unwrap();
    let o = run(&[
        OsStr::new("map"),
        mapfile.as_os_str(),
        OsStr::new("--width"),
        OsStr::new("40"),
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(o.stdout.contains("40.0% of 1000 B"), "{}", o.stdout);
    let rows: Vec<&str> = o
        .stdout
        .lines()
        .filter(|l| l.contains("%") && l.contains(" B "))
        .collect();
    assert!(
        rows.iter()
            .any(|r| r.contains("copied") && r.contains("40.0%")),
        "{}",
        o.stdout
    );
    assert!(
        rows.iter()
            .any(|r| r.contains("unreadable") && r.contains("10.0%")),
        "{}",
        o.stdout
    );
    assert!(
        rows.iter()
            .any(|r| r.contains("not read yet") && r.contains("50.0%")),
        "{}",
        o.stdout
    );
    let picture = o
        .stdout
        .lines()
        .find(|l| l.chars().all(|c| "+?*/-".contains(c)) && !l.is_empty())
        .expect("picture line");
    assert_eq!(picture.chars().count(), 40);

    let o = run(&[OsStr::new("map"), mapfile.as_os_str(), OsStr::new("--json")]);
    let v = json(&o.stdout);
    assert_eq!(v["total"], 1000);
    assert_eq!(v["rescued"], 400);
    assert_eq!(v["bad"], 100);
    assert_eq!(v["non_tried"], 500);
    assert_eq!(v["bad_areas"], 1);
    assert_eq!(v["percent_rescued"], 40.0);
}

#[test]
fn map_rejects_garbage_and_missing_files() {
    let dir = tempfile::tempdir().unwrap();
    let junk = dir.path().join("junk.map");
    std::fs::write(&junk, "hello world\nthis is not a mapfile\n").unwrap();
    let o = run(&[OsStr::new("map"), junk.as_os_str()]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(o.stderr.contains("not a readable mapfile"), "{}", o.stderr);
    let o = run(&[OsStr::new("map"), dir.path().join("none.map").as_os_str()]);
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("does not exist"), "{}", o.stderr);
}

// ------------------------------------------------------------------ scan / recover / carve

#[test]
fn scan_lists_files_and_writes_nothing() {
    let Some(sc) = camera_card() else { return };
    let img = sc.disk_image.as_ref().unwrap();
    let before = std::fs::metadata(img).unwrap().modified().unwrap();
    let o = run(&[OsStr::new("scan"), img.as_os_str()]);
    assert_eq!(o.code, 0, "{}", o.all());
    for needle in [
        "EOS_DIGITAL",
        "IMG_0001.JPG",
        "deleted",
        "MVI_0003.MP4",
        "photos 8",
    ] {
        assert!(o.stdout.contains(needle), "missing {needle}:\n{}", o.stdout);
    }
    let o = run(&[OsStr::new("scan"), img.as_os_str(), OsStr::new("--json")]);
    assert_eq!(o.code, 0, "{}", o.all());
    let v = json(&o.stdout);
    assert_eq!(v["files"].as_array().unwrap().len(), 11, "{v}");
    assert_eq!(v["volumes"][0]["fs"], "fat32");
    let deleted = v["files"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["deleted"] == true)
        .count();
    assert_eq!(deleted, 2);
    // Filters and limits.
    let o = run(&[
        OsStr::new("scan"),
        img.as_os_str(),
        OsStr::new("--deleted-only"),
        OsStr::new("--limit"),
        OsStr::new("1"),
    ]);
    assert_eq!(o.code, 0);
    assert!(o.stdout.contains("0 existing, 2 deleted"), "{}", o.stdout);
    assert!(o.stdout.contains("and 1 more"), "{}", o.stdout);
    assert_eq!(std::fs::metadata(img).unwrap().modified().unwrap(), before);
}

#[test]
fn scan_with_carve_finds_files_by_content() {
    let Some(sc) = camera_card() else { return };
    let img = sc.disk_image.as_ref().unwrap();
    let o = run(&[
        OsStr::new("scan"),
        img.as_os_str(),
        OsStr::new("--carve"),
        OsStr::new("--json"),
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    let v = json(&o.stdout);
    assert!(!v["carved"].as_array().unwrap().is_empty(), "{v}");
}

#[test]
fn recover_gets_back_every_file_byte_for_byte() {
    let Some(sc) = camera_card() else { return };
    let img = sc.disk_image.as_ref().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    let o = run(&[
        OsStr::new("recover"),
        img.as_os_str(),
        OsStr::new("-o"),
        out.as_os_str(),
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(out.join("report.txt").is_file() && out.join("report.json").is_file());
    assert!(
        o.stdout.contains("report.txt") && o.stdout.contains("report.json"),
        "{}",
        o.stdout
    );
    assert!(o.stdout.contains("Look in deleted/ first"), "{}", o.stdout);

    // Existing files: same path, same bytes.
    let vol_dir = std::fs::read_dir(out.join("files"))
        .unwrap()
        .flatten()
        .next()
        .expect("a volume folder under files/")
        .path();
    assert!(
        vol_dir
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("vol1_fat32"),
        "{}",
        vol_dir.display()
    );
    let mut live = 0;
    for f in sc.files.iter().filter(|f| f.expect == Recoverability::Live) {
        let got = std::fs::read(vol_dir.join(&f.path))
            .unwrap_or_else(|e| panic!("{} missing: {e}", f.path));
        assert_eq!(blake3_hex(&got), f.blake3, "{} differs", f.path);
        live += 1;
    }
    assert!(live >= 9, "expected at least nine live files, saw {live}");

    // Deleted files: a first letter is lost on FAT, so match by content.
    let deleted_hashes: Vec<String> = files_under(&out.join("deleted"))
        .iter()
        .map(|p| blake3_hex(&std::fs::read(p).unwrap()))
        .collect();
    let mut deleted = 0;
    for f in sc.files.iter().filter(|f| f.deleted) {
        assert!(
            deleted_hashes.contains(&f.blake3),
            "deleted {} not recovered intact",
            f.path
        );
        deleted += 1;
    }
    assert_eq!(deleted, 2);

    // The JSON report lists every file that was written.
    let report = json(&std::fs::read_to_string(out.join("report.json")).unwrap());
    let listed: Vec<String> = report["files"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f["output"].as_str().map(String::from))
        .collect();
    for p in files_under(&out) {
        let rel = p
            .strip_prefix(&out)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if rel.starts_with("report.") {
            continue;
        }
        assert!(listed.contains(&rel), "{rel} is not in report.json");
    }
    assert_eq!(report["totals"]["files_written"], 11);
    assert_eq!(report["totals"]["deleted"], 2);
}

#[test]
fn a_second_run_into_the_same_folder_keeps_the_first_report() {
    let Some(sc) = camera_card() else { return };
    let img = sc.disk_image.as_ref().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    let args = [
        OsStr::new("recover"),
        img.as_os_str(),
        OsStr::new("-o"),
        out.as_os_str(),
        OsStr::new("--carve"),
        OsStr::new("off"),
    ];
    assert_eq!(run(&args).code, 0);
    let first = std::fs::read_to_string(out.join("report.json")).unwrap();
    let o = run(&args);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(o.stderr.contains("report.previous"), "{}", o.stderr);
    assert_eq!(
        std::fs::read_to_string(out.join("report.previous.json")).unwrap(),
        first
    );
    // Nothing recovered before was overwritten; the new copies are numbered.
    let files = files_under(&out.join("files"));
    assert_eq!(files.len(), 18, "{files:?}");
}

#[test]
fn recover_selection_flags_narrow_the_result() {
    let Some(sc) = camera_card() else { return };
    let img = sc.disk_image.as_ref().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("videos");
    let o = run(&[
        OsStr::new("recover"),
        img.as_os_str(),
        OsStr::new("-o"),
        out.as_os_str(),
        OsStr::new("--videos"),
        OsStr::new("--existing-only"),
        OsStr::new("--carve"),
        OsStr::new("off"),
        OsStr::new("--json"),
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    let all = files_under(&out);
    let media: Vec<_> = all
        .iter()
        .filter(|p| !p.to_string_lossy().contains("report."))
        .collect();
    assert_eq!(media.len(), 1, "{media:?}");
    assert!(media[0].to_string_lossy().ends_with("MVI_0003.MP4"));
    // --json: every stdout line is JSON and the last is the summary.
    let events: Vec<Value> = o.stdout.lines().map(json).collect();
    assert_eq!(events.last().unwrap()["event"], "finished");
    assert_eq!(events.last().unwrap()["totals"]["files_written"], 1);
}

#[test]
fn recover_uses_the_mapfile_to_flag_damaged_files() {
    let Some(sc) = camera_card() else { return };
    let img = sc.disk_image.as_ref().unwrap();
    let live = sc
        .files
        .iter()
        .find(|f| f.path.ends_with("IMG_0001.JPG"))
        .unwrap();
    let (off, _) = live.extents[0];
    let size = std::fs::metadata(img).unwrap().len();
    let dir = tempfile::tempdir().unwrap();
    let map = dir.path().join("card.map");
    std::fs::write(
        &map,
        diskrescue_fixtures::raw::mapfile_text(size, &[(off + 4096, 4096)]),
    )
    .unwrap();
    let out = dir.path().join("out");
    let o = run(&[
        OsStr::new("recover"),
        img.as_os_str(),
        OsStr::new("-o"),
        out.as_os_str(),
        OsStr::new("--map"),
        map.as_os_str(),
        OsStr::new("--carve"),
        OsStr::new("off"),
        OsStr::new("--no-repair"),
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(o.stderr.contains("could not be copied"), "{}", o.stderr);
    let report = json(&std::fs::read_to_string(out.join("report.json")).unwrap());
    assert_eq!(report["source"]["unreadable_bytes"], 4096);
    let rec = report["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| {
            f["origin"]["path"]
                .as_str()
                .is_some_and(|p| p.ends_with("IMG_0001.JPG"))
        })
        .expect("IMG_0001.JPG in the report");
    assert_eq!(rec["unreadable_bytes"], 4096, "{rec}");
    assert_eq!(rec["outcome"], "damaged", "{rec}");
    assert!(o.stdout.contains("Files needing attention"), "{}", o.stdout);
}

#[test]
fn recover_picks_up_the_mapfile_next_to_the_image() {
    let Some(sc) = camera_card() else { return };
    let src = sc.disk_image.as_ref().unwrap();
    let dir = tempfile::tempdir().unwrap();
    // A small image: the first MiB of the card with a map saying the rest was never copied.
    let img = dir.path().join("partial.img");
    let head = std::fs::read(src).unwrap()[..1 << 20].to_vec();
    std::fs::write(&img, &head).unwrap();
    let o = run(&[OsStr::new("scan"), img.as_os_str()]);
    // No mapfile yet: a plain (truncated) image.
    assert!(!o.stderr.contains("could not be copied"), "{}", o.stderr);
    std::fs::write(
        dir.path().join("partial.img.map"),
        "# Mapfile. Created by test\n# current_pos  current_status  current_pass\n0x00100000  ?  1\n#      pos        size  status\n0x00000000  0x00100000  +\n0x00100000  0x00100000  ?\n",
    )
    .unwrap();
    let o = run(&[OsStr::new("scan"), img.as_os_str()]);
    assert!(o.stderr.contains("could not be copied"), "{}", o.stderr);
    assert!(o.stderr.contains("Imaging did not finish"), "{}", o.stderr);
}

#[test]
fn recover_refuses_to_write_into_the_image() {
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("card.img");
    std::fs::write(&img, vec![0u8; 4096]).unwrap();
    let o = run(&[
        OsStr::new("recover"),
        img.as_os_str(),
        OsStr::new("-o"),
        img.as_os_str(),
    ]);
    assert_eq!(o.code, 3, "{}", o.all());
    assert!(o.stderr.contains("Refused for safety"), "{}", o.stderr);
    assert_eq!(std::fs::read(&img).unwrap(), vec![0u8; 4096]);
    // Any other existing file is not a folder either.
    let other = dir.path().join("other.txt");
    std::fs::write(&other, b"x").unwrap();
    let o = run(&[
        OsStr::new("recover"),
        img.as_os_str(),
        OsStr::new("-o"),
        other.as_os_str(),
    ]);
    assert_eq!(o.code, 3, "{}", o.all());
    assert_eq!(std::fs::read(&other).unwrap(), b"x");
    let o = run(&[
        OsStr::new("carve"),
        img.as_os_str(),
        OsStr::new("-o"),
        img.as_os_str(),
    ]);
    assert_eq!(o.code, 3, "carve: {}", o.all());
}

#[test]
fn recover_with_image_to_runs_both_steps() {
    let Some(sc) = camera_card() else { return };
    let src = sc.disk_image.as_ref().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let copy = dir.path().join("copy.img");
    let out = dir.path().join("out");
    let o = run(&[
        OsStr::new("recover"),
        src.as_os_str(),
        OsStr::new("--image-to"),
        copy.as_os_str(),
        OsStr::new("-o"),
        out.as_os_str(),
        OsStr::new("--no-tui"),
        OsStr::new("--carve"),
        OsStr::new("off"),
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(o.stdout.contains("Finished copying"), "{}", o.stdout);
    assert!(
        o.stdout.contains("Now recovering files from the image"),
        "{}",
        o.stdout
    );
    assert_eq!(std::fs::read(&copy).unwrap(), std::fs::read(src).unwrap());
    assert!(dir.path().join("copy.img.map").is_file());
    let report = json(&std::fs::read_to_string(out.join("report.json")).unwrap());
    assert_eq!(report["totals"]["files_written"], 11);
    assert_eq!(
        report["source"]["mapfile"]
            .as_str()
            .map(|p| p.ends_with("copy.img.map")),
        Some(true)
    );
    // A second run resumes the finished image and recovers again into a new folder.
    let out2 = dir.path().join("out2");
    let o = run(&[
        OsStr::new("recover"),
        src.as_os_str(),
        OsStr::new("--image-to"),
        copy.as_os_str(),
        OsStr::new("-o"),
        out2.as_os_str(),
        OsStr::new("--no-tui"),
        OsStr::new("--carve"),
        OsStr::new("off"),
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(
        o.stdout.contains("already completely copied"),
        "{}",
        o.stdout
    );
}

#[test]
fn carve_finds_photos_and_videos_without_the_filesystem() {
    let Some(sc) = camera_card() else { return };
    let img = sc.disk_image.as_ref().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("carved");
    let o = run(&[
        OsStr::new("carve"),
        img.as_os_str(),
        OsStr::new("-o"),
        out.as_os_str(),
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    let files = files_under(&out.join("carved"));
    let jpgs = files
        .iter()
        .filter(|p| p.extension().is_some_and(|e| e == "jpg"))
        .count();
    let mp4s = files
        .iter()
        .filter(|p| p.extension().is_some_and(|e| e == "mp4"))
        .count();
    assert_eq!(jpgs, 8, "{files:?}");
    assert_eq!(mp4s, 2, "{files:?}");
    assert!(
        !out.join("files").exists(),
        "carve must not use the filesystem"
    );

    // --free-only finds only what the filesystem no longer lists.
    let out2 = dir.path().join("free");
    let o = run(&[
        OsStr::new("carve"),
        img.as_os_str(),
        OsStr::new("-o"),
        out2.as_os_str(),
        OsStr::new("--free-only"),
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    let files = files_under(&out2.join("carved"));
    assert_eq!(files.len(), 2, "{files:?}");
}

// ------------------------------------------------------------------ repair / verify

#[test]
fn repair_then_verify() {
    let Some(sc) = damaged_media() else { return };
    let root = &sc.root;
    let dir = tempfile::tempdir().unwrap();
    let fixed = dir.path().join("fixed");

    // The broken files fail verification.
    let broken_jpg = root.join("truncated.jpg");
    let broken_mp4 = root.join("no_moov.mp4");
    let v = run(&[OsStr::new("verify"), broken_mp4.as_os_str()]);
    assert_eq!(v.code, 1, "{}", v.all());
    assert!(v.stdout.contains("DAMAGED"), "{}", v.stdout);
    assert!(v.stdout.contains("moov"), "{}", v.stdout);
    let v = run(&[
        OsStr::new("verify"),
        root.join("originals/no_moov.mp4").as_os_str(),
    ]);
    assert_eq!(v.code, 0, "{}", v.all());
    assert!(v.stdout.contains("ok"), "{}", v.stdout);

    let o = run(&[
        OsStr::new("repair"),
        broken_jpg.as_os_str(),
        broken_mp4.as_os_str(),
        OsStr::new("-o"),
        fixed.as_os_str(),
        OsStr::new("--donor"),
        root.join("donors/donor.mp4").as_os_str(),
    ]);
    assert_eq!(o.code, 0, "{}", o.all());
    let repaired_mp4 = fixed.join("repaired/no_moov.mp4");
    assert!(repaired_mp4.is_file(), "{}", o.all());
    assert!(fixed.join("repaired/truncated.jpg").is_file());
    assert!(o.stdout.contains("repaired"), "{}", o.stdout);
    assert!(
        o.stdout.contains("original files were not changed"),
        "{}",
        o.stdout
    );
    // The inputs are untouched.
    assert_eq!(
        blake3_hex(&std::fs::read(&broken_mp4).unwrap()),
        sc.file("no_moov.mp4").unwrap().blake3
    );

    let v = run(&[OsStr::new("verify"), repaired_mp4.as_os_str()]);
    assert_eq!(v.code, 0, "{}", v.all());
    // A repaired JPEG is only partly there; verify still says so.
    let v = run(&[
        OsStr::new("verify"),
        fixed.join("repaired/truncated.jpg").as_os_str(),
    ]);
    assert_eq!(v.code, 1, "{}", v.all());

    // `verify --json` is machine readable.
    let v = run(&[
        OsStr::new("verify"),
        repaired_mp4.as_os_str(),
        broken_mp4.as_os_str(),
        OsStr::new("--json"),
    ]);
    assert_eq!(v.code, 1);
    let arr = json(&v.stdout);
    assert_eq!(arr[0]["ok"], true);
    assert_eq!(arr[1]["ok"], false);
}

#[test]
fn repair_without_a_donor_explains_what_to_do() {
    let Some(sc) = damaged_media() else { return };
    let dir = tempfile::tempdir().unwrap();
    let fixed = dir.path().join("fixed");
    let o = run(&[
        OsStr::new("repair"),
        sc.root.join("no_moov.mp4").as_os_str(),
        OsStr::new("-o"),
        fixed.as_os_str(),
    ]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(o.stdout.contains("--donor"), "{}", o.stdout);
    assert!(o.stdout.contains("could not be repaired"), "{}", o.stdout);
}

#[test]
fn repair_and_verify_check_their_inputs() {
    let dir = tempfile::tempdir().unwrap();
    let o = run(&[
        OsStr::new("repair"),
        dir.path().join("nothing.jpg").as_os_str(),
        OsStr::new("-o"),
        dir.path().join("out").as_os_str(),
    ]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(o.stderr.contains("does not exist"), "{}", o.stderr);
    assert!(!dir.path().join("out").exists());

    let o = run(&[OsStr::new("verify"), dir.path().as_os_str()]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(o.stderr.contains("folder"), "{}", o.stderr);

    let o = run(&[
        OsStr::new("repair"),
        dir.path().join("*.jpg").as_os_str(),
        OsStr::new("-o"),
        dir.path().join("out").as_os_str(),
    ]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(o.stderr.contains("no files match"), "{}", o.stderr);

    // Not a media file at all.
    let junk = dir.path().join("notes.txt");
    std::fs::write(&junk, b"just some text").unwrap();
    let o = run(&[OsStr::new("verify"), junk.as_os_str()]);
    assert_eq!(o.code, 1, "{}", o.all());
    assert!(o.stdout.contains("UNRECOGNISED"), "{}", o.stdout);
}

#[test]
fn verify_expands_wildcards_itself() {
    let Some(sc) = damaged_media() else { return };
    let pattern = sc.root.join("originals").join("*.png");
    let o = run(&[OsStr::new("verify"), pattern.as_os_str()]);
    assert_eq!(o.code, 0, "{}", o.all());
    assert!(
        o.stdout.contains("bad_crc.png") && o.stdout.contains("truncated.png"),
        "{}",
        o.stdout
    );
}
