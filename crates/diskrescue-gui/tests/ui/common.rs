//! Shared helpers for the user interface tests. They run headless (no
//! window is ever opened) and never open a real block device: drives are
//! made up, sources are image files.

#![allow(dead_code)]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use diskrescue_app::jobs::{start_image, ImageState};
use diskrescue_app::plan::{ImagePlan, RecoverPlan};
use diskrescue_app::preflight::{Preflight, ResolvedRecover};
use diskrescue_app::JobHandle;
use diskrescue_core::device::{DeviceInfo, DeviceKind, HealthInfo, Transport};
use diskrescue_fixtures::sets::{self, Scenario};
use diskrescue_gui::{Backend, DiskRescueApp, Saved, SystemBackend};
use egui_kittest::kittest::{NodeT, Queryable};
use egui_kittest::Harness;

/// Made-up drives; preflight is real unless a fixed result is set.
pub struct FakeBackend {
    pub devices: Vec<DeviceInfo>,
    pub needs_privilege: HashSet<PathBuf>,
    pub preflight: Mutex<Option<Preflight<ResolvedRecover>>>,
}

impl FakeBackend {
    pub fn new(devices: Vec<DeviceInfo>) -> Self {
        FakeBackend {
            devices,
            needs_privilege: HashSet::new(),
            preflight: Mutex::new(None),
        }
    }
}

impl Backend for FakeBackend {
    fn list_devices(&self) -> Result<Vec<DeviceInfo>, String> {
        Ok(self.devices.clone())
    }

    fn needs_privilege(&self, path: &Path) -> bool {
        self.needs_privilege.contains(path)
    }

    fn preflight_recover(&self, plan: &RecoverPlan) -> Preflight<ResolvedRecover> {
        match self.preflight.lock().unwrap().clone() {
            Some(p) => p,
            None => SystemBackend.preflight_recover(plan),
        }
    }

    /// Never asks for a password: the "administrator" copy runs in this
    /// process with normal rights (sources are image files).
    fn start_image_privileged(
        &self,
        plan: ImagePlan,
    ) -> diskrescue_app::Result<JobHandle<ImageState>> {
        Ok(start_image(plan))
    }
}

pub fn device(path: &str, kind: DeviceKind, vendor: &str, model: &str, size: u64) -> DeviceInfo {
    DeviceInfo {
        path: PathBuf::from(path),
        name: path.rsplit('/').next().unwrap_or(path).to_string(),
        kind,
        transport: match kind {
            DeviceKind::UsbFlash | DeviceKind::UsbHdd | DeviceKind::UsbSsd => Transport::Usb,
            DeviceKind::SdCard => Transport::Mmc,
            DeviceKind::NvmeSsd => Transport::Nvme,
            _ => Transport::Ata,
        },
        size,
        logical_sector: 512,
        physical_sector: 512,
        model: (!model.is_empty()).then(|| model.to_string()),
        vendor: (!vendor.is_empty()).then(|| vendor.to_string()),
        serial: None,
        firmware: None,
        stable_id: None,
        removable: matches!(kind, DeviceKind::UsbFlash | DeviceKind::SdCard),
        rotational: matches!(kind, DeviceKind::Hdd | DeviceKind::UsbHdd),
        read_only: false,
        partitions: vec![],
        mountpoints: vec![],
        health: None,
        warnings: vec![],
    }
}

/// A USB stick (mounted), an SD card, a failing USB hard disk, an SSD and
/// the system disk.
pub fn fake_devices() -> Vec<DeviceInfo> {
    let mut stick = device(
        "/dev/sdb",
        DeviceKind::UsbFlash,
        "SanDisk",
        "Ultra",
        32_017_047_552,
    );
    stick.mountpoints = vec![PathBuf::from("/media/me/SANDISK")];
    let card = device(
        "/dev/mmcblk0",
        DeviceKind::SdCard,
        "",
        "SD64G",
        63_864_569_856,
    );
    let mut hdd = device(
        "/dev/sdc",
        DeviceKind::UsbHdd,
        "WD",
        "Elements 25A2",
        1_000_204_886_016,
    );
    hdd.health = Some(HealthInfo {
        passed: Some(false),
        pending_sectors: Some(24),
        reallocated_sectors: Some(310),
        ..HealthInfo::default()
    });
    let ssd = device(
        "/dev/sdd",
        DeviceKind::SataSsd,
        "Samsung",
        "SSD 870 EVO",
        500_107_862_016,
    );
    let mut sys = device(
        "/dev/nvme0n1",
        DeviceKind::NvmeSsd,
        "",
        "WD Black SN770",
        1_000_204_886_016,
    );
    sys.mountpoints = vec![PathBuf::from(if cfg!(windows) { "C:\\" } else { "/" })];
    vec![stick, card, hdd, ssd, sys]
}

pub const SIZE: egui::Vec2 = egui::vec2(1180.0, 820.0);

pub fn harness_with(app: DiskRescueApp, theme: egui::Theme) -> Harness<'static, DiskRescueApp> {
    Harness::builder()
        .with_size(SIZE)
        .with_theme(theme)
        .with_max_steps(64)
        .build_eframe(move |_cc| app)
}

pub fn harness(app: DiskRescueApp) -> Harness<'static, DiskRescueApp> {
    harness_with(app, egui::Theme::Light)
}

pub fn fake_app(devices: Vec<DeviceInfo>) -> (DiskRescueApp, Arc<FakeBackend>) {
    let backend = Arc::new(FakeBackend::new(devices));
    let app = DiskRescueApp::with_backend(backend.clone(), Saved::default());
    (app, backend)
}

/// Step the harness (sleeping a little between frames, so that background
/// threads progress) until `cond` holds; panics after `limit`.
pub fn wait_for(
    h: &mut Harness<'_, DiskRescueApp>,
    limit: Duration,
    what: &str,
    mut cond: impl FnMut(&Harness<'_, DiskRescueApp>) -> bool,
) {
    let t = Instant::now();
    loop {
        h.step();
        if cond(h) {
            h.step();
            return;
        }
        assert!(t.elapsed() < limit, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

pub fn has_label(h: &Harness<'_, DiskRescueApp>, label: &str) -> bool {
    h.query_by_label(label).is_some()
}

pub fn has_label_containing(h: &Harness<'_, DiskRescueApp>, label: &str) -> bool {
    h.query_by_label_contains(label).is_some()
}

pub fn enabled(h: &Harness<'_, DiskRescueApp>, label: &str) -> bool {
    h.query_by_label(label)
        .is_some_and(|n| !n.accesskit_node().is_disabled())
}

/// Replace the text of the field labelled `label` with `text`.
pub fn type_into(h: &mut Harness<'_, DiskRescueApp>, label: &str, text: &str) {
    h.get_by_label(label).focus();
    h.step();
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
    h.step();
    h.get_by_label(label).type_text(text);
    h.step();
}

/// Click the element labelled `label` (through AccessKit, like a screen
/// reader, so that elements scrolled out of view work too).
pub fn click(h: &mut Harness<'_, DiskRescueApp>, label: &str) {
    h.get_by_label(label).click_accesskit();
    h.step();
    h.step();
}

/// Where the screenshots go.
pub fn screenshot_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/screenshots")
}

/// GPU rendering from several test threads at once crashes some drivers:
/// harnesses render (and release their renderer) one at a time.
static RENDER: Mutex<()> = Mutex::new(());

/// Hold while rendering (and while dropping a harness that rendered).
pub fn render_lock() -> std::sync::MutexGuard<'static, ()> {
    RENDER.lock().unwrap_or_else(|p| p.into_inner())
}

/// Render the last frame of a test; save it as
/// `docs/screenshots/<name>.png` when `DISKRESCUE_SCREENSHOTS` is set (so
/// that ordinary test runs leave the tree clean). Ends the test's harness.
pub fn screenshot(mut h: Harness<'static, DiskRescueApp>, name: &str) {
    let _g = render_lock();
    render_and_save(&mut h, name);
    drop(h);
}

fn render_and_save(h: &mut Harness<'_, DiskRescueApp>, name: &str) {
    // A few frames so that thumbnails and layout settle; no mouse pointer.
    h.remove_cursor();
    for _ in 0..3 {
        h.step();
    }
    let img = h.render().expect("headless rendering");
    assert!(img.width() > 0 && img.height() > 0);
    if std::env::var_os("DISKRESCUE_SCREENSHOTS").is_some() {
        let dir = screenshot_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{name}.png"));
        let (w, hgt) = (img.width(), img.height());
        write_png(&path, w, hgt, img.as_raw(), false);
        if std::fs::metadata(&path).unwrap().len() >= 300 * 1024 {
            write_png(&path, w, hgt, img.as_raw(), true);
        }
        let len = std::fs::metadata(&path).unwrap().len();
        assert!(len < 300 * 1024, "{} is {} KiB", path.display(), len / 1024);
    }
}

/// Write RGBA pixels as a PNG: true colour, or with a 256-colour palette
/// (screens with photos).
fn write_png(path: &Path, w: u32, h: u32, rgba: &[u8], indexed: bool) {
    let file = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    let mut enc = png::Encoder::new(file, w, h);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::Best);
    enc.set_adaptive_filter(png::AdaptiveFilterType::Adaptive);
    if indexed {
        let nq = color_quant::NeuQuant::new(3, 256, rgba);
        let mut palette = Vec::with_capacity(768);
        for c in nq.color_map_rgba().chunks(4) {
            palette.extend_from_slice(&c[..3]);
        }
        let idx: Vec<u8> = rgba.chunks(4).map(|p| nq.index_of(p) as u8).collect();
        enc.set_color(png::ColorType::Indexed);
        enc.set_palette(palette);
        let mut wr = enc.write_header().unwrap();
        wr.write_image_data(&idx).unwrap();
    } else {
        enc.set_color(png::ColorType::Rgba);
        let mut wr = enc.write_header().unwrap();
        wr.write_image_data(rgba).unwrap();
    }
}

fn cached(
    name: &str,
    ok: impl Fn(&Path, &Scenario) -> bool,
    build: fn(&Path) -> diskrescue_fixtures::Result<Scenario>,
) -> Option<Scenario> {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("fixtures")
        .join(name);
    if let Ok(sc) = Scenario::load(&dir) {
        if ok(&dir, &sc) {
            return Some(sc);
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    match build(&dir) {
        Ok(sc) => Some(sc),
        Err(e) if e.is_missing() => {
            eprintln!("skipped: {e}");
            None
        }
        Err(e) => panic!("cannot build the {name} fixture: {e}"),
    }
}

static BUILD: Mutex<()> = Mutex::new(());

pub fn camera_card() -> Option<&'static Scenario> {
    static CELL: OnceLock<Option<Scenario>> = OnceLock::new();
    CELL.get_or_init(|| {
        let _g = BUILD.lock().unwrap_or_else(|p| p.into_inner());
        cached(
            "camera-sd-card",
            |_, sc| sc.disk_image.as_ref().is_some_and(|p| p.exists()),
            sets::camera_sd_card,
        )
    })
    .as_ref()
}

pub fn damaged_media() -> Option<&'static Scenario> {
    static CELL: OnceLock<Option<Scenario>> = OnceLock::new();
    CELL.get_or_init(|| {
        let _g = BUILD.lock().unwrap_or_else(|p| p.into_inner());
        cached(
            "damaged-media",
            |dir, _| dir.join("no_moov.mp4").exists(),
            sets::damaged_media,
        )
    })
    .as_ref()
}

/// A temporary folder inside the target folder (disk-backed).
pub fn scratch(prefix: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap()
}

/// A dropped file for `RawInput::dropped_files`.
#[derive(Debug)]
pub struct Dropped(pub PathBuf);

impl egui::DroppedFile for Dropped {
    fn path(&self) -> &Path {
        &self.0
    }

    fn bytes(&self) -> Result<Vec<u8>, String> {
        std::fs::read(&self.0).map_err(|e| e.to_string())
    }
}

pub fn drop_files(h: &mut Harness<'_, DiskRescueApp>, files: &[PathBuf]) {
    for f in files {
        h.input_mut()
            .dropped_files
            .push(Arc::new(Dropped(f.clone())));
    }
    h.step();
    h.step();
}
