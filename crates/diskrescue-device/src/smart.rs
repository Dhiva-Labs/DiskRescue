//! Optional SMART summary through `smartctl -j -a`.
//!
//! DiskRescue does not speak ATA/NVMe pass-through itself. When the
//! `smartctl` binary (smartmontools >= 7.0) is on `PATH`, [`query`] runs
//! `smartctl -j -a <device>` (and once more with `-d sat` when the first
//! run could not open the device or its SMART commands failed, which is
//! what USB-SATA bridges usually need) and maps the JSON to
//! [`HealthInfo`]:
//!
//! * ATA attributes 5 (reallocated), 197 (pending), 198 (offline
//!   uncorrectable), 194 (temperature) and 9 (power-on hours), plus notes
//!   for 187/188/199 (uncorrectable reports, command timeouts, interface
//!   CRC errors) and attributes failing now;
//! * the NVMe SMART/health log: temperature, percentage used, power-on
//!   hours, media errors, available spare and the critical-warning bits
//!   (bit 3 = media in read-only mode);
//! * `smart_status.passed` and the exit-status bits 3-7.
//!
//! It never fails hard: a missing binary, a timeout (30 s; a dying drive
//! can hang even IDENTIFY), unparsable output or a bridge that hides SMART
//! all yield `None`. smartctl needs root for most devices.
//!
//! Run it *before* opening the source for direct I/O: spawning a process
//! while an `O_DIRECT` read is in flight is discouraged (open(2) NOTES).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use diskrescue_core::device::HealthInfo;
use serde_json::Value;

/// Default time allowed for one smartctl run.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Largest smartctl output accepted.
const MAX_OUTPUT: u64 = 8 << 20;

/// Locate `smartctl` on `PATH`.
pub fn find_smartctl() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let names: &[&str] = if cfg!(windows) {
        &["smartctl.exe", "smartctl"]
    } else {
        &["smartctl"]
    };
    for dir in std::env::split_paths(&path).take(256) {
        for n in names {
            let p = dir.join(n);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// SMART summary of `path`, or `None` when unavailable. See the module docs.
pub fn query(path: &Path) -> Option<HealthInfo> {
    query_with_timeout(path, DEFAULT_TIMEOUT)
}

/// [`query`] with an explicit per-run timeout.
pub fn query_with_timeout(path: &Path, timeout: Duration) -> Option<HealthInfo> {
    let Some(bin) = find_smartctl() else {
        log::debug!("smartctl not found on PATH; no SMART data");
        return None;
    };
    query_with_binary(&bin, path, timeout)
}

/// [`query_with_timeout`] with an explicit `smartctl` executable.
pub fn query_with_binary(bin: &Path, path: &Path, timeout: Duration) -> Option<HealthInfo> {
    let bin = bin.to_path_buf();
    let first = run(&bin, &["-j", "-a"], path, timeout)?;
    if let Some(h) = parse_smartctl_json(&first) {
        return Some(h);
    }
    let status = exit_status(&first).unwrap_or(0);
    if status & 0b110 != 0 {
        log::debug!(
            "smartctl exit status {status} on {}; retrying with -d sat",
            path.display()
        );
        let second = run(&bin, &["-j", "-a", "-d", "sat"], path, timeout)?;
        return parse_smartctl_json(&second);
    }
    None
}

fn run(bin: &Path, args: &[&str], dev: &Path, timeout: Duration) -> Option<String> {
    let mut child = Command::new(bin)
        .args(args)
        .arg(dev)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| log::debug!("cannot run smartctl: {e}"))
        .ok()?;
    let Some(mut stdout) = child.stdout.take() else {
        abandon(child);
        return None;
    };
    let (tx, rx) = std::sync::mpsc::channel();
    // Detached: it ends when the pipe closes (the process and anything it
    // left behind exited) or the output limit is reached.
    let _ = std::thread::Builder::new()
        .name("dr-smartctl-out".into())
        .spawn(move || {
            let mut v = Vec::new();
            let _ = (&mut stdout).take(MAX_OUTPUT).read_to_end(&mut v);
            let _ = tx.send(v);
        });
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() < timeout => std::thread::sleep(Duration::from_millis(50)),
            Ok(None) | Err(_) => {
                log::warn!(
                    "smartctl on {} did not finish within {:?}; killed",
                    dev.display(),
                    timeout
                );
                abandon(child);
                return None;
            }
        }
    }
    // smartctl exited; its output is complete once the pipe closes. A
    // helper it left running could keep the pipe open: do not wait for it.
    let out = rx.recv_timeout(OUTPUT_GRACE).ok()?;
    Some(String::from_utf8_lossy(&out).into_owned())
}

/// How long to wait for the output pipe to close after smartctl exited.
const OUTPUT_GRACE: Duration = Duration::from_secs(2);

/// Kill a smartctl that ran too long without ever blocking on it: a
/// process stuck in uninterruptible I/O on a dying drive does not die
/// until the kernel gives up on the command, which can take minutes. It is
/// reaped by a background thread so no zombie is left behind.
fn abandon(mut child: std::process::Child) {
    let _ = child.kill();
    for _ in 0..20 {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = std::thread::Builder::new()
        .name("dr-smartctl-reap".into())
        .spawn(move || {
            let _ = child.wait();
        });
}

fn exit_status(json: &str) -> Option<u64> {
    let v: Value = serde_json::from_str(json).ok()?;
    v.pointer("/smartctl/exit_status")?.as_u64()
}

fn leading_u64(s: &str) -> Option<u64> {
    let d: String = s
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if d.is_empty() || d.len() > 19 {
        None
    } else {
        d.parse().ok()
    }
}

fn attr_raw(attr: &Value) -> Option<u64> {
    let raw = attr.get("raw")?;
    raw.get("string")
        .and_then(Value::as_str)
        .and_then(leading_u64)
        .or_else(|| raw.get("value").and_then(Value::as_u64))
}

/// Map `smartctl -j` output to a [`HealthInfo`]. Returns `None` when the
/// JSON is invalid or carries no SMART data at all (for example a USB
/// bridge smartctl cannot talk through).
pub fn parse_smartctl_json(json: &str) -> Option<HealthInfo> {
    let v: Value = serde_json::from_str(json).ok()?;
    if !v.is_object() {
        return None;
    }
    let mut h = HealthInfo::default();
    let mut has_data = false;

    if let Some(p) = v.pointer("/smart_status/passed").and_then(Value::as_bool) {
        h.passed = Some(p);
        has_data = true;
    }
    if let Some(t) = v.pointer("/temperature/current").and_then(Value::as_i64) {
        h.temperature_c = i32::try_from(t).ok();
        has_data = true;
    }
    if let Some(p) = v.pointer("/power_on_time/hours").and_then(Value::as_u64) {
        h.power_on_hours = Some(p);
        has_data = true;
    }

    if let Some(table) = v
        .pointer("/ata_smart_attributes/table")
        .and_then(Value::as_array)
    {
        has_data = true;
        for attr in table.iter().take(256) {
            let Some(id) = attr.get("id").and_then(Value::as_u64) else {
                continue;
            };
            let raw = attr_raw(attr);
            let name = attr.get("name").and_then(Value::as_str).unwrap_or("");
            match id {
                5 => h.reallocated_sectors = raw,
                197 => h.pending_sectors = raw,
                198 => h.uncorrectable_sectors = raw,
                194 | 190 => {
                    if h.temperature_c.is_none() {
                        h.temperature_c = raw.map(|r| (r & 0xff) as i32);
                    }
                }
                9 => {
                    if h.power_on_hours.is_none() {
                        h.power_on_hours = raw;
                    }
                }
                187 if raw.unwrap_or(0) > 0 => h.notes.push(format!(
                    "reported uncorrectable errors: {}",
                    raw.unwrap_or(0)
                )),
                188 if raw.unwrap_or(0) > 0 => h
                    .notes
                    .push(format!("command timeouts: {}", raw.unwrap_or(0))),
                199 if raw.unwrap_or(0) > 0 => h.notes.push(format!(
                    "interface CRC errors: {} (cable or USB bridge problem, not the medium)",
                    raw.unwrap_or(0)
                )),
                _ => {}
            }
            if attr.get("when_failed").and_then(Value::as_str) == Some("now") {
                h.notes
                    .push(format!("attribute {id} {name} is failing now"));
            }
        }
    }

    if let Some(log) = v.get("nvme_smart_health_information_log") {
        if log.is_object() {
            has_data = true;
            if h.temperature_c.is_none() {
                h.temperature_c = log
                    .get("temperature")
                    .and_then(Value::as_i64)
                    .and_then(|t| i32::try_from(t).ok());
            }
            if let Some(p) = log.get("percentage_used").and_then(Value::as_u64) {
                h.percentage_used = Some(p.min(255) as u8);
            }
            if h.power_on_hours.is_none() {
                h.power_on_hours = log.get("power_on_hours").and_then(Value::as_u64);
            }
            if let Some(m) = log.get("media_errors").and_then(Value::as_u64) {
                if m > 0 {
                    h.notes
                        .push(format!("media and data integrity errors: {m}"));
                }
            }
            let spare = log.get("available_spare").and_then(Value::as_u64);
            let thresh = log.get("available_spare_threshold").and_then(Value::as_u64);
            if let (Some(s), Some(t)) = (spare, thresh) {
                if s < t {
                    h.notes
                        .push(format!("available spare {s}% is below the threshold {t}%"));
                }
            }
            if let Some(cw) = log.get("critical_warning").and_then(Value::as_u64) {
                const BITS: [&str; 6] = [
                    "available spare below threshold",
                    "temperature out of range",
                    "reliability degraded (media errors)",
                    "media in read-only mode",
                    "volatile memory backup failed",
                    "persistent memory region read-only",
                ];
                for (i, b) in BITS.iter().enumerate() {
                    if cw & (1 << i) != 0 {
                        h.notes.push(format!("NVMe critical warning: {b}"));
                    }
                }
            }
        }
    }

    if let Some(st) = v.pointer("/smartctl/exit_status").and_then(Value::as_u64) {
        if st & (1 << 3) != 0 {
            h.notes
                .push("SMART status check returned DISK FAILING".into());
        }
        if st & (1 << 4) != 0 {
            h.notes
                .push("some pre-failure attributes are at or below their threshold".into());
        }
        if st & (1 << 5) != 0 {
            h.notes
                .push("some attributes were at or below their threshold in the past".into());
        }
        if st & (1 << 6) != 0 {
            h.notes.push("the device error log contains errors".into());
        }
        if st & (1 << 7) != 0 {
            h.notes.push("the self-test log contains errors".into());
        }
    }
    if !has_data {
        if let Some(msgs) = v.pointer("/smartctl/messages").and_then(Value::as_array) {
            for m in msgs.iter().take(8) {
                if let Some(s) = m.get("string").and_then(Value::as_str) {
                    log::debug!("smartctl: {s}");
                }
            }
        }
        return None;
    }
    if let Some(msgs) = v.pointer("/smartctl/messages").and_then(Value::as_array) {
        for m in msgs.iter().take(8) {
            let sev = m.get("severity").and_then(Value::as_str).unwrap_or("");
            if let Some(s) = m.get("string").and_then(Value::as_str) {
                if sev == "error" || sev == "warning" {
                    h.notes.push(format!("smartctl: {s}"));
                }
            }
        }
    }
    Some(h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_documents() {
        assert!(parse_smartctl_json("").is_none());
        assert!(parse_smartctl_json("[]").is_none());
        assert!(parse_smartctl_json("{}").is_none());
        let h = parse_smartctl_json(
            r#"{"smart_status":{"passed":false},"smartctl":{"exit_status":8}}"#,
        )
        .unwrap();
        assert_eq!(h.passed, Some(false));
        assert!(h.notes.iter().any(|n| n.contains("DISK FAILING")));
        assert_eq!(leading_u64("34 (Min/Max 18/45)"), Some(34));
        assert_eq!(leading_u64("12345h+05m"), Some(12345));
        assert_eq!(leading_u64("99999999999999999999999"), None);
        assert_eq!(leading_u64("x"), None);
    }

    #[test]
    fn query_without_smartctl_is_none() {
        // smartctl is not installed on the build machines; when it is, the
        // query of a non-device must still not fail hard.
        assert!(
            query_with_timeout(Path::new("/nonexistent/device"), Duration::from_secs(5)).is_none()
        );
    }
}
