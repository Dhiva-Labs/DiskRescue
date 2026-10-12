//! Shared building blocks: callouts, badges, buttons, path fields, a
//! switch, cards. Every interactive element has a text label so that
//! screen readers (and the tests) can find it.

use egui::{
    vec2, Align, Color32, CornerRadius, Frame, Layout, Margin, Response, RichText, Sense, Stroke,
    StrokeKind, Ui, WidgetInfo, WidgetText, WidgetType,
};

use crate::icons;
use crate::theme::{self, Kind};

/// A coloured box with a symbol, a title and a body.
pub fn callout<R>(ui: &mut Ui, kind: Kind, title: &str, body: impl FnOnce(&mut Ui) -> R) -> R {
    let pal = theme::palette(ui.visuals(), kind);
    Frame::new()
        .fill(pal.bg)
        .stroke(Stroke::new(1.0, pal.border))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(14, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_top(|ui| {
                let (rect, _) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::hover());
                icons::kind_symbol(ui.painter(), rect.center(), 10.0, kind, pal.fg);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 6.0;
                    if !title.is_empty() {
                        ui.label(RichText::new(title).strong().color(pal.fg));
                    }
                    body(ui)
                })
                .inner
            })
            .inner
        })
        .inner
}

/// A callout with plain text.
pub fn callout_text(ui: &mut Ui, kind: Kind, title: &str, text: &str) {
    callout(ui, kind, title, |ui| {
        if !text.is_empty() {
            ui.label(text);
        }
    });
}

/// A small coloured label ("Mounted", "Deleted").
pub fn badge(ui: &mut Ui, text: &str, kind: Kind) -> Response {
    let pal = theme::palette(ui.visuals(), kind);
    Frame::new()
        .fill(pal.bg)
        .stroke(Stroke::new(1.0, pal.border))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::symmetric(8, 2))
        .show(ui, |ui| {
            ui.label(RichText::new(text).small().strong().color(pal.fg))
        })
        .inner
}

/// The main action of a page: filled with the accent colour.
pub fn primary_button(ui: &mut Ui, enabled: bool, text: &str) -> Response {
    let fill = theme::accent(ui.visuals());
    let b = egui::Button::new(
        RichText::new(text)
            .strong()
            .size(16.0)
            .color(theme::on_accent()),
    )
    .fill(fill)
    .corner_radius(CornerRadius::same(8))
    .min_size(vec2(150.0, 38.0));
    ui.add_enabled(enabled, b)
}

/// A destructive action (red).
pub fn danger_button(ui: &mut Ui, enabled: bool, text: &str) -> Response {
    let fill = if ui.visuals().dark_mode {
        Color32::from_rgb(185, 40, 40)
    } else {
        Color32::from_rgb(200, 35, 35)
    };
    let b = egui::Button::new(RichText::new(text).strong().color(Color32::WHITE))
        .fill(fill)
        .corner_radius(CornerRadius::same(8))
        .min_size(vec2(120.0, 34.0));
    ui.add_enabled(enabled, b)
}

/// An ordinary button with a minimum height.
pub fn button(ui: &mut Ui, enabled: bool, text: &str) -> Response {
    let b = egui::Button::new(text)
        .corner_radius(CornerRadius::same(6))
        .min_size(vec2(0.0, 32.0));
    ui.add_enabled(enabled, b)
}

/// Give `response` the accessible label `label` (for buttons whose text is
/// repeated on the page, like "Rescue" on every drive).
pub fn accessible(response: Response, typ: WidgetType, label: &str) -> Response {
    let enabled = response.enabled();
    let label = label.to_string();
    response.widget_info(|| WidgetInfo::labeled(typ, enabled, label.clone()));
    response
}

/// What a [`path_field`] did this frame.
pub struct PathField {
    /// The text changed.
    pub changed: bool,
    /// The browse button was clicked.
    pub browse: bool,
}

/// A label above an editable path and a browse button.
pub fn path_field(
    ui: &mut Ui,
    label: &str,
    value: &mut String,
    hint: &str,
    browse_text: &str,
    browse_enabled: bool,
) -> PathField {
    let label_resp = ui.label(RichText::new(label).strong());
    let mut out = PathField {
        changed: false,
        browse: false,
    };
    ui.horizontal(|ui| {
        let button_w = 150.0;
        let w = (ui.available_width() - button_w - ui.spacing().item_spacing.x).max(160.0);
        let te = egui::TextEdit::singleline(value)
            .hint_text(hint)
            .desired_width(w)
            .margin(Margin::symmetric(8, 6));
        let r = ui.add(te).labelled_by(label_resp.id);
        out.changed = r.changed();
        let b = ui.add_enabled(
            browse_enabled,
            egui::Button::new(browse_text).min_size(vec2(button_w, 30.0)),
        );
        out.browse = b.clicked();
    });
    out
}

/// An on/off switch with a label; clicking anywhere on it toggles.
pub fn switch(ui: &mut Ui, on: &mut bool, label: &str, enabled: bool) -> Response {
    let text: WidgetText = RichText::new(label).strong().into();
    let galley = text.into_galley(
        ui,
        Some(egui::TextWrapMode::Wrap),
        ui.available_width() - 60.0,
        egui::TextStyle::Body,
    );
    let size = vec2(44.0 + 10.0 + galley.size().x, galley.size().y.max(24.0));
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, mut resp) = ui.allocate_exact_size(size, sense);
    if enabled && resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let label_s = label.to_string();
    let state = *on;
    resp.widget_info(|| {
        WidgetInfo::selected(WidgetType::Checkbox, enabled, state, label_s.clone())
    });
    if ui.is_rect_visible(rect) {
        let t = ui.ctx().animate_bool_responsive(resp.id, *on);
        let track =
            egui::Rect::from_min_size(rect.left_center() - vec2(0.0, 12.0), vec2(44.0, 24.0));
        let accent = theme::accent(ui.visuals());
        let off = ui.visuals().widgets.inactive.bg_fill;
        let fill = if enabled {
            lerp_color(off, accent, t)
        } else {
            off.gamma_multiply(0.6)
        };
        ui.painter().rect_filled(track, 12.0, fill);
        if resp.hovered() && enabled {
            ui.painter().rect_stroke(
                track,
                12.0,
                ui.visuals().widgets.hovered.fg_stroke,
                StrokeKind::Outside,
            );
        }
        let x = egui::lerp(track.left() + 12.0..=track.right() - 12.0, t);
        ui.painter()
            .circle_filled(egui::pos2(x, track.center().y), 9.0, Color32::WHITE);
        let color = if enabled {
            ui.visuals().strong_text_color()
        } else {
            ui.visuals().weak_text_color()
        };
        ui.painter().galley(
            egui::pos2(
                track.right() + 10.0,
                rect.center().y - galley.size().y / 2.0,
            ),
            galley,
            color,
        );
    }
    resp
}

fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

/// A white (or dark) rounded card.
pub fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    let v = ui.visuals();
    Frame::new()
        .fill(theme::card_fill(v))
        .stroke(Stroke::new(1.0, theme::card_border(v)))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::same(16))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

/// A page title with an optional line below it.
pub fn page_title(ui: &mut Ui, title: &str, subtitle: &str) {
    ui.add_space(4.0);
    ui.heading(RichText::new(title).strong());
    if !subtitle.is_empty() {
        ui.label(RichText::new(subtitle).color(ui.visuals().weak_text_color()));
    }
    ui.add_space(6.0);
}

/// A section title.
pub fn section(ui: &mut Ui, title: &str) {
    ui.add_space(6.0);
    ui.label(RichText::new(title).strong().size(17.0));
}

/// Secondary text.
pub fn weak(ui: &mut Ui, text: impl Into<String>) -> Response {
    let c = ui.visuals().weak_text_color();
    ui.label(RichText::new(text.into()).color(c))
}

/// "Label: value" rows in a two-column grid.
pub fn facts(ui: &mut Ui, id: &str, rows: &[(&str, String)]) {
    egui::Grid::new(id)
        .num_columns(2)
        .spacing(vec2(16.0, 6.0))
        .show(ui, |ui| {
            for (k, v) in rows {
                ui.label(RichText::new(*k).color(ui.visuals().weak_text_color()));
                ui.add(egui::Label::new(v.as_str()).wrap());
                ui.end_row();
            }
        });
}

/// A big number with a caption under it.
pub fn stat(ui: &mut Ui, caption: &str, value: &str, kind: Option<Kind>) {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        let mut t = RichText::new(value).strong().size(20.0);
        if let Some(k) = kind {
            t = t.color(theme::palette(ui.visuals(), k).fg);
        }
        ui.label(t);
        ui.label(
            RichText::new(caption)
                .small()
                .color(ui.visuals().weak_text_color()),
        );
    });
}

/// A calm "working" indicator: a three-quarter ring turning slowly
/// (always the same length, unlike egui's spinner, so that it reads the
/// same in every frame). Animates at about 30 frames per second.
pub fn busy_at(ui: &Ui, rect: egui::Rect) {
    let t = ui.input(|i| i.time);
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_millis(33));
    let c = rect.center();
    let r = rect.width().min(rect.height()) / 2.0 - 2.0;
    let start = (t * 4.0 % std::f64::consts::TAU) as f32;
    let n = 28;
    let pts: Vec<egui::Pos2> = (0..=n)
        .map(|i| {
            let a = start + i as f32 / n as f32 * std::f32::consts::TAU * 0.75;
            c + vec2(a.cos(), a.sin()) * r
        })
        .collect();
    let color = theme::accent(ui.visuals());
    ui.painter().add(egui::Shape::line(
        pts,
        Stroke::new((r / 4.0).max(1.5), color),
    ));
}

/// [`busy_at`] in a square of `size` points.
pub fn busy(ui: &mut Ui, size: f32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
    busy_at(ui, rect);
    resp
}

/// One figure of a [`stats`] row.
pub struct Stat {
    /// What it is ("speed").
    pub caption: String,
    /// The figure ("25 MB/s").
    pub value: String,
    /// Its colour.
    pub kind: Option<Kind>,
}

impl Stat {
    /// A figure.
    pub fn new(caption: impl Into<String>, value: impl Into<String>, kind: Option<Kind>) -> Self {
        Stat {
            caption: caption.into(),
            value: value.into(),
            kind,
        }
    }
}

/// Figures in columns of equal width, wrapping onto more rows when the
/// window is narrow.
pub fn stats(ui: &mut Ui, items: &[Stat]) {
    let min_col = 140.0;
    let avail = ui.available_width();
    let cols = ((avail / min_col).floor() as usize).max(1);
    let w = avail / cols as f32;
    let per_row = cols.min(items.len().max(1));
    for chunk in items.chunks(per_row) {
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            for s in chunk {
                ui.allocate_ui_with_layout(vec2(w, 0.0), Layout::top_down(Align::Min), |ui| {
                    ui.set_min_width(w - 10.0);
                    ui.set_max_width(w - 10.0);
                    stat(ui, &s.caption, &s.value, s.kind);
                });
            }
        });
    }
}

/// The steps of a wizard: done ones ticked, the current one highlighted.
pub fn steps(ui: &mut Ui, current: usize, names: &[&str]) {
    ui.horizontal(|ui| {
        for (i, name) in names.iter().enumerate() {
            let pal_good = theme::palette(ui.visuals(), Kind::Good).fg;
            let accent = theme::accent(ui.visuals());
            let weak = ui.visuals().weak_text_color();
            let (rect, _) = ui.allocate_exact_size(vec2(24.0, 24.0), Sense::hover());
            let p = ui.painter();
            if i < current {
                icons::check(p, rect.center(), 11.0, pal_good);
            } else {
                let c = if i == current { accent } else { weak };
                p.circle_filled(rect.center(), 11.0, c);
                p.text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    format!("{}", i + 1),
                    egui::FontId::proportional(13.0),
                    Color32::WHITE,
                );
            }
            let text = RichText::new(*name);
            let text = if i == current {
                text.strong()
            } else {
                text.color(weak)
            };
            ui.label(text);
            if i + 1 < names.len() {
                let (r, _) = ui.allocate_exact_size(vec2(36.0, 24.0), Sense::hover());
                ui.painter().line_segment(
                    [
                        r.left_center() + vec2(4.0, 0.0),
                        r.right_center() - vec2(4.0, 0.0),
                    ],
                    Stroke::new(1.5, weak),
                );
            }
        }
    });
}

/// A row with content on the left and on the right.
pub fn sides(ui: &mut Ui, left: impl FnOnce(&mut Ui), right: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        left(ui);
        ui.with_layout(Layout::right_to_left(Align::Center), right);
    });
}

/// Clean a message written for the command line: drop sentences and
/// parentheses that name command-line options or commands, which mean
/// nothing in the desktop app.
pub fn gui_text(msg: &str) -> String {
    let mut s = String::new();
    // Parentheses mentioning an option.
    let mut depth = 0usize;
    let mut buf = String::new();
    for ch in msg.chars() {
        match ch {
            '(' => {
                if depth == 0 {
                    s.push_str(&buf);
                    buf.clear();
                }
                depth += 1;
                buf.push(ch);
            }
            ')' if depth > 0 => {
                buf.push(ch);
                depth -= 1;
                if depth == 0 {
                    if !buf.contains("--") {
                        s.push_str(&buf);
                    } else if s.ends_with(' ') {
                        s.pop();
                    }
                    buf.clear();
                }
            }
            _ => buf.push(ch),
        }
    }
    s.push_str(&buf);
    let cli = |t: &str| t.contains("--") || t.contains("diskrescue ") || t.contains("'diskrescue");
    let mut out: Vec<String> = Vec::new();
    for part in split_sentences(&s) {
        let t = part.trim();
        if t.is_empty() {
            continue;
        }
        if cli(t) {
            // Keep what comes before "with: diskrescue ..." style commands.
            if let Some(i) = t
                .find(" with: diskrescue")
                .or_else(|| t.find(": diskrescue"))
            {
                let head = t[..i].trim_end_matches(',');
                if !head.contains("--") {
                    out.push(format!("{head}."));
                }
            } else if let Some(i) = t.find(", or add --") {
                out.push(format!("{}.", &t[..i]));
            }
            continue;
        }
        out.push(t.to_string());
    }
    let mut joined = out.join(" ");
    if let Some(first) = joined.chars().next() {
        if first.is_lowercase() {
            joined = first.to_uppercase().collect::<String>() + &joined[first.len_utf8()..];
        }
    }
    if joined.is_empty() {
        msg.to_string()
    } else {
        joined
    }
}

fn split_sentences(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let chars: Vec<char> = s.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        cur.push(c);
        let end = matches!(c, '.' | ';')
            && chars.get(i + 1).is_none_or(|n| n.is_whitespace())
            && !cur.ends_with("e.g.")
            && !cur.ends_with("i.e.");
        if end {
            out.push(cur.trim().to_string());
            cur.clear();
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_line_hints_are_removed() {
        let m = "/x/card.img already exists, but there is no progress map (/x/card.img.map) to resume \
                 from. DiskRescue will not overwrite it. Choose a new image name, or add --force if \
                 the file can be replaced.";
        let t = gui_text(m);
        assert!(!t.contains("--force"), "{t}");
        assert!(t.contains("Choose a new image name."), "{t}");
        let m = "the output is on the source (try again with --donor <a file>)";
        assert_eq!(gui_text(m), "The output is on the source");
        let m = "the first letter was lost; restored as \"IMG_0004.JPG\" from the other files.";
        assert_eq!(
            gui_text(m),
            "The first letter was lost; restored as \"IMG_0004.JPG\" from the other files."
        );
        let m =
            "there is not enough free space for the image. x will hold 1 GiB but only 2 MiB is \
                 free on that disk. Choose a location with more room, for example an external \
                 drive: diskrescue image /dev/sdb /path/on/another/disk/x";
        let t = gui_text(m);
        assert!(t.ends_with("for example an external drive."), "{t}");
        assert!(t.starts_with("There is not enough"), "{t}");
    }
}
