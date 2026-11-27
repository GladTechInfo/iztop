//! Pool state: joins the iostat stream, the status feed and the disks' busy
//! figures, and keeps the history the graphs are drawn from.

use crate::disks;
use crate::zpool::{self, Feed, IoRow, IoStream, PoolStatus, Space, VdevStatus};
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

/// History columns, one per second of sampled time: an hour, whatever the
/// sampling rate.
pub const HISTORY: usize = 3600;

/// The spans the history graphs can show, in seconds; `w` steps through them.
pub const WINDOWS: [usize; 3] = [60, 600, HISTORY];

pub fn window_label(seconds: usize) -> String {
    format!("{}m", seconds / 60)
}

/// A window as written on the command line or in the config file.
pub fn parse_window(s: &str) -> Option<usize> {
    match s {
        "1m" | "60s" => Some(60),
        "10m" => Some(600),
        "60m" | "1h" => Some(3600),
        _ => None,
    }
}

/// How often health, errors and scrub progress are re-read.
const STATUS_EVERY: Duration = Duration::from_secs(5);
/// How long to wait before restarting a `zpool iostat` that ended.
const RESPAWN_AFTER: Duration = Duration::from_secs(5);
/// Quiet time after which the rows received make a complete interval.
const LULL: Duration = Duration::from_millis(150);

/// One column per second of sampled time: each column is the time-weighted
/// mean of the samples in that second.
#[derive(Clone, Debug, Default)]
pub struct History {
    pub ring: VecDeque<f64>,
    /// Seconds pushed since the start, so a column can be pinned to the
    /// seconds it holds rather than to their age.
    total: u64,
    acc: f64,
    acc_t: f64,
}

impl History {
    pub fn add(&mut self, value: f64, mut dt: f64) {
        while dt >= 1.0 - self.acc_t {
            let remaining = 1.0 - self.acc_t;
            let mean = self.acc + value * remaining;
            if self.ring.len() == HISTORY {
                self.ring.pop_front();
            }
            self.ring.push_back(mean);
            self.total += 1;
            dt -= remaining;
            self.acc_t = 0.0;
            self.acc = 0.0;
        }
        self.acc += value * dt;
        self.acc_t += dt;
    }

    pub fn last(&self, n: usize) -> impl Iterator<Item = f64> + '_ {
        self.ring.iter().skip(self.ring.len().saturating_sub(n)).copied()
    }

    /// The last `window` seconds as `n` columns, newest at the right. Each is
    /// the highest second it covers, so a short burst survives a long window;
    /// a column older than the history is `None`.
    ///
    /// Column edges are fixed to the seconds themselves (second `s` since the
    /// start belongs to column `s * n / window`), not to their age. Measured
    /// back from the newest second instead, the edges would slide along the
    /// data each second; with a column of, say, 4.9 seconds and ZFS writing
    /// every 5, a column would now and then catch no write, and that empty
    /// column would race across the graph. Pinned edges scroll with the data.
    /// The newest column fills up as its seconds arrive.
    pub fn columns(&self, window: usize, n: usize) -> Vec<Option<f64>> {
        let (w, n) = (window.max(1) as i64, n as i64);
        let total = self.total as i64;
        // Seconds still in the ring: `first` up to, not including, `total`.
        let first = total - self.ring.len() as i64;
        // Column k starts at second floor(k * w / n); the newest second, total - 1,
        // lies in the last k whose start is not after it.
        let start = |k: i64| (k * w).div_euclid(n.max(1));
        let newest = if total == 0 { -1 } else { (total * n + w - 1).div_euclid(w) - 1 };
        (0..n)
            .map(|c| {
                let k = newest - (n - 1 - c);
                // A column narrower than a second still shows the second it is in.
                let (lo, hi) = (start(k), start(k + 1).max(start(k) + 1));
                (lo.max(first)..hi.min(total))
                    .map(|s| self.ring[(s - first) as usize])
                    .fold(None, |m: Option<f64>, v| Some(m.map_or(v, |m| m.max(v))))
            })
            .collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Pool,
    /// special, logs, cache, spares, dedup.
    Group,
    /// mirror, raidz, draid: a vdev with members.
    Vdev,
    Disk,
}

pub struct Vdev {
    /// As iostat names it; the key into the status.
    pub key: String,
    pub depth: usize,
    pub kind: Kind,
    pub io: IoRow,
    /// Whole-disk block device, for a disk.
    pub dev: Option<String>,
    /// Busy %: the disk's own, or the busiest disk beneath.
    pub busy: Option<f64>,
    /// Busy % per second.
    pub hist: History,
}

impl Vdev {
    /// A short name: a disk's path without the directory or a `-partN` tail.
    pub fn short_name(&self, full: bool) -> String {
        if self.kind != Kind::Disk {
            return self.key.clone();
        }
        let base = self.key.rsplit('/').next().unwrap_or(&self.key);
        if full {
            return base.to_string();
        }
        let base = match base.rfind("-part") {
            Some(i) if base[i + 5..].bytes().all(|b| b.is_ascii_digit()) && i + 5 < base.len() => &base[..i],
            _ => base,
        };
        for pre in ["ata-", "nvme-", "scsi-", "usb-", "sas-"] {
            if let Some(rest) = base.strip_prefix(pre) {
                return rest.to_string();
            }
        }
        base.to_string()
    }
}

pub struct Pool {
    pub name: String,
    /// Row 0 is the pool itself; the rest in iostat order.
    pub rows: Vec<Vdev>,
    pub status: Option<PoolStatus>,
    pub rd_hist: History,
    pub wr_hist: History,
}

impl Pool {
    pub fn visible_rows(&self, leaves: bool) -> impl Iterator<Item = &Vdev> {
        self.rows.iter().filter(move |v| leaves || v.kind != Kind::Disk || v.depth < 2)
    }

    pub fn vdev_status(&self, key: &str) -> Option<&VdevStatus> {
        self.status.as_ref().and_then(|s| s.vdevs.get(key))
    }

    /// A row's space for the capacity view. The pool row takes `zpool list`'s
    /// pool line, the figures the box title shows; a vdev or disk its own
    /// line of `zpool list -v`. Without those (an older zpool), a vdev falls
    /// back on iostat's alloc and free, and a disk has nothing.
    pub fn space(&self, r: &Vdev) -> Option<Space> {
        let st = self.status.as_ref();
        if r.kind == Kind::Pool {
            if let Some(s) = st.filter(|s| s.size > 0) {
                return Some(Space {
                    size: s.size,
                    alloc: Some(s.alloc),
                    free: Some(s.size.saturating_sub(s.alloc)),
                    frag: s.frag,
                    // Truncated, as zpool list's CAP is.
                    cap: Some(s.alloc * 100 / s.size),
                });
            }
        } else if let Some(sp) = st.and_then(|s| s.space.get(&r.key)) {
            return Some(sp.clone());
        }
        // A disk inside a mirror or raidz reports 0 and 0; one that is itself
        // a top-level vdev reports its own figures.
        let own = r.kind != Kind::Disk || r.depth < 2;
        match (r.io.alloc, r.io.free) {
            (Some(a), Some(f)) if own && a + f > 0 => Some(Space {
                size: a + f,
                alloc: Some(a),
                free: Some(f),
                frag: None,
                cap: Some(a * 100 / (a + f)),
            }),
            _ => None,
        }
    }

    pub fn read_bps(&self) -> f64 {
        self.rows.first().and_then(|r| r.io.rbw).unwrap_or(0.0)
    }

    pub fn write_bps(&self) -> f64 {
        self.rows.first().and_then(|r| r.io.wbw).unwrap_or(0.0)
    }
}

pub struct App {
    pub host: String,
    pub pools: Vec<Pool>,
    /// Pools named on the command line; empty for all.
    pub only: Vec<String>,
    pub interval_ms: u64,
    stream: Option<IoStream>,
    spawned: Instant,
    /// Rows of the interval now arriving.
    pending: Vec<IoRow>,
    pending_at: Instant,
    status_feed: zpool::StatusFeed,
    last_status: Option<Instant>,
    statuses: Option<Vec<PoolStatus>>,
    /// Why there is nothing to show, when there is nothing to show.
    pub error: Option<String>,
    pub blocks: u64,
    prev_busy: HashMap<String, u64>,
    prev_busy_t: Option<Instant>,
    last_device_refresh: Option<Instant>,
    pub status_error: Option<String>,
    pub status_warning: Option<String>,
    // A script needs a timestamp boundary to prove an interval is complete.
    once: bool,
}

pub fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname").map(|s| s.trim().to_string()).unwrap_or_default()
}

impl App {
    pub fn new(interval_ms: u64, only: Vec<String>, once: bool) -> Self {
        let mut a = App {
            host: hostname(),
            pools: Vec::new(),
            only,
            interval_ms,
            stream: None,
            spawned: Instant::now(),
            pending: Vec::new(),
            pending_at: Instant::now(),
            status_feed: zpool::status_feed(STATUS_EVERY),
            last_status: None,
            statuses: None,
            error: None,
            blocks: 0,
            prev_busy: HashMap::new(),
            prev_busy_t: None,
            last_device_refresh: None,
            status_error: None,
            status_warning: None,
            once,
        };
        a.spawn();
        a
    }

    pub fn sample_ready(&self) -> bool {
        self.blocks >= 2 && !self.pools.is_empty() && self.pools.iter().all(|p| p.status.is_some())
            && if self.only.is_empty() {
                self.statuses.as_ref().is_some_and(|s| s.len() == self.pools.len())
            } else {
                self.only.iter().all(|n| self.pools.iter().any(|p| &p.name == n))
            }
    }

    fn spawn(&mut self) {
        self.spawned = Instant::now();
        self.prev_busy_t = None;
        self.pending.clear();
        match IoStream::spawn(self.interval_ms, &self.only) {
            Ok(s) => self.stream = Some(s),
            Err(e) => {
                self.stream = None;
                self.error = Some(format!("cannot run zpool: {}", e));
            }
        }
    }

    pub fn set_interval(&mut self, ms: u64) {
        self.interval_ms = ms;
        self.stream = None;
        self.spawn();
    }

    /// After a pause, the next busy figure would average the whole pause.
    pub fn reset_rates(&mut self) {
        self.prev_busy_t = None;
    }

    /// Takes whatever the feeds have delivered. Returns whether anything
    /// changed. While `paused`, iostat blocks are read and dropped, so the
    /// first one after the pause is current.
    pub fn poll(&mut self, paused: bool) -> bool {
        let mut changed = false;
        let mut ended = None;
        let mut done = Vec::new();
        if let Some(s) = &self.stream {
            loop {
                match s.rx.try_recv() {
                    Ok(Feed::Stamp) => {
                        if !self.pending.is_empty() {
                            done.push(std::mem::take(&mut self.pending));
                        }
                    }
                    Ok(Feed::Rows(r)) => {
                        self.pending.extend(r);
                        self.pending_at = Instant::now();
                    }
                    Ok(Feed::Exited(e)) => {
                        ended = Some(e);
                        break;
                    }
                    Err(_) => break,
                }
            }
        }
        // An interval's rows come in a burst; a lull ends it without waiting
        // a whole interval for the next timestamp.
        if !self.once && !self.pending.is_empty() && (ended.is_some() || self.pending_at.elapsed() >= LULL) {
            done.push(std::mem::take(&mut self.pending));
        }
        // Should more than one interval have queued up, only the newest counts.
        if let Some(b) = done.pop().filter(|_| !paused) {
            self.apply_block(b);
            changed = true;
        }
        if let Some(e) = ended {
            self.stream = None;
            self.error = Some(if e.is_empty() { "zpool iostat ended".into() } else { format!("zpool iostat: {e}") });
            changed = true;
        }
        if !self.once && self.stream.is_none() && self.spawned.elapsed() >= RESPAWN_AFTER {
            self.spawn();
        }
        while let Ok(r) = self.status_feed.rx.try_recv() {
            match r {
                Ok(report) => {
                    self.last_status = Some(report.sampled_at);
                    self.status_error = None;
                    self.status_warning = report.warning;
                    self.apply_status(report.pools);
                }
                Err(e) => self.status_error = Some(e),
            }
            changed = true;
        }
        changed
    }

    pub fn stale_health_age(&self) -> Option<u64> {
        self.last_status.map(|t| t.elapsed().as_secs())
            .filter(|age| *age >= 10 || self.status_error.is_some())
    }

    fn apply_status(&mut self, st: Vec<PoolStatus>) {
        if !self.once && (!st.is_empty() || self.pools.is_empty()) {
            // A pool exported since the last block goes; one imported is
            // picked up by iostat itself.
            self.pools.retain(|p| st.iter().any(|s| s.name == p.name));
        }
        for p in self.pools.iter_mut() {
            p.status = st.iter().find(|s| s.name == p.name).cloned();
        }
        self.statuses = Some(st);
    }

    pub fn apply_block(&mut self, rows: Vec<IoRow>) {
        self.error = None;
        self.blocks += 1;
        let now = Instant::now();
        let refresh_devices = self.last_device_refresh.is_none_or(|t| now.duration_since(t) >= STATUS_EVERY);
        if refresh_devices {
            self.last_device_refresh = Some(now);
        }
        let busy_ms = disks::read_busy_ms();
        let dt = self.prev_busy_t.map(|t| now.duration_since(t).as_secs_f64());
        let interval = self.interval_ms as f64 / 1000.0;

        // Split into one section per pool: a depth-0 row that is not a group
        // heading starts a pool.
        let mut sections: Vec<Vec<IoRow>> = Vec::new();
        for r in rows {
            if r.depth == 0 && !r.group {
                sections.push(Vec::new());
            }
            if let Some(s) = sections.last_mut() {
                s.push(r);
            }
        }

        if self.once {
            // A pool that first appears in the second interval has no baseline.
            if self.pools.len() != sections.len()
                || sections.iter().any(|sec| !self.pools.iter().any(|p| p.name == sec[0].name))
            {
                self.blocks = 1;
                self.pools.clear();
            }
        }
        for sec in sections {
            let name = sec[0].name.clone();
            let idx = match self.pools.iter().position(|p| p.name == name) {
                Some(i) => i,
                None => {
                    let status = self.statuses.as_ref().and_then(|s| s.iter().find(|s| s.name == name).cloned());
                    self.pools.push(Pool {
                        name: name.clone(),
                        rows: Vec::new(),
                        status,
                        rd_hist: History::default(),
                        wr_hist: History::default(),
                    });
                    self.pools.sort_by(|a, b| a.name.cmp(&b.name));
                    self.pools.iter().position(|p| p.name == name).unwrap_or(0)
                }
            };
            let pool = &mut self.pools[idx];
            let n = sec.len();
            let mut out: Vec<Vdev> = Vec::with_capacity(n);
            // Keep history with its device when ZFS inserts or reorders rows.
            let mut previous: HashMap<_, _> = pool.rows.iter_mut().map(|v| {
                ((v.key.as_str(), v.depth), (v.kind, v.dev.take(), std::mem::take(&mut v.hist)))
            }).collect();
            for (i, io) in sec.iter().enumerate() {
                let leaf = sec.get(i + 1).is_none_or(|next| next.depth <= io.depth);
                let kind = if i == 0 {
                    Kind::Pool
                } else if io.group {
                    Kind::Group
                } else if leaf {
                    Kind::Disk
                } else {
                    Kind::Vdev
                };
                let prior = previous.remove(&(io.name.as_str(), io.depth));
                let (dev, hist) = match prior {
                    Some((Kind::Disk, dev, hist)) if kind == Kind::Disk && !refresh_devices => (dev, hist),
                    other => {
                        let dev = if kind == Kind::Disk { disks::whole_disk(&io.name) } else { None };
                        (dev, other.map(|(_, _, hist)| hist).unwrap_or_default())
                    }
                };
                let busy = match (&dev, dt) {
                    (Some(d), Some(dt)) => match (self.prev_busy.get(d), busy_ms.get(d)) {
                        (Some(p), Some(c)) => Some(disks::busy_pct(*p, *c, dt)),
                        _ => None,
                    },
                    _ => None,
                };
                out.push(Vdev { key: io.name.clone(), depth: io.depth, kind, io: io.clone(), dev, busy, hist });
            }
            // Anything with members shows its busiest disk: one saturated
            // disk holds up the whole vdev.
            for i in (0..n).rev() {
                if out[i].kind == Kind::Disk {
                    continue;
                }
                let d = out[i].depth;
                let top = i == 0;
                let max = out[i + 1..]
                    .iter()
                    .take_while(|v| top || v.depth > d)
                    .filter(|v| v.kind == Kind::Disk)
                    .filter_map(|v| v.busy)
                    .fold(None, |m: Option<f64>, b| Some(m.map_or(b, |m| m.max(b))));
                out[i].busy = max;
            }
            for v in out.iter_mut() {
                if let Some(b) = v.busy {
                    v.hist.add(b, interval);
                }
            }
            pool.rows = out;
            pool.rd_hist.add(pool.read_bps(), interval);
            pool.wr_hist.add(pool.write_bps(), interval);
        }
        self.prev_busy = busy_ms;
        self.prev_busy_t = Some(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_names_drop_prefix_and_partition() {
        let v = |key: &str, kind| Vdev {
            key: key.into(),
            depth: 2,
            kind,
            io: IoRow::default(),
            dev: None,
            busy: None,
            hist: History::default(),
        };
        assert_eq!(v("/dev/disk/by-id/ata-ST8000VN004-2M2101_WKD0A1B2-part1", Kind::Disk).short_name(false), "ST8000VN004-2M2101_WKD0A1B2");
        assert_eq!(v("/dev/disk/by-id/ata-X-part1", Kind::Disk).short_name(true), "ata-X-part1");
        assert_eq!(v("/dev/sdb", Kind::Disk).short_name(false), "sdb");
        assert_eq!(v("mirror-0", Kind::Vdev).short_name(false), "mirror-0");
    }

    #[test]
    fn columns_keep_peaks_and_leave_the_unknown_empty() {
        let mut h = History::default();
        for v in [1.0, 2.0, 9.0, 4.0, 5.0, 6.0] {
            h.add(v, 1.0);
        }
        // Six seconds over three columns of two: the higher of each pair.
        assert_eq!(h.columns(6, 3), [Some(2.0), Some(9.0), Some(6.0)]);
        // A window longer than the history: older columns are empty, and the
        // newest holds only the seconds it has so far.
        assert_eq!(h.columns(12, 3), [None, Some(9.0), Some(6.0)]);
        // More columns than seconds: each second is stretched.
        assert_eq!(h.columns(2, 4), [Some(5.0), Some(5.0), Some(6.0), Some(6.0)]);
    }

    #[test]
    fn history_splits_samples_at_second_boundaries() {
        let mut h = History::default();
        h.add(0.0, 0.75);
        h.add(100.0, 1.25);
        assert_eq!(h.last(2).collect::<Vec<_>>(), [25.0, 100.0]);
    }

    #[test]
    fn columns_scroll_with_the_data() {
        // A write every 5 seconds over 4.9-second columns. Each second the
        // finished columns may only move left, whole and unchanged; with
        // edges measured back from the newest second they changed in place,
        // and an empty column raced to the right.
        let mut h = History::default();
        let mut prev: Option<Vec<Option<f64>>> = None;
        for s in 0..800 {
            h.add(if s % 5 == 0 { 100.0 } else { 0.0 }, 1.0);
            let cols = h.columns(600, 122);
            if let Some(p) = prev.filter(|_| s >= 700) {
                // Shift 0 or 1: the newest column (index 121) may still change.
                let same = |d: usize| p[d..121] == cols[..121 - d];
                assert!(same(0) || same(1), "second {}: columns changed in place", s);
            }
            prev = Some(cols);
        }
    }
}
