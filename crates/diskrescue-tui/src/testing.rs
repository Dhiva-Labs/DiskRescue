//! Helpers for tests (also used by the integration tests): fixture
//! devices, key presses and a text rendering of the screen.

#![doc(hidden)]

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use diskrescue_core::device::{DeviceInfo, DeviceKind, HealthInfo, PartitionInfo, Transport};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

use crate::app::App;
use crate::config::FixedDevice;
use crate::event::Event;

/// A device as the system would describe it.
pub fn device(path: &str, kind: DeviceKind, size: u64) -> DeviceInfo {
    let name = path.rsplit('/').next().unwrap_or(path).to_string();
    DeviceInfo {
        path: PathBuf::from(path),
        name,
        kind,
        transport: if kind.is_usb() {
            Transport::Usb
        } else {
            Transport::Ata
        },
        size,
        logical_sector: 512,
        physical_sector: 512,
        model: None,
        vendor: None,
        serial: None,
        firmware: None,
        stable_id: None,
        removable: kind.is_usb() || kind.is_card(),
        rotational: kind.is_rotational(),
        read_only: false,
        partitions: vec![],
        mountpoints: vec![],
        health: None,
        warnings: vec![],
    }
}

/// A USB flash drive with a failing health check and one FAT32 partition.
pub fn failing_usb_stick() -> FixedDevice {
    let mut d = device("/dev/fixture-sdb", DeviceKind::UsbFlash, 32_018_267_136);
    d.vendor = Some("SanDisk".into());
    d.model = Some("Ultra".into());
    d.serial = Some("4C530001".into());
    d.health = Some(HealthInfo {
        passed: Some(false),
        pending_sectors: Some(12),
        ..HealthInfo::default()
    });
    d.partitions.push(PartitionInfo {
        path: Some(PathBuf::from("/dev/fixture-sdb1")),
        number: 1,
        start: 1 << 20,
        size: 32_000_000_000,
        fs_type: Some("vfat".into()),
        mountpoints: vec![],
    });
    FixedDevice {
        info: d,
        needs_root: false,
    }
}

/// An internal disk that holds the running system.
pub fn system_disk() -> FixedDevice {
    let mut d = device("/dev/fixture-nvme0n1", DeviceKind::NvmeSsd, 512_110_190_592);
    d.model = Some("Samsung SSD 970".into());
    d.mountpoints = vec![PathBuf::from("/"), PathBuf::from("/boot/efi")];
    d.partitions.push(PartitionInfo {
        path: Some(PathBuf::from("/dev/fixture-nvme0n1p2")),
        number: 2,
        start: 1 << 30,
        size: 500_000_000_000,
        fs_type: Some("ext4".into()),
        mountpoints: vec![PathBuf::from("/")],
    });
    FixedDevice {
        info: d,
        needs_root: false,
    }
}

/// A key press.
pub fn key(code: KeyCode) -> Event {
    Event::Key(KeyEvent {
        code,
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    })
}

/// A character key.
pub fn ch(c: char) -> Event {
    key(KeyCode::Char(c))
}

/// Ctrl and a character.
pub fn ctrl(c: char) -> Event {
    Event::Key(KeyEvent {
        code: KeyCode::Char(c),
        modifiers: KeyModifiers::CONTROL,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    })
}

/// Type `text` one key at a time.
pub fn type_text(app: &mut App, text: &str) {
    for c in text.chars() {
        app.handle_event(ch(c));
    }
}

/// Press a key.
pub fn press(app: &mut App, code: KeyCode) {
    app.handle_event(key(code));
}

/// Draw the app into a terminal of `w` x `h` characters and return the
/// screen as text, one line per row.
pub fn render(app: &App, w: u16, h: u16) -> String {
    let mut term = Terminal::new(TestBackend::new(w, h)).expect("test terminal");
    term.draw(|f| app.draw(f)).expect("draw");
    let buf = term.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..h {
        let mut line = String::new();
        for x in 0..w {
            line.push_str(buf[(x, y)].symbol());
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// Like [`render`], but keeps the styles: returns the buffer.
pub fn render_buffer(app: &App, w: u16, h: u16) -> ratatui::buffer::Buffer {
    let mut term = Terminal::new(TestBackend::new(w, h)).expect("test terminal");
    term.draw(|f| app.draw(f)).expect("draw");
    term.backend().buffer().clone()
}

/// Send ticks until `done(app)` or `limit` passes; returns whether it came
/// true.
pub fn tick_until(
    app: &mut App,
    limit: std::time::Duration,
    mut done: impl FnMut(&App) -> bool,
) -> bool {
    let start = std::time::Instant::now();
    loop {
        app.handle_event(Event::Tick);
        if done(app) {
            return true;
        }
        if start.elapsed() > limit {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}
