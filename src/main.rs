//! iztop — ZFS pool I/O in the manner of `zpool iostat -v`, drawn in igtop's
//! and istop's style: per-vdev operations, bandwidth and latency, how busy
//! each disk has been, pool health and scrub progress.
//!
//! Written entirely in Rust with no `unsafe`.

#![forbid(unsafe_code)]

mod app;
mod clock;
mod config;
mod disks;
mod json;
mod theme;
mod ui;
mod zpool;

use config::Config;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use ui::GraphMode;

const USAGE: &str = "\
iztop — ZFS pool I/O monitor: per-vdev operations, bandwidth, latency and disk busy

USAGE:
    iztop [OPTIONS] [POOL...]

OPTIONS:
    -i, --interval <MS>   refresh interval in milliseconds (250-10000, default 1000)
    -l, --vdevs-only      hide the disks inside mirrors and raidz vdevs
    -g, --graph <MODE>    bandwidth graph: auto (when there is room), on, off
    -w, --window <SPAN>   history the graphs span: 1m, 10m or 60m (default 1m,
                          or as saved in the config file)
        --full-names      full device names, not shortened ones
        --once            print a single plain-text sample and exit
        --no-icons        plain ASCII markers instead of Nerd Font glyphs
        --no-clock        no clock in the header
    -c, --config <FILE>   config file (default ~/.config/iztop/iztop.conf)
    -V, --version         version
    -h, --help            this text

With no POOL, every imported pool is shown, except those hidden in the
config file. Press d in iztop to order, show or hide pools, and W to save
the pools and the history window to the config file. Pools named on the
command line are shown whether hidden or not, in the configured order.

FIGURES:
    Operations, bandwidth and latency are zpool iostat's own, read from a
    running `zpool iostat -v -l` over the refresh interval. Latency is total
    wait: time queued in ZFS plus time at the disk.

    BUSY is the share of the interval each disk had I/O in flight, from
    /proc/diskstats, read from the whole disk under a partition. A mirror,
    raidz or pool shows its busiest disk, since one saturated disk holds up
    the rest. Its graph keeps the last hour; each column is the busiest
    second it covers. Press w to step between 1, 10 and 60 minutes.

    Health, READ/WRITE/CKSUM errors, fragmentation and scrub or resilver
    progress come from `zpool status -j` and `zpool list` every 5 seconds.
    Capacity is raw, parity included, as zpool list and iostat count it.
";

const INTERVAL_STEPS: [u64; 6] = [250, 500, 1000, 2000, 5000, 10_000];

struct Args {
    interval_ms: u64,
    pools: Vec<String>,
    leaves: bool,
    graph: GraphMode,
    full_names: bool,
    once: bool,
    icons: bool,
    clock: bool,
    /// Seconds of history the graphs span, if given.
    window: Option<usize>,
    config: Option<PathBuf>,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        interval_ms: 1000,
        pools: Vec::new(),
        leaves: true,
        graph: GraphMode::Auto,
        full_names: false,
        once: false,
        icons: true,
        clock: true,
        window: None,
        config: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{}", USAGE);
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("iztop {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "-i" | "--interval" => {
                let v = it.next().ok_or("--interval needs a value")?;
                a.interval_ms = v.parse::<u64>().map_err(|_| "--interval must be a number")?.clamp(250, 10_000);
            }
            "-l" | "--vdevs-only" => a.leaves = false,
            "-g" | "--graph" => {
                a.graph = match it.next().as_deref() {
                    Some("auto") => GraphMode::Auto,
                    Some("on") => GraphMode::On,
                    Some("off") => GraphMode::Off,
                    _ => return Err("--graph takes auto, on or off".into()),
                }
            }
            "-w" | "--window" => {
                a.window = Some(
                    it.next().as_deref().and_then(app::parse_window).ok_or("--window takes 1m, 10m or 60m")?,
                )
            }
            "--full-names" => a.full_names = true,
            "--once" => a.once = true,
            "--no-icons" => a.icons = false,
            "--no-clock" => a.clock = false,
            "-c" | "--config" => a.config = Some(PathBuf::from(it.next().ok_or("--config needs a file")?)),
            s if s.starts_with('-') => return Err(format!("unrecognised argument: {}", s)),
            pool => a.pools.push(pool.to_string()),
        }
    }
    Ok(a)
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("iztop: {}\n\n{}", e, USAGE);
            std::process::exit(2);
        }
    };
    let path = args.config.clone().or_else(config::default_path);
    let (cfg, warnings) = match &path {
        Some(p) => match Config::load(p) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("iztop: {}", e);
                std::process::exit(1);
            }
        },
        None => (Config::default(), Vec::new()),
    };
    for w in &warnings {
        eprintln!("iztop: {}: {}", path.as_ref().map(|p| p.display().to_string()).unwrap_or_default(), w);
    }
    if args.once {
        std::process::exit(once(&args, &cfg));
    }
    let mut app = app::App::new(args.interval_ms, args.pools.clone());
    let status = warnings.first().map(|w| format!("config {}", w));
    if let Err(e) = run_tui(&mut app, &args, cfg, path, status) {
        ratatui::restore();
        eprintln!("iztop: {}", e);
        std::process::exit(1);
    }
}

/// The pools to draw, as indices into `app.pools`, in the configured order.
/// Hidden pools are left out unless named on the command line.
fn display_order(app: &app::App, cfg: &Config, named: bool) -> Vec<usize> {
    let names: Vec<&str> = app.pools.iter().map(|p| p.name.as_str()).collect();
    cfg.rank(&names).into_iter().filter(|&i| named || cfg.shown(names[i])).collect()
}

/// One interval of `zpool iostat`, printed as a plain table for scripts.
fn once(args: &Args, cfg: &Config) -> i32 {
    use std::fmt::Write as _;
    let mut app = app::App::new(args.interval_ms, args.pools.clone());
    let deadline = Instant::now() + Duration::from_millis(args.interval_ms * 4 + 10_000);
    // Two blocks, since busy is measured between them, and the status, which
    // is read alongside.
    loop {
        app.poll(false);
        let have_status = !app.pools.is_empty() && app.pools.iter().all(|p| p.status.is_some());
        if (app.blocks >= 2 && have_status) || Instant::now() >= deadline {
            break;
        }
        if app.error.is_some() && app.blocks == 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    if app.blocks == 0 {
        eprintln!("iztop: {}", app.error.as_deref().unwrap_or("no output from zpool iostat"));
        return 1;
    }
    let na = |o: Option<String>| o.unwrap_or_else(|| "-".into());
    let f0 = |x: Option<f64>| na(x.map(|v| format!("{:.0}", v)));
    let mut o = String::new();
    let _ = writeln!(
        o,
        "{:<40} {:>8} {:>8} {:>6} {:>8} {:>8} {:>14} {:>14} {:>12} {:>12} {:>9}",
        "name", "state", "dev", "busy%", "r_ops", "w_ops", "r_bytes_s", "w_bytes_s", "r_wait_ns", "w_wait_ns", "errors"
    );
    for p in display_order(&app, cfg, !args.pools.is_empty()).into_iter().map(|i| &app.pools[i]) {
        if let Some(s) = &p.status {
            let scan = s
                .scan
                .as_ref()
                .filter(|sc| sc.running())
                .map(|sc| format!(" {}={:.1}%", sc.function.to_lowercase(), sc.pct()))
                .unwrap_or_default();
            let _ = writeln!(
                o,
                "# pool {} health={} size={} alloc={} frag={}{}",
                p.name,
                s.health,
                s.size,
                s.alloc,
                na(s.frag.map(|f| f.to_string())),
                scan
            );
        }
        for r in &p.rows {
            let st = p.vdev_status(&r.key);
            let name = format!("{}{}", "  ".repeat(r.depth), r.short_name(args.full_names));
            let _ = writeln!(
                o,
                "{:<40} {:>8} {:>8} {:>6} {:>8} {:>8} {:>14} {:>14} {:>12} {:>12} {:>9}",
                name,
                st.map(|s| s.state.as_str()).filter(|s| !s.is_empty()).unwrap_or("-"),
                r.dev.as_deref().unwrap_or("-"),
                na(r.busy.map(|b| format!("{:.1}", b))),
                f0(r.io.rops),
                f0(r.io.wops),
                f0(r.io.rbw),
                f0(r.io.wbw),
                f0(r.io.rwait),
                f0(r.io.wwait),
                na(st.map(|s| format!("{}/{}/{}", s.read_errors, s.write_errors, s.checksum_errors))),
            );
        }
    }
    print!("{}", o);
    0
}

struct Ui {
    window: usize,
    icons: bool,
    leaves: bool,
    full_names: bool,
    graph: GraphMode,
    /// None: follow the busiest pool.
    graph_pool: Option<String>,
    /// First pool shown: a position in the display order.
    offset: usize,
    help: bool,
    status: Option<(String, bool, Instant)>,
    cfg: Config,
    path: Option<PathBuf>,
    /// Pools named on the command line: shown even if hidden.
    named: bool,
    /// The pools menu's cursor, while it is open, and its pools in order.
    menu: Option<usize>,
    menu_items: Vec<String>,
}

impl Ui {
    fn say(&mut self, msg: impl Into<String>, err: bool) {
        self.status = Some((msg.into(), err, Instant::now()));
    }
}

enum Flow {
    Go,
    Quit,
    Suspend,
}

fn run_tui(
    app: &mut app::App,
    args: &Args,
    cfg: Config,
    path: Option<PathBuf>,
    status: Option<String>,
) -> io::Result<()> {
    let mut terminal = ratatui::init();
    let mut ui = Ui {
        window: args.window.or(cfg.window).unwrap_or(app::WINDOWS[0]),
        icons: args.icons,
        leaves: args.leaves,
        full_names: args.full_names,
        graph: args.graph,
        graph_pool: None,
        offset: 0,
        help: false,
        status: None,
        cfg,
        path,
        named: !args.pools.is_empty(),
        menu: None,
        menu_items: Vec::new(),
    };
    if let Some(s) = status {
        ui.say(s, true);
    }
    let zone = clock::Zone::local();
    let mut paused = false;
    let mut last_clock = String::new();
    let mut dirty = true;
    let mut hit = ui::Hit::default();

    loop {
        let now = Instant::now();
        if app.poll(paused) {
            dirty = true;
        }
        if ui.status.as_ref().is_some_and(|(_, _, t)| now.duration_since(*t) > Duration::from_secs(5)) {
            ui.status = None;
            dirty = true;
        }
        let clock = args.clock.then(|| zone.now_hms());
        if clock.as_deref().is_some_and(|c| c != last_clock) {
            last_clock = clock.clone().unwrap_or_default();
            dirty = true;
        }

        if dirty {
            dirty = false;
            let order = display_order(app, &ui.cfg, ui.named);
            ui.offset = ui.offset.min(order.len().saturating_sub(1));
            let graph_pool = graph_index(&ui, app, &order);
            let items: Vec<ui::MenuItem> = ui
                .menu_items
                .iter()
                .map(|n| {
                    let health = app.pools.iter().find(|p| &p.name == n).map(|p| {
                        p.status.as_ref().map(|s| s.health.clone()).unwrap_or_default()
                    });
                    (n.clone(), health.unwrap_or_else(|| "gone".into()), ui.cfg.shown(n))
                })
                .collect();
            let view = ui::View {
                app,
                window: ui.window,
                icons: ui.icons,
                paused,
                leaves: ui.leaves,
                full_names: ui.full_names,
                graph: ui.graph,
                graph_pool,
                order: &order,
                offset: ui.offset,
                help: ui.help,
                menu: ui.menu.map(|c| (items.as_slice(), c)),
                status: ui.status.as_ref().map(|(s, e, _)| (s.as_str(), *e)),
                clock: clock.clone(),
            };
            // Synchronised output: the terminal shows the frame whole or not
            // at all. Ignored where unsupported.
            crossterm::queue!(io::stdout(), BeginSynchronizedUpdate)?;
            terminal.draw(|f| hit = ui::draw(f, &view))?;
            crossterm::execute!(io::stdout(), EndSynchronizedUpdate)?;
        }

        // Often enough to pick up the next iostat block promptly.
        let wait = if dirty { Duration::ZERO } else { Duration::from_millis(50) };
        if !event::poll(wait)? {
            continue;
        }
        let ev = event::read()?;
        dirty = true;
        match ev {
            Event::Key(k) if k.kind == KeyEventKind::Press => match handle_key(k, &mut ui, app, &mut paused, hit) {
                Flow::Quit => break,
                Flow::Suspend => {
                    ratatui::restore();
                    // Stops here until `fg`, then carries on.
                    let _ = rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::Tstp);
                    terminal = ratatui::init();
                    terminal.clear()?;
                    app.reset_rates();
                }
                Flow::Go => {}
            },
            Event::Resize(..) => {}
            _ => dirty = false,
        }
    }
    ratatui::restore();
    let _ = io::stdout().flush();
    Ok(())
}

fn handle_key(k: KeyEvent, ui: &mut Ui, app: &mut app::App, paused: &mut bool, hit: ui::Hit) -> Flow {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    if ui.help {
        // Any key closes the help; q still quits.
        ui.help = false;
        return if matches!(k.code, KeyCode::Char('q')) { Flow::Quit } else { Flow::Go };
    }
    if let Some(c) = ui.menu {
        menu_key(k, ui, app, c);
        return Flow::Go;
    }
    let order = display_order(app, &ui.cfg, ui.named);
    let n = order.len();
    match k.code {
        KeyCode::Char('c') if ctrl => return Flow::Quit,
        KeyCode::Char('z') if ctrl => return Flow::Suspend,
        KeyCode::Char('q') | KeyCode::Esc => return Flow::Quit,
        KeyCode::Char('?') | KeyCode::Char('h') | KeyCode::F(1) => ui.help = true,
        KeyCode::Down | KeyCode::Char('j') | KeyCode::PageDown => {
            if hit.hidden_below > 0 {
                ui.offset = (ui.offset + 1).min(n.saturating_sub(1));
            }
        }
        KeyCode::Up | KeyCode::Char('k') | KeyCode::PageUp => ui.offset = ui.offset.saturating_sub(1),
        KeyCode::Home => ui.offset = 0,
        KeyCode::End => ui.offset = n.saturating_sub(1),
        KeyCode::Char('l') => {
            ui.leaves = !ui.leaves;
            ui.say(if ui.leaves { "disks shown" } else { "vdevs only" }, false);
        }
        KeyCode::Char('p') => ui.full_names = !ui.full_names,
        KeyCode::Char('g') => {
            ui.graph = match ui.graph {
                GraphMode::Auto => GraphMode::On,
                GraphMode::On => GraphMode::Off,
                GraphMode::Off => GraphMode::Auto,
            };
            let m = match ui.graph {
                GraphMode::Auto => "graph: when there is room",
                GraphMode::On => "graph: on",
                GraphMode::Off => "graph: off",
            };
            ui.say(m, false);
        }
        KeyCode::Char('d') => {
            let names: Vec<&str> = app.pools.iter().map(|p| p.name.as_str()).collect();
            ui.menu_items = ui.cfg.rank(&names).into_iter().map(|i| names[i].to_string()).collect();
            if ui.menu_items.is_empty() {
                ui.say("no pools", true);
            } else {
                ui.menu = Some(0);
            }
        }
        KeyCode::Char('n') | KeyCode::Char('b') if n > 0 => {
            // Steps through the pools in display order.
            let cur = graph_index(ui, app, &order).and_then(|g| order.iter().position(|&i| i == g)).unwrap_or(0);
            let next = if k.code == KeyCode::Char('n') { (cur + 1) % n } else { (cur + n - 1) % n };
            ui.graph_pool = Some(app.pools[order[next]].name.clone());
            if ui.graph == GraphMode::Off {
                ui.graph = GraphMode::On;
            }
        }
        KeyCode::Char('a') => {
            ui.graph_pool = None;
            ui.say("graph: busiest pool", false);
        }
        KeyCode::Char(' ') => {
            *paused = !*paused;
            if !*paused {
                app.reset_rates();
            }
        }
        KeyCode::Char('w') => {
            let i = app::WINDOWS.iter().position(|&w| w == ui.window).map_or(0, |i| (i + 1) % app::WINDOWS.len());
            ui.window = app::WINDOWS[i];
            ui.say(format!("history: last {}", app::window_label(ui.window)), false);
        }
        KeyCode::Char('W') => save(ui),
        KeyCode::Char('i') => ui.icons = !ui.icons,
        KeyCode::Char('+') | KeyCode::Char('=') | KeyCode::Char('-') => {
            let cur = INTERVAL_STEPS.iter().position(|s| *s >= app.interval_ms).unwrap_or(2);
            let i = if k.code == KeyCode::Char('-') { cur.saturating_sub(1) } else { (cur + 1).min(INTERVAL_STEPS.len() - 1) };
            if INTERVAL_STEPS[i] != app.interval_ms {
                app.set_interval(INTERVAL_STEPS[i]);
                ui.say(format!("refresh {} ms", INTERVAL_STEPS[i]), false);
            }
        }
        _ => {}
    }
    Flow::Go
}

/// The pool the graph follows, as an index into `app.pools`: the chosen one
/// if it is still shown, otherwise the busiest shown.
fn graph_index(ui: &Ui, app: &app::App, order: &[usize]) -> Option<usize> {
    ui.graph_pool
        .as_ref()
        .and_then(|name| order.iter().copied().find(|&i| &app.pools[i].name == name))
        .or_else(|| ui::default_pool(&app.pools, order))
}

/// Keys while the pools menu is open, as istop's disks menu.
fn menu_key(k: KeyEvent, ui: &mut Ui, app: &app::App, c: usize) {
    let n = ui.menu_items.len();
    let shift = k.modifiers.contains(KeyModifiers::SHIFT);
    match k.code {
        KeyCode::Up if shift => move_pool(ui, c, -1),
        KeyCode::Down if shift => move_pool(ui, c, 1),
        KeyCode::Char('+') | KeyCode::Char('=') | KeyCode::Char('K') => move_pool(ui, c, -1),
        KeyCode::Char('-') | KeyCode::Char('J') => move_pool(ui, c, 1),
        KeyCode::Up | KeyCode::Char('k') => ui.menu = Some(c.saturating_sub(1)),
        KeyCode::Down | KeyCode::Char('j') => ui.menu = Some((c + 1).min(n.saturating_sub(1))),
        KeyCode::Home => ui.menu = Some(0),
        KeyCode::End => ui.menu = Some(n.saturating_sub(1)),
        KeyCode::Char(' ') | KeyCode::Enter => {
            if let Some(name) = ui.menu_items.get(c).cloned() {
                if ui.cfg.shown(&name) {
                    ui.cfg.pool_hidden.push(name);
                } else {
                    ui.cfg.pool_hidden.retain(|h| *h != name);
                }
            }
        }
        KeyCode::Char('r') => {
            ui.cfg.pool_order.clear();
            ui.cfg.pool_hidden.clear();
            ui.menu_items = app.pools.iter().map(|p| p.name.clone()).collect();
            ui.menu = Some(0);
        }
        KeyCode::Char('W') => save(ui),
        KeyCode::Esc | KeyCode::Char('d') | KeyCode::Char('q') => ui.menu = None,
        _ => {}
    }
}

/// Moves the selected pool one place, and records the whole order, keeping
/// any configured pool that is not imported now where it was in the list.
fn move_pool(ui: &mut Ui, c: usize, by: isize) {
    let to = c as isize + by;
    if c >= ui.menu_items.len() || to < 0 || to as usize >= ui.menu_items.len() {
        return;
    }
    ui.menu_items.swap(c, to as usize);
    let absent: Vec<String> = ui.cfg.pool_order.iter().filter(|o| !ui.menu_items.contains(o)).cloned().collect();
    ui.cfg.pool_order = ui.menu_items.iter().cloned().chain(absent).collect();
    ui.menu = Some(to as usize);
}

/// `W`: the pool order, hidden pools and history window, written to the
/// config file on request only.
fn save(ui: &mut Ui) {
    let Some(path) = ui.path.clone() else {
        ui.say("no config path: HOME is not set", true);
        return;
    };
    ui.cfg.window = Some(ui.window);
    match ui.cfg.save(&path) {
        Ok(()) => ui.say(format!("saved {}", path.display()), false),
        Err(e) => ui.say(format!("save failed: {}", e), true),
    }
}
