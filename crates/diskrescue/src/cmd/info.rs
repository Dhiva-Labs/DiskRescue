//! `diskrescue info`: device details, partitions and filesystems.

use std::time::Duration;

use diskrescue_core::device::{DeviceInfo, DeviceKind, HealthInfo};
use diskrescue_core::partition::{PartitionScheme, PartitionTable};
use diskrescue_core::profile::ImagingProfile;
use diskrescue_core::util::human;
use diskrescue_core::SharedSource;
use diskrescue_partition::Volume;
use serde_json::json;

use crate::args::InfoArgs;
use crate::cmd::list::{size_text, transport_name};
use crate::error::{CliError, CmdResult};
use crate::source::{self, SourceRequest};
use crate::term::{self, Align, Paint, Table};
use crate::{profile_text, util};

/// SMART data is only asked of real disks.
fn wants_smart(info: &DeviceInfo) -> bool {
    !matches!(
        info.kind,
        DeviceKind::ImageFile | DeviceKind::Virtual | DeviceKind::Optical
    ) && info.health.is_none()
}

fn scheme_text(s: PartitionScheme) -> &'static str {
    match s {
        PartitionScheme::Mbr => "MBR (classic partition table)",
        PartitionScheme::Gpt => "GPT",
        PartitionScheme::None => "none (a filesystem starts at the beginning, like a camera card)",
        PartitionScheme::Unknown => "none found",
    }
}

fn health_lines(h: &HealthInfo) -> Vec<String> {
    let mut v = Vec::new();
    match h.passed {
        Some(true) => v.push("the drive's own health check passed".to_string()),
        Some(false) => v.push("the drive's own health check FAILED: it is failing".to_string()),
        None => {}
    }
    let mut counters = Vec::new();
    if let Some(n) = h.reallocated_sectors {
        counters.push(format!("{n} reallocated sectors"));
    }
    if let Some(n) = h.pending_sectors {
        counters.push(format!("{n} pending sectors"));
    }
    if let Some(n) = h.uncorrectable_sectors {
        counters.push(format!("{n} uncorrectable sectors"));
    }
    if !counters.is_empty() {
        v.push(counters.join(", "));
    }
    let mut misc = Vec::new();
    if let Some(t) = h.temperature_c {
        misc.push(format!("{t} C"));
    }
    if let Some(n) = h.power_on_hours {
        misc.push(format!("{} hours powered on", human::count(n)));
    }
    if let Some(n) = h.percentage_used {
        misc.push(format!("{n}% of rated life used"));
    }
    if !misc.is_empty() {
        v.push(misc.join(", "));
    }
    v.extend(h.notes.iter().cloned());
    v
}

fn field(out: &mut String, p: &Paint, name: &str, value: &str) {
    out.push_str(&format!("  {}{value}\n", p.dim(&format!("{name:<14}"))));
}

/// Text report for a described device.
pub fn render_device(info: &DeviceInfo, health: Option<&HealthInfo>) -> String {
    let p = term::out();
    let mut out = String::new();
    out.push_str(&format!("{}\n", p.bold(&info.path.display().to_string())));
    field(
        &mut out,
        &p,
        "Type",
        &format!(
            "{}{}",
            info.kind.display_name(),
            if info.transport == diskrescue_core::device::Transport::Unknown {
                String::new()
            } else {
                format!(" ({})", transport_name(info.transport))
            }
        ),
    );
    let model = [info.vendor.as_deref(), info.model.as_deref()]
        .iter()
        .flatten()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if !model.is_empty() {
        field(&mut out, &p, "Model", &model);
    }
    if let Some(s) = &info.serial {
        field(&mut out, &p, "Serial", s);
    }
    if let Some(s) = &info.firmware {
        field(&mut out, &p, "Firmware", s);
    }
    field(
        &mut out,
        &p,
        "Size",
        &format!(
            "{} - {} bytes",
            size_text(info.size),
            human::count(info.size)
        ),
    );
    if info.kind != DeviceKind::ImageFile {
        field(
            &mut out,
            &p,
            "Sectors",
            &format!(
                "{} bytes logical, {} bytes physical",
                info.logical_sector, info.physical_sector
            ),
        );
        field(
            &mut out,
            &p,
            "Removable",
            if info.removable { "yes" } else { "no" },
        );
        if info.read_only {
            field(
                &mut out,
                &p,
                "Read-only",
                "yes (the system has marked it read-only)",
            );
        }
        field(
            &mut out,
            &p,
            "Mounted",
            &if info.mountpoints.is_empty() {
                "no".to_string()
            } else {
                format!(
                    "{}, unmount it before imaging",
                    info.mountpoints
                        .iter()
                        .map(|m| m.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            },
        );
    }
    if let Some(h) = health {
        let lines = health_lines(h);
        if lines.is_empty() {
            field(&mut out, &p, "SMART", "no readable values");
        } else {
            for (i, l) in lines.iter().enumerate() {
                let name = if i == 0 { "SMART" } else { "" };
                let l = if l.contains("FAILED") {
                    p.bold_red(l)
                } else {
                    l.clone()
                };
                field(&mut out, &p, name, &l);
            }
        }
    }
    if !info.warnings.is_empty() {
        out.push('\n');
        out.push_str(&format!("  {}\n", p.yellow("Warnings")));
        for w in &info.warnings {
            out.push_str(&format!("    - {w}\n"));
        }
    }
    out
}

/// Text for the partition table and its volumes.
pub fn render_partitions(table: &PartitionTable, vols: &[Volume], width: usize) -> String {
    let p = term::out();
    let mut out = String::new();
    out.push_str(&format!(
        "{} {}\n",
        p.bold("Partition table:"),
        scheme_text(table.scheme)
    ));
    if let Some(g) = &table.disk_guid {
        out.push_str(&format!("  disk GUID {g}\n"));
    }
    if table.entries.is_empty() {
        out.push_str("  no partitions found\n");
    } else {
        let mut t = Table::new(&["#", "START", "SIZE", "TYPE", "FILESYSTEM", "LABEL"])
            .align(0, Align::Right)
            .align(1, Align::Right)
            .align(2, Align::Right)
            .flex(5)
            .indent(2);
        for e in &table.entries {
            let vol = vols.iter().find(|v| v.entry.index == e.index);
            let probe = vol.and_then(|v| v.probe.as_ref());
            let fs = probe
                .map(|pr| {
                    let mut s = pr.display_name();
                    if pr.via_backup {
                        s.push_str(" (damaged, backup copy used)");
                    }
                    s
                })
                .or_else(|| e.fs.map(|f| f.name().to_string()))
                .unwrap_or_else(|| "not recognised".into());
            t.row(vec![
                e.index.to_string(),
                human::bytes(e.start),
                human::bytes(e.size),
                if e.type_desc.is_empty() {
                    e.type_id.clone()
                } else {
                    e.type_desc.clone()
                },
                fs,
                probe
                    .and_then(|pr| pr.label.clone())
                    .or_else(|| e.name.clone())
                    .unwrap_or_default(),
            ]);
        }
        out.push_str(&t.render(width, &p));
    }
    for w in &table.warnings {
        out.push_str(&format!("  {} {w}\n", p.yellow("note:")));
    }
    for v in vols {
        if let Some(pr) = &v.probe {
            for n in &pr.notes {
                out.push_str(&format!("  partition {}: {n}\n", v.entry.index));
            }
        }
    }
    out
}

/// Run the command.
pub fn run(args: &InfoArgs) -> CmdResult {
    let mut info = source::describe(&args.source)?;
    let p = term::out();

    let mut health = info.health.clone();
    if health.is_none() && wants_smart(&info) {
        health = diskrescue_device::smart::query_with_timeout(&info.path, Duration::from_secs(8));
    }

    // Opening can fail (permissions); the description above is still useful.
    let mut req = SourceRequest::reading(&args.source);
    req.map = args.map.as_deref();
    let mut open_error: Option<String> = None;
    let mut table: Option<PartitionTable> = None;
    let mut vols: Vec<Volume> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let mut mapfile = None;
    let mut map_stats = None;
    match source::open(&req) {
        Ok(opened) => {
            info = opened.info.clone();
            notes.extend(opened.warnings.iter().cloned());
            mapfile = opened.mapfile.clone();
            map_stats = opened.map_stats;
            let shared: SharedSource = opened.shared.clone();
            match diskrescue_partition::volumes(&shared, opened.info.logical_sector) {
                Ok((t, v)) => {
                    table = Some(t);
                    vols = v;
                }
                Err(e) => open_error = Some(format!("the partition table could not be read: {e}")),
            }
        }
        Err(CliError::Failed(m)) | Err(CliError::Refused(m)) => open_error = Some(m),
        Err(e) => return Err(e),
    }

    let profile = ImagingProfile::for_kind(info.kind);

    if args.json {
        let volumes: Vec<_> = vols
            .iter()
            .map(|v| {
                json!({
                    "index": v.entry.index,
                    "start": v.entry.start,
                    "size": v.entry.size,
                    "filesystem": v.probe.as_ref().map(|p| p.kind),
                    "filesystem_name": v.probe.as_ref().map(|p| p.display_name()),
                    "label": v.probe.as_ref().and_then(|p| p.label.clone()),
                    "via_backup": v.probe.as_ref().map(|p| p.via_backup),
                    "notes": v.probe.as_ref().map(|p| p.notes.clone()).unwrap_or_default(),
                })
            })
            .collect();
        let doc = json!({
            "device": info,
            "health": health,
            "mapfile": mapfile,
            "map_stats": map_stats,
            "partition_table": table,
            "volumes": volumes,
            "profile": profile,
            "notes": notes,
            "error": open_error,
        });
        let text = serde_json::to_string_pretty(&doc)
            .map_err(|e| CliError::failed(format!("cannot format the details: {e}")))?;
        println!("{text}");
        return Ok(0);
    }

    print!("{}", render_device(&info, health.as_ref()));
    if let (Some(mf), Some(st)) = (&mapfile, &map_stats) {
        println!(
            "  {}{} - {} of {} copied",
            p.dim(&format!("{:<14}", "Mapfile")),
            mf.display(),
            util::percent(st.rescued, st.total()),
            human::bytes(st.total())
        );
    }
    for n in &notes {
        println!("  {} {n}", p.yellow("note:"));
    }
    println!();
    match &table {
        Some(t) => print!("{}", render_partitions(t, &vols, term::width())),
        None => {
            if let Some(e) = &open_error {
                println!("{} {e}", p.yellow("Could not read the partition table:"));
            }
        }
    }
    println!();
    let kind_profile = profile.name.clone();
    println!("{} {}", p.bold("Imaging profile:"), kind_profile);
    println!("  {}", profile_text::blurb(&kind_profile));
    println!("  {}", profile_text::details(&profile));
    if info.kind != DeviceKind::ImageFile {
        println!("  Override with --profile if the device was detected wrongly (USB bridges often hide the real type).");
        println!();
        println!(
            "Next: diskrescue image {} <image file>",
            util::shell_quote(&info.path.display().to_string())
        );
    } else {
        println!();
        println!(
            "Next: diskrescue scan {}   (see what can be recovered)",
            util::shell_quote(&info.path.display().to_string())
        );
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskrescue_core::device::Transport;
    use diskrescue_core::partition::PartitionEntry;
    use std::path::PathBuf;

    fn device() -> DeviceInfo {
        DeviceInfo {
            path: PathBuf::from("/dev/sdb"),
            name: "sdb".into(),
            kind: DeviceKind::UsbFlash,
            transport: Transport::Usb,
            size: 32_018_267_136,
            logical_sector: 512,
            physical_sector: 512,
            model: Some("Ultra".into()),
            vendor: Some("SanDisk".into()),
            serial: Some("ABC".into()),
            firmware: None,
            stable_id: None,
            removable: true,
            rotational: false,
            read_only: false,
            partitions: vec![],
            mountpoints: vec![PathBuf::from("/media/x")],
            health: None,
            warnings: vec!["be careful".into()],
        }
    }

    #[test]
    fn device_text_has_the_essentials() {
        let h = HealthInfo {
            passed: Some(false),
            reallocated_sectors: Some(12),
            pending_sectors: Some(3),
            temperature_c: Some(41),
            power_on_hours: Some(12345),
            ..HealthInfo::default()
        };
        let s = render_device(&device(), Some(&h));
        for needle in [
            "/dev/sdb",
            "USB flash drive (USB)",
            "SanDisk Ultra",
            "ABC",
            "29.8 GiB (32 GB)",
            "512 bytes logical",
            "unmount it before imaging",
            "FAILED",
            "12 reallocated sectors",
            "12,345 hours",
            "be careful",
        ] {
            assert!(s.contains(needle), "missing {needle:?} in:\n{s}");
        }
    }

    #[test]
    fn partition_table_text() {
        let table = PartitionTable {
            scheme: PartitionScheme::Gpt,
            entries: vec![PartitionEntry {
                index: 1,
                start: 1 << 20,
                size: 14 << 30,
                type_id: "EBD0A0A2".into(),
                type_desc: "Microsoft basic data".into(),
                name: Some("PHONE".into()),
                guid: None,
                bootable: false,
                origin: "gpt".into(),
                fs: Some(diskrescue_core::fs::FsKind::ExFat),
            }],
            disk_guid: Some("1234".into()),
            warnings: vec!["backup header used".into()],
        };
        let s = render_partitions(&table, &[], 100);
        for needle in [
            "GPT",
            "disk GUID 1234",
            "Microsoft basic data",
            "exfat",
            "PHONE",
            "backup header used",
        ] {
            assert!(s.contains(needle), "missing {needle:?} in:\n{s}");
        }
        let empty = PartitionTable::empty(PartitionScheme::Unknown);
        assert!(render_partitions(&empty, &[], 80).contains("no partitions found"));
    }

    #[test]
    fn smart_is_not_asked_of_image_files() {
        let mut d = device();
        assert!(wants_smart(&d));
        d.kind = DeviceKind::ImageFile;
        assert!(!wants_smart(&d));
    }
}
