//! The privileged helper's JSON-line reader, fed with real output of the
//! `diskrescue` program run as a normal user on a regular file, and the
//! helper job itself without elevation.

mod common;

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use diskrescue_app::helper::{
    apply_stderr_line, apply_stdout_line, start_image_via_cli, Elevation, StderrInfo,
};
use diskrescue_app::jobs::{ImageState, ImageStatus};
use diskrescue_app::plan::ImagePlan;
use diskrescue_app::JobOutcome;
use diskrescue_imaging::Phase;

const LIMIT: Duration = Duration::from_secs(300);

#[test]
fn json_lines_of_the_real_program_fill_the_state() {
    let Some(cli) = common::cli_binary() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("source.bin");
    std::fs::write(&src, common::noise(4 << 20, 9)).unwrap();
    let img = dir.path().join("j.img");
    let out = Command::new(&cli)
        .args([
            "image".as_ref(),
            src.as_os_str(),
            img.as_os_str(),
            "--no-tui".as_ref(),
            "--json-progress".as_ref(),
        ])
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .output()
        .expect("cannot run diskrescue");
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.lines().count() >= 3, "{stdout}");

    let mut st = ImageState::default();
    for line in stdout.lines() {
        apply_stdout_line(&mut st, line);
    }
    let mut info = StderrInfo::default();
    for line in String::from_utf8_lossy(&out.stderr).lines() {
        apply_stderr_line(&mut st, &mut info, line);
    }
    assert!(info.refused.is_none() && info.errors.is_empty(), "{info:?}");
    assert_eq!(st.status, ImageStatus::Copying);
    assert_eq!(st.image, img);
    assert_eq!(st.mapfile, dir.path().join("j.img.map"));
    assert_eq!(st.copy_size, 4 << 20);
    assert_eq!(st.profile.as_ref().unwrap().name, "generic");
    assert!(
        st.resume_command.contains("diskrescue image"),
        "{}",
        st.resume_command
    );
    let sum = st.summary.as_ref().expect("the finished line");
    assert!(sum.finished && !sum.cancelled);
    assert_eq!(sum.stats.rescued, 4 << 20);
    assert_eq!(sum.mapfile, dir.path().join("j.img.map"));
    assert!(st.phase.is_some());
    assert!(st
        .logs
        .iter()
        .any(|l| l.text.starts_with("Copying non-tried blocks")));
    if let Some(p) = &st.progress {
        assert!(matches!(p.phase, Phase::Copying { .. } | Phase::Finished));
        assert_eq!(p.size, 4 << 20);
    }
}

#[test]
fn the_helper_job_drives_the_program() {
    let Some(cli) = common::cli_binary() else {
        return;
    };
    let dir = common::scratch("helper-");
    let src = dir.path().join("source.bin");
    std::fs::write(&src, common::noise(2 << 20, 10)).unwrap();
    let img = dir.path().join("h.img");
    let h = start_image_via_cli(ImagePlan::new(&src, &img), cli.clone(), Elevation::None);
    assert_eq!(common::wait(&h, LIMIT), JobOutcome::Finished);
    let s = h.snapshot();
    assert_eq!(s.status, ImageStatus::Done);
    assert!(s.summary.as_ref().is_some_and(|x| x.finished));
    assert_eq!(s.map.as_ref().map(|m| m.size()), Some(2 << 20));
    assert!(common::same_content(&src, &img));

    // Again: already complete, the program prints nothing and exits 0.
    let h = start_image_via_cli(ImagePlan::new(&src, &img), cli.clone(), Elevation::None);
    assert_eq!(common::wait(&h, LIMIT), JobOutcome::Finished);
    assert!(h.snapshot().already_complete);

    // A refusal of the program is a refusal of the job (the preflight
    // catches this one first, which is the point).
    let h = start_image_via_cli(ImagePlan::new(&src, &src), cli, Elevation::None);
    assert!(
        matches!(common::wait(&h, LIMIT), JobOutcome::Refused(m) if m.contains("the source itself"))
    );
}

#[test]
fn the_helper_job_stops_cleanly_and_resumes() {
    let Some(cli) = common::cli_binary() else {
        return;
    };
    let dir = common::scratch("helper-stop-");
    let src = dir.path().join("big.bin");
    let f = std::fs::File::create(&src).unwrap();
    // Large enough that the first copy is still running when it is stopped:
    // 512 MiB was sometimes copied completely before the stop arrived on a
    // busy CI machine.
    f.set_len(2 << 30).unwrap();
    drop(f);
    let img = dir.path().join("big.img");
    let mut plan = ImagePlan::new(&src, &img);
    plan.tuning.block_size = Some(512);
    let h = start_image_via_cli(plan.clone(), cli.clone(), Elevation::None);
    // The program cannot be paused from outside.
    assert!(!h.can_pause());
    h.pause(true);
    assert!(!h.snapshot().paused);
    let t = Instant::now();
    while !h
        .snapshot()
        .progress
        .as_ref()
        .is_some_and(|p| p.stats.rescued > 0)
    {
        assert!(!h.is_finished(), "ended early: {:?}", h.outcome());
        assert!(t.elapsed() < Duration::from_secs(60), "never started");
        std::thread::sleep(Duration::from_millis(20));
    }
    h.cancel();
    h.cancel();
    assert_eq!(common::wait(&h, LIMIT), JobOutcome::Cancelled);
    let s = h.snapshot();
    assert!(s.summary.as_ref().is_some_and(|x| x.cancelled));
    let st = s.map.as_ref().unwrap().stats();
    assert!(st.rescued > 0 && st.rescued < 2 << 30, "{st:?}");

    plan.tuning.block_size = None;
    let h = start_image_via_cli(plan, cli, Elevation::None);
    assert_eq!(common::wait(&h, LIMIT), JobOutcome::Finished);
    assert!(h.snapshot().resumed_from.is_some());
    assert!(common::same_content(&src, &img));
}

#[test]
fn closing_standard_input_stops_the_program() {
    let Some(cli) = common::cli_binary() else {
        return;
    };
    let dir = common::scratch("stdin-");
    let src = dir.path().join("big.bin");
    let f = std::fs::File::create(&src).unwrap();
    f.set_len(512 << 20).unwrap();
    drop(f);
    let img = dir.path().join("big.img");
    let map = dir.path().join("big.img.map");
    let mut child = Command::new(&cli)
        .args([
            "image".as_ref(),
            src.as_os_str(),
            img.as_os_str(),
            "--json-progress".as_ref(),
            "--block-size".as_ref(),
            "512".as_ref(),
            "--stop-on-stdin-close".as_ref(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"keep going\n").unwrap();
    let t = Instant::now();
    while !map.exists() {
        assert!(t.elapsed() < Duration::from_secs(30), "never started");
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(300));
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(130), "{out:?}");
    let last = String::from_utf8_lossy(&out.stdout)
        .lines()
        .last()
        .unwrap_or_default()
        .to_string();
    assert!(last.contains("\"cancelled\":true"), "{last}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("standard input was closed"));
}
