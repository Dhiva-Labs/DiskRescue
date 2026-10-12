//! Colours, text sizes and spacing. Light and dark follow the system; every
//! colour here has a light and a dark variant.

use diskrescue_app::text::Tone;
use egui::{Color32, FontId, TextStyle, Theme, Visuals};

/// What a message or a badge means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    /// Good news.
    Good,
    /// Needs attention.
    Warn,
    /// A problem.
    Bad,
    /// Information.
    Info,
    /// Neither.
    Neutral,
}

impl From<Tone> for Kind {
    fn from(t: Tone) -> Self {
        match t {
            Tone::Good => Kind::Good,
            Tone::Warn => Kind::Warn,
            Tone::Bad => Kind::Bad,
            Tone::Neutral => Kind::Neutral,
        }
    }
}

/// Foreground, background and border of a [`Kind`].
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    /// Text and icons.
    pub fg: Color32,
    /// Fill of a callout or badge.
    pub bg: Color32,
    /// Border.
    pub border: Color32,
}

const fn rgb(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}

/// The colours of `kind` for the current light or dark mode.
pub fn palette(v: &Visuals, kind: Kind) -> Palette {
    let dark = v.dark_mode;
    let (fg, bg, border) = match (kind, dark) {
        (Kind::Good, false) => (rgb(21, 110, 52), rgb(226, 246, 233), rgb(134, 205, 158)),
        (Kind::Good, true) => (rgb(110, 225, 150), rgb(18, 46, 29), rgb(37, 105, 63)),
        (Kind::Warn, false) => (rgb(146, 64, 14), rgb(254, 244, 214), rgb(240, 190, 90)),
        (Kind::Warn, true) => (rgb(250, 204, 92), rgb(52, 38, 10), rgb(130, 95, 25)),
        (Kind::Bad, false) => (rgb(170, 28, 28), rgb(254, 231, 231), rgb(240, 150, 150)),
        (Kind::Bad, true) => (rgb(252, 140, 140), rgb(58, 18, 18), rgb(140, 45, 45)),
        (Kind::Info, false) => (rgb(29, 78, 186), rgb(229, 238, 254), rgb(150, 182, 245)),
        (Kind::Info, true) => (rgb(150, 190, 252), rgb(20, 32, 60), rgb(50, 80, 140)),
        (Kind::Neutral, false) => (rgb(70, 76, 88), rgb(241, 242, 245), rgb(205, 208, 215)),
        (Kind::Neutral, true) => (rgb(170, 176, 188), rgb(36, 39, 45), rgb(70, 75, 85)),
    };
    Palette { fg, bg, border }
}

/// The accent colour of primary buttons and selections.
pub fn accent(v: &Visuals) -> Color32 {
    if v.dark_mode {
        rgb(59, 130, 246)
    } else {
        rgb(37, 99, 235)
    }
}

/// Text on top of [`accent`].
pub fn on_accent() -> Color32 {
    Color32::WHITE
}

/// Background of cards.
pub fn card_fill(v: &Visuals) -> Color32 {
    if v.dark_mode {
        rgb(32, 34, 39)
    } else {
        Color32::WHITE
    }
}

/// Border of cards.
pub fn card_border(v: &Visuals) -> Color32 {
    if v.dark_mode {
        rgb(58, 62, 70)
    } else {
        rgb(218, 221, 228)
    }
}

/// Background of the page behind cards.
pub fn page_fill(v: &Visuals) -> Color32 {
    if v.dark_mode {
        rgb(24, 25, 29)
    } else {
        rgb(246, 247, 250)
    }
}

/// Background of the navigation column.
pub fn nav_fill(v: &Visuals) -> Color32 {
    if v.dark_mode {
        rgb(18, 19, 22)
    } else {
        rgb(235, 238, 244)
    }
}

/// Colours of the block map.
pub struct MapColors {
    /// Copied.
    pub rescued: Color32,
    /// Not read yet.
    pub not_tried: Color32,
    /// Failed reads still being worked on (trimming, scraping).
    pub retrying: Color32,
    /// Unreadable.
    pub bad: Color32,
    /// Outline of the current position.
    pub current: Color32,
}

/// The block map colours for the current mode.
pub fn map_colors(v: &Visuals) -> MapColors {
    if v.dark_mode {
        MapColors {
            rescued: rgb(52, 190, 104),
            not_tried: rgb(72, 77, 88),
            retrying: rgb(240, 170, 40),
            bad: rgb(235, 72, 72),
            current: Color32::WHITE,
        }
    } else {
        MapColors {
            rescued: rgb(46, 170, 90),
            not_tried: rgb(208, 212, 220),
            retrying: rgb(235, 155, 20),
            bad: rgb(215, 45, 45),
            current: rgb(20, 24, 33),
        }
    }
}

/// Text sizes, spacing and accent colours for both themes. Called once.
pub fn apply(ctx: &egui::Context) {
    ctx.all_styles_mut(|style| {
        style
            .text_styles
            .insert(TextStyle::Heading, FontId::proportional(24.0));
        style
            .text_styles
            .insert(TextStyle::Body, FontId::proportional(15.0));
        style
            .text_styles
            .insert(TextStyle::Button, FontId::proportional(15.0));
        style
            .text_styles
            .insert(TextStyle::Small, FontId::proportional(12.5));
        style
            .text_styles
            .insert(TextStyle::Monospace, FontId::monospace(13.5));
        style.spacing.item_spacing = egui::vec2(8.0, 8.0);
        style.spacing.button_padding = egui::vec2(12.0, 6.0);
        style.spacing.interact_size.y = 28.0;
        style.spacing.icon_width = 18.0;
        style.spacing.icon_width_inner = 10.0;
    });
    for theme in [Theme::Light, Theme::Dark] {
        ctx.style_mut_of(theme, |style| {
            let a = accent(&style.visuals);
            style.visuals.selection.bg_fill = a;
            style.visuals.selection.stroke.color = on_accent();
            style.visuals.hyperlink_color = a;
            style.visuals.panel_fill = page_fill(&style.visuals);
            // Text fields stand out from white cards.
            let dark = style.visuals.dark_mode;
            style.visuals.text_edit_bg_color = Some(if dark {
                rgb(22, 23, 27)
            } else {
                rgb(250, 251, 253)
            });
            if !dark {
                // Progress bar tracks and scroll bars must show on white cards.
                style.visuals.extreme_bg_color = rgb(226, 229, 236);
            }
            style.visuals.widgets.inactive.bg_stroke = egui::Stroke::new(
                1.0,
                if dark {
                    rgb(70, 75, 85)
                } else {
                    rgb(200, 204, 213)
                },
            );
        });
    }
}
