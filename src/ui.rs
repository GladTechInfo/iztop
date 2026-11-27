//! Rendering.
//!
//! Gauges and graphs are written straight into the frame buffer, cell by
//! cell, as igtop and istop do: each cell gets its own colour from the ramp.
//!
//! One rounded box per pool and one row per vdev or disk, as `zpool iostat -v`
//! lays them out, so the screen reads the same way. Columns are dropped from
//! the right as the terminal narrows — latency first, then operations — so
//! the tool fits a half-width pane.

use crate::app::{window_label, App, Kind, Pool, Vdev};
use crate::zpool::Space;
use crate::theme::*;
use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphMode {
    /// Shown when every pool fits and there is height to spare.
    Auto,
    On,
    Off,
}

/// A row of the pools menu: name, health, shown.
pub type MenuItem = (String, String, bool);

pub struct View<'a> {
    pub app: &'a App,
    /// Seconds of history the graphs span.
    pub window: usize,
    pub icons: bool,
    pub paused: bool,
    pub leaves: bool,
    pub full_names: bool,
    /// The capacity view in place of the I/O figures.
    pub capacity: bool,
    pub graph: GraphMode,
    /// The pool the graph follows, an index into `app.pools`.
    pub graph_pool: Option<usize>,
    /// The pools shown, as indices into `app.pools`, in display order.
    pub order: &'a [usize],
    /// First pool shown: a position in `order`.
    pub offset: usize,
    pub help: bool,
    /// The pools menu: its rows in order, and the cursor.
    pub menu: Option<(&'a [MenuItem], usize)>,
    pub status: Option<(&'a str, bool)>,
    pub clock: Option<String>,
}

/// What the last frame could not show, for scrolling.
#[derive(Clone, Copy, Debug, Default)]
pub struct Hit {
    /// Pools below the last one drawn in full.
    pub hidden_below: usize,
}

// ─────────────────────────── formatting ───────────────────────────

/// Compact byte formatting, as istop's.
pub fn short_bytes(b: u64) -> String {
    const U: [(u64, &str); 6] =
        [(1 << 50, "P"), (1 << 40, "T"), (1 << 30, "G"), (1 << 20, "M"), (1 << 10, "K"), (1, "B")];
    for (div, suf) in U {
        if b >= div {
            let v = b as f64 / div as f64;
            return if v >= 100.0 || div == 1 {
                format!("{:.0}{}", v, suf)
            } else if v >= 10.0 {
                format!("{:.1}{}", v, suf)
            } else {
                format!("{:.2}{}", v, suf)
            };
        }
    }
    "0".into()
}

pub fn rate(bps: f64) -> String {
    format!("{}/s", short_bytes(bps.max(0.0) as u64))
}

/// Operations per second, five cells at most.
pub fn si(v: f64) -> String {
    if v >= 1e6 {
        format!("{:.1}M", v / 1e6)
    } else if v >= 1e4 {
        format!("{:.0}k", v / 1e3)
    } else if v >= 1e3 {
        format!("{:.1}k", v / 1e3)
    } else {
        format!("{:.0}", v)
    }
}

/// A latency in nanoseconds, five cells at most.
pub fn latency(ns: f64) -> String {
    if ns < 1e3 {
        format!("{:.0}ns", ns)
    } else if ns < 1e6 {
        format!("{:.0}us", ns / 1e3)
    } else if ns < 1e7 {
        format!("{:.1}ms", ns / 1e6)
    } else if ns < 1e9 {
        format!("{:.0}ms", ns / 1e6)
    } else if ns < 1e11 {
        format!("{:.1}s", ns / 1e9)
    } else {
        format!("{:.0}s", ns / 1e9)
    }
}

/// Where a latency sits on the ramp, on a log scale: 100 µs and below is
/// cool, a second is hot. A spinning disk's ordinary 10 ms reads amber.
fn latency_pct(ns: f64) -> f64 {
    ((ns.max(1.0).log10() - 5.0) / 4.0 * 100.0).clamp(1.0, 100.0)
}

pub fn duration(s: f64) -> String {
    let s = s.max(0.0) as u64;
    let (d, h, m) = (s / 86_400, s / 3600 % 24, s / 60 % 60);
    if d > 0 {
        format!("{}d {}h", d, h)
    } else if h > 0 {
        format!("{}h {:02}m", h, m)
    } else {
        format!("{}m {:02}s", m, s % 60)
    }
}

/// Keeps the end of a name, which is where a disk's serial number is.
fn tail(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n <= w {
        s.to_string()
    } else if w == 0 {
        String::new()
    } else {
        let keep: String = s.chars().skip(n + 1 - w).collect();
        format!("…{}", keep)
    }
}

/// Fits a disk name into `w` cells. A by-id name is model, then serial after
/// the last underscore; both matter — the model to tell the drive families
/// apart, the serial to find the drive — so the middle of the model gives way
/// first, then the start of the serial.
fn fit_name(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n <= w {
        return s.to_string();
    }
    if let Some(i) = s.rfind('_') {
        let serial = &s[i + 1..];
        let sn = serial.chars().count();
        // At least four cells of model, or it says nothing.
        if sn + 5 <= w {
            let head: String = s.chars().take(w - sn - 1).collect();
            return format!("{}…{}", head, serial);
        }
        return tail(serial, w);
    }
    tail(s, w)
}

fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

// ─────────────────────────── primitives ───────────────────────────

/// Writes `s` at (x, y), clipped to `w` cells. Returns the x after it.
fn put(buf: &mut Buffer, x: u16, y: u16, w: usize, s: &str, style: Style) -> u16 {
    if w == 0 {
        return x;
    }
    buf.set_stringn(x, y, s, w, style).0
}

/// Spans laid end to end, right-aligned so the last ends at `right`.
fn put_right(buf: &mut Buffer, right: u16, y: u16, parts: &[(String, Color)]) {
    let total: usize = parts.iter().map(|(s, _)| s.chars().count()).sum();
    let mut x = right.saturating_sub(total as u16);
    for (s, c) in parts {
        x = put(buf, x, y, right.saturating_sub(x) as usize, s, Style::default().fg(*c));
    }
}

/// The ramp colour of every cell along a gauge `width` cells long.
fn ramp_cells(width: usize) -> Vec<Color> {
    let span = (width.saturating_sub(1)).max(1) as f64;
    (0..width).map(|i| ramp(i as f64 / span)).collect()
}

/// A one-row history graph of percentages, a column per cell, newest at the
/// right: an eighth-block per column, coloured by its height. An idle column
/// is the dotted track; one from before the history began is left blank.
fn spark(buf: &mut Buffer, x: u16, y: u16, cols: &[Option<f64>]) {
    for (i, v) in cols.iter().enumerate() {
        let Some(c) = buf.cell_mut((x + i as u16, y)) else { continue };
        match v {
            None => {
                c.set_symbol(" ");
            }
            Some(v) => {
                let v = v.clamp(0.0, 100.0);
                let level = match (v / 100.0 * 8.0).round() as usize {
                    0 if v > 0.0 => 1,
                    l => l,
                };
                if level == 0 {
                    c.set_symbol(TRACK_CH).set_fg(TRACK);
                } else {
                    c.set_symbol(VEIGHTHS[level - 1]).set_fg(ramp(v / 100.0));
                }
            }
        }
    }
}

/// A short gauge as spans, for a box title.
fn gauge_spans(pct: f64, cells: usize) -> Vec<Span<'static>> {
    let colours = ramp_cells(cells);
    let filled = (pct / 100.0 * cells as f64 * 8.0).round().max(0.0) as usize;
    let (full, rem) = ((filled / 8).min(cells), filled % 8);
    colours
        .iter()
        .enumerate()
        .map(|(i, &c)| {
            if i < full {
                Span::styled("█", Style::default().fg(c))
            } else if i == full && rem > 0 {
                Span::styled(EIGHTHS[rem - 1], Style::default().fg(c))
            } else {
                Span::styled(TRACK_CH, Style::default().fg(TRACK))
            }
        })
        .collect()
}

/// A gauge drawn straight into the buffer, `cells` wide, coloured along the
/// ramp as the title gauge is.
fn gauge(buf: &mut Buffer, x: u16, y: u16, cells: usize, pct: f64) {
    for (i, span) in gauge_spans(pct, cells).into_iter().enumerate() {
        if let Some(c) = buf.cell_mut((x + i as u16, y)) {
            c.set_symbol(&span.content).set_style(span.style);
        }
    }
}

/// A filled area graph, newest column at the right edge, as istop's network
/// graph: `up` grows from the bottom, otherwise it hangs from the top.
fn graph(buf: &mut Buffer, area: Rect, data: &[f64], up: bool) {
    let h = area.height as usize;
    if h == 0 || area.width == 0 {
        return;
    }
    let n = data.len().min(area.width as usize);
    let data = &data[data.len() - n..];
    let x0 = area.right() - n as u16;
    for (i, &v) in data.iter().enumerate() {
        let v = if v.is_finite() { v.clamp(0.0, 1.0) } else { 0.0 };
        let mut total = (v * h as f64 * 8.0).round() as usize;
        if v > 0.0 {
            total = total.max(1);
        }
        let colour = ramp(v);
        let x = x0 + i as u16;
        for r in 0..h {
            let fill = total.saturating_sub(r * 8).min(8);
            if fill == 0 {
                break;
            }
            let y = if up { area.bottom() - 1 - r as u16 } else { area.y + r as u16 };
            let Some(c) = buf.cell_mut((x, y)) else { continue };
            if fill == 8 {
                c.set_symbol("█").set_fg(colour);
            } else if up {
                c.set_symbol(VEIGHTHS[fill - 1]).set_fg(colour);
            } else {
                c.set_symbol(VEIGHTHS[7 - fill])
                    .set_style(Style::default().fg(colour).bg(Color::Reset).add_modifier(Modifier::REVERSED));
            }
        }
    }
}

fn boxed<'a>(title: Vec<Span<'a>>) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(CHROME))
        .title(Line::from(title))
}

fn dim(s: impl Into<String>) -> (String, Color) {
    (s.into(), DIM)
}

// ─────────────────────────── columns ───────────────────────────

/// Which figure groups fit, decided once per frame from the box width so the
/// legend and every row agree.
#[derive(Clone, Copy)]
struct Cols {
    /// The capacity view: alloc, free, size, frag and used in place of
    /// the I/O figures, and a gauge in place of the busy graph.
    capacity: bool,
    ops: bool,
    lat: bool,
    /// Capacity view: free and frag drop, in that order, as width shrinks.
    free: bool,
    frag: bool,
    /// Indent, glyph and name.
    name: usize,
    bar: usize,
}

const BUSY_W: usize = 5; // " 100%"
const OPS_W: usize = 13; // "  rrrrr wwwww"
const BW_W: usize = 15; // "  rrrrrr wwwwww"
const LAT_W: usize = 13; // "  rrrrr wwwww"
const NAME_MIN: usize = 12;
const ALLOC_W: usize = 8; // "  aaaaaa"
const FREE_W: usize = 7; // " ffffff"
const SIZE_W: usize = 7; // " ssssss"
const FRAG_W: usize = 6; // "  fff%"
const USED_W: usize = 6; // "  uuu%"
const BAR_MIN: usize = 6;

impl Cols {
    fn new(inner_w: usize, longest_name: usize, capacity: bool) -> Cols {
        if capacity {
            return Cols::new_capacity(inner_w, longest_name);
        }
        // One cell of margin either side, one between name and bar.
        let room = |ops: bool, lat: bool| {
            inner_w as isize
                - 3
                - (BUSY_W + BW_W + if ops { OPS_W } else { 0 } + if lat { LAT_W } else { 0 }) as isize
        };
        // Each extra figure group is added only while the name and bar keep
        // a useful width: a bar too short to read is worth less than latency.
        let fits = |ops, lat, need: usize| room(ops, lat) >= need as isize;
        let (ops, lat) = if fits(true, true, 34) {
            (true, true)
        } else if fits(true, false, 24) {
            (true, false)
        } else {
            (false, false)
        };
        let left = room(ops, lat).max(0) as usize;
        // The name takes what it needs up to three fifths; the bar the rest.
        let name = longest_name.clamp(NAME_MIN, (left * 3 / 5).max(NAME_MIN)).min(left.saturating_sub(BAR_MIN));
        Cols { capacity: false, ops, lat, free: false, frag: false, name, bar: left.saturating_sub(name).max(1) }
    }

    /// The capacity view's columns: frag goes first as the box narrows,
    /// then free; allocated, size and used always stay.
    fn new_capacity(inner_w: usize, longest_name: usize) -> Cols {
        let room = |free: bool, frag: bool| {
            inner_w as isize
                - 3
                - (ALLOC_W + SIZE_W + USED_W + if free { FREE_W } else { 0 } + if frag { FRAG_W } else { 0 }) as isize
        };
        let fits = |free, frag, need: usize| room(free, frag) >= need as isize;
        let (free, frag) = if fits(true, true, 30) {
            (true, true)
        } else if fits(true, false, 24) {
            (true, false)
        } else {
            (false, false)
        };
        let left = room(free, frag).max(0) as usize;
        let name = longest_name.clamp(NAME_MIN, (left * 3 / 5).max(NAME_MIN)).min(left.saturating_sub(BAR_MIN));
        Cols { capacity: true, ops: false, lat: false, free, frag, name, bar: left.saturating_sub(name).max(1) }
    }

    /// The capacity view's figures for one row. A disk inside a mirror or
    /// raidz has only a size: ZFS keeps no allocation for it.
    fn space_figures(&self, sp: Option<&Space>, disk: bool) -> Vec<(String, Color)> {
        let b = |x: Option<u64>| x.map(short_bytes).unwrap_or_else(|| "-".into());
        let pct = |x: Option<u64>| x.map(|v| format!("{}%", v)).unwrap_or_else(|| "-".into());
        let Some(sp) = sp else {
            let mut out = vec![dim(format!("  {:>6}", "-"))];
            if self.free {
                out.push(dim(format!(" {:>6}", "-")));
            }
            out.push(dim(format!(" {:>6}", "-")));
            if self.frag {
                out.push(dim(format!("  {:>4}", "-")));
            }
            out.push(dim(format!("  {:>4}", "-")));
            return out;
        };
        let shade = |x: Option<u64>| if x.is_some() { TEXT } else { DIM };
        // A disk never has these figures, so its row stays blank but for the
        // size rather than filling with dashes.
        let b = |x: Option<u64>| if disk && x.is_none() { String::new() } else { b(x) };
        let pct = |x: Option<u64>| if disk && x.is_none() { String::new() } else { pct(x) };
        let mut out = vec![(format!("  {:>6}", b(sp.alloc)), shade(sp.alloc))];
        if self.free {
            out.push((format!(" {:>6}", b(sp.free)), shade(sp.free)));
        }
        out.push((format!(" {:>6}", short_bytes(sp.size)), if disk { DIM } else { TEXT }));
        if self.frag {
            let c = match sp.frag {
                Some(f) if f >= 50 => AMBER,
                Some(_) => TEXT,
                None => DIM,
            };
            out.push((format!("  {:>4}", pct(sp.frag)), c));
        }
        let c = sp.cap.map(|c| value_colour(c as f64)).unwrap_or(DIM);
        out.push((format!("  {:>4}", pct(sp.cap)), c));
        out
    }

    fn figures(&self, v: &Vdev) -> Vec<(String, Color)> {
        let io = &v.io;
        let shade = |x: Option<f64>| if x.unwrap_or(0.0) >= 0.5 { TEXT } else { DIM };
        let mut out = vec![match v.busy {
            Some(b) => (format!("{:>4.0}%", b), value_colour(b)),
            None => dim("    -"),
        }];
        if self.ops {
            let f = |x: Option<f64>| x.map(si).unwrap_or_else(|| "-".into());
            out.push((format!("  {:>5}", f(io.rops)), shade(io.rops)));
            out.push((format!(" {:>5}", f(io.wops)), shade(io.wops)));
        }
        let b = |x: Option<f64>| x.map(|x| short_bytes(x as u64)).unwrap_or_else(|| "-".into());
        out.push((format!("  {:>6}", b(io.rbw)), shade(io.rbw)));
        out.push((format!(" {:>6}", b(io.wbw)), shade(io.wbw)));
        if self.lat {
            // A latency only means something when there were operations.
            let l = |ns: Option<f64>, ops: Option<f64>| match ns.filter(|_| ops.unwrap_or(0.0) > 0.0) {
                Some(ns) => (latency(ns), value_colour(latency_pct(ns))),
                None => dim("-"),
            };
            let (r, rc) = l(io.rwait, io.rops);
            let (w, wc) = l(io.wwait, io.wops);
            out.push((format!("  {:>5}", r), rc));
            out.push((format!(" {:>5}", w), wc));
        }
        out
    }

    fn legend(&self, ic: &Icons) -> Vec<(String, Color)> {
        if self.capacity {
            let mut s = format!("  {:>6}", "alloc");
            if self.free {
                s.push_str(&format!(" {:>6}", "free"));
            }
            s.push_str(&format!(" {:>6}", "size"));
            if self.frag {
                s.push_str(&format!("  {:>4}", "frag"));
            }
            s.push_str(&format!("  {:>4}", "used"));
            return vec![(s, LABEL)];
        }
        let mut out = vec![(format!("{:>5}", "busy"), LABEL)];
        if self.ops {
            out.push((format!("  {:>5} {:>5}", "r ops", "w ops"), LABEL));
        }
        out.push(("  ".into(), LABEL));
        out.push((format!("{} ", ic.read), MINT));
        out.push((format!("{:>4}", "read"), LABEL));
        out.push((" ".into(), LABEL));
        out.push((ic.write.to_string(), AMBER));
        out.push(("write".into(), LABEL));
        if self.lat {
            out.push((format!("  {:>5} {:>5}", "r lat", "w lat"), LABEL));
        }
        out
    }
}

fn display_name(v: &Vdev, full: bool) -> String {
    v.short_name(full)
}

// ───────────────────────────── frame ─────────────────────────────

pub fn draw(f: &mut Frame, v: &View) -> Hit {
    let area = f.area();
    let a = v.app;
    let mut hit = Hit::default();
    if area.height < 3 {
        return hit;
    }
    draw_header(f, Rect { height: 1, ..area }, v);
    let body = Rect { y: area.y + 1, height: area.height - 1, ..area };

    if v.order.is_empty() {
        let msg = match &a.error {
            Some(e) if a.pools.is_empty() => format!("   {}", e),
            _ if a.blocks == 0 => "   waiting for zpool iostat…".to_string(),
            _ if a.pools.is_empty() => "   no pools".to_string(),
            _ => "   every pool is hidden: d to choose".to_string(),
        };
        let colour = if a.error.is_some() { WARN } else { DIM };
        f.render_widget(Paragraph::new(Span::styled(msg, Style::default().fg(colour))), body);
        draw_popups(f, area, v);
        return hit;
    }

    let pools: Vec<&Pool> = v.order.iter().filter_map(|&i| a.pools.get(i)).collect();
    let longest = pools
        .iter()
        .flat_map(|p| p.visible_rows(v.leaves))
        .map(|r| r.depth * 2 + 2 + display_name(r, v.full_names).chars().count())
        .max()
        .unwrap_or(NAME_MIN);
    let cols = Cols::new(body.width.saturating_sub(2) as usize, longest, v.capacity);
    let ic = icons(v.icons);

    // Legend, aligned with the figures inside the boxes.
    let legend_y = body.y;
    put_right(f.buffer_mut(), body.right().saturating_sub(2), legend_y, &cols.legend(&ic));
    let label = if v.capacity { "space used".to_string() } else { format!("busy, last {}", window_label(v.window)) };
    let gx = body.x + 2 + cols.name as u16 + 1;
    put(f.buffer_mut(), gx, legend_y, cols.bar, &label, Style::default().fg(LABEL));
    let rest = Rect { y: body.y + 1, height: body.height - 1, ..body };

    let heights: Vec<u16> = pools.iter().map(|p| p.visible_rows(v.leaves).count() as u16 + 2).collect();
    let offset = v.offset.min(pools.len() - 1);
    let need: u16 = heights[offset..].iter().sum();
    let spare = rest.height.saturating_sub(need);
    // The bandwidth graph belongs to the I/O view.
    let graph_h = match v.graph {
        _ if v.capacity => 0,
        GraphMode::Off => 0,
        GraphMode::Auto if spare >= 7 && offset == 0 => spare.min(14),
        GraphMode::Auto => 0,
        GraphMode::On => spare.clamp(7, 14).min(rest.height.saturating_sub(3)),
    };
    let mut y = rest.y;
    if graph_h > 0 {
        draw_graph(f, Rect { y, height: graph_h, ..rest }, v);
        y += graph_h;
    }
    let bottom = rest.bottom();
    for (i, p) in pools.iter().enumerate().skip(offset) {
        let h = heights[i];
        if y >= bottom || bottom - y < 3 {
            hit.hidden_below = pools.len() - i;
            break;
        }
        let box_h = h.min(bottom - y);
        draw_pool(f, Rect { y, height: box_h, ..rest }, p, &cols, v);
        if box_h < h {
            hit.hidden_below = pools.len() - i;
        }
        y += box_h;
    }
    if hit.hidden_below > 0 || offset > 0 {
        let more = format!(
            " {}{} ",
            if offset > 0 { format!("▲ {}  ", offset) } else { String::new() },
            if hit.hidden_below > 0 { format!("▼ {}", hit.hidden_below) } else { String::new() }
        );
        put(f.buffer_mut(), body.x + 1, legend_y, more.chars().count(), &more, Style::default().fg(AMBER));
    }
    draw_popups(f, area, v);
    hit
}

fn draw_popups(f: &mut Frame, area: Rect, v: &View) {
    if let Some((items, cursor)) = v.menu {
        draw_pool_menu(f, area, items, cursor);
    }
    if v.help {
        draw_help(f, area);
    }
}

/// `d`: every pool, in display order, with whether it is shown and its
/// health; the selected one can be moved or hidden.
fn draw_pool_menu(f: &mut Frame, area: Rect, items: &[MenuItem], cursor: usize) {
    let name_w = items.iter().map(|i| i.0.chars().count()).max().unwrap_or(4).clamp(12, 32);
    let w = (name_w as u16 + 24).max(64).min(area.width);
    // Two lines of help and a blank above and below the list.
    let room = area.height.saturating_sub(7).max(1) as usize;
    let h = (items.len().min(room) as u16 + 7).min(area.height);
    let r = Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h };
    f.render_widget(Clear, r);
    let block = boxed(vec![Span::styled(" pools ", Style::default().fg(TITLE).add_modifier(Modifier::BOLD))]);
    let inner = block.inner(r);
    f.render_widget(block, r);
    let top = cursor.saturating_sub(room.saturating_sub(1));
    let mut lines = vec![Line::from("")];
    for (i, (name, health, shown)) in items.iter().enumerate().skip(top).take(room) {
        let sel = i == cursor;
        let mut name_style = Style::default().fg(if *shown { TEXT } else { DIM });
        if sel {
            name_style = name_style.bg(SELECT).add_modifier(Modifier::BOLD);
        }
        lines.push(Line::from(vec![
            Span::styled(if sel { "  ▶ " } else { "    " }, Style::default().fg(AMBER)),
            Span::styled(if *shown { "[x] " } else { "[ ] " }, Style::default().fg(if *shown { MINT } else { DIM })),
            Span::styled(format!(" {:<w$} ", tail(name, name_w), w = name_w), name_style),
            Span::styled(format!(" {}", health), Style::default().fg(if ok_state(health) { DIM } else { WARN })),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  ↑ ↓  select    space  show / hide    Shift+↑ ↓ or + -  move",
        Style::default().fg(DIM),
    )));
    lines.push(Line::from(Span::styled(
        "  r  default order, all shown    Esc d q  close    W  save",
        Style::default().fg(DIM),
    )));
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_header(f: &mut Frame, area: Rect, v: &View) {
    let a = v.app;
    let ic = icons(v.icons);
    let mut spans = vec![
        Span::styled(" iztop", Style::default().fg(TITLE).add_modifier(Modifier::BOLD)),
        Span::styled(format!("  {}", a.host), Style::default().fg(TEXT)),
    ];
    if let Some(t) = &v.clock {
        spans.push(Span::styled(format!("  {} ", ic.clock), Style::default().fg(DIM)));
        spans.push(Span::styled(t.clone(), Style::default().fg(TEXT)));
    }
    spans.push(Span::styled(format!("  {} ms", a.interval_ms), Style::default().fg(DIM)));
    let n = a.pools.len();
    let shown = if v.order.len() < n { format!("{} of ", v.order.len()) } else { String::new() };
    spans.push(Span::styled(
        format!("  {}{} pool{}", shown, n, if n == 1 { "" } else { "s" }),
        Style::default().fg(if shown.is_empty() { DIM } else { AMBER }),
    ));
    if v.capacity {
        spans.push(Span::styled("  capacity", Style::default().fg(TITLE).add_modifier(Modifier::BOLD)));
    }
    if !v.leaves {
        spans.push(Span::styled("  vdevs only", Style::default().fg(AMBER)));
    }
    if v.paused {
        spans.push(Span::styled("  paused", Style::default().fg(WARN).add_modifier(Modifier::BOLD)));
    }
    if let Some(e) = a.error.as_ref().filter(|_| !a.pools.is_empty()) {
        spans.push(Span::styled(format!("  {} {}", ic.warn, e), Style::default().fg(WARN)));
    }
    if let Some(e) = a.status_error.as_ref().or(a.status_warning.as_ref()) {
        spans.push(Span::styled(format!("  {} {}", ic.warn, e), Style::default().fg(WARN)));
    }
    if let Some((msg, err)) = v.status {
        spans.push(Span::styled(
            format!("  {}", msg),
            Style::default().fg(if err { WARN } else { MINT }).add_modifier(Modifier::BOLD),
        ));
    }
    spans.push(Span::styled("   q quit  ? help", Style::default().fg(CHROME)));
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn ok_state(s: &str) -> bool {
    s.is_empty() || s == "ONLINE" || s == "AVAIL"
}

fn pool_title<'a>(p: &'a Pool, ic: &Icons) -> Vec<Span<'a>> {
    let mut t = vec![
        Span::styled(format!(" {} ", ic.pool), Style::default().fg(TITLE)),
        Span::styled(p.name.as_str(), Style::default().fg(TITLE).add_modifier(Modifier::BOLD)),
    ];
    let sep = || Span::styled(" · ", Style::default().fg(DIM));
    let st = p.status.as_ref();
    if let Some(h) = st.map(|s| s.health.as_str()).filter(|h| !h.is_empty()) {
        t.push(sep());
        let c = if ok_state(h) { MINT } else { WARN };
        t.push(Span::styled(h, Style::default().fg(c).add_modifier(Modifier::BOLD)));
    }
    // A scrub or resilver in progress matters more than capacity, so it
    // comes first and is the last thing a narrow box clips.
    if let Some(sc) = st.and_then(|s| s.scan.as_ref()) {
        let what = sc.function.to_lowercase();
        if sc.running() {
            t.push(sep());
            let mut s = format!("{} {} {:.1}%", ic.scan, what, sc.pct());
            if sc.paused {
                s.push_str(" paused");
            } else {
                let now = now_unix();
                s.push_str(&format!(" {}", rate(sc.rate(now))));
                if let Some(eta) = sc.eta_s(now) {
                    s.push_str(&format!(" {} left", duration(eta)));
                }
            }
            t.push(Span::styled(s, Style::default().fg(AMBER).add_modifier(Modifier::BOLD)));
        } else if sc.errors > 0 {
            t.push(sep());
            t.push(Span::styled(format!("{} {} errors", what, sc.errors), Style::default().fg(WARN)));
        }
    }
    let (alloc, size) = match st.filter(|s| s.size > 0) {
        Some(s) => (s.alloc, s.size),
        None => {
            let r = &p.rows[0].io;
            (r.alloc.unwrap_or(0), r.alloc.unwrap_or(0) + r.free.unwrap_or(0))
        }
    };
    if size > 0 {
        // Truncated, as zpool list's CAP is.
        let pct = (alloc as f64 / size as f64 * 100.0).floor();
        t.push(sep());
        t.extend(gauge_spans(pct, 8));
        t.push(Span::styled(format!(" {:.0}%", pct), Style::default().fg(value_colour(pct))));
        t.push(Span::styled(
            format!(" {} / {}", short_bytes(alloc), short_bytes(size)),
            Style::default().fg(TEXT),
        ));
    }
    if let Some(fr) = st.and_then(|s| s.frag) {
        t.push(Span::styled(format!(" · frag {}%", fr), Style::default().fg(DIM)));
    }
    t.push(Span::raw(" "));
    t
}

fn draw_pool(f: &mut Frame, area: Rect, p: &Pool, cols: &Cols, v: &View) {
    let ic = icons(v.icons);
    let block = boxed(pool_title(p, &ic));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let buf = f.buffer_mut();
    let right = inner.right().saturating_sub(1);
    for (row, r) in p.visible_rows(v.leaves).enumerate() {
        let y = inner.y + row as u16;
        if y >= inner.bottom() {
            break;
        }
        let (glyph, gcol, ncol, bold) = match r.kind {
            Kind::Pool => (ic.pool, TITLE, TEXT, true),
            Kind::Group => (ic.group, VIOLET, VIOLET, false),
            Kind::Vdev => (ic.vdev, BLUE, LABEL, true),
            Kind::Disk => (ic.disk, BLUE, TEXT, false),
        };
        let x0 = inner.x + 1;
        let indent = (r.depth * 2) as u16;
        let x = put(buf, x0 + indent, y, cols.name.saturating_sub(indent as usize), glyph, Style::default().fg(gcol)) + 1;
        let name_room = cols.name.saturating_sub((x - x0) as usize);

        // Anything not healthy is said on the row, in red, at the end of the
        // name column; the name gives way to it.
        let vs = p.vdev_status(&r.key);
        let mut warn = String::new();
        if let Some(s) = vs {
            if !ok_state(&s.state) {
                warn.push_str(&s.state);
            }
            if s.errors() > 0 {
                if !warn.is_empty() {
                    warn.push(' ');
                }
                warn.push_str(&format!("{}/{}/{}", s.read_errors, s.write_errors, s.checksum_errors));
            }
        }
        let warn_w = if warn.is_empty() { 0 } else { (warn.chars().count() + 1).min(name_room) };
        let name = fit_name(&display_name(r, v.full_names), name_room.saturating_sub(warn_w));
        let mut style = Style::default().fg(if warn.is_empty() { ncol } else { WARN });
        if bold {
            style = style.add_modifier(Modifier::BOLD);
        }
        put(buf, x, y, name_room, &name, style);
        if warn_w > 0 {
            let wx = x + (name_room - warn_w) as u16 + 1;
            put(buf, wx, y, warn_w - 1, &warn, Style::default().fg(WARN).add_modifier(Modifier::BOLD));
        }
        if r.kind == Kind::Group {
            continue;
        }
        let bx = x0 + cols.name as u16 + 1;
        if cols.capacity {
            let sp = p.space(r);
            match sp.as_ref().and_then(|s| s.cap) {
                Some(c) => gauge(buf, bx, y, cols.bar, c as f64),
                // A disk has no allocation of its own: its device name,
                // for finding it in other tools, stands in for the gauge.
                None => {
                    if let Some(d) = &r.dev {
                        put(buf, bx, y, cols.bar, d, Style::default().fg(DIM));
                    }
                }
            }
            put_right(buf, right, y, &cols.space_figures(sp.as_ref(), r.kind == Kind::Disk && sp.as_ref().is_some_and(|s| s.cap.is_none())));
            continue;
        }
        if !r.hist.ring.is_empty() {
            spark(buf, bx, y, &r.hist.columns(v.window, cols.bar));
        } else if r.dev.is_some() {
            // Not measured yet: busy needs two samples.
            put(buf, bx, y, cols.bar, &" ".repeat(cols.bar), Style::default());
        } else {
            // No block device to read (a file vdev), or nothing beneath
            // measured yet: a bare track.
            put(buf, bx, y, cols.bar, &TRACK_CH.repeat(cols.bar), Style::default().fg(TRACK));
        }
        put_right(buf, right, y, &cols.figures(r));
    }
}

// ─────────────────────────── graph ───────────────────────────

/// Which pool the graph follows when none is chosen: the busiest shown pool
/// over the visible window. `order` holds indices into `pools`.
pub fn default_pool(pools: &[Pool], order: &[usize]) -> Option<usize> {
    let busy = |p: &Pool| p.rd_hist.last(120).chain(p.wr_hist.last(120)).sum::<f64>() + p.read_bps() + p.write_bps();
    order
        .iter()
        .filter_map(|&i| pools.get(i).map(|p| (i, p)))
        .max_by(|a, b| busy(a.1).total_cmp(&busy(b.1)).then(b.1.name.cmp(&a.1.name)))
        .map(|(i, _)| i)
}

fn draw_graph(f: &mut Frame, area: Rect, v: &View) {
    let a = v.app;
    let ic = icons(v.icons);
    let Some(p) = v.graph_pool.and_then(|i| a.pools.get(i)) else { return };
    let title = vec![
        Span::styled(format!(" {} ", ic.graph), Style::default().fg(TITLE)),
        Span::styled("bandwidth", Style::default().fg(TITLE).add_modifier(Modifier::BOLD)),
        Span::styled(" · ", Style::default().fg(DIM)),
        Span::styled(p.name.as_str(), Style::default().fg(TEXT).add_modifier(Modifier::BOLD)),
        Span::styled(format!(" · last {}", window_label(v.window)), Style::default().fg(DIM)),
        Span::styled(
            format!(
                "  {}/{}  b n ",
                v.graph_pool.and_then(|g| v.order.iter().position(|&i| i == g)).unwrap_or(0) + 1,
                v.order.len()
            ),
            Style::default().fg(CHROME),
        ),
    ];
    let block = boxed(title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height < 4 || inner.width < 20 {
        return;
    }
    let w = inner.width.saturating_sub(2) as usize;
    // Columns before the history began are NaN, which draws nothing.
    let column = |h: &crate::app::History| -> Vec<f64> {
        h.columns(v.window, w).into_iter().map(|c| c.unwrap_or(f64::NAN)).collect()
    };
    let (rd_cols, wr_cols) = (column(&p.rd_hist), column(&p.wr_hist));
    // Peak and mean over every second in the window, not the columns.
    let rd: Vec<f64> = p.rd_hist.last(v.window).collect();
    let wr: Vec<f64> = p.wr_hist.last(v.window).collect();
    // One scale for both halves, floored at 1 MiB/s so a pool at rest is not
    // drawn as busy. f64::max passes over NaN.
    let scale = rd_cols.iter().chain(wr_cols.iter()).copied().fold(1024.0 * 1024.0, f64::max);
    let half = (inner.height.saturating_sub(2)) / 2;
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(half),
        Constraint::Length(half),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .split(inner);
    let label = |icon: &str, colour: Color, name: &str, now: f64, hist: &[f64]| {
        let peak = hist.iter().copied().fold(0.0, f64::max);
        let avg = if hist.is_empty() { 0.0 } else { hist.iter().sum::<f64>() / hist.len() as f64 };
        Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{} ", icon), Style::default().fg(colour)),
            Span::styled(format!("{:<6}", name), Style::default().fg(LABEL).add_modifier(Modifier::BOLD)),
            Span::styled(format!("{:>10}", rate(now)), Style::default().fg(value_colour(now / scale * 100.0))),
            Span::styled("   peak ", Style::default().fg(DIM)),
            Span::styled(rate(peak), Style::default().fg(TEXT)),
            Span::styled("   mean ", Style::default().fg(DIM)),
            Span::styled(rate(avg), Style::default().fg(TEXT)),
        ])
    };
    f.render_widget(Paragraph::new(label(ic.read, MINT, "read", p.read_bps(), &rd)), rows[0]);
    let buf = f.buffer_mut();
    let g = |r: Rect| Rect { x: r.x + 1, width: r.width.saturating_sub(2), ..r };
    graph(buf, g(rows[1]), &rd_cols.iter().map(|b| b / scale).collect::<Vec<_>>(), true);
    graph(buf, g(rows[2]), &wr_cols.iter().map(|b| b / scale).collect::<Vec<_>>(), false);
    f.render_widget(Paragraph::new(label(ic.write, AMBER, "write", p.write_bps(), &wr)), rows[3]);
}

// ─────────────────────────── help ───────────────────────────

pub const HELP: [(&str, &str); 16] = [
    ("q  Esc", "quit (Esc first closes this)"),
    ("↑ ↓  PgUp PgDn", "scroll through the pools"),
    ("Home End", "first or last pool"),
    ("d", "pools menu: order, show or hide; W saves"),
    ("l", "disks inside vdevs: show or hide"),
    ("g", "bandwidth graph: auto, on, off"),
    ("n  b", "graph: next or previous pool"),
    ("a", "graph: follow the busiest pool"),
    ("p", "full device names or short ones"),
    ("c", "capacity view or I/O view"),
    ("+  -", "slower or faster refresh"),
    ("w", "history: last 1, 10 or 60 minutes"),
    ("W", "save pools, view and history window"),
    ("Space", "pause"),
    ("i", "icons or ASCII"),
    ("?", "this help"),
];

fn draw_help(f: &mut Frame, area: Rect) {
    let w = 64.min(area.width);
    let h = (HELP.len() as u16 + 9).min(area.height);
    let r = Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h };
    f.render_widget(Clear, r);
    let block = boxed(vec![Span::styled(" help ", Style::default().fg(TITLE).add_modifier(Modifier::BOLD))]);
    let inner = block.inner(r);
    f.render_widget(block, r);
    let mut lines: Vec<Line> = HELP
        .iter()
        .map(|(k, d)| {
            Line::from(vec![
                Span::styled(format!("  {:<16}", k), Style::default().fg(TITLE)),
                Span::styled(*d, Style::default().fg(TEXT)),
            ])
        })
        .collect();
    lines.push(Line::raw(""));
    for t in [
        "  busy: share of time the disk had I/O in flight; a vdev",
        "  shows its busiest disk. Each column of its graph is the",
        "  busiest second it covers. lat: total wait, queue and disk.",
        "  Errors are READ/WRITE/CKSUM, as zpool status counts them.",
    ] {
        lines.push(Line::from(Span::styled(t, Style::default().fg(DIM))));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn figures_fit_their_columns() {
        for ns in [12.0, 850.0, 12_000.0, 999_000.0, 1.2e6, 9.9e6, 12e6, 999e6, 1.5e9, 99e9, 500e9] {
            assert!(latency(ns).chars().count() <= 5, "{}", latency(ns));
        }
        for v in [0.0, 12.0, 999.0, 1500.0, 9999.0, 15_000.0, 999_000.0, 2.5e6] {
            assert!(si(v).chars().count() <= 5, "{}", si(v));
        }
        for b in [0u64, 1023, 1 << 20, 999 << 30, 5 << 40, 3 << 50] {
            assert!(short_bytes(b).chars().count() <= 6, "{}", short_bytes(b));
        }
    }

    #[test]
    fn names_keep_model_start_and_serial() {
        assert_eq!(fit_name("ST8000VN004-2M2101_WKD0A1B2", 22), "ST8000VN004-2…WKD0A1B2");
        assert_eq!(fit_name("ST8000VN004-2M2101_WKD0A1B2", 40), "ST8000VN004-2M2101_WKD0A1B2");
        assert_eq!(fit_name("ST8000VN004-2M2101_WKD0A1B2", 10), "WKD0A1B2");
        assert_eq!(fit_name("ST8000VN004-2M2101_WKD0A1B2", 6), "…0A1B2");
        assert_eq!(fit_name("sdb", 12), "sdb");
        assert_eq!(fit_name("raidz2-0-very-long-name", 8).chars().count(), 8);
    }

    #[test]
    fn columns_drop_from_the_right_as_width_shrinks() {
        let wide = Cols::new(120, 30, false);
        assert!(wide.ops && wide.lat);
        let mid = Cols::new(70, 30, false);
        assert!(mid.ops && !mid.lat);
        let narrow = Cols::new(50, 30, false);
        assert!(!narrow.ops && !narrow.lat);
        assert!(narrow.name >= NAME_MIN && narrow.bar >= 1);
    }

    #[test]
    fn capacity_columns_drop_frag_then_free() {
        let wide = Cols::new(88, 30, true);
        assert!(wide.capacity && wide.free && wide.frag);
        let mid = Cols::new(58, 30, true);
        assert!(mid.free && !mid.frag);
        let narrow = Cols::new(44, 30, true);
        assert!(!narrow.free && !narrow.frag);
        assert!(narrow.name >= NAME_MIN && narrow.bar >= 1);
    }

    #[test]
    fn space_figures_fit_the_legend() {
        let c = Cols::new(88, 30, true);
        let width = |v: &[(String, Color)]| v.iter().map(|(s, _)| s.chars().count()).sum::<usize>();
        let sp = Space { size: 8_000_000_000_000, alloc: Some(5_000_000_000_000), free: Some(3_000_000_000_000), frag: Some(30), cap: Some(62) };
        let disk = Space { size: 4_000_000_000_000, ..Default::default() };
        let legend = width(&c.legend(&icons(false)));
        assert_eq!(width(&c.space_figures(Some(&sp), false)), legend);
        assert_eq!(width(&c.space_figures(Some(&disk), true)), legend);
        assert_eq!(width(&c.space_figures(None, false)), legend);
    }
}
