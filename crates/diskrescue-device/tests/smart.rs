//! smartctl JSON parsing with committed sample output (smartctl itself is
//! not installed on the build machines).

mod common;

use std::path::Path;

use common::XorShift;
use diskrescue_device::smart::{parse_smartctl_json, query};

fn fixture(name: &str) -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/smart")
        .join(name);
    std::fs::read_to_string(p).unwrap()
}

#[test]
fn ata_disk() {
    let h = parse_smartctl_json(&fixture("ata_hdd.json")).unwrap();
    assert_eq!(h.passed, Some(true));
    assert_eq!(h.reallocated_sectors, Some(412));
    assert_eq!(h.pending_sectors, Some(36));
    assert_eq!(h.uncorrectable_sectors, Some(9));
    assert_eq!(h.temperature_c, Some(39));
    assert_eq!(h.power_on_hours, Some(33876));
    assert_eq!(h.percentage_used, None);
    let notes = h.notes.join("\n");
    assert!(
        notes.contains("reported uncorrectable errors: 17"),
        "{notes}"
    );
    assert!(
        !notes.contains("CRC"),
        "zero CRC errors are not reported: {notes}"
    );
    assert!(notes.contains("error log contains errors"), "{notes}");
    assert!(notes.contains("self-test log contains errors"), "{notes}");
}

#[test]
fn nvme_disk() {
    let h = parse_smartctl_json(&fixture("nvme.json")).unwrap();
    assert_eq!(h.passed, Some(false));
    assert_eq!(h.temperature_c, Some(41));
    assert_eq!(h.percentage_used, Some(117));
    assert_eq!(h.power_on_hours, Some(18263));
    assert_eq!(h.reallocated_sectors, None);
    let notes = h.notes.join("\n");
    assert!(
        notes.contains("media and data integrity errors: 2381"),
        "{notes}"
    );
    assert!(notes.contains("read-only mode"), "{notes}");
    assert!(notes.contains("reliability degraded"), "{notes}");
    assert!(notes.contains("below the threshold"), "{notes}");
    assert!(notes.contains("smartctl: Read 1 entries"), "{notes}");
}

#[test]
fn usb_bridge_without_smart_is_none() {
    assert!(parse_smartctl_json(&fixture("usb_bridge_fail.json")).is_none());
}

#[test]
fn ata_temperature_from_attribute_when_top_level_missing() {
    let j = r#"{"ata_smart_attributes":{"table":[
        {"id":194,"name":"Temperature_Celsius","raw":{"value":167503724583}},
        {"id":9,"name":"Power_On_Hours","raw":{"value":5,"string":"1234h+05m+10.000s"}},
        {"id":199,"name":"UDMA_CRC_Error_Count","raw":{"value":7,"string":"7"}},
        {"id":5,"name":"Reallocated_Sector_Ct","when_failed":"now","raw":{"value":3000}},
        {"name":"no id"},
        {"id":"x"}
    ]}}"#;
    let h = parse_smartctl_json(j).unwrap();
    assert_eq!(h.temperature_c, Some(39));
    assert_eq!(h.power_on_hours, Some(1234));
    assert_eq!(h.reallocated_sectors, Some(3000));
    assert!(h.notes.iter().any(|n| n.contains("CRC errors: 7")));
    assert!(h.notes.iter().any(|n| n.contains("failing now")));
}

#[test]
fn hostile_json_never_panics() {
    let good = fixture("ata_hdd.json");
    for cut in (0..good.len()).step_by(37) {
        let _ = parse_smartctl_json(&good[..cut]);
    }
    let mut rng = XorShift(0xDEAD_BEEF_1234_5678);
    for len in 0..400 {
        let b = rng.bytes(len);
        let _ = parse_smartctl_json(&String::from_utf8_lossy(&b));
    }
    // Deep nesting, wrong types, absurd numbers.
    let deep = "[".repeat(10_000);
    assert!(parse_smartctl_json(&deep).is_none());
    let weird = r#"{"smart_status":{"passed":"yes"},"temperature":{"current":1e300},
        "nvme_smart_health_information_log":{"percentage_used":99999,"temperature":-5,
        "critical_warning":18446744073709551615},"ata_smart_attributes":{"table":{}}}"#;
    let h = parse_smartctl_json(weird).unwrap();
    assert_eq!(h.percentage_used, Some(255));
    assert_eq!(h.temperature_c, Some(-5));
}

#[test]
fn query_never_fails_hard() {
    // smartctl is absent here; with it present, a bogus path must still
    // just yield None.
    assert!(query(Path::new("/dev/does-not-exist-diskrescue")).is_none());
}

/// Fake `smartctl` executables (shell scripts): output, retry with
/// `-d sat`, hangs and helpers that keep the output pipe open must never
/// make the query hang or fail hard.
#[cfg(unix)]
#[test]
fn fake_smartctl_runs_are_bounded() {
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};

    use diskrescue_device::smart::query_with_binary;

    if !Path::new("/bin/sh").exists() {
        eprintln!("skipped: /bin/sh missing");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let json = dir.path().join("ata.json");
    std::fs::write(&json, fixture("ata_hdd.json")).unwrap();
    let script = |name: &str, body: String| {
        let p = dir.path().join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    };
    let ok = script("ok", format!("cat '{}'", json.display()));
    // First run: "device open failed" (exit bit 1); with -d sat: the data.
    let sat = script(
        "sat",
        format!(
            "case \"$*\" in *'-d sat'*) cat '{}';; *) echo '{{\"smartctl\":{{\"exit_status\":2}}}}';; esac",
            json.display()
        ),
    );
    let hang = script("hang", "exec sleep 30".into());
    // Prints valid output, then leaves a helper holding stdout open.
    let leak = script(
        "leak",
        format!("cat '{}'; (sleep 30 &) ; exit 0", json.display()),
    );
    let garbage = script("garbage", "head -c 100000 /dev/urandom; exit 3".into());
    let dev = Path::new("/dev/does-not-exist-diskrescue");

    let h = query_with_binary(&ok, dev, Duration::from_secs(20)).unwrap();
    assert_eq!(h.reallocated_sectors, Some(412));
    let h = query_with_binary(&sat, dev, Duration::from_secs(20)).unwrap();
    assert_eq!(h.pending_sectors, Some(36));

    let t = Instant::now();
    assert!(query_with_binary(&hang, dev, Duration::from_millis(500)).is_none());
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());

    let t = Instant::now();
    let _ = query_with_binary(&leak, dev, Duration::from_secs(10));
    assert!(t.elapsed() < Duration::from_secs(8), "{:?}", t.elapsed());

    assert!(query_with_binary(&garbage, dev, Duration::from_secs(10)).is_none());
    assert!(query_with_binary(&dir.path().join("missing"), dev, Duration::from_secs(1)).is_none());
}
