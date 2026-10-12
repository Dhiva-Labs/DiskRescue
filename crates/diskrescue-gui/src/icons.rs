//! Icons drawn with shapes (no image files, crisp at every scale).

use diskrescue_core::device::DeviceKind;
use diskrescue_core::media::MediaCategory;
use egui::{pos2, vec2, Align2, Color32, FontId, Painter, Pos2, Rect, Shape, Stroke, StrokeKind};

fn soft(c: Color32) -> Color32 {
    c.gamma_multiply(0.18)
}

/// The kinds of drive icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriveIcon {
    /// USB pen drive.
    UsbStick,
    /// SD or other memory card.
    SdCard,
    /// Spinning hard disk.
    Hdd,
    /// Solid-state disk.
    Ssd,
    /// CD/DVD drive.
    Optical,
    /// An image file.
    ImageFile,
    /// Anything else.
    Generic,
}

impl DriveIcon {
    /// The icon for a kind of device.
    pub fn for_kind(kind: DeviceKind) -> Self {
        match kind {
            DeviceKind::UsbFlash => DriveIcon::UsbStick,
            DeviceKind::SdCard | DeviceKind::Mmc => DriveIcon::SdCard,
            DeviceKind::Hdd | DeviceKind::UsbHdd => DriveIcon::Hdd,
            DeviceKind::SataSsd | DeviceKind::NvmeSsd | DeviceKind::UsbSsd => DriveIcon::Ssd,
            DeviceKind::Optical => DriveIcon::Optical,
            DeviceKind::ImageFile => DriveIcon::ImageFile,
            DeviceKind::Virtual | DeviceKind::Unknown => DriveIcon::Generic,
        }
    }
}

/// Draw a drive icon filling `rect` (square-ish) in colour `c`.
pub fn drive(p: &Painter, rect: Rect, icon: DriveIcon, c: Color32) {
    let s = rect.width().min(rect.height());
    let r = Rect::from_center_size(rect.center(), vec2(s, s));
    let w = (s / 24.0).max(1.5);
    let stroke = Stroke::new(w, c);
    let at = |x: f32, y: f32| pos2(r.left() + x * s, r.top() + y * s);
    match icon {
        DriveIcon::UsbStick => {
            let plug = Rect::from_min_max(at(0.38, 0.08), at(0.62, 0.30));
            p.rect_stroke(plug, 2.0, stroke, StrokeKind::Inside);
            for x in [0.45, 0.55] {
                let hole = Rect::from_center_size(at(x, 0.17), vec2(s * 0.05, s * 0.06));
                p.rect_filled(hole, 0.0, c);
            }
            let body = Rect::from_min_max(at(0.30, 0.30), at(0.70, 0.92));
            p.rect_filled(body, s * 0.06, soft(c));
            p.rect_stroke(body, s * 0.06, stroke, StrokeKind::Inside);
            p.circle_filled(at(0.5, 0.78), s * 0.04, c);
        }
        DriveIcon::SdCard => {
            let pts = vec![
                at(0.24, 0.08),
                at(0.62, 0.08),
                at(0.78, 0.24),
                at(0.78, 0.92),
                at(0.24, 0.92),
            ];
            p.add(Shape::convex_polygon(pts.clone(), soft(c), Stroke::NONE));
            let mut closed = pts;
            closed.push(at(0.24, 0.08));
            p.add(Shape::line(closed, stroke));
            for i in 0..4 {
                let x = 0.32 + i as f32 * 0.09;
                let contact = Rect::from_min_size(at(x, 0.14), vec2(s * 0.05, s * 0.14));
                p.rect_filled(contact, 1.0, c);
            }
            p.line_segment([at(0.34, 0.66), at(0.66, 0.66)], stroke);
            p.line_segment([at(0.34, 0.76), at(0.58, 0.76)], stroke);
        }
        DriveIcon::Hdd => {
            let body = Rect::from_min_max(at(0.14, 0.10), at(0.86, 0.90));
            p.rect_filled(body, s * 0.07, soft(c));
            p.rect_stroke(body, s * 0.07, stroke, StrokeKind::Inside);
            p.circle_stroke(at(0.5, 0.44), s * 0.24, stroke);
            p.circle_filled(at(0.5, 0.44), s * 0.05, c);
            p.line_segment([at(0.74, 0.76), at(0.56, 0.50)], Stroke::new(w * 1.3, c));
            p.circle_filled(at(0.74, 0.76), s * 0.045, c);
            for (x, y) in [(0.21, 0.16), (0.79, 0.16), (0.21, 0.84)] {
                p.circle_filled(at(x, y), s * 0.02, c);
            }
        }
        DriveIcon::Ssd => {
            let body = Rect::from_min_max(at(0.10, 0.20), at(0.90, 0.80));
            p.rect_filled(body, s * 0.06, soft(c));
            p.rect_stroke(body, s * 0.06, stroke, StrokeKind::Inside);
            for i in 0..3 {
                let x = 0.18 + i as f32 * 0.22;
                let chip = Rect::from_min_size(at(x, 0.32), vec2(s * 0.17, s * 0.22));
                p.rect_stroke(chip, 2.0, stroke, StrokeKind::Inside);
            }
            p.line_segment([at(0.18, 0.66), at(0.60, 0.66)], stroke);
            p.circle_filled(at(0.78, 0.66), s * 0.035, c);
        }
        DriveIcon::Optical => {
            p.circle_filled(at(0.5, 0.5), s * 0.40, soft(c));
            p.circle_stroke(at(0.5, 0.5), s * 0.40, stroke);
            p.circle_stroke(at(0.5, 0.5), s * 0.12, stroke);
            p.circle_filled(at(0.5, 0.5), s * 0.04, c);
        }
        DriveIcon::ImageFile => document(p, r, c, stroke, Some("IMG")),
        DriveIcon::Generic => {
            let body = Rect::from_min_max(at(0.12, 0.28), at(0.88, 0.72));
            p.rect_filled(body, s * 0.08, soft(c));
            p.rect_stroke(body, s * 0.08, stroke, StrokeKind::Inside);
            p.circle_filled(at(0.76, 0.5), s * 0.045, c);
            p.line_segment([at(0.22, 0.5), at(0.56, 0.5)], stroke);
        }
    }
}

fn document(p: &Painter, r: Rect, c: Color32, stroke: Stroke, text: Option<&str>) {
    let s = r.width();
    let at = |x: f32, y: f32| pos2(r.left() + x * s, r.top() + y * s);
    let pts = vec![
        at(0.22, 0.08),
        at(0.62, 0.08),
        at(0.80, 0.26),
        at(0.80, 0.92),
        at(0.22, 0.92),
    ];
    p.add(Shape::convex_polygon(pts.clone(), soft(c), Stroke::NONE));
    let mut closed = pts;
    closed.push(at(0.22, 0.08));
    p.add(Shape::line(closed, stroke));
    p.add(Shape::line(
        vec![at(0.62, 0.08), at(0.62, 0.26), at(0.80, 0.26)],
        stroke,
    ));
    if let Some(t) = text {
        p.text(
            at(0.51, 0.62),
            Align2::CENTER_CENTER,
            t,
            FontId::proportional(s * 0.17),
            c,
        );
    }
}

/// Draw the icon of a kind of file filling `rect`.
pub fn file(p: &Painter, rect: Rect, category: MediaCategory, c: Color32) {
    let s = rect.width().min(rect.height());
    let r = Rect::from_center_size(rect.center(), vec2(s, s));
    let w = (s / 26.0).max(1.5);
    let stroke = Stroke::new(w, c);
    let at = |x: f32, y: f32| pos2(r.left() + x * s, r.top() + y * s);
    match category {
        MediaCategory::Video => {
            let frame = Rect::from_min_max(at(0.10, 0.22), at(0.90, 0.78));
            p.rect_filled(frame, s * 0.06, soft(c));
            p.rect_stroke(frame, s * 0.06, stroke, StrokeKind::Inside);
            p.add(Shape::convex_polygon(
                vec![at(0.42, 0.36), at(0.64, 0.50), at(0.42, 0.64)],
                c,
                Stroke::NONE,
            ));
        }
        MediaCategory::Audio => {
            p.circle_filled(at(0.36, 0.72), s * 0.11, c);
            p.circle_filled(at(0.70, 0.64), s * 0.11, c);
            p.line_segment([at(0.46, 0.72), at(0.46, 0.22)], Stroke::new(w * 1.4, c));
            p.line_segment([at(0.80, 0.64), at(0.80, 0.14)], Stroke::new(w * 1.4, c));
            p.add(Shape::convex_polygon(
                vec![
                    at(0.46, 0.22),
                    at(0.80, 0.14),
                    at(0.80, 0.26),
                    at(0.46, 0.34),
                ],
                c,
                Stroke::NONE,
            ));
        }
        MediaCategory::Photo => photo(p, r, c, stroke),
        MediaCategory::RawPhoto => {
            photo(p, r, c, stroke);
            let tag = Rect::from_min_max(at(0.54, 0.62), at(0.92, 0.86));
            p.rect_filled(tag, 3.0, c);
            p.text(
                tag.center(),
                Align2::CENTER_CENTER,
                "RAW",
                FontId::proportional(s * 0.13),
                Color32::WHITE,
            );
        }
        MediaCategory::Document | MediaCategory::Other => document(p, r, c, stroke, None),
    }
}

fn photo(p: &Painter, r: Rect, c: Color32, stroke: Stroke) {
    let s = r.width();
    let at = |x: f32, y: f32| pos2(r.left() + x * s, r.top() + y * s);
    let frame = Rect::from_min_max(at(0.10, 0.18), at(0.90, 0.82));
    p.rect_filled(frame, s * 0.06, soft(c));
    p.rect_stroke(frame, s * 0.06, stroke, StrokeKind::Inside);
    p.add(Shape::convex_polygon(
        vec![at(0.16, 0.76), at(0.40, 0.44), at(0.60, 0.76)],
        c.gamma_multiply(0.7),
        Stroke::NONE,
    ));
    p.add(Shape::convex_polygon(
        vec![at(0.44, 0.76), at(0.64, 0.52), at(0.84, 0.76)],
        c,
        Stroke::NONE,
    ));
    p.circle_filled(at(0.68, 0.34), s * 0.07, c);
}

/// A filled circle with a check mark.
pub fn check(p: &Painter, center: Pos2, radius: f32, c: Color32) {
    p.circle_filled(center, radius, c);
    let w = Stroke::new((radius / 4.5).max(1.5), Color32::WHITE);
    let pts = vec![
        center + vec2(-radius * 0.45, 0.0),
        center + vec2(-radius * 0.1, radius * 0.38),
        center + vec2(radius * 0.5, -radius * 0.35),
    ];
    p.add(Shape::line(pts, w));
}

/// A filled circle with a cross.
pub fn cross(p: &Painter, center: Pos2, radius: f32, c: Color32) {
    p.circle_filled(center, radius, c);
    let w = Stroke::new((radius / 4.5).max(1.5), Color32::WHITE);
    let d = radius * 0.4;
    p.line_segment([center + vec2(-d, -d), center + vec2(d, d)], w);
    p.line_segment([center + vec2(-d, d), center + vec2(d, -d)], w);
}

/// An empty circle (a step not started yet).
pub fn pending(p: &Painter, center: Pos2, radius: f32, c: Color32) {
    p.circle_stroke(center, radius - 1.0, Stroke::new(1.5, c));
}

/// The symbol of a message kind: "!" for problems, "i" for information,
/// a check mark for good news.
pub fn kind_symbol(p: &Painter, center: Pos2, radius: f32, kind: crate::theme::Kind, c: Color32) {
    use crate::theme::Kind;
    match kind {
        Kind::Good => check(p, center, radius, c),
        Kind::Bad | Kind::Warn => {
            p.circle_filled(center, radius, c);
            p.text(
                center,
                Align2::CENTER_CENTER,
                "!",
                FontId::proportional(radius * 1.5),
                Color32::WHITE,
            );
        }
        Kind::Info | Kind::Neutral => {
            p.circle_filled(center, radius, c);
            p.text(
                center + vec2(0.0, 0.5),
                Align2::CENTER_CENTER,
                "i",
                FontId::proportional(radius * 1.5),
                Color32::WHITE,
            );
        }
    }
}
