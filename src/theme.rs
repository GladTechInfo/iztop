//! Palette and glyphs — igtop's and istop's, unchanged, so the three tools sit
//! side by side in one terminal and read as a set.
//!
//! The bar ramp is the load-bearing idea: colour is interpolated **per
//! character along the track**, so a bar encodes its magnitude twice — once by
//! length and once by hue.

use ratatui::style::Color;

/// Anchors for the gradient, as (position 0..1, RGB). Interpolated between.
const RAMP: [(f64, (u8, u8, u8)); 5] = [
    (0.00, (167, 230, 170)), // mint
    (0.35, (214, 230, 158)), // chartreuse
    (0.60, (242, 216, 158)), // amber
    (0.80, (243, 186, 152)), // peach
    (1.00, (238, 155, 155)), // soft red
];

fn lerp(a: u8, b: u8, k: f64) -> u8 {
    (a as f64 + (b as f64 - a as f64) * k).round().clamp(0.0, 255.0) as u8
}

/// Colour at fractional position `t` along the track.
pub fn ramp(t: f64) -> Color {
    let t = if t.is_finite() { t.clamp(0.0, 1.0) } else { 0.0 };
    for w in RAMP.windows(2) {
        let (t0, c0) = w[0];
        let (t1, c1) = w[1];
        if t <= t1 {
            let k = if t1 > t0 { (t - t0) / (t1 - t0) } else { 0.0 };
            return Color::Rgb(lerp(c0.0, c1.0, k), lerp(c0.1, c1.1, k), lerp(c0.2, c1.2, k));
        }
    }
    let l = RAMP[RAMP.len() - 1].1;
    Color::Rgb(l.0, l.1, l.2)
}

/// Colour for a bare number, matched to where that value sits on the ramp so
/// figures and bars agree.
pub fn value_colour(pct: f64) -> Color {
    if pct <= 0.5 {
        DIM
    } else {
        ramp(pct / 100.0)
    }
}

pub const TRACK: Color = Color::Rgb(58, 66, 80);
pub const CHROME: Color = Color::Rgb(68, 78, 92);
pub const DIM: Color = Color::Rgb(108, 120, 136);
pub const LABEL: Color = Color::Rgb(150, 163, 180);
pub const TEXT: Color = Color::Rgb(201, 211, 223);
pub const TITLE: Color = Color::Rgb(158, 216, 224); // pastel cyan
pub const BLUE: Color = Color::Rgb(168, 197, 235); // pastel blue
pub const MINT: Color = Color::Rgb(167, 230, 170);
pub const WARN: Color = Color::Rgb(238, 155, 155);
pub const AMBER: Color = Color::Rgb(242, 216, 158);
pub const VIOLET: Color = Color::Rgb(198, 180, 236);
/// Background of the selected menu row: one step above the ground, below the
/// track, so the row is found at a glance without shouting.
pub const SELECT: Color = Color::Rgb(40, 47, 60);

pub const EIGHTHS: [&str; 8] = ["▏", "▎", "▍", "▌", "▋", "▊", "▉", "█"];
/// Vertical eighths, for the graphs.
pub const VEIGHTHS: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
pub const TRACK_CH: &str = "░";

/// Nerd Font glyphs, all from the Font Awesome range that Nerd Fonts has
/// carried since v1, so they survive most patched fonts.
///
/// Every vdev row carries exactly one glyph. If a terminal renders these as
/// double-width, all rows shift by the same amount and the columns still line
/// up.
#[derive(Clone, Copy)]
pub struct Icons {
    pub pool: &'static str,
    pub vdev: &'static str,
    pub group: &'static str,
    pub disk: &'static str,
    pub read: &'static str,
    pub write: &'static str,
    pub clock: &'static str,
    pub graph: &'static str,
    pub scan: &'static str,
    pub warn: &'static str,
}

pub const NERD: Icons = Icons {
    pool: "\u{f1c0}",
    vdev: "\u{f0e8}",
    group: "\u{f02b}",
    disk: "\u{f0a0}",
    read: "\u{f063}",
    write: "\u{f062}",
    clock: "\u{f017}",
    graph: "\u{f201}",
    scan: "\u{f021}",
    warn: "\u{f071}",
};

/// Fallback for a terminal without a patched font. Same column width, so the
/// layout does not move when icons are switched off.
pub const PLAIN: Icons = Icons {
    pool: "Z",
    vdev: "+",
    group: "#",
    disk: "o",
    read: "v",
    write: "^",
    clock: "T",
    graph: "~",
    scan: "@",
    warn: "!",
};

pub fn icons(enabled: bool) -> Icons {
    if enabled {
        NERD
    } else {
        PLAIN
    }
}
