//! `diskrescue list`: show disks, cards and USB drives.

use diskrescue_core::device::DeviceInfo;
#[cfg(test)]
use diskrescue_core::device::{DeviceKind, Transport};
use diskrescue_core::util::human;

#[cfg(test)]
pub use diskrescue_app::devices::visible_devices;
pub use diskrescue_app::text::{size_label as size_text, transport_label as transport_name};

use crate::args::ListArgs;
use crate::error::{CliError, CmdResult};
use crate::term::{self, Align, Table};

fn mounted_text(d: &DeviceInfo) -> String {
    if d.mountpoints.is_empty() {
        "no".to_string()
    } else {
        let mut v: Vec<String> = d
            .mountpoints
            .iter()
            .map(|m| m.display().to_string())
            .collect();
        v.sort();
        v.dedup();
        v.join(", ")
    }
}

/// Render the human-readable table.
pub fn render(devices: &[DeviceInfo], width: usize) -> String {
    let paint = term::out();
    let mut table = Table::new(&[
        "DEVICE", "SIZE", "TYPE", "BUS", "MODEL", "SERIAL", "MOUNTED",
    ]);
    table = table.align(1, Align::Right).flex(4).flex(5).flex(6);
    for d in devices {
        let model = [d.vendor.as_deref(), d.model.as_deref()]
            .iter()
            .flatten()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        table.row(vec![
            d.path.display().to_string(),
            size_text(d.size),
            d.kind.display_name().to_string(),
            transport_name(d.transport).to_string(),
            if model.is_empty() { "-".into() } else { model },
            d.serial.clone().unwrap_or_else(|| "-".into()),
            mounted_text(d),
        ]);
        for p in &d.partitions {
            let name = p
                .path
                .as_ref()
                .map(|x| x.display().to_string())
                .unwrap_or_else(|| format!("partition {}", p.number));
            table.row(vec![
                paint.dim(&format!("  {name}")),
                paint.dim(&human::bytes(p.size)),
                paint.dim(p.fs_type.as_deref().unwrap_or("")),
                String::new(),
                String::new(),
                String::new(),
                paint.dim(&if p.mountpoints.is_empty() {
                    String::new()
                } else {
                    p.mountpoints
                        .iter()
                        .map(|m| m.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                }),
            ]);
        }
    }
    let mut out = table.render(width, &paint);
    let notes: Vec<String> = devices
        .iter()
        .flat_map(|d| {
            d.warnings
                .iter()
                .map(move |w| format!("  {}: {w}", d.path.display()))
        })
        .collect();
    if !notes.is_empty() {
        out.push('\n');
        out.push_str(&paint.bold("Notes"));
        out.push('\n');
        for n in notes {
            out.push_str(&n);
            out.push('\n');
        }
    }
    out
}

/// Run the command.
pub fn run(args: &ListArgs) -> CmdResult {
    let devices = diskrescue_app::devices::list(args.all)?;
    if args.json {
        let text = serde_json::to_string_pretty(&devices)
            .map_err(|e| CliError::failed(format!("cannot format the device list: {e}")))?;
        println!("{text}");
        return Ok(0);
    }
    if devices.is_empty() {
        println!("No disks, cards or USB drives were found.");
        println!(
            "Check that the device is plugged in and recognised by the system. \
             Use --all to include virtual devices, or give an image file directly to the other commands."
        );
        return Ok(0);
    }
    print!("{}", render(&devices, term::width()));
    println!();
    println!("Next: diskrescue info <device>   (details and partitions)");
    println!("      diskrescue image <device> <image file>   (copy a failing device first)");
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskrescue_core::device::PartitionInfo;
    use std::path::PathBuf;

    fn dev(path: &str, kind: DeviceKind) -> DeviceInfo {
        DeviceInfo {
            path: PathBuf::from(path),
            name: path.rsplit('/').next().unwrap().to_string(),
            kind,
            transport: Transport::Usb,
            size: 32_018_267_136,
            logical_sector: 512,
            physical_sector: 512,
            model: Some("Ultra".into()),
            vendor: Some("SanDisk".into()),
            serial: Some("ABC123".into()),
            firmware: None,
            stable_id: None,
            removable: true,
            rotational: false,
            read_only: false,
            partitions: vec![PartitionInfo {
                path: Some(PathBuf::from(format!("{path}1"))),
                number: 1,
                start: 1 << 20,
                size: 32_000_000_000,
                fs_type: Some("vfat".into()),
                mountpoints: vec![PathBuf::from("/media/card")],
            }],
            mountpoints: vec![PathBuf::from("/media/card")],
            health: None,
            warnings: vec!["USB bridge may misreport the drive type".into()],
        }
    }

    #[test]
    fn hides_virtual_and_sorts() {
        let list = vec![
            dev("/dev/sdc", DeviceKind::UsbFlash),
            dev("/dev/loop0", DeviceKind::Virtual),
            dev("/dev/sdb", DeviceKind::SdCard),
        ];
        let v = visible_devices(list.clone(), false);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].path, PathBuf::from("/dev/sdb"));
        assert_eq!(visible_devices(list, true).len(), 3);
    }

    #[test]
    fn table_has_columns_partitions_and_notes() {
        let s = render(&[dev("/dev/sdb", DeviceKind::UsbFlash)], 120);
        for needle in [
            "DEVICE",
            "/dev/sdb",
            "29.8 GiB (32 GB)",
            "USB flash drive",
            "USB",
            "SanDisk Ultra",
            "ABC123",
            "/media/card",
            "/dev/sdb1",
            "vfat",
            "Notes",
            "misreport",
        ] {
            assert!(s.contains(needle), "missing {needle:?} in:\n{s}");
        }
    }

    #[test]
    fn size_text_handles_tiny_and_zero() {
        assert_eq!(size_text(0), "0 B");
        assert_eq!(size_text(512), "512 B");
    }
}
